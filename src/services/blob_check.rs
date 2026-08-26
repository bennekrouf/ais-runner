use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct BlobConnection {
    pub connection_name: String,
    pub display_name: String,
    pub endpoint_key: String, // appsetting key for blobStorageEndpoint
    pub endpoint: String,     // resolved value (empty if not set)
    pub configured: bool,
}

/// Reads the current value of AzureWebJobsStorage from local.settings.json.
pub fn read_webjobs_storage(logic_apps_dir: &str) -> String {
    let dir = crate::services::workflows::resolve_logic_apps_dir(logic_apps_dir);
    let text = std::fs::read_to_string(dir.join("local.settings.json")).unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    v["Values"]["AzureWebJobsStorage"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

/// Scans connections.json for AzureBlob serviceProviderConnections and resolves
/// their blobStorageEndpoint appsetting keys against local.settings.json.
pub fn detect_blob_connections(logic_apps_dir: &str) -> Vec<BlobConnection> {
    let dir = crate::services::workflows::resolve_logic_apps_dir(logic_apps_dir);

    let conn_text = match std::fs::read_to_string(dir.join("connections.json")) {
        Ok(t) => t,
        Err(_) => return vec![],
    };
    let conn_json: Value = match serde_json::from_str(&conn_text) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let settings_text =
        std::fs::read_to_string(dir.join("local.settings.json")).unwrap_or_default();
    let settings: Value = serde_json::from_str(&settings_text).unwrap_or_default();

    let mut result = Vec::new();
    if let Some(providers) = conn_json["serviceProviderConnections"].as_object() {
        for (name, conn) in providers {
            if conn["serviceProvider"]["id"].as_str() != Some("/serviceProviders/AzureBlob") {
                continue;
            }
            let display = conn["displayName"].as_str().unwrap_or(name).to_string();
            let pv = &conn["parameterValues"];

            let raw = match pv["blobStorageEndpoint"].as_str() {
                Some(r) => r,
                None => continue,
            };
            let key = raw
                .strip_prefix("@appsetting('")
                .and_then(|s| s.strip_suffix("')"))
                .unwrap_or(raw)
                .to_string();
            let endpoint = settings["Values"][&key].as_str().unwrap_or("").to_string();
            let configured = !endpoint.is_empty();

            result.push(BlobConnection {
                connection_name: name.clone(),
                display_name: display,
                endpoint_key: key,
                endpoint,
                configured,
            });
        }
    }
    result.sort_by(|a, b| a.connection_name.cmp(&b.connection_name));
    result
}
