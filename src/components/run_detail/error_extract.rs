//! Error-message extraction: the Logic Apps management API omits
//! `properties.error` for several failure modes, so we dig the real message
//! out of the expanded action detail or the Functions host stdout.

use std::collections::HashMap;
use crate::components::log_panel::LogLine;

// ── Failed-action message extraction ──────────────────────────────────────
//
// The action listing endpoint omits `properties.error` for some failure
// modes — most notably ParseJson, where the runtime stuffs the actual
// schema-mismatch message into the outputs blob instead. We probe a handful
// of well-known paths in the expanded action detail and return the first
// non-empty string we find. Order is deliberate: top-level error first,
// then outputs.body.error (the ParseJson shape), then any nested message.
pub(super) fn extract_error_from_detail(v: &serde_json::Value) -> Option<String> {
    // Paths checked, in priority order, against the detail object returned
    // by `get_action_detail` (which has already inlined inputs/outputs).
    const PATHS: &[&str] = &[
        "/properties/error/message",
        "/properties/outputs/body/error/message",
        "/properties/outputs/body/message",
        "/properties/outputs/error/message",
        "/properties/outputs/message",
        "/error/message",
    ];
    for p in PATHS {
        if let Some(s) = v.pointer(p).and_then(|x| x.as_str()) {
            let s = s.trim();
            if !s.is_empty() { return Some(s.to_string()); }
        }
    }
    // Fallback: if outputs.body is itself a JSON-encoded string from a
    // ParseJson failure, the message may be the body text directly.
    if let Some(s) = v.pointer("/properties/outputs/body").and_then(|x| x.as_str()) {
        let s = s.trim();
        if !s.is_empty() { return Some(s.to_string()); }
    }
    // Last resort: surface the error code so the user at least sees *what*
    // kind of failure it was rather than a silent red row. The runtime puts
    // the code at one of two paths depending on the action type — Foreach
    // and Scope put it at properties.code, leaf actions at properties.error.code.
    let code = v.pointer("/properties/error/code")
        .or_else(|| v.pointer("/properties/code"))
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    code.map(|c| {
        // "NotSpecified" by itself is unhelpful, but it means different things
        // depending on the action type — Logic Apps stamps it on scope-type
        // actions (Foreach / Scope / Until / If) when a child action failed,
        // and on expression-evaluation actions like ParseJson / Compose /
        // Set variable when schema validation or template parsing fails.
        // For the latter the runtime intentionally does NOT attach the
        // message to the action record — it goes to the Functions host
        // stdout instead (known Logic Apps Standard limitation). Tell the
        // user where to look so they don't think this is an ais-runner bug.
        let atype = v.pointer("/properties/type")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        let is_scope = matches!(atype, "Foreach" | "Scope" | "Until" | "If");
        let is_expr  = matches!(atype, "ParseJson" | "Compose" | "InitializeVariable"
                              | "SetVariable" | "AppendToStringVariable" | "AppendToArrayVariable"
                              | "IncrementVariable" | "DecrementVariable");
        if c.eq_ignore_ascii_case("NotSpecified") && is_scope {
            format!("{c} — a child action failed; expand to see which.")
        } else if c.eq_ignore_ascii_case("NotSpecified") && is_expr {
            format!(
                "{c} — Logic Apps Standard does not expose {atype} errors via API. \
                 Check the func start console (Logs → console) for the schema/expression message."
            )
        } else if c.eq_ignore_ascii_case("NotSpecified") {
            format!(
                "{c} — runtime did not attach a message. \
                 Check the func start console for action-level errors."
            )
        } else {
            c.to_string()
        }
    })
}

#[cfg(test)]
mod extract_error_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn picks_top_level_error_message_first() {
        let v = json!({
            "properties": {
                "error": { "message": "top-level", "code": "X" },
                "outputs": { "body": { "error": { "message": "deeper" } } },
            }
        });
        assert_eq!(extract_error_from_detail(&v).as_deref(), Some("top-level"));
    }

    #[test]
    fn falls_through_to_outputs_body_error_for_parsejson_shape() {
        let v = json!({
            "properties": {
                "outputs": {
                    "body": { "error": { "message": "Invalid type. Expected Integer but got String." } }
                }
            }
        });
        assert_eq!(
            extract_error_from_detail(&v).as_deref(),
            Some("Invalid type. Expected Integer but got String."),
        );
    }

    #[test]
    fn falls_back_to_code_when_no_message_found() {
        let v = json!({ "properties": { "error": { "code": "BadRequest" } } });
        assert_eq!(extract_error_from_detail(&v).as_deref(), Some("BadRequest"));
    }

    #[test]
    fn falls_back_to_top_level_properties_code() {
        // For_each_page shape: code is at properties.code, no error object.
        let v = json!({ "properties": { "status": "Failed", "code": "ActionFailed" } });
        assert_eq!(extract_error_from_detail(&v).as_deref(), Some("ActionFailed"));
    }

    #[test]
    fn notspecified_on_scope_is_annotated_with_child_hint() {
        let v = json!({
            "properties": { "status": "Failed", "code": "NotSpecified", "type": "Foreach" }
        });
        let msg = extract_error_from_detail(&v).unwrap();
        assert!(msg.contains("NotSpecified"));
        assert!(msg.contains("child"), "expected child-action hint, got: {msg}");
    }

    #[test]
    fn notspecified_on_parsejson_points_at_func_console() {
        let v = json!({
            "properties": { "status": "Failed", "code": "NotSpecified", "type": "ParseJson" }
        });
        let msg = extract_error_from_detail(&v).unwrap();
        assert!(msg.contains("ParseJson"), "expected action type in hint, got: {msg}");
        assert!(msg.contains("func start") || msg.contains("console"),
                "expected console hint, got: {msg}");
    }

    #[test]
    fn returns_none_when_no_error_information_present() {
        let v = json!({ "properties": { "status": "Succeeded" } });
        assert_eq!(extract_error_from_detail(&v), None);
    }
}


// ── Log-derived action error extraction ───────────────────────────────────
//
// Logic Apps Standard does not write ParseJson / Compose / expression-evaluation
// failures to the management API — they only appear in the Functions host
// stdout. We've already captured that stdout in `log_lines`; this builds a
// per-action lookup so the action row can show the real error inline.
//
// Heuristic: walk the workflow-filtered log lines (already pre-filtered in
// RunDetail to those mentioning the workflow name), find ones that mention
// any action name *and* an error keyword, and keep the most recent match per
// action. Tolerant by design — every Logic Apps runtime version phrases the
// failure line slightly differently ("Action 'X' failed:", "action='X' …
// Exception:", schema-validation lines that name the action somewhere in the
// middle of the message, etc.).
pub(super) fn build_action_error_map(logs: &[LogLine]) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    // Walk newest first so the latest matching line wins per action. The
    // earlier lines are the "starting…" / "evaluating…" traces we don't want.
    for line in logs.iter().rev() {
        if let Some((action, msg)) = parse_action_error_line(&line.msg) {
            out.entry(action).or_insert(msg);
        }
    }
    out
}

/// Try to pull out (action_name, error_message) from a single log line.
/// Returns `None` if the line doesn't look like an action-level failure.
fn parse_action_error_line(line: &str) -> Option<(String, String)> {
    let lower = line.to_lowercase();
    // Filter: must mention an error/failure keyword. Pure "info" lines about
    // an action's success don't make it into the map.
    let has_err_keyword = lower.contains("failed")
        || lower.contains("error")
        || lower.contains("exception")
        || lower.contains("invalid")
        || lower.contains("schema validation");
    if !has_err_keyword { return None; }

    // Most Logic Apps runtime error lines name the action in single quotes,
    // e.g. "Action 'Restrictive_Parse_JSON' failed: ..." or
    // "action 'X' status 'Failed'. Exception: ...". Use the single-quoted
    // token immediately after the word "action" as a strong signal.
    let action = extract_quoted_after(line, "action");
    let action = action.or_else(|| extract_quoted_after(line, "Action"));
    let action = action?;

    // Trim known fluff to surface the bit the user actually cares about.
    let msg = clean_action_error_message(line, &action);
    if msg.trim().is_empty() { return None; }
    Some((action, msg))
}

/// Find the first single-quoted substring that follows a marker word.
/// Returns the inner text (without the quotes). Case-sensitive on the marker.
fn extract_quoted_after(line: &str, marker: &str) -> Option<String> {
    let idx = line.find(marker)?;
    let tail = &line[idx + marker.len()..];
    let q1 = tail.find('\'')?;
    let after_q1 = &tail[q1 + 1..];
    let q2 = after_q1.find('\'')?;
    let inner = &after_q1[..q2];
    if inner.is_empty() { None } else { Some(inner.to_string()) }
}

/// Cut the action-error line down to the part the user cares about: take
/// everything after "<action>'" so the leading "Action 'X'" boilerplate is
/// dropped. If no obvious cut point exists, return the raw line.
fn clean_action_error_message(line: &str, action: &str) -> String {
    let needle = format!("'{}'", action);
    if let Some(i) = line.find(&needle) {
        let after = &line[i + needle.len()..];
        // Strip leading punctuation/spaces — ":" / "." / " " / ","
        let trimmed = after.trim_start_matches(|c: char|
            c.is_whitespace() || c == ':' || c == '.' || c == ',' || c == '-'
        );
        if !trimmed.is_empty() { return trimmed.to_string(); }
    }
    line.trim().to_string()
}

#[cfg(test)]
mod log_scrape_tests {
    use super::*;
    use crate::components::log_panel::LogLevel;

    fn mk(msg: &str) -> LogLine {
        LogLine { time: "00:00:00".into(), msg: msg.into(), level: LogLevel::Error }
    }

    #[test]
    fn picks_parsejson_validation_message_from_runtime_log() {
        let logs = vec![mk(
            "[2026-06-19T20:31:02Z] Action 'Restrictive_Parse_JSON' failed: \
             Invalid type. Expected Integer but got String at #/properties/age",
        )];
        let m = build_action_error_map(&logs);
        let v = m.get("Restrictive_Parse_JSON").unwrap();
        assert!(v.contains("Invalid type"), "got: {v}");
        assert!(v.contains("#/properties/age"), "got: {v}");
    }

    #[test]
    fn ignores_success_lines_even_if_they_name_the_action() {
        let logs = vec![mk("Action 'X' completed successfully")];
        assert!(build_action_error_map(&logs).is_empty());
    }

    #[test]
    fn most_recent_line_per_action_wins() {
        let logs = vec![
            mk("Action 'X' failed: first try"),
            mk("Action 'X' failed: retry attempt"),
        ];
        // We walk newest-first; the SECOND entry is newer in the vec, so it wins.
        let m = build_action_error_map(&logs);
        assert!(m.get("X").unwrap().contains("retry attempt"));
    }

    #[test]
    fn skips_lines_that_dont_name_an_action_in_quotes() {
        let logs = vec![mk("[ERROR] something failed somewhere")];
        assert!(build_action_error_map(&logs).is_empty());
    }
}

