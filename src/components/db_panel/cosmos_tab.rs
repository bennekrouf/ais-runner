use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};
use crate::services::cosmos_check::{self, CosmosConnection};
use crate::services::cosmos_query;

/// Per-connection query console — same UI as the ad-hoc one but the endpoint
/// and key come from the connection's resolved values. We hold them as
/// independent signals so the user can override per-query (e.g. point at the
/// emulator for a quick check) without dirtying the saved settings.
#[component]
fn CosmosConnQueryConsole(endpoint_seed: String, account_key_seed: String, console_id: String) -> Element {
    let endpoint:    Signal<String> = use_signal(|| endpoint_seed.clone());
    let account_key: Signal<String> = use_signal(|| account_key_seed.clone());
    rsx! {
        CosmosQueryConsole { endpoint, account_key, console_id }
    }
}

/// Reusable query-console UI used by both the ad-hoc Emulator card and each
/// detected connection card. Owns its own state: db list, container list,
/// query input, results. `console_id` namespaces the signals so two consoles
/// in the same view don't collide.
#[component]
fn CosmosQueryConsole(
    endpoint: Signal<String>,
    account_key: Signal<String>,
    console_id: String,
) -> Element {
    let mut dbs:        Signal<Vec<String>> = use_signal(Vec::new);
    let mut dbs_busy:   Signal<bool>        = use_signal(|| false);
    let mut dbs_status: Signal<Option<String>> = use_signal(|| None);
    let mut db_sel:     Signal<String>      = use_signal(String::new);

    let mut colls:      Signal<Vec<String>> = use_signal(Vec::new);
    let mut colls_busy: Signal<bool>        = use_signal(|| false);
    let mut coll_sel:   Signal<String>      = use_signal(String::new);

    let mut query:      Signal<String>      = use_signal(|| "SELECT TOP 10 * FROM c".to_string());
    let mut q_busy:     Signal<bool>        = use_signal(|| false);
    let mut q_result:   Signal<Option<Result<String, String>>> = use_signal(|| None);

    // Treat a value as "really selected" only when it matches a known entry
    // in the loaded list. Webview select onchange has been observed to fire
    // with the option's text content ("— select —") instead of its empty
    // value attribute on some platforms — this guard catches that case so a
    // ghost selection can't slip through to the REST call.
    let normalize_sel = |raw: &str, options: &[String]| -> String {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('—') { return String::new(); }
        if options.iter().any(|o| o == trimmed) { trimmed.to_string() } else { String::new() }
    };

    let load_dbs = move |_| {
        let ep = endpoint.read().clone();
        let key = account_key.read().clone();
        dbs_busy.set(true);
        dbs_status.set(None);
        spawn(async move {
            match cosmos_query::list_databases(&ep, &key).await {
                Ok(list) => {
                    let n = list.len();
                    dbs.set(list);
                    // Reset any stale selection so the dropdown shows the
                    // placeholder until the user picks from the fresh list.
                    db_sel.set(String::new());
                    coll_sel.set(String::new());
                    colls.set(Vec::new());
                    dbs_status.set(Some(if n == 0 {
                        "⚠ Connected, but the account has no databases.".to_string()
                    } else {
                        format!("✅ Loaded {n} database(s).")
                    }));
                }
                Err(e) => {
                    dbs_status.set(Some(format!("❌ {e}")));
                    q_result.set(Some(Err(format!("List databases failed: {e}"))));
                }
            }
            dbs_busy.set(false);
        });
    };

    let load_colls = move |_| {
        let ep   = endpoint.read().clone();
        let key  = account_key.read().clone();
        let db_list = dbs.read().clone();
        let db   = normalize_sel(&db_sel.read(), &db_list);
        if db.is_empty() {
            q_result.set(Some(Err(
                "Pick a database from the dropdown before loading containers.".into(),
            )));
            return;
        }
        colls_busy.set(true);
        spawn(async move {
            match cosmos_query::list_containers(&ep, &key, &db).await {
                Ok(list) => { colls.set(list); }
                Err(e)   => { q_result.set(Some(Err(format!("List containers failed: {e}")))); }
            }
            colls_busy.set(false);
        });
    };

    let run_q = move |_| {
        let ep      = endpoint.read().clone();
        let key     = account_key.read().clone();
        let db_list = dbs.read().clone();
        let cl_list = colls.read().clone();
        let db   = normalize_sel(&db_sel.read(),  &db_list);
        let coll = normalize_sel(&coll_sel.read(), &cl_list);
        let q    = query.read().clone();
        if db.is_empty() || coll.is_empty() || q.trim().is_empty() {
            q_result.set(Some(Err(
                "Pick a database and container, and enter a query, before running.".into(),
            )));
            return;
        }
        q_busy.set(true);
        q_result.set(None);
        spawn(async move {
            let r = cosmos_query::run_query(&ep, &key, &db, &coll, &q).await
                .and_then(|v| serde_json::to_string_pretty(&v).map_err(|e| e.to_string()));
            q_result.set(Some(r));
            q_busy.set(false);
        });
    };

    let _ = console_id; // currently only used for visual diff in the DOM tree

    rsx! {
        div { class: "db-card-header", style: "margin-top:12px;",
            span { class: "db-card-name", "Query Console" }
        }

        // ── Database picker ─────────────────────────────────────────────
        div { class: "db-field-row",
            label { class: "db-field-label", "Database" }
            select {
                class: "db-field-input",
                value: "{db_sel.read()}",
                onchange: move |e| {
                    // Normalize against the loaded list before storing so the
                    // placeholder option (or a webview-quirk text bubble-up)
                    // never lands in db_sel as a literal "— select —".
                    let raw = e.value();
                    let dbs_list = dbs.read().clone();
                    let cleaned = if raw.trim().is_empty() || raw.trim().starts_with('—')
                        || !dbs_list.iter().any(|o| o == raw.trim())
                    { String::new() } else { raw.trim().to_string() };
                    db_sel.set(cleaned);
                    colls.set(Vec::new());
                    coll_sel.set(String::new());
                },
                // `disabled` keeps the placeholder visible as a label but
                // prevents the user from re-selecting it after loading the
                // real list, and stops some webview builds from re-emitting
                // its textContent as the active value.
                option { value: "", disabled: true, "— select —" }
                for d in dbs.read().iter() {
                    option { value: "{d}", "{d}" }
                }
            }
            button {
                class: "btn btn-small",
                style: "flex-shrink:0;",
                disabled: *dbs_busy.read(),
                onclick: load_dbs,
                if *dbs_busy.read() { "…" } else { "↻ Load" }
            }
        }
        if let Some(status) = dbs_status.read().clone() {
            div {
                style: "font-size:11px; margin:2px 0 6px 110px; opacity:0.85;",
                "{status}"
            }
        }

        // ── Container picker ────────────────────────────────────────────
        div { class: "db-field-row",
            label { class: "db-field-label", "Container" }
            select {
                class: "db-field-input",
                value: "{coll_sel.read()}",
                onchange: move |e| {
                    let raw = e.value();
                    let cl = colls.read().clone();
                    let cleaned = if raw.trim().is_empty() || raw.trim().starts_with('—')
                        || !cl.iter().any(|o| o == raw.trim())
                    { String::new() } else { raw.trim().to_string() };
                    coll_sel.set(cleaned);
                },
                option { value: "", disabled: true, "— select —" }
                for c in colls.read().iter() {
                    option { value: "{c}", "{c}" }
                }
            }
            button {
                class: "btn btn-small",
                style: "flex-shrink:0;",
                disabled: *colls_busy.read() || db_sel.read().is_empty(),
                onclick: load_colls,
                if *colls_busy.read() { "…" } else { "↻ Load" }
            }
        }

        // ── Query input ─────────────────────────────────────────────────
        div { class: "db-field-row", style: "align-items:flex-start;",
            label { class: "db-field-label", style: "padding-top:6px;", "Query" }
            textarea {
                class: "db-field-textarea",
                style: "font-family:monospace; font-size:12px; min-height:80px;",
                placeholder: "SELECT * FROM c WHERE c.id = '...'",
                value: "{query.read()}",
                oninput: move |e| { query.set(e.value()); q_result.set(None); },
            }
        }
        div { style: "display:flex; justify-content:flex-end; margin-top:4px;",
            button {
                class: "btn btn-small btn-fetch",
                disabled: *q_busy.read()
                    || db_sel.read().is_empty()
                    || coll_sel.read().is_empty()
                    || query.read().trim().is_empty(),
                onclick: run_q,
                if *q_busy.read() { "Running…" } else { "▶ Run Query" }
            }
        }

        {
            let r = q_result.read();
            match r.as_ref() {
                Some(Ok(text)) => {
                    let body = text.clone();
                    rsx! {
                        pre {
                            class: "db-test-ok",
                            style: "font-size:11px; white-space:pre-wrap; max-height:300px; overflow:auto; margin:4px 0 0;",
                            "{body}"
                        }
                    }
                }
                Some(Err(e)) => {
                    let body = e.clone();
                    rsx! {
                        pre {
                            class: "db-test-error-detail",
                            style: "font-size:11px; white-space:pre-wrap; max-height:300px; overflow:auto; margin:4px 0 0;",
                            "{body}"
                        }
                    }
                }
                None => rsx! {},
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
pub struct CosmosTabProps {
    pub cosmos_connections: Vec<CosmosConnection>,
    pub cosmos_edits:       Signal<HashMap<String, String>>,
    pub status:             Signal<Option<(String, bool)>>,
}

#[component]
pub fn CosmosTab(props: CosmosTabProps) -> Element {
    let mut cosmos_test_results: Signal<HashMap<String, Result<u64, String>>> = use_signal(HashMap::new);
    let mut cosmos_testing: Signal<HashSet<String>> = use_signal(HashSet::new);

    let mut cosmos_adhoc_endpoint: Signal<String> = use_signal(|| cosmos_check::EMULATOR_ENDPOINT.to_string());
    let mut cosmos_adhoc_key:      Signal<String> = use_signal(|| cosmos_check::EMULATOR_KEY.to_string());
    let mut cosmos_adhoc_testing:  Signal<bool>   = use_signal(|| false);
    let mut cosmos_adhoc_result:   Signal<Option<Result<u64, String>>> = use_signal(|| None);

    let mut cosmos_edits = props.cosmos_edits;

    rsx! {
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
                    title: "Fill in the emulator default (port 1234 is UI only — API is 8081)",
                    onclick: move |_| {
                        cosmos_adhoc_endpoint.set(cosmos_check::EMULATOR_ENDPOINT.to_string());
                        cosmos_adhoc_result.set(None);
                    },
                    "↺ Default"
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
                    title: "Fill in the well-known emulator key",
                    onclick: move |_| {
                        cosmos_adhoc_key.set(cosmos_check::EMULATOR_KEY.to_string());
                        cosmos_adhoc_result.set(None);
                    },
                    "↺ Default"
                }
            }

            if let Some(Err(ref e)) = *cosmos_adhoc_result.read() {
                div { class: "db-test-error-detail", "{e}" }
            }
            div { class: "db-msi-note",
                "Port 1234 is the data-explorer UI only — Logic Apps connects to "
                code { "8081" } "."
            }

            CosmosQueryConsole {
                endpoint:    cosmos_adhoc_endpoint,
                account_key: cosmos_adhoc_key,
                console_id:  "adhoc".to_string(),
            }
        }

        if !props.cosmos_connections.is_empty() {
            div { class: "db-section-title", style: "margin-top:16px", "Connections from connections.json" }
        }

        for conn in props.cosmos_connections.clone() {
            {
                let conn_name  = conn.connection_name.clone();
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

                        CosmosConnQueryConsole {
                            endpoint_seed:    cosmos_edits.read().get(&conn.endpoint_key.clone().unwrap_or_default()).cloned().unwrap_or_else(|| conn.endpoint.clone()),
                            account_key_seed: cosmos_edits.read().get(&conn.key_key.clone().unwrap_or_default()).cloned().unwrap_or_else(|| conn.account_key.clone()),
                            console_id:       conn.connection_name.clone(),
                        }
                    }
                }
            }
        }
    }
}
