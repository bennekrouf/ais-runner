use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};
use crate::services::cosmos_check::{self, CosmosConnection};

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
    }
}
