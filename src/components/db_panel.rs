use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};
use crate::services::{
    azure_cli::{self, SbQueueStats},
    sb_check::{self, SbQueueInfo},
    sql_check::{self, SqlAuthType, SqlConnection, TestResult},
    settings_file,
};

#[derive(Props, Clone, PartialEq)]
pub struct DbPanelProps {
    pub logic_apps_dir: String,
    pub connections:    Vec<SqlConnection>,
    pub sb_namespace:   String,
    pub sb_queues:      Vec<SbQueueInfo>,
    pub on_close:       EventHandler<()>,
    pub on_saved:       EventHandler<()>,
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

    // ── Shared status bar ────────────────────────────────────────────────────
    let mut status: Signal<Option<(String, bool)>> = use_signal(|| None);

    let dir = props.logic_apps_dir.clone();

    // ── Save SQL settings ───────────────────────────────────────────────────
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
                div { class: "db-section-title", style: "margin-top:20px", "📨 Service Bus" }

                if props.sb_namespace.is_empty() {
                    div { class: "empty-state", "No Service Bus connection found in connections.json" }
                } else {
                    // Namespace TCP card
                    {
                        let ns = props.sb_namespace.clone();
                        let ns2 = ns.clone();
                        let tcp_res = sb_tcp_result.read().clone();
                        let is_tcp_testing = *sb_tcp_testing.read();
                        rsx! {
                            div { class: "db-card",
                                div { class: "db-card-header",
                                    span { class: "db-card-name", "{ns}" }
                                    span { class: "db-auth-badge msi", "MSI" }
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
                                            disabled: is_tcp_testing,
                                            onclick: move |_| {
                                                let ns3 = ns2.clone();
                                                sb_tcp_testing.set(true);
                                                spawn(async move {
                                                    let result = tokio::task::spawn_blocking(move || {
                                                        sb_check::test_sb_tcp(&ns3)
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
                                if let Some(ref r) = tcp_res {
                                    if !r.reachable {
                                        if let Some(ref err) = r.error {
                                            div { class: "db-test-error-detail",
                                                "{err} — port 443 may also work; AMQP over WebSockets uses 443."
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Queue cards
                    for q in props.sb_queues.clone() {
                        {
                            let queue_name  = q.queue.clone();
                            let queue_name2 = queue_name.clone();
                            let queue_name3 = queue_name.clone();
                            let queue_name4 = queue_name.clone();
                            let ns = props.sb_namespace.clone();
                            let ns2 = ns.clone();
                            let is_fetching = sb_fetching.read().contains(&queue_name);
                            let stats_opt   = sb_stats.read().get(&queue_name).cloned();
                            let is_send_open = sb_send_open.read().contains(&queue_name);
                            let send_body   = sb_send_bodies.read().get(&queue_name).cloned().unwrap_or_default();

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
                                                                                .body(body)
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
                }
            }
        }
    }
}
