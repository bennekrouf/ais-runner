/// Well-known Cosmos DB Emulator defaults for the vnext-preview Linux Docker image.
/// That image serves plain HTTP on 8081 (no TLS) — use http://, not https://.
/// The Windows installer emulator still uses HTTPS; test_cosmos_endpoint handles both.
/// Data-explorer UI is on port 1234.
pub const EMULATOR_ENDPOINT: &str = "http://localhost:8081";
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
            let pid_lower = provider_id.to_lowercase();
            let is_cosmos = pid_lower == "/serviceproviders/azurecosmosdb"
                         || pid_lower == "/serviceproviders/cosmosdb"
                         || pid_lower == "/serviceproviders/documentdb";
            if !is_cosmos { continue; }
            let display = conn["displayName"].as_str().unwrap_or(name).to_string();
            let pv = &conn["parameterValues"];

            let resolve_setting = |k: &Option<String>| -> String {
                k.as_deref()
                    .and_then(|k| settings["Values"][k].as_str())
                    .unwrap_or("")
                    .to_string()
            };

            // Two supported parameter layouts:
            // 1. connectionString  →  "AccountEndpoint=...;AccountKey=..."
            // 2. accountEndpoint + authenticationPolicy.credential.accountKey (legacy)
            let conn_str_key = pv["connectionString"].as_str().and_then(extract_appsetting);
            let (endpoint, account_key, endpoint_key, key_key) = if let Some(ref cs_key) = conn_str_key {
                let cs = resolve_setting(&conn_str_key);
                let endpoint = cs.split(';')
                    .find_map(|p| p.strip_prefix("AccountEndpoint=").map(str::to_string))
                    .unwrap_or_default();
                let key = cs.split(';')
                    .find_map(|p| p.strip_prefix("AccountKey=").map(str::to_string))
                    .unwrap_or_default();
                (endpoint, key, Some(cs_key.clone()), Some(cs_key.clone()))
            } else {
                let endpoint_key = pv["accountEndpoint"].as_str().and_then(extract_appsetting);
                let key_key = pv["authenticationPolicy"]["credential"]["accountKey"].as_str()
                    .or_else(|| pv["accountKey"].as_str())
                    .and_then(extract_appsetting);
                (resolve_setting(&endpoint_key), resolve_setting(&key_key), endpoint_key, key_key)
            };

            result.push(CosmosConnection {
                connection_name: name.clone(),
                display_name:    display,
                endpoint,
                account_key,
                endpoint_key,
                key_key,
            });
        }
    }
    result.sort_by(|a, b| a.connection_name.cmp(&b.connection_name));
    result
}

/// Test connectivity by GET-ing the Cosmos DB root endpoint (returns account info, no auth needed).
/// Handles both the Windows emulator (HTTPS + self-signed cert) and the vnext-preview Linux
/// Docker emulator (plain HTTP on the same port).
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
    let result = client.get(&url).send().await;

    // The vnext-preview Linux emulator serves plain HTTP even if the configured URL says https.
    // If TLS negotiation fails, retry with http:// on the same host:port.
    let resp = match result {
        Ok(r) => r,
        Err(ref e) if e.to_string().contains("wrong version number")
                   || e.to_string().contains("tls")
                   || e.to_string().contains("ssl")
                   || e.to_string().contains("SSL") =>
        {
            let http_url = url.replacen("https://", "http://", 1);
            client.get(&http_url).send().await.map_err(|e2| e2.to_string())?
        }
        Err(e) => return Err(e.to_string()),
    };

    let status = resp.status().as_u16();
    // 200 = account info returned, 401 = reachable but needs auth (also fine for connectivity check)
    if status == 200 || status == 401 {
        Ok(start.elapsed().as_millis() as u64)
    } else {
        Err(format!("HTTP {}", status))
    }
}
