use crate::services::recorder;
use crate::services::scenario::Step;
use crate::services::sql_check::{self, SqlAuthType, SqlConnection, TestResult};
use crate::services::sql_runner;
use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};

#[derive(Props, Clone, PartialEq)]
pub struct SqlTabProps {
    pub connections: Vec<SqlConnection>,
    pub edits: Signal<HashMap<String, String>>,
    pub status: Signal<Option<(String, bool)>>,
}

#[component]
pub fn SqlTab(props: SqlTabProps) -> Element {
    let mut sql_test_results: Signal<HashMap<String, TestResult>> = use_signal(HashMap::new);
    let mut sql_testing: Signal<HashSet<String>> = use_signal(HashSet::new);

    // Scenario recording — a no-op unless a recording is in progress.
    let recorder = use_context::<crate::screens::MainContext>().recorder;

    // ── SQL Dev Console state ──────────────────────────────────────────
    let mut dev_db_name: Signal<String> = use_signal(String::new);
    let mut dev_db_result: Signal<Option<Result<String, String>>> = use_signal(|| None);
    let mut dev_db_busy: Signal<bool> = use_signal(|| false);

    let mut dev_sql_db: Signal<String> = use_signal(String::new);
    let mut dev_sql_input: Signal<String> = use_signal(String::new);
    let mut dev_sql_result: Signal<Option<Result<String, String>>> = use_signal(|| None);
    let mut dev_sql_busy: Signal<bool> = use_signal(|| false);

    // Status line for the "Copy shell command" button (item 6 in the
    // batch-B bug report). Hoisted to component scope because Dioxus tracks
    // hook call order — calling `use_signal` from inside an rsx block can
    // panic at render time if rendering order isn't perfectly stable.
    let mut shell_status: Signal<Option<String>> = use_signal(|| None);

    // ── Database tree ──────────────────────────────────────────────────
    // Vec<(db_name, Vec<(schema, table, row_count)>)>
    let mut db_tree: Signal<Vec<(String, Vec<(String, String, u64)>)>> = use_signal(Vec::new);
    let mut tree_loading: Signal<bool> = use_signal(|| false);
    let mut expanded_dbs: Signal<std::collections::HashSet<String>> =
        use_signal(std::collections::HashSet::new);
    // key = "db::schema.table" — pending drop confirmation
    let mut confirm_drop: Signal<Option<String>> = use_signal(|| None);

    let mut refresh_tree = move || {
        tree_loading.set(true);
        spawn(async move {
            match sql_runner::list_databases().await {
                Ok(dbs) => {
                    let mut tree = Vec::new();
                    for db in dbs {
                        let tables = sql_runner::list_tables(&db).await.unwrap_or_default();
                        tree.push((db, tables));
                    }
                    db_tree.set(tree);
                }
                Err(_) => {
                    db_tree.set(vec![]);
                }
            }
            tree_loading.set(false);
        });
    };

    // Load tree on first mount
    use_effect(move || {
        refresh_tree();
    });

    // Live refresh every 3 seconds
    use_coroutine(
        move |_rx: dioxus::prelude::UnboundedReceiver<()>| async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                refresh_tree();
            }
        },
    );

    let mut edits = props.edits;
    let _status = props.status; // passed to parent; unused locally

    rsx! {
        if props.connections.is_empty() {
            div { class: "empty-state", "No SQL connections found in connections.json" }
        }

        // ── SQL Dev Console (ais-sql-dev on localhost:1433) ───────────────
        div { class: "db-section-title",
            "🛠 SQL Dev Console"
            button {
                class: "btn btn-small",
                style: "margin-left:8px; font-size:11px;",
                disabled: *tree_loading.read(),
                onclick: move |_| refresh_tree(),
                if *tree_loading.read() { "↻ …" } else { "↻ Refresh" }
            }
            // "Copy shell command" — auto-detects whether the container ships
            // `mssql-tools` (legacy) or `mssql-tools18` (newer, needs -C) and
            // copies a docker-exec command line the user can paste into their
            // own terminal. Cross-platform safer than trying to spawn an
            // interactive PTY from inside the GUI.
            {
                let status_text = shell_status.read().clone();
                rsx! {
                    button {
                        class: "btn btn-small",
                        style: "margin-left:6px; font-size:11px;",
                        title: "Copy a docker-exec sqlcmd command line to the clipboard",
                        onclick: move |_| {
                            spawn(async move {
                                match sql_runner::detect_sqlcmd_path().await {
                                    Some((path, needs_c)) => {
                                        let line = sql_runner::shell_command_line(&path, needs_c);
                                        let copied = tokio::task::spawn_blocking(move || {
                                            if let Ok(mut cb) = arboard::Clipboard::new() {
                                                cb.set_text(line).is_ok()
                                            } else { false }
                                        }).await.unwrap_or(false);
                                        let suffix = if needs_c { " (tools18)" } else { " (legacy tools)" };
                                        shell_status.set(Some(if copied {
                                            format!("📋 Copied{}", suffix)
                                        } else {
                                            format!("⚠ Detected{} — clipboard unavailable", suffix)
                                        }));
                                    }
                                    None => shell_status.set(Some(
                                        "⚠ sqlcmd not found in container — is SQL Dev running?".into()
                                    )),
                                }
                                // Auto-clear after 3s so the button label doesn't stay
                                // permanently in a transient state.
                                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                shell_status.set(None);
                            });
                        },
                        "📋 Copy shell command"
                    }
                    if let Some(s) = status_text {
                        span { style: "margin-left:8px; font-size:11px; color: var(--text2);", "{s}" }
                    }
                }
            }
        }

        // ── Database / table tree ─────────────────────────────────────────
        {
            let tree = db_tree.read();
            if tree.is_empty() {
                rsx! {
                    div { class: "db-card",
                        span { style: "font-size:12px; opacity:0.6;",
                            "No databases yet — create one below."
                        }
                    }
                }
            } else {
                rsx! {
                    div { class: "db-card", style: "padding:6px 8px;",
                        for (db_name, tables) in tree.iter() {
                            {
                                let db = db_name.clone();
                                let db_click = db_name.clone();
                                let db_drop  = db_name.clone();
                                let db_drop_key  = format!("db::{}", db_name);
                                let db_drop_key2 = db_drop_key.clone();
                                let is_expanded    = expanded_dbs.read().contains(db_name);
                                let is_confirming_db = confirm_drop.read().as_deref() == Some(&db_drop_key);
                                rsx! {
                                    div {
                                        // Database row
                                        div {
                                            style: "display:flex; align-items:center; gap:4px; cursor:pointer; padding:2px 0;",
                                            onclick: {
                                                let db = db.clone();
                                                move |_| {
                                                    let mut set = expanded_dbs.write();
                                                    if set.contains(&db) { set.remove(&db); } else { set.insert(db.clone()); }
                                                }
                                            },
                                            span { style: "font-size:11px;", if is_expanded { "▾" } else { "▸" } }
                                            span { style: "font-size:12px; font-weight:600;", "🗄 {db_name}" }
                                            // Use button
                                            button {
                                                class: "btn btn-small",
                                                style: "margin-left:auto; font-size:10px; padding:1px 6px;",
                                                title: "Use this database",
                                                onclick: move |e| {
                                                    e.stop_propagation();
                                                    dev_sql_db.set(db_click.clone());
                                                },
                                                "Use"
                                            }
                                            // Drop database — two-click confirm
                                            if is_confirming_db {
                                                button {
                                                    class: "btn btn-small",
                                                    style: "font-size:10px; padding:0 5px; background:var(--red,#f85149); color:#fff;",
                                                    title: "Click again to drop database",
                                                    onclick: move |e| {
                                                        e.stop_propagation();
                                                        let db = db_drop.clone();
                                                        confirm_drop.set(None);
                                                        spawn(async move {
                                                            if sql_runner::drop_database(&db).await.is_ok() {
                                                                recorder::record(recorder, Step::DropSqlDatabase { name: db.clone() });
                                                            }
                                                            refresh_tree();
                                                        });
                                                    },
                                                    "⚠ Confirm"
                                                }
                                                button {
                                                    class: "btn btn-small",
                                                    style: "font-size:10px; padding:0 4px;",
                                                    onclick: move |e| { e.stop_propagation(); confirm_drop.set(None); },
                                                    "✕"
                                                }
                                            } else {
                                                button {
                                                    class: "btn btn-small",
                                                    style: "font-size:10px; padding:0 5px; opacity:0.7;",
                                                    title: "Drop database — requires confirmation",
                                                    onclick: move |e| {
                                                        e.stop_propagation();
                                                        confirm_drop.set(Some(db_drop_key2.clone()));
                                                    },
                                                    "🗑"
                                                }
                                            }
                                        }
                                        // Tables (when expanded)
                                        if is_expanded {
                                            if tables.is_empty() {
                                                div { style: "padding-left:20px; font-size:11px; opacity:0.55;", "(no tables)" }
                                            }
                                            for (schema, table, count) in tables.iter() {
                                                {
                                                    let db_f  = db_name.clone();
                                                    let db_d  = db_name.clone();
                                                    let sch_f = schema.clone();
                                                    let sch_d = schema.clone();
                                                    let tbl_f = table.clone();
                                                    let tbl_d = table.clone();
                                                    let drop_key = format!("{}::{}:{}", db_name, schema, table);
                                                    let drop_key2 = drop_key.clone();
                                                    let is_confirming = confirm_drop.read().as_deref() == Some(&drop_key);
                                                    rsx! {
                                                        div {
                                                            style: "display:flex; align-items:center; padding:1px 0 1px 20px; gap:4px;",
                                                            span {
                                                                style: "font-size:11px; color:var(--fg-dim, #888); flex:1;",
                                                                "📋 {schema}.{table}"
                                                            }
                                                            span {
                                                                style: "font-size:10px; opacity:0.55; font-variant-numeric:tabular-nums;",
                                                                "{count}"
                                                            }
                                                            // Flush button
                                                            button {
                                                                class: "btn btn-small",
                                                                style: "font-size:10px; padding:0 5px; opacity:0.7;",
                                                                title: "Truncate — delete all rows, keep table",
                                                                onclick: move |_| {
                                                                    let db = db_f.clone(); let s = sch_f.clone(); let t = tbl_f.clone();
                                                                    spawn(async move {
                                                                        if sql_runner::truncate_table(&db, &s, &t).await.is_ok() {
                                                                            recorder::record(recorder, Step::TruncateTable {
                                                                                database: db.clone(), schema: s.clone(), table: t.clone(),
                                                                            });
                                                                        }
                                                                        refresh_tree();
                                                                    });
                                                                },
                                                                "⌫ Flush"
                                                            }
                                                            // Drop button — two-click confirm
                                                            if is_confirming {
                                                                button {
                                                                    class: "btn btn-small",
                                                                    style: "font-size:10px; padding:0 5px; background:var(--red,#f85149); color:#fff;",
                                                                    title: "Click again to confirm drop",
                                                                    onclick: move |_| {
                                                                        let db = db_d.clone(); let s = sch_d.clone(); let t = tbl_d.clone();
                                                                        confirm_drop.set(None);
                                                                        spawn(async move {
                                                                            if sql_runner::drop_table(&db, &s, &t).await.is_ok() {
                                                                                recorder::record(recorder, Step::DropTable {
                                                                                    database: db.clone(), schema: s.clone(), table: t.clone(),
                                                                                });
                                                                            }
                                                                            refresh_tree();
                                                                        });
                                                                    },
                                                                    "⚠ Confirm"
                                                                }
                                                                button {
                                                                    class: "btn btn-small",
                                                                    style: "font-size:10px; padding:0 4px;",
                                                                    onclick: move |_| { confirm_drop.set(None); },
                                                                    "✕"
                                                                }
                                                            } else {
                                                                button {
                                                                    class: "btn btn-small",
                                                                    style: "font-size:10px; padding:0 5px; opacity:0.7;",
                                                                    title: "Drop table — requires confirmation",
                                                                    onclick: move |_| { confirm_drop.set(Some(drop_key2.clone())); },
                                                                    "🗑 Drop"
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

        div { class: "db-card",

            // ── Create Database ──────────────────────────────────────────
            div { class: "db-card-header",
                span { class: "db-card-name", "Create Database" }
                span { class: "db-auth-badge cs", "ais-sql-dev" }
            }
            div { class: "db-field-row",
                label { class: "db-field-label", "Name" }
                input {
                    class: "db-field-input",
                    placeholder: "e.g. mydb",
                    value: "{dev_db_name.read()}",
                    oninput: move |e| { dev_db_name.set(e.value()); dev_db_result.set(None); },
                }
                button {
                    class: "btn btn-small btn-fetch",
                    style: "margin-left:8px; white-space:nowrap;",
                    disabled: *dev_db_busy.read() || dev_db_name.read().trim().is_empty(),
                    onclick: move |_| {
                        let name = dev_db_name.read().trim().to_string();
                        if name.is_empty() { return; }
                        dev_db_busy.set(true);
                        dev_db_result.set(None);
                        spawn(async move {
                            let result = sql_runner::create_database(&name).await;
                            if result.is_ok() {
                                recorder::record(recorder, Step::CreateSqlDatabase { name: name.clone() });
                            }
                            dev_db_result.set(Some(result));
                            dev_db_busy.set(false);
                            refresh_tree();
                        });
                    },
                    if *dev_db_busy.read() { "Creating…" } else { "Create" }
                }
            }
            if let Some(ref r) = *dev_db_result.read() {
                match r {
                    Ok(msg)  => rsx! { div { class: "db-test-ok",  style: "padding:4px 8px; font-size:12px;", "{msg.trim()}" } },
                    Err(msg) => rsx! { div { class: "db-test-error-detail", "{msg.trim()}" } },
                }
            }

            // ── Run SQL ──────────────────────────────────────────────────
            div { class: "db-card-header", style: "margin-top:12px;",
                span { class: "db-card-name", "Run SQL" }
            }
            div { class: "db-field-row",
                label { class: "db-field-label", "Database" }
                input {
                    class: "db-field-input",
                    placeholder: "e.g. mydb",
                    value: "{dev_sql_db.read()}",
                    oninput: move |e| { dev_sql_db.set(e.value()); dev_sql_result.set(None); },
                }
            }
            div { class: "db-field-row", style: "align-items:flex-start;",
                label { class: "db-field-label", style: "padding-top:6px;", "SQL" }
                textarea {
                    class: "db-field-textarea",
                    style: "font-family:monospace; font-size:12px; min-height:120px;",
                    placeholder: "CREATE SCHEMA myschema;\nCREATE TABLE [myschema].[MyTable] (\n  Id INT IDENTITY(1,1) PRIMARY KEY,\n  Name NVARCHAR(100)\n);",
                    value: "{dev_sql_input.read()}",
                    oninput: move |e| { dev_sql_input.set(e.value()); dev_sql_result.set(None); },
                }
            }
            div { style: "display:flex; justify-content:flex-end; gap:6px; margin-top:4px;",
                button {
                    class: "btn btn-small",
                    title: "Load SQL from a .sql file on disk",
                    onclick: move |_| {
                        spawn(async move {
                            let picked = rfd::AsyncFileDialog::new()
                                .add_filter("SQL", &["sql"])
                                .add_filter("All files", &["*"])
                                .pick_file()
                                .await;
                            if let Some(handle) = picked {
                                let path = handle.path().to_path_buf();
                                let read = tokio::task::spawn_blocking(move || {
                                    std::fs::read_to_string(&path).map_err(|e| (path, e))
                                }).await;
                                match read {
                                    Ok(Ok(contents)) => {
                                        dev_sql_input.set(contents);
                                        dev_sql_result.set(None);
                                    }
                                    Ok(Err((path, e))) => {
                                        dev_sql_result.set(Some(Err(format!(
                                            "Failed to read {}: {}", path.display(), e
                                        ))));
                                    }
                                    Err(e) => {
                                        dev_sql_result.set(Some(Err(format!("Read task failed: {e}"))));
                                    }
                                }
                            }
                        });
                    },
                    "📂 Import .sql"
                }
                button {
                    class: "btn btn-small btn-fetch",
                    disabled: *dev_sql_busy.read()
                        || dev_sql_db.read().trim().is_empty()
                        || dev_sql_input.read().trim().is_empty(),
                    onclick: move |_| {
                        let db  = dev_sql_db.read().trim().to_string();
                        let sql = dev_sql_input.read().trim().to_string();
                        if db.is_empty() || sql.is_empty() { return; }
                        dev_sql_busy.set(true);
                        dev_sql_result.set(None);
                        spawn(async move {
                            let result = sql_runner::run_sql(&db, &sql).await;
                            if result.is_ok() {
                                // Covers CREATE TABLE and INSERT as well as SELECT —
                                // a recorded scenario needs the schema and the seed
                                // rows, and both arrive through this one console.
                                recorder::record(recorder, Step::RunSql {
                                    database: db.clone(), sql: sql.clone(), capture: None,
                                });
                            }
                            dev_sql_result.set(Some(result));
                            dev_sql_busy.set(false);
                            refresh_tree();
                        });
                    },
                    if *dev_sql_busy.read() { "Running…" } else { "▶ Run" }
                }
            }
            {
                let result_read = dev_sql_result.read();
                match result_read.as_ref() {
                    Some(Ok(m)) => {
                        let msg = m.trim().to_string();
                        rsx! { pre { class: "db-test-ok", style: "font-size:11px; white-space:pre-wrap; margin:4px 0 0;", "{msg}" } }
                    }
                    Some(Err(m)) => {
                        let msg = m.trim().to_string();
                        rsx! { pre { class: "db-test-error-detail", style: "font-size:11px; white-space:pre-wrap; margin:4px 0 0;", "{msg}" } }
                    }
                    None => rsx! {},
                }
            }
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
    }
}
