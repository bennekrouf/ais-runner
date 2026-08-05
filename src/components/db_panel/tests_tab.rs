//! Test-suite tab — lists the scenarios saved in the current project and runs
//! them, streaming each step's result as it completes.
//!
//! Scenarios are plain JSON under `<project>/.ais-runner/scenarios`, so this tab
//! deliberately doesn't try to be a step editor: it lists, runs, and opens files
//! for editing. Authoring happens in the user's own editor, where the JSON is
//! diffable and reviewable.

use dioxus::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::services::cosmos_check::{self, CosmosConnection};
use crate::services::scenario::{self, RunContext, Scenario, StepResult, StepStatus};

/// The Service Bus emulator always runs on loopback; `sb_amqp` appends the port.
const SB_HOST: &str = "127.0.0.1";

#[derive(Props, Clone, PartialEq)]
pub struct TestsTabProps {
    pub logic_apps_dir: String,
    pub cosmos_connections: Vec<CosmosConnection>,
    /// Shared panel status line — reused so results surface in the same place
    /// as every other connector action.
    pub status: Signal<Option<(String, bool)>>,
}

#[component]
pub fn TestsTab(props: TestsTabProps) -> Element {
    let project_root = project_root_of(&props.logic_apps_dir);

    let mut scenarios: Signal<Vec<Scenario>> = use_signal(Vec::new);
    let mut load_errors: Signal<Vec<String>> = use_signal(Vec::new);
    // Per-scenario step results, keyed by scenario name.
    let mut results: Signal<HashMap<String, Vec<StepResult>>> = use_signal(HashMap::new);
    let mut running: Signal<Option<String>> = use_signal(|| None);

    let reload = {
        let root = project_root.clone();
        move |_| {
            let (found, errors) = scenario::discover(&root);
            scenarios.set(found);
            load_errors.set(errors);
        }
    };

    // Populate on first render, and whenever the project directory changes.
    use_effect({
        let root = project_root.clone();
        move || {
            let (found, errors) = scenario::discover(&root);
            scenarios.set(found);
            load_errors.set(errors);
        }
    });

    let ctx = RunContext {
        sb_host: SB_HOST.to_string(),
        cosmos_endpoint: cosmos_endpoint_of(&props.cosmos_connections),
        cosmos_key: cosmos_key_of(&props.cosmos_connections),
    };

    let scenario_dir = project_root.join(".ais-runner/scenarios");
    let list = scenarios.read().clone();
    let busy = running.read().clone();

    rsx! {
        div { class: "db-section",
            div { class: "db-section-title-row",
                span { class: "db-section-title", "🧪 Test scenarios" }
                div { style: "display:flex;gap:8px;align-items:center;margin-left:auto",
                    button {
                        class: "btn btn-small",
                        title: "Re-read .ais-runner/scenarios from disk",
                        onclick: reload,
                        "↻ Reload"
                    }
                    button {
                        class: "btn btn-small",
                        title: "Create a starter scenario file and open it in your editor",
                        onclick: {
                            let dir = scenario_dir.clone();
                            let mut status = props.status;
                            move |_| match scaffold_scenario(&dir) {
                                Ok(path) => {
                                    crate::utils::open_in_editor(&path.to_string_lossy());
                                    status.set(Some((format!("Created {}", path.display()), false)));
                                }
                                Err(e) => status.set(Some((e, true))),
                            }
                        },
                        "+ New scenario"
                    }
                }
            }

            for err in load_errors.read().iter() {
                div { class: "settings-status error", "{err}" }
            }

            if list.is_empty() {
                div { class: "empty-state",
                    "No scenarios yet. Create "
                    code { "{scenario_dir.display()}/<name>.json" }
                    " or use “+ New scenario”."
                }
            }

            for scenario_item in list {
                {
                    let name = scenario_item.name.clone();
                    let step_count = scenario_item.steps.len();
                    let source = scenario_item.source.clone();
                    let is_running = busy.as_deref() == Some(name.as_str());
                    // Any scenario is blocked while another one runs: they share
                    // the same emulators, so overlapping runs would corrupt each
                    // other's queues and containers.
                    let blocked = busy.is_some() && !is_running;
                    let steps = results.read().get(&name).cloned().unwrap_or_default();

                    rsx! {
                        div { class: "scenario-card",
                            div { class: "scenario-header",
                                span { class: "scenario-name", "{name}" }
                                span { class: "scenario-count", "{step_count} step(s)" }
                                {summary_badge(&steps, is_running)}
                                div { style: "display:flex;gap:6px;margin-left:auto",
                                    button {
                                        class: "btn btn-small",
                                        title: "Open the scenario JSON in your editor",
                                        onclick: {
                                            let path = source.clone();
                                            move |_| crate::utils::open_in_editor(&path.to_string_lossy())
                                        },
                                        "Edit"
                                    }
                                    button {
                                        class: "btn btn-run btn-small",
                                        disabled: is_running || blocked,
                                        title: if blocked { "Another scenario is running" } else { "Run every step in order" },
                                        onclick: {
                                            let to_run = scenario_item.clone();
                                            let ctx = ctx.clone();
                                            let key = name.clone();
                                            let mut status = props.status;
                                            move |_| {
                                                let to_run = to_run.clone();
                                                let ctx = ctx.clone();
                                                let key = key.clone();
                                                // Clear previous results so a rerun
                                                // never shows a mix of two runs.
                                                results.write().insert(key.clone(), Vec::new());
                                                running.set(Some(key.clone()));
                                                spawn(async move {
                                                    let key_for_step = key.clone();
                                                    let all = scenario::run(&to_run, &ctx, move |r| {
                                                        results
                                                            .write()
                                                            .entry(key_for_step.clone())
                                                            .or_default()
                                                            .push(r);
                                                    })
                                                    .await;

                                                    let failed = all
                                                        .iter()
                                                        .filter(|r| r.status == StepStatus::Failed)
                                                        .count();
                                                    status.set(Some(if failed == 0 {
                                                        (format!("✅ {key}: {} step(s) passed", all.len()), false)
                                                    } else {
                                                        (format!("❌ {key}: {failed} step(s) failed"), true)
                                                    }));
                                                    running.set(None);
                                                });
                                            }
                                        },
                                        if is_running { "Running…" } else { "▶ Run" }
                                    }
                                }
                            }

                            if !scenario_item.description.is_empty() {
                                div { class: "scenario-desc", "{scenario_item.description}" }
                            }

                            if !steps.is_empty() {
                                div { class: "scenario-steps",
                                    for step in steps.iter() {
                                        div { class: step_row_class(step.status),
                                            span { class: "scenario-step-num", "{step.index + 1}" }
                                            span { class: "scenario-step-icon", {step_icon(step.status)} }
                                            span { class: "scenario-step-label", "{step.label}" }
                                            span { class: "scenario-step-detail", "{step.detail}" }
                                            if step.status != StepStatus::Skipped {
                                                span { class: "scenario-step-time", "{step.elapsed_ms}ms" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Overall verdict for a scenario, shown next to its name.
///
/// Reports "passed" only once every step has settled — a run whose steps have
/// all succeeded *so far* is still in progress, and calling it green early
/// would be misleading.
fn summary_badge(steps: &[StepResult], is_running: bool) -> Element {
    if is_running {
        return rsx! { span { class: "scenario-badge running", "running…" } };
    }
    if steps.is_empty() {
        return rsx! { span { class: "scenario-badge idle", "not run" } };
    }
    let failed = steps.iter().filter(|s| s.status == StepStatus::Failed).count();
    let skipped = steps.iter().filter(|s| s.status == StepStatus::Skipped).count();
    if failed == 0 && skipped == 0 {
        rsx! { span { class: "scenario-badge ok", "✅ passed" } }
    } else {
        rsx! { span { class: "scenario-badge fail", "❌ {failed} failed" } }
    }
}

fn step_row_class(status: StepStatus) -> &'static str {
    match status {
        StepStatus::Ok => "scenario-step ok",
        StepStatus::Failed => "scenario-step fail",
        StepStatus::Skipped => "scenario-step skipped",
    }
}

fn step_icon(status: StepStatus) -> &'static str {
    match status {
        StepStatus::Ok => "✅",
        StepStatus::Failed => "❌",
        StepStatus::Skipped => "⊘",
    }
}

/// The project root that holds `.ais-runner/`, given the resolved workflow dir.
///
/// `logic_apps_dir` points at the folder containing the workflow folders, which
/// is usually `<project>/logic_apps`. Scenarios belong beside the project, not
/// inside the deployable folder, so step up one level when that's the shape.
fn project_root_of(logic_apps_dir: &str) -> PathBuf {
    let path = Path::new(logic_apps_dir);
    let is_nested = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n == "logic_apps" || n == "logic-apps")
        .unwrap_or(false);

    if is_nested {
        path.parent().unwrap_or(path).to_path_buf()
    } else {
        path.to_path_buf()
    }
}

fn cosmos_endpoint_of(connections: &[CosmosConnection]) -> String {
    connections
        .iter()
        .map(|c| c.endpoint.trim())
        .find(|e| !e.is_empty())
        .unwrap_or(cosmos_check::EMULATOR_ENDPOINT)
        .to_string()
}

fn cosmos_key_of(connections: &[CosmosConnection]) -> String {
    connections
        .iter()
        .map(|c| c.account_key.trim())
        .find(|k| !k.is_empty())
        .unwrap_or(cosmos_check::EMULATOR_KEY)
        .to_string()
}

/// Write a starter scenario next to any existing ones, without clobbering.
fn scaffold_scenario(dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;

    let mut path = dir.join("new-scenario.json");
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("new-scenario-{n}.json"));
        n += 1;
    }

    const STARTER: &str = r#"{
  "name": "New scenario",
  "description": "Describe what this proves.",
  "vars": {
    "id": "TEST-001"
  },
  "steps": [
    { "action": "drain_queue", "queue": "REPLACE.input.queue" },
    { "action": "drain_queue", "queue": "REPLACE.output.queue" },

    {
      "action": "send_message",
      "queue": "REPLACE.input.queue",
      "body": "{\"id\":\"{{id}}\"}"
    },

    { "action": "wait_for_run", "workflow": "REPLACE-Workflow-Name", "timeout_ms": 60000 },

    {
      "action": "wait_for_message",
      "queue": "REPLACE.output.queue",
      "path": "id",
      "expected": "{{id}}",
      "min_count": 1
    }
  ]
}
"#;

    std::fs::write(&path, STARTER).map_err(|e| e.to_string())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_root_steps_out_of_the_logic_apps_folder() {
        assert_eq!(
            project_root_of("/repo/ais_tom_platform/logic_apps"),
            PathBuf::from("/repo/ais_tom_platform")
        );
        assert_eq!(
            project_root_of("/repo/ais_tom_platform/logic-apps"),
            PathBuf::from("/repo/ais_tom_platform")
        );
    }

    #[test]
    fn project_root_keeps_a_directory_that_is_already_the_root() {
        assert_eq!(
            project_root_of("/repo/workflows"),
            PathBuf::from("/repo/workflows")
        );
    }

    #[test]
    fn cosmos_falls_back_to_the_emulator_when_nothing_is_configured() {
        assert_eq!(cosmos_endpoint_of(&[]), cosmos_check::EMULATOR_ENDPOINT);
        assert_eq!(cosmos_key_of(&[]), cosmos_check::EMULATOR_KEY);
    }

    #[test]
    fn cosmos_prefers_a_configured_connection_over_the_emulator_default() {
        let conn = CosmosConnection {
            connection_name: "cosmos".into(),
            display_name: "cosmos".into(),
            endpoint_key: None,
            key_key: None,
            endpoint: "http://localhost:9999".into(),
            account_key: "abc".into(),
        };
        assert_eq!(cosmos_endpoint_of(&[conn.clone()]), "http://localhost:9999");
        assert_eq!(cosmos_key_of(&[conn]), "abc");
    }
}
