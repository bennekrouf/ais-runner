use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};
use crate::services::sql_check::{self, SqlAuthType, SqlConnection, TestResult};

#[derive(Props, Clone, PartialEq)]
pub struct SqlTabProps {
    pub connections: Vec<SqlConnection>,
    pub edits:       Signal<HashMap<String, String>>,
    pub status:      Signal<Option<(String, bool)>>,
}

#[component]
pub fn SqlTab(props: SqlTabProps) -> Element {
    let mut sql_test_results: Signal<HashMap<String, TestResult>> = use_signal(HashMap::new);
    let mut sql_testing:      Signal<HashSet<String>>             = use_signal(HashSet::new);

    let mut edits  = props.edits;
    let _status    = props.status; // passed to parent; unused locally

    rsx! {
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
    }
}
