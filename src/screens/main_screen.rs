use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use chrono::Utc;
use dioxus::prelude::*;

use crate::components::{
    log_panel::{LogLevel, LogLine, LogPanel},
    run_detail::RunDetail,
    run_dialog::RunDialog,
    toolbar::ServiceBlock,
    workflow_list::WorkflowList,
    settings_editor::SettingsEditor,
    db_panel::DbPanel,
    azure_panel::AzurePanel,
};
use crate::services::{
    azure_cli, azure_sync, blob_check, config, connection_diag, cosmos_check,
    env_mode::{self, EnvMode},
    process::{ManagedProcess, ServiceState},
    setup_manager, sftp_check, sql_check, sb_check, system_check,
    workflows::{self, WorkflowItem},
};
use crate::utils::make_push;
use crate::handlers::{azurite, func_start, java, setup, workflow_select, workflow_run};

#[derive(Props, Clone, PartialEq)]
pub struct MainScreenProps {
    pub logic_apps_dir: String,
    pub on_back: EventHandler<()>,
}

#[component]
pub fn MainScreen(props: MainScreenProps) -> Element {
    let dir            = props.logic_apps_dir.clone();
    let cfg            = use_signal(config::load);
    let workspace_link = cfg.read().get_link(&dir).cloned();

    // ── Service states & processes ─────────────────────────────────────────
    let azurite_state   = use_signal(|| ServiceState::Stopped);
    let func_state      = use_signal(|| ServiceState::Stopped);
    let java_func_state = use_signal(|| ServiceState::Stopped);
    let azurite_proc    = use_signal(|| Arc::new(ManagedProcess::new()));
    let func_proc       = use_signal(|| Arc::new(ManagedProcess::new()));
    let java_func_proc  = use_signal(|| Arc::new(ManagedProcess::new()));

    let func_apps_dir = std::path::Path::new(&dir)
        .parent()
        .map(|p| p.join("function_apps").to_string_lossy().to_string())
        .unwrap_or_else(|| format!("{}/function_apps", dir));

    // ── Data signals ───────────────────────────────────────────────────────
    let mut workflows   = use_signal(|| Vec::<WorkflowItem>::new());
    let selected_wf     = use_signal(|| Option::<String>::None);
    let source_text = use_signal(String::new);
    let mut runs        = use_signal(|| Vec::<workflows::RunItem>::new());
    let mut actions     = use_signal(|| Vec::<workflows::ActionItem>::new());
    let mut running_wfs = use_signal(|| HashSet::<String>::new());
    let mut current_view = use_signal(|| "Workflows".to_string());
    let system_light    = dark_light::detect() != dark_light::Mode::Dark;
    let mut is_light    = use_signal(|| system_light);
    let active_tab  = use_signal(|| "Source".to_string());
    let mut run_dialog  = use_signal(|| Option::<(String, String, String, String, Option<String>)>::None);
    let mut traced_wfs  = use_signal(|| HashSet::<String>::new());
    let mut cleared_wfs = use_signal(|| HashMap::<String, String>::new());
    let mut auto_watch  = use_signal(|| true);

    // ── Log ────────────────────────────────────────────────────────────────
    let mut log_lines = use_signal(|| Vec::<LogLine>::new());

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

        // Channel: OS thread → UI coroutine.
        // Capacity 16 is plenty; we never burst more than a handful of events.
        let (tx, rx) = tokio::sync::mpsc::channel::<(String, String)>(16);

        // Dedicated OS thread — all blocking HTTP to Azurite happens here.
        let bg_dir   = dir.clone();
        let bg_watch = Arc::clone(&watch_flag);
        let bg_func  = Arc::clone(&func_flag);
        std::thread::Builder::new()
            .name("ais-blob-watcher".into())
            .spawn(move || {
                use std::collections::{HashMap, HashSet};

                let trigger_map = workflows::scan_all_blob_triggers(&bg_dir);

                // Initial snapshot — pre-existing blobs are ignored.
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

                    // One pass: list all containers sequentially on this thread.
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
                            // Non-blocking send — drop the event if channel is full.
                            let _ = tx.try_send((container.clone(), wf_name.clone()));
                        }
                    }
                }
            })
            .ok(); // ignore spawn failure (non-critical background task)

        // UI coroutine — only wakes when the OS thread found something new.
        // rx is not Copy so we use Option to move it into the async block once.
        let rx_opt = std::cell::Cell::new(Some(rx));
        use_coroutine(move |_rx: dioxus::prelude::UnboundedReceiver<()>| {
            let mut rx = rx_opt.take().expect("coroutine called twice");
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

                    let wf     = wf_name.clone();
                    let cleared = cleared_wfs;
                    dioxus::prelude::spawn(workflow_run::poll_for_run(
                        wf, Some(trigger_ts),
                        runs, actions, log_lines,
                        running_wfs, traced_wfs, cleared,
                    ));
                }
            }
        });
    }

    // ── Setup ──────────────────────────────────────────────────────────────
    let dir_for_setup = dir.clone();
    let setup_status  = use_signal(move || setup_manager::check_setup(&dir_for_setup));
    let setup_updates: Signal<HashMap<String, String>> = use_signal(HashMap::new);
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
                    status.set(setup_manager::check_setup(&d2));
                }
            });
        }
    };

    // ── Env mode ───────────────────────────────────────────────────────────
    let dir_for_env    = dir.clone();
    let mut current_env = use_signal(move || env_mode::detect_mode(&dir_for_env));

    // ── Connection / SQL / SB signals ─────────────────────────────────────
    let mut sql_wfs          = use_signal(|| HashSet::<String>::new());
    let mut msi_wfs          = use_signal(|| HashSet::<String>::new());
    let mut wf_connectors    = use_signal(|| std::collections::HashMap::<String, Vec<workflows::ConnectorKind>>::new());
    let mut sql_conns        = use_signal(|| Vec::<sql_check::SqlConnection>::new());
    let mut db_panel_open    = use_signal(|| false);
    let mut azure_panel_open = use_signal(|| false);
    let az_diff_cache = use_signal(|| std::collections::HashMap::<String, crate::components::azure_panel::DiffStatus>::new());
    let mut sftp_conns       = use_signal(|| Vec::<sftp_check::SftpConnection>::new());
    let mut blob_conns       = use_signal(|| Vec::<blob_check::BlobConnection>::new());
    let mut cosmos_conns     = use_signal(|| Vec::<cosmos_check::CosmosConnection>::new());
    let mut webjobs_storage  = use_signal(String::new);
    let mut sb_namespace     = use_signal(|| {
        config::load().get_link(&dir).and_then(|l| l.sb_namespace.clone()).unwrap_or_default()
    });
    let mut sb_namespace_key = use_signal(|| Option::<String>::None);
    let mut sb_conn_str      = use_signal(|| Option::<(String, String)>::None);
    let mut sb_queues        = use_signal(|| Vec::<sb_check::SbQueueInfo>::new());

    // ── Tool check / Azure login ───────────────────────────────────────────
    let mut tool_statuses    = use_signal(|| Vec::<system_check::ToolStatus>::new());
    let mut tools_dismissed  = use_signal(|| false);
    let mut az_status:     Signal<Option<Result<String, azure_cli::AzError>>> = use_signal(|| None);
    let mut active_tenant: Signal<Option<String>>                          = use_signal(|| None);

    // ══ Effects ════════════════════════════════════════════════════════════

    use_effect(move || {
        document::eval(&format!("document.body.className = '{}';", if *is_light.read() { "light" } else { "" }));
    });

    use_effect(move || {
        spawn(async move {
            tool_statuses.set(tokio::task::spawn_blocking(system_check::check_tools).await.unwrap_or_default());
        });
    });

    use_effect(move || {
        spawn(async move {
            let (result, tenant) = tokio::task::spawn_blocking(|| {
                let r = azure_cli::check_login();
                let t = if r.is_ok() { azure_cli::get_active_tenant().ok() } else { None };
                (r, t)
            }).await.unwrap_or((Err(azure_cli::AzError::Other("check failed".into())), None));
            az_status.set(Some(result));
            active_tenant.set(tenant);
        });
    });

    use_effect({
        let dir2     = dir.clone();
        let mut cfg2 = cfg;
        move || {
            if cfg2.read().get_link(&dir2).is_none() {
                if let Some(link) = crate::services::settings_file::try_bootstrap_link(&dir2) {
                    let mut c = cfg2.write();
                    c.set_link(dir2.clone(), link);
                    config::save(&c);
                }
            }
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

    use_effect(move || { document::eval(RESIZE_JS); });

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

    // ══ View ═══════════════════════════════════════════════════════════════
    let dir_label = dir.clone();

    rsx! {
        div { id: "app",

            // ── Setup banner ──────────────────────────────────────────────
            {setup_banner(setup_status, &workspace_link, active_tab, &dir, log_lines)}

            // ── Toolbar ───────────────────────────────────────────────────
            div { id: "toolbar",
                button { class: "btn-back", onclick: move |_| props.on_back.call(()), "‹ Back" }
                span { id: "toolbar-dir", title: "{dir_label}", "{dir_label}" }

                ServiceBlock {
                    label: "Azurite".to_string(),
                    cmd:   "azurite --location /tmp/azurite".to_string(),
                    state: azurite_state.read().clone(),
                    on_start: move |_| azurite::handle_start(azurite_state, azurite_proc, log_lines),
                    on_stop:  move |_| azurite::handle_stop(azurite_state, azurite_proc, log_lines),
                }
                button {
                    class: "btn btn-warn btn-svc",
                    title: "Stop func + Azurite, wipe storage, restart Azurite — fixes 'run not recording'",
                    onclick: move |_| azurite::handle_reset(
                        azurite_state, azurite_proc, func_state, func_proc, log_lines,
                    ),
                    "⟳ Reset"
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
                    on_stop: move |_| func_start::handle_stop(func_state, func_proc, log_lines),
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
                ServiceBlock {
                    label: "Java Functions".to_string(),
                    cmd:   "mvn azure-functions:run".to_string(),
                    state: java_func_state.read().clone(),
                    on_start: {
                        let d = func_apps_dir.clone();
                        move |_| java::handle_start(java_func_state, java_func_proc, log_lines, &d)
                    },
                    on_stop: move |_| java::handle_stop(java_func_state, java_func_proc, log_lines),
                }

                {az_login_widget(az_status, active_tenant, workspace_link.as_ref().and_then(|l| l.tenant_id.clone()), &dir)}
                {env_badge(setup_status, current_env)}

                // ── spacer pushes the right group to the far edge ─────────────
                div { style: "flex:1; min-width:0" }

                // ── view switch: Workflows | Settings ─────────────────────────
                div { class: "view-switch",
                    button {
                        class: if *current_view.read() != "Settings" { "view-btn active" } else { "view-btn" },
                        title: "Workflow list and run detail",
                        onclick: move |_| current_view.set("Workflows".into()),
                        "Workflows"
                    }
                    button {
                        class: if *current_view.read() == "Settings" { "view-btn active" } else { "view-btn" },
                        title: "Edit local.settings.json",
                        onclick: move |_| current_view.set("Settings".into()),
                        "Settings"
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
                    rsx! {
                        div { id: "tool-banner",
                            span { id: "tool-banner-icon", "⚠" }
                            div { id: "tool-banner-items",
                                for t in missing {
                                    span { class: "tool-banner-item",
                                        strong { "{t.name}" }
                                        " not found — install: "
                                        code { "{t.install_hint}" }
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
                if *current_view.read() == "Settings" {
                    SettingsEditor { logic_apps_dir: dir.clone() }
                } else {
                    WorkflowList { // always-rendered content block
                        workflows:  workflows.read().clone(),
                        selected:   selected_wf.read().clone(),
                        traced:     traced_wfs.read().clone(),
                        running:    running_wfs.read().clone(),
                        sql_wfs:    sql_wfs.read().clone(),
                        msi_wfs:    msi_wfs.read().clone(),
                        connectors: wf_connectors.read().clone(),
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
                                    az_status, run_dialog, log_lines,
                                )
                            }
                        },
                    }
                    div { id: "wf-resize-handle" }
                    RunDetail {
                        workflow:      selected_wf.read().clone(),
                        source_text:   source_text.read().clone(),
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
                        active_tab: active_tab,
                        on_run: {
                            let dir = dir.clone();
                            move |_| workflow_run::handle_trigger_from_detail(
                                &dir, workflows, selected_wf, az_status, run_dialog, log_lines,
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

            div { id: "log-resize-handle" }

            LogPanel {
                lines:    log_lines,
                on_clear: move |_| { log_lines.write().clear(); },
            }

            if let Some((wf_name, trigger_name, trigger_type, suggested, blob_container)) = run_dialog.read().clone() {
                RunDialog {
                    workflow:        wf_name.clone(),
                    trigger_type:    trigger_type.clone(),
                    payload:         suggested,
                    blob_container:  blob_container,
                    on_cancel:       move |_| run_dialog.set(None),
                    on_run: {
                        let dir = dir.clone();
                        move |(_blob_name, body): (String, String)| workflow_run::handle_run(
                            wf_name.clone(), trigger_name.clone(), trigger_type.clone(),
                            body, &dir,
                            runs, actions, log_lines, running_wfs, active_tab,
                            traced_wfs, cleared_wfs, run_dialog,
                        )
                    },
                }
            }
        }
    }
}

// ── Setup banner ──────────────────────────────────────────────────────────────

fn setup_banner(
    setup_status: Signal<setup_manager::SetupStatus>,
    workspace_link: &Option<config::WorkspaceLink>,
    mut active_tab: Signal<String>,
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
        setup_manager::SetupStatus::NeedsConfiguration(count) => rsx! {
            div { class: "setup-banner",
                span { "⚠ {count} settings require attention (SQL passwords, Azure endpoints)." }
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
                    onclick: move |_| active_tab.set("Settings".into()),
                    "Configure Manually"
                }
            }
        },
        setup_manager::SetupStatus::MissingKeys(keys) => rsx! {
            div { class: "setup-banner",
                span { "⚠ {keys.len()} key(s) referenced in connections.json are missing from local.settings.json." }
                button {
                    class: "setup-banner-btn",
                    style: "background: var(--blue); margin-right: 8px;",
                    onclick: {
                        let keys = keys.clone(); let dir = dir.clone();
                        move |_| {
                            let _ = setup_manager::stub_missing_keys(&dir, &keys);
                            ss.set(setup_manager::check_setup(&dir));
                        }
                    },
                    "Auto-stub Missing Keys"
                }
                button {
                    class: "setup-banner-btn",
                    onclick: move |_| active_tab.set("Settings".into()),
                    "Edit Manually"
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
    let tenant_badge: Option<(String, &'static str, String)> =
        active_tenant.read().as_deref().map(|active| {
            let short = &active[..active.len().min(8)];
            match &configured_tenant {
                Some(cfg) if !cfg.is_empty() => {
                    let cfg_short = &cfg[..cfg.len().min(8)];
                    if active.starts_with(cfg_short) || cfg.starts_with(short) {
                        // match
                        (format!("{}", short), "az-tenant-badge",
                         format!("Active tenant: {}\nWorkspace tenant: {} ✓", active, cfg))
                    } else {
                        // mismatch
                        (format!("⚠ {}", short), "az-tenant-badge az-tenant-mismatch",
                         format!("Tenant mismatch!\nActive:     {}\nConfigured: {}\nClick ⟳ or re-login to fix.", active, cfg))
                    }
                }
                // no workspace tenant configured — just show what's active
                _ => (format!("{}", short), "az-tenant-badge az-tenant-default",
                      format!("Active tenant: {}\nNo tenant pinned for this workspace — set one in Settings.", active)),
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
                                azure_cli::open_login(configured_tenant.as_deref());
                                az_status.set(None);
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
    rsx! { span { class: "{badge_class}", title: "Blob storage mode — open Connections to switch", "{badge_label}" } }
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
            "🔌 Connections"
        }
    }
}

// ── Resize JS ─────────────────────────────────────────────────────────────────

const RESIZE_JS: &str = r#"
(function() {
    if (window.__ais_resize_init) return;
    window.__ais_resize_init = true;
    document.body.addEventListener('mousedown', function(e) {
        var target = e.target;
        if (!target) return;
        if (target.id === 'log-resize-handle') {
            e.preventDefault();
            var lp = document.getElementById('log-panel'); if (!lp) return;
            var startY = e.clientY, startH = lp.getBoundingClientRect().height;
            target.classList.add('dragging');
            document.body.style.cursor = 'ns-resize'; document.body.style.userSelect = 'none'; document.body.style.webkitUserSelect = 'none';
            var onMove = function(ev) { lp.style.height = Math.max(80, Math.min(Math.floor(window.innerHeight / 3), startH + (startY - ev.clientY))) + 'px'; };
            var onUp   = function() {
                var h = document.getElementById('log-resize-handle'); if (h) h.classList.remove('dragging');
                document.body.style.cursor = ''; document.body.style.userSelect = ''; document.body.style.webkitUserSelect = '';
                document.removeEventListener('mousemove', onMove); document.removeEventListener('mouseup', onUp);
            };
            document.addEventListener('mousemove', onMove); document.addEventListener('mouseup', onUp);
        } else if (target.id === 'wf-resize-handle') {
            e.preventDefault();
            var wp = document.getElementById('workflows'); if (!wp) return;
            var startX = e.clientX, startW = wp.getBoundingClientRect().width;
            target.classList.add('dragging');
            document.body.style.cursor = 'ew-resize'; document.body.style.userSelect = 'none'; document.body.style.webkitUserSelect = 'none';
            var onMove2 = function(ev) { wp.style.width = Math.max(160, Math.min(520, startW + (ev.clientX - startX))) + 'px'; };
            var onUp2   = function() {
                var h = document.getElementById('wf-resize-handle'); if (h) h.classList.remove('dragging');
                document.body.style.cursor = ''; document.body.style.userSelect = ''; document.body.style.webkitUserSelect = '';
                document.removeEventListener('mousemove', onMove2); document.removeEventListener('mouseup', onUp2);
            };
            document.addEventListener('mousemove', onMove2); document.addEventListener('mouseup', onUp2);
        }
    });
})();
"#;
