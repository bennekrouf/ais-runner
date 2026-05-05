mod components;
mod services;

use chrono::{Local, Utc};
use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};
use dioxus::desktop::LogicalSize;
use dark_light;

use components::{
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
    config,
    payload,
    process::{ManagedProcess, ServiceState},
    system_check,
    sql_check,
    sb_check,
    env_mode::{self, EnvMode},
    setup_manager,
    workflows::{self, ActionItem, RunItem, WorkflowItem, run_trigger_direct},
};

const MAIN_CSS: &str = include_str!("../assets/main.css");

fn now() -> String {
    Local::now().format("%H:%M:%S").to_string()
}

fn main() {
    tracing_subscriber::fmt::init();
    let cfg = dioxus::desktop::Config::new()
        .with_window(
            dioxus::desktop::WindowBuilder::new()
                .with_title("AIS Local Runner")
                .with_inner_size(LogicalSize::new(1280.0, 820.0))
                .with_maximized(true)
                .with_always_on_top(false),
        );
    LaunchBuilder::desktop().with_cfg(cfg).launch(App);
}

// ── Screens ────────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq)]
enum Screen {
    Welcome,
    Main(String), // selected logic_apps_dir
}

// ── Root ───────────────────────────────────────────────────────────────────

#[component]
fn App() -> Element {
    let saved = config::load();
    let initial_screen = Screen::Welcome;

    let screen    = use_signal(|| initial_screen);
    let app_cfg   = use_signal(|| saved);

    let on_open = {
        let mut screen  = screen.clone();
        let mut app_cfg = app_cfg.clone();
        move |dir: String| {
            // persist
            let mut cfg = app_cfg.read().clone();
            cfg.push_dir(dir.clone());
            config::save(&cfg);
            app_cfg.set(cfg);
            screen.set(Screen::Main(dir));
        }
    };

    let on_back = {
        let mut screen = screen.clone();
        move |_| screen.set(Screen::Welcome)
    };

    rsx! {
        document::Style { "{MAIN_CSS}" }
        match screen.read().clone() {
            Screen::Welcome => rsx! {
                WelcomeScreen {
                    app_cfg: app_cfg,
                    on_open: on_open,
                }
            },
            Screen::Main(dir) => rsx! {
                MainScreen {
                    logic_apps_dir: dir,
                    on_back: on_back,
                }
            },
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════
// WELCOME SCREEN
// ══════════════════════════════════════════════════════════════════════════

#[derive(Props, Clone, PartialEq)]
struct WelcomeScreenProps {
    app_cfg: Signal<config::AppConfig>,
    on_open: EventHandler<String>,
}

#[component]
fn WelcomeScreen(props: WelcomeScreenProps) -> Element {
    let app_cfg = props.app_cfg;
    let on_open = props.on_open.clone();
    let recent = app_cfg.read().recent_dirs.clone();
    
    // Onboarding state for a newly picked folder
    let mut picked_dir: Signal<Option<String>> = use_signal(|| None);
    let mut rg_list:    Signal<Vec<crate::services::azure_sync::LogicAppSite>> = use_signal(Vec::new);
    let mut fetching:   Signal<bool> = use_signal(|| false);
    let mut error:      Signal<Option<String>> = use_signal(|| None);

    rsx! {
        div { id: "welcome",
            div { id: "welcome-header",
                h1 { "AIS Local Runner" }
                p { "Select your AIS platform folder to get started." }
            }

            div { id: "welcome-box",
                if let Some(dir) = picked_dir.read().clone() {
                    // ── ONBOARDING: PICK RESOURCE GROUP ──────────────────────
                    div { id: "onboarding",
                        h3 { "🔗 Link Project to Azure" }
                        p { class: "onboarding-path", "{dir}" }
                        p { class: "onboarding-hint", "Select the Logic App site (Resource Group) for this customer repo:" }
                        
                        if *fetching.read() {
                            div { class: "onboarding-loading", "Fetching sites from Azure..." }
                        } else if let Some(err) = error.read().clone() {
                            div { class: "onboarding-error", 
                                "{err}"
                                button { 
                                    class: "btn btn-small", 
                                    style: "margin-left: 10px",
                                    onclick: move |_| {
                                        error.set(None);
                                        fetching.set(true);
                                        spawn(async move {
                                            // Trigger re-login
                                            crate::services::azure_cli::launch_az_login(None);
                                        });
                                    },
                                    "🔐 Login"
                                }
                            }
                        } else {
                            div { class: "onboarding-list",
                                for site in rg_list.read().clone() {
                                    div { 
                                        class: "onboarding-item",
                                        onclick: {
                                            let site = site.clone();
                                            let dir = dir.clone();
                                            let mut app_cfg = app_cfg.clone();
                                            let on_open = on_open.clone();
                                            move |_| {
                                                let mut cfg = app_cfg.read().clone();
                                                cfg.workspace_links.insert(dir.clone(), config::WorkspaceLink {
                                                    subscription_id: site.subscription.clone(),
                                                    resource_group:  site.resource_group.clone(),
                                                    logic_app_name:  Some(site.name.clone()),
                                                    sb_namespace:    None,
                                                });
                                                config::save(&cfg);
                                                app_cfg.set(cfg);
                                                on_open.call(dir.clone());
                                            }
                                        },
                                        span { class: "onboarding-item-name", "{site.name}" }
                                        span { class: "onboarding-item-rg", "{site.resource_group}" }
                                    }
                                }
                            }
                        }
                        
                        div { class: "onboarding-actions",
                            button { 
                                class: "btn-back", 
                                onclick: move |_| picked_dir.set(None),
                                "← Back"
                            }
                        }
                    }
                } else {
                    // ── DEFAULT: FOLDER PICKER & RECENT ──────────────────────
                    div { id: "welcome-pick",
                        p { "Choose the root folder of your ais_platform repo" }
                        button {
                            class: "btn-pick-folder",
                            onclick: move |_| {
                                spawn(async move {
                                    let picked = tokio::task::spawn_blocking(|| {
                                        config::pick_folder(None)
                                    }).await.ok().flatten();
                                    
                                    if let Some(dir) = picked {
                                        // Check if already linked
                                        let cfg = config::load();
                                        if cfg.workspace_links.contains_key(&dir) {
                                            on_open.call(dir);
                                        } else {
                                            // Start onboarding
                                            picked_dir.set(Some(dir));
                                            fetching.set(true);
                                            let res = tokio::task::spawn_blocking(|| {
                                                crate::services::azure_sync::list_logic_app_sites(None)
                                            }).await.unwrap_or(Err(crate::services::azure_cli::AzError::Other("Task failed".into())));
                                            
                                            fetching.set(false);
                                            match res {
                                                Ok(list) => rg_list.set(list),
                                                Err(e) => error.set(Some(format!("Azure error: {:?}", e))),
                                            }
                                        }
                                    }
                                });
                            },
                            "📁  Browse…"
                        }
                    }

                    if !recent.is_empty() {
                        div { id: "recent-list",
                            h3 { "Recent Projects" }
                            for dir in recent {
                                {
                                    let dir2 = dir.clone();
                                    let on_open = on_open.clone();
                                    let link = app_cfg.read().workspace_links.get(&dir).cloned();
                                    let rg_name = link.as_ref().and_then(|l| l.logic_app_name.clone())
                                        .or_else(|| link.as_ref().map(|l| l.resource_group.clone()))
                                        .unwrap_or_else(|| "Not linked".to_string());
                                    
                                    rsx! {
                                        div {
                                            class: "recent-item",
                                            onclick: move |_| on_open.call(dir2.clone()),
                                            div { style: "display:flex; flex-direction:column",
                                                span { class: "recent-path", title: "{dir}", "{dir}" }
                                                span { class: "recent-rg", "{rg_name}" }
                                            }
                                            span { class: "recent-arrow", "›" }
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

/// Keep only runs whose start_time is after `cleared_at` (RFC3339 lexicographic order works).
fn filter_cleared(runs: Vec<services::workflows::RunItem>, cleared_at: Option<&str>) -> Vec<services::workflows::RunItem> {
    let Some(ts) = cleared_at else { return runs };
    runs.into_iter()
        .filter(|r| r.properties.start_time.as_deref().map(|s| s > ts).unwrap_or(false))
        .collect()
}

/// Background sweep: checks every workflow for existing runs and populates traced_wfs.
/// Skips workflows the user explicitly cleared this session.
async fn sweep_run_history(
    names: Vec<String>,
    traced: &mut Signal<HashSet<String>>,
    cleared: &Signal<HashMap<String, String>>,
) {
    for name in names {
        if workflows::check_has_runs(&name).await {
            // Only mark as traced if there are runs newer than any clear timestamp
            let cleared_at = cleared.read().get(&name).cloned();
            if cleared_at.is_none() {
                traced.write().insert(name);
            }
            // If there's a clear timestamp we'd need a full fetch to know if newer runs exist;
            // leave it to on_select_wf to decide — don't mark traced here.
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════
// MAIN SCREEN
// ══════════════════════════════════════════════════════════════════════════

#[derive(Props, Clone, PartialEq)]
struct MainScreenProps {
    logic_apps_dir: String,
    on_back: EventHandler<()>,
}

#[component]
fn MainScreen(props: MainScreenProps) -> Element {
    let dir = props.logic_apps_dir.clone();
    let cfg = use_signal(config::load);
    let workspace_link = cfg.read().get_link(&dir).cloned();

    // ── Service states ─────────────────────────────────────────────────────
    let azurite_state    = use_signal(|| ServiceState::Stopped);
    let func_state       = use_signal(|| ServiceState::Stopped);
    let java_func_state  = use_signal(|| ServiceState::Stopped);
    let azurite_proc     = use_signal(|| std::sync::Arc::new(ManagedProcess::new()));
    let func_proc        = use_signal(|| std::sync::Arc::new(ManagedProcess::new()));
    let java_func_proc   = use_signal(|| std::sync::Arc::new(ManagedProcess::new()));

    // Derive function_apps dir: sibling of logic_apps_dir
    let func_apps_dir = std::path::Path::new(&dir)
        .parent()
        .map(|p| p.join("function_apps").to_string_lossy().to_string())
        .unwrap_or_else(|| format!("{}/function_apps", dir));

    // ── Data ───────────────────────────────────────────────────────────────
    let workflows   = use_signal(|| Vec::<WorkflowItem>::new());
    let selected_wf = use_signal(|| Option::<String>::None);
    let mut source_text = use_signal(|| String::new());
    let mut runs    = use_signal(|| Vec::<RunItem>::new());
    let mut actions = use_signal(|| Vec::<ActionItem>::new());
    let running_wfs = use_signal(|| HashSet::<String>::new());
    let current_view = use_signal(|| "Workflows".to_string());
    let system_light = dark_light::detect() != dark_light::Mode::Dark;
    let mut is_light = use_signal(|| system_light);

    // ── Log ────────────────────────────────────────────────────────────────
    let log_lines = use_signal(|| Vec::<LogLine>::new());

    let push_log = {
        let mut log_lines = log_lines.clone();
        move |msg: String, level: LogLevel| {
            log_lines.write().push(LogLine { time: now(), msg, level });
        }
    };

    // ── Setup / Onboarding ─────────────────────────────────────────────────
    let dir_for_setup = dir.clone();
    let setup_status = use_signal(move || setup_manager::check_setup(&dir_for_setup));
    let setup_updates: Signal<HashMap<String, String>> = use_signal(HashMap::new);

    let on_initialize = {
        let dir = dir.clone();
        let mut status = setup_status.clone();
        let push = push_log.clone();
        move |_| {
            let d = dir.clone();
            let d2 = dir.clone();
            let mut push = push.clone();
            spawn(async move {
                match tokio::task::spawn_blocking(move || setup_manager::initialize_from_template(&d)).await.unwrap() {
                    Ok(_) => {
                        push("Settings initialized from template.".to_string(), LogLevel::Ok);
                        status.set(setup_manager::check_setup(&d2));
                    }
                    Err(e) => push(format!("Failed to initialize: {}", e), LogLevel::Error),
                }
            });
        }
    };

    let on_initialize_default = {
        let dir = dir.clone();
        let mut status = setup_status.clone();
        let push = push_log.clone();
        let link = workspace_link.clone();
        move |_| {
            let d = dir.clone();
            let d2 = dir.clone();
            let d3 = dir.clone();
            let mut push = push.clone();
            let link = link.clone();
            spawn(async move {
                // 1. Create files
                match tokio::task::spawn_blocking(move || setup_manager::initialize_default(&d)).await.unwrap() {
                    Ok(_) => {
                        push("Default local.settings.json created.".to_string(), LogLevel::Ok);
                        
                        // 2. Auto-detect if linked
                        if let Some(l) = link {
                            push(format!("Auto-detecting resources in {}...", l.resource_group), LogLevel::Info);
                            match tokio::task::spawn_blocking(move || {
                                setup_manager::auto_detect_resources(&d3, Some(&l.subscription_id), &l.resource_group)
                            }).await.unwrap() {
                                Ok(msg) => push(msg, LogLevel::Ok),
                                Err(e) => push(format!("Auto-detect failed: {}", e), LogLevel::Error),
                            }
                        }

                        status.set(setup_manager::check_setup(&d2));
                    }
                    Err(e) => push(format!("Failed to create default settings: {}", e), LogLevel::Error),
                }
            });
        }
    };

    let on_auto_detect = {
        let dir = dir.clone();
        let mut status = setup_status.clone();
        let push = push_log.clone();
        let link = workspace_link.clone();
        move |_| {
            if let Some(l) = &link {
                let d = dir.clone();
                let d2 = dir.clone();
                let rg = l.resource_group.clone();
                let sub_id = l.subscription_id.clone();
                let mut push = push.clone();
                spawn(async move {
                    push(format!("Auto-detecting resources in {}...", rg), LogLevel::Info);
                    match tokio::task::spawn_blocking(move || {
                        setup_manager::auto_detect_resources(&d, Some(&sub_id), &rg)
                    }).await.unwrap() {
                        Ok(msg) => push(msg, LogLevel::Ok),
                        Err(e) => push(format!("Auto-detect failed: {}", e), LogLevel::Error),
                    }
                    status.set(setup_manager::check_setup(&d2));
                });
            }
        }
    };

    let _on_apply_setup = {
        let dir = dir.clone();
        let mut status = setup_status.clone();
        let updates = setup_updates.clone();
        move |_: Event<MouseData>| {
            let d = dir.clone();
            let d2 = dir.clone();
            let u = updates.read().clone();
            spawn(async move {
                if let Ok(_) = tokio::task::spawn_blocking(move || setup_manager::apply_settings(&d, u)).await.unwrap() {
                    status.set(setup_manager::check_setup(&d2));
                }
            });
        }
    };

    // ── Environment mode (Local ↔ Azure) ───────────────────────────────────
    let dir_for_env = dir.clone();
    let mut current_env  = use_signal(move || env_mode::detect_mode(&dir_for_env));

    // Apply body class on first render so no flash of wrong theme.
    use_effect(move || {
        let cls = if *is_light.read() { "light" } else { "" };
        document::eval(&format!("document.body.className = '{}';", cls));
    });
    // (wf_name, trigger_name, trigger_type, suggested_payload)
    let mut run_dialog  = use_signal(|| Option::<(String, String, String, String)>::None);
    let mut active_tab   = use_signal(|| "Source".to_string());
    // workflows that have at least one run in history (survives workflow selection changes)
    let mut traced_wfs   = use_signal(|| HashSet::<String>::new());
    // workflows the user explicitly cleared → timestamp of the clear (ISO 8601)
    // runs with start_time before this timestamp are hidden even after re-fetch
    let mut cleared_wfs  = use_signal(|| HashMap::<String, String>::new());
    // SQL-dependent workflows and DB panel state
    let mut sql_wfs      = use_signal(|| HashSet::<String>::new());
    let mut sql_conns    = use_signal(|| Vec::<sql_check::SqlConnection>::new());
    let mut db_panel_open    = use_signal(|| false);
    let mut azure_panel_open = use_signal(|| false);

    // SFTP connections
    let mut sftp_conns = use_signal(|| Vec::<services::sftp_check::SftpConnection>::new());

    // Blob connections (from connections.json)
    let mut blob_conns = use_signal(|| Vec::<services::blob_check::BlobConnection>::new());

    // Cosmos DB connections (from connections.json)
    let mut cosmos_conns = use_signal(|| Vec::<services::cosmos_check::CosmosConnection>::new());

    // AzureWebJobsStorage current value
    let mut webjobs_storage = use_signal(|| String::new());

    // Service Bus panel state
    let mut sb_namespace      = use_signal(|| {
        config::load().get_link(&dir)
            .and_then(|l| l.sb_namespace.clone())
            .unwrap_or_default()
    });
    let mut sb_namespace_key  = use_signal(|| Option::<String>::None);
    // (setting_key, current_value)
    let mut sb_conn_str       = use_signal(|| Option::<(String, String)>::None);
    let mut sb_queues         = use_signal(|| Vec::<sb_check::SbQueueInfo>::new());

    // ── Tool check ─────────────────────────────────────────────────────────
    let mut tool_statuses = use_signal(|| Vec::<system_check::ToolStatus>::new());
    let mut tools_dismissed = use_signal(|| false);
    use_effect(move || {
        spawn(async move {
            let statuses = tokio::task::spawn_blocking(system_check::check_tools)
                .await
                .unwrap_or_default();
            tool_statuses.set(statuses);
        });
    });

    // ── Azure login status ─────────────────────────────────────────────────
    // None = still checking; Some(Ok(name)) = logged in; Some(Err(_)) = not logged in
    let mut az_status: Signal<Option<Result<String, services::azure_cli::AzError>>> =
        use_signal(|| None);
    use_effect(move || {
        spawn(async move {
            let result = tokio::task::spawn_blocking(services::azure_cli::check_login)
                .await
                .unwrap_or(Err(services::azure_cli::AzError::Other("check failed".into())));
            az_status.set(Some(result));
        });
    });

    // ── Auto-bootstrap workspace link from local.settings.json ───────────
    use_effect({
        let dir2 = dir.clone();
        let mut cfg2 = cfg.clone();
        move || {
            if cfg2.read().get_link(&dir2).is_none() {
                if let Some(link) = services::settings_file::try_bootstrap_link(&dir2) {
                    let mut c = cfg2.write();
                    c.set_link(dir2.clone(), link);
                    config::save(&c);
                }
            }
        }
    });

    // ── SQL + SB detection on mount ────────────────────────────────────────
    use_effect({
        let dir2 = dir.clone();
        move || {
            let dir3 = dir2.clone();
            let dir4 = dir2.clone();
            let dir5 = dir2.clone();
            let dir6 = dir2.clone();
            let dir7 = dir2.clone();
            let dir8 = dir2.clone();
            let dir9 = dir2.clone();
            let dira = dir2.clone();
            spawn(async move {
                let (wfs, conns, (sb_ns, sb_qs), sb_key, sb_cs_key, blobs, cosmos, wjs) =
                    tokio::task::spawn_blocking(move || {
                        let wfs      = sql_check::detect_sql_workflows(&dir3);
                        let conns    = sql_check::load_sql_connections(&dir4);
                        let sb       = sb_check::detect_sb_queues(&dir5);
                        let sb_key   = sb_check::detect_sb_namespace_key(&dir6);
                        let sb_cs_key = sb_check::detect_sb_conn_str_key(&dir7);
                        let blobs    = services::blob_check::detect_blob_connections(&dir8);
                        let cosmos   = services::cosmos_check::detect_cosmos_connections(&dir9);
                        let wjs      = services::blob_check::read_webjobs_storage(&dira);
                        (wfs, conns, sb, sb_key, sb_cs_key, blobs, cosmos, wjs)
                    }).await.unwrap_or_default();
                sql_wfs.set(wfs);
                sql_conns.set(conns);
                sb_namespace.set(sb_ns);
                sb_queues.set(sb_qs);
                sb_namespace_key.set(sb_key);
                sb_conn_str.set(sb_cs_key);
                blob_conns.set(blobs);
                cosmos_conns.set(cosmos);
                webjobs_storage.set(wjs);
            });
        }
    });




    // ── Setup / Onboarding ─────────────────────────────────────────────────

    // ── Azurite ────────────────────────────────────────────────────────────
    let on_azurite_start = {
        let mut state = azurite_state.clone();
        let proc = azurite_proc.clone();
        let mut push = push_log.clone();
        move |_| {
            state.set(ServiceState::Starting);
            let az_bin = services::runtime_manager::resolve_tool("azurite");
            push(format!("$ {} --location /tmp/azurite --debug /tmp/azurite/debug.log --skipApiVersionCheck", az_bin), LogLevel::Info);
            match proc.read().start(&az_bin,
                &["--location", "/tmp/azurite", "--debug", "/tmp/azurite/debug.log", "--skipApiVersionCheck"], None) {
                Ok(_) => { state.set(ServiceState::Running); push("Azurite started on ports 10000/10001/10002".to_string(), LogLevel::Ok); }
                Err(e) => { state.set(ServiceState::Stopped); push(format!("Azurite error: {}", e), LogLevel::Error); }
            }
        }
    };

    let on_azurite_stop = {
        let mut state = azurite_state.clone();
        let proc = azurite_proc.clone();
        let mut push = push_log.clone();
        move |_| {
            match proc.read().stop() {
                Ok(_) => { state.set(ServiceState::Stopped); push("Azurite stopped.".to_string(), LogLevel::Warn); }
                Err(e) => push(format!("Error: {}", e), LogLevel::Error),
            }
        }
    };

    // ── func start ─────────────────────────────────────────────────────────
    let on_func_start = {
        let mut state = func_state.clone();
        let proc      = func_proc.clone();
        let wfs   = workflows.clone();
        let mut push  = push_log.clone();
        let dir2      = dir.clone();
        move |_| {
            // Azurite must be running first — Logic Apps Standard uses it for the
            // blob-based secret repository (AzureWebJobsStorage=UseDevelopmentStorage=true).
            if !matches!(*azurite_state.read(), ServiceState::Running) {
                push(
                    "⚠ Start Azurite first — func start needs blob storage for its secret repository.".to_string(),
                    LogLevel::Warn,
                );
                return;
            }
            state.set(ServiceState::Starting);

            // Detect correct working directory for func (it needs host.json)
            let mut func_cwd = dir2.clone();
            let p = std::path::Path::new(&dir2);
            if !p.join("host.json").exists() {
                if p.join("logic_apps").join("host.json").exists() {
                    func_cwd = p.join("logic_apps").to_str().unwrap_or(&dir2).to_string();
                } else if p.join("logic-apps").join("host.json").exists() {
                    func_cwd = p.join("logic-apps").to_str().unwrap_or(&dir2).to_string();
                }
            }

            // Fail fast if local.settings.json is absent — func start will hang without it
            if !std::path::Path::new(&func_cwd).join("local.settings.json").exists() {
                push(
                    format!("⚠ local.settings.json not found in {} — func start requires it. Create the file first.", func_cwd),
                    LogLevel::Warn,
                );
                state.set(ServiceState::Stopped);
                return;
            }

            push(format!("$ cd {} && func start", func_cwd), LogLevel::Info);
            // Kill any stale process on port 7071 before starting
            let _ = std::process::Command::new("/bin/sh")
                .args(["-c", "lsof -ti :7071 | xargs kill -9 2>/dev/null; true"])
                .env("PATH", services::process::rich_path())
                .output();
            match proc.read().start("func", &["start"], Some(&func_cwd)) {
                Ok((stdout, stderr)) => {
                    state.set(ServiceState::Running);
                    push("func start launched — waiting for workflows…".to_string(), LogLevel::Ok);
                    let mut wfs    = wfs.clone();
                    let mut push2  = push.clone();
                    let mut push3  = push.clone();
                    let mut traced = traced_wfs.clone();
                    let cleared    = cleared_wfs.clone();
                    let la_dir     = dir2.clone();

                    // Stream func start output into the log
                    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, bool)>();
                    services::process::stream_output(stdout, stderr, tx, true);
                    spawn(async move {
                        let mut function_conn_warned = false;
                        while let Some((line, is_err)) = rx.recv().await {
                            // Collapse the repeated functionConnections parse error into one note
                            if line.contains("functionConnections")
                                && line.contains("cannot be parsed")
                            {
                                if !function_conn_warned {
                                    function_conn_warned = true;
                                    push3(
                                        "⚠ functionConnections: @{appsetting(...)} interpolation in function.id is not supported locally — triggerUrl is used for invocation and is unaffected.".into(),
                                        LogLevel::Warn,
                                    );
                                }
                                continue;
                            }
                            // Suppress the redundant follow-on lines for the same error
                            if line.contains("Workflow processing failed")
                                && line.contains("functionConnections")
                            {
                                continue;
                            }
                            let level = if is_err { LogLevel::Error } else { LogLevel::Info };
                            push3(line, level);
                        }
                    });

                    spawn(async move {
                        match workflows::wait_for_workflows(120).await {
                            Ok(list) => {
                                let func_names: std::collections::HashSet<String> =
                                    list.iter().map(|w| w.name.clone()).collect();

                                // Cross-check local workflows against what func actually registered.
                                // Any local workflow absent from func's list was silently dropped —
                                // almost always because a connection failed to initialise.
                                let la_dir2 = la_dir.clone();
                                let local_names = tokio::task::spawn_blocking(move || {
                                    workflows::scan_local_workflows(&la_dir2)
                                }).await.unwrap_or_default();

                                let missing: Vec<String> = local_names.iter()
                                    .map(|w| w.name.clone())
                                    .filter(|n| !func_names.contains(n))
                                    .collect();

                                push2(
                                    format!("Loaded {} workflow(s){}", list.len(),
                                        if !missing.is_empty() {
                                            format!(" — ⚠ {} local workflow(s) not registered", missing.len())
                                        } else { String::new() }
                                    ),
                                    if !missing.is_empty() { LogLevel::Warn } else { LogLevel::Ok },
                                );

                                for name in &missing {
                                    push2(
                                        format!("⚠ '{}' not registered by func — a connection likely failed to initialise. Open Connections and check for missing or unreachable endpoints, then restart func.", name),
                                        LogLevel::Warn,
                                    );
                                }

                                // Warn on workflows that are registered but unhealthy.
                                // Being in func's list doesn't mean the workflow can run —
                                // a bad connection keeps it in an error state indefinitely.
                                for wf in list.iter().filter(|w| !w.healthy) {
                                    let detail = wf.health_error.as_deref()
                                        .unwrap_or("connection failed to initialise");
                                    push2(
                                        format!("⚠ '{}' loaded but unhealthy: {} — Open Connections and check endpoints, then restart func.", wf.name, detail),
                                        LogLevel::Warn,
                                    );
                                }

                                let names: Vec<String> = list.iter().map(|w| w.name.clone()).collect();
                                wfs.set(list);
                                sweep_run_history(names, &mut traced, &cleared).await;
                            }
                            Err(_) => {
                                push2("Host did not become ready — scanning for broken workflow.json files…".to_string(), LogLevel::Warn);
                                let broken = tokio::task::spawn_blocking(move || {
                                    workflows::scan_broken_workflows(&la_dir)
                                }).await.unwrap_or_default();
                                if broken.is_empty() {
                                    push2("No JSON errors found in workflow files. Check func start output above for errors.".to_string(), LogLevel::Warn);
                                } else {
                                    for (name, err) in broken {
                                        push2(format!("❌ {}/workflow.json — {}", name, err), LogLevel::Error);
                                    }
                                }
                            }
                        }
                    });
                }
                Err(e) => { state.set(ServiceState::Stopped); push(format!("func start error: {}", e), LogLevel::Error); }
            }
        }
    };

    let on_func_stop = {
        let mut state = func_state.clone();
        let proc = func_proc.clone();
        let mut push = push_log.clone();
        move |_| {
            match proc.read().stop() {
                Ok(_) => { state.set(ServiceState::Stopped); push("func start stopped.".to_string(), LogLevel::Warn); }
                Err(e) => push(format!("Error: {}", e), LogLevel::Error),
            }
        }
    };

    // ── Java Function App ──────────────────────────────────────────────────
    let on_java_start = {
        let mut state = java_func_state.clone();
        let proc      = java_func_proc.clone();
        let mut push  = push_log.clone();
        let dir2      = func_apps_dir.clone();
        move |_| {
            state.set(ServiceState::Starting);
            push(format!("$ cd {} && mvn azure-functions:run", dir2), LogLevel::Info);
            match proc.read().start("mvn", &["azure-functions:run"], Some(&dir2)) {
                Ok((stdout, stderr)) => {
                    state.set(ServiceState::Running);
                    push("Java Function App starting on port 7072…".to_string(), LogLevel::Ok);
                    let mut push2 = push.clone();
                    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, bool)>();
                    services::process::stream_output(stdout, stderr, tx, true);
                    spawn(async move {
                        while let Some((line, is_err)) = rx.recv().await {
                            let level = if is_err { LogLevel::Error } else { LogLevel::Info };
                            push2(line, level);
                        }
                    });
                }
                Err(e) => {
                    state.set(ServiceState::Stopped);
                    push(format!("Java Function App error: {}", e), LogLevel::Error);
                }
            }
        }
    };

    let on_java_stop = {
        let mut state = java_func_state.clone();
        let proc      = java_func_proc.clone();
        let mut push  = push_log.clone();
        move |_| {
            match proc.read().stop() {
                Ok(_) => { state.set(ServiceState::Stopped); push("Java Function App stopped.".to_string(), LogLevel::Warn); }
                Err(e) => push(format!("Error: {}", e), LogLevel::Error),
            }
        }
    };

    // ── Load workflows ─────────────────────────────────────────────────────
    // ── Select workflow ────────────────────────────────────────────────────
    let on_select_wf = {
        let mut selected = selected_wf.clone();
        let runs         = runs.clone();
        let mut actions  = actions.clone();
        let push         = push_log.clone();
        let dir_src      = dir.clone();
        let traced       = traced_wfs.clone();
        let cleared      = cleared_wfs.clone();
        move |name: String| {
            let wf = name.clone();
            selected.set(Some(name.clone()));
            actions.set(vec![]);
            // load source file synchronously — it's local disk, negligible latency
            let src_path = workflows::resolve_logic_apps_dir(&dir_src).join(&name).join("workflow.json");
            source_text.set(match std::fs::read_to_string(&src_path) {
                Ok(txt) => txt,
                Err(e)  => format!("// could not read {}: {}", src_path.display(), e),
            });
            let cleared_at  = cleared.read().get(&wf).cloned();
            let mut runs    = runs.clone();
            let mut actions = actions.clone();
            let mut push    = push.clone();
            let mut traced  = traced.clone();
            spawn(async move {
                match workflows::list_runs(&wf).await {
                    Ok(r) => {
                        let r = filter_cleared(r, cleared_at.as_deref());
                        if !r.is_empty() {
                            traced.write().insert(wf.clone());
                        } else {
                            traced.write().remove(&wf);
                        }
                        if let Some(latest) = r.first() {
                            if let Ok(a) = workflows::list_actions(&wf, &latest.name).await {
                                actions.set(a);
                            }
                        }
                        runs.set(r);
                    }
                    Err(e) => push(format!("Runs error: {}", e), LogLevel::Error),
                }
            });
        }
    };

    // ── Open run dialog ────────────────────────────────────────────────────
    let on_open_dialog = {
        let dir     = dir.clone();
        let dir_src = dir.clone();
        let mut sel = selected_wf.clone();
        let mut tab = active_tab.clone();
        let az_status = az_status.clone();
        let mut push = push_log.clone();
        move |(name, trigger_name, trigger_type): (String, String, String)| {
            if !matches!(az_status.read().as_ref(), Some(Ok(_))) {
                push("Cannot run workflow: not logged into Azure. Please click '⚠ az login' in the toolbar first.".to_string(), LogLevel::Error);
                return;
            }
            // Select the workflow and switch to Run tab so it's ready when the dialog confirms
            sel.set(Some(name.clone()));
            tab.set("Run".to_string());
            let src_path = workflows::resolve_logic_apps_dir(&dir_src).join(&name).join("workflow.json");
            source_text.set(match std::fs::read_to_string(&src_path) {
                Ok(txt) => txt,
                Err(e)  => format!("// could not read {}: {}", src_path.display(), e),
            });
            let suggested = payload::suggest_payload(&dir, &name);
            run_dialog.set(Some((name, trigger_name, trigger_type, suggested)));
        }
    };
    // Same logic invoked from the detail panel's Trigger button (no workflow args needed)
    let on_trigger_from_detail = {
        let dir = dir.clone();
        let wfs = workflows.clone();
        let sel = selected_wf.clone();
        let az_status = az_status.clone();
        let mut push = push_log.clone();
        move |_| {
            if !matches!(az_status.read().as_ref(), Some(Ok(_))) {
                push("Cannot run workflow: not logged into Azure. Please click '⚠ az login' in the toolbar first.".to_string(), LogLevel::Error);
                return;
            }
            if let Some(wf_name) = sel.read().clone() {
                if let Some(wf) = wfs.read().iter().find(|w| w.name == wf_name).cloned() {
                    let suggested = payload::suggest_payload(&dir, &wf.name);
                    run_dialog.set(Some((wf.name, wf.trigger_name, wf.trigger_type, suggested)));
                }
            }
        }
    };

    // ── Run workflow ───────────────────────────────────────────────────────
    let mut on_run_wf = {
        let runs        = runs.clone();
        let actions     = actions.clone();
        let push        = push_log.clone();
        let running     = running_wfs.clone();
        let mut tab     = active_tab.clone();
        let mut traced  = traced_wfs.clone();
        let mut cleared = cleared_wfs.clone();
        let dir_diag    = dir.clone();
        move |(name, trigger_name, trigger_type, body): (String, String, String, String)| {
            run_dialog.set(None);
            tab.set("Run".to_string());
            traced.write().insert(name.clone());
            // Record now as the trigger time so the poll filter only shows this new run
            // (and any future runs), not the pre-clear history.
            let trigger_ts = Utc::now().to_rfc3339();
            cleared.write().insert(name.clone(), trigger_ts.clone());
            let wf = name.clone();
            let mut runs    = runs.clone();
            let mut actions = actions.clone();
            let mut push    = push.clone();
            let mut running = running.clone();
            let cleared_at  = Some(trigger_ts);
            let dir_diag    = dir_diag.clone();
            push(format!("Triggering: {}", wf), LogLevel::Info);
            // only Request/Http triggers support listCallbackUrl
            let is_recurrence = !matches!(trigger_type.to_lowercase().as_str(), "request" | "http");
            running.write().insert(wf.clone());
            spawn(async move {
                let push_not_found_hints = |push: &mut dyn FnMut(String, LogLevel), dir: &str, wf: &str, hints: Vec<String>| {
                    let missing = services::connection_diag::missing_endpoints_for_workflow(dir, wf);
                    for (conn, key) in &missing {
                        push(format!("  hint: '{}' has empty '{}' in local.settings.json — set it and restart func", conn, key), LogLevel::Warn);
                    }
                    for msg in hints {
                        push(msg, LogLevel::Warn);
                    }
                };
                // ── Fire the trigger ──────────────────────────────────────
                if is_recurrence {
                    match run_trigger_direct(&wf, &trigger_name, &body).await {
                        Ok(_) => push(format!("Run triggered ({})", trigger_type), LogLevel::Ok),
                        Err(e) => {
                            let es = e.to_string();
                            push(format!("Trigger error: {}", es), LogLevel::Error);
                            if es.to_lowercase().contains("could not be found") || es.contains("WorkflowNotFound") {
                                let hints = workflows::not_found_hints(&wf).await;
                                push_not_found_hints(&mut push, &dir_diag, &wf, hints);
                            }
                            running.write().remove(&wf);
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
                                        push_not_found_hints(&mut push, &dir_diag, &wf, hints);
                                    }
                                    running.write().remove(&wf);
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            let es = e.to_string();
                            push(format!("Callback URL error: {}", es), LogLevel::Error);
                            if es.to_lowercase().contains("could not be found") || es.contains("WorkflowNotFound") {
                                let hints = workflows::not_found_hints(&wf).await;
                                push_not_found_hints(&mut push, &dir_diag, &wf, hints);
                            }
                            running.write().remove(&wf);
                            return;
                        }
                    }
                }

                // ── Poll until all actions reach a terminal state ─────────
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
                                        let ms = services::workflows::duration_ms(
                                            &act.properties.start_time,
                                            &act.properties.end_time,
                                        ).unwrap_or(0);
                                        let icon = match act.properties.status.to_lowercase().as_str() {
                                            "succeeded" => "✅", "failed" => "❌", "skipped" => "⏭", _ => "⏳",
                                        };
                                        push(format!("  {} {}  {}ms", icon, act.name, ms), LogLevel::Info);
                                    }
                                    if err > 0 {
                                        push(format!("Run complete — {} ok, {} failed", ok, err), LogLevel::Error);
                                    } else {
                                        push(format!("Run complete — {} actions in {:.1}s", ok,
                                            services::workflows::duration_ms(
                                                &a.first().and_then(|x| x.properties.start_time.clone()),
                                                &a.last().and_then(|x| x.properties.end_time.clone()),
                                            ).unwrap_or(0) as f64 / 1000.0
                                        ), LogLevel::Ok);
                                    }
                                    break;
                                }
                            }
                        }
                    }
                    if tokio::time::Instant::now() >= deadline {
                        push("Live poll timed out after 5 min".to_string(), LogLevel::Warn);
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                }
                running.write().remove(&wf);
            });
        }
    };

    // ── Refresh ────────────────────────────────────────────────────────────
    let on_refresh = {
        let selected = selected_wf.clone();
        let runs    = runs.clone();
        let actions = actions.clone();
        let push    = push_log.clone();
        move |_| {
            if let Some(wf) = selected.read().clone() {
                let mut runs    = runs.clone();
                let mut actions = actions.clone();
                let mut push    = push.clone();
                spawn(async move {
                    match workflows::list_runs(&wf).await {
                        Ok(r) => {
                            if let Some(latest) = r.first() {
                                match workflows::list_actions(&wf, &latest.name).await {
                                    Ok(a)  => actions.set(a),
                                    Err(e) => push(format!("Actions error: {}", e), LogLevel::Error),
                                }
                            }
                            runs.set(r);
                        }
                        Err(e) => push(format!("Refresh failed: {}", e), LogLevel::Error),
                    }
                });
            }
        }
    };

    let on_select_run = {
        let selected = selected_wf.clone();
        let actions = actions.clone();
        let push    = push_log.clone();
        move |run_id: String| {
            if let Some(wf) = selected.read().clone() {
                let mut actions = actions.clone();
                let mut push    = push.clone();
                spawn(async move {
                    match workflows::list_actions(&wf, &run_id).await {
                        Ok(a) => actions.set(a),
                        Err(e) => push(format!("Actions error: {}", e), LogLevel::Error),
                    }
                });
            }
        }
    };

    // ── Resize JS ──────────────────────────────────────────────────────────
    use_effect(move || {
        document::eval(r#"
            (function() {
                if (window.__ais_resize_init) return;
                window.__ais_resize_init = true;

                document.body.addEventListener('mousedown', function(e) {
                    if (e.target && e.target.id === 'log-resize-handle') {
                        e.preventDefault();
                        var lp = document.getElementById('log-panel');
                        if (!lp) return;
                        var startY = e.clientY;
                        var startH = lp.getBoundingClientRect().height;
                        e.target.classList.add('dragging');
                        document.body.style.cursor              = 'ns-resize';
                        document.body.style.userSelect          = 'none';
                        document.body.style.webkitUserSelect    = 'none';
                        function onMove(eMove) {
                            var newH = Math.max(80, Math.min(600, startH + (startY - eMove.clientY)));
                            lp.style.height = newH + 'px';
                        }
                        function onUp() {
                            var handle = document.getElementById('log-resize-handle');
                            if (handle) handle.classList.remove('dragging');
                            document.body.style.cursor              = '';
                            document.body.style.userSelect          = '';
                            document.body.style.webkitUserSelect    = '';
                            document.removeEventListener('mousemove', onMove);
                            document.removeEventListener('mouseup', onUp);
                        }
                        document.addEventListener('mousemove', onMove);
                        document.addEventListener('mouseup', onUp);
                    } else if (e.target && e.target.id === 'wf-resize-handle') {
                        e.preventDefault();
                        var wp = document.getElementById('workflows');
                        if (!wp) return;
                        var startX = e.clientX;
                        var startW = wp.getBoundingClientRect().width;
                        e.target.classList.add('dragging');
                        document.body.style.cursor              = 'ew-resize';
                        document.body.style.userSelect          = 'none';
                        document.body.style.webkitUserSelect    = 'none';
                        function onMove(eMove) {
                            var newW = Math.max(160, Math.min(520, startW + (eMove.clientX - startX)));
                            wp.style.width = newW + 'px';
                        }
                        function onUp() {
                            var handle = document.getElementById('wf-resize-handle');
                            if (handle) handle.classList.remove('dragging');
                            document.body.style.cursor              = '';
                            document.body.style.userSelect          = '';
                            document.body.style.webkitUserSelect    = '';
                            document.removeEventListener('mousemove', onMove);
                            document.removeEventListener('mouseup', onUp);
                        }
                        document.addEventListener('mousemove', onMove);
                        document.addEventListener('mouseup', onUp);
                    }
                });
            })();
        "#);
    });

    let dir_label = dir.clone();

    rsx! {
        div { id: "app",
            match setup_status.read().clone() {
                setup_manager::SetupStatus::MissingSettings => rsx! {
                    div { class: "setup-banner",
                        span { "⚠ local.settings.json is missing and no template was found." }
                        button { class: "setup-banner-btn", onclick: on_initialize_default, "Bootstrap Default Settings" }
                    }
                },
                setup_manager::SetupStatus::NeedsInitialization => rsx! {
                    div { class: "setup-banner",
                        span { "🚀 Your environment is not initialized yet. Create local.settings.json from template?" }
                        button { class: "setup-banner-btn", onclick: on_initialize, "Bootstrap Settings" }
                    }
                },
                setup_manager::SetupStatus::NeedsConfiguration(count) => rsx! {
                    div { class: "setup-banner",
                        span { "⚠ {count} settings require attention (SQL passwords, Azure endpoints)." }
                        if let Some(_) = &workspace_link {
                            button { 
                                class: "setup-banner-btn", 
                                style: "background: var(--blue); margin-right: 8px;",
                                onclick: on_auto_detect, 
                                "Auto-Detect from Azure" 
                            }
                        }
                        button { 
                            class: "setup-banner-btn", 
                            onclick: move |_| { active_tab.set("Settings".to_string()); }, 
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
                                let keys = keys.clone();
                                let dir  = dir.clone();
                                let mut setup_status = setup_status.clone();
                                move |_| {
                                    let _ = setup_manager::stub_missing_keys(&dir, &keys);
                                    setup_status.set(setup_manager::check_setup(&dir));
                                }
                            },
                            "Auto-stub Missing Keys"
                        }
                        button {
                            class: "setup-banner-btn",
                            onclick: move |_| { active_tab.set("Settings".to_string()); },
                            "Edit Manually"
                        }
                    }
                },
                _ => rsx! {}
            }

            // TOOLBAR
            div { id: "toolbar",
                button {
                    class: "btn-back",
                    onclick: move |_| props.on_back.call(()),
                    "‹ Back"
                }
                span { id: "toolbar-dir", title: "{dir_label}", "{dir_label}" }

                ServiceBlock {
                    label: "Azurite".to_string(),
                    cmd: "azurite --location /tmp/azurite".to_string(),
                    state: azurite_state.read().clone(),
                    on_start: on_azurite_start,
                    on_stop: on_azurite_stop,
                }
                ServiceBlock {
                    label: "func start".to_string(),
                    cmd: "func start".to_string(),
                    state: func_state.read().clone(),
                    on_start: on_func_start,
                    on_stop: on_func_stop,
                }
                ServiceBlock {
                    label: "Java Functions".to_string(),
                    cmd: "mvn azure-functions:run".to_string(),
                    state: java_func_state.read().clone(),
                    on_start: on_java_start,
                    on_stop: on_java_stop,
                }
                // ── Azure login indicator ──────────────────────────────
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
                                button {
                                    class: "az-action-btn",
                                    title: "Re-check login status",
                                    onclick: move |_| {
                                        az_status.set(None);
                                        spawn(async move {
                                            let result = tokio::task::spawn_blocking(services::azure_cli::check_login)
                                                .await
                                                .unwrap_or(Err(services::azure_cli::AzError::Other("check failed".into())));
                                            az_status.set(Some(result));
                                        });
                                    },
                                    "⟳"
                                }
                                button {
                                    class: "az-action-btn az-logout-btn",
                                    title: "Sign out",
                                    onclick: move |_| {
                                        services::azure_cli::logout();
                                        az_status.set(None);
                                        spawn(async move {
                                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                            az_status.set(Some(Err(services::azure_cli::AzError::NotLoggedIn)));
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
                                    class: "az-login-btn",
                                    title: "Sign in with az login",
                                    onclick: {
                                        let dir = dir.clone();
                                        move |_| {
                                        services::azure_cli::open_login("68fac18b-9e76-4cef-b2b7-2c51b521cb94");
                                        az_status.set(None);
                                        let login_dir = dir.clone();
                                        spawn(async move {
                                            for _ in 0..24 {
                                                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                                let result = tokio::task::spawn_blocking(services::azure_cli::check_login)
                                                    .await
                                                    .unwrap_or(Err(services::azure_cli::AzError::Other("check failed".into())));
                                                let done = result.is_ok();
                                                az_status.set(Some(result));
                                                if done {
                                                    // Auto-select the project subscription so the
                                                    // interactive prompt never blocks us.
                                                    let d = login_dir.clone();
                                                    let _ = tokio::task::spawn_blocking(move || {
                                                        if let Some(sub) = services::azure_sync::detect_subscription(&d) {
                                                            let _ = services::azure_cli::set_subscription(&sub);
                                                        }
                                                    }).await;
                                                    break;
                                                }
                                            }
                                        });
                                    }
                                },
                                "Sign in"
                            }
                        }
                    },
                    }
                }
                // ── Environment badge (read-only — switch lives in Connections panel) ──
                {
                    let has_settings = !matches!(*setup_status.read(), setup_manager::SetupStatus::MissingSettings);
                    if !has_settings { return rsx! {}; }
                    let mode = current_env.read().clone();
                    let (badge_class, badge_label) = match &mode {
                        EnvMode::Local   => ("env-badge local",   "🏠 Local"),
                        EnvMode::Azure   => ("env-badge azure",   "☁ Azure"),
                        EnvMode::Mixed   => ("env-badge mixed",   "⚠ Mixed"),
                        EnvMode::Unknown => ("env-badge unknown", "? Env"),
                    };
                    rsx! {
                        span {
                            class: "{badge_class}",
                            title: "Blob storage mode — open Connections to switch",
                            "{badge_label}"
                        }
                    }
                }
                {
                    let dir_sql = dir.clone();
                    rsx! {
                        button {
                            class: "btn btn-run btn-small",
                            style: "margin-left: 10px;",
                            title: "SQL & Service Bus connections — test & configure",
                            onclick: move |_| {
                                let dir5 = dir_sql.clone();
                                let dir6 = dir_sql.clone();
                                let dir7 = dir_sql.clone();
                                let dir8 = dir_sql.clone();
                                let dir9 = dir_sql.clone();
                                let dira = dir_sql.clone();
                                let dirb = dir_sql.clone();
                                let dirc = dir_sql.clone();
                                let dird = dir_sql.clone();
                                spawn(async move {
                                    let (wfs, conns, (sb_ns, sb_qs), sb_key, sb_cs_key, sftp, blobs, cosmos, wjs) =
                                        tokio::task::spawn_blocking(move || {
                                            let wfs       = sql_check::detect_sql_workflows(&dir5);
                                            let conns     = sql_check::load_sql_connections(&dir6);
                                            let sb        = sb_check::detect_sb_queues(&dir7);
                                            let sb_key    = sb_check::detect_sb_namespace_key(&dir8);
                                            let sb_cs_key = sb_check::detect_sb_conn_str_key(&dir9);
                                            let sftp      = services::sftp_check::detect_sftp_connections(&dira);
                                            let blobs     = services::blob_check::detect_blob_connections(&dirb);
                                            let cosmos    = services::cosmos_check::detect_cosmos_connections(&dirc);
                                            let wjs       = services::blob_check::read_webjobs_storage(&dird);
                                            (wfs, conns, sb, sb_key, sb_cs_key, sftp, blobs, cosmos, wjs)
                                        }).await.unwrap_or_default();
                                    sql_wfs.set(wfs);
                                    sql_conns.set(conns);
                                    sb_namespace.set(sb_ns);
                                    sb_queues.set(sb_qs);
                                    sb_namespace_key.set(sb_key);
                                    sb_conn_str.set(sb_cs_key);
                                    sftp_conns.set(sftp);
                                    blob_conns.set(blobs);
                                    cosmos_conns.set(cosmos);
                                    webjobs_storage.set(wjs);
                                });
                                db_panel_open.set(true);
                            },
                            "🔌 Connections"
                        }
                    }
                }
                button {
                    class: "btn btn-run btn-small",
                    style: "margin-left: 10px;",
                    onclick: move |_| {
                        let mut view = current_view.clone();
                        if *view.read() == "Workflows" {
                            view.set("Settings".to_string());
                        } else {
                            view.set("Workflows".to_string());
                        }
                    },
                    if *current_view.read() == "Settings" { "Workflows" } else { "⚙️ Settings" }
                }
                button {
                    class: "btn btn-run btn-small",
                    title: "Compare local workflows with Azure",
                    onclick: move |_| azure_panel_open.set(true),
                    "☁ Azure"
                }
                button {
                    class: "btn-theme",
                    title: if *is_light.read() { "Switch to dark mode" } else { "Switch to light mode" },
                    onclick: move |_| {
                        let next = !*is_light.read();
                        is_light.set(next);
                        let cls = if next { "light" } else { "" };
                        document::eval(&format!("document.body.className = '{}';", cls));
                    },
                    if *is_light.read() { "🌙" } else { "☀" }
                }
            }

            // TOOL CHECK BANNER
            {
                let missing: Vec<system_check::ToolStatus> = tool_statuses.read().iter()
                    .filter(|t| !t.available)
                    .cloned()
                    .collect();
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
                            button {
                                class: "btn-icon",
                                onclick: move |_| tools_dismissed.set(true),
                                "×"
                            }
                        }
                    }
                } else {
                    rsx! {}
                }
            }

            // MIDDLE
            div { id: "main",
                if *current_view.read() == "Settings" {
                    SettingsEditor { logic_apps_dir: dir.clone() }
                } else {
                    WorkflowList {
                        workflows: workflows.read().clone(),
                        selected: selected_wf.read().clone(),
                        traced: traced_wfs.read().clone(),
                        running: running_wfs.read().clone(),
                        sql_wfs: sql_wfs.read().clone(),
                        on_select: on_select_wf,
                        on_run: on_open_dialog,
                    }
                    div { id: "wf-resize-handle" }
                    RunDetail {
                        workflow: selected_wf.read().clone(),
                        source_text: source_text.read().clone(),
                        runs: runs.read().clone(),
                        actions: actions.read().clone(),
                        is_live: selected_wf.read().as_deref()
                            .map(|n| running_wfs.read().contains(n))
                            .unwrap_or(false),
                        health_error: selected_wf.read().as_ref().and_then(|name| {
                            workflows.read().iter().find(|w| &w.name == name).and_then(|w| w.health_error.clone())
                        }),
                        logs: {
                            let wf_name = selected_wf.read().clone();
                            let logs = log_lines.read();
                            if let Some(name) = wf_name {
                                logs.iter()
                                    .filter(|line| line.msg.contains(&name))
                                    .cloned()
                                    .collect()
                            } else {
                                vec![]
                            }
                        },
                        active_tab: active_tab,
                        on_run: on_trigger_from_detail,
                        on_refresh: on_refresh,
                        on_clear_runs: move |_| {
                            runs.write().clear();
                            actions.write().clear();
                            if let Some(wf) = selected_wf.read().clone() {
                                traced_wfs.write().remove(&wf);
                                cleared_wfs.write().insert(wf, Utc::now().to_rfc3339());
                            }
                        },
                        on_select_run: on_select_run,
                    }
                }
            }

            // RESIZE HANDLE
            div { id: "log-resize-handle" }

            // LOG
            LogPanel {
                lines: log_lines.read().clone(),
                on_clear: move |_| { let mut ll = log_lines.clone(); ll.write().clear(); },
            }

            // RUN DIALOG
            if let Some((wf_name, trigger_name, trigger_type, suggested)) = run_dialog.read().clone() {
                RunDialog {
                    workflow:     wf_name.clone(),
                    trigger_type: trigger_type.clone(),
                    payload:      suggested,
                    on_cancel:    move |_| run_dialog.set(None),
                    on_run:       move |body: String| {
                        on_run_wf((wf_name.clone(), trigger_name.clone(), trigger_type.clone(), body));
                    },
                }
            }

            // CONNECTIONS PANEL
            if *azure_panel_open.read() {
                {
                    let mut push2 = push_log.clone();
                    let wfs_reload   = workflows.clone();
                    let func_check   = func_state.clone();
                    let dir_reload   = dir.clone();
                    rsx! {
                        AzurePanel {
                            logic_apps_dir:  dir.clone(),
                            local_workflows: workflows.read().iter().map(|w| w.name.clone()).collect(),
                            on_close:  move |_| azure_panel_open.set(false),
                            on_pulled: move |name: String| {
                                push2(format!("⬇ {} pulled from Azure", name), LogLevel::Ok);
                                let mut wfs = wfs_reload.clone();
                                let is_running = matches!(*func_check.read(), ServiceState::Running);
                                let dir_c = dir_reload.clone();
                                spawn(async move {
                                    if is_running {
                                        if let Ok(list) = services::workflows::list_workflows().await {
                                            wfs.set(list);
                                        }
                                    } else {
                                        let list = tokio::task::spawn_blocking(move || {
                                            services::workflows::scan_local_workflows(&dir_c)
                                        }).await.unwrap_or_default();
                                        if !list.is_empty() {
                                            wfs.set(list);
                                        }
                                    }
                                });
                            },
                        }
                    }
                }
            }
            if *db_panel_open.read() {
                {
                let dir_env1 = dir.clone();
                let dir_env2 = dir.clone();
                rsx! { DbPanel {
                    logic_apps_dir:    dir.clone(),
                    connections:       sql_conns.read().clone(),
                    sb_namespace:      sb_namespace.read().clone(),
                    sb_namespace_key:  sb_namespace_key.read().clone(),
                    sb_conn_str:       sb_conn_str.read().clone(),
                    sb_queues:         sb_queues.read().clone(),
                    sftp_connections:  sftp_conns.read().clone(),
                    blob_connections:  blob_conns.read().clone(),
                    webjobs_storage:   webjobs_storage.read().clone(),
                    cosmos_connections: cosmos_conns.read().clone(),
                    env_mode:          current_env.read().clone(),
                    azurite_running:   *azurite_state.read() == ServiceState::Running,
                    on_close: move |_| db_panel_open.set(false),
                    on_env_changed: move |_| {
                        let d  = dir_env1.clone();
                        let d2 = dir_env2.clone();
                        spawn(async move {
                            let mode = tokio::task::spawn_blocking(move || env_mode::detect_mode(&d))
                                .await.unwrap_or(EnvMode::Unknown);
                            current_env.set(mode);
                            let blobs = tokio::task::spawn_blocking(move || {
                                services::blob_check::detect_blob_connections(&d2)
                            }).await.unwrap_or_default();
                            blob_conns.set(blobs);
                        });
                    },
                    on_saved: {
                        let mut push = push_log.clone();
                        move |msg: String| {
                        push(msg, LogLevel::Warn);
                        let dir7 = dir.clone();
                        let dir8 = dir.clone();
                        let dir9 = dir.clone();
                        let dira = dir.clone();
                        let dirb = dir.clone();
                        let dirc = dir.clone();
                        let dird = dir.clone();
                        let dire = dir.clone();
                        let dirf = dir.clone();
                        spawn(async move {
                            let (wfs, conns, (sb_ns, sb_qs), sb_key, sb_cs_key, sftp, blobs, cosmos, wjs) =
                                tokio::task::spawn_blocking(move || {
                                    let wfs       = sql_check::detect_sql_workflows(&dir7);
                                    let conns     = sql_check::load_sql_connections(&dir8);
                                    let sb        = sb_check::detect_sb_queues(&dir9);
                                    let sb_key    = sb_check::detect_sb_namespace_key(&dira);
                                    let sb_cs_key = sb_check::detect_sb_conn_str_key(&dirb);
                                    let sftp      = services::sftp_check::detect_sftp_connections(&dirc);
                                    let blobs     = services::blob_check::detect_blob_connections(&dird);
                                    let cosmos    = services::cosmos_check::detect_cosmos_connections(&dire);
                                    let wjs       = services::blob_check::read_webjobs_storage(&dirf);
                                    (wfs, conns, sb, sb_key, sb_cs_key, sftp, blobs, cosmos, wjs)
                                }).await.unwrap_or_default();
                            sql_wfs.set(wfs);
                            sql_conns.set(conns);
                            sb_namespace.set(sb_ns);
                            sb_queues.set(sb_qs);
                            sb_namespace_key.set(sb_key);
                            sb_conn_str.set(sb_cs_key);
                            sftp_conns.set(sftp);
                            blob_conns.set(blobs);
                            cosmos_conns.set(cosmos);
                            webjobs_storage.set(wjs);
                        });
                    } },
                } }
                }
            }
        }
    }
}
