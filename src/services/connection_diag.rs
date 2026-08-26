use serde_json::Value;

/// Recursively walk a JSON value and collect every `@appsetting('key')` string found.
/// This discovers all settings a connection needs regardless of field name.
fn collect_appsetting_refs(val: &Value, out: &mut Vec<String>) {
    match val {
        Value::String(s) => {
            if let Some(key) = s
                .strip_prefix("@appsetting('")
                .and_then(|s| s.strip_suffix("')"))
            {
                out.push(key.to_string());
            }
        }
        Value::Object(map) => {
            for v in map.values() {
                collect_appsetting_refs(v, out);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collect_appsetting_refs(v, out);
            }
        }
        _ => {}
    }
}

/// Scan ALL workflows for connections that would cause func startup to crash.
///
/// Logic Apps Standard validates every workflow at startup regardless of whether
/// it is disabled. If any validation throws (e.g. empty/invalid connection), it
/// aborts the Azurite table-initialisation sequence and ALL other workflows lose
/// run-history persistence — they execute fine but nothing is ever recorded.
///
/// Returns one entry per at-risk workflow: (workflow_name, [issue descriptions]).
pub fn scan_startup_risks(logic_apps_dir: &str) -> Vec<(String, Vec<String>)> {
    let dir = crate::services::workflows::resolve_logic_apps_dir(logic_apps_dir);

    let conn_text = std::fs::read_to_string(dir.join("connections.json")).unwrap_or_default();
    let settings_text =
        std::fs::read_to_string(dir.join("local.settings.json")).unwrap_or_default();
    let conn: Value = serde_json::from_str(&conn_text).unwrap_or_default();
    let settings: Value = serde_json::from_str(&settings_text).unwrap_or_default();

    let Ok(entries) = std::fs::read_dir(&dir) else {
        return vec![];
    };
    let mut risks = Vec::new();

    for entry in entries.flatten() {
        let wf_path = entry.path().join("workflow.json");
        if !wf_path.exists() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(wf_text) = std::fs::read_to_string(&wf_path) else {
            continue;
        };
        let Ok(wf) = serde_json::from_str::<Value>(&wf_text) else {
            risks.push((name, vec!["invalid JSON — will crash func startup".into()]));
            continue;
        };

        let wf_str = wf.to_string();
        let mut issues = Vec::new();

        // Collect every connectionName the workflow uses
        let conn_re = regex::Regex::new(r#""connectionName"\s*:\s*"([^"]+)""#).unwrap();
        let used: std::collections::HashSet<String> = conn_re
            .captures_iter(&wf_str)
            .map(|c| c[1].to_string())
            .collect();

        if let Some(providers) = conn["serviceProviderConnections"].as_object() {
            for conn_name in &used {
                if let Some(provider) = providers.get(conn_name) {
                    // Walk the entire parameterValues tree and collect every
                    // @appsetting('key') reference, regardless of field name.
                    // This catches any new connection type without needing to
                    // enumerate field names upfront.
                    let mut appsetting_keys = Vec::new();
                    collect_appsetting_refs(&provider["parameterValues"], &mut appsetting_keys);

                    for key in appsetting_keys {
                        if settings["Values"][&key].as_str().unwrap_or("").is_empty() {
                            issues.push(format!(
                                "connection '{}': setting '{}' is empty",
                                conn_name, key
                            ));
                        }
                    }
                } else {
                    issues.push(format!(
                        "connection '{}' used but not in connections.json",
                        conn_name
                    ));
                }
            }
        } else if !used.is_empty() {
            for conn_name in &used {
                issues.push(format!(
                    "connection '{}' used but connections.json is missing",
                    conn_name
                ));
            }
        }

        if !issues.is_empty() {
            risks.push((name, issues));
        }
    }

    risks.sort_by(|a, b| a.0.cmp(&b.0));
    risks
}

/// For a given workflow, return the names of service-provider connections it uses
/// whose appsetting endpoints are empty in local.settings.json.
/// Returns pairs of (connection_name, empty_appsetting_key).
pub fn missing_endpoints_for_workflow(
    logic_apps_dir: &str,
    workflow_name: &str,
) -> Vec<(String, String)> {
    let dir = crate::services::workflows::resolve_logic_apps_dir(logic_apps_dir);

    let wf_text = match std::fs::read_to_string(dir.join(workflow_name).join("workflow.json")) {
        Ok(t) => t,
        Err(_) => return vec![],
    };
    let conn_text = match std::fs::read_to_string(dir.join("connections.json")) {
        Ok(t) => t,
        Err(_) => return vec![],
    };
    let settings_text =
        std::fs::read_to_string(dir.join("local.settings.json")).unwrap_or_default();

    let wf: Value = serde_json::from_str(&wf_text).unwrap_or_default();
    let conn: Value = serde_json::from_str(&conn_text).unwrap_or_default();
    let settings: Value = serde_json::from_str(&settings_text).unwrap_or_default();

    // Collect all connectionName values referenced in the workflow actions
    let wf_str = wf.to_string();
    let mut used_connections = std::collections::HashSet::new();
    for cap in regex::Regex::new(r#""connectionName"\s*:\s*"([^"]+)""#)
        .unwrap()
        .captures_iter(&wf_str)
    {
        used_connections.insert(cap[1].to_string());
    }

    let mut missing = Vec::new();
    let empty_providers = conn["serviceProviderConnections"].as_object();
    if let Some(providers) = empty_providers {
        for (name, provider) in providers {
            if !used_connections.contains(name) {
                continue;
            }

            // Find the appsetting key for the primary endpoint field
            let pv = &provider["parameterValues"];
            let endpoint_fields = [
                "blobStorageEndpoint",
                "fullyQualifiedNamespace",
                "connectionString",
                "topicEndpoint",
                "VaultUri",
                "sshHostAddress",
            ];
            for field in &endpoint_fields {
                if let Some(raw) = pv[field].as_str() {
                    let key = raw
                        .strip_prefix("@appsetting('")
                        .and_then(|s| s.strip_suffix("')"))
                        .unwrap_or(raw);
                    let val = settings["Values"][key].as_str().unwrap_or("");
                    if val.is_empty() {
                        missing.push((name.clone(), key.to_string()));
                        break;
                    }
                }
            }
        }
    }
    missing.sort();
    missing
}

/// Returns the set of workflow names whose **trigger** connection uses
/// Managed Identity auth but has an endpoint pointing to a local Azurite URL.
///
/// MSI requires the Azure Instance Metadata endpoint (169.254.169.254) which
/// does not exist locally, so the blob trigger poller silently fails to
/// authenticate and never fires — even though the endpoint itself is reachable.
/// Switching the connection to `parameterSetName: "connectionString"` with
/// `UseDevelopmentStorage=true` fixes this for local development.
pub fn scan_msi_local_trigger_workflows(logic_apps_dir: &str) -> std::collections::HashSet<String> {
    let dir = crate::services::workflows::resolve_logic_apps_dir(logic_apps_dir);

    let conn_text = std::fs::read_to_string(dir.join("connections.json")).unwrap_or_default();
    let settings_text =
        std::fs::read_to_string(dir.join("local.settings.json")).unwrap_or_default();
    let conn: Value = serde_json::from_str(&conn_text).unwrap_or_default();
    let settings: Value = serde_json::from_str(&settings_text).unwrap_or_default();

    // Build a set of connection names that use MSI + local Azurite endpoint
    let mut msi_local: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(providers) = conn["serviceProviderConnections"].as_object() {
        for (conn_name, provider) in providers {
            let param_set = provider["parameterSetName"].as_str().unwrap_or("");
            if !param_set.eq_ignore_ascii_case("ManagedServiceIdentity") {
                continue;
            }

            // Check if any endpoint appsetting resolves to a local URL
            let pv = &provider["parameterValues"];
            let endpoint_fields = [
                "blobStorageEndpoint",
                "fullyQualifiedNamespace",
                "topicEndpoint",
                "VaultUri",
                "sshHostAddress",
                "accountEndpoint",
            ];
            for field in &endpoint_fields {
                if let Some(raw) = pv[field].as_str() {
                    let key = raw
                        .strip_prefix("@appsetting('")
                        .and_then(|s| s.strip_suffix("')"))
                        .unwrap_or(raw);
                    let val = settings["Values"][key].as_str().unwrap_or("");
                    if val.contains("127.0.0.1") || val.contains("localhost") {
                        msi_local.insert(conn_name.clone());
                        break;
                    }
                }
            }
        }
    }

    if msi_local.is_empty() {
        return std::collections::HashSet::new();
    }

    // Return workflows whose TRIGGER uses one of those MSI-local connections
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return std::collections::HashSet::new(),
    };
    let mut affected = std::collections::HashSet::new();
    for entry in entries.flatten() {
        let wf_path = entry.path().join("workflow.json");
        if !wf_path.exists() {
            continue;
        }
        let Ok(wf_text) = std::fs::read_to_string(&wf_path) else {
            continue;
        };
        let Ok(wf) = serde_json::from_str::<Value>(&wf_text) else {
            continue;
        };

        if let Some(triggers) = wf["definition"]["triggers"].as_object() {
            for trigger in triggers.values() {
                let conn_name = trigger["inputs"]["serviceProviderConfiguration"]["connectionName"]
                    .as_str()
                    .unwrap_or("");
                if msi_local.contains(conn_name) {
                    affected.insert(entry.file_name().to_string_lossy().into_owned());
                    break;
                }
            }
        }
    }
    affected
}
