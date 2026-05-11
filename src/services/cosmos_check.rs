/// Well-known Cosmos DB Emulator defaults (Windows + Linux Docker emulator).
/// The data-explorer UI runs on port 1234; the actual document API is on 8081.
pub const EMULATOR_ENDPOINT: &str = "https://localhost:8081/";
pub const EMULATOR_KEY: &str =
    "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==";

#[derive(Debug, Clone, PartialEq)]
pub struct CosmosConnection {
    pub connection_name: String,
    pub display_name:    String,
    /// appsetting key for accountEndpoint
    pub endpoint_key:    Option<String>,
    /// appsetting key for authenticationPolicy / account key
    pub key_key:         Option<String>,
    /// Resolved values
    pub endpoint:        String,
    pub account_key:     String,
}

fn extract_appsetting(val: &str) -> Option<String> {
    val.strip_prefix("@appsetting('")
        .and_then(|s| s.strip_suffix("')"))
        .map(|s| s.to_string())
}

/// Scan connections.json for /serviceProviders/documentDb entries and resolve
/// their settings from local.settings.json.
pub fn detect_cosmos_connections(logic_apps_dir: &str) -> Vec<CosmosConnection> {
    let dir = crate::services::workflows::resolve_logic_apps_dir(logic_apps_dir);

    let conn_text = match std::fs::read_to_string(dir.join("connections.json")) {
        Ok(t) => t,
        Err(_) => return vec![],
    };
    let conn_json: serde_json::Value = match serde_json::from_str(&conn_text) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let settings: serde_json::Value =
        std::fs::read_to_string(dir.join("local.settings.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();

    let mut result = Vec::new();
    if let Some(providers) = conn_json["serviceProviderConnections"].as_object() {
        for (name, conn) in providers {
            let provider_id = conn["serviceProvider"]["id"].as_str().unwrap_or("");
            let is_cosmos = provider_id == "/serviceProviders/documentDb"   // original (rejected by some runtimes)
                         || provider_id == "/serviceProviders/cosmosDb"     // camelCase variant
                         || provider_id == "/serviceProviders/documentDB";  // capital-DB variant
            if !is_cosmos { continue; }
            let display = conn["displayName"].as_str().unwrap_or(name).to_string();
            let pv = &conn["parameterValues"];

            let endpoint_key = pv["accountEndpoint"].as_str()
                .and_then(extract_appsetting);
            let key_key = pv["authenticationPolicy"]["credential"]["accountKey"].as_str()
                .or_else(|| pv["accountKey"].as_str())
                .and_then(extract_appsetting);

            let resolve = |k: &Option<String>| -> String {
                k.as_deref()
                    .and_then(|k| settings["Values"][k].as_str())
                    .unwrap_or("")
                    .to_string()
            };

            result.push(CosmosConnection {
                connection_name: name.clone(),
                display_name:    display,
                endpoint:        resolve(&endpoint_key),
                account_key:     resolve(&key_key),
                endpoint_key,
                key_key,
            });
        }
    }
    result.sort_by(|a, b| a.connection_name.cmp(&b.connection_name));
    result
}

/// Test connectivity by GET-ing the Cosmos DB root endpoint (returns account info, no auth needed).
/// Accepts self-signed TLS — the emulator uses a self-signed cert.
pub async fn test_cosmos_endpoint(endpoint: &str) -> Result<u64, String> {
    if endpoint.is_empty() {
        return Err("No endpoint configured".into());
    }
    let start = std::time::Instant::now();
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?;
    let url = endpoint.trim_end_matches('/').to_string();
    let resp = client.get(&url).send().await.map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    // 200 = account info returned, 401 = reachable but needs auth (also fine for connectivity check)
    if status == 200 || status == 401 {
        Ok(start.elapsed().as_millis() as u64)
    } else {
        Err(format!("HTTP {}", status))
    }
}
