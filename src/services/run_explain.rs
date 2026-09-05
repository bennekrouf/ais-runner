//! Why did this run fail?
//!
//! A Logic Apps run reports a failure at every level *except* the one that
//! broke: the scope says "An action failed. No dependent actions succeeded.",
//! its siblings say they were skipped by a `runAfter` condition, and the action
//! that actually failed inside a `Foreach` carries no error at all — its status
//! lives one level down, in a repetition, and the reason one level further, in
//! the response body of the workflow it invoked.
//!
//! This module walks that trail once, for every caller: the scenario runner
//! puts the one-line summary on the failed step, and the run detail panel can
//! show the same conclusion without re-deriving it.

use crate::services::workflows::{self, ActionItem};

/// The root cause of a failed run.
#[derive(Debug, Clone, PartialEq)]
pub struct Cause {
    /// One line, ready to put on a failed step.
    pub summary: String,
    /// Containers that only restated the failure, outermost first. Empty when
    /// the failing action was top level.
    pub trail: Vec<String>,
}

/// Codes an action reports when something *inside* it failed — never the
/// reason, only the echo.
const CONTAINER_CODES: [&str; 2] = ["ActionFailed", "ActionDependencyFailed"];

/// Explain a failed run, following repetitions and invoked workflows.
pub async fn explain(workflow: &str, run_id: &str) -> Option<Cause> {
    let actions = workflows::list_actions(workflow, run_id).await.ok()?;
    let root = pick_root(&actions)?;
    let trail = containers(&actions, &root.name);

    if let Some(summary) = describe(root) {
        return Some(Cause { summary, trail });
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
                return Some(Cause {
                    summary: format!("'{}' failed: {text}", root.name),
                    trail,
                });
            }
            if let Some(detail) = outputs_of(workflow, run_id, &root.name, Some(&failed.name)).await
            {
                return Some(Cause {
                    summary: format!("'{}' failed: {detail}", root.name),
                    trail,
                });
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
    let summary = match (detail, code.is_empty()) {
        (Some(d), _) => format!("'{}' failed: {d}", root.name),
        // Nothing to read: the code alone still beats a bare status.
        (None, false) => format!("'{}' failed: {code}", root.name),
        (None, true) => format!("'{}' failed", root.name),
    };
    Some(Cause { summary, trail })
}

/// Read an action's (or one repetition's) outputs and turn them into a reason.
///
/// The listing endpoints carry no payload — only a SAS link on the individual
/// record — so this is two hops: fetch the record, then follow `outputsLink`.
async fn outputs_of(
    workflow: &str,
    run_id: &str,
    action: &str,
    rep: Option<&str>,
) -> Option<String> {
    let base = format!(
        "{}/workflows/{}/runs/{}/actions/{}",
        workflows::BASE,
        workflow,
        run_id,
        action
    );
    let url = match rep {
        Some(r) => format!("{base}/repetitions/{r}"),
        None => base,
    };
    let record: serde_json::Value = serde_json::from_str(&fetch(&url).await?).ok()?;
    let uri = record
        .pointer("/properties/outputsLink/uri")
        .and_then(|u| u.as_str())?;
    explain_outputs(&fetch(uri).await?)
}

/// The failed action worth reporting: the first one carrying a real error, or
/// a container as a last resort.
fn pick_root(actions: &[ActionItem]) -> Option<&ActionItem> {
    let mut fallback = None;
    for a in actions.iter().filter(|a| a.properties.status == "Failed") {
        let code = a
            .properties
            .error
            .as_ref()
            .and_then(|e| e.code.clone())
            .unwrap_or_default();
        if CONTAINER_CODES.contains(&code.as_str()) {
            fallback.get_or_insert(a);
        } else {
            return Some(a);
        }
    }
    fallback
}

/// Failed containers other than the root — the "it failed because something in
/// it failed" chain, kept for the detail view.
fn containers(actions: &[ActionItem], root: &str) -> Vec<String> {
    actions
        .iter()
        .filter(|a| a.properties.status == "Failed" && a.name != root)
        .map(|a| a.name.clone())
        .collect()
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

async fn fetch(url: &str) -> Option<String> {
    reqwest::get(url).await.ok()?.text().await.ok()
}

/// Turn an action's raw outputs into a reason.
///
/// A failed `Invoke a workflow` returns the child's HTTP response: the status
/// code, the child's name in `x-ms-workflow-name`, and — for this platform's
/// workflows — the error the child built under `ais.workflow.error`. Falls back
/// to the usual `error`/`message` shapes so a plain HTTP action reads too.
pub fn explain_outputs(body: &str) -> Option<String> {
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
        .or_else(|| inner.get("code").and_then(|c| c.as_str()));

    let mut out = String::new();
    if let Some(s) = status {
        out.push_str(&s.to_string());
    }
    if let Some(c) = callee {
        out.push_str(&format!(
            "{}from {c}",
            if out.is_empty() { "" } else { " " }
        ));
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
        ActionItem {
            name: name.into(),
            properties: ActionProperties {
                status: status.into(),
                start_time: None,
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
        assert_eq!(containers(&actions, &root.name), ["Module_=_Companies"]);
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
}
