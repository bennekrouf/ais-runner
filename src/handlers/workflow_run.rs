use std::collections::{HashMap, HashSet};
use chrono::Utc;
use dioxus::prelude::*;

use crate::components::log_panel::{LogLevel, LogLine};
use crate::services::{
    azure_cli, connection_diag, payload, sb_check,
    workflows::{self, ActionItem, RunItem, WorkflowItem},
};
use crate::utils::{filter_cleared, make_push, sweep_run_history};

fn blob_container_for(dir: &str, name: &str, trigger_type: &str) -> Option<String> {
    if trigger_type.to_lowercase() != "serviceprovider" { return None; }
    let src_path = workflows::resolve_logic_apps_dir(dir).join(name).join("workflow.json");
    let json = std::fs::read_to_string(&src_path).ok()?;
    workflows::read_blob_trigger_info(&json).map(|(container, _)| container)
}

pub fn handle_open_dialog(
    name: String,
    trigger_name: String,
    trigger_type: String,
    dir: &str,
    mut selected_wf: Signal<Option<String>>,
    mut source_text: Signal<String>,
    mut active_tab: Signal<String>,
    az_status: Signal<Option<Result<String, azure_cli::AzError>>>,
    mut run_dialog: Signal<Option<(String, String, String, String, Option<String>, Option<String>)>>,
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
    let wf_text = match std::fs::read_to_string(&src_path) {
        Ok(txt) => txt,
        Err(e)  => format!("// could not read {}: {}", src_path.display(), e),
    };
    source_text.set(wf_text.clone());
    let blob_container = blob_container_for(dir, &name, &trigger_type);
    let queue_name     = sb_check::trigger_queue_for(dir, &name).map(|(_fqdn, q)| q);
    let suggested      = payload::suggest_payload(dir, &name);
    run_dialog.set(Some((name, trigger_name, trigger_type, suggested, blob_container, queue_name)));
}

pub fn handle_trigger_from_detail(
    dir: &str,
    workflows_sig: Signal<Vec<WorkflowItem>>,
    selected_wf: Signal<Option<String>>,
    az_status: Signal<Option<Result<String, azure_cli::AzError>>>,
    mut run_dialog: Signal<Option<(String, String, String, String, Option<String>, Option<String>)>>,
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
    let blob_container = blob_container_for(dir, &wf.name, &wf.trigger_type);
    let queue_name     = sb_check::trigger_queue_for(dir, &wf.name).map(|(_fqdn, q)| q);
    let suggested      = payload::suggest_payload(dir, &wf.name);
    run_dialog.set(Some((wf.name, wf.trigger_name, wf.trigger_type, suggested, blob_container, queue_name)));
}

pub fn handle_run(
    name: String,
    trigger_name: String,
    trigger_type: String,
    queue_or_blob: String,   // queue name for SB triggers, blob filename for blob triggers
    body: String,
    dir: &str,
    runs: Signal<Vec<RunItem>>,
    actions: Signal<Vec<ActionItem>>,
    log_lines: Signal<Vec<LogLine>>,
    mut running_wfs: Signal<HashSet<String>>,
    mut active_tab: Signal<String>,
    mut traced_wfs: Signal<HashSet<String>>,
    mut cleared_wfs: Signal<HashMap<String, String>>,
    mut run_dialog: Signal<Option<(String, String, String, String, Option<String>, Option<String>)>>,
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

    let t = trigger_type.to_lowercase();
    let is_recurrence = t == "recurrence" || t == "schedule";
    let is_http       = matches!(t.as_str(), "request" | "http");

    // Derive blob trigger and container directly from the workflow JSON so this
    // works regardless of whether the dialog passed a blob_name.
    let blob_container = blob_container_for(&dir_diag, &wf, &trigger_type);
    let is_blob        = blob_container.is_some();
    let blob_container = blob_container.unwrap_or_default();

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
        if is_blob {
            let check_container = blob_container.clone();
            let existing = tokio::task::spawn_blocking(move || {
                crate::services::azurite_client::list_blobs(&check_container)
            }).await.ok().and_then(|r| r.ok()).unwrap_or_default();

            let pending: Vec<_> = existing.iter()
                .filter(|b| !b.name.ends_with(".keep"))
                .collect();

            if pending.is_empty() {
                push(
                    format!("❌ Container '{}' is empty — upload a file first.", blob_container),
                    LogLevel::Error,
                );
                running_wfs.write().remove(&wf);
                return;
            }

            // Pre-flight: check every container the workflow references exists in Azurite.
            // Missing containers would cause the workflow to fail mid-run, wasting a trigger.
            let src_path2 = workflows::resolve_logic_apps_dir(&dir_diag).join(&wf).join("workflow.json");
            if let Ok(wf_json) = std::fs::read_to_string(&src_path2) {
                let needed  = workflows::extract_all_blob_containers(&wf_json);
                let all_containers = tokio::task::spawn_blocking(|| {
                    crate::services::azurite_client::list_containers()
                }).await.ok().and_then(|r| r.ok()).unwrap_or_default();

                let missing: Vec<_> = needed.iter()
                    .filter(|c| !all_containers.contains(*c))
                    .cloned()
                    .collect();

                if !missing.is_empty() {
                    for c in &missing {
                        push(
                            format!("❌ Container '{}' does not exist in Azurite — create it first (DB panel → Blobs → + Create).", c),
                            LogLevel::Error,
                        );
                    }
                    running_wfs.write().remove(&wf);
                    return;
                }
            }

            push(
                format!("👁 Watching '{}' ({} file(s)) — polling for a run…", blob_container, pending.len()),
                LogLevel::Info,
            );
        } else if is_recurrence {
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
        } else if is_http {
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
        } else if !queue_or_blob.is_empty() {
            // Service Bus trigger — send the message body to the resolved queue
            let queue  = queue_or_blob.clone();
            let fqdn   = sb_check::trigger_queue_for(&dir_diag, &wf)
                .map(|(f, _)| f)
                .unwrap_or_default();
            push(format!("📨 Sending message to {}/{}…", fqdn, queue), LogLevel::Info);
            let body_clone = body.clone();
            // Decide send path: AMQP to localhost if the emulator is active.
            // Three ways to detect it — any one is sufficient:
            //   1. fqdn resolved to localhost/127.x (connection string style)
            //   2. UseDevelopmentEmulator=true in local.settings.json (conn str was patched)
            //   3. Port 5672 is open on localhost (most reliable: covers MSI connections
            //      where settings were never patched because there is no conn string key)
            let emulator_port_open = tokio::net::TcpStream::connect("127.0.0.1:5672")
                .await
                .is_ok();
            let result = if emulator_port_open
                || sb_check::is_local_emulator(&fqdn)
                || sb_check::is_emulator_configured(&dir_diag)
            {
                crate::services::sb_amqp::send_amqp_message("localhost", &queue, &body_clone).await
            } else {
                Err("SB Emulator is not running — start it from the toolbar first.".into())
            };
            match result {
                Ok(()) => {
                    push("✅ Message sent — waiting for workflow run…".into(), LogLevel::Ok);
                }
                Err(e) => {
                    push(format!("❌ Send failed: {}", e), LogLevel::Error);
                    running_wfs.write().remove(&wf);
                    return;
                }
            }
        } else {
            push(
                "This workflow is triggered by Service Bus — cannot resolve the queue name. \
                 Check ServiceBus_*QueueName in local.settings.json.".into(),
                LogLevel::Warn,
            );
            running_wfs.write().remove(&wf);
            return;
        }

        poll_for_run(
            wf, cleared_at, runs, actions, log_lines, running_wfs, traced_wfs, cleared,
        ).await;
        return;
    });
}

/// Shared polling loop used by both manual Watch and the auto-trigger watcher.
/// Polls run history until the run reaches a terminal state or times out.
pub async fn poll_for_run(
    wf:          String,
    cleared_at:  Option<String>,
    mut runs:    Signal<Vec<workflows::RunItem>>,
    mut actions: Signal<Vec<workflows::ActionItem>>,
    log_lines:   Signal<Vec<LogLine>>,
    mut running_wfs: Signal<HashSet<String>>,
    mut traced_wfs:  Signal<HashSet<String>>,
    cleared:         Signal<HashMap<String, String>>,
) {
        // Poll until terminal.  Stop quickly if nothing appears — a working blob
        // trigger fires within a few seconds of the blob being added.
        let empty_tick_limit: u32 = 20; // ~16 s at 800 ms/tick
        let patience_secs         = 16u32;
        let mut push = make_push(log_lines);
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        let deadline        = tokio::time::Instant::now() + std::time::Duration::from_secs(300);
        let mut empty_ticks = 0u32;
        let mut last_err    = String::new();
        loop {
            match workflows::list_runs(&wf).await {
                Err(e) => {
                    let msg = e.to_string();
                    if msg != last_err {
                        push(format!("  ⚠ run history error: {}", msg), LogLevel::Warn);
                        for hint in workflows::not_found_hints(&wf).await {
                            push(hint, LogLevel::Warn);
                        }
                        last_err = msg;
                    }
                    empty_ticks += 1;
                }
                Ok(r) => {
                    last_err.clear();
                    let r = filter_cleared(r, cleared_at.as_deref());
                    if let Some(latest) = r.first() {
                        empty_ticks = 0;
                        let run_name   = latest.name.clone();
                        let run_status = latest.properties.status.to_lowercase();
                        let run_done   = matches!(run_status.as_str(),
                            "succeeded" | "failed" | "cancelled" | "timedout");
                        runs.set(r.clone());
                        if let Ok(a) = workflows::list_actions(&wf, &run_name).await {
                            let actions_terminal = a.iter().all(|act| {
                                matches!(act.properties.status.to_lowercase().as_str(),
                                    "succeeded" | "failed" | "skipped" | "timedout" | "cancelled")
                            });
                            let all_terminal = run_done && actions_terminal;
                            actions.set(a.clone());
                            if all_terminal {
                                let ok  = a.iter().filter(|x| x.properties.status.to_lowercase() == "succeeded").count();
                                let err = a.iter().filter(|x| x.properties.status.to_lowercase() == "failed").count();
                                for act in &a {
                                    let ms = workflows::duration_ms(&act.properties.start_time, &act.properties.end_time).unwrap_or(0);
                                    let status = act.properties.status.to_lowercase();
                                    let icon = match status.as_str() {
                                        "succeeded" => "✅", "failed" => "❌", "skipped" => "⏭", _ => "⏳",
                                    };
                                    push(format!("  {} {}  {}ms", icon, act.name, ms), LogLevel::Info);
                                    if status == "failed" {
                                        // Try inline error first, fall back to get_action_detail
                                        // (the list endpoint omits error body in some runtime versions)
                                        let err_msg = match &act.properties.error {
                                            Some(e) if e.message.is_some() || e.code.is_some() => {
                                                let code = e.code.as_deref().unwrap_or("");
                                                let msg  = e.message.as_deref().unwrap_or("(no message)");
                                                if code.is_empty() { format!("{}", msg) }
                                                else { format!("[{}] {}", code, msg) }
                                            }
                                            _ => {
                                                // Fetch detail for richer error
                                                if let Ok(detail) = workflows::get_action_detail(&wf, &run_name, &act.name).await {
                                                    let code = detail["properties"]["error"]["code"].as_str().unwrap_or("");
                                                    let msg  = detail["properties"]["error"]["message"].as_str()
                                                        .or_else(|| detail["properties"]["code"].as_str())
                                                        .unwrap_or("(no detail)");
                                                    if code.is_empty() { format!("{}", msg) }
                                                    else { format!("[{}] {}", code, msg) }
                                                } else {
                                                    "(no detail — check func start log)".into()
                                                }
                                            }
                                        };
                                        push(format!("     ↳ {}", err_msg), LogLevel::Error);
                                    }
                                }
                                if err > 0 {
                                    push(format!("Run complete — {} ok, {} failed", ok, err), LogLevel::Error);
                                } else if a.is_empty() {
                                    push(format!("Run complete — {}", run_status), LogLevel::Ok);
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
                    } else {
                        empty_ticks += 1;
                        if empty_ticks >= empty_tick_limit {
                            push(
                                format!(
                                    "❌ No run appeared after {} s. Possible causes: \
                                     (1) workflow health error — check the workflow list for ⚠; \
                                     (2) blob trigger didn't fire — re-upload the file to make it fresh; \
                                     (3) Azurite table storage not initialised — click ⟳ Reset Azurite and restart func.",
                                    patience_secs
                                ),
                                LogLevel::Error,
                            );
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
}
