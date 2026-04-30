use std::collections::HashMap;
use crate::services::sql_check::{test_tcp, TestResult};

#[derive(Debug, Clone, PartialEq)]
pub struct SbQueueInfo {
    pub queue:              String,
    pub namespace:          String,
    pub trigger_workflows:  Vec<String>,
    pub action_workflows:   Vec<String>,
}

/// Scan all workflow.json files and return (namespace_fqdn, queues).
pub fn detect_sb_queues(logic_apps_dir: &str) -> (String, Vec<SbQueueInfo>) {
    let dir = std::path::Path::new(logic_apps_dir);

    // Resolve @appsetting references from local.settings.json
    let settings: HashMap<String, String> = std::fs::read_to_string(dir.join("local.settings.json"))
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
                if let Some(raw) = conn["parameterValues"]["fullyQualifiedNamespace"].as_str() {
                    let resolved = resolve_appsetting(raw, &settings);
                    if !resolved.is_empty() {
                        namespace = resolved;
                        break;
                    }
                }
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
                if trigger["kind"].as_str() == Some("ServiceBusTrigger") {
                    if let Some(queue) = resolve_queue_name(trigger, &settings) {
                        let entry = queue_map.entry(queue.clone()).or_insert_with(|| SbQueueInfo {
                            queue: queue.clone(),
                            namespace: namespace.clone(),
                            trigger_workflows: vec![],
                            action_workflows: vec![],
                        });
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
    actions:   &serde_json::Map<String, serde_json::Value>,
    wf_name:   &str,
    namespace: &str,
    settings:  &HashMap<String, String>,
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
                    let entry = queue_map.entry(queue.clone()).or_insert_with(|| SbQueueInfo {
                        queue: queue.clone(),
                        namespace: namespace.to_string(),
                        trigger_workflows: vec![],
                        action_workflows: vec![],
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
fn resolve_queue_name(node: &serde_json::Value, settings: &HashMap<String, String>) -> Option<String> {
    let raw = node["inputs"]["parameters"]["queueOrTopicName"].as_str()?;
    let resolved = resolve_appsetting(raw, settings);
    if resolved.is_empty() { None } else { Some(resolved) }
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

/// TCP test to the Service Bus namespace on AMQP port 5671.
pub fn test_sb_tcp(namespace: &str) -> TestResult {
    test_tcp(namespace, 5671)
}
