use std::sync::Arc;
use std::collections::{HashMap, HashSet};
use dioxus::prelude::*;

use crate::components::log_panel::{LogLevel, LogLine};
use crate::services::{
    connection_diag,
    process::{ManagedProcess, ServiceState},
    setup_manager,
    workflows::{self, WorkflowItem},
};
use crate::utils::{make_push, sweep_run_history};

pub fn handle_start(
    azurite_state: Signal<ServiceState>,
    mut func_state: Signal<ServiceState>,
    proc: Signal<Arc<ManagedProcess>>,
    mut workflows_sig: Signal<Vec<WorkflowItem>>,
    mut traced_wfs: Signal<HashSet<String>>,
    cleared_wfs: Signal<HashMap<String, String>>,
    log_lines: Signal<Vec<LogLine>>,
    dir: String,
) {
    let mut push = make_push(log_lines);

    if !matches!(*azurite_state.read(), ServiceState::Running) {
        push("⚠ Start Azurite first — func start needs blob/queue/table storage.".into(), LogLevel::Warn);
        return;
    }

    // Resolve working directory (func needs host.json)
    let mut func_cwd = dir.clone();
    let p = std::path::Path::new(&dir);
    if !p.join("host.json").exists() {
        if p.join("logic_apps").join("host.json").exists() {
            func_cwd = p.join("logic_apps").to_str().unwrap_or(&dir).to_string();
        } else if p.join("logic-apps").join("host.json").exists() {
            func_cwd = p.join("logic-apps").to_str().unwrap_or(&dir).to_string();
        }
    }

    if !std::path::Path::new(&func_cwd).join("local.settings.json").exists() {
        push(
            format!("⚠ local.settings.json not found in {} — func start requires it.", func_cwd),
            LogLevel::Warn,
        );
        return;
    }

    // ── Everything else runs in a single spawn so we can await spawn_blocking ─
    spawn(async move {
    let mut push = make_push(log_lines);

    // ── Pre-flight: auto-fix what we can, warn about the rest ────────────
    // All file I/O runs in spawn_blocking so the tokio executor is never stalled.
    {
        let d = func_cwd.clone();
        // Collect log messages from the blocking thread and emit them here.
        let msgs: Vec<(String, LogLevel)> = tokio::task::spawn_blocking(move || {
            let mut out: Vec<(String, LogLevel)> = Vec::new();
            // Shadow the outer `push` closure so all existing push(msg, lvl) calls
            // inside this block collect into `out` instead of touching a Dioxus signal.
            let mut push = |msg: String, lvl: LogLevel| out.push((msg, lvl));
            (|| {
            // 1. package.json — required by node worker runtime
            let pkg = std::path::Path::new(&d).join("package.json");
            if !pkg.exists() {
                let _ = std::fs::write(&pkg, b"{\n  \"name\": \"logic-apps\",\n  \"version\": \"1.0.0\",\n  \"dependencies\": {}\n}\n");
                push("  ✅ Created missing package.json".into(), LogLevel::Ok);
            }

            // 2. connections.json — fix ARM syntax + patch MSI → local equivalents.
            //    Done on every func start so the user never has to run Setup manually
            //    and the file is never committed in patched form (it reverts on git checkout).
            let conn_path = std::path::Path::new(&d).join("connections.json");
            if conn_path.exists() {
                if let Ok(raw) = std::fs::read_to_string(&conn_path) {
                    let syntax_fixed = setup_manager::fix_connections_json(&raw);
                    let fully_fixed  = setup_manager::patch_connections_for_local(&syntax_fixed);
                    if fully_fixed != raw {
                        let _ = std::fs::write(&conn_path, &fully_fixed);
                        let syntax_changed = syntax_fixed != raw;
                        let msi_changed    = fully_fixed != syntax_fixed;
                        if syntax_changed {
                            push("  ✅ Fixed ARM template syntax in connections.json".into(), LogLevel::Ok);
                        }
                        if msi_changed {
                            push("  ✅ Patched connections.json: MSI → local (AzureBlob → Azurite, ServiceBus → emulator)".into(), LogLevel::Ok);
                        }
                    }
                }
            }

            // 3. Settings with known safe defaults — stub silently, warn about the rest
            let risks = connection_diag::scan_startup_risks(&d);
            if !risks.is_empty() {
                let mut auto_fixed: Vec<String> = Vec::new();
                let mut needs_user: Vec<(String, String)> = Vec::new();

                for (_wf, issues) in &risks {
                    for issue in issues {
                        // Extract setting key from "connection '…': setting '…' is empty"
                        if let Some(key) = issue
                            .split("setting '").nth(1)
                            .and_then(|s| s.split('\'').next())
                        {
                            let default = setup_manager::smart_default(key);
                            if !default.is_empty() {
                                let _ = setup_manager::stub_missing_keys(&d, &[key.to_string()]);
                                auto_fixed.push(key.to_string());
                            } else {
                                needs_user.push((_wf.clone(), key.to_string()));
                            }
                        }
                    }
                }

                if !auto_fixed.is_empty() {
                    push(
                        format!("  ✅ Auto-stubbed settings with local defaults: {}", auto_fixed.join(", ")),
                        LogLevel::Ok,
                    );
                }
                if !needs_user.is_empty() {
                    push(
                        format!(
                            "  ⚠ {} workflow(s) still have empty settings that require real values — \
                             run history may fail for ALL workflows:",
                            needs_user.len()
                        ),
                        LogLevel::Warn,
                    );
                    for (wf, key) in &needs_user {
                        push(format!("     • '{}': set '{}' in local.settings.json", wf, key), LogLevel::Warn);
                    }
                    push(
                        "     → Open Connections → set the values above, or remove the workflow folder.".into(),
                        LogLevel::Warn,
                    );
                }

                // MSI + local endpoint → trigger will silently never fire
                let msi_affected = connection_diag::scan_msi_local_trigger_workflows(&d);
                if !msi_affected.is_empty() {
                    let mut names: Vec<_> = msi_affected.into_iter().collect();
                    names.sort();
                    push(
                        format!(
                            "  ⚠ {} workflow(s) have blob triggers using Managed Identity auth against Azurite — \
                             the trigger poller cannot authenticate and will never fire:",
                            names.len()
                        ),
                        LogLevel::Warn,
                    );
                    for name in &names {
                        push(format!("     • {}", name), LogLevel::Warn);
                    }
                    push(
                        "     Fix: in connections.json set parameterSetName: \"connectionString\" \
                         for the blob connection, then add <Name>_connectionString = \
                         \"UseDevelopmentStorage=true\" to local.settings.json.".into(),
                        LogLevel::Warn,
                    );
                }
            }
            })();
            out
        }).await.unwrap_or_default();
        for (msg, lvl) in msgs { push(msg, lvl); }
    }

    func_state.set(ServiceState::Starting);

    {
        // Quick async port check before launching func
        let mut ready = false;
        for attempt in 0u8..6 {
            let all_up = async {
                for port in [10000u16, 10001, 10002] {
                    if tokio::net::TcpStream::connect(
                        std::net::SocketAddr::from(([127, 0, 0, 1], port))
                    ).await.is_err() { return false; }
                }
                true
            }.await;
            if all_up { ready = true; break; }
            if attempt == 0 {
                push("Waiting for Azurite storage services…".into(), LogLevel::Info);
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }

        if !ready {
            let mut dead = Vec::new();
            for port in [10000u16, 10001, 10002] {
                if tokio::net::TcpStream::connect(
                    std::net::SocketAddr::from(([127, 0, 0, 1], port))
                ).await.is_err() { dead.push(port.to_string()); }
            }
            push(
                format!("⚠ Azurite port(s) {} not responding — click Stop then Start on Azurite to restart it.", dead.join(", ")),
                LogLevel::Error,
            );
            func_state.set(ServiceState::Stopped);
            return;
        }

        push(format!("$ cd {} && func start", func_cwd), LogLevel::Info);

        // Kill stale process on port 7071
        let _ = std::process::Command::new("/bin/sh")
            .args(["-c", "lsof -ti :7071 | xargs kill -9 2>/dev/null; true"])
            .env("PATH", crate::services::process::rich_path())
            .output();

        match proc.read().start("func", &["start"], Some(&func_cwd)) {
            Ok((stdout, stderr)) => {
                func_state.set(ServiceState::Running);
                push("func start launched — waiting for workflows…".into(), LogLevel::Ok);

                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, bool)>();
                crate::services::process::stream_output(stdout, stderr, tx, true);

                let mut push3 = make_push(log_lines);
                spawn(async move {
                    let mut function_conn_warned = false;
                    while let Some((line, is_err)) = rx.recv().await {
                        if line.contains("functionConnections") && line.contains("cannot be parsed") {
                            if !function_conn_warned {
                                function_conn_warned = true;
                                push3(
                                    "⚠ functionConnections: @{appsetting(...)} interpolation in function.id is not supported locally — triggerUrl is used for invocation and is unaffected.".into(),
                                    LogLevel::Warn,
                                );
                            }
                            continue;
                        }
                        if line.contains("Workflow processing failed") && line.contains("functionConnections") {
                            continue;
                        }
                        push3(line, if is_err { LogLevel::Error } else { LogLevel::Info });
                    }
                });

                let mut push4 = make_push(log_lines);
                spawn(async move {
                    match workflows::wait_for_workflows(120).await {
                        Ok(mut list) => {
                            let func_names: HashSet<String> = list.iter().map(|w| w.name.clone()).collect();
                            let la_dir2 = dir.clone();
                            let local_names = tokio::task::spawn_blocking(move || {
                                workflows::scan_local_workflows(&la_dir2)
                            }).await.unwrap_or_default();

                            // Enrich live list with trigger_provider from local files —
                            // the management API only returns type/kind, not serviceProviderId.
                            let providers: std::collections::HashMap<String, String> = local_names.iter()
                                .filter_map(|w| w.trigger_provider.as_ref().map(|p| (w.name.clone(), p.clone())))
                                .collect();
                            for w in &mut list {
                                if w.trigger_provider.is_none() {
                                    w.trigger_provider = providers.get(&w.name).cloned();
                                }
                            }

                            let missing: Vec<String> = local_names.iter()
                                .map(|w| w.name.clone())
                                .filter(|n| !func_names.contains(n))
                                .collect();

                            push4(
                                format!("Loaded {} workflow(s){}", list.len(),
                                    if !missing.is_empty() {
                                        format!(" — ⚠ {} local workflow(s) not registered", missing.len())
                                    } else { String::new() }
                                ),
                                if !missing.is_empty() { LogLevel::Warn } else { LogLevel::Ok },
                            );
                            for name in &missing {
                                push4(
                                    format!("⚠ '{}' not registered by func — a connection likely failed to initialise. Open Connections and check for missing or unreachable endpoints, then restart func.", name),
                                    LogLevel::Warn,
                                );
                            }
                            for wf in list.iter().filter(|w| !w.healthy) {
                                push4(
                                    format!("⚠ '{}' loaded but unhealthy: {} — Open Connections and check endpoints, then restart func.",
                                        wf.name, wf.health_error.as_deref().unwrap_or("connection failed to initialise")),
                                    LogLevel::Warn,
                                );
                            }
                            let names: Vec<String> = list.iter().map(|w| w.name.clone()).collect();
                            workflows_sig.set(list);
                            sweep_run_history(names, &mut traced_wfs, &cleared_wfs).await;
                        }
                        Err(_) => {
                            push4("Host did not become ready — scanning for broken workflow.json files…".into(), LogLevel::Warn);
                            let broken = tokio::task::spawn_blocking(move || {
                                workflows::scan_broken_workflows(&dir)
                            }).await.unwrap_or_default();
                            if broken.is_empty() {
                                push4("No JSON errors found in workflow files. Check func start output above for errors.".into(), LogLevel::Warn);
                            } else {
                                for (name, err) in broken {
                                    push4(format!("❌ {}/workflow.json — {}", name, err), LogLevel::Error);
                                }
                            }
                        }
                    }
                });
            }
            Err(e) => {
                func_state.set(ServiceState::Stopped);
                push(format!("func start error: {}", e), LogLevel::Error);
            }
        }
    } // end port-check block
    }); // end spawn(async move)
}

pub fn handle_stop(
    mut state: Signal<ServiceState>,
    proc: Signal<Arc<ManagedProcess>>,
    log_lines: Signal<Vec<LogLine>>,
) {
    let mut push = make_push(log_lines);
    match proc.read().stop() {
        Ok(_)  => { state.set(ServiceState::Stopped); push("func start stopped.".into(), LogLevel::Warn); }
        Err(e) => push(format!("Error: {}", e), LogLevel::Error),
    }
}
