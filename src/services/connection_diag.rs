use serde_json::Value;

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
    let settings_text = std::fs::read_to_string(dir.join("local.settings.json")).unwrap_or_default();
    let conn: Value     = serde_json::from_str(&conn_text).unwrap_or_default();
    let settings: Value = serde_json::from_str(&settings_text).unwrap_or_default();

    let Ok(entries) = std::fs::read_dir(&dir) else { return vec![] };
    let mut risks = Vec::new();

    for entry in entries.flatten() {
        let wf_path = entry.path().join("workflow.json");
        if !wf_path.exists() { continue; }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(wf_text) = std::fs::read_to_string(&wf_path) else { continue };
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
            let endpoint_fields = ["blobStorageEndpoint", "fullyQualifiedNamespace",
                                   "connectionString", "topicEndpoint", "VaultUri",
                                   "sshHostAddress", "accountEndpoint"];
            for conn_name in &used {
                if let Some(provider) = providers.get(conn_name) {
                    let pv = &provider["parameterValues"];
                    // Flat values
                    for field in &endpoint_fields {
                        if let Some(raw) = pv[field].as_str() {
                            let key = raw
                                .strip_prefix("@appsetting('").and_then(|s| s.strip_suffix("')"))
                                .unwrap_or(raw);
                            if settings["Values"][key].as_str().unwrap_or("").is_empty() {
                                issues.push(format!("connection '{}': setting '{}' is empty", conn_name, key));
                                break;
                            }
                        }
                    }
                    // Nested: authenticationPolicy.credential.accountKey
                    if let Some(raw) = pv["authenticationPolicy"]["credential"]["accountKey"].as_str() {
                        let key = raw
                            .strip_prefix("@appsetting('").and_then(|s| s.strip_suffix("')"))
                            .unwrap_or(raw);
                        if settings["Values"][key].as_str().unwrap_or("").is_empty() {
                            issues.push(format!("connection '{}': setting '{}' is empty", conn_name, key));
                        }
                    }
                } else {
                    // Connection referenced in workflow but not declared in connections.json
                    issues.push(format!("connection '{}' used but not in connections.json", conn_name));
                }
            }
        } else if !used.is_empty() {
            for conn_name in &used {
                issues.push(format!("connection '{}' used but connections.json is missing", conn_name));
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
    workflow_name:  &str,
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
    let settings_text = std::fs::read_to_string(dir.join("local.settings.json")).unwrap_or_default();

    let wf: Value        = serde_json::from_str(&wf_text).unwrap_or_default();
    let conn: Value      = serde_json::from_str(&conn_text).unwrap_or_default();
    let settings: Value  = serde_json::from_str(&settings_text).unwrap_or_default();

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
            if !used_connections.contains(name) { continue; }

            // Find the appsetting key for the primary endpoint field
            let pv = &provider["parameterValues"];
            let endpoint_fields = ["blobStorageEndpoint", "fullyQualifiedNamespace",
                                   "connectionString", "topicEndpoint", "VaultUri",
                                   "sshHostAddress"];
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
