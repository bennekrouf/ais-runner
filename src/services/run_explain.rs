//! Why did this run fail?
//!
//! A Logic Apps run reports a failure at every level *except* the one that
//! broke: the scope says "An action failed. No dependent actions succeeded.",
//! its siblings say they were skipped by a `runAfter` condition, and the action
//! that actually failed inside a `Foreach` carries no error at all — its status
//! lives one level down, in a repetition, and the reason one level further, in
//! the response body of the workflow it invoked.
//!
//! This module walks that trail once, for every caller.

use std::time::Duration;

use crate::services::workflows::{self, ActionItem};

/// Codes an action reports when something *inside* it failed — never the
/// reason, only the echo.
const CONTAINER_CODES: [&str; 2] = ["ActionFailed", "ActionDependencyFailed"];

/// Total time this may spend talking to the runtime before giving up.
///
/// It runs on the failure path of a scenario step: the step has already failed
/// and this only decorates the message. `reqwest` applies no timeout of its
/// own, and `poll_until`'s deadline is not in play — that arm has already left
/// the poll loop — so without a bound here a wedged func host hangs the whole
/// scenario, forever, on the one path most likely to meet a wedged func host.
const BUDGET: Duration = Duration::from_secs(10);

/// One line naming the action that actually broke a failed run, ready to put on
/// a failed step. `None` when the runtime will not say.
pub async fn explain(workflow: &str, run_id: &str) -> Option<String> {
    tokio::time::timeout(BUDGET, walk(workflow, run_id))
        .await
        .ok()
        .flatten()
}

async fn walk(workflow: &str, run_id: &str) -> Option<String> {
    let actions = workflows::list_actions(workflow, run_id).await.ok()?;
    let root = pick_root(&actions)?;

    if let Some(summary) = describe(root) {
        return Some(summary);
    }

    // No error on the action itself. Two shapes to follow, in order: a Foreach
    // child, whose status and payload live under /repetitions, and a top-level
    // action, whose own detail carries the outputs link (its /repetitions
    // endpoint 404s).
    if let Ok(reps) = workflows::list_repetitions(workflow, run_id, &root.name).await {
        if let Some(failed) = reps.iter().find(|r| r.properties.status == "Failed") {
            if let Some(text) = failed
                .properties
                .error
                .as_ref()
                .and_then(|e| error_text(e.code.as_deref(), e.message.as_deref()))
            {
                return Some(format!("'{}' failed: {text}", root.name));
            }
            if let Some(detail) = outputs_of(workflow, run_id, &root.name, Some(&failed.name)).await
            {
                return Some(format!("'{}' failed: {detail}", root.name));
            }
        }
    }
    let detail = outputs_of(workflow, run_id, &root.name, None).await;
    let code = root
        .properties
        .code
        .clone()
        .or_else(|| root.properties.error.as_ref().and_then(|e| e.code.clone()))
        .unwrap_or_default();
    Some(match (detail, code.is_empty()) {
        (Some(d), _) => format!("'{}' failed: {d}", root.name),
        // Nothing to read: the code alone still beats a bare status.
        (None, false) => format!("'{}' failed: {code}", root.name),
        (None, true) => format!("'{}' failed", root.name),
    })
}

/// Read an action's (or one repetition's) outputs and turn them into a reason.
///
/// `get_action_detail_at` already does the two hops — fetch the record, follow
/// `outputsLink` — and inlines the blob under `/properties/outputs`, parsed
/// when it was JSON and as raw text when it was not.
async fn outputs_of(
    workflow: &str,
    run_id: &str,
    action: &str,
    rep: Option<&str>,
) -> Option<String> {
    let detail = workflows::get_action_detail_at(workflow, run_id, action, rep)
        .await
        .ok()?;
    let outputs = detail.pointer("/properties/outputs")?;
    match outputs.as_str() {
        Some(text) => explain_outputs(text),
        None => explain_outputs(&outputs.to_string()),
    }
}

/// The failed action worth reporting: the earliest one carrying a real error,
/// or the earliest container as a last resort.
///
/// Ordered by start time rather than by position in the response. A run with
/// two independent failures should name the one that happened first, not
/// whichever the management API chose to list first — the ordering of that
/// list is not part of its contract.
fn pick_root(actions: &[ActionItem]) -> Option<&ActionItem> {
    let mut failed: Vec<&ActionItem> = actions
        .iter()
        .filter(|a| a.properties.status == "Failed")
        .collect();
    // RFC 3339 in UTC, so lexical order is chronological order. Records with no
    // start time sort last: they carry no evidence either way, and `~` is above
    // every digit and letter a timestamp can start with.
    failed.sort_by_key(|a| {
        a.properties
            .start_time
            .clone()
            .unwrap_or_else(|| "~".to_string())
    });

    let is_echo = |a: &ActionItem| {
        a.properties
            .error
            .as_ref()
            .and_then(|e| e.code.as_deref())
            .is_some_and(|c| CONTAINER_CODES.contains(&c))
    };
    failed
        .iter()
        .find(|a| !is_echo(a))
        .or(failed.first())
        .copied()
}

/// "'Name' failed: Code — message", when the action knows why it failed.
fn describe(a: &ActionItem) -> Option<String> {
    let err = a.properties.error.as_ref()?;
    let text = error_text(err.code.as_deref(), err.message.as_deref())?;
    Some(format!("'{}' failed: {text}", a.name))
}

fn error_text(code: Option<&str>, message: Option<&str>) -> Option<String> {
    let code = code.unwrap_or_default().trim();
    let message = message.unwrap_or_default().trim();
    match (code.is_empty(), message.is_empty()) {
        (true, true) => None,
        (false, false) => Some(format!("{code} — {message}")),
        (true, false) => Some(message.to_string()),
        (false, true) => Some(code.to_string()),
    }
}

/// Turn an action's raw outputs into a reason.
///
/// A failed `Invoke a workflow` returns the child's HTTP response: the status
/// code, the child's name in `x-ms-workflow-name`, and — for this platform's
/// workflows — the error the child built under `ais.workflow.error`. Falls back
/// to the usual `error`/`message` shapes, and to a plain string body, so an
/// ordinary HTTP action reads too.
fn explain_outputs(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let status = v["statusCode"].as_i64();
    let callee = v
        .pointer("/headers/x-ms-workflow-name")
        .and_then(|n| n.as_str());
    let body = &v["body"];
    let inner = body
        .get("ais.workflow.error")
        .or_else(|| body.get("error"))
        .unwrap_or(body);
    let action = inner.get("action").and_then(|a| a.as_str());
    let message = inner
        .get("message")
        .and_then(|m| m.as_str())
        .or_else(|| inner.get("code").and_then(|c| c.as_str()))
        // A string body *is* the message. Without this the only text the
        // response carried is dropped and the reader gets a bare status code.
        .or_else(|| inner.as_str())
        .map(str::trim)
        .filter(|m| !m.is_empty());

    let mut out = String::new();
    if let Some(s) = status {
        out.push_str(&s.to_string());
    }
    if let Some(c) = callee {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&format!("from {c}"));
    }
    match (action, message) {
        (Some(a), Some(m)) => out.push_str(&format!(" — action '{a}': {m}")),
        (None, Some(m)) => out.push_str(&format!(" — {m}")),
        (Some(a), None) => out.push_str(&format!(" — action '{a}'")),
        (None, None) => {}
    }
    let out = out.trim().to_string();
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::workflows::{ActionError, ActionProperties};

    fn action(name: &str, status: &str, code: Option<&str>, msg: Option<&str>) -> ActionItem {
        at(name, status, code, msg, None)
    }

    fn at(
        name: &str,
        status: &str,
        code: Option<&str>,
        msg: Option<&str>,
        start: Option<&str>,
    ) -> ActionItem {
        ActionItem {
            name: name.into(),
            properties: ActionProperties {
                status: status.into(),
                start_time: start.map(|s| s.into()),
                end_time: None,
                error: code.map(|c| ActionError {
                    code: Some(c.into()),
                    message: msg.map(|m| m.into()),
                }),
                code: None,
                action_type: None,
            },
        }
    }

    /// AB 03's shape: the If scope reports first and says nothing useful.
    #[test]
    fn the_action_with_a_real_error_beats_the_scope_that_echoes_it() {
        let actions = [
            action(
                "Module_=_Companies",
                "Failed",
                Some("ActionFailed"),
                Some("An action failed. No dependent actions succeeded."),
            ),
            action("Filter_Errors", "Succeeded", None, None),
            action(
                "Execute_Storage_stored_procedure",
                "Failed",
                Some("BadRequest"),
                Some("The required OAuth authentication property 'tenant' is missing."),
            ),
        ];
        let root = pick_root(&actions).unwrap();
        assert_eq!(root.name, "Execute_Storage_stored_procedure");
        let summary = describe(root).unwrap();
        assert!(
            summary.contains("BadRequest") && summary.contains("tenant"),
            "{summary}"
        );
    }

    /// AB 05's shape: the only failed action outside containers carries no
    /// error at all, so `describe` must decline and let the caller drill down.
    #[test]
    fn an_action_without_an_error_is_not_described_from_the_action_alone() {
        let a = action(
            "Invoke_AIS-GenericTransform_for_msg_tracking",
            "Failed",
            None,
            None,
        );
        assert!(describe(&a).is_none());
        let actions = [
            action(
                "Scope_Processing",
                "Failed",
                Some("ActionFailed"),
                Some("An action failed."),
            ),
            a,
        ];
        assert_eq!(
            pick_root(&actions).unwrap().name,
            "Invoke_AIS-GenericTransform_for_msg_tracking"
        );
    }

    /// Two independent failures: the answer is the one that happened first, not
    /// the one the management API happened to list first.
    #[test]
    fn the_earliest_failure_wins_regardless_of_list_order() {
        let actions = [
            at(
                "Later",
                "Failed",
                Some("Conflict"),
                Some("b"),
                Some("2024-05-02T10:00:02Z"),
            ),
            at(
                "Earlier",
                "Failed",
                Some("BadRequest"),
                Some("a"),
                Some("2024-05-02T10:00:01Z"),
            ),
        ];
        assert_eq!(pick_root(&actions).unwrap().name, "Earlier");
    }

    /// The 400 a child workflow returns, as the runtime actually writes it.
    #[test]
    fn a_child_workflow_response_names_the_callee_and_its_failed_action() {
        let body = r#"{"statusCode":400,
            "headers":{"x-ms-workflow-name":"AIS-GenericTransform","Content-Length":"0"},
            "body":{"ais.workflow.error":{"action":"Condition",
                "message":"An action failed. No dependent actions succeeded.","messageDetails":""}}}"#;
        let out = explain_outputs(body).unwrap();
        assert_eq!(
            out,
            "400 from AIS-GenericTransform — action 'Condition': An action failed. No dependent \
             actions succeeded."
        );
    }

    #[test]
    fn a_plain_http_failure_still_reads() {
        let out = explain_outputs(r#"{"statusCode":500,"body":{"error":{"message":"boom"}}}"#);
        assert_eq!(out.unwrap(), "500 — boom");
        // Nothing usable in, nothing invented out.
        assert!(explain_outputs("{}").is_none());
        assert!(explain_outputs("not json").is_none());
    }

    /// A string body is the only text the response carried; it used to be
    /// dropped on the floor, leaving the reader a bare status code.
    #[test]
    fn a_string_body_is_the_message() {
        assert_eq!(
            explain_outputs(r#"{"statusCode":502,"body":"upstream refused the connection"}"#)
                .unwrap(),
            "502 — upstream refused the connection"
        );
        // Still nothing invented when the body is empty.
        assert_eq!(
            explain_outputs(r#"{"statusCode":502,"body":"   "}"#).unwrap(),
            "502"
        );
    }
}
