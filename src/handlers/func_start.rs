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

            // 2. connections.json — fix ARM syntax + patch MSI → local equivalents,
            //    then apply connections.local.json (gitignored per-developer override).
            //    Done on every func start so the user never has to run Setup manually
            //    and the file is never committed in patched form (it reverts on git checkout).
            let conn_path = std::path::Path::new(&d).join("connections.json");
            if conn_path.exists() {
                if let Ok(raw) = std::fs::read_to_string(&conn_path) {
                    let syntax_fixed = setup_manager::fix_connections_json(&raw);
                    let fully_fixed  = setup_manager::patch_connections_for_local(&syntax_fixed);
                    // Layer connections.local.json on top of the auto-patched result.
                    // Done as a JSON merge (not a string patch) so the override can
                    // target individual fields without having to mirror the full file.
                    let mut merged_value: serde_json::Value =
                        serde_json::from_str(&fully_fixed).unwrap_or(serde_json::Value::Null);
                    let dir_path = std::path::Path::new(&d);
                    let local_applied = match crate::services::connections_local::load_overrides(dir_path) {
                        Ok(Some(ov)) => {
                            let touched = crate::services::connections_local::apply_overrides(&mut merged_value, &ov);
                            let summary = crate::services::connections_local::override_summary(&ov);
                            Some((touched, summary))
                        }
                        Ok(None) => None,
                        Err(e) => {
                            push(
                                format!("  ⚠ connections.local.json present but invalid — skipped: {e}"),
                                LogLevel::Warn,
                            );
                            None
                        }
                    };
                    let final_text = if local_applied.is_some() {
                        serde_json::to_string_pretty(&merged_value).unwrap_or(fully_fixed.clone())
                    } else {
                        fully_fixed.clone()
                    };
                    if final_text != raw {
                        let _ = std::fs::write(&conn_path, &final_text);
                        let syntax_changed = syntax_fixed != raw;
                        let msi_changed    = fully_fixed != syntax_fixed;
                        if syntax_changed {
                            push("  ✅ Fixed ARM template syntax in connections.json".into(), LogLevel::Ok);
                        }
                        if msi_changed {
                            push("  ✅ Patched connections.json: MSI → local (AzureBlob → Azurite, ServiceBus → emulator)".into(), LogLevel::Ok);
                        }
                        if let Some((touched, summary)) = local_applied {
                            push(
                                format!(
                                    "  ✅ Applied connections.local.json — {touched} field(s) overridden \
                                     ({sp} service-provider, {ma} managed-api connection(s) touched)",
                                    sp = summary.service_provider, ma = summary.managed_api,
                                ),
                                LogLevel::Ok,
                            );
                        }
                    } else if let Some((touched, _)) = local_applied {
                        // Override loaded but ended up matching what's on disk —
                        // still worth surfacing so users know it was honoured.
                        push(
                            format!("  ✓ connections.local.json applied ({touched} field(s) matched current state)"),
                            LogLevel::Info,
                        );
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

    // Inline-JavaScript pre-flight: flag workflows using `Execute JavaScript
    // Code`. They need the Node language worker, which the host starts on
    // demand — we can't start it, but forewarning saves a confusing debug.
    {
        let d = dir.clone();
        let js_wfs = tokio::task::spawn_blocking(move || {
            crate::services::inline_js::workflows_with_inline_js(&d)
        }).await.unwrap_or_default();
        if !js_wfs.is_empty() {
            push(format!(
                "  ℹ {} workflow(s) use inline JavaScript ({}). These run in the Node language worker — \
                 ensure Node.js is installed. If a run fails with 'actively refused (localhost:PORT)', \
                 that's the JS worker not starting, not your workflow.",
                js_wfs.len(), js_wfs.join(", ")
            ), LogLevel::Info);
        }
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

        // Reclaim :7071 from a stale func host. Must target the *listener*
        // only — a bare `lsof -ti :7071` also matches this app's own pooled
        // keep-alive connections to the func management API, which made
        // ais-runner SIGKILL itself mid-run.
        let _ = tokio::task::spawn_blocking(|| {
            crate::services::port_owner::kill_listener(7071)
        }).await;

        // Pin the func host to the C locale. On machines with a comma-decimal
        // locale (fr/de/…), the Logic Apps job scheduler serializes
        // numbers/timestamps with a comma, which corrupts the Azurite table
        // row-keys it dispatches on — jobs silently stop firing. LC_ALL/LANG=C
        // makes .NET resolve CurrentCulture to Invariant, which is enough.
        // Do NOT add DOTNET_SYSTEM_GLOBALIZATION_INVARIANT: it unloads ICU, and
        // the Workflows extension hardcodes CultureInfo("en-us") during host
        // startup, which then throws and faults the script host.
        let func_env: Vec<(String, String)> = vec![
            ("LC_ALL".into(), "C".into()),
            ("LANG".into(), "C".into()),
        ];
        match proc.read().start_with_env("func", &["start"], Some(&func_cwd), &func_env) {
            Ok((stdout, stderr)) => {
                func_state.set(ServiceState::Running);
                push("func start launched — waiting for workflows…".into(), LogLevel::Ok);

                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, bool)>();
                crate::services::process::stream_output(stdout, stderr, tx, true);

                let mut push3 = make_push(log_lines);
                spawn(async move {
                    let mut function_conn_warned = false;
                    let mut bundle_warned = false;
                    let mut prebuild_warned = false;
                    let mut js_warned = false;
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
                        // Host-readiness probe failures spam the log many times a
                        // second while the func host is still booting — pure noise.
                        if line.contains("Host unavailable after check") {
                            continue;
                        }
                        // Leftover from workflows still patched to Stateful on disk
                        // (the old debug-mode feature, now removed). The runtime
                        // refuses to flip kind on a live registration and logs this
                        // every start until the file is reverted — drop the noise.
                        if line.contains("cannot be changed from 'Stateless' to 'Stateful'") {
                            continue;
                        }
                        // Inline-JS worker didn't start — translate the cryptic
                        // "actively refused (localhost:PORT)" into plain English.
                        if crate::services::inline_js::is_inline_js_worker_error(&line) {
                            push3(line.clone(), LogLevel::Error);
                            if !js_warned {
                                js_warned = true;
                                push3("⚠ That 'actively refused (localhost:PORT)' is the inline-JavaScript (Node language worker) failing to start — not your workflow. Check Node.js is installed and on PATH, then restart func.".into(), LogLevel::Warn);
                            }
                            continue;
                        }
                        // A native binding the bundle has no build of for this
                        // platform/Node ABI. Called out separately from a corrupt
                        // cache because clearing the cache cannot fix it.
                        if crate::services::bundle_cache::is_missing_prebuild(&line) {
                            push3(line.clone(), LogLevel::Error);
                            if !prebuild_warned {
                                prebuild_warned = true;
                                push3("⚠ The extension bundle ships no build of this native module for your platform/Node version — inline-JavaScript actions will fail. Clearing the bundle cache will NOT help (it re-downloads the same bundle). Workflows without a JavaScriptCode action are unaffected.".into(), LogLevel::Warn);
                            }
                            continue;
                        }
                        // Corrupt extension-bundle cache — surface a clear,
                        // actionable message once (the raw error is cryptic).
                        if crate::services::bundle_cache::is_bundle_error(&line) {
                            push3(line.clone(), LogLevel::Error);
                            if !bundle_warned {
                                bundle_warned = true;
                                push3("⚠ Extension bundle cache looks corrupt. Click ⟳ Clear bundle cache (next to func), then Start again — func will re-download it.".into(), LogLevel::Warn);
                            }
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
