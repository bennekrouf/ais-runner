use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};
use crate::services::{
    azure_cli::{self, BlobInfo, SbQueueStats},
    azurite_client,
    azure_sync,
    blob_check::BlobConnection,
    config,
    cosmos_check::{self, CosmosConnection},
    env_mode::{self, EnvMode},
    sb_check::{self, SbQueueInfo},
    sftp_check::{self, SftpConnection, SftpTestResult},
    sql_check::{self, SqlAuthType, SqlConnection, TestResult},
    settings_file,
};

/// Fetch the full container+blob list directly from Azurite over HTTP.
/// Uses the Azurite REST API with Shared Key auth — no az CLI required.
/// Parse `input` as `container[/optional/folder/prefix]`, create the container
/// (idempotent), then — if a prefix is given — create a `.keep` placeholder blob
/// so the virtual folder appears in listings immediately.
async fn do_create_container_or_folder(input: String) -> Result<(), String> {
    let (container, folder) = match input.find('/') {
        None => (input.clone(), None),
        Some(pos) => {
            let c = input[..pos].trim().to_string();
            let f = input[pos + 1..].trim().trim_end_matches('/').to_string();
            (c, if f.is_empty() { None } else { Some(f) })
        }
    };
    // Create the container (ignores 409 Already Exists)
    let c2 = container.clone();
    tokio::task::spawn_blocking(move || azurite_client::create_container(&c2))
        .await
        .map_err(|e| format!("Task panicked: {}", e))?
        .map_err(|e| format!("Create container failed: {}", e))?;
    // If a folder prefix was given, materialise it with a .keep blob
    if let Some(prefix) = folder {
        let c3 = container.clone();
        tokio::task::spawn_blocking(move || {
            azurite_client::create_virtual_folder(&c3, &prefix)
        })
        .await
        .map_err(|e| format!("Task panicked: {}", e))?
        .map_err(|e| format!("Create folder failed: {}", e))?;
    }
    Ok(())
}

fn blob_fetch_all() -> Result<Vec<(String, Vec<BlobInfo>)>, String> {
    let names = azurite_client::list_containers()
        .map_err(|e| format!("list containers: {}", e))?;
    let mut out = Vec::new();
    for name in names {
        let blobs = azurite_client::list_blobs(&name).unwrap_or_default();
        out.push((name, blobs));
    }
    Ok(out)
}

fn do_fetch_conn_str(
    ns:             String,
    rg_cached:      Option<String>,
    subscription:   Option<String>,
    mut sb_rg:      Signal<Option<String>>,
    mut sb_cs_edit: Signal<String>,
    mut fetching:   Signal<bool>,
    mut status:     Signal<Option<(String, bool)>>,
) {
    fetching.set(true);
    spawn(async move {
        let rg = if let Some(r) = rg_cached {
            Ok(r)
        } else {
            let ns2 = ns.clone();
            let sub = subscription.clone();
            tokio::task::spawn_blocking(move || azure_cli::sb_find_rg(&ns2, sub.as_deref()))
                .await
                .unwrap_or(Err(azure_cli::AzError::Other("task failed".into())))
        };
        match rg {
            Ok(r) => {
                sb_rg.set(Some(r.clone()));
                match tokio::task::spawn_blocking(move || azure_cli::sb_fetch_conn_str(&r, &ns)).await {
                    Ok(Ok(cs)) => {
                        sb_cs_edit.set(cs);
                        status.set(Some(("Connection string fetched — click 💾 Save to apply.".into(), false)));
                    }
                    Ok(Err(e)) => { status.set(Some((format!("Fetch error: {:?}", e), true))); }
                    Err(_)     => { status.set(Some(("Task failed".into(), true))); }
                }
            }
            Err(e) => { status.set(Some((format!("RG lookup failed: {:?}", e), true))); }
        }
        fetching.set(false);
    });
}

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
    pub on_close:          EventHandler<()>,
    pub on_saved:          EventHandler<String>,
    pub on_env_changed:    EventHandler<()>,
}

#[component]
pub fn DbPanel(props: DbPanelProps) -> Element {
    // ── SQL signals ──────────────────────────────────────────────────────────
    let mut edits: Signal<HashMap<String, String>> = use_signal(|| {
        let mut m = HashMap::new();
        for c in &props.connections {
            if let Some(k) = &c.server_key   { m.insert(k.clone(), c.resolved_server.clone()); }
            if let Some(k) = &c.db_key       { m.insert(k.clone(), c.resolved_db.clone()); }
            if let Some(k) = &c.conn_str_key { m.insert(k.clone(), c.resolved_conn_str.clone()); }
        }
        m
    });

    // Blob connection endpoint edits: appsetting_key → current value
    let mut blob_edits: Signal<HashMap<String, String>> = use_signal(|| {
        props.blob_connections.iter()
            .map(|c| (c.endpoint_key.clone(), c.endpoint.clone()))
            .collect()
    });

    // AzureWebJobsStorage edit
    let mut webjobs_edit: Signal<String> = use_signal(|| props.webjobs_storage.clone());

    // Cosmos connection edits: appsetting_key → current value
    let mut cosmos_edits: Signal<HashMap<String, String>> = use_signal(|| {
        let mut m = HashMap::new();
        for c in &props.cosmos_connections {
            if let Some(k) = &c.endpoint_key { m.insert(k.clone(), c.endpoint.clone()); }
            if let Some(k) = &c.key_key      { m.insert(k.clone(), c.account_key.clone()); }
        }
        m
    });
    let mut cosmos_test_results: Signal<HashMap<String, Result<u64, String>>> = use_signal(HashMap::new);
    let mut cosmos_testing: Signal<HashSet<String>> = use_signal(HashSet::new);

    // Standalone emulator test (works even with no connections.json entry)
    let mut cosmos_adhoc_endpoint: Signal<String> = use_signal(|| cosmos_check::EMULATOR_ENDPOINT.to_string());
    let mut cosmos_adhoc_key:      Signal<String> = use_signal(|| cosmos_check::EMULATOR_KEY.to_string());
    let mut cosmos_adhoc_testing:  Signal<bool>   = use_signal(|| false);
    let mut cosmos_adhoc_result:   Signal<Option<Result<u64, String>>> = use_signal(|| None);

    // ── Env switch (Local ↔ Azure) ────────────────────────────────────────────
    let mut env_switching:    Signal<bool>                = use_signal(|| false);
    let mut env_pending_keys: Signal<Vec<String>>         = use_signal(Vec::new);
    let mut env_accounts:     Signal<Vec<(String, String)>> = use_signal(Vec::new);
    let mut sql_test_results: Signal<HashMap<String, TestResult>> = use_signal(HashMap::new);
    let mut sql_testing:      Signal<HashSet<String>>             = use_signal(HashSet::new);

    // ── Service Bus signals ──────────────────────────────────────────────────
    let mut sb_tcp_result: Signal<Option<TestResult>> = use_signal(|| None);
    let mut sb_tcp_testing: Signal<bool>              = use_signal(|| false);
    let mut sb_tcp_443_result: Signal<Option<TestResult>> = use_signal(|| None);
    let mut sb_tcp_443_testing: Signal<bool>              = use_signal(|| false);
    let mut sb_rg:          Signal<Option<String>>    = use_signal(|| None);
    let mut sb_stats:       Signal<HashMap<String, SbQueueStats>> = use_signal(HashMap::new);
    let mut sb_fetching:    Signal<HashSet<String>>   = use_signal(HashSet::new);
    let mut sb_send_open:   Signal<HashSet<String>>   = use_signal(HashSet::new);
    let mut sb_send_bodies: Signal<HashMap<String, String>> = use_signal(HashMap::new);

    // Namespace editing
    let mut sb_ns_edit: Signal<String> = use_signal(|| {
        if !props.sb_namespace.is_empty() {
            props.sb_namespace.clone()
        } else {
            config::load().get_link(&props.logic_apps_dir)
                .and_then(|l| l.sb_namespace.clone())
                .unwrap_or_default()
        }
    });
    // (short_name, fqdn, rg) list fetched from az
    let mut sb_ns_list:    Signal<Vec<(String, String, String)>> = use_signal(Vec::new);
    let mut sb_ns_loading: Signal<bool>                          = use_signal(|| false);

    // Connection string (for local dev — alternative to MSI)
    let initial_conn_str = props.sb_conn_str.as_ref()
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let sb_cs_edit:    Signal<String> = use_signal(|| initial_conn_str);
    let sb_cs_fetching: Signal<bool>  = use_signal(|| false);

    // ── Azure queue sync ─────────────────────────────────────────────────────
    // None = not fetched yet, Some(vec) = queue names that exist in Azure
    let mut az_queues:         Signal<Option<Vec<String>>> = use_signal(|| None);
    let mut az_queues_loading: Signal<bool>                = use_signal(|| false);
    let mut az_creating:       Signal<HashSet<String>>     = use_signal(HashSet::new);

    // ── Subscription (for scoped az calls) ──────────────────────────────────
    // Stored as a Signal so closures can clone it freely without move conflicts.
    let subscription: Signal<Option<String>> =
        use_signal(|| azure_sync::detect_subscription(&props.logic_apps_dir));

    // ── Local blob storage (Azurite) ─────────────────────────────────────────
    // Vec of (container_name, blobs) — None = not yet fetched
    let mut blob_containers:  Signal<Option<Vec<(String, Vec<BlobInfo>)>>> = use_signal(|| None);
    let mut blob_loading:     Signal<bool>              = use_signal(|| false);
    let mut blob_clearing:    Signal<HashSet<String>>   = use_signal(HashSet::new);
    let mut blob_uploading:   Signal<HashSet<String>>   = use_signal(HashSet::new);
    let mut blob_creating:    Signal<bool>              = use_signal(|| false);
    // Which containers have their blob list expanded
    let mut blob_expanded:    Signal<HashSet<String>>   = use_signal(HashSet::new);
    // Input for new container name
    let mut new_container_name: Signal<String>          = use_signal(String::new);
    // Inline confirmation: container name waiting for a second "Confirm" click
    let mut blob_clear_confirm: Signal<Option<String>>  = use_signal(|| None);

    // Input for new queue name
    let mut new_queue_name:     Signal<String>          = use_signal(String::new);
    let mut new_queue_creating: Signal<bool>            = use_signal(|| false);

    // ── Active tab ───────────────────────────────────────────────────────────
    let mut active_tab: Signal<&'static str> = use_signal(|| "blob");

    // ── Shared status bar ────────────────────────────────────────────────────
    let mut status: Signal<Option<(String, bool)>> = use_signal(|| None);

    // Auto-refresh blob list whenever the blob tab becomes active and Azurite is running.
    // Use .peek() for blob_loading so it is NOT a reactive dependency — only active_tab
    // switching drives re-runs, preventing the infinite loop that .read() would cause.
    let azurite_up = props.azurite_running;
    use_effect(move || {
        if *active_tab.read() == "blob" && azurite_up && !*blob_loading.peek() {
            blob_loading.set(true);
            blob_clear_confirm.set(None);
            spawn(async move {
                match tokio::task::spawn_blocking(blob_fetch_all).await {
                    Ok(Ok(list)) => { blob_containers.set(Some(list)); }
                    Ok(Err(e))   => { status.set(Some((format!("Refresh failed: {}", e), true))); }
                    Err(_)       => {}
                }
                blob_loading.set(false);
            });
        }
    });

    // Auto-refresh SB queues when the SB tab becomes active
    use_effect(move || {
        if *active_tab.read() == "sb" && !sb_ns_edit.read().is_empty() && az_queues.read().is_none() && !*az_queues_loading.peek() {
            let ns = sb_ns_edit.read().clone();
            let rg_cached = sb_rg.read().clone();
            let sub = subscription.read().clone();
            az_queues_loading.set(true);
            spawn(async move {
                let rg = if let Some(r) = rg_cached { Ok(r) } else {
                    let ns2 = ns.clone();
                    tokio::task::spawn_blocking(move || azure_cli::sb_find_rg(&ns2, sub.as_deref()))
                        .await
                        .unwrap_or(Err(azure_cli::AzError::Other("task failed".into())))
                };
                match rg {
                    Ok(r) => {
                        sb_rg.set(Some(r.clone()));
                        match tokio::task::spawn_blocking(move || azure_cli::sb_list_queues(&r, &ns)).await {
                            Ok(Ok(list)) => { az_queues.set(Some(list)); }
                            Ok(Err(_))   => {}
                            Err(_)       => {}
                        }
                    }
                    Err(_) => {}
                }
                az_queues_loading.set(false);
            });
        }
    });

    let dir = props.logic_apps_dir.clone();

    // ── Save SQL + SB namespace + SB connection string ───────────────────────
    let on_save = {
        let dir    = dir.clone();
        let ns_key = props.sb_namespace_key.clone();
        let cs_key = props.sb_conn_str.as_ref().map(|(k, _)| k.clone());
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
                        if let Some(ref key) = ns_key {
                            let new_ns = sb_ns_edit.read().clone();
                            if !new_ns.is_empty() {
                                vals.insert(key.clone(), serde_json::Value::String(new_ns));
                            }
                        }
                        if let Some(ref key) = cs_key {
                            let new_cs = sb_cs_edit.read().clone();
                            if !new_cs.is_empty() {
                                vals.insert(key.clone(), serde_json::Value::String(new_cs));
                            }
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

    rsx! {
        div { class: "dialog-backdrop", onclick: move |_| props.on_close.call(()) }

        div { class: "db-panel",
            // ── Header ──────────────────────────────────────────────────────
            div { class: "db-panel-header",
                span { class: "db-panel-title", "🔌 Connections" }
                div { style: "display:flex;gap:8px;align-items:center",
                    if *active_tab.read() != "blob" || !props.blob_connections.is_empty() {
                        button { class: "btn btn-run btn-small", onclick: on_save, "💾 Save" }
                    }
                    button { class: "btn-icon", onclick: move |_| props.on_close.call(()), "×" }
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
            }

            // ── Status bar ─────────────────────────────────────────────────
            if let Some((msg, is_err)) = status.read().clone() {
                div {
                    class: if is_err { "settings-status error" } else { "settings-status ok" },
                    "{msg}"
                }
            }

            div { class: "db-panel-body",

                // ════════════════════════════════════════════════════════════
                // SQL SECTION
                // ════════════════════════════════════════════════════════════
                div {
                    class: if *active_tab.read() == "sql" { "tab-pane" } else { "tab-pane hidden" },
                div { class: "db-section-title", "🗄 SQL Connections" }

                if props.connections.is_empty() {
                    div { class: "empty-state", "No SQL connections found in connections.json" }
                }

                for conn in props.connections.clone() {
                    {
                        let conn_name  = conn.name.clone();
                        let is_testing = sql_testing.read().contains(&conn.name);

                        let host_for_test = match &conn.auth_type {
                            SqlAuthType::ManagedIdentity => {
                                let k = conn.server_key.as_deref().unwrap_or("");
                                edits.read().get(k).cloned().unwrap_or_default()
                            }
                            SqlAuthType::ConnectionString => {
                                let k = conn.conn_str_key.as_deref().unwrap_or("");
                                let cs = edits.read().get(k).cloned().unwrap_or_default();
                                sql_check::parse_server_from_conn_str(&cs).unwrap_or_default()
                            }
                            SqlAuthType::Unknown => String::new(),
                        };

                        let test_result = sql_test_results.read().get(&conn.name).cloned();

                        rsx! {
                            div { class: "db-card",
                                div { class: "db-card-header",
                                    span { class: "db-card-name", "{conn.name}" }
                                    span {
                                        class: match &conn.auth_type {
                                            SqlAuthType::ManagedIdentity  => "db-auth-badge msi",
                                            SqlAuthType::ConnectionString => "db-auth-badge cs",
                                            SqlAuthType::Unknown          => "db-auth-badge",
                                        },
                                        "{conn.auth_type.label()}"
                                    }
                                    div { style: "margin-left:auto;display:flex;gap:8px;align-items:center",
                                        if let Some(ref r) = test_result {
                                            if r.reachable {
                                                span { class: "db-test-ok", "✅ {r.latency_ms.unwrap_or(0)}ms" }
                                            } else {
                                                span { class: "db-test-err",
                                                    title: "{r.error.as_deref().unwrap_or(\"\")}",
                                                    "❌ unreachable"
                                                }
                                            }
                                        }
                                        button {
                                            class: "btn btn-small btn-fetch",
                                            disabled: is_testing || host_for_test.is_empty(),
                                            title: if host_for_test.is_empty() { "Configure a host first" } else { "Test TCP:1433" },
                                            onclick: move |_| {
                                                let host = host_for_test.clone();
                                                let name = conn_name.clone();
                                                sql_testing.write().insert(name.clone());
                                                spawn(async move {
                                                    let result = tokio::task::spawn_blocking(move || {
                                                        sql_check::test_tcp(&host, 1433)
                                                    }).await.unwrap_or(TestResult {
                                                        reachable: false, latency_ms: None,
                                                        error: Some("Task failed".into()),
                                                    });
                                                    sql_test_results.write().insert(name.clone(), result);
                                                    sql_testing.write().remove(&name);
                                                });
                                            },
                                            if is_testing { "Testing…" } else { "⚡ Test" }
                                        }
                                    }
                                }

                                match &conn.auth_type {
                                    SqlAuthType::ManagedIdentity => {
                                        let sk  = conn.server_key.clone().unwrap_or_default();
                                        let dk  = conn.db_key.clone().unwrap_or_default();
                                        let dk2 = dk.clone();
                                        rsx! {
                                            div { class: "db-field-row",
                                                label { class: "db-field-label", "Server" }
                                                input {
                                                    class: "db-field-input",
                                                    placeholder: "e.g. localhost or server.database.windows.net",
                                                    value: "{edits.read().get(&sk).cloned().unwrap_or_default()}",
                                                    oninput: move |e| { edits.write().insert(sk.clone(), e.value()); },
                                                }
                                            }
                                            div { class: "db-field-row",
                                                label { class: "db-field-label", "Database" }
                                                input {
                                                    class: "db-field-input",
                                                    placeholder: "e.g. ais",
                                                    value: "{edits.read().get(&dk).cloned().unwrap_or_default()}",
                                                    oninput: move |e| { edits.write().insert(dk2.clone(), e.value()); },
                                                }
                                            }
                                            div { class: "db-msi-note",
                                                "Auth: Managed Identity — works on Azure. "
                                                "For local dev, point Server to a local SQL instance."
                                            }
                                        }
                                    }
                                    SqlAuthType::ConnectionString => {
                                        let csk = conn.conn_str_key.clone().unwrap_or_default();
                                        let current_cs = edits.read().get(&csk).cloned().unwrap_or_default();
                                        let parsed_host = sql_check::parse_server_from_conn_str(&current_cs);
                                        rsx! {
                                            div { class: "db-field-row",
                                                label { class: "db-field-label", "Connection String" }
                                                textarea {
                                                    class: "db-field-textarea",
                                                    placeholder: "Server=localhost;Database=ODS_X3;User Id=sa;Password=...;TrustServerCertificate=true",
                                                    value: "{current_cs}",
                                                    oninput: move |e| { edits.write().insert(csk.clone(), e.value()); },
                                                }
                                            }
                                            if let Some(host) = parsed_host {
                                                div { class: "db-parsed-host",
                                                    "Parsed host: " span { style: "color:var(--blue)", "{host}" } " (port 1433)"
                                                }
                                            }
                                        }
                                    }
                                    SqlAuthType::Unknown => rsx! {
                                        div { class: "db-msi-note", "Unknown auth type — edit connections.json directly." }
                                    },
                                }

                                if let Some(ref r) = test_result {
                                    if !r.reachable {
                                        if let Some(ref err) = r.error {
                                            div { class: "db-test-error-detail", "{err}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                } // end SQL for loop
                } // end SQL tab-pane

                // ════════════════════════════════════════════════════════════
                // COSMOS DB SECTION
                // ════════════════════════════════════════════════════════════
                div {
                    class: if *active_tab.read() == "cosmos" { "tab-pane" } else { "tab-pane hidden" },

                    div { class: "db-section-title", "🌌 Cosmos DB" }

                    // ── Standalone emulator test ──────────────────────────
                    div { class: "db-card",
                        div { class: "db-card-header",
                            span { class: "db-card-name", "Emulator Test" }
                            span { class: "db-auth-badge cs", "ad-hoc" }
                            div { style: "margin-left:auto;display:flex;gap:8px;align-items:center",
                                if let Some(ref r) = *cosmos_adhoc_result.read() {
                                    match r {
                                        Ok(ms) => rsx! { span { class: "db-test-ok", "✅ {ms}ms" } },
                                        Err(e) => rsx! {
                                            span { class: "db-test-err", title: "{e}", "❌ unreachable" }
                                        },
                                    }
                                }
                                button {
                                    class: "btn btn-small btn-fetch",
                                    disabled: *cosmos_adhoc_testing.read(),
                                    onclick: move |_| {
                                        let ep = cosmos_adhoc_endpoint.read().clone();
                                        cosmos_adhoc_testing.set(true);
                                        cosmos_adhoc_result.set(None);
                                        spawn(async move {
                                            let r = cosmos_check::test_cosmos_endpoint(&ep).await;
                                            cosmos_adhoc_result.set(Some(r));
                                            cosmos_adhoc_testing.set(false);
                                        });
                                    },
                                    if *cosmos_adhoc_testing.read() { "Testing…" } else { "⚡ Test Connection" }
                                }
                            }
                        }

                        div { class: "db-field-row",
                            label { class: "db-field-label", "API Endpoint" }
                            input {
                                class: "db-field-input",
                                placeholder: "https://localhost:8081/",
                                value: "{cosmos_adhoc_endpoint.read()}",
                                oninput: move |e| {
                                    cosmos_adhoc_endpoint.set(e.value());
                                    cosmos_adhoc_result.set(None);
                                },
                            }
                            button {
                                class: "btn btn-small",
                                style: "flex-shrink:0",
                                title: "Reset to emulator default (port 1234 is UI only — API is 8081)",
                                onclick: move |_| {
                                    cosmos_adhoc_endpoint.set(cosmos_check::EMULATOR_ENDPOINT.to_string());
                                    cosmos_adhoc_result.set(None);
                                },
                                "↺ Reset"
                            }
                        }

                        div { class: "db-field-row",
                            label { class: "db-field-label", "Account Key" }
                            input {
                                class: "db-field-input",
                                r#type: "password",
                                placeholder: "emulator or Azure key",
                                value: "{cosmos_adhoc_key.read()}",
                                oninput: move |e| {
                                    cosmos_adhoc_key.set(e.value());
                                    cosmos_adhoc_result.set(None);
                                },
                            }
                            button {
                                class: "btn btn-small",
                                style: "flex-shrink:0",
                                title: "Reset to well-known emulator key",
                                onclick: move |_| {
                                    cosmos_adhoc_key.set(cosmos_check::EMULATOR_KEY.to_string());
                                    cosmos_adhoc_result.set(None);
                                },
                                "↺ Reset"
                            }
                        }

                        if let Some(Err(ref e)) = *cosmos_adhoc_result.read() {
                            div { class: "db-test-error-detail", "{e}" }
                        }
                        div { class: "db-msi-note",
                            "Port 1234 is the data-explorer UI only — Logic Apps connects to "
                            code { "8081" } "."
                        }
                    }

                    if !props.cosmos_connections.is_empty() {
                        div { class: "db-section-title", style: "margin-top:16px", "Connections from connections.json" }
                    }

                    for conn in props.cosmos_connections.clone() {
                        {
                            let conn_name  = conn.connection_name.clone();
                            let conn_name2 = conn_name.clone();
                            let is_testing = cosmos_testing.read().contains(&conn.connection_name);
                            let test_result = cosmos_test_results.read().get(&conn.connection_name).cloned();

                            let ep_key  = conn.endpoint_key.clone().unwrap_or_default();
                            let ep_key2 = ep_key.clone();
                            let ep_key3 = ep_key.clone();
                            let key_key  = conn.key_key.clone().unwrap_or_default();
                            let key_key2 = key_key.clone();

                            let current_endpoint = cosmos_edits.read()
                                .get(&ep_key).cloned().unwrap_or_default();

                            rsx! {
                                div { class: "db-card",
                                    div { class: "db-card-header",
                                        span { class: "db-card-name", "{conn.display_name}" }
                                        span { class: "db-auth-badge cs", "CosmosDB" }
                                        div { style: "margin-left:auto;display:flex;gap:8px;align-items:center",
                                            if let Some(ref r) = test_result {
                                                match r {
                                                    Ok(ms) => rsx! { span { class: "db-test-ok", "✅ {ms}ms" } },
                                                    Err(e) => rsx! {
                                                        span { class: "db-test-err", title: "{e}", "❌ unreachable" }
                                                    },
                                                }
                                            }
                                            button {
                                                class: "btn btn-small btn-fetch",
                                                disabled: is_testing || current_endpoint.is_empty(),
                                                title: if current_endpoint.is_empty() { "Set endpoint first" } else { "Test Cosmos endpoint" },
                                                onclick: move |_| {
                                                    let ep = cosmos_edits.read().get(&ep_key2).cloned().unwrap_or_default();
                                                    let name = conn_name.clone();
                                                    cosmos_testing.write().insert(name.clone());
                                                    spawn(async move {
                                                        let result = cosmos_check::test_cosmos_endpoint(&ep).await;
                                                        cosmos_test_results.write().insert(name.clone(), result);
                                                        cosmos_testing.write().remove(&name);
                                                    });
                                                },
                                                if is_testing { "Testing…" } else { "⚡ Test" }
                                            }
                                        }
                                    }

                                    div { class: "db-field-row",
                                        label { class: "db-field-label", "Endpoint" }
                                        input {
                                            class: "db-field-input",
                                            placeholder: "https://localhost:8081/",
                                            value: "{cosmos_edits.read().get(&ep_key).cloned().unwrap_or_default()}",
                                            oninput: move |e| { cosmos_edits.write().insert(ep_key.clone(), e.value()); },
                                        }
                                        button {
                                            class: "btn btn-small",
                                            style: "flex-shrink:0",
                                            title: "Use Cosmos DB Emulator endpoint",
                                            onclick: move |_| {
                                                cosmos_edits.write().insert(ep_key3.clone(), cosmos_check::EMULATOR_ENDPOINT.to_string());
                                            },
                                            "🌌 Use Emulator"
                                        }
                                    }

                                    if !key_key.is_empty() {
                                        div { class: "db-field-row",
                                            label { class: "db-field-label", "Account Key" }
                                            input {
                                                class: "db-field-input",
                                                r#type: "password",
                                                placeholder: "(emulator key or Azure key)",
                                                value: "{cosmos_edits.read().get(&key_key).cloned().unwrap_or_default()}",
                                                oninput: move |e| { cosmos_edits.write().insert(key_key.clone(), e.value()); },
                                            }
                                            button {
                                                class: "btn btn-small",
                                                style: "flex-shrink:0",
                                                title: "Use well-known Cosmos DB Emulator key",
                                                onclick: move |_| {
                                                    cosmos_edits.write().insert(key_key2.clone(), cosmos_check::EMULATOR_KEY.to_string());
                                                },
                                                "🌌 Use Emulator Key"
                                            }
                                        }
                                    }

                                    if let Some(ref r) = test_result {
                                        if let Err(ref e) = r {
                                            div { class: "db-test-error-detail", "{e}" }
                                        }
                                    }
                                    div { class: "db-msi-note",
                                        "After saving, restart func start. The emulator uses a self-signed cert — "
                                        "Logic Apps Standard accepts it locally."
                                    }
                                }
                            }
                        }
                    }
                } // end Cosmos tab-pane

                // ════════════════════════════════════════════════════════════
                // SERVICE BUS SECTION
                // ════════════════════════════════════════════════════════════
                div {
                    class: if *active_tab.read() == "sb" { "tab-pane" } else { "tab-pane hidden" },
                div { class: "db-section-title", style: "margin-top:20px; display:flex; align-items:center; gap:10px;",
                    span { "📨 Service Bus" }
                    {
                        let is_loading = *az_queues_loading.read();
                        let ns = sb_ns_edit.read().clone();
                        let already = az_queues.read().is_some();
                        let sub_for_check = subscription.read().clone();
                        rsx! {
                            button {
                                class: "btn btn-small btn-fetch",
                                disabled: is_loading || ns.is_empty(),
                                title: "List all queues in Azure and compare with local workflows",
                                onclick: move |_| {
                                    let ns2 = sb_ns_edit.read().clone();
                                    let rg_cached = sb_rg.read().clone();
                                    az_queues.set(None);
                                    az_queues_loading.set(true);
                                    let sub2 = sub_for_check.clone();
                                    spawn(async move {
                                        let rg = if let Some(r) = rg_cached {
                                            Ok(r)
                                        } else {
                                            let ns3 = ns2.clone();
                                            tokio::task::spawn_blocking(move || azure_cli::sb_find_rg(&ns3, sub2.as_deref()))
                                                .await
                                                .unwrap_or(Err(azure_cli::AzError::Other("task failed".into())))
                                        };
                                        match rg {
                                            Ok(r) => {
                                                sb_rg.set(Some(r.clone()));
                                                match tokio::task::spawn_blocking(move || {
                                                    azure_cli::sb_list_queues(&r, &ns2)
                                                }).await {
                                                    Ok(Ok(list)) => az_queues.set(Some(list)),
                                                    Ok(Err(e))   => status.set(Some((format!("Queue list error: {:?}", e), true))),
                                                    Err(_)       => status.set(Some(("Task failed".into(), true))),
                                                }
                                            }
                                            Err(e) => status.set(Some((format!("RG lookup failed: {:?}", e), true))),
                                        }
                                        az_queues_loading.set(false);
                                    });
                                },
                                if is_loading { "☁ Loading…" } else if already { "☁ Refresh" } else { "☁ Check Queues" }
                            }
                            if let Some(ref list) = *az_queues.read() {
                                span { class: "db-az-summary",
                                    "{list.len()} queues in Azure"
                                }
                            }
                        }
                    }
                }

                div { class: "db-create-row",
                    input {
                        class: "db-field-input",
                        style: "flex: 1",
                        placeholder: "Create new queue (e.g. ais.workflow.error)...",
                        value: "{new_queue_name}",
                        oninput: move |e| new_queue_name.set(e.value()),
                    }
                    button {
                        class: "btn btn-run btn-small",
                        disabled: *new_queue_creating.read() || new_queue_name.read().is_empty() || sb_ns_edit.read().is_empty(),
                        onclick: move |_| {
                            let q = new_queue_name.read().clone();
                            let ns = sb_ns_edit.read().clone();
                            let rg_cached = sb_rg.read().clone();
                            let sub = subscription.read().clone();
                            new_queue_creating.set(true);
                            spawn(async move {
                                let rg = if let Some(r) = rg_cached { Ok(r) } else {
                                    let ns2 = ns.clone();
                                    tokio::task::spawn_blocking(move || azure_cli::sb_find_rg(&ns2, sub.as_deref()))
                                        .await
                                        .unwrap_or(Err(azure_cli::AzError::Other("task failed".into())))
                                };
                                match rg {
                                    Ok(r) => {
                                        sb_rg.set(Some(r.clone()));
                                        let q_task = q.clone();
                                        match tokio::task::spawn_blocking(move || azure_cli::sb_create_queue(&r, &ns, &q_task)).await {
                                            Ok(Ok(_)) => {
                                                status.set(Some((format!("✅ Created queue '{}'", q), false)));
                                                new_queue_name.set(String::new());
                                                if let Some(ref mut list) = *az_queues.write() {
                                                    list.push(q);
                                                }
                                            }
                                            Ok(Err(e)) => status.set(Some((format!("Create failed: {:?}", e), true))),
                                            Err(_)     => status.set(Some(("Task failed".into(), true))),
                                        }
                                    }
                                    Err(e) => status.set(Some((format!("RG lookup failed: {:?}", e), true))),
                                }
                                new_queue_creating.set(false);
                            });
                        },
                        if *new_queue_creating.read() { "Creating..." } else { "➕ Create Queue" }
                    }
                }

                {
                    let app_cfg = config::load();
                    let link = app_cfg.get_link(&props.logic_apps_dir);

                    if props.sb_namespace.is_empty() && props.sb_namespace_key.is_none() && props.sb_conn_str.is_none() {
                        rsx! { div { class: "empty-state", "No Service Bus connection found in connections.json" } }
                    } else if props.sb_namespace.is_empty() && link.is_some() {
                        let l = link.unwrap();
                        let rg = l.resource_group.clone();
                        let sub_id = l.subscription_id.clone();
                        let dir_auto = props.logic_apps_dir.clone();
                        rsx! {
                            div { class: "db-card", style: "background: var(--blue-bg); border: 1px solid var(--blue); padding: 15px; display: flex; flex-direction: column; gap: 10px;",
                                div { style: "font-weight: 600; color: var(--blue);", "🔗 Azure Link Detected" }
                                div { style: "font-size: 13px;", "This project is linked to Resource Group " b { "{rg}" } ". We can automatically find your Service Bus namespace." }
                                button {
                                    class: "btn btn-run btn-small",
                                    style: "align-self: flex-start;",
                                    onclick: move |_| {
                                        let d = dir_auto.clone();
                                        let rg2 = rg.clone();
                                        let sub2 = sub_id.clone();
                                        spawn(async move {
                                            let _ = tokio::task::spawn_blocking(move || {
                                                crate::services::setup_manager::auto_detect_resources(&d, Some(&sub2), &rg2)
                                            }).await;
                                            props.on_saved.call("⚠ Settings saved — stop and restart func start to apply changes.".into());
                                        });
                                    },
                                    "Auto-Link Service Bus"
                                }
                            }
                        }
                    } else {
                        // Namespace card — editable + browse
                        let tcp_res = sb_tcp_result.read().clone();
                        let is_tcp_testing = *sb_tcp_testing.read();
                        let is_loading = *sb_ns_loading.read();
                        let ns_list = sb_ns_list.read().clone();
                        let current_edit = sb_ns_edit.read().clone();
                        let has_key = props.sb_namespace_key.is_some();
                        let uses_conn_str = props.sb_conn_str.is_some();
                        let auth_badge: &str = if uses_conn_str { "ConnStr" } else { "MSI" };
                        let auth_class: &str = if uses_conn_str { "db-auth-badge cs" } else { "db-auth-badge msi" };
                        let fqdn_title: &str = if has_key && !uses_conn_str {
                            "Edit to switch to a different namespace"
                        } else if uses_conn_str {
                            "FQDN is derived from the connection string — edit the connection string to change it"
                        } else {
                            "Namespace is hardcoded in connections.json"
                        };

                        rsx! {
                            div { class: "db-card",
                                div { class: "db-card-header",
                                    span { class: "db-card-name", "Namespace" }
                                    span { class: "{auth_class}", "{auth_badge}" }
                                    div { style: "margin-left:auto;display:flex;gap:8px;align-items:center",
                                        // Fetch button — prominent when namespace is empty
                                        button {
                                            class: if current_edit.is_empty() { "btn btn-run btn-small" } else { "btn btn-small btn-fetch" },
                                            disabled: is_loading,
                                            title: "List Service Bus namespaces from your Azure subscription",
                                            onclick: move |_| {
                                                if sb_ns_list.read().is_empty() && !*sb_ns_loading.peek() {
                                                    sb_ns_loading.set(true);
                                                    spawn(async move {
                                                        match tokio::task::spawn_blocking(azure_cli::sb_list_namespaces).await {
                                                            Ok(Ok(list)) => { sb_ns_list.set(list); }
                                                            Ok(Err(e))   => { status.set(Some((format!("az error: {:?}", e), true))); }
                                                            Err(_)       => { status.set(Some(("Could not list namespaces — az login required".into(), true))); }
                                                        }
                                                        sb_ns_loading.set(false);
                                                    });
                                                } else {
                                                    sb_ns_list.write().clear();
                                                }
                                            },
                                            if is_loading { "☁ Loading…" } else if !sb_ns_list.read().is_empty() { "☁ Close" } else { "☁ Fetch from Azure" }
                                        }
                                        if let Some(ref r) = tcp_res {
                                            if r.reachable {
                                                span { class: "db-test-ok", "✅ {r.latency_ms.unwrap_or(0)}ms (5671)" }
                                            } else {
                                                span { class: "db-test-err",
                                                    title: "{r.error.as_deref().unwrap_or(\"\")}",
                                                    "❌ port 5671 unreachable"
                                                }
                                            }
                                        }
                                        button {
                                            class: "btn btn-small btn-fetch",
                                            disabled: is_tcp_testing || current_edit.is_empty(),
                                            onclick: move |_| {
                                                let ns = sb_ns_edit.read().clone();
                                                sb_tcp_result.set(None);
                                                sb_tcp_testing.set(true);
                                                spawn(async move {
                                                    let result = tokio::task::spawn_blocking(move || {
                                                        sb_check::test_sb_tcp(&ns)
                                                    }).await.unwrap_or(TestResult {
                                                        reachable: false, latency_ms: None,
                                                        error: Some("Task failed".into()),
                                                    });
                                                    sb_tcp_result.set(Some(result));
                                                    sb_tcp_testing.set(false);
                                                });
                                            },
                                            if is_tcp_testing { "Testing…" } else { "⚡ Test TCP" }
                                        }

                                        if let Some(ref r) = *sb_tcp_result.read() {
                                            if !r.reachable {
                                                button {
                                                    class: "btn btn-small btn-fetch",
                                                    style: "margin-left: 8px",
                                                    disabled: *sb_tcp_443_testing.read() || current_edit.is_empty(),
                                                    onclick: move |_| {
                                                        let ns = sb_ns_edit.read().clone();
                                                        sb_tcp_443_result.set(None);
                                                        sb_tcp_443_testing.set(true);
                                                        spawn(async move {
                                                            let result = tokio::task::spawn_blocking(move || {
                                                                sb_check::test_sb_tcp_443(&ns)
                                                            }).await.unwrap_or(TestResult {
                                                                reachable: false, latency_ms: None,
                                                                error: Some("Task failed".into()),
                                                            });
                                                            sb_tcp_443_result.set(Some(result));
                                                            sb_tcp_443_testing.set(false);
                                                        });
                                                    },
                                                    if *sb_tcp_443_testing.read() { "Testing 443…" } else { "⚡ Test 443" }
                                                }
                                            }
                                        }
                                        
                                        if let Some(ref r) = *sb_tcp_443_result.read() {
                                            if r.reachable {
                                                span { class: "db-test-ok", style: "margin-left: 8px", "✅ Port 443 reachable ({r.latency_ms.unwrap_or(0)}ms)" }
                                            } else {
                                                span { class: "db-test-err", style: "margin-left: 8px", title: "{r.error.as_deref().unwrap_or(\"\")}", "❌ Port 443 unreachable" }
                                            }
                                        }
                                    }
                                }

                                // Namespace combobox
                                div { class: "db-field-row",
                                    label { class: "db-field-label", "Namespace" }
                                    div { class: "db-ns-combobox",
                                        div {
                                            class: if is_loading { "db-ns-field loading" } else { "db-ns-field" },
                                            title: fqdn_title,
                                            onclick: move |_| {
                                                if ns_list.is_empty() && !is_loading {
                                                    sb_ns_loading.set(true);
                                                    spawn(async move {
                                                        match tokio::task::spawn_blocking(azure_cli::sb_list_namespaces).await {
                                                            Ok(Ok(list)) => { sb_ns_list.set(list); }
                                                            Ok(Err(e)) => { status.set(Some((format!("az error: {:?}", e), true))); }
                                                            Err(_) => { status.set(Some(("Could not list namespaces — az login required".into(), true))); }
                                                        }
                                                        sb_ns_loading.set(false);
                                                    });
                                                } else {
                                                    sb_ns_list.write().clear();
                                                }
                                            },
                                            span { class: "db-ns-field-value",
                                                if current_edit.is_empty() {
                                                    span { class: "db-ns-placeholder", "Click to pick a namespace…" }
                                                } else {
                                                    "{current_edit}"
                                                }
                                            }
                                            span { class: "db-ns-chevron",
                                                if is_loading { "…" } else if !ns_list.is_empty() { "▲" } else { "▾" }
                                            }
                                        }
                                        if !ns_list.is_empty() {
                                            {
                                                let uses_cs = uses_conn_str;
                                                rsx! {
                                                    div { class: "db-ns-dropdown",
                                                        for (short_name, fqdn, rg) in ns_list.iter() {
                                                            {
                                                                let fqdn2 = fqdn.clone();
                                                                let fqdn3 = fqdn.clone();
                                                                let rg2   = rg.clone();
                                                                let is_active = *fqdn == current_edit;
                                                                rsx! {
                                                                    div {
                                                                        class: if is_active { "db-ns-item active" } else { "db-ns-item" },
                                                                        onclick: move |_| {
                                                                            sb_ns_edit.set(fqdn2.clone());
                                                                            sb_tcp_result.set(None);
                                                                            sb_ns_list.write().clear();
                                                                            if uses_cs {
                                                                                sb_rg.set(Some(rg2.clone()));
                                                                                do_fetch_conn_str(fqdn3.clone(), Some(rg2.clone()), subscription.read().clone(), sb_rg, sb_cs_edit, sb_cs_fetching, status);
                                                                            }
                                                                        },
                                                                        span { class: "db-ns-item-name", "{short_name}" }
                                                                        span { class: "db-ns-item-rg", "{rg}" }
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
                                if let Some((ref cs_key_label, _)) = props.sb_conn_str {
                                    {
                                        let is_cs_fetching = *sb_cs_fetching.read();
                                        let cs_value = sb_cs_edit.read().clone();
                                        let cs_status: &str = if is_cs_fetching { "Fetching…" } else if !cs_value.is_empty() { "✅ Set" } else { "⚠ Not set" };
                                        let cs_status_class: &str = if is_cs_fetching || cs_value.is_empty() { "db-cs-status warn" } else { "db-cs-status ok" };
                                        rsx! {
                                            div { class: "db-field-row", style: "margin-top:6px",
                                                label { class: "db-field-label", title: "Setting: {cs_key_label}", "Conn string" }
                                                span { class: "{cs_status_class}", "{cs_status}" }
                                            }
                                        }
                                    }
                                }

                                if !has_key && !uses_conn_str {
                                    div { class: "db-msi-note", "⚠ Namespace is hardcoded — add @appsetting('serviceBus_fullyQualifiedNamespace') to connections.json" }
                                }

                                if let Some(ref r) = tcp_res {
                                    if !r.reachable {
                                        if let Some(ref err) = r.error {
                                            div { class: "db-test-error-detail", "{err} — try port 443 (AMQP over WebSockets)." }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                    // Queue cards (local workflows)
                    for q in props.sb_queues.clone() {
                        {
                            let queue_name  = q.queue.clone();
                            let queue_name2 = queue_name.clone();
                            let queue_name3 = queue_name.clone();
                            let queue_name4 = queue_name.clone();
                            let queue_name5 = queue_name.clone();
                            let ns  = props.sb_namespace.clone();
                            let ns2 = ns.clone();
                            let ns3 = ns.clone();
                            let sub_stats  = subscription.read().clone();
                            let sub_create = subscription.read().clone();
                            let is_fetching    = sb_fetching.read().contains(&queue_name);
                            let stats_opt      = sb_stats.read().get(&queue_name).cloned();
                            let is_send_open   = sb_send_open.read().contains(&queue_name);
                            let send_body      = sb_send_bodies.read().get(&queue_name).cloned().unwrap_or_default();
                            let az_status: Option<bool> = az_queues.read().as_ref().map(|list| list.contains(&queue_name));
                            let is_creating    = az_creating.read().contains(&queue_name);

                            rsx! {
                                div { class: "db-card",
                                    div { class: "db-card-header",
                                        span { class: "db-card-name", "{queue_name}" }
                                        div { style: "display:flex;gap:4px;align-items:center",
                                            if !q.trigger_workflows.is_empty() {
                                                span {
                                                    class: "db-wf-badge trigger",
                                                    title: "{q.trigger_workflows.join(\", \")}",
                                                    "T:{q.trigger_workflows.len()}"
                                                }
                                            }
                                            if !q.action_workflows.is_empty() {
                                                span {
                                                    class: "db-wf-badge action",
                                                    title: "{q.action_workflows.join(\", \")}",
                                                    "A:{q.action_workflows.len()}"
                                                }
                                            }
                                            // Azure existence badge
                                            match az_status {
                                                Some(true)  => rsx! { span { class: "db-az-ok",      title: "Queue exists in Azure", "☁✅" } },
                                                Some(false) => rsx! {
                                                    span { class: "db-az-missing", title: "Queue not found in Azure", "☁❌" }
                                                    button {
                                                        class: "btn btn-small btn-warn",
                                                        disabled: is_creating,
                                                        title: "Create this queue in Azure",
                                                        onclick: move |_| {
                                                            let qn  = queue_name5.clone();
                                                            let qn2 = queue_name5.clone();
                                                            let ns4 = ns3.clone();
                                                            let rg_now = sb_rg.read().clone();
                                                            let sub4 = sub_create.clone();
                                                            az_creating.write().insert(qn.clone());
                                                            spawn(async move {
                                                                let rg = if let Some(r) = rg_now { Ok(r) } else {
                                                                    let ns5 = ns4.clone();
                                                                    tokio::task::spawn_blocking(move || azure_cli::sb_find_rg(&ns5, sub4.as_deref()))
                                                                        .await
                                                                        .unwrap_or(Err(azure_cli::AzError::Other("task failed".into())))
                                                                };
                                                                match rg {
                                                                    Ok(r) => {
                                                                        sb_rg.set(Some(r.clone()));
                                                                        match tokio::task::spawn_blocking(move || {
                                                                            azure_cli::sb_create_queue(&r, &ns4, &qn)
                                                                        }).await {
                                                                            Ok(Ok(_)) => {
                                                                                status.set(Some((format!("✅ Created queue '{}'", qn2), false)));
                                                                                // Add to local az_queues list so badge flips to ✅
                                                                                if let Some(ref mut list) = *az_queues.write() {
                                                                                    list.push(qn2.clone());
                                                                                }
                                                                            }
                                                                            Ok(Err(e)) => status.set(Some((format!("Create failed: {:?}", e), true))),
                                                                            Err(_)     => status.set(Some(("Task failed".into(), true))),
                                                                        }
                                                                    }
                                                                    Err(e) => status.set(Some((format!("RG lookup failed: {:?}", e), true))),
                                                                }
                                                                az_creating.write().remove(&qn2);
                                                            });
                                                        },
                                                        if is_creating { "Creating…" } else { "Create" }
                                                    }
                                                },
                                                None => rsx! {},
                                            }
                                        }
                                        div { style: "margin-left:auto;display:flex;gap:6px;align-items:center",
                                            if let Some(ref s) = stats_opt {
                                                span { class: "db-sb-stats",
                                                    "📬 {s.active_message_count}"
                                                    if s.dead_letter_count > 0 {
                                                        span { class: "db-sb-dlq",
                                                            " DLQ:{s.dead_letter_count}"
                                                        }
                                                    }
                                                }
                                            }
                                            button {
                                                class: "btn btn-small btn-fetch",
                                                disabled: is_fetching,
                                                title: "Fetch active message count (requires az login)",
                                                onclick: move |_| {
                                                    let qn  = queue_name2.clone();
                                                    let qn2 = queue_name2.clone();
                                                    let ns3 = ns.clone();
                                                    let rg_now = sb_rg.read().clone();
                                                    let sub3 = sub_stats.clone();
                                                    sb_fetching.write().insert(qn.clone());
                                                    spawn(async move {
                                                        // Resolve RG once
                                                        let rg = if let Some(r) = rg_now {
                                                            r
                                                        } else {
                                                            let ns4 = ns3.clone();
                                                            match tokio::task::spawn_blocking(move || {
                                                                azure_cli::sb_find_rg(&ns4, sub3.as_deref())
                                                            }).await {
                                                                Ok(Ok(r)) => {
                                                                    sb_rg.set(Some(r.clone()));
                                                                    r
                                                                }
                                                                _ => {
                                                                    status.set(Some((
                                                                        "Could not find SB resource group — ensure az login is active".into(),
                                                                        true,
                                                                    )));
                                                                    sb_fetching.write().remove(&qn);
                                                                    return;
                                                                }
                                                            }
                                                        };
                                                        match tokio::task::spawn_blocking(move || {
                                                            azure_cli::sb_queue_stats(&rg, &ns3, &qn)
                                                        }).await {
                                                            Ok(Ok(s)) => {
                                                                sb_stats.write().insert(qn2.clone(), s);
                                                            }
                                                            Ok(Err(e)) => {
                                                                status.set(Some((format!("Stats error: {:?}", e), true)));
                                                            }
                                                            Err(_) => {
                                                                status.set(Some(("Stats task panicked".into(), true)));
                                                            }
                                                        }
                                                        sb_fetching.write().remove(&qn2);
                                                    });
                                                },
                                                if is_fetching { "…" } else { "📊" }
                                            }
                                            button {
                                                class: "btn btn-small",
                                                title: "Send a test message to this queue",
                                                onclick: move |_| {
                                                    if sb_send_open.read().contains(&queue_name3) {
                                                        sb_send_open.write().remove(&queue_name3);
                                                    } else {
                                                        sb_send_open.write().insert(queue_name3.clone());
                                                    }
                                                },
                                                if is_send_open { "▲ Close" } else { "📤 Send" }
                                            }
                                        }
                                    }

                                    // Inline send form
                                    if is_send_open {
                                        {
                                            let qn  = queue_name4.clone();
                                            let qn2 = queue_name4.clone();
                                            let ns3 = ns2.clone();
                                            rsx! {
                                                div { class: "db-send-form",
                                                    textarea {
                                                        class: "db-field-textarea",
                                                        placeholder: "{{ \"key\": \"value\" }}",
                                                        value: "{send_body}",
                                                        oninput: move |e| {
                                                            sb_send_bodies.write().insert(qn.clone(), e.value());
                                                        },
                                                    }
                                                    div { style: "display:flex;gap:8px;margin-top:6px",
                                                        button {
                                                            class: "btn btn-run btn-small",
                                                            onclick: move |_| {
                                                                let ns4  = ns3.clone();
                                                                let qn4  = qn2.clone();
                                                                let body = sb_send_bodies.read()
                                                                    .get(&qn2).cloned().unwrap_or_default();
                                                                spawn(async move {
                                                                    let token = tokio::task::spawn_blocking(
                                                                        azure_cli::sb_get_bearer_token
                                                                    ).await
                                                                     .unwrap_or(Err(azure_cli::AzError::Other("task failed".into())));
                                                                    match token {
                                                                        Err(e) => {
                                                                            status.set(Some((
                                                                                format!("Token error: {:?}", e), true,
                                                                            )));
                                                                        }
                                                                        Ok(t) => {
                                                                            let url = format!(
                                                                                "https://{}/{}/messages",
                                                                                ns4, qn4
                                                                            );
                                                                            let client = reqwest::Client::new();
                                                                            match client
                                                                                .post(&url)
                                                                                .header("Authorization", format!("Bearer {}", t))
                                                                                .header("Content-Type", "application/json")
                                                                                .body(reqwest::Body::from(body))
                                                                                .send()
                                                                                .await
                                                                            {
                                                                                Ok(resp) if resp.status().is_success() => {
                                                                                    status.set(Some((
                                                                                        format!("✅ Message sent to {}", qn4),
                                                                                        false,
                                                                                    )));
                                                                                }
                                                                                Ok(resp) => {
                                                                                    let code = resp.status();
                                                                                    let body = resp.text().await.unwrap_or_default();
                                                                                    status.set(Some((
                                                                                        format!("HTTP {}: {}", code, body),
                                                                                        true,
                                                                                    )));
                                                                                }
                                                                                Err(e) => {
                                                                                    status.set(Some((
                                                                                        format!("Send error: {}", e), true,
                                                                                    )));
                                                                                }
                                                                            }
                                                                        }
                                                                    }
                                                                });
                                                            },
                                                            "Send"
                                                        }
                                                        span { class: "db-msi-note",
                                                            "Sent via REST API (Bearer token from current az session)"
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

                    // Azure-only queues (exist in Azure but not referenced by any local workflow)
                    {
                        let local_names: HashSet<String> = props.sb_queues.iter()
                            .map(|q| q.queue.clone())
                            .collect();
                        let azure_only: Vec<String> = az_queues.read()
                            .as_ref()
                            .map(|list| list.iter()
                                .filter(|q| !local_names.contains(*q))
                                .cloned()
                                .collect())
                            .unwrap_or_default();

                        if !azure_only.is_empty() {
                            rsx! {
                                div { class: "db-section-sub", "☁ Azure-only queues (not used by local workflows)" }
                                for qname in azure_only {
                                    div { class: "db-card db-card-azure-only",
                                        div { class: "db-card-header",
                                            span { class: "db-card-name", "{qname}" }
                                            span { class: "db-az-ok", "☁ Azure only" }
                                        }
                                    }
                                }
                            }
                        } else {
                            rsx! {}
                        }
                    }
                } // end SB tab-pane

                // ════════════════════════════════════════════════════════════
                // LOCAL BLOB STORAGE (AZURITE)
                // ════════════════════════════════════════════════════════════
                div {
                    class: if *active_tab.read() == "blob" { "tab-pane" } else { "tab-pane hidden" },

                // ── AzureWebJobsStorage ───────────────────────────────────────
                // This setting controls where the Logic Apps runtime stores workflow
                // state. If it points to Azure instead of Azurite, ALL workflow runs
                // APIs return "not found" locally.
                div { class: "db-card", style: "margin-bottom:12px",
                    div { class: "db-card-header",
                        span { class: "db-card-name", "AzureWebJobsStorage" }
                        {
                            let v = webjobs_edit.read().clone();
                            let is_azurite = v == "UseDevelopmentStorage=true"
                                || v.contains("127.0.0.1:10000")
                                || v.contains("localhost:10000");
                            let is_azure   = !v.is_empty() && !is_azurite;
                            if is_azurite {
                                rsx! { span { class: "db-auth-badge", style: "color:var(--green);border-color:var(--green)", "🏠 Azurite" } }
                            } else if is_azure {
                                rsx! { span { class: "db-auth-badge", style: "color:var(--yellow);border-color:var(--yellow)", "⚠ Azure" } }
                            } else {
                                rsx! { span { class: "db-auth-badge db-badge-missing", "not set" } }
                            }
                        }
                    }
                    div { class: "db-field-row",
                        input {
                            class: "db-field-input",
                            placeholder: "UseDevelopmentStorage=true",
                            value: "{webjobs_edit.read()}",
                            oninput: move |e| webjobs_edit.set(e.value()),
                        }
                        button {
                            class: "btn btn-small",
                            style: "flex-shrink:0",
                            title: "Use Azurite local emulator",
                            onclick: move |_| webjobs_edit.set("UseDevelopmentStorage=true".into()),
                            "🏠 Use Azurite"
                        }
                    }
                    div { class: "db-msi-note",
                        "Must point to Azurite locally — if set to an Azure URL, "
                        em { "all" }
                        " workflow runs APIs return 'not found'."
                    }
                }

                // ── Azure Blob Connections (from connections.json) ────────────
                div { class: "db-section",
                    div { class: "db-section-title", style: "display:flex;align-items:center;gap:10px;",
                        span { "☁ Azure Blob Connections" }
                        {
                            let mode      = props.env_mode.clone();
                            let switching = *env_switching.read();
                            let dir_sw    = props.logic_apps_dir.clone();
                            let dir_sw2   = props.logic_apps_dir.clone();
                            let sub       = azure_sync::detect_subscription(&props.logic_apps_dir);
                            let going_local = !matches!(mode, EnvMode::Azure);
                            let (btn_label, btn_title) = if going_local {
                                ("🏠 All Local", "Set all blob endpoints to Azurite (local dev)")
                            } else {
                                ("☁ All Azure", "Set all blob endpoints to real Azure storage")
                            };
                            rsx! {
                                button {
                                    class: "btn btn-small btn-fetch",
                                    disabled: switching || props.blob_connections.is_empty(),
                                    title: "{btn_title}",
                                    onclick: move |_| {
                                        env_switching.set(true);
                                        env_pending_keys.set(vec![]);
                                        env_accounts.set(vec![]);
                                        let d  = dir_sw.clone();
                                        let d2 = dir_sw2.clone();
                                        let sub2 = sub.clone();
                                        spawn(async move {
                                            if going_local {
                                                let _ = tokio::task::spawn_blocking(move || env_mode::switch_to_local(&d)).await;
                                            } else {
                                                let result = tokio::task::spawn_blocking(move || env_mode::switch_to_azure(&d))
                                                    .await.unwrap_or(Err("task failed".into()));
                                                if let Ok(r) = result {
                                                    if !r.needs_fetch.is_empty() {
                                                        let accounts = tokio::task::spawn_blocking(move || {
                                                            azure_cli::list_storage_accounts(sub2.as_deref())
                                                        }).await.unwrap_or(Ok(vec![])).unwrap_or_default();
                                                        env_pending_keys.set(r.needs_fetch);
                                                        env_accounts.set(accounts);
                                                    }
                                                }
                                            }
                                            // Reload blob_edits from disk so inputs reflect new values
                                            let _ = tokio::task::spawn_blocking(move || {
                                                env_mode::detect_mode(&d2)
                                            }).await;
                                            env_switching.set(false);
                                            props.on_env_changed.call(());
                                        });
                                    },
                                    if switching { "…" } else { "{btn_label}" }
                                }
                                // Badge showing current mode
                                span {
                                    class: match mode {
                                        EnvMode::Local   => "db-auth-badge msi",
                                        EnvMode::Azure   => "db-auth-badge cs",
                                        EnvMode::Mixed   => "db-auth-badge db-badge-missing",
                                        EnvMode::Unknown => "db-auth-badge",
                                    },
                                    match mode {
                                        EnvMode::Local   => "🏠 local",
                                        EnvMode::Azure   => "☁ azure",
                                        EnvMode::Mixed   => "⚠ mixed",
                                        EnvMode::Unknown => "? unknown",
                                    }
                                }
                            }
                        }
                    }
                    // Picker: shown when switching to Azure reveals uncached endpoints
                    if !env_pending_keys.read().is_empty() && !env_accounts.read().is_empty() {
                        div { class: "env-picker",
                            span { class: "env-picker-title", "Pick an Azure storage account for each connection:" }
                            for key in env_pending_keys.read().clone() {
                                {
                                    let key2    = key.clone();
                                    let display = key.trim_end_matches("_blobStorageEndpoint").to_string();
                                    let dir_k   = props.logic_apps_dir.clone();
                                    let dir_k2  = props.logic_apps_dir.clone();
                                    rsx! {
                                        div { class: "env-picker-row",
                                            span { class: "env-picker-key", "{display}" }
                                            select {
                                                class: "env-picker-select",
                                                onchange: move |e| {
                                                    let endpoint = e.value();
                                                    let k  = key2.clone();
                                                    let d  = dir_k.clone();
                                                    let d2 = dir_k2.clone();
                                                    let k2       = k.clone();
                                                    let endpoint2 = endpoint.clone();
                                                    spawn(async move {
                                                        let _ = tokio::task::spawn_blocking(move || {
                                                            env_mode::apply_fetched_endpoint(&d, &k, &endpoint)
                                                        }).await;
                                                        blob_edits.write().insert(k2, endpoint2);
                                                        props.on_env_changed.call(());
                                                        let _ = d2;
                                                    });
                                                },
                                                option { value: "", "— pick —" }
                                                for (name, ep) in env_accounts.read().clone() {
                                                    option { value: "{ep}", "{name}" }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if props.blob_connections.is_empty() {
                        div { class: "empty-state", "No AzureBlob connections found in connections.json" }
                    } else {
                        for conn in props.blob_connections.clone() {
                            {
                                let key  = conn.endpoint_key.clone();
                                let key2 = key.clone();
                                let key3 = key.clone();
                                let current = blob_edits.read().get(&key).cloned().unwrap_or_default();
                                let is_configured = !current.is_empty();
                                let azurite_up = props.azurite_running;
                                rsx! {
                                    div { class: "db-card",
                                        div { class: "db-card-header",
                                            span { class: "db-card-name", "{conn.display_name}" }
                                            span {
                                                class: if is_configured { "db-auth-badge msi" } else { "db-auth-badge db-badge-missing" },
                                                if is_configured { "✅ configured" } else { "⚠ missing" }
                                            }
                                            if !is_configured {
                                                div { style: "margin-left:auto;display:flex;gap:6px;",
                                                    button {
                                                        class: "btn btn-small",
                                                        title: "Use local Azurite storage (http://127.0.0.1:10000/devstoreaccount1)",
                                                        onclick: move |_| {
                                                            blob_edits.write().insert(key3.clone(), env_mode::AZURITE_BLOB.to_string());
                                                        },
                                                        if azurite_up { "🏠 Use Azurite" } else { "🏠 Use Azurite (start it first)" }
                                                    }
                                                }
                                            }
                                        }
                                        div { class: "db-field-row",
                                            label { class: "db-field-label", "Endpoint" }
                                            input {
                                                class: "db-field-input",
                                                placeholder: "https://<account>.blob.core.windows.net",
                                                value: "{current}",
                                                oninput: move |e| { blob_edits.write().insert(key.clone(), e.value()); },
                                            }
                                        }
                                        div { class: "db-msi-note",
                                            "appsetting: " code { "{key2}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                div { class: "db-section",
                    // ── Header row ───────────────────────────────────────────
                    div { class: "db-section-header",
                        div { class: "db-section-title-row",
                            span { class: "db-section-title", "🗄 Local Blob Storage" }
                            div { class: "db-section-title-right",
                                span { class: "db-az-badge local", "via Azurite" }
                                if *blob_loading.read() {
                                    span { class: "db-fetching", "loading…" }
                                } else if props.azurite_running {
                                    button {
                                        class: "btn btn-small",
                                        onclick: move |_| {
                                            blob_loading.set(true);
                                            blob_clear_confirm.set(None);
                                            status.set(None);
                                            spawn(async move {
                                                match tokio::task::spawn_blocking(blob_fetch_all).await {
                                                    Ok(Ok(list)) => { blob_containers.set(Some(list)); }
                                                    Ok(Err(e))   => {
                                                        status.set(Some((format!("Refresh failed: {}", e), true)));
                                                        blob_containers.set(Some(vec![]));
                                                    }
                                                    Err(e) => {
                                                        status.set(Some((format!("Task panicked: {}", e), true)));
                                                    }
                                                }
                                                blob_loading.set(false);
                                            });
                                        },
                                        "⟳ Refresh"
                                    }
                                }
                            }
                        }
                        span { class: "db-section-sub", "http://127.0.0.1:10000/devstoreaccount1" }
                    }

                    if !props.azurite_running {
                        div { class: "blob-offline", "⚠ Start Azurite to manage local storage" }
                    } else {
                        // ── New container form ────────────────────────────────
                        div { class: "blob-new-row",
                            input {
                                r#type: "text",
                                placeholder: "container  or  container/folder/subfolder",
                                value: "{new_container_name.read()}",
                                oninput: move |e| new_container_name.set(e.value()),
                                onkeydown: move |e| {
                                    if e.key() == Key::Enter {
                                        let name = new_container_name.read().trim().to_string();
                                        if !name.is_empty() && !*blob_creating.read() {
                                            blob_creating.set(true);
                                            status.set(None);
                                            spawn(async move {
                                                if let Err(msg) = do_create_container_or_folder(name).await {
                                                    status.set(Some((msg, true)));
                                                    blob_creating.set(false);
                                                    return;
                                                }
                                                match tokio::task::spawn_blocking(blob_fetch_all).await {
                                                    Ok(Ok(list)) => { blob_containers.set(Some(list)); }
                                                    Ok(Err(e)) => { status.set(Some((format!("List failed: {}", e), true))); }
                                                    Err(_) => {}
                                                }
                                                new_container_name.set(String::new());
                                                blob_creating.set(false);
                                            });
                                        }
                                    }
                                },
                            }
                            button {
                                class: "btn btn-small btn-run",
                                disabled: new_container_name.read().trim().is_empty() || *blob_creating.read(),
                                onclick: move |_| {
                                    let name = new_container_name.read().trim().to_string();
                                    if !name.is_empty() && !*blob_creating.read() {
                                        blob_creating.set(true);
                                        status.set(None);
                                        spawn(async move {
                                            if let Err(msg) = do_create_container_or_folder(name).await {
                                                status.set(Some((msg, true)));
                                                blob_creating.set(false);
                                                return;
                                            }
                                            match tokio::task::spawn_blocking(blob_fetch_all).await {
                                                Ok(Ok(list)) => { blob_containers.set(Some(list)); }
                                                Ok(Err(e)) => { status.set(Some((format!("List failed: {}", e), true))); }
                                                Err(_) => {}
                                            }
                                            new_container_name.set(String::new());
                                            blob_creating.set(false);
                                        });
                                    }
                                },
                                if *blob_creating.read() { "Creating…" } else { "+ Create" }
                            }
                        }

                        // ── Container tree ────────────────────────────────────
                        div { class: "blob-tree",
                            if let Some(containers) = blob_containers.read().clone() {
                                if containers.is_empty() {
                                    div { class: "blob-empty", "No containers — create one above or click ⟳ Refresh" }
                                }
                                for (cname, blobs) in containers {
                                    {
                                        let cname2    = cname.clone();
                                        let cname3    = cname.clone();
                                        let cname4    = cname.clone();
                                        let cname_exp = cname.clone();
                                        let is_expanded  = blob_expanded.read().contains(&cname);
                                        let is_clearing  = blob_clearing.read().contains(&cname);
                                        let is_uploading = blob_uploading.read().contains(&cname);
                                        let confirm_pending = blob_clear_confirm.read().as_deref() == Some(&cname);
                                        let blob_count_label = if blobs.is_empty() {
                                            "empty".to_string()
                                        } else {
                                            let s = if blobs.len() == 1 { "" } else { "s" };
                                            format!("{} blob{}", blobs.len(), s)
                                        };
                                        let expand_icon = if is_expanded { "▼" } else { "▶" };
                                        rsx! {
                                            // Container row
                                            div { class: "blob-container-row",
                                                // Expand / collapse toggle
                                                button {
                                                    class: "blob-expand-btn",
                                                    title: if is_expanded { "Collapse" } else { "Expand blob list" },
                                                    onclick: move |_| {
                                                        let mut exp = blob_expanded.write();
                                                        if exp.contains(&cname_exp) {
                                                            exp.remove(&cname_exp);
                                                        } else {
                                                            exp.insert(cname_exp.clone());
                                                        }
                                                    },
                                                    "{expand_icon}"
                                                }
                                                div { class: "blob-container-info",
                                                    span { class: "blob-container-name", "{cname}" }
                                                    span { class: "blob-count", "{blob_count_label}" }
                                                }
                                                div { class: "blob-container-actions",
                                                    // Upload button
                                                    button {
                                                        class: "btn btn-small",
                                                        disabled: is_uploading || is_clearing,
                                                        title: "Upload a file into this container",
                                                        onclick: move |_| {
                                                            let c = cname3.clone();
                                                            blob_uploading.write().insert(c.clone());
                                                            spawn(async move {
                                                                if let Some(file) = rfd::AsyncFileDialog::new().pick_file().await {
                                                                    let path = file.path().to_string_lossy().to_string();
                                                                    let name = file.file_name();
                                                                    let c2   = c.clone();
                                                                    let _ = tokio::task::spawn_blocking(move || {
                                                                        azurite_client::upload_blob(&c2, &path, &name)
                                                                    }).await;
                                                                    // Auto-expand + refresh this container
                                                                    blob_expanded.write().insert(c.clone());
                                                                    let c3 = c.clone();
                                                                    if let Ok(Ok(updated)) = tokio::task::spawn_blocking(move || {
                                                                        azurite_client::list_blobs(&c3)
                                                                    }).await {
                                                                        if let Some(ref mut list) = *blob_containers.write() {
                                                                            if let Some(entry) = list.iter_mut().find(|(n, _)| n == &c) {
                                                                                entry.1 = updated;
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                                blob_uploading.write().remove(&c);
                                                            });
                                                        },
                                                        if is_uploading { "↑ …" } else { "↑ Upload" }
                                                    }
                                                    // Clear button (two-step confirmation)
                                                    if confirm_pending {
                                                        button {
                                                            class: "btn btn-small btn-danger",
                                                            disabled: is_clearing,
                                                            title: "Confirm — delete ALL blobs in this container",
                                                            onclick: move |_| {
                                                                let c = cname2.clone();
                                                                blob_clear_confirm.set(None);
                                                                blob_clearing.write().insert(c.clone());
                                                                spawn(async move {
                                                                    let c2 = c.clone();
                                                                    let _ = tokio::task::spawn_blocking(move || {
                                                                        azurite_client::clear_container(&c2)
                                                                    }).await;
                                                                    if let Some(ref mut list) = *blob_containers.write() {
                                                                        if let Some(entry) = list.iter_mut().find(|(n, _)| n == &c) {
                                                                            entry.1.clear();
                                                                        }
                                                                    }
                                                                    blob_clearing.write().remove(&c);
                                                                });
                                                            },
                                                            if is_clearing { "🗑 …" } else { "🗑 Confirm?" }
                                                        }
                                                        button {
                                                            class: "btn btn-small",
                                                            onclick: move |_| blob_clear_confirm.set(None),
                                                            "Cancel"
                                                        }
                                                    } else {
                                                        button {
                                                            class: "btn btn-small",
                                                            disabled: is_clearing || is_uploading,
                                                            title: "Delete all blobs in this container",
                                                            onclick: move |_| {
                                                                blob_clear_confirm.set(Some(cname4.clone()));
                                                            },
                                                            if is_clearing { "🗑 …" } else { "🗑 Clear" }
                                                        }
                                                    }
                                                }
                                            }
                                            // ── Expanded blob list ────────────────
                                            if is_expanded {
                                                if blobs.is_empty() {
                                                    div { class: "blob-list",
                                                        div { class: "blob-empty", style: "padding:6px 0",
                                                            "Container is empty"
                                                        }
                                                    }
                                                } else {
                                                    div { class: "blob-list",
                                                        for b in &blobs {
                                                            {
                                                                // Blobs ending with /.keep are virtual folder markers
                                                                let is_folder = b.name.ends_with("/.keep");
                                                                let (icon, display, full) = if is_folder {
                                                                    let folder_path = b.name
                                                                        .strip_suffix("/.keep")
                                                                        .unwrap_or(&b.name);
                                                                    let folder_name = folder_path
                                                                        .rsplit('/')
                                                                        .next()
                                                                        .unwrap_or(folder_path);
                                                                    ("📁", folder_name.to_string(), folder_path.to_string())
                                                                } else {
                                                                    let name = b.name
                                                                        .rsplit('/')
                                                                        .next()
                                                                        .unwrap_or(&b.name)
                                                                        .to_string();
                                                                    ("📄", name, b.name.clone())
                                                                };
                                                                let size_str = if is_folder {
                                                                    String::new()
                                                                } else {
                                                                    let kb = b.size as f64 / 1024.0;
                                                                    if b.size < 1024 {
                                                                        format!("{} B", b.size)
                                                                    } else if kb < 1024.0 {
                                                                        format!("{:.1} KB", kb)
                                                                    } else {
                                                                        format!("{:.1} MB", kb / 1024.0)
                                                                    }
                                                                };
                                                                // Files inside a virtual folder get an extra indent level
                                                                let is_nested = !is_folder && b.name.contains('/');
                                                                let row_cls = if is_folder {
                                                                    "blob-row blob-folder-row"
                                                                } else if is_nested {
                                                                    "blob-row blob-nested-row"
                                                                } else {
                                                                    "blob-row"
                                                                };
                                                                // For folder rows: composite upload key = "container/folder"
                                                                let upload_key = format!("{}/{}", cname, full);
                                                                let is_folder_uploading = blob_uploading.read().contains(&upload_key);
                                                                let folder_prefix = full.clone();
                                                                let ct_for_folder = cname.clone();
                                                                rsx! {
                                                                    div { class: "{row_cls}", title: "{full}",
                                                                        span { class: "blob-row-icon", "{icon}" }
                                                                        span { class: "blob-name", "{display}" }
                                                                        span { class: "blob-size", "{size_str}" }
                                                                        if is_folder {
                                                                            button {
                                                                                class: "btn btn-small blob-folder-upload-btn",
                                                                                disabled: is_folder_uploading,
                                                                                title: "Upload a file into this folder",
                                                                                onclick: move |_| {
                                                                                    let uk  = upload_key.clone();
                                                                                    let ct  = ct_for_folder.clone();
                                                                                    let pfx = folder_prefix.clone();
                                                                                    blob_uploading.write().insert(uk.clone());
                                                                                    spawn(async move {
                                                                                        if let Some(file) = rfd::AsyncFileDialog::new().pick_file().await {
                                                                                            let path = file.path().to_string_lossy().to_string();
                                                                                            let blob_name = format!("{}/{}", pfx, file.file_name());
                                                                                            let ct2 = ct.clone();
                                                                                            let _ = tokio::task::spawn_blocking(move || {
                                                                                                azurite_client::upload_blob(&ct2, &path, &blob_name)
                                                                                            }).await;
                                                                                            blob_expanded.write().insert(ct.clone());
                                                                                            let ct3 = ct.clone();
                                                                                            if let Ok(Ok(updated)) = tokio::task::spawn_blocking(move || {
                                                                                                azurite_client::list_blobs(&ct3)
                                                                                            }).await {
                                                                                                if let Some(ref mut list) = *blob_containers.write() {
                                                                                                    if let Some(entry) = list.iter_mut().find(|(n, _)| n == &ct) {
                                                                                                        entry.1 = updated;
                                                                                                    }
                                                                                                }
                                                                                            }
                                                                                        }
                                                                                        blob_uploading.write().remove(&uk);
                                                                                    });
                                                                                },
                                                                                if is_folder_uploading { "↑ …" } else { "↑ Upload" }
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
                            } else {
                                div { class: "blob-empty", "Click ⟳ Refresh to list containers" }
                            }
                        }
                    }
                }
                } // end blob tab-pane

                // ════════════════════════════════════════════════════════════
                // SFTP TAB
                // ════════════════════════════════════════════════════════════
                div {
                    class: if *active_tab.read() == "sftp" { "tab-pane" } else { "tab-pane hidden" },
                    SftpTab { connections: props.sftp_connections.clone(), logic_apps_dir: props.logic_apps_dir.clone() }
                }
            }
        }
    }
}

// ── SFTP Tab component ────────────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct SftpTabProps {
    connections:    Vec<SftpConnection>,
    logic_apps_dir: String,
}

#[component]
fn SftpTab(props: SftpTabProps) -> Element {
    let mut test_results: Signal<std::collections::HashMap<String, SftpTestResult>> =
        use_signal(std::collections::HashMap::new);
    let mut testing: Signal<std::collections::HashSet<String>> =
        use_signal(std::collections::HashSet::new);

    if props.connections.is_empty() {
        return rsx! {
            div { class: "empty-state", "No SFTP connections found in connections.json" }
        };
    }

    rsx! {
        div { class: "db-section",
            div { class: "db-section-title", "📡 SFTP Connections" }

            for conn in props.connections.clone() {
                {
                    let name    = conn.connection_name.clone();
                    let is_testing = testing.read().contains(&name);
                    let result  = test_results.read().get(&name).cloned();

                    let status_badge = match &result {
                        None                          => rsx! { span { class: "sftp-badge unchecked", "—" } },
                        Some(SftpTestResult::Ok)      => rsx! { span { class: "sftp-badge ok",        "✅ Reachable" } },
                        Some(SftpTestResult::AuthFailed) => rsx! { span { class: "sftp-badge warn",   "⚠ Auth — host reachable, password auth will work at runtime" } },
                        Some(SftpTestResult::Unreachable(e)) => rsx! { span { class: "sftp-badge error", "❌ {e}" } },
                        Some(SftpTestResult::NotConfigured)  => rsx! { span { class: "sftp-badge error", "❌ Not configured" } },
                    };

                    rsx! {
                        div { class: "sftp-row",
                            div { class: "sftp-row-header",
                                span { class: "sftp-name", "{conn.display_name}" }
                                if is_testing {
                                    span { class: "sftp-badge unchecked", "Testing…" }
                                } else {
                                    {status_badge}
                                }
                                button {
                                    class: "btn btn-small",
                                    disabled: is_testing || !conn.configured,
                                    title: if conn.configured { "Test SSH connectivity" } else { "Host or username not configured" },
                                    onclick: {
                                        let host = conn.host.clone();
                                        let user = conn.user.clone();
                                        let key  = name.clone();
                                        move |_| {
                                            testing.write().insert(key.clone());
                                            let h = host.clone();
                                            let u = user.clone();
                                            let k = key.clone();
                                            spawn(async move {
                                                let res = tokio::task::spawn_blocking(move || {
                                                    sftp_check::test_sftp_connection(&h, &u)
                                                }).await.unwrap_or(SftpTestResult::Unreachable("task failed".into()));
                                                testing.write().remove(&k);
                                                test_results.write().insert(k, res);
                                            });
                                        }
                                    },
                                    "Test"
                                }
                            }
                            div { class: "sftp-details",
                                span { class: "sftp-field",
                                    span { class: "sftp-label", "Host" }
                                    if conn.host.is_empty() {
                                        span { class: "sftp-value missing", "not set ({conn.host_key})" }
                                    } else {
                                        span { class: "sftp-value", "{conn.host}" }
                                    }
                                }
                                span { class: "sftp-field",
                                    span { class: "sftp-label", "User" }
                                    if conn.user.is_empty() {
                                        span { class: "sftp-value missing", "not set ({conn.user_key})" }
                                    } else {
                                        span { class: "sftp-value", "{conn.user}" }
                                    }
                                }
                                span { class: "sftp-field",
                                    span { class: "sftp-label", "Password" }
                                    span { class: "sftp-value", "••••• ({conn.pass_key})" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
