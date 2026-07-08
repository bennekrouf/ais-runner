//! Generic Service Bus testing helpers.
//!
//! Everything here works from workflow.json contents and message bodies only —
//! no assumptions about any specific project's schemas:
//!
//! - **Consumer-aware encoding** — a workflow that reads its SB trigger body
//!   via `decodeBase64(…$content)` needs messages sent with a NON-JSON
//!   content-type (the connector then base64-wraps the body); one that reads
//!   `json(contentData)` needs `application/json`. Sending the wrong kind
//!   produces the cryptic "decodeBase64 expects string, got Null" failure.
//! - **Payload variants** — null out a field (dot path) to test validation
//!   branches; burst-send N copies to test alert consolidation.
//! - **Correlation trace** — peek every queue and report where messages
//!   containing a given id currently sit.
//! - **Adaptive Card preview** — summarize a card payload found in a peeked
//!   message so the user doesn't have to mentally render JSON.
//! - **Expectations** — assert "queue X holds >= N messages where <path> = <value>".

use serde_json::Value;

// ─────────────────────────────────────────────────────────────────────────────
// 1. Consumer-aware content-type
// ─────────────────────────────────────────────────────────────────────────────

/// How the workflow consuming a queue expects the AMQP body to be delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueEncoding {
    /// Consumer decodes `contentData.$content` — send with a non-JSON
    /// content-type so the SB connector base64-wraps the body.
    Base64Wrapped { consumer: String },
    /// Consumer reads `contentData` as a (JSON) string — send as
    /// `application/json`.
    RawJson { consumer: Option<String> },
}

impl QueueEncoding {
    /// AMQP content-type to use on send.
    pub fn content_type(&self) -> &'static str {
        match self {
            QueueEncoding::Base64Wrapped { .. } => "application/octet-stream",
            QueueEncoding::RawJson { .. } => "application/json",
        }
    }

    /// Short human-readable explanation for the status line.
    pub fn describe(&self) -> String {
        match self {
            QueueEncoding::Base64Wrapped { consumer } => format!(
                "base64-wrapped — {consumer} decodes contentData.$content"
            ),
            QueueEncoding::RawJson { consumer: Some(c) } => {
                format!("raw JSON — {c} reads contentData directly")
            }
            QueueEncoding::RawJson { consumer: None } => "raw JSON (no consumer found)".into(),
        }
    }
}

/// Detect the encoding the consumer of `queue` expects by scanning every
/// workflow.json in the workspace for a Service Bus trigger on that queue,
/// then checking how the workflow dereferences the message body.
pub fn queue_encoding(logic_apps_dir: &str, queue: &str) -> QueueEncoding {
    let dir = crate::services::workflows::resolve_logic_apps_dir(logic_apps_dir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return QueueEncoding::RawJson { consumer: None };
    };
    for entry in entries.flatten() {
        let wf_path = entry.path().join("workflow.json");
        let Ok(content) = std::fs::read_to_string(&wf_path) else { continue };
        let Ok(workflow) = serde_json::from_str::<Value>(&content) else { continue };
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(enc) = encoding_from_workflow(&workflow, &content, queue, &name) {
            return enc;
        }
    }
    QueueEncoding::RawJson { consumer: None }
}

/// Inspect one parsed workflow: if its trigger consumes `queue`, classify how
/// it reads the body. Exposed for tests.
fn encoding_from_workflow(
    workflow: &Value,
    raw_text: &str,
    queue: &str,
    workflow_name: &str,
) -> Option<QueueEncoding> {
    let defn = workflow.get("definition").unwrap_or(workflow);
    let triggers = defn["triggers"].as_object()?;
    let consumes = triggers.values().any(|t| {
        t["inputs"]["serviceProviderConfiguration"]["serviceProviderId"].as_str()
            == Some("/serviceProviders/serviceBus")
            && t["inputs"]["parameters"]["queueName"].as_str() == Some(queue)
    });
    if !consumes {
        return None;
    }
    // The tell-tale: dereferencing `$content` (usually via decodeBase64) means
    // the workflow expects the connector's base64 envelope.
    let wants_base64 = raw_text.contains("$content")
        && (raw_text.contains("decodeBase64") || raw_text.contains("base64ToString"));
    Some(if wants_base64 {
        QueueEncoding::Base64Wrapped { consumer: workflow_name.to_string() }
    } else {
        QueueEncoding::RawJson { consumer: Some(workflow_name.to_string()) }
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// 2. Payload variants
// ─────────────────────────────────────────────────────────────────────────────

/// Set the field at dot-separated `path` (e.g. `data.msg.content.CompanyId`)
/// to `null` in a JSON payload. Errors if the payload isn't JSON or the path
/// doesn't exist — silently "nulling" a field that was never there would make
/// the user think they tested a validation branch they didn't.
pub fn null_field(raw: &str, path: &str) -> Result<String, String> {
    let mut v: Value =
        serde_json::from_str(raw).map_err(|e| format!("payload is not valid JSON: {e}"))?;
    let segs: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return Err("empty field path".into());
    }
    null_at(&mut v, &segs).map_err(|e| e.replace("<path>", path))?;
    serde_json::to_string_pretty(&v).map_err(|e| e.to_string())
}

fn null_at(v: &mut Value, segs: &[&str]) -> Result<(), String> {
    let obj = v
        .as_object_mut()
        .ok_or_else(|| "field '<path>' not found in payload (non-object parent)".to_string())?;
    let (first, rest) = segs.split_first().expect("segs is non-empty");
    let slot = obj
        .get_mut(*first)
        .ok_or_else(|| "field '<path>' not found in payload".to_string())?;
    if rest.is_empty() {
        *slot = Value::Null;
        Ok(())
    } else {
        null_at(slot, rest)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 3. Correlation trace
// ─────────────────────────────────────────────────────────────────────────────

/// Where messages matching a correlation id currently sit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceHit {
    pub queue: String,
    pub count: usize,
}

/// Peek every queue and count messages whose body contains `needle`
/// (substring match — ids are UUIDs, so this is both generic and precise).
/// Queues that error on peek are skipped: a trace is a read-only diagnostic
/// and one broken queue shouldn't kill the whole picture.
pub async fn trace_correlation(host: &str, queues: &[String], needle: &str) -> Vec<TraceHit> {
    let mut hits = Vec::new();
    for q in queues {
        let Ok(msgs) = crate::services::sb_amqp::peek_amqp_messages(host, q, 32).await else {
            continue;
        };
        let count = msgs.iter().filter(|m| m.body.contains(needle)).count();
        if count > 0 {
            hits.push(TraceHit { queue: q.clone(), count });
        }
    }
    hits
}

// ─────────────────────────────────────────────────────────────────────────────
// 4. Adaptive Card preview
// ─────────────────────────────────────────────────────────────────────────────

/// A rendering-friendly summary of an Adaptive Card found in a message body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardPreview {
    /// First TextBlock's `color` (Attention / Warning / Good / Default…) —
    /// the alerting convention for red/yellow/green cards.
    pub accent: String,
    /// TextBlock texts in document order.
    pub lines: Vec<String>,
    /// FactSet entries as `title: value`.
    pub facts: Vec<String>,
    /// Action titles (buttons).
    pub actions: Vec<String>,
}

/// Find and summarize the first Adaptive Card in a message body. Looks for
/// any object with `"type": "AdaptiveCard"` anywhere in the JSON — cards are
/// usually nested under an envelope key like `adaptiveCard` or `attachments`.
pub fn adaptive_card_preview(body: &str) -> Option<CardPreview> {
    let v: Value = serde_json::from_str(body).ok()?;
    let card = find_adaptive_card(&v)?;
    let mut preview = CardPreview {
        accent: "Default".into(),
        lines: Vec::new(),
        facts: Vec::new(),
        actions: Vec::new(),
    };
    collect_card_elements(&card["body"], &mut preview);
    if let Some(actions) = card["actions"].as_array() {
        for a in actions {
            if let Some(t) = a["title"].as_str() {
                preview.actions.push(t.to_string());
            }
        }
    }
    Some(preview)
}

fn find_adaptive_card(v: &Value) -> Option<Value> {
    match v {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("AdaptiveCard") {
                return Some(v.clone());
            }
            map.values().find_map(find_adaptive_card)
        }
        Value::Array(items) => items.iter().find_map(find_adaptive_card),
        // Cards are sometimes embedded as a JSON string (double-encoded)
        Value::String(s) if s.contains("AdaptiveCard") => {
            serde_json::from_str::<Value>(s).ok().and_then(|inner| find_adaptive_card(&inner))
        }
        _ => None,
    }
}

fn collect_card_elements(body: &Value, preview: &mut CardPreview) {
    let Some(items) = body.as_array() else { return };
    for item in items {
        match item["type"].as_str() {
            Some("TextBlock") => {
                if let Some(t) = item["text"].as_str() {
                    preview.lines.push(t.to_string());
                }
                if preview.accent == "Default" {
                    if let Some(c) = item["color"].as_str() {
                        preview.accent = c.to_string();
                    }
                }
            }
            Some("FactSet") => {
                if let Some(facts) = item["facts"].as_array() {
                    for f in facts {
                        let title = f["title"].as_str().unwrap_or("");
                        let value = f["value"].as_str().unwrap_or("");
                        preview.facts.push(format!("{title} {value}"));
                    }
                }
            }
            // Containers nest more elements
            Some("Container") | Some("ColumnSet") | Some("Column") => {
                collect_card_elements(&item["items"], preview);
                collect_card_elements(&item["columns"], preview);
            }
            _ => {}
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 5. Expectations
// ─────────────────────────────────────────────────────────────────────────────

/// Result of checking an expectation against a queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectationResult {
    pub passed: bool,
    pub matched: usize,
    pub peeked: usize,
    pub detail: String,
}

/// Check that `queue` currently holds at least `min_count` messages where the
/// JSON value at dot-`path` equals `expected` (string compare; numbers and
/// bools are compared via their JSON rendering). `path` empty = count all.
pub async fn check_expectation(
    host: &str,
    queue: &str,
    path: &str,
    expected: &str,
    min_count: usize,
) -> Result<ExpectationResult, String> {
    let msgs = crate::services::sb_amqp::peek_amqp_messages(host, queue, 64).await?;
    let matched = msgs
        .iter()
        .filter(|m| message_matches(&m.body, path, expected))
        .count();
    let passed = matched >= min_count;
    Ok(ExpectationResult {
        passed,
        matched,
        peeked: msgs.len(),
        detail: if path.is_empty() {
            format!("{matched}/{} messages (expected >= {min_count})", msgs.len())
        } else {
            format!(
                "{matched}/{} messages with {path} = {expected} (expected >= {min_count})",
                msgs.len()
            )
        },
    })
}

fn message_matches(body: &str, path: &str, expected: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        // Non-JSON body: empty path means "count it"; a path can't match.
        return path.is_empty();
    };
    if path.is_empty() {
        return true;
    }
    let Some(found) = lookup_path(&v, path) else { return false };
    match found {
        Value::String(s) => s == expected,
        other => other.to_string() == expected,
    }
}

/// Walk a dot-separated path through objects (and array indices).
pub fn lookup_path<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = v;
    for seg in path.split('.').filter(|s| !s.is_empty()) {
        cur = match cur {
            Value::Object(map) => map.get(seg)?,
            Value::Array(items) => items.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wf(queue: &str, parse_expr: &str) -> (Value, String) {
        let w = json!({
            "definition": {
                "triggers": {
                    "T": {
                        "type": "ServiceProvider",
                        "inputs": {
                            "parameters": { "queueName": queue },
                            "serviceProviderConfiguration": {
                                "serviceProviderId": "/serviceProviders/serviceBus"
                            }
                        }
                    }
                },
                "actions": {
                    "Parse": { "type": "ParseJson", "inputs": { "content": parse_expr } }
                }
            }
        });
        let raw = serde_json::to_string(&w).unwrap();
        (w, raw)
    }

    #[test]
    fn detects_base64_consumer() {
        let (w, raw) = wf(
            "ais.event.ignite",
            "@decodeBase64(triggerBody()?['contentData']?['$content'])",
        );
        let enc = encoding_from_workflow(&w, &raw, "ais.event.ignite", "Get-AddressBook").unwrap();
        assert_eq!(
            enc,
            QueueEncoding::Base64Wrapped { consumer: "Get-AddressBook".into() }
        );
        assert_eq!(enc.content_type(), "application/octet-stream");
    }

    #[test]
    fn detects_raw_json_consumer() {
        let (w, raw) = wf("ais.ignite.counterparty", "@json(item()['contentData'])");
        let enc =
            encoding_from_workflow(&w, &raw, "ais.ignite.counterparty", "Pivot-Cp").unwrap();
        assert_eq!(enc, QueueEncoding::RawJson { consumer: Some("Pivot-Cp".into()) });
        assert_eq!(enc.content_type(), "application/json");
    }

    #[test]
    fn skips_workflow_on_other_queue() {
        let (w, raw) = wf("some.other.queue", "@json(item()['contentData'])");
        assert!(encoding_from_workflow(&w, &raw, "ais.event.ignite", "X").is_none());
    }

    #[test]
    fn null_field_nulls_nested_path() {
        let raw = r#"{ "data": { "msg": { "content": { "CompanyId": 90001 } } } }"#;
        let out = null_field(raw, "data.msg.content.CompanyId").unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["data"]["msg"]["content"]["CompanyId"].is_null());
    }

    #[test]
    fn null_field_rejects_missing_path() {
        let raw = r#"{ "a": 1 }"#;
        let err = null_field(raw, "a.b.c").unwrap_err();
        assert!(err.contains("not"), "unexpected error: {err}");
    }

    #[test]
    fn adaptive_card_preview_extracts_color_text_facts_actions() {
        let body = json!({
            "correlationId": "x",
            "adaptiveCard": {
                "type": "AdaptiveCard",
                "body": [
                    { "type": "TextBlock", "text": "[DEV] ❌ Integration Error",
                      "color": "Attention" },
                    { "type": "TextBlock", "text": "Send-Http-Get-Ignite-AddressBook" },
                    { "type": "FactSet", "facts": [
                        { "title": "Error:", "value": "5xx from JDE" }
                    ] }
                ],
                "actions": [ { "type": "Action.OpenUrl", "title": "Link to Logs" } ]
            }
        })
        .to_string();
        let p = adaptive_card_preview(&body).unwrap();
        assert_eq!(p.accent, "Attention");
        assert_eq!(p.lines.len(), 2);
        assert_eq!(p.facts, vec!["Error: 5xx from JDE"]);
        assert_eq!(p.actions, vec!["Link to Logs"]);
    }

    #[test]
    fn adaptive_card_preview_ignores_plain_messages() {
        assert!(adaptive_card_preview(r#"{ "hello": "world" }"#).is_none());
        assert!(adaptive_card_preview("not json").is_none());
    }

    #[test]
    fn message_matches_string_and_number_values() {
        let body = r#"{ "error": { "cp": "AB" }, "n": 5 }"#;
        assert!(message_matches(body, "error.cp", "AB"));
        assert!(!message_matches(body, "error.cp", "CA03"));
        assert!(message_matches(body, "n", "5"));
        assert!(!message_matches(body, "n", "6"));
        // empty path = count every message
        assert!(message_matches(body, "", "anything"));
    }

    #[test]
    fn lookup_path_walks_arrays() {
        let v = json!({ "items": [ { "id": "a" }, { "id": "b" } ] });
        assert_eq!(lookup_path(&v, "items.1.id"), Some(&json!("b")));
    }
}
