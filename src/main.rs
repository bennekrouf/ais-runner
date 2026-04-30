mod components;
mod services;

use chrono::{Local, Utc};
use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};
use dioxus::desktop::LogicalSize;

use components::{
    log_panel::{LogLevel, LogLine, LogPanel},
    run_detail::RunDetail,
    run_dialog::RunDialog,
    toolbar::ServiceBlock,
    workflow_list::WorkflowList,
    settings_editor::SettingsEditor,
};
use services::{
    config,
    payload,
    process::{ManagedProcess, ServiceState},
    system_check,
    workflows::{self, ActionItem, RunItem, WorkflowItem, run_trigger_direct},
};

const MAIN_CSS: Asset = asset!("/assets/main.css");

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
        document::Link { rel: "stylesheet", href: MAIN_CSS }
        match screen.read().clone() {
            Screen::Welcome => rsx! {
                WelcomeScreen {
                    recent: app_cfg.read().recent_dirs.clone(),
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
    recent: Vec<String>,
    on_open: EventHandler<String>,
}

#[component]
fn WelcomeScreen(props: WelcomeScreenProps) -> Element {
    let on_open = props.on_open.clone();
    let on_open2 = props.on_open.clone();
    let recent = props.recent.clone();

    rsx! {
        div { id: "welcome",
            div { id: "welcome-header",
                h1 { "AIS Local Runner" }
                p { "Select your AIS platform folder to get started." }
            }

            div { id: "welcome-box",
                div { id: "welcome-pick",
                    p { "Choose the root folder of your ais_platform repo" }
                    button {
                        class: "btn-pick-folder",
                        onclick: move |_| {
                            let on_open = on_open.clone();
                            spawn(async move {
                                let picked = tokio::task::spawn_blocking(|| {
                                    config::pick_folder(None)
                                }).await.ok().flatten();
                                if let Some(dir) = picked {
                                    on_open.call(dir);
                                }
                            });
                        },
                        "📁  Browse…"
                    }
                }

                if !recent.is_empty() {
                    div { id: "recent-list",
                        h3 { "Recent" }
                        for dir in recent {
                            {
                                let dir2 = dir.clone();
                                let on_open = on_open2.clone();
                                rsx! {
                                    div {
                                        class: "recent-item",
                                        onclick: move |_| on_open.call(dir2.clone()),
                                        span { class: "recent-icon", "🗂" }
                                        span { class: "recent-path", title: "{dir}", "{dir}" }
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

    // ── Service states ─────────────────────────────────────────────────────
    let azurite_state = use_signal(|| ServiceState::Stopped);
    let func_state    = use_signal(|| ServiceState::Stopped);
    let azurite_proc  = use_signal(|| std::sync::Arc::new(ManagedProcess::new()));
    let func_proc     = use_signal(|| std::sync::Arc::new(ManagedProcess::new()));

    // ── Data ───────────────────────────────────────────────────────────────
    let workflows   = use_signal(|| Vec::<WorkflowItem>::new());
    let selected_wf = use_signal(|| Option::<String>::None);
    let mut source_text = use_signal(|| String::new());
    let mut runs    = use_signal(|| Vec::<RunItem>::new());
    let mut actions = use_signal(|| Vec::<ActionItem>::new());
    let is_live      = use_signal(|| false);
    let current_view = use_signal(|| "Workflows".to_string());
    let mut is_light = use_signal(|| false);
    // (wf_name, trigger_name, trigger_type, suggested_payload)
    let mut run_dialog  = use_signal(|| Option::<(String, String, String, String)>::None);
    let active_tab       = use_signal(|| "Source".to_string());
    // workflows that have at least one run in history (survives workflow selection changes)
    let mut traced_wfs   = use_signal(|| HashSet::<String>::new());
    // workflows the user explicitly cleared → timestamp of the clear (ISO 8601)
    // runs with start_time before this timestamp are hidden even after re-fetch
    let mut cleared_wfs  = use_signal(|| HashMap::<String, String>::new());

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

    // ── Log ────────────────────────────────────────────────────────────────
    let log_lines = use_signal(|| Vec::<LogLine>::new());

    let push_log = {
        let mut log_lines = log_lines.clone();
        move |msg: String, level: LogLevel| {
            log_lines.write().push(LogLine { time: now(), msg, level });
        }
    };

    // ── Azurite ────────────────────────────────────────────────────────────
    let on_azurite_start = {
        let mut state = azurite_state.clone();
        let proc = azurite_proc.clone();
        let mut push = push_log.clone();
        move |_| {
            state.set(ServiceState::Starting);
            push("$ azurite --location /tmp/azurite --debug /tmp/azurite/debug.log".to_string(), LogLevel::Info);
            match proc.read().start("azurite",
                &["--location", "/tmp/azurite", "--debug", "/tmp/azurite/debug.log"], None) {
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
            state.set(ServiceState::Starting);
            push(format!("$ cd {} && func start", dir2), LogLevel::Info);
            match proc.read().start("func", &["start"], Some(&dir2)) {
                Ok(_) => {
                    state.set(ServiceState::Running);
                    push("func start launched — waiting for workflows…".to_string(), LogLevel::Ok);
                    let mut wfs    = wfs.clone();
                    let mut push2  = push.clone();
                    let mut traced = traced_wfs.clone();
                    let cleared    = cleared_wfs.clone();
                    spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                        match workflows::list_workflows().await {
                            Ok(list) => {
                                push2(format!("Loaded {} workflow(s)", list.len()), LogLevel::Ok);
                                let names: Vec<String> = list.iter().map(|w| w.name.clone()).collect();
                                wfs.set(list);
                                sweep_run_history(names, &mut traced, &cleared).await;
                            }
                            Err(e) => push2(format!("Workflow list error: {}", e), LogLevel::Warn),
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

    // ── Load workflows ─────────────────────────────────────────────────────
    let on_load_workflows = {
        let wfs  = workflows.clone();
        let push = push_log.clone();
        let traced  = traced_wfs.clone();
        let cleared = cleared_wfs.clone();
        move |_| {
            let mut wfs    = wfs.clone();
            let mut push   = push.clone();
            let mut traced = traced.clone();
            let cleared    = cleared.clone();
            spawn(async move {
                push("Fetching workflows from localhost:7071…".to_string(), LogLevel::Info);
                match workflows::list_workflows().await {
                    Ok(list) => {
                        push(format!("Loaded {} workflow(s)", list.len()), LogLevel::Ok);
                        let names: Vec<String> = list.iter().map(|w| w.name.clone()).collect();
                        wfs.set(list);
                        sweep_run_history(names, &mut traced, &cleared).await;
                    }
                    Err(e) => push(format!("Cannot reach func start: {}", e), LogLevel::Error),
                }
            });
        }
    };

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
            let src_path = std::path::Path::new(&dir_src).join(&name).join("workflow.json");
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
        let dir = dir.clone();
        move |(name, trigger_name, trigger_type): (String, String, String)| {
            let suggested = payload::suggest_payload(&dir, &name);
            run_dialog.set(Some((name, trigger_name, trigger_type, suggested)));
        }
    };
    // Same logic invoked from the detail panel's Trigger button (no workflow args needed)
    let on_trigger_from_detail = {
        let dir = dir.clone();
        let wfs = workflows.clone();
        let sel = selected_wf.clone();
        move |_| {
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
        let runs       = runs.clone();
        let actions    = actions.clone();
        let push       = push_log.clone();
        let mut live   = is_live.clone();
        let mut tab    = active_tab.clone();
        let mut traced = traced_wfs.clone();
        let mut cleared = cleared_wfs.clone();
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
            let cleared_at  = Some(trigger_ts);
            push(format!("Triggering: {}", wf), LogLevel::Info);
            // only Request/Http triggers support listCallbackUrl
            let is_recurrence = !matches!(trigger_type.to_lowercase().as_str(), "request" | "http");
            live.set(true);
            spawn(async move {
                // ── Fire the trigger ──────────────────────────────────────
                if is_recurrence {
                    match run_trigger_direct(&wf, &trigger_name, &body).await {
                        Ok(_) => push(format!("Run triggered ({})", trigger_type), LogLevel::Ok),
                        Err(e) => { push(format!("Trigger error: {}", e), LogLevel::Error); live.set(false); return; }
                    }
                } else {
                    match workflows::get_callback_url(&wf, &trigger_name).await {
                        Ok(url) => {
                            push(format!("$ curl -X POST \"{}\"", url), LogLevel::Info);
                            match workflows::trigger_workflow(&url, &body).await {
                                Ok(run_id) => push(format!("Run started: {}", run_id), LogLevel::Ok),
                                Err(e) => { push(format!("Trigger error: {}", e), LogLevel::Error); live.set(false); return; }
                            }
                        }
                        Err(e) => { push(format!("Callback URL error: {}", e), LogLevel::Error); live.set(false); return; }
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
                live.set(false);
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
                    if let Ok(r) = workflows::list_runs(&wf).await {
                        if let Some(latest) = r.first() {
                            if let Ok(a) = workflows::list_actions(&wf, &latest.name).await {
                                actions.set(a);
                            }
                        }
                        runs.set(r);
                    } else {
                        push("Refresh failed".to_string(), LogLevel::Warn);
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
                // ── Log panel (horizontal drag) ────────────────────────
                var lh = document.getElementById('log-resize-handle');
                var lp = document.getElementById('log-panel');
                if (lh && lp) {
                    var startY, startH;
                    lh.addEventListener('mousedown', function(e) {
                        e.preventDefault();
                        startY = e.clientY;
                        startH = lp.getBoundingClientRect().height;
                        lh.classList.add('dragging');
                        document.body.style.cursor              = 'ns-resize';
                        document.body.style.userSelect          = 'none';
                        document.body.style.webkitUserSelect    = 'none';
                        function onMove(e) {
                            var newH = Math.max(80, Math.min(600, startH + (startY - e.clientY)));
                            lp.style.height = newH + 'px';
                        }
                        function onUp() {
                            lh.classList.remove('dragging');
                            document.body.style.cursor              = '';
                            document.body.style.userSelect          = '';
                            document.body.style.webkitUserSelect    = '';
                            document.removeEventListener('mousemove', onMove);
                            document.removeEventListener('mouseup', onUp);
                        }
                        document.addEventListener('mousemove', onMove);
                        document.addEventListener('mouseup', onUp);
                    });
                }

                // ── Workflow panel (vertical drag) ─────────────────────
                var wh = document.getElementById('wf-resize-handle');
                var wp = document.getElementById('workflows');
                if (wh && wp) {
                    var startX, startW;
                    wh.addEventListener('mousedown', function(e) {
                        e.preventDefault();
                        startX = e.clientX;
                        startW = wp.getBoundingClientRect().width;
                        wh.classList.add('dragging');
                        document.body.style.cursor              = 'ew-resize';
                        document.body.style.userSelect          = 'none';
                        document.body.style.webkitUserSelect    = 'none';
                        function onMove(e) {
                            var newW = Math.max(160, Math.min(520, startW + (e.clientX - startX)));
                            wp.style.width = newW + 'px';
                        }
                        function onUp() {
                            wh.classList.remove('dragging');
                            document.body.style.cursor              = '';
                            document.body.style.userSelect          = '';
                            document.body.style.webkitUserSelect    = '';
                            document.removeEventListener('mousemove', onMove);
                            document.removeEventListener('mouseup', onUp);
                        }
                        document.addEventListener('mousemove', onMove);
                        document.addEventListener('mouseup', onUp);
                    });
                }
            })();
        "#);
    });

    let dir_label = dir.clone();

    rsx! {
        div { id: "app",

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
                button {
                    class: "btn btn-run btn-small",
                    style: "margin-left:auto",
                    onclick: on_load_workflows,
                    "⟳ Load Workflows"
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
                        on_select: on_select_wf,
                        on_run: on_open_dialog,
                    }
                    div { id: "wf-resize-handle" }
                    RunDetail {
                        workflow: selected_wf.read().clone(),
                        source_text: source_text.read().clone(),
                        runs: runs.read().clone(),
                        actions: actions.read().clone(),
                        is_live: *is_live.read(),
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
        }
    }
}
