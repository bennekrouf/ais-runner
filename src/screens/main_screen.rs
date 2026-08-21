use std::collections::HashSet;
use chrono::Utc;
use dioxus::prelude::*;

use crate::components::{
    log_panel::{LogLevel, LogLine, LogPanel},
    run_detail::RunDetail,
    run_dialog::RunDialog,
    run_gate_dialog::RunGateDialog,
    toolbar::ServiceBlock,
    workflow_list::WorkflowList,
    settings_editor::SettingsEditor,
    db_panel::DbPanel,
    azure_panel::AzurePanel,
    tests_panel::TestsPanel,
    devops_panel::DevOpsPanel,
    func_panel::FuncPanel,
    graph_panel::GraphPanel,
};
use crate::services::{
    azure_cli, azure_sync, blob_check, config, connection_diag, cosmos_check,
    env_mode::{self, EnvMode},
    process::ServiceState,
    setup_manager, sftp_check, sql_check, sb_check, system_check,
    workflow_analysis,
    workflows,
};
use crate::utils::make_push;
use crate::handlers::{azurite, cosmos_emulator, func_start, mock_server, sb_emulator, sql_emulator, setup, workflow_select, workflow_run};
use crate::screens::MainContext;

#[derive(Props, Clone, PartialEq)]
pub struct MainScreenProps {
    pub logic_apps_dir: String,
    pub on_back: EventHandler<()>,
}

#[component]
pub fn MainScreen(props: MainScreenProps) -> Element {
    let dir            = props.logic_apps_dir.clone();

    // Get context - signals are now managed at App level to prevent re-mounts
    let mut ctx = use_context::<MainContext>();


    // Update the window title to include the project basename so users running
    // multiple instances (one per customer) can tell them apart at a glance.
    {
        let basename = std::path::Path::new(&dir)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| dir.clone());
        dioxus::desktop::window().set_title(&format!(
            "AIS Local Runner {} — {}",
            env!("CARGO_PKG_VERSION"),
            basename,
        ));
    }

    let cfg            = ctx.cfg;
    // Derive workspace_link directly rather than via cfg.read() — in Dioxus 0.6
    // the signal holds an internal write lock during hook initialisation, so
    // reading the same signal on the very next line causes AlreadyBorrowedMut
    // on Windows.  config::load() is a cheap file read; calling it twice here
    // is the simplest way to sidestep the re-entrant borrow.
    let workspace_link = config::load().get_link(&dir).cloned();

    // ── Service states & processes (from context) ─────────────────────────────
    let azurite_state    = ctx.azurite_state;
    let func_state       = ctx.func_state;
    let java_func_state  = ctx.java_func_state;
    let sb_emu_state     = ctx.sb_emu_state;
    let cosmos_emu_state = ctx.cosmos_emu_state;
    let sql_dev_state    = ctx.sql_dev_state;
    let mock_state       = ctx.mock_state;
    let mock_handle      = ctx.mock_handle;
    let azurite_proc     = ctx.azurite_proc;
    let func_proc        = ctx.func_proc;
    let java_func_proc   = ctx.java_func_proc;
    let sb_emu_proc      = ctx.sb_emu_proc;
    let cosmos_emu_proc  = ctx.cosmos_emu_proc;
    let sql_dev_proc     = ctx.sql_dev_proc;
    let sb_emu_lines     = ctx.sb_emu_lines;
    let java_func_lines  = ctx.java_func_lines;
    let mut sql_dev_lines = ctx.sql_dev_lines;
    let az_lines         = ctx.az_lines;

    // resolve_logic_apps_dir may descend into a logic_apps/ subfolder,
    // so derive func_apps_dir from the resolved path's parent (the platform
    // root) rather than the raw dir the user selected.
    let func_apps_dir = {
        let resolved = workflows::resolve_logic_apps_dir(&dir);
        resolved
            .parent()
            .map(|p| p.join("function_apps").to_string_lossy().to_string())
            .unwrap_or_else(|| format!("{}/function_apps", dir))
    };

    // ── Data signals (now from context) ────────────────────────────────────
    let mut workflows   = ctx.workflows;
    let selected_wf     = ctx.selected_wf;
    let source_text = ctx.source_text;
    let wf_analysis = use_memo(move || {
        workflow_analysis::analyse(&source_text.read())
    });
    let mut runs        = ctx.runs;
    let mut actions     = ctx.actions;
    let mut running_wfs = ctx.running_wfs;
    let mut current_view = ctx.current_view;
    let mut is_light     = ctx.is_light;
    let active_tab  = ctx.active_tab;
    let mut run_dialog  = ctx.run_dialog;
    let mut run_gate    = ctx.run_gate;
    let mut traced_wfs  = ctx.traced_wfs;
    let mut cleared_wfs = ctx.cleared_wfs;
    let mut last_ran    = ctx.last_ran;
    let mut auto_watch  = ctx.auto_watch;
    let mut recorder    = ctx.recorder;
    // Track which views have been opened — panels are lazy-mounted on first visit
    // but stay in the DOM afterwards so their state (caches, signals) survives tab switches.
    let mut visited_views = ctx.visited_views;

    // ── Log (from context) ─────────────────────────────────────────────────────
    let mut log_lines = ctx.log_lines;

    // ── SQL Dev log channel ────────────────────────────────────────────────
    use_hook(|| {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        crate::services::sql_runner::init_log_channel(tx);
        // Drain the channel into the signal via a Dioxus coroutine (not tokio::spawn — signals aren't Send)
        dioxus::prelude::spawn(async move {
            while let Some(line) = rx.recv().await {
                let ts = chrono::Local::now().format("%H:%M:%S").to_string();
                let entry = format!("{} {}", ts, line);
                let mut w = sql_dev_lines.write();
                let len = w.len();
                if len > 1000 { w.drain(..len - 1000); }
                w.push(entry);
            }
        });
    });

    // ── Auto blob-trigger watcher ──────────────────────────────────────────
    // All Azurite I/O runs on a dedicated OS thread (std::thread::spawn).
    // The UI coroutine only drains the event channel — zero blocking on the
    // Dioxus/tokio executor, so rendering is never stalled.
    {
        use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

        // Shared flags written by the UI, read by the OS thread.
        let watch_flag = Arc::new(AtomicBool::new(true));
        let func_flag  = Arc::new(AtomicBool::new(false));

        // Keep Arc clones that the UI closures will update.
        let wf_clone   = Arc::clone(&watch_flag);
        let ff_clone   = Arc::clone(&func_flag);

        // Sync the flags with Dioxus signals via a lightweight coroutine.
        use_coroutine(move |_rx: dioxus::prelude::UnboundedReceiver<()>| {
            let wf_clone = Arc::clone(&wf_clone);
            let ff_clone = Arc::clone(&ff_clone);
            async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    wf_clone.store(*auto_watch.read(), Ordering::Relaxed);
                    ff_clone.store(
                        *func_state.read() == crate::services::process::ServiceState::Running,
                        Ordering::Relaxed,
                    );
                }
            }
        });

        // Channel + OS thread: Arc<Mutex<Option>> ensures the thread is spawned
        // exactly once (use_hook requires Clone; the Mutex lets us take() the
        // receiver on the first coroutine run).
        let rx_holder = use_hook(|| {
            let (tx, rx) = tokio::sync::mpsc::channel::<(String, String)>(16);

            let bg_dir   = dir.clone();
            let bg_watch = Arc::clone(&watch_flag);
            let bg_func  = Arc::clone(&func_flag);
            std::thread::Builder::new()
                .name("ais-blob-watcher".into())
                .spawn(move || {
                    use std::collections::{HashMap, HashSet};

                    let trigger_map = workflows::scan_all_blob_triggers(&bg_dir);

                    let mut seen: HashMap<String, HashSet<String>> = trigger_map.iter()
                        .map(|(c, _)| {
                            let names = crate::services::azurite_client::list_blobs(c)
                                .unwrap_or_default().into_iter()
                                .filter(|b| !b.name.ends_with("/.keep"))
                                .map(|b| b.name).collect();
                            (c.clone(), names)
                        })
                        .collect();

                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(3));

                        if !bg_watch.load(Ordering::Relaxed) || !bg_func.load(Ordering::Relaxed) {
                            continue;
                        }

                        for (container, wf_name) in &trigger_map {
                            let current: HashSet<String> =
                                crate::services::azurite_client::list_blobs(container)
                                    .unwrap_or_default().into_iter()
                                    .filter(|b| !b.name.ends_with("/.keep"))
                                    .map(|b| b.name)
                                    .collect();

                            let prev = seen.entry(container.clone()).or_default();
                            let has_new = current.iter().any(|n| !prev.contains(n));
                            *prev = current;

                            if has_new {
                                let _ = tx.try_send((container.clone(), wf_name.clone()));
                            }
                        }
                    }
                })
                .ok();

            Arc::new(std::sync::Mutex::new(Some(rx)))
        });

        // UI coroutine — only wakes when the OS thread found something new.
        use_coroutine(move |_rx: dioxus::prelude::UnboundedReceiver<()>| {
            let mut rx = rx_holder.lock().unwrap().take().expect("coroutine called twice");
            async move {
                while let Some((container, wf_name)) = rx.recv().await {
                    if running_wfs.read().contains(&wf_name) { continue; }

                    let mut push = make_push(log_lines);
                    push(
                        format!("⚡ Auto: new blob in '{}' → watching {}…", container, wf_name),
                        LogLevel::Info,
                    );

                    let trigger_ts = chrono::Utc::now().to_rfc3339();
                    cleared_wfs.write().insert(wf_name.clone(), trigger_ts.clone());
                    traced_wfs.write().insert(wf_name.clone());
                    running_wfs.write().insert(wf_name.clone());
                    last_ran.write().insert(wf_name.clone(), epoch_now());

                    let wf     = wf_name.clone();
                    let cleared = cleared_wfs;
                    dioxus::prelude::spawn(workflow_run::poll_for_run(
                        wf, Some(trigger_ts),
                        runs, actions, log_lines,
                        running_wfs, traced_wfs, cleared,
                        false, false, // manual Watch — not a blob or SB trigger
                    ));
                }
            }
        });
    }

    // ── Setup ──────────────────────────────────────────────────────────────
    // Start with a neutral default; check_setup reads local.settings.json which
    // must not block the GUI thread.
    let mut setup_status  = ctx.setup_status;
    let setup_updates     = ctx.setup_updates;

    // Load setup status off the GUI thread on first mount.
    use_effect({
        let d = dir.clone();
        move || {
            let d = d.clone();
            spawn(async move {
                let s = tokio::task::spawn_blocking(move || setup_manager::check_setup(&d))
                    .await.unwrap_or(setup_manager::SetupStatus::MissingSettings);
                setup_status.set(s);
            });
        }
    });

    let _on_apply_setup = {
        let dir = dir.clone();
        let mut status  = setup_status;
        let updates     = setup_updates;
        move |_: Event<MouseData>| {
            let d  = dir.clone();
            let d2 = dir.clone();
            let u  = updates.read().clone();
            spawn(async move {
                if tokio::task::spawn_blocking(move || setup_manager::apply_settings(&d, u))
                    .await.ok().and_then(|r| r.ok()).is_some()
                {
                    // Re-check setup status off the GUI thread.
                    let s = tokio::task::spawn_blocking(move || setup_manager::check_setup(&d2))
                        .await.unwrap_or(setup_manager::SetupStatus::MissingSettings);
                    status.set(s);
                }
            });
        }
    };

    // ── Env mode ───────────────────────────────────────────────────────────
    // detect_mode reads local.settings.json — must not block the GUI thread.
    let mut current_env = ctx.current_env;
    use_effect({
        let d = dir.clone();
        move || {
            let d = d.clone();
            spawn(async move {
                let mode = tokio::task::spawn_blocking(move || env_mode::detect_mode(&d))
                    .await.unwrap_or(env_mode::EnvMode::Local);
                current_env.set(mode);
            });
        }
    });

    // ── Connection / SQL / SB signals ─────────────────────────────────────
    let mut sql_wfs          = ctx.sql_wfs;
    let mut msi_wfs          = ctx.msi_wfs;
    let mut wf_connectors    = ctx.wf_connectors;
    let mut sql_conns        = ctx.sql_conns;
    // sproc qualified name → Some(exists) once checked, None while loading.
    let mut sproc_status     = ctx.sproc_status;
    let mut db_panel_open    = ctx.db_panel_open;
    let mut azure_panel_open = ctx.azure_panel_open;
    let az_diff_cache        = ctx.az_diff_cache;
    let mut sftp_conns       = ctx.sftp_conns;
    let mut blob_conns       = ctx.blob_conns;
    let mut cosmos_conns     = ctx.cosmos_conns;
    let mut webjobs_storage  = ctx.webjobs_storage;
    let mut sb_namespace     = ctx.sb_namespace;
    let mut sb_namespace_key = ctx.sb_namespace_key;
    let mut sb_conn_str      = ctx.sb_conn_str;
    let mut sb_queues        = ctx.sb_queues;

    // ── Tool check / Azure login ───────────────────────────────────────────
    let mut tool_statuses    = ctx.tool_statuses;
    let mut tools_dismissed  = ctx.tools_dismissed;
    let az_status            = ctx.az_status;
    let active_tenant        = ctx.active_tenant;

    // ══ Effects ════════════════════════════════════════════════════════════

    use_effect(move || {
        spawn(async move {
            tool_statuses.set(tokio::task::spawn_blocking(system_check::check_tools).await.unwrap_or_default());
        });
    });

    // Surface what the pre-window Azurite check did. It runs before Dioxus
    // launches, so this is the first moment there is a log panel to say it in.
    // take_ drains, so a re-render cannot repeat the lines.
    use_effect(move || {
        let mut push = make_push(log_lines);
        for line in crate::services::azurite_health::take_startup_notes() {
            push(line, LogLevel::Warn);
        }
    });

    // Probe sproc existence in the local SQL emulator whenever the selected
    // workflow's analysis changes. Uses the first configured SQL connection's
    // resolved database name. Silent on failure (e.g. SQL emulator down).
    use_effect(move || {
        let names: Vec<String> = wf_analysis.read().sql_sprocs.iter().map(|sp| sp.name.clone()).collect();
        if names.is_empty() { return; }
        let db = sql_conns.read().first().map(|c| c.resolved_db.clone()).unwrap_or_default();
        if db.is_empty() { return; }
        {
            let mut st = sproc_status.write();
            for n in &names {
                st.entry(n.clone()).or_insert(None);
            }
        }
        spawn(async move {
            for n in names {
                let exists = crate::services::sql_runner::sproc_exists(&db, &n).await.ok();
                if let Some(e) = exists {
                    sproc_status.write().insert(n, Some(e));
                }
            }
        });
    });

    // Azure login is checked lazily — only when the user clicks the login widget
    // or when an Azure-dependent feature (DevOps, Publish, cloud SB) is invoked.
    // Do NOT run az account show on every startup: the app works fully offline
    // for local workflow development.

    use_effect({
        let dir2     = dir.clone();
        let mut cfg2 = cfg;
        move || {
            // try_bootstrap_link reads local.settings.json — must run off the GUI thread.
            if cfg2.read().get_link(&dir2).is_some() { return; }
            let d     = dir2.clone();
            let dir2b = dir2.clone();  // second clone for use inside the async block
            spawn(async move {
                let link = tokio::task::spawn_blocking(move || {
                    crate::services::settings_file::try_bootstrap_link(&d)
                }).await.ok().flatten();
                if let Some(link) = link {
                    let mut c = cfg2.write();
                    c.set_link(dir2b, link);
                    let snap = c.clone();
                    tokio::task::spawn_blocking(move || config::save(&snap)).await.ok();
                }
            });
        }
    });

    use_effect({
        let d = dir.clone();
        move || {
            let d = d.clone();
            spawn(async move {
                let (wfs, msi, conns, (sb_ns, sb_qs), sb_key, sb_cs_key, blobs, cosmos, wjs) =
                    tokio::task::spawn_blocking(move || {
                        let wfs       = sql_check::detect_sql_workflows(&d);
                        let msi       = connection_diag::scan_msi_local_trigger_workflows(&d);
                        let conns     = sql_check::load_sql_connections(&d);
                        let sb        = sb_check::detect_sb_queues(&d);
                        let sb_key    = sb_check::detect_sb_namespace_key(&d);
                        let sb_cs_key = sb_check::detect_sb_conn_str_key(&d);
                        let blobs     = blob_check::detect_blob_connections(&d);
                        let cosmos    = cosmos_check::detect_cosmos_connections(&d);
                        let wjs       = blob_check::read_webjobs_storage(&d);
                        (wfs, msi, conns, sb, sb_key, sb_cs_key, blobs, cosmos, wjs)
                    }).await.unwrap_or_default();
                sql_wfs.set(wfs); msi_wfs.set(msi); sql_conns.set(conns); sb_namespace.set(sb_ns);
                sb_queues.set(sb_qs); sb_namespace_key.set(sb_key); sb_conn_str.set(sb_cs_key);
                blob_conns.set(blobs); cosmos_conns.set(cosmos); webjobs_storage.set(wjs);
            });
        }
    });

    // (Resize handles now use per-element Dioxus onmousedown — see below.
    //  The previous global document.body delegate was removed because it
    //  occasionally lost the mousedown after the workflow-filter input took
    //  focus in some webview builds.)

    // Re-scan connector usage whenever the workflow list is refreshed.
    use_effect({
        let dir = dir.clone();
        move || {
            let _ = workflows.read(); // reactive: re-runs when workflows changes
            let d = dir.clone();
            spawn(async move {
                let map = tokio::task::spawn_blocking(move || {
                    workflows::scan_all_connectors(&d)
                }).await.unwrap_or_default();
                wf_connectors.set(map);
            });
        }
    });

    // ── Auto-refresh run detail for the selected workflow ─────────────────
    // Polls every 2 s when a workflow is selected.  Lightweight: the HTTP
    // call to list_runs is a single localhost request to the func runtime.
    // Skips when poll_for_run is already active for this workflow.
    use_effect(move || {
        spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
                let wf = match selected_wf.read().clone() {
                    Some(w) => w,
                    None => continue,
                };
                // Skip if the poll_for_run loop is already driving updates
                if running_wfs.read().contains(&wf) { continue; }
                // Fetch fresh data
                match tokio::time::timeout(std::time::Duration::from_secs(5), workflows::list_runs(&wf)).await {
                    Ok(Ok(r)) => {
                        let cleared_at = cleared_wfs.read().get(&wf).cloned();
                        let r = crate::utils::filter_cleared(r, cleared_at.as_deref());
                        if let Some(latest) = r.first() {
                            if let Ok(a) = workflows::list_actions(&wf, &latest.name).await {
                                actions.set(a);
                            }
                        }
                        runs.set(r);
                    }
                    Ok(Err(_e)) => {}
                    Err(_) => {}
                }
            }
        });
    });

    // ── Global running-state poll ─────────────────────────────────────────
    // Detects externally-triggered runs (e.g. Service Bus messages posted from
    // outside ais-runner) so the spinner next to the workflow name lights up
    // even when the user didn't click Run/Watch in the UI.
    //
    // Every 3 s, queries the latest run for every workflow. To avoid stepping
    // on poll_for_run (which already manages its own entries in running_wfs),
    // we track separately which entries WE added and only reconcile those.
    use_effect(move || {
        spawn(async move {
            use std::collections::HashSet;
            let mut owned: HashSet<String> = HashSet::new();
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(3000)).await;
                let names: Vec<String> = workflows.read().iter().map(|w| w.name.clone()).collect();
                for name in names {
                    let Ok(r) = workflows::list_runs(&name).await else { continue };
                    let cleared_at = cleared_wfs.read().get(&name).cloned();
                    let r = crate::utils::filter_cleared(r, cleared_at.as_deref());
                    let is_running = r.first()
                        .map(|x| x.properties.status.eq_ignore_ascii_case("Running"))
                        .unwrap_or(false);
                    let already_in_set = running_wfs.read().contains(&name);
                    if is_running && !already_in_set {
                        running_wfs.write().insert(name.clone());
                        owned.insert(name);
                    } else if !is_running && owned.contains(&name) {
                        running_wfs.write().remove(&name);
                        owned.remove(&name);
                    }
                }
            }
        });
    });

    // ── Auto-select first workflow when list is first populated ───────────
    use_effect({
        let dir = dir.clone();
        move || {
            let list = workflows.read();
            if selected_wf.read().is_none() {
                if let Some(first) = list.first() {
                    let name = first.name.clone();
                    drop(list);
                    workflow_select::handle_select(
                        name, selected_wf, runs, actions, source_text,
                        traced_wfs, cleared_wfs, log_lines, &dir,
                    );
                }
            }
        }
    });

    // ══ View ═══════════════════════════════════════════════════════════════
    let dir_label = dir.clone();

    rsx! {
        div { id: "app",

            // ── Setup banner ──────────────────────────────────────────────
            {setup_banner(setup_status, &workspace_link, current_view, visited_views, &dir, log_lines)}

            // ── Toolbar ───────────────────────────────────────────────────
            div { id: "toolbar",
                div { class: "back-wrap",
                    button { class: "btn-back", onclick: move |_| props.on_back.call(()), "‹ Back" }
                    span { id: "toolbar-dir", title: "{dir_label}", "{dir_label}" }
                    span {
                        style: "font-size:10px; opacity:0.4; white-space:nowrap;",
                        { concat!("v", env!("CARGO_PKG_VERSION")) }
                    }
                }

                ServiceBlock {
                    label: "Azurite".to_string(),
                    cmd:   format!("azurite --location {}", crate::utils::azurite_dir().display()),
                    state: azurite_state.read().clone(),
                    on_start: move |_| azurite::handle_start(azurite_state, azurite_proc, log_lines),
                    on_stop:  move |_| azurite::handle_stop(azurite_state, azurite_proc, log_lines),
                }
                {
                    let dir_az = dir.clone();
                    rsx! {
                        button {
                            class: "btn btn-warn btn-svc",
                            title: "Stop func + Azurite, wipe storage, restart both — fixes 'run not recording' and 'runtime state is missing'",
                            onclick: move |_| azurite::handle_reset(
                                azurite_state, azurite_proc, func_state, func_proc,
                                workflows, traced_wfs, cleared_wfs, log_lines, dir_az.clone(),
                            ),
                            "⟳ Reset"
                        }
                    }
                }
                ServiceBlock {
                    label: "SB Emulator".to_string(),
                    cmd:   format!("docker run -p 5672:5672 -e ACCEPT_EULA=Y {}", sb_emulator::SB_EMULATOR_IMAGE),
                    state: sb_emu_state.read().clone(),
                    on_start: { let dir = dir.clone(); move |_| sb_emulator::handle_start(sb_emu_state, sb_emu_proc, log_lines, sb_emu_lines, dir.clone()) },
                    on_stop:  move |_| sb_emulator::handle_stop(sb_emu_state, sb_emu_proc, log_lines),
                }
                {
                    let dir_r = dir.clone();
                    rsx! {
                        button {
                            class: "btn btn-warn btn-svc",
                            title: "Stop emulator, wipe Config.json + Docker volumes, restart fresh — fixes wrong namespace, NullReferenceException, SQL Edge corruption",
                            onclick: move |_| sb_emulator::handle_reset(
                                sb_emu_state, sb_emu_proc, log_lines, sb_emu_lines, dir_r.clone(),
                            ),
                            "⟳ Reset"
                        }
                    }
                }
                ServiceBlock {
                    label: "SQL Dev".to_string(),
                    cmd:   format!("docker run -d --name {} -p {}:{} -e ACCEPT_EULA=Y {}",
                        sql_emulator::CONTAINER_NAME,
                        sql_emulator::SQL_PORT, sql_emulator::SQL_PORT,
                        sql_emulator::SQL_IMAGE,
                    ),
                    state: sql_dev_state.read().clone(),
                    on_start: move |_| sql_emulator::handle_start(sql_dev_state, sql_dev_proc, log_lines),
                    on_stop:  move |_| sql_emulator::handle_stop(sql_dev_state, sql_dev_proc, log_lines),
                }
                ServiceBlock {
                    label: "Cosmos".to_string(),
                    cmd:   format!("docker run -d --name {} -p {}:{} -p {}:{} {}",
                        cosmos_emulator::CONTAINER_NAME,
                        cosmos_emulator::COSMOS_API_PORT, cosmos_emulator::COSMOS_API_PORT,
                        cosmos_emulator::COSMOS_UI_PORT,  cosmos_emulator::COSMOS_UI_PORT,
                        cosmos_emulator::COSMOS_IMAGE,
                    ),
                    state: cosmos_emu_state.read().clone(),
                    on_start: move |_| cosmos_emulator::handle_start(cosmos_emu_state, cosmos_emu_proc, log_lines),
                    on_stop:  move |_| cosmos_emulator::handle_stop(cosmos_emu_state, cosmos_emu_proc, log_lines),
                }
                // Before func: starting the mock rewrites local.settings.json,
                // and func only reads that file at startup.
                ServiceBlock {
                    label: "Mock APIs".to_string(),
                    cmd:   "scan workflows → serve stubbed HTTP on localhost (start before func)".to_string(),
                    state: mock_state.read().clone(),
                    on_start: {
                        let dir = dir.clone();
                        move |_| mock_server::handle_start(dir.clone(), mock_state, mock_handle, log_lines)
                    },
                    on_stop: move |_| mock_server::handle_stop(mock_state, mock_handle, log_lines),
                }
                ServiceBlock {
                    label: "func start".to_string(),
                    cmd:   "func start".to_string(),
                    state: func_state.read().clone(),
                    on_start: {
                        let dir = dir.clone();
                        move |_| func_start::handle_start(
                            azurite_state, func_state, func_proc, workflows,
                            traced_wfs, cleared_wfs, log_lines, dir.clone(),
                        )
                    },
                    on_stop: {
                        let dir = dir.clone();
                        move |_| func_start::handle_stop(func_state, func_proc, log_lines, dir.clone())
                    },
                }
                // Clear extension-bundle cache (repairs corrupt-bundle func start failures)
                button {
                    class: "btn btn-svc btn-icon-only",
                    title: "Clear bundle cache — delete the func extension-bundle cache so it re-downloads cleanly. Fixes 'File already exists in ExtensionBundles' / SSL / missing-DLL start failures. Then Start func again.",
                    onclick: move |_| {
                        spawn(async move {
                            let mut push = crate::utils::make_push(log_lines);
                            let res = tokio::task::spawn_blocking(crate::services::bundle_cache::clear).await
                                .unwrap_or_else(|e| Err(format!("task failed: {e}")));
                            match res {
                                Ok(cleared) if cleared.is_empty() =>
                                    push("ℹ No extension-bundle cache found — nothing to clear.".into(), LogLevel::Info),
                                Ok(cleared) => push(
                                    format!("🧹 Cleared extension-bundle cache ({}). Start func — it will re-download the bundle.", cleared.join(", ")),
                                    LogLevel::Ok),
                                Err(e) => push(format!("❌ Could not clear bundle cache: {e}"), LogLevel::Error),
                            }
                        });
                    },
                    "⟳"
                }
                // Auto blob-trigger toggle
                {
                    let is_on   = *auto_watch.read();
                    let lbl     = if is_on { "⚡ Auto" } else { "⚡ Off" };
                    let cls     = if is_on { "btn auto-watch-btn on" }
                                  else     { "btn auto-watch-btn off" };
                    let tip     = if is_on {
                        "Auto-trigger ON — watching Azurite for new blobs every 2.5 s. Click to pause."
                    } else {
                        "Auto-trigger OFF. Click to resume."
                    };
                    rsx! {
                        button {
                            class:   "{cls}",
                            title:   "{tip}",
                            onclick: move |_| auto_watch.set(!auto_watch()),
                            "{lbl}"
                        }
                    }
                }
                {az_login_widget(az_status, active_tenant, workspace_link.as_ref().and_then(|l| l.tenant_id.clone()), &dir)}
                {env_badge(setup_status, current_env)}
                {
                    let dir_s  = dir.clone();
                    let link_s = workspace_link.clone();
                    let mut msi = msi_wfs;
                    rsx! {
                        button {
                            class: "btn btn-svc btn-icon-only",
                            title: "Setup — re-run setup: patch connections.json (MSI → connectionString), stub missing keys, auto-detect Azure resources",
                            onclick: move |_| {
                                let d = dir_s.clone();
                                setup::handle_initialize_default(&dir_s, setup_status, log_lines, link_s.clone());
                                // Refresh the MSI-warning set so the workflow list icons
                                // update immediately after connections.json is patched.
                                spawn(async move {
                                    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                                    let refreshed = tokio::task::spawn_blocking(move || {
                                        connection_diag::scan_msi_local_trigger_workflows(&d)
                                    }).await.unwrap_or_default();
                                    msi.set(refreshed);
                                });
                            },
                            "⚙"
                        }
                    }
                }

                // ── recording indicator ───────────────────────────────────────
                // In the toolbar rather than the Tests view because the actions
                // being captured happen elsewhere — usually in the Connectors
                // panel — and a recording the user has forgotten about is worse
                // than no recording at all.
                if recorder.read().is_recording() {
                    div { class: "rec-indicator",
                        span { class: "rec-dot" }
                        span { class: "rec-name", "{recorder.read().name}" }
                        span { class: "rec-count", "{recorder.read().steps.len()} step(s)" }
                        button {
                            class: "btn btn-small btn-danger",
                            title: "Stop recording and review the captured steps",
                            onclick: move |_| {
                                recorder.write().stop();
                                visited_views.write().insert("Tests".into());
                                current_view.set("Tests".into());
                            },
                            "■ Stop"
                        }
                    }
                }

                // ── spacer pushes the right group to the far edge ─────────────
                div { style: "flex:1; min-width:0" }

                // ── view switch: Workflows | Functions | Tests | DevOps | Settings ──
                // Icon-only: the full name is still available on hover via
                // `title`, and five spelled-out labels plus Graph didn't fit the
                // header alongside the service buttons and status widgets.
                div { class: "view-switch",
                    button {
                        class: if *current_view.read() == "Workflows" { "view-btn active" } else { "view-btn" },
                        title: "Workflows — workflow list and run detail",
                        onclick: move |_| current_view.set("Workflows".into()),
                        "📋"
                    }
                    button {
                        class: if *current_view.read() == "Functions" { "view-btn active" } else { "view-btn" },
                        title: "Functions — browse and edit function app source files",
                        onclick: move |_| { visited_views.write().insert("Functions".into()); current_view.set("Functions".into()); },
                        "𝑓(x)"
                    }
                    button {
                        class: if *current_view.read() == "Tests" { "view-btn active" } else { "view-btn" },
                        title: "Tests — saved scenarios, replayed against the local emulators",
                        onclick: move |_| { visited_views.write().insert("Tests".into()); current_view.set("Tests".into()); },
                        "🧪"
                    }
                    button {
                        class: if *current_view.read() == "DevOps" { "view-btn active" } else { "view-btn" },
                        title: "DevOps — Azure DevOps pipelines and runs",
                        onclick: move |_| { visited_views.write().insert("DevOps".into()); current_view.set("DevOps".into()); },
                        "🚀"
                    }
                    button {
                        class: if *current_view.read() == "Settings" { "view-btn active" } else { "view-btn" },
                        title: "Settings — edit local.settings.json",
                        onclick: move |_| { visited_views.write().insert("Settings".into()); current_view.set("Settings".into()); },
                        "🛠"
                    }
                }

                // Graph is a visualization, not a config/list panel like the rest of
                // the switch — kept out of that group and set off with its own
                // divider so it doesn't read as "just another tab".
                div { class: "view-switch view-switch-graph",
                    button {
                        class: if *current_view.read() == "Graph" { "view-btn active" } else { "view-btn" },
                        title: "Graph — workflow chain graph, interactive D3.js visualization",
                        onclick: move |_| { visited_views.write().insert("Graph".into()); current_view.set("Graph".into()); },
                        "🔗"
                    }
                }

                // ── panel toggles: Connections + Azure ────────────────────────
                div { class: "toolbar-panels",
                    {connections_button(&dir, sql_wfs, msi_wfs, sql_conns, sb_namespace, sb_queues,
                        sb_namespace_key, sb_conn_str, sftp_conns, blob_conns, cosmos_conns,
                        webjobs_storage, db_panel_open, azure_panel_open)}
                    button {
                        class: if *azure_panel_open.read() { "btn btn-small btn-panel active" } else { "btn btn-small btn-panel" },
                        title: "Compare local workflows with Azure",
                        onclick: move |_| {
                            let next = !*azure_panel_open.read();
                            azure_panel_open.set(next);
                            if next { db_panel_open.set(false); }
                        },
                        "☁ Azure"
                    }
                }

                button {
                    class: "btn-theme",
                    title: if *is_light.read() { "Switch to dark mode" } else { "Switch to light mode" },
                    onclick: move |_| {
                        let next = !*is_light.read();
                        ctx.theme_overridden.set(true);
                        is_light.set(next);
                        document::eval(&format!("document.body.className = '{}';", if next { "light" } else { "" }));
                    },
                    if *is_light.read() { "🌙" } else { "☀" }
                }
            }

            // ── Tool check banner ─────────────────────────────────────────
            {
                let missing: Vec<system_check::ToolStatus> = tool_statuses.read().iter()
                    .filter(|t| !t.available).cloned().collect();
                if !missing.is_empty() && !*tools_dismissed.read() {
                    let log_path = dirs::data_local_dir()
                        .unwrap_or_else(std::env::temp_dir)
                        .join("AIS Runner")
                        .join("tool-check.log");
                    let log_path_str = log_path.to_string_lossy().to_string();
                    let copy_log_path = log_path_str.clone();
                    rsx! {
                        div { id: "tool-banner",
                            span { id: "tool-banner-icon", "⚠" }
                            div { id: "tool-banner-items",
                                for t in missing {
                                    span { class: "tool-banner-item",
                                        title: "{t.diagnostic}",
                                        strong { "{t.name}" }
                                        // version present = installed but not running (e.g. Docker daemon stopped)
                                        // version absent  = not installed at all
                                        if t.version.is_some() {
                                            " not running — "
                                        } else {
                                            " not found — "
                                        }
                                        code { "{t.install_hint}" }
                                        " "
                                        span { style: "opacity:.7;font-size:10px;cursor:help",
                                            title: "{t.diagnostic}",
                                            "🔍 hover for details"
                                        }
                                    }
                                }
                                span { class: "tool-banner-item", style: "opacity:.7;font-size:11px",
                                    "Full log: "
                                    code {
                                        title: "Click to copy path",
                                        onclick: move |_| {
                                            let p = copy_log_path.clone();
                                            std::thread::spawn(move || {
                                                if let Ok(mut cb) = arboard::Clipboard::new() {
                                                    let _ = cb.set_text(p);
                                                }
                                            });
                                        },
                                        "{log_path_str}"
                                    }
                                }
                            }
                            button { class: "btn-icon", onclick: move |_| tools_dismissed.set(true), "×" }
                        }
                    }
                } else { rsx! {} }
            }

            // ── Main content ──────────────────────────────────────────────
            div { id: "main",
                // Lazy-mounted panels: only created on first visit, then kept alive
                // so caches/signals survive tab switches.  This avoids the heavy
                // initial DOM build that caused a multi-second freeze on project open.
                if visited_views.read().contains("Settings") {
                    div { style: if *current_view.read() == "Settings" { "display:contents" } else { "display:none" },
                        SettingsEditor { logic_apps_dir: dir.clone() }
                    }
                }
                if visited_views.read().contains("DevOps") {
                    div { style: if *current_view.read() == "DevOps" { "display:contents" } else { "display:none" },
                        DevOpsPanel { logic_apps_dir: dir.clone() }
                    }
                }
                if visited_views.read().contains("Tests") {
                    div { style: if *current_view.read() == "Tests" { "display:contents" } else { "display:none" },
                        TestsPanel { logic_apps_dir: dir.clone() }
                    }
                }
                if visited_views.read().contains("Graph") {
                    div { style: if *current_view.read() == "Graph" { "display:contents" } else { "display:none" },
                        GraphPanel { logic_apps_dir: dir.clone(), is_light: is_light }
                    }
                }
                if visited_views.read().contains("Functions") {
                    div { style: if *current_view.read() == "Functions" { "display:contents" } else { "display:none" },
                        FuncPanel {
                            func_apps_dir: func_apps_dir.clone(),
                            state:      java_func_state,
                            proc:       java_func_proc,
                            log_lines:  log_lines,
                            java_lines: java_func_lines,
                        }
                    }
                }
                // Workflows — always mounted; hidden when another view is active
                div { style: if *current_view.read() == "Workflows" { "display:contents" } else { "display:none" },
                    WorkflowList { // always-rendered content block
                        workflows:  workflows.read().clone(),
                        selected:   selected_wf.read().clone(),
                        traced:     traced_wfs.read().clone(),
                        running:    running_wfs.read().clone(),
                        sql_wfs:    sql_wfs.read().clone(),
                        msi_wfs:    msi_wfs.read().clone(),
                        connectors: wf_connectors.read().clone(),
                        last_ran:   last_ran.read().clone(),
                        on_select: {
                            let dir = dir.clone();
                            move |name: String| workflow_select::handle_select(
                                name, selected_wf, runs, actions, source_text,
                                traced_wfs, cleared_wfs, log_lines, &dir,
                            )
                        },
                        on_run: {
                            let dir = dir.clone();
                            move |(name, trigger_name, trigger_type): (String, String, String)| {
                                workflow_run::handle_open_dialog(
                                    name, trigger_name, trigger_type, &dir,
                                    selected_wf, source_text, active_tab,
                                    run_dialog, log_lines,
                                )
                            }
                        },
                    }
                    div {
                        id: "wf-resize-handle",
                        // Dioxus-native mousedown — survives re-renders triggered
                        // by the workflow filter input. The previous global
                        // document.body delegate occasionally lost events once
                        // the input took focus on some webview versions.
                        onmousedown: move |e| {
                            let start_x = e.client_coordinates().x;
                            document::eval(&format!(r#"
                                (function() {{
                                    var wp = document.getElementById('workflows'); if (!wp) return;
                                    var h  = document.getElementById('wf-resize-handle'); if (h) h.classList.add('dragging');
                                    var startX = {start_x};
                                    var startW = wp.getBoundingClientRect().width;
                                    document.body.style.cursor = 'ew-resize';
                                    document.body.style.userSelect = 'none';
                                    document.body.style.webkitUserSelect = 'none';
                                    var onMove = function(ev) {{
                                        wp.style.width = Math.max(160, Math.min(520, startW + (ev.clientX - startX))) + 'px';
                                    }};
                                    var onUp = function() {{
                                        if (h) h.classList.remove('dragging');
                                        document.body.style.cursor = '';
                                        document.body.style.userSelect = '';
                                        document.body.style.webkitUserSelect = '';
                                        document.removeEventListener('mousemove', onMove);
                                        document.removeEventListener('mouseup', onUp);
                                    }};
                                    document.addEventListener('mousemove', onMove);
                                    document.addEventListener('mouseup', onUp);
                                }})();
                            "#));
                        }
                    }
                    RunDetail {
                        workflow:       selected_wf.read().clone(),
                        source_text:    source_text,
                        analysis:       wf_analysis.read().clone(),
                        sproc_status:   sproc_status,
                        source_path:    selected_wf.read().as_ref().map(|name| {
                            workflows::resolve_logic_apps_dir(&dir)
                                .join(name).join("workflow.json")
                                .to_string_lossy().to_string()
                        }),
                        runs:          runs.read().clone(),
                        actions:       actions.read().clone(),
                        is_live:       selected_wf.read().as_deref()
                            .map(|n| running_wfs.read().contains(n)).unwrap_or(false),
                        health_error:  selected_wf.read().as_ref().and_then(|name| {
                            workflows.read().iter().find(|w| &w.name == name)
                                .and_then(|w| w.health_error.clone())
                        }),
                        logs: {
                            let wf_name = selected_wf.read().clone();
                            let logs    = log_lines.read();
                            if let Some(name) = wf_name {
                                logs.iter().filter(|l| l.msg.contains(&name)).cloned().collect()
                            } else { vec![] }
                        },
                        az_lines: az_lines,
                        active_tab: active_tab,
                        on_run: {
                            let dir = dir.clone();
                            move |_| workflow_run::handle_trigger_from_detail(
                                &dir, workflows, selected_wf, run_dialog, log_lines,
                            )
                        },
                        on_refresh:    move |_| workflow_select::handle_refresh(selected_wf, runs, actions, cleared_wfs, log_lines),
                        on_clear_runs: move |_| {
                            runs.write().clear();
                            actions.write().clear();
                            if let Some(wf) = selected_wf.read().clone() {
                                traced_wfs.write().remove(&wf);
                                cleared_wfs.write().insert(wf, Utc::now().to_rfc3339());
                            }
                        },
                        on_select_run: move |run_id: String| {
                            workflow_select::handle_select_run(run_id, selected_wf, actions, log_lines)
                        },
                        suggested_payload: selected_wf.read().as_ref()
                            .map(|name| crate::services::payload::suggest_payload(&dir, name))
                            .unwrap_or_default(),
                        services_ready: matches!(*azurite_state.read(), ServiceState::Running)
                            && matches!(*func_state.read(), ServiceState::Running),
                        services_starting: matches!(*azurite_state.read(), ServiceState::Starting)
                            || matches!(*func_state.read(), ServiceState::Starting),
                        workflow_count: workflows.read().len(),
                        azurite_state: azurite_state.read().clone(),
                        func_state:    func_state.read().clone(),
                        on_start_azurite: move |_| azurite::handle_start(azurite_state, azurite_proc, log_lines),
                        on_start_func: {
                            let dir = dir.clone();
                            move |_| func_start::handle_start(
                                azurite_state, func_state, func_proc, workflows,
                                traced_wfs, cleared_wfs, log_lines, dir.clone(),
                            )
                        },
                    }
                }

                // Azure panel — always in DOM as flex sibling; CSS drives the slide
                {
                    let mut push2  = make_push(log_lines);
                    let dir_reload = dir.clone();
                    rsx! {
                        div {
                            id: "az-panel-slot",
                            class: if *azure_panel_open.read() { "open" } else { "" },
                            AzurePanel {
                                logic_apps_dir:  dir.clone(),
                                local_workflows: workflows.read().iter().map(|w| w.name.clone()).collect(),
                                diff_cache:      az_diff_cache,
                                tenant_id:       workspace_link.as_ref().and_then(|l| l.tenant_id.clone()),
                                is_open:         azure_panel_open,
                                on_pulled: move |name: String| {
                                    push2(format!("⬇ {} pulled from Azure", name), LogLevel::Ok);
                                    let is_running = matches!(*func_state.read(), ServiceState::Running);
                                    let dir_c = dir_reload.clone();
                                    spawn(async move {
                                        if is_running {
                                            if let Ok(mut list) = workflows::list_workflows().await {
                                                let dir_e = dir_c.clone();
                                                if let Ok(providers) = tokio::task::spawn_blocking(move || {
                                                    workflows::scan_trigger_providers(&dir_e)
                                                }).await {
                                                    for w in &mut list {
                                                        if w.trigger_provider.is_none() {
                                                            w.trigger_provider = providers.get(&w.name).cloned();
                                                        }
                                                    }
                                                }
                                                workflows.set(list);
                                            }
                                        } else {
                                            let list = tokio::task::spawn_blocking(move || {
                                                workflows::scan_local_workflows(&dir_c)
                                            }).await.unwrap_or_default();
                                            if !list.is_empty() { workflows.set(list); }
                                        }
                                    });
                                },
                            }
                        }
                    }
                }

                // Connections panel — same slot pattern
                {
                    let dir_db = dir.clone(); // keep `dir` available for RunDialog below
                    let d1 = dir_db.clone();
                    let d2 = dir_db.clone();
                    let mut push = make_push(log_lines);
                    rsx! {
                        div {
                            id: "db-panel-slot",
                            class: if *db_panel_open.read() { "open" } else { "" },
                            DbPanel {
                                logic_apps_dir:     dir.clone(),
                                connections:        sql_conns.read().clone(),
                                sb_namespace:       sb_namespace.read().clone(),
                                sb_namespace_key:   sb_namespace_key.read().clone(),
                                sb_conn_str:        sb_conn_str.read().clone(),
                                sb_queues:          sb_queues.read().clone(),
                                sftp_connections:   sftp_conns.read().clone(),
                                blob_connections:   blob_conns.read().clone(),
                                webjobs_storage:    webjobs_storage.read().clone(),
                                cosmos_connections: cosmos_conns.read().clone(),
                                env_mode:           current_env.read().clone(),
                                azurite_running:    *azurite_state.read() == ServiceState::Running,
                                is_open:            db_panel_open,
                                az_status:          az_status,
                                on_env_changed: move |_| {
                                    let d  = d1.clone();
                                    let d2 = d2.clone();
                                    spawn(async move {
                                        current_env.set(
                                            tokio::task::spawn_blocking(move || env_mode::detect_mode(&d))
                                                .await.unwrap_or(EnvMode::Unknown)
                                        );
                                        blob_conns.set(
                                            tokio::task::spawn_blocking(move || blob_check::detect_blob_connections(&d2))
                                                .await.unwrap_or_default()
                                        );
                                    });
                                },
                                on_saved: move |msg: String| {
                                    push(msg, LogLevel::Warn);
                                    let d = dir_db.clone();
                                    spawn(async move {
                                        let (wfs, msi2, conns, (sb_ns, sb_qs), sb_key, sb_cs_key, sftp, blobs, cosmos, wjs) =
                                            tokio::task::spawn_blocking(move || {
                                                (sql_check::detect_sql_workflows(&d),
                                                 connection_diag::scan_msi_local_trigger_workflows(&d),
                                                 sql_check::load_sql_connections(&d),
                                                 sb_check::detect_sb_queues(&d),
                                                 sb_check::detect_sb_namespace_key(&d),
                                                 sb_check::detect_sb_conn_str_key(&d),
                                                 sftp_check::detect_sftp_connections(&d),
                                                 blob_check::detect_blob_connections(&d),
                                                 cosmos_check::detect_cosmos_connections(&d),
                                                 blob_check::read_webjobs_storage(&d))
                                            }).await.unwrap_or_default();
                                        sql_wfs.set(wfs); msi_wfs.set(msi2); sql_conns.set(conns);
                                        sb_namespace.set(sb_ns); sb_queues.set(sb_qs);
                                        sb_namespace_key.set(sb_key); sb_conn_str.set(sb_cs_key);
                                        sftp_conns.set(sftp); blob_conns.set(blobs);
                                        cosmos_conns.set(cosmos); webjobs_storage.set(wjs);
                                    });
                                },
                            }
                        }
                    }
                }
            }

            div {
                id: "log-resize-handle",
                // Native Dioxus mousedown — same reasoning as wf-resize-handle:
                // global delegated listeners on document.body can get clipped
                // once a text input takes focus in some webview builds.
                onmousedown: move |e| {
                    let start_y = e.client_coordinates().y;
                    document::eval(&format!(r#"
                        (function() {{
                            var lp = document.getElementById('log-panel'); if (!lp) return;
                            var h  = document.getElementById('log-resize-handle'); if (h) h.classList.add('dragging');
                            var startY = {start_y};
                            var startH = lp.getBoundingClientRect().height;
                            document.body.style.cursor = 'ns-resize';
                            document.body.style.userSelect = 'none';
                            document.body.style.webkitUserSelect = 'none';
                            var onMove = function(ev) {{
                                lp.style.height = Math.max(80, Math.min(Math.floor(window.innerHeight * 0.6), startH + (startY - ev.clientY))) + 'px';
                            }};
                            var onUp = function() {{
                                if (h) h.classList.remove('dragging');
                                document.body.style.cursor = '';
                                document.body.style.userSelect = '';
                                document.body.style.webkitUserSelect = '';
                                document.removeEventListener('mousemove', onMove);
                                document.removeEventListener('mouseup', onUp);
                            }};
                            document.addEventListener('mousemove', onMove);
                            document.addEventListener('mouseup', onUp);
                        }})();
                    "#));
                }
            }

            LogPanel {
                lines:         log_lines,
                az_lines:      az_lines,
                sb_emu_lines:  sb_emu_lines,
                java_lines:    java_func_lines,
                sql_dev_lines: sql_dev_lines,
                on_clear:      move |_| { log_lines.write().clear(); },
                workspace_dir: dir.clone(),
            }

            if let Some((wf_name, trigger_name, trigger_type, suggested, blob_container, queue_name)) = run_dialog.read().clone() {
                RunDialog {
                    workflow:        wf_name.clone(),
                    trigger_type:    trigger_type.clone(),
                    payload:         suggested,
                    blob_container:  blob_container,
                    queue_name:      queue_name,
                    on_cancel:       move |_| run_dialog.set(None),
                    on_run: {
                        let dir = dir.clone();
                        move |(queue_or_blob, body): (String, String)| workflow_run::handle_run(
                            wf_name.clone(), trigger_name.clone(), trigger_type.clone(),
                            queue_or_blob, body, &dir,
                            runs, actions, log_lines, running_wfs, active_tab,
                            traced_wfs, cleared_wfs, run_dialog, last_ran, run_gate,
                            recorder,
                        )
                    },
                }
            }

            // ── Local-readiness consent gate ──────────────────────────────
            if let Some(readiness) = run_gate.read().clone() {
                RunGateDialog {
                    readiness: readiness.clone(),
                    on_cancel: move |_| run_gate.set(None),
                    on_fix: {
                        let dir = dir.clone();
                        move |_| {
                            let mut push = make_push(log_lines);
                            let report = workflow_run::apply_readiness_fixes(&dir, &readiness);
                            if !report.auto_filled.is_empty() {
                                push(format!("🔧 Filled local defaults in local.settings.json: {}",
                                    report.auto_filled.join(", ")), LogLevel::Ok);
                            }
                            if !report.redirected.is_empty() {
                                push(format!("🔧 Redirected cloud endpoints → local: {}",
                                    report.redirected.join(", ")), LogLevel::Ok);
                            }
                            if let Some(path) = &report.scaffolded {
                                push(format!("🔧 Ensured connections.local.json exists: {path}"), LogLevel::Ok);
                            }
                            if readiness.needs_manual() {
                                push("⚠ Some settings need real values only you can provide — see the list, edit \
                                      local.settings.json / connections.local.json, then restart func.".into(),
                                    LogLevel::Warn);
                            } else {
                                push("✅ Local files updated. Restart func (⏹ then ▶) so it reloads the new \
                                      connection config, then run the workflow again.".into(),
                                    LogLevel::Ok);
                            }
                            for e in &report.errors {
                                push(format!("❌ {e}"), LogLevel::Error);
                            }
                            run_gate.set(None);
                        }
                    },
                }
            }
        }
    }
}

fn epoch_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ── Setup banner ──────────────────────────────────────────────────────────────

fn setup_banner(
    setup_status: Signal<setup_manager::SetupStatus>,
    workspace_link: &Option<config::WorkspaceLink>,
    mut current_view: Signal<String>,
    mut visited_views: Signal<HashSet<String>>,
    dir: &str,
    log_lines: Signal<Vec<LogLine>>,
) -> Element {
    let dir   = dir.to_string();
    let link  = workspace_link.clone();
    let mut ss = setup_status;
    match setup_status.read().clone() {
        setup_manager::SetupStatus::MissingSettings => rsx! {
            div { class: "setup-banner",
                span { "⚠ local.settings.json is missing and no template was found." }
                button {
                    class: "setup-banner-btn",
                    onclick: {
                        let dir = dir.clone(); let link = link.clone();
                        move |_| setup::handle_initialize_default(&dir, setup_status, log_lines, link.clone())
                    },
                    "Bootstrap Default Settings"
                }
            }
        },
        setup_manager::SetupStatus::RemoteStorage => rsx! {
            div { class: "setup-banner",
                span { "⚠ AzureWebJobsStorage points to a remote Azure account — func cannot start locally." }
                button {
                    class: "setup-banner-btn",
                    onclick: {
                        let dir = dir.clone();
                        move |_| {
                            let d  = dir.clone();
                            let d2 = dir.clone();
                            let mut push = make_push(log_lines);
                            spawn(async move {
                                match tokio::task::spawn_blocking(move || setup_manager::fix_remote_storage(&d)).await.unwrap_or(Err("task panicked".into())) {
                                    Ok(_)  => { push("✅ AzureWebJobsStorage → UseDevelopmentStorage=true".into(), LogLevel::Ok); ss.set(setup_manager::check_setup(&d2)); }
                                    Err(e) => push(format!("Failed: {}", e), LogLevel::Error),
                                }
                            });
                        }
                    },
                    "Fix → UseDevelopmentStorage=true"
                }
            }
        },
        setup_manager::SetupStatus::NeedsInitialization => rsx! {
            div { class: "setup-banner",
                span { "🚀 Your environment is not initialized yet. Create local.settings.json from template?" }
                button {
                    class: "setup-banner-btn",
                    onclick: {
                        let dir = dir.clone();
                        move |_| setup::handle_initialize(&dir, setup_status, log_lines)
                    },
                    "Bootstrap Settings"
                }
            }
        },
        // Blank values and absent keys arrive together and get a row each —
        // they need different fixes, so they keep their own buttons. Each row
        // names the keys: "3 settings require attention" on its own left the
        // user grepping local.settings.json to find out which three.
        setup_manager::SetupStatus::NeedsConfiguration { blank, absent } => rsx! {
            div { class: "setup-banner setup-banner-stack",
                if !blank.is_empty() {
                    div { class: "setup-banner-row",
                        span { "⚠ {blank.len()} setting(s) need a value: {setup_manager::summarize_keys(&blank)}" }
                        if link.is_some() {
                            button {
                                class: "setup-banner-btn",
                                style: "background: var(--blue); margin-right: 8px;",
                                onclick: {
                                    let dir = dir.clone(); let link = link.clone();
                                    move |_| setup::handle_auto_detect(&dir, setup_status, log_lines, link.clone())
                                },
                                "Auto-Detect from Azure"
                            }
                        }
                        button {
                            class: "setup-banner-btn",
                            onclick: move |_| {
                                visited_views.write().insert("Settings".into());
                                current_view.set("Settings".into());
                            },
                            "Configure Manually"
                        }
                    }
                }
                if !absent.is_empty() {
                    div { class: "setup-banner-row",
                        span { "⚠ {absent.len()} key(s) referenced in connections.json are missing from local.settings.json: {setup_manager::summarize_keys(&absent)}" }
                        button {
                            class: "setup-banner-btn",
                            style: "background: var(--blue); margin-right: 8px;",
                            onclick: {
                                let absent = absent.clone(); let dir = dir.clone();
                                move |_| {
                                    let _ = setup_manager::stub_missing_keys(&dir, &absent);
                                    ss.set(setup_manager::check_setup(&dir));
                                }
                            },
                            "Auto-stub Missing Keys"
                        }
                        button {
                            class: "setup-banner-btn",
                            onclick: move |_| {
                                visited_views.write().insert("Settings".into());
                                current_view.set("Settings".into());
                            },
                            "Edit Manually"
                        }
                    }
                }
            }
        },
        _ => rsx! {},
    }
}

// ── Azure login widget ────────────────────────────────────────────────────────

fn az_login_widget(
    mut az_status:     Signal<Option<Result<String, azure_cli::AzError>>>,
    mut active_tenant: Signal<Option<String>>,
    configured_tenant: Option<String>,
    dir: &str,
) -> Element {
    let dir = dir.to_string();

    // Tenant badge: (label, css_class, tooltip)
    // Only show tenant badge when a workspace tenant is configured and there's a mismatch.
    // When no tenant is configured, hiding the badge avoids displaying the raw GUID.
    let tenant_badge: Option<(String, &'static str, String)> =
        active_tenant.read().as_deref().and_then(|active| {
            match &configured_tenant {
                Some(cfg) if !cfg.is_empty() => {
                    let short     = &active[..active.len().min(8)];
                    let cfg_short = &cfg[..cfg.len().min(8)];
                    if active.starts_with(cfg_short) || cfg.starts_with(short) {
                        // tenant matches config — no badge needed, all good
                        None
                    } else {
                        // mismatch — show warning
                        Some((
                            format!("⚠ tenant mismatch"),
                            "az-tenant-badge az-tenant-mismatch",
                            format!("Tenant mismatch!\nActive:     {}\nConfigured: {}\nClick ⟳ or re-login to fix.", active, cfg),
                        ))
                    }
                }
                // no workspace tenant configured — don't show the raw GUID
                _ => None,
            }
        });

    rsx! {
        div { class: "az-status-wrap",
            match az_status.read().clone() {
                None => rsx! {
                    div { class: "az-block az-block-checking",
                        span { class: "dot starting" }
                        span { class: "az-account", "Checking…" }
                    }
                },
                Some(Ok(name)) => rsx! {
                    div { class: "az-block az-block-ok",
                        span { class: "dot running" }
                        span { class: "az-account", title: "{name}", "{name}" }
                        if let Some((label, cls, tip)) = tenant_badge {
                            span { class: "{cls}", title: "{tip}", "{label}" }
                        }
                        button {
                            class: "az-action-btn", title: "Re-check login status",
                            onclick: move |_| {
                                az_status.set(None);
                                spawn(async move {
                                    let (result, tenant) = tokio::task::spawn_blocking(|| {
                                        let r = azure_cli::check_login();
                                        let t = if r.is_ok() { azure_cli::get_active_tenant().ok() } else { None };
                                        (r, t)
                                    }).await.unwrap_or((Err(azure_cli::AzError::Other("check failed".into())), None));
                                    az_status.set(Some(result));
                                    active_tenant.set(tenant);
                                });
                            },
                            "⟳"
                        }
                        button {
                            class: "az-action-btn az-logout-btn", title: "Sign out",
                            onclick: move |_| {
                                azure_cli::logout();
                                az_status.set(None);
                                active_tenant.set(None);
                                spawn(async move {
                                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                    az_status.set(Some(Err(azure_cli::AzError::NotLoggedIn)));
                                });
                            },
                            "↪ out"
                        }
                    }
                },
                Some(Err(_)) => rsx! {
                    div { class: "az-block az-block-out",
                        span { class: "dot stopped" }
                        span { class: "az-account az-account-out", "Not signed in" }
                        button {
                            class: "az-login-btn", title: "Sign in with az login",
                            onclick: move |_| {
                                match azure_cli::open_login(configured_tenant.as_deref()) {
                                    Ok(()) => {
                                        az_status.set(None);
                                    }
                                    Err(msg) => {
                                        // Surface the spawn failure so the user isn't left
                                        // wondering why nothing happened.
                                        az_status.set(Some(Err(azure_cli::AzError::Other(msg))));
                                        return;
                                    }
                                }
                                let login_dir = dir.clone();
                                spawn(async move {
                                    for _ in 0..24 {
                                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                        let (result, tenant) = tokio::task::spawn_blocking(|| {
                                            let r = azure_cli::check_login();
                                            let t = if r.is_ok() { azure_cli::get_active_tenant().ok() } else { None };
                                            (r, t)
                                        }).await.unwrap_or((Err(azure_cli::AzError::Other("check failed".into())), None));
                                        let done = result.is_ok();
                                        az_status.set(Some(result));
                                        active_tenant.set(tenant);
                                        if done {
                                            let d = login_dir.clone();
                                            let _ = tokio::task::spawn_blocking(move || {
                                                if let Some(sub) = azure_sync::detect_subscription(&d) {
                                                    let _ = azure_cli::set_subscription(&sub);
                                                }
                                            }).await;
                                            break;
                                        }
                                    }
                                });
                            },
                            "Sign in"
                        }
                    }
                },
            }
        }
    }
}

// ── Env badge ─────────────────────────────────────────────────────────────────

fn env_badge(
    setup_status: Signal<setup_manager::SetupStatus>,
    current_env: Signal<EnvMode>,
) -> Element {
    if matches!(*setup_status.read(), setup_manager::SetupStatus::MissingSettings) { return rsx! {}; }
    let (badge_class, badge_label) = match *current_env.read() {
        EnvMode::Local   => ("env-badge local",   "🏠 Local"),
        EnvMode::Azure   => ("env-badge azure",   "☁ Azure"),
        EnvMode::Mixed   => ("env-badge mixed",   "⚠ Mixed"),
        EnvMode::Unknown => ("env-badge unknown", "? Env"),
    };
    rsx! { span { class: "{badge_class}", title: "Blob storage mode — open Connectors to switch", "{badge_label}" } }
}

// ── Connections button ────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn connections_button(
    dir: &str,
    mut sql_wfs: Signal<HashSet<String>>,
    mut msi_wfs: Signal<HashSet<String>>,
    mut sql_conns: Signal<Vec<sql_check::SqlConnection>>,
    mut sb_namespace: Signal<String>,
    mut sb_queues: Signal<Vec<sb_check::SbQueueInfo>>,
    mut sb_namespace_key: Signal<Option<String>>,
    mut sb_conn_str: Signal<Option<(String, String)>>,
    mut sftp_conns: Signal<Vec<sftp_check::SftpConnection>>,
    mut blob_conns: Signal<Vec<blob_check::BlobConnection>>,
    mut cosmos_conns: Signal<Vec<cosmos_check::CosmosConnection>>,
    mut webjobs_storage: Signal<String>,
    mut db_panel_open: Signal<bool>,
    mut azure_panel_open: Signal<bool>,
) -> Element {
    let dir = dir.to_string();
    rsx! {
        button {
            class: if *db_panel_open.read() { "btn btn-small btn-panel active" } else { "btn btn-small btn-panel" },
            title: "SQL & Service Bus connections — test & configure",
            onclick: move |_| {
                let opening = !*db_panel_open.read();
                db_panel_open.set(opening);
                if opening {
                    azure_panel_open.set(false);
                    let d = dir.clone();
                    spawn(async move {
                        let (wfs, msi3, conns, (sb_ns, sb_qs), sb_key, sb_cs_key, sftp, blobs, cosmos, wjs) =
                            tokio::task::spawn_blocking(move || {
                                (sql_check::detect_sql_workflows(&d),
                                 connection_diag::scan_msi_local_trigger_workflows(&d),
                                 sql_check::load_sql_connections(&d),
                                 sb_check::detect_sb_queues(&d),
                                 sb_check::detect_sb_namespace_key(&d),
                                 sb_check::detect_sb_conn_str_key(&d),
                                 sftp_check::detect_sftp_connections(&d),
                                 blob_check::detect_blob_connections(&d),
                                 cosmos_check::detect_cosmos_connections(&d),
                                 blob_check::read_webjobs_storage(&d))
                            }).await.unwrap_or_default();
                        sql_wfs.set(wfs); msi_wfs.set(msi3); sql_conns.set(conns);
                        sb_namespace.set(sb_ns); sb_queues.set(sb_qs);
                        sb_namespace_key.set(sb_key); sb_conn_str.set(sb_cs_key);
                        sftp_conns.set(sftp); blob_conns.set(blobs);
                        cosmos_conns.set(cosmos); webjobs_storage.set(wjs);
                    });
                }
            },
            "🔌 Connectors"
        }
    }
}

// Resize handles wire up their own mousedown via Dioxus `onmousedown` —
// see the rsx! blocks above. No global JS is needed any more.
