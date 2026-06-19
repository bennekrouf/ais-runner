pub mod blob_tab;
pub mod cosmos_tab;
pub mod sb_tab;
pub mod sftp_tab;
pub mod sql_tab;

pub use blob_tab::BlobTab;
pub use cosmos_tab::CosmosTab;
pub use sb_tab::SbTab;
pub use sftp_tab::SftpTab;
pub use sql_tab::SqlTab;

use dioxus::prelude::*;
use std::collections::HashMap;
use crate::services::{
    azure_cli,
    azure_sync,
    blob_check::BlobConnection,
    cosmos_check::CosmosConnection,
    env_mode::EnvMode,
    sb_check::SbQueueInfo,
    sftp_check::SftpConnection,
    sql_check::SqlConnection,
    settings_file,
    maps_check,
};

#[derive(Props, Clone, PartialEq)]
pub struct DbPanelProps {
    pub logic_apps_dir:    String,
    pub connections:       Vec<SqlConnection>,
    pub sb_namespace:      String,
    /// The local.settings.json key that holds the SB FQDN (e.g. "serviceBus_fullyQualifiedNamespace")
    pub sb_namespace_key:  Option<String>,
    /// (setting_key, current_value) for the SB connection string, if detected
    pub sb_conn_str:       Option<(String, String)>,
    pub sb_queues:         Vec<SbQueueInfo>,
    pub sftp_connections:  Vec<SftpConnection>,
    pub blob_connections:  Vec<BlobConnection>,
    /// Current value of AzureWebJobsStorage from local.settings.json
    pub webjobs_storage:   String,
    pub cosmos_connections: Vec<CosmosConnection>,
    pub env_mode:          EnvMode,
    pub azurite_running:   bool,
    pub is_open:           Signal<bool>,
    /// Shared az login state — updated when any panel operation discovers the token is expired.
    pub az_status:         Signal<Option<Result<String, azure_cli::AzError>>>,
    pub on_saved:          EventHandler<String>,
    pub on_env_changed:    EventHandler<()>,
}

#[component]
pub fn DbPanel(props: DbPanelProps) -> Element {
    // ── SQL signals ──────────────────────────────────────────────────────────
    let edits: Signal<HashMap<String, String>> = use_signal(|| {
        let mut m = HashMap::new();
        for c in &props.connections {
            if let Some(k) = &c.server_key   { m.insert(k.clone(), c.resolved_server.clone()); }
            if let Some(k) = &c.db_key       { m.insert(k.clone(), c.resolved_db.clone()); }
            if let Some(k) = &c.conn_str_key { m.insert(k.clone(), c.resolved_conn_str.clone()); }
        }
        m
    });

    // Blob connection endpoint edits: appsetting_key → current value
    let blob_edits: Signal<HashMap<String, String>> = use_signal(|| {
        props.blob_connections.iter()
            .map(|c| (c.endpoint_key.clone(), c.endpoint.clone()))
            .collect()
    });

    // AzureWebJobsStorage edit
    let webjobs_edit: Signal<String> = use_signal(|| props.webjobs_storage.clone());

    // Cosmos connection edits: appsetting_key → current value
    let cosmos_edits: Signal<HashMap<String, String>> = use_signal(|| {
        let mut m = HashMap::new();
        for c in &props.cosmos_connections {
            if let Some(k) = &c.endpoint_key { m.insert(k.clone(), c.endpoint.clone()); }
            if let Some(k) = &c.key_key      { m.insert(k.clone(), c.account_key.clone()); }
        }
        m
    });

    // ── Subscription (for scoped az calls) ──────────────────────────────────
    let subscription: Signal<Option<String>> =
        use_signal(|| azure_sync::detect_subscription(&props.logic_apps_dir));

    // ── Active tab ───────────────────────────────────────────────────────────
    let mut active_tab: Signal<&'static str> = use_signal(|| "blob");

    // ── Shared status bar ────────────────────────────────────────────────────
    let mut status: Signal<Option<(String, bool)>> = use_signal(|| None);

    // Auto-dismiss non-error status messages after 3 s
    use_effect(move || {
        if let Some((_, is_err)) = status.read().clone() {
            if !is_err {
                spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    // Only clear if still showing a non-error message
                    let still_ok = matches!(status.read().clone(), Some((_, false)));
                    if still_ok {
                        status.set(None);
                    }
                });
            }
        }
    });


    let dir = props.logic_apps_dir.clone();

    // ── Save SQL + Blob + Cosmos connection strings ──────────────────────────
    let on_save = {
        let dir = dir.clone();
        move |_| {
            match settings_file::read_local_settings(&dir) {
                Err(e) => { status.set(Some((e, true))); return; }
                Ok(text) => {
                    let mut root: serde_json::Value = match serde_json::from_str(&text) {
                        Ok(v) => v,
                        Err(e) => { status.set(Some((format!("Parse error: {}", e), true))); return; }
                    };
                    if let Some(vals) = root["Values"].as_object_mut() {
                        for (k, v) in edits.read().iter() {
                            vals.insert(k.clone(), serde_json::Value::String(v.clone()));
                        }
                        for (k, v) in blob_edits.read().iter() {
                            vals.insert(k.clone(), serde_json::Value::String(v.clone()));
                        }
                        let wjs = webjobs_edit.read().clone();
                        if !wjs.is_empty() {
                            vals.insert("AzureWebJobsStorage".into(), serde_json::Value::String(wjs));
                        }
                        for (k, v) in cosmos_edits.read().iter() {
                            vals.insert(k.clone(), serde_json::Value::String(v.clone()));
                        }
                    }
                    let text = serde_json::to_string_pretty(&root).unwrap_or_default();
                    match settings_file::write_local_settings(&dir, &text) {
                        Ok(_) => {
                            status.set(Some(("Saved — restart func start to apply.".into(), false)));
                            props.on_saved.call("⚠ Settings saved — stop and restart func start to apply changes.".into());
                        }
                        Err(e) => { status.set(Some((e, true))); }
                    }
                }
            }
        }
    };

    let mut is_open = props.is_open;
    rsx! {
        div { class: "db-panel",
            // ── Header ──────────────────────────────────────────────────────
            div { class: "db-panel-header",
                span { class: "db-panel-title", "🔌 Connectors" }
                div { style: "display:flex;gap:8px;align-items:center",
                    if *active_tab.read() != "blob" || !props.blob_connections.is_empty() {
                        button { class: "btn btn-run btn-small", onclick: on_save, "💾 Save" }
                    }
                    button { class: "btn-icon", onclick: move |_| is_open.set(false), "×" }
                }
            }

            // ── Tab bar ────────────────────────────────────────────────────
            div { class: "db-tabs",
                button {
                    class: if *active_tab.read() == "blob" { "db-tab active" } else { "db-tab" },
                    onclick: move |_| active_tab.set("blob"),
                    "🗄 Blob"
                }
                button {
                    class: if *active_tab.read() == "sb" { "db-tab active" } else { "db-tab" },
                    onclick: move |_| active_tab.set("sb"),
                    "📨 Service Bus"
                }
                button {
                    class: if *active_tab.read() == "sql" { "db-tab active" } else { "db-tab" },
                    onclick: move |_| active_tab.set("sql"),
                    "🗄 SQL"
                }
                button {
                    class: if *active_tab.read() == "cosmos" { "db-tab active" } else { "db-tab" },
                    onclick: move |_| active_tab.set("cosmos"),
                    "🌌 Cosmos"
                }
                button {
                    class: if *active_tab.read() == "sftp" { "db-tab active" } else { "db-tab" },
                    onclick: move |_| active_tab.set("sftp"),
                    "📡 SFTP"
                }
                button {
                    class: if *active_tab.read() == "maps" { "db-tab active" } else { "db-tab" },
                    onclick: move |_| active_tab.set("maps"),
                    "🔄 Maps"
                }
            }

            // ── Status bar ─────────────────────────────────────────────────
            if let Some((msg, is_err)) = status.read().clone() {
                div {
                    class: if is_err { "settings-status error" } else { "settings-status ok" },
                    "{msg}"
                }
            }

            div { class: "db-panel-body",
                // SQL tab
                div {
                    class: if *active_tab.read() == "sql" { "tab-pane" } else { "tab-pane hidden" },
                    SqlTab { connections: props.connections.clone(), edits, status }
                }
                // Cosmos tab
                div {
                    class: if *active_tab.read() == "cosmos" { "tab-pane" } else { "tab-pane hidden" },
                    CosmosTab { cosmos_connections: props.cosmos_connections.clone(), cosmos_edits, status }
                }
                // SB tab
                div {
                    class: if *active_tab.read() == "sb" { "tab-pane" } else { "tab-pane hidden" },
                    SbTab { sb_queues: props.sb_queues.clone(), sb_namespace: props.sb_namespace.clone(), subscription, is_open: props.is_open, active_tab, status }
                }
                // Blob tab
                div {
                    class: if *active_tab.read() == "blob" { "tab-pane" } else { "tab-pane hidden" },
                    BlobTab { azurite_running: props.azurite_running, blob_edits, webjobs_edit, is_open: props.is_open, active_tab, status }
                }
                // SFTP tab
                div {
                    class: if *active_tab.read() == "sftp" { "tab-pane" } else { "tab-pane hidden" },
                    SftpTab { connections: props.sftp_connections.clone(), logic_apps_dir: props.logic_apps_dir.clone() }
                }
                // Maps tab
                div {
                    class: if *active_tab.read() == "maps" { "tab-pane" } else { "tab-pane hidden" },
                    MapsTab { logic_apps_dir: props.logic_apps_dir.clone() }
                }
            }
        }
    }
}

// ── Maps Tab ──────────────────────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct MapsTabProps {
    logic_apps_dir: String,
}

#[component]
fn MapsTab(props: MapsTabProps) -> Element {
    let dir  = props.logic_apps_dir.clone();
    let dir2 = dir.clone();

    let maps   = use_memo(move || maps_check::scan_maps(&dir));
    let usages = use_memo(move || maps_check::scan_workflow_map_usages(&dir2));

    // tester state: which map is expanded for testing
    let mut test_open:   Signal<Option<String>>  = use_signal(|| None);
    let mut test_input:  Signal<String>          = use_signal(|| "{}".to_string());
    let mut test_output: Signal<String>          = use_signal(String::new);
    let mut test_err:    Signal<bool>            = use_signal(|| false);
    let mut test_engine:    Signal<String> = use_signal(String::new);
    let mut installing:     Signal<bool>   = use_signal(|| false);
    let mut install_status: Signal<String> = use_signal(String::new);
    let mut install_err:    Signal<bool>   = use_signal(|| false);
    // Computed once — subprocess checks must NOT run on every render
    let has_dotnet        = use_memo(|| maps_check::dotnet_available());
    let has_dotnet_script = use_memo(move || *has_dotnet.read() && maps_check::dotnet_script_available());

    if maps.read().is_empty() {
        return rsx! {
            div { class: "empty-state", "No .liquid / .xslt files found under this directory." }
        };
    }

    rsx! {
        div { class: "db-scroll",
            div { class: "db-section",
                div { class: "db-section-title",
                    "🔄 Maps"
                    span { style: "font-size:11px; color:var(--text3); font-weight:400; margin-left:8px",
                        "{maps.read().len()} file(s)"
                    }
                    // engine status — right-aligned
                    div { style: "margin-left:auto; display:flex; align-items:center; gap:8px",
                        if *has_dotnet_script.read() {
                            span { style: "font-size:10px; color:var(--green); font-style:italic",
                                "✓ DotLiquid via dotnet-script"
                            }
                        } else if *has_dotnet.read() {
                            // dotnet found but dotnet-script missing — offer install
                            if *installing.read() {
                                span { style: "font-size:10px; color:var(--text3); font-style:italic",
                                    "Installing dotnet-script…"
                                }
                            } else if !install_status.read().is_empty() {
                                span {
                                    style: if *install_err.read() { "font-size:10px; color:var(--red)" } else { "font-size:10px; color:var(--green)" },
                                    "{install_status}"
                                }
                            } else {
                                span { style: "font-size:10px; color:var(--text3); font-style:italic",
                                    "liquid 0.26"
                                }
                                button {
                                    class: "btn btn-small",
                                    style: "font-size:10px; padding:0 7px; height:22px",
                                    title: "Install dotnet-script for exact DotLiquid compatibility",
                                    onclick: move |_| {
                                        installing.set(true);
                                        install_status.set(String::new());
                                        spawn(async move {
                                            let res = tokio::task::spawn_blocking(maps_check::install_dotnet_script)
                                                .await.unwrap_or_else(|e| Err(e.to_string()));
                                            installing.set(false);
                                            match res {
                                                Ok(()) => {
                                                    install_status.set("✓ dotnet-script installed — restart to use DotLiquid".into());
                                                    install_err.set(false);
                                                }
                                                Err(e) => {
                                                    install_status.set(format!("✗ {}", e));
                                                    install_err.set(true);
                                                }
                                            }
                                        });
                                    },
                                    "Install dotnet-script"
                                }
                            }
                        } else {
                            span { style: "font-size:10px; color:var(--text3); font-style:italic",
                                "liquid 0.26 — install .NET for DotLiquid"
                            }
                        }
                    }
                }

                for map in maps.read().clone().into_iter() {
                    {
                        let path         = map.path.clone();
                        let path_suggest = path.clone();
                        let map_name  = map.name.clone();
                        let map_name2 = map_name.clone();
                        let icon      = map.kind.icon();
                        let is_liquid = matches!(map.kind, maps_check::MapKind::Liquid);
                        let ext       = match &map.kind {
                            maps_check::MapKind::Liquid => "liquid",
                            maps_check::MapKind::Xslt   => "xslt",
                            maps_check::MapKind::Other  => "",
                        };
                        let file_name = format!("{}.{}", map.name, ext);
                        let folder = {
                            let rel = &map.filename;
                            if let Some(idx) = rel.rfind('/').or_else(|| rel.rfind('\\')) {
                                rel[..idx].to_string()
                            } else { String::new() }
                        };
                        let wf_list = usages.read().get(&map_name).cloned().unwrap_or_default();
                        let is_test_open = test_open.read().as_deref() == Some(&map_name2);

                        rsx! {
                            div { class: "sftp-row",
                                // ── header row ────────────────────────────
                                div { class: "sftp-row-header",
                                    span { style: "font-size:14px; flex-shrink:0", "{icon}" }
                                    div { style: "flex:1; min-width:0; margin-left:8px",
                                        div { class: "sftp-name",
                                            style: "font-family:monospace; font-size:12px; overflow:hidden; text-overflow:ellipsis; white-space:nowrap",
                                            title: "{path}", "{file_name}"
                                        }
                                        if !folder.is_empty() {
                                            div { style: "font-size:10px; color:var(--text3); overflow:hidden; text-overflow:ellipsis",
                                                "{folder}"
                                            }
                                        }
                                    }
                                    button {
                                        class: "btn btn-small",
                                        title: "Open in editor",
                                        onclick: move |_| {
                                            let p = path.clone();
                                            std::thread::spawn(move || crate::utils::open_in_editor(&p));
                                        },
                                        "✎"
                                    }
                                    if is_liquid {
                                        button {
                                            class: if is_test_open { "btn btn-run btn-small" } else { "btn btn-small" },
                                            title: "Test this template",
                                            onclick: move |_| {
                                                if is_test_open {
                                                    test_open.set(None);
                                                } else {
                                                    test_open.set(Some(map_name2.clone()));
                                                    test_output.set(String::new());
                                                    test_err.set(false);
                                                    test_engine.set(String::new());
                                                    // pre-fill input from template variable references
                                                    let suggested = std::fs::read_to_string(&path_suggest)
                                                        .map(|src| maps_check::suggest_liquid_input(&src))
                                                        .unwrap_or_else(|_| "{}".to_string());
                                                    test_input.set(suggested);
                                                }
                                            },
                                            "▶ Test"
                                        }
                                    }
                                }

                                // ── used-by + tester body ─────────────────
                                if !wf_list.is_empty() || (is_test_open && is_liquid) {
                                    div { class: "sftp-details",

                                        // used-by chips
                                        if !wf_list.is_empty() {
                                            div { style: "display:flex; flex-wrap:wrap; gap:4px; align-items:center",
                                                span { class: "sftp-label", "Used by" }
                                                for wf in &wf_list {
                                                    span { class: "wf-chip wf-chip-http",
                                                        style: "font-size:10px; padding:1px 6px",
                                                        "{wf}"
                                                    }
                                                }
                                            }
                                        }

                                        // inline tester
                                        if is_test_open && is_liquid {
                                            {
                                                let map_path = map.path.clone();
                                                rsx! {
                                                    div { class: "maps-tester",
                                                        div { class: "maps-tester-row",
                                                            div { class: "maps-tester-col",
                                                                div { class: "maps-tester-label", "Input JSON" }
                                                                textarea {
                                                                    class: "maps-tester-area",
                                                                    placeholder: "Paste JSON input here",
                                                                    value: "{test_input}",
                                                                    oninput: move |e| test_input.set(e.value()),
                                                                }
                                                            }
                                                            div { class: "maps-tester-col",
                                                                div { class: "maps-tester-label", "Output" }
                                                                pre {
                                                                    class: if *test_err.read() { "maps-tester-area maps-tester-err" }
                                                                           else { "maps-tester-area" },
                                                                    style: "overflow:auto; white-space:pre-wrap",
                                                                    "{test_output}"
                                                                }
                                                            }
                                                        }
                                                        div { class: "sftp-row-header", style: "border-top:1px solid var(--border2); border-bottom:none; background:transparent; padding:6px 10px",
                                                            button {
                                                                class: "btn btn-run btn-small",
                                                                onclick: move |_| {
                                                                    let input = test_input.read().clone();
                                                                    let src = std::fs::read_to_string(&map_path)
                                                                        .unwrap_or_else(|e| format!("read error: {}", e));
                                                                    match maps_check::eval_liquid(&src, &input) {
                                                                        Ok((out, engine)) => {
                                                                            test_output.set(out);
                                                                            test_err.set(false);
                                                                            test_engine.set(match engine {
                                                                                maps_check::LiquidEngine::DotLiquid => "DotLiquid via dotnet".into(),
                                                                                maps_check::LiquidEngine::Stdlib    => "liquid 0.26 + DotLiquid filters".into(),
                                                                            });
                                                                        }
                                                                        Err(e) => { test_output.set(e); test_err.set(true); test_engine.set(String::new()); }
                                                                    }
                                                                },
                                                                "▶ Run"
                                                            }
                                                            if !test_engine.read().is_empty() {
                                                                span { style: "font-size:10px; color:var(--green); font-style:italic",
                                                                    "✓ {test_engine}"
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
                }
            }
        }
    }
}
