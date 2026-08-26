use std::collections::HashMap;

/// Return the resolved (fqdn, queue_name) for the trigger of a single workflow,
/// or None if the workflow is not Service Bus-triggered.
pub fn trigger_queue_for(logic_apps_dir: &str, workflow_name: &str) -> Option<(String, String)> {
    let (fqdn, queues) = detect_sb_queues(logic_apps_dir);
    queues
        .into_iter()
        .find(|q| q.trigger_workflows.iter().any(|w| w == workflow_name))
        .map(|q| (fqdn, q.queue))
}

#[derive(Debug, Clone, PartialEq)]
pub struct SbQueueInfo {
    pub queue: String,
    pub namespace: String,
    pub trigger_workflows: Vec<String>,
    pub action_workflows: Vec<String>,
    pub requires_session: bool,
}

/// The local.settings.json key + current value for the SB connection string, if present.
/// Returns `(key, current_value)`.
pub fn detect_sb_conn_str_key(logic_apps_dir: &str) -> Option<(String, String)> {
    let dir = crate::services::workflows::resolve_logic_apps_dir(logic_apps_dir);

    let settings_text = std::fs::read_to_string(dir.join("local.settings.json")).ok()?;
    let settings: serde_json::Value = serde_json::from_str(&settings_text).ok()?;
    let vals = settings["Values"].as_object()?;

    let resolve = |key: &str| -> String {
        vals.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };

    // Primary: check connections.json @appsetting reference
    if let Ok(conn_text) = std::fs::read_to_string(dir.join("connections.json")) {
        if let Ok(conn_json) = serde_json::from_str::<serde_json::Value>(&conn_text) {
            if let Some(providers) = conn_json["serviceProviderConnections"].as_object() {
                for (_name, conn) in providers {
                    if conn["serviceProvider"]["id"].as_str()
                        == Some("/serviceProviders/serviceBus")
                    {
                        if let Some(raw) = conn["parameterValues"]["connectionString"].as_str() {
                            if let Some(key) = raw
                                .strip_prefix("@appsetting('")
                                .and_then(|s| s.strip_suffix("')"))
                            {
                                return Some((key.to_string(), resolve(key)));
                            }
                        }
                    }
                }
            }
        }
    }

    // Fallback: scan local.settings.json for a SB-looking connection string key
    for (k, v) in vals {
        let kl = k.to_lowercase();
        if (kl.contains("servicebus") || kl.contains("service_bus")) && kl.contains("connection") {
            let val = v.as_str().unwrap_or("");
            if val.is_empty() || val.contains("Endpoint=sb://") || val.contains("SharedAccessKey") {
                return Some((k.clone(), val.to_string()));
            }
        }
    }
    None
}

/// The setting key that holds the Service Bus FQDN (or connection string), if resolved from @appsetting.
/// Returns None when the namespace/connection string is hardcoded in connections.json.
pub fn detect_sb_namespace_key(logic_apps_dir: &str) -> Option<String> {
    let dir = crate::services::workflows::resolve_logic_apps_dir(logic_apps_dir);
    let conn_text = std::fs::read_to_string(dir.join("connections.json")).ok()?;
    let conn_json: serde_json::Value = serde_json::from_str(&conn_text).ok()?;
    let providers = conn_json["serviceProviderConnections"].as_object()?;
    for (_name, conn) in providers {
        if conn["serviceProvider"]["id"].as_str() == Some("/serviceProviders/serviceBus") {
            // MSI: fullyQualifiedNamespace is the direct namespace key
            if let Some(raw) = conn["parameterValues"]["fullyQualifiedNamespace"].as_str() {
                if let Some(key) = raw
                    .strip_prefix("@appsetting('")
                    .and_then(|s| s.strip_suffix("')"))
                {
                    return Some(key.to_string());
                }
            }
            // ConnStr: namespace is embedded in the connection string — return None
            // (the connection string key is handled by detect_sb_conn_str_key)
        }
    }
    None
}

/// Scan all workflow.json files and return (namespace_fqdn, queues).
pub fn detect_sb_queues(logic_apps_dir: &str) -> (String, Vec<SbQueueInfo>) {
    let dir = crate::services::workflows::resolve_logic_apps_dir(logic_apps_dir);

    // Resolve @appsetting references from local.settings.json
    let settings: HashMap<String, String> =
        std::fs::read_to_string(dir.join("local.settings.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v["Values"].as_object().cloned())
            .map(|m| {
                m.into_iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

    // Find the Service Bus namespace FQDN from connections.json
    let conn_text = std::fs::read_to_string(dir.join("connections.json")).unwrap_or_default();
    let conn_json: serde_json::Value = serde_json::from_str(&conn_text).unwrap_or_default();

    let mut namespace = String::new();
    if let Some(providers) = conn_json["serviceProviderConnections"].as_object() {
        for (_name, conn) in providers {
            if conn["serviceProvider"]["id"].as_str() == Some("/serviceProviders/serviceBus") {
                // MSI style: fullyQualifiedNamespace
                // The setting may be the short name ("sbns-foo") or the full FQDN
                // ("sbns-foo.servicebus.windows.net") — normalise to the full FQDN.
                if let Some(raw) = conn["parameterValues"]["fullyQualifiedNamespace"].as_str() {
                    let resolved = resolve_appsetting(raw, &settings);
                    if !resolved.is_empty() {
                        namespace = normalise_sb_fqdn(&resolved);
                        break;
                    }
                }
                // Connection-string style: extract FQDN from Endpoint=sb://…
                if let Some(raw) = conn["parameterValues"]["connectionString"].as_str() {
                    let cs = resolve_appsetting(raw, &settings);
                    if let Some(fqdn) = fqdn_from_conn_str(&cs) {
                        namespace = fqdn;
                        break;
                    }
                }
            }
        }
    }

    // Fallback: scan local.settings.json for any value that looks like a SB namespace
    if namespace.is_empty() {
        for val in settings.values() {
            if let Some(fqdn) = fqdn_from_conn_str(val) {
                namespace = fqdn;
                break;
            }
            // Accept both the full FQDN and the bare short name
            let v = val.trim();
            if v.contains(".servicebus.windows.net") && !v.contains("Endpoint=") {
                namespace = v.to_string();
                break;
            }
            // Short name like "sbns-foo" stored directly — normalise it
            if !v.is_empty()
                && !v.contains(' ')
                && !v.contains('=')
                && (v.starts_with("sbns-") || v.ends_with("-sb") || v.contains("servicebus"))
            {
                namespace = normalise_sb_fqdn(v);
                break;
            }
        }
    }

    // Second Fallback: check project link in config
    if namespace.is_empty() {
        if let Some(link) = crate::services::config::load().get_link(logic_apps_dir) {
            if let Some(ns) = &link.sb_namespace {
                namespace = ns.clone();
            }
        }
    }

    // Scan every workflow folder
    let mut queue_map: HashMap<String, SbQueueInfo> = HashMap::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return (namespace, vec![]),
    };

    for entry in entries.flatten() {
        let wf_name = match entry.file_name().to_str() {
            Some(n) => n.to_string(),
            None => continue,
        };
        let wf_path = entry.path().join("workflow.json");
        let wf_text = match std::fs::read_to_string(&wf_path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let wf: serde_json::Value = match serde_json::from_str(&wf_text) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let defn = wf.get("definition").unwrap_or(&wf);

        // Trigger check
        if let Some(triggers) = defn["triggers"].as_object() {
            for (_tname, trigger) in triggers {
                let is_sb_trigger = trigger["kind"].as_str() == Some("ServiceBusTrigger")
                    || (trigger["type"].as_str() == Some("ServiceProvider")
                        && trigger["inputs"]["serviceProviderConfiguration"]["serviceProviderId"]
                            .as_str()
                            == Some("/serviceProviders/serviceBus"));
                if is_sb_trigger {
                    if let Some(queue) = resolve_queue_name(trigger, &settings) {
                        let is_session = trigger["inputs"]["serviceProviderConfiguration"]
                            ["operationId"]
                            .as_str()
                            == Some("onNewMessagesFromQueueSession");
                        let entry = queue_map
                            .entry(queue.clone())
                            .or_insert_with(|| SbQueueInfo {
                                queue: queue.clone(),
                                namespace: namespace.clone(),
                                trigger_workflows: vec![],
                                action_workflows: vec![],
                                requires_session: false,
                            });
                        if is_session {
                            entry.requires_session = true;
                        }
                        if !entry.trigger_workflows.contains(&wf_name) {
                            entry.trigger_workflows.push(wf_name.clone());
                        }
                    }
                }
            }
        }

        // Action check (recursive)
        if let Some(actions) = defn["actions"].as_object() {
            scan_sb_actions(actions, &wf_name, &namespace, &settings, &mut queue_map);
        }
    }

    let mut queues: Vec<SbQueueInfo> = queue_map.into_values().collect();
    queues.sort_by(|a, b| a.queue.cmp(&b.queue));
    (namespace, queues)
}

fn scan_sb_actions(
    actions: &serde_json::Map<String, serde_json::Value>,
    wf_name: &str,
    namespace: &str,
    settings: &HashMap<String, String>,
    queue_map: &mut HashMap<String, SbQueueInfo>,
) {
    for (_name, action) in actions {
        // Service Provider action for Service Bus
        if action["type"].as_str() == Some("ServiceProvider") {
            let provider_id = action["inputs"]["serviceProviderConfiguration"]["serviceProviderId"]
                .as_str()
                .unwrap_or("");
            if provider_id == "/serviceProviders/serviceBus" {
                if let Some(queue) = resolve_queue_name(action, settings) {
                    let entry = queue_map
                        .entry(queue.clone())
                        .or_insert_with(|| SbQueueInfo {
                            queue: queue.clone(),
                            namespace: namespace.to_string(),
                            trigger_workflows: vec![],
                            action_workflows: vec![],
                            requires_session: false,
                        });
                    if !entry.action_workflows.contains(&wf_name.to_string()) {
                        entry.action_workflows.push(wf_name.to_string());
                    }
                }
            }
        }

        // Recurse into standard nested containers
        for sub_key in &["actions", "else", "default"] {
            if let Some(nested) = action[sub_key].as_object() {
                scan_sb_actions(nested, wf_name, namespace, settings, queue_map);
            }
        }

        // Switch cases: action["cases"][case_name]["actions"]
        if let Some(cases) = action["cases"].as_object() {
            for (_case_name, case_val) in cases {
                if let Some(case_actions) = case_val["actions"].as_object() {
                    scan_sb_actions(case_actions, wf_name, namespace, settings, queue_map);
                }
            }
        }
    }
}

/// Extract the queue/topic name from an action or trigger node, resolving @appsetting refs.
/// Checks all field name variants used across Logic Apps Standard versions.
fn resolve_queue_name(
    node: &serde_json::Value,
    settings: &HashMap<String, String>,
) -> Option<String> {
    let params = &node["inputs"]["parameters"];
    let raw = params["queueName"]
        .as_str()
        .or_else(|| params["entityName"].as_str())
        .or_else(|| params["queueOrTopicName"].as_str())?;
    // Trim — queue names from @appsetting values or hand-edited JSON sometimes
    // carry stray whitespace which the SB emulator rejects with a regex assertion.
    let resolved = resolve_appsetting(raw, settings).trim().to_string();
    if resolved.is_empty() {
        None
    } else {
        Some(resolved)
    }
}

fn resolve_appsetting(val: &str, settings: &HashMap<String, String>) -> String {
    if let Some(key) = val
        .strip_prefix("@appsetting('")
        .and_then(|s| s.strip_suffix("')"))
    {
        settings.get(key).cloned().unwrap_or_default()
    } else {
        val.to_string()
    }
}

/// Extract the hostname from a Service Bus connection string.
/// `Endpoint=sb://sbns-xxx.servicebus.windows.net/;...` → `sbns-xxx.servicebus.windows.net`
/// Ensure the namespace is a fully-qualified Service Bus hostname.
/// Accepts either the short name ("sbns-foo") or the full FQDN ("sbns-foo.servicebus.windows.net").
pub fn normalise_sb_fqdn(name: &str) -> String {
    let n = name.trim();
    // localhost, 127.0.0.1, or any bare IP — leave as-is
    if n == "localhost" || n.starts_with("127.") || n.parse::<std::net::IpAddr>().is_ok() {
        return n.to_string();
    }
    if n.contains('.') {
        n.to_string()
    } else {
        format!("{}.servicebus.windows.net", n)
    }
}

/// True when the resolved namespace points to the local Service Bus emulator
/// (localhost / 127.x.x.x).
pub fn is_local_emulator(fqdn: &str) -> bool {
    let h = fqdn.trim();
    h == "localhost" || h.starts_with("127.") || h.parse::<std::net::IpAddr>().is_ok()
}

/// True when the project's local.settings.json has a Service Bus connection string
/// that contains `UseDevelopmentEmulator=true` — meaning the local emulator is active
/// regardless of what namespace the connections.json resolves to.
pub fn is_emulator_configured(logic_apps_dir: &str) -> bool {
    let dir = crate::services::workflows::resolve_logic_apps_dir(logic_apps_dir);
    let text = match std::fs::read_to_string(dir.join("local.settings.json")) {
        Ok(t) => t,
        Err(_) => return false,
    };
    let settings: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return false,
    };
    if let Some(vals) = settings["Values"].as_object() {
        for val in vals.values() {
            if let Some(s) = val.as_str() {
                if s.contains("UseDevelopmentEmulator=true") {
                    return true;
                }
            }
        }
    }
    false
}

pub fn fqdn_from_conn_str(conn_str: &str) -> Option<String> {
    for part in conn_str.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("Endpoint=sb://") {
            let host = rest.trim_end_matches('/');
            if !host.is_empty() {
                return Some(host.to_string());
            }
        }
    }
    None
}

/// Read queue names from the emulator's config.json that are not referenced
/// by any local workflow — these are manually added queues.
pub fn emulator_only_queues(workflow_queues: &[SbQueueInfo]) -> Vec<String> {
    let config_path = crate::handlers::sb_emulator::work_dir().join("Config.json");
    let text = match std::fs::read_to_string(&config_path) {
        Ok(t) => t,
        Err(_) => return vec![],
    };
    let v: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let known: std::collections::HashSet<&str> =
        workflow_queues.iter().map(|q| q.queue.as_str()).collect();

    v["UserConfig"]["Namespaces"][0]["Queues"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|q| q["Name"].as_str())
                .filter(|name| !known.contains(name) && *name != "ais.default")
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default()
}
