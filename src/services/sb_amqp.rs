use fe2o3_amqp::{Connection, Session, Sender};

/// Send a JSON message to a Service Bus queue via AMQP 1.0.
/// Used for the local emulator (localhost:5672) where no Azure auth is needed.
pub async fn send_amqp_message(host: &str, queue: &str, body: &str) -> Result<(), String> {
    let url = format!("amqp://{}:5672", host);

    let mut connection = Connection::open("ais-runner", url.as_str())
        .await
        .map_err(|e| format!("AMQP connect to {}: {}", url, e))?;

    let mut session = Session::begin(&mut connection)
        .await
        .map_err(|e| format!("AMQP session: {}", e))?;

    let mut sender = Sender::attach(&mut session, "ais-runner-sender", queue)
        .await
        .map_err(|e| format!("AMQP attach to queue '{}': {}", queue, e))?;

    let outcome = sender
        .send(body)
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
