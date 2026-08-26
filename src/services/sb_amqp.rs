use fe2o3_amqp::link::receiver::CreditMode;
use fe2o3_amqp::{Connection, Receiver, Sender, Session};
use fe2o3_amqp_types::messaging::{Body, Data, Message, Properties};

/// Returns true if the AMQP broker at `host:5672` is ready to negotiate.
/// Used during emulator startup to distinguish "port open" from "broker ready".
pub async fn probe_amqp(host: &str) -> bool {
    let url = format!("amqp://{}:5672", host);
    match tokio::time::timeout(
        std::time::Duration::from_millis(800),
        Connection::open("ais-runner-probe", url.as_str()),
    )
    .await
    {
        Ok(Ok(mut conn)) => {
            conn.close().await.ok();
            true
        }
        _ => false,
    }
}

/// Send a JSON message to a Service Bus queue via AMQP 1.0.
/// Used for the local emulator (localhost:5672) where no Azure auth is needed.
///
/// Retries up to `MAX_ATTEMPTS` times with exponential backoff — the SB emulator
/// depends on SQL Edge and can take several seconds after the port opens before
/// the AMQP broker is ready to negotiate ("Waiting for header exchange" error).
pub async fn send_amqp_message(host: &str, queue: &str, body: &str) -> Result<(), String> {
    send_amqp_message_with_type(host, queue, body, "application/json").await
}

/// Like [`send_amqp_message`] but with an explicit AMQP content-type.
///
/// The content-type decides how the Logic Apps SB trigger delivers the body:
/// `application/json` arrives as a raw string in `contentData`; any non-JSON
/// type (e.g. `application/octet-stream`) arrives base64-wrapped as
/// `contentData.$content` — which is what workflows using
/// `decodeBase64(…$content)` require. See `sb_testing::queue_encoding` for
/// picking the right one automatically.
pub async fn send_amqp_message_with_type(
    host: &str,
    queue: &str,
    body: &str,
    content_type: &str,
) -> Result<(), String> {
    const MAX_ATTEMPTS: u32 = 5;
    const BASE_DELAY_MS: u64 = 800;

    let url = format!("amqp://{}:5672", host);
    let mut last_err = String::new();

    for attempt in 1..=MAX_ATTEMPTS {
        match try_send(&url, queue, body, content_type).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = e.clone();
                // Only retry on transient broker-not-ready errors
                let is_transient = e.contains("header exchange")
                    || e.contains("UnexpectedEof")
                    || e.contains("Connection refused")
                    || e.contains("reset by peer");
                if !is_transient || attempt == MAX_ATTEMPTS {
                    break;
                }
                let delay = BASE_DELAY_MS * (1 << (attempt - 1)); // 800 ms, 1.6 s, 3.2 s, 6.4 s
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
        }
    }

    Err(last_err)
}

/// One message returned from a peek. Carries the body text plus AMQP-level
/// metadata that helps the user diagnose poison-message loops without having
/// to leave the queue browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeekedMessage {
    pub body: String,
    /// AMQP `header.delivery-count` — number of *prior* unsuccessful delivery
    /// attempts. A growing delivery-count on a single message is the classic
    /// "poison" signature: the workflow keeps abandoning the message, the
    /// broker keeps re-presenting it, eventually the queue dead-letters it.
    pub delivery_count: u32,
}

/// Peek up to `max` messages from a queue without consuming them.
/// Receives messages then releases them back to the broker.
pub async fn peek_amqp_messages(
    host: &str,
    queue: &str,
    max: usize,
) -> Result<Vec<PeekedMessage>, String> {
    let url = format!("amqp://{}:5672", host);

    let mut connection = Connection::open("ais-runner-peek", url.as_str())
        .await
        .map_err(|e| format!("AMQP connect: {e}"))?;

    let mut session = Session::begin(&mut connection)
        .await
        .map_err(|e| format!("AMQP session: {e}"))?;

    let mut receiver = Receiver::builder()
        .name("ais-runner-peek")
        .source(queue)
        .auto_accept(false)
        .credit_mode(CreditMode::Manual)
        .attach(&mut session)
        .await
        .map_err(|e| format!("AMQP attach receiver on '{}': {e}", queue))?;

    // Grant credits for the messages we want to peek
    receiver
        .set_credit(max as u32)
        .await
        .map_err(|e| format!("set_credit: {e}"))?;

    let mut messages = Vec::new();
    let timeout = std::time::Duration::from_millis(1500);

    for _ in 0..max {
        match tokio::time::timeout(timeout, receiver.recv::<Body<String>>()).await {
            Ok(Ok(delivery)) => {
                let body_text = body_to_string(delivery.body());
                let delivery_count = delivery_count_from(&delivery);
                receiver.release(&delivery).await.ok();
                messages.push(PeekedMessage {
                    body: body_text,
                    delivery_count,
                });
            }
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }

    receiver.close().await.ok();
    session.end().await.ok();
    connection.close().await.ok();

    Ok(messages)
}

/// Purge every message from a queue by receiving and **accepting** (settling)
/// each one — accepted messages are removed permanently, unlike peek which
/// releases them back. Returns the number removed.
///
/// Drains in an auto-replenishing credit window and stops when a receive idles
/// out (the queue is empty). Only for the local emulator (localhost:5672).
///
/// Note: this drains the main queue only. Dead-lettered messages live in a
/// separate sub-queue (`<queue>/$deadletterqueue`) and are not touched.
pub async fn drain_queue(host: &str, queue: &str) -> Result<u64, String> {
    const BATCH: u32 = 50;
    const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1200);
    // Guard against a producer flooding the queue faster than we drain it.
    const SAFETY_CAP: u64 = 1_000_000;

    let url = format!("amqp://{}:5672", host);

    let mut connection = Connection::open("ais-runner-drain", url.as_str())
        .await
        .map_err(|e| format!("AMQP connect: {e}"))?;

    let mut session = Session::begin(&mut connection)
        .await
        .map_err(|e| format!("AMQP session: {e}"))?;

    let mut receiver = Receiver::builder()
        .name("ais-runner-drain")
        .source(queue)
        .auto_accept(false)
        .credit_mode(CreditMode::Auto(BATCH))
        .attach(&mut session)
        .await
        .map_err(|e| format!("AMQP attach receiver on '{}': {e}", queue))?;

    let mut removed = 0u64;
    loop {
        match tokio::time::timeout(IDLE_TIMEOUT, receiver.recv::<Body<String>>()).await {
            Ok(Ok(delivery)) => {
                receiver
                    .accept(&delivery)
                    .await
                    .map_err(|e| format!("AMQP accept: {e}"))?;
                removed += 1;
                if removed >= SAFETY_CAP {
                    break;
                }
            }
            Ok(Err(_)) => break, // link/broker error — stop, report what we removed
            Err(_) => break,     // idle timeout — queue drained
        }
    }

    receiver.close().await.ok();
    session.end().await.ok();
    connection.close().await.ok();

    Ok(removed)
}

/// Extract the `delivery-count` field from an AMQP delivery's header, with a
/// defensive default of 0 (matches the AMQP spec — `delivery-count` is
/// optional in the wire format, and "absent" is semantically equivalent to
/// zero prior attempts).
fn delivery_count_from<T>(delivery: &fe2o3_amqp::link::delivery::Delivery<T>) -> u32 {
    delivery
        .message()
        .header
        .as_ref()
        .map(|h| h.delivery_count)
        .unwrap_or(0)
}

/// Extract a human-readable string from an AMQP Body.
/// Handles Data (binary), Value (AMQP value), and Sequence sections.
/// For Data sections the bytes are decoded as UTF-8; if the result is
/// valid JSON it is pretty-printed.
/// Extract a human-readable string from an AMQP Body.
/// Handles Data (binary), Value (AMQP value), and Sequence sections.
/// For Data sections the bytes are decoded as UTF-8; if the result is
/// valid JSON it is pretty-printed.
fn body_to_string(body: &Body<String>) -> String {
    let raw = match body {
        Body::Data(batch) => {
            let bytes: Vec<u8> = batch.iter().flat_map(|d| d.0.iter().cloned()).collect();
            String::from_utf8_lossy(&bytes).into_owned()
        }
        Body::Value(v) => format!("{:?}", v),
        Body::Sequence(s) => format!("{:?}", s),
        Body::Empty => String::new(),
    };
    // Pretty-print if it's valid JSON
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
        serde_json::to_string_pretty(&v).unwrap_or(raw)
    } else {
        raw
    }
}

async fn try_send(url: &str, queue: &str, body: &str, content_type: &str) -> Result<(), String> {
    let mut connection = Connection::open("ais-runner", url)
        .await
        .map_err(|e| format!("AMQP connect to {}: {}", url, e))?;

    let mut session = Session::begin(&mut connection)
        .await
        .map_err(|e| format!("AMQP session: {}", e))?;

    let mut sender = Sender::attach(&mut session, "ais-runner-sender", queue)
        .await
        .map_err(|e| format!("AMQP attach to queue '{}': {}", queue, e))?;

    // The content-type decides the trigger-side envelope: application/json is
    // delivered as a raw string in contentData; non-JSON types arrive
    // base64-wrapped in contentData.$content (what decodeBase64 consumers need).
    let props = Properties::builder().content_type(content_type).build();
    let msg = Message::builder()
        .properties(props)
        .data(Data::from(body.as_bytes().to_vec()))
        .build();

    let outcome = sender
        .send(msg)
        .await
        .map_err(|e| format!("AMQP send: {}", e))?;

    outcome
        .accepted_or_else(|state| format!("message not accepted: {:?}", state))
        .map_err(|e| e)?;

    sender.close().await.ok();
    session.end().await.ok();
    connection.close().await.ok();

    Ok(())
}

#[cfg(test)]
mod drain_live_tests {
    use super::*;

    /// End-to-end against a running SB emulator. Ignored by default.
    ///   cargo test drain_queue_live -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn drain_queue_live() {
        const HOST: &str = "localhost";
        const Q: &str = "ais.apim.event"; // exists in the emulator Config.json

        // Start clean, then seed 5 messages.
        let _ = drain_queue(HOST, Q).await;
        for i in 0..5 {
            send_amqp_message(HOST, Q, &format!("{{\"n\":{i}}}"))
                .await
                .expect("send");
        }

        let removed = drain_queue(HOST, Q).await.expect("drain");
        assert_eq!(removed, 5, "should drain exactly the 5 seeded messages");

        // Draining an already-empty queue removes nothing and doesn't hang.
        let again = drain_queue(HOST, Q).await.expect("drain empty");
        assert_eq!(again, 0);

        // And peek confirms the queue is empty.
        let peeked = peek_amqp_messages(HOST, Q, 10).await.expect("peek");
        assert!(
            peeked.is_empty(),
            "queue should be empty after flush, got {}",
            peeked.len()
        );
    }
}
