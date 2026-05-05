use serde_json::Value;

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
