use std::collections::{HashMap, HashSet};
use chrono::Utc;
use dioxus::prelude::*;

use crate::components::log_panel::{LogLevel, LogLine};
use crate::services::{
    azure_cli, connection_diag, payload,
    workflows::{self, ActionItem, RunItem, WorkflowItem},
};
use crate::utils::{filter_cleared, make_push, sweep_run_history};

pub fn handle_open_dialog(
    name: String,
    trigger_name: String,
    trigger_type: String,
    dir: &str,
    mut selected_wf: Signal<Option<String>>,
    mut source_text: Signal<String>,
    mut active_tab: Signal<String>,
    az_status: Signal<Option<Result<String, azure_cli::AzError>>>,
    mut run_dialog: Signal<Option<(String, String, String, String)>>,
    log_lines: Signal<Vec<LogLine>>,
) {
    let mut push = make_push(log_lines);
    if !matches!(az_status.read().as_ref(), Some(Ok(_))) {
        push(
            "Cannot run workflow: not logged into Azure. Please click '⚠ az login' in the toolbar first.".into(),
            LogLevel::Error,
        );
        return;
    }
    selected_wf.set(Some(name.clone()));
    active_tab.set("Run".into());
    let src_path = workflows::resolve_logic_apps_dir(dir).join(&name).join("workflow.json");
    source_text.set(match std::fs::read_to_string(&src_path) {
        Ok(txt) => txt,
        Err(e)  => format!("// could not read {}: {}", src_path.display(), e),
    });
    let suggested = payload::suggest_payload(dir, &name);
    run_dialog.set(Some((name, trigger_name, trigger_type, suggested)));
}

pub fn handle_trigger_from_detail(
    dir: &str,
    workflows_sig: Signal<Vec<WorkflowItem>>,
    selected_wf: Signal<Option<String>>,
    az_status: Signal<Option<Result<String, azure_cli::AzError>>>,
    mut run_dialog: Signal<Option<(String, String, String, String)>>,
    log_lines: Signal<Vec<LogLine>>,
) {
    let mut push = make_push(log_lines);
    if !matches!(az_status.read().as_ref(), Some(Ok(_))) {
        push(
            "Cannot run workflow: not logged into Azure. Please click '⚠ az login' in the toolbar first.".into(),
            LogLevel::Error,
        );
        return;
    }
    let Some(wf_name) = selected_wf.read().clone() else { return };
    let Some(wf) = workflows_sig.read().iter().find(|w| w.name == wf_name).cloned() else { return };
    let suggested = payload::suggest_payload(dir, &wf.name);
    run_dialog.set(Some((wf.name, wf.trigger_name, wf.trigger_type, suggested)));
}

pub fn handle_run(
    name: String,
    trigger_name: String,
    trigger_type: String,
    body: String,
    dir: &str,
    mut runs: Signal<Vec<RunItem>>,
    mut actions: Signal<Vec<ActionItem>>,
    log_lines: Signal<Vec<LogLine>>,
    mut running_wfs: Signal<HashSet<String>>,
    mut active_tab: Signal<String>,
    mut traced_wfs: Signal<HashSet<String>>,
    mut cleared_wfs: Signal<HashMap<String, String>>,
    mut run_dialog: Signal<Option<(String, String, String, String)>>,
) {
    run_dialog.set(None);
    active_tab.set("Run".into());
    traced_wfs.write().insert(name.clone());

    let trigger_ts = Utc::now().to_rfc3339();
    cleared_wfs.write().insert(name.clone(), trigger_ts.clone());

    let wf         = name.clone();
    let cleared_at = Some(trigger_ts);
    let dir_diag   = dir.to_string();
    let mut push   = make_push(log_lines);
    let cleared    = cleared_wfs;
    let is_recurrence = !matches!(trigger_type.to_lowercase().as_str(), "request" | "http");

    push(format!("Triggering: {}", wf), LogLevel::Info);
    running_wfs.write().insert(wf.clone());

    spawn(async move {
        let not_found_hints = |push: &mut dyn FnMut(String, LogLevel), dir: &str, wf: &str, hints: Vec<String>| {
            for (conn, key) in connection_diag::missing_endpoints_for_workflow(dir, wf) {
                push(
                    format!("  hint: '{}' has empty '{}' in local.settings.json — set it and restart func", conn, key),
                    LogLevel::Warn,
                );
            }
            for msg in hints { push(msg, LogLevel::Warn); }
        };

        // Fire
        if is_recurrence {
            match workflows::run_trigger_direct(&wf, &trigger_name, &body).await {
                Ok(_)  => push(format!("Run triggered ({})", trigger_type), LogLevel::Ok),
                Err(e) => {
                    let es = e.to_string();
                    push(format!("Trigger error: {}", es), LogLevel::Error);
                    if es.to_lowercase().contains("could not be found") || es.contains("WorkflowNotFound") {
                        let hints = workflows::not_found_hints(&wf).await;
                        not_found_hints(&mut push, &dir_diag, &wf, hints);
                    }
                    running_wfs.write().remove(&wf);
                    return;
                }
            }
        } else {
            match workflows::get_callback_url(&wf, &trigger_name).await {
                Ok(url) => {
                    push(format!("$ curl -X POST \"{}\"", url), LogLevel::Info);
                    match workflows::trigger_workflow(&url, &body).await {
                        Ok(run_id) => push(format!("Run started: {}", run_id), LogLevel::Ok),
                        Err(e) => {
                            let es = e.to_string();
                            push(format!("Trigger error: {}", es), LogLevel::Error);
                            if es.to_lowercase().contains("could not be found") || es.contains("WorkflowNotFound") {
                                let hints = workflows::not_found_hints(&wf).await;
                                not_found_hints(&mut push, &dir_diag, &wf, hints);
                            }
                            running_wfs.write().remove(&wf);
                            return;
                        }
                    }
                }
                Err(e) => {
                    let es = e.to_string();
                    push(format!("Callback URL error: {}", es), LogLevel::Error);
                    if es.to_lowercase().contains("could not be found") || es.contains("WorkflowNotFound") {
                        let hints = workflows::not_found_hints(&wf).await;
                        not_found_hints(&mut push, &dir_diag, &wf, hints);
                    }
                    running_wfs.write().remove(&wf);
                    return;
                }
            }
        }

        // Poll until terminal
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(300);
        loop {
            if let Ok(r) = workflows::list_runs(&wf).await {
                let r = filter_cleared(r, cleared_at.as_deref());
                if let Some(latest) = r.first() {
                    let run_name = latest.name.clone();
                    runs.set(r.clone());
                    if let Ok(a) = workflows::list_actions(&wf, &run_name).await {
                        let all_terminal = !a.is_empty() && a.iter().all(|act| {
                            matches!(act.properties.status.to_lowercase().as_str(),
                                "succeeded" | "failed" | "skipped" | "timedout" | "cancelled")
                        });
                        actions.set(a.clone());
                        if all_terminal {
                            let ok  = a.iter().filter(|x| x.properties.status.to_lowercase() == "succeeded").count();
                            let err = a.iter().filter(|x| x.properties.status.to_lowercase() == "failed").count();
                            for act in &a {
                                let ms = workflows::duration_ms(&act.properties.start_time, &act.properties.end_time).unwrap_or(0);
                                let icon = match act.properties.status.to_lowercase().as_str() {
                                    "succeeded" => "✅", "failed" => "❌", "skipped" => "⏭", _ => "⏳",
                                };
                                push(format!("  {} {}  {}ms", icon, act.name, ms), LogLevel::Info);
                            }
                            if err > 0 {
                                push(format!("Run complete — {} ok, {} failed", ok, err), LogLevel::Error);
                            } else {
                                push(
                                    format!("Run complete — {} actions in {:.1}s", ok,
                                        workflows::duration_ms(
                                            &a.first().and_then(|x| x.properties.start_time.clone()),
                                            &a.last().and_then(|x| x.properties.end_time.clone()),
                                        ).unwrap_or(0) as f64 / 1000.0),
                                    LogLevel::Ok,
                                );
                            }
                            break;
                        }
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                push("Live poll timed out after 5 min".into(), LogLevel::Warn);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        }
        running_wfs.write().remove(&wf);
        let names: Vec<String> = runs.read().iter().map(|r| r.name.clone()).collect();
        sweep_run_history(names, &mut traced_wfs, &cleared).await;
    });
}
