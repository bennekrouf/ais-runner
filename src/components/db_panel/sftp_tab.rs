use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};
use crate::services::sftp_check::{self, SftpConnection, SftpTestResult};

#[derive(Props, Clone, PartialEq)]
pub struct SftpTabProps {
    pub connections:    Vec<SftpConnection>,
    pub logic_apps_dir: String,
}

#[component]
pub fn SftpTab(props: SftpTabProps) -> Element {
    let mut test_results: Signal<HashMap<String, SftpTestResult>> =
        use_signal(HashMap::new);
    let mut testing: Signal<HashSet<String>> =
        use_signal(HashSet::new);

    if props.connections.is_empty() {
        return rsx! {
            div { class: "empty-state", "No SFTP connections found in connections.json" }
        };
    }

    rsx! {
        div { class: "db-section",
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
