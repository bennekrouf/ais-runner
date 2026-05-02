use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};
use crate::services::{
    azure_cli::{self, SbQueueStats},
    sb_check::{self, SbQueueInfo},
    sql_check::{self, SqlAuthType, SqlConnection, TestResult},
    settings_file,
};

fn do_fetch_conn_str(
    ns:             String,
    rg_cached:      Option<String>,
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
            tokio::task::spawn_blocking(move || azure_cli::sb_find_rg(&ns2))
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
    pub on_close:          EventHandler<()>,
    pub on_saved:          EventHandler<()>,
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
    let mut sql_test_results: Signal<HashMap<String, TestResult>> = use_signal(HashMap::new);
    let mut sql_testing:      Signal<HashSet<String>>             = use_signal(HashSet::new);

    // ── Service Bus signals ──────────────────────────────────────────────────
    let mut sb_tcp_result: Signal<Option<TestResult>> = use_signal(|| None);
    let mut sb_tcp_testing: Signal<bool>              = use_signal(|| false);
    let mut sb_rg:          Signal<Option<String>>    = use_signal(|| None);
    let mut sb_stats:       Signal<HashMap<String, SbQueueStats>> = use_signal(HashMap::new);
    let mut sb_fetching:    Signal<HashSet<String>>   = use_signal(HashSet::new);
    let mut sb_send_open:   Signal<HashSet<String>>   = use_signal(HashSet::new);
    let mut sb_send_bodies: Signal<HashMap<String, String>> = use_signal(HashMap::new);

    // Namespace editing
    let mut sb_ns_edit: Signal<String> =
        use_signal(|| props.sb_namespace.clone());
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

    // ── Shared status bar ────────────────────────────────────────────────────
    let mut status: Signal<Option<(String, bool)>> = use_signal(|| None);

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
                            props.on_saved.call(());
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
                    button { class: "btn btn-run btn-small", onclick: on_save, "💾 Save" }
                    button { class: "btn-icon", onclick: move |_| props.on_close.call(()), "×" }
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
                }

                // ════════════════════════════════════════════════════════════
                // SERVICE BUS SECTION
                // ════════════════════════════════════════════════════════════
                div { class: "db-section-title", style: "margin-top:20px; display:flex; align-items:center; gap:10px;",
                    span { "📨 Service Bus" }
                    {
                        let is_loading = *az_queues_loading.read();
                        let ns = sb_ns_edit.read().clone();
                        let already = az_queues.read().is_some();
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
                                    spawn(async move {
                                        let rg = if let Some(r) = rg_cached {
                                            Ok(r)
                                        } else {
                                            let ns3 = ns2.clone();
                                            tokio::task::spawn_blocking(move || azure_cli::sb_find_rg(&ns3))
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

                if props.sb_namespace.is_empty() {
                    div { class: "empty-state", "No Service Bus connection found in connections.json" }
                } else {
                    // Namespace card — editable + browse
                    {
                        let tcp_res = sb_tcp_result.read().clone();
                        let is_tcp_testing = *sb_tcp_testing.read();
                        let is_loading = *sb_ns_loading.read();
                        let ns_list = sb_ns_list.read().clone();
                        let current_edit = sb_ns_edit.read().clone();
                        let has_key = props.sb_namespace_key.is_some();
                        let uses_conn_str = props.sb_conn_str.is_some();
                        // FQDN is editable only when it maps directly to a setting key (MSI mode)
                        let fqdn_editable = has_key && !uses_conn_str;
                        let auth_badge: &str = if uses_conn_str { "ConnStr" } else { "MSI" };
                        let auth_class: &str = if uses_conn_str { "db-auth-badge cs" } else { "db-auth-badge msi" };
                        let fqdn_title: &str = if fqdn_editable {
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
                                    }
                                }

                                // Namespace combobox — click to open list, pick to close
                                div { class: "db-field-row",
                                    label { class: "db-field-label", "Namespace" }
                                    div { class: "db-ns-combobox",
                                        // The clickable field
                                        div {
                                            class: if is_loading { "db-ns-field loading" } else { "db-ns-field" },
                                            title: fqdn_title,
                                            onclick: move |_| {
                                                if ns_list.is_empty() && !is_loading {
                                                    sb_ns_loading.set(true);
                                                    spawn(async move {
                                                        match tokio::task::spawn_blocking(azure_cli::sb_list_namespaces).await {
                                                            Ok(Ok(list)) => { sb_ns_list.set(list); }
                                                            Ok(Err(e)) => {
                                                                status.set(Some((format!("az error: {:?}", e), true)));
                                                            }
                                                            Err(_) => {
                                                                status.set(Some(("Could not list namespaces — az login required".into(), true)));
                                                            }
                                                        }
                                                        sb_ns_loading.set(false);
                                                    });
                                                } else {
                                                    // Toggle — close if already open
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
                                        // Dropdown list
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
                                                                            sb_ns_list.write().clear(); // close
                                                                            if uses_cs {
                                                                                sb_rg.set(Some(rg2.clone()));
                                                                                do_fetch_conn_str(
                                                                                    fqdn3.clone(),
                                                                                    Some(rg2.clone()),
                                                                                    sb_rg, sb_cs_edit,
                                                                                    sb_cs_fetching, status,
                                                                                );
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

                                // Connection string status row
                                if let Some((ref cs_key_label, _)) = props.sb_conn_str {
                                    {
                                        let cs_key_label   = cs_key_label.clone();
                                        let is_cs_fetching = *sb_cs_fetching.read();
                                        let cs_value       = sb_cs_edit.read().clone();
                                        let cs_status: &str = if is_cs_fetching {
                                            "Fetching…"
                                        } else if !cs_value.is_empty() {
                                            "✅ Set"
                                        } else {
                                            "⚠ Not set — pick a namespace above"
                                        };
                                        let cs_status_class: &str = if is_cs_fetching || cs_value.is_empty() {
                                            "db-cs-status warn"
                                        } else {
                                            "db-cs-status ok"
                                        };
                                        rsx! {
                                            div { class: "db-field-row", style: "margin-top:6px",
                                                label {
                                                    class: "db-field-label",
                                                    title: "Setting: {cs_key_label}",
                                                    "Conn string"
                                                }
                                                span { class: "{cs_status_class}", "{cs_status}" }
                                            }
                                        }
                                    }
                                }

                                if !has_key && !uses_conn_str {
                                    div { class: "db-msi-note",
                                        "⚠ Namespace is hardcoded — add "
                                        code { "\"fullyQualifiedNamespace\": \"@appsetting('serviceBus_fullyQualifiedNamespace')\"" }
                                        " to connections.json to make it switchable."
                                    }
                                }

                                if let Some(ref r) = tcp_res {
                                    if !r.reachable {
                                        if let Some(ref err) = r.error {
                                            div { class: "db-test-error-detail",
                                                "{err} — try port 443 (AMQP over WebSockets)."
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
                                                            az_creating.write().insert(qn.clone());
                                                            spawn(async move {
                                                                let rg = if let Some(r) = rg_now { Ok(r) } else {
                                                                    let ns5 = ns4.clone();
                                                                    tokio::task::spawn_blocking(move || azure_cli::sb_find_rg(&ns5))
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
                                                    sb_fetching.write().insert(qn.clone());
                                                    spawn(async move {
                                                        // Resolve RG once
                                                        let rg = if let Some(r) = rg_now {
                                                            r
                                                        } else {
                                                            let ns4 = ns3.clone();
                                                            match tokio::task::spawn_blocking(move || {
                                                                azure_cli::sb_find_rg(&ns4)
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
                }
            }
        }
    }
}
