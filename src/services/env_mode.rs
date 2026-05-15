use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum EnvMode {
    Local,   // all blob endpoints → Azurite
    Azure,   // all blob endpoints → real Azure
    Mixed,   // some local, some azure
    Unknown, // no blob endpoint keys found
}

pub fn is_azurite(url: &str) -> bool {
    url.contains("127.0.0.1:10000")
        || url.contains("localhost:10000")
        || url.to_lowercase().contains("usedevelopmentstorage=true")
}

/// All `*_blobStorageEndpoint` keys (excluding `_azure` backup variants).
pub fn blob_keys(settings: &HashMap<String, String>) -> Vec<String> {
    let mut keys: Vec<String> = settings
        .keys()
        .filter(|k| k.ends_with("_blobStorageEndpoint") && !k.ends_with("_azure"))
        .cloned()
        .collect();
    keys.sort();
    keys
}

pub fn detect_mode(dir: &str) -> EnvMode {
    let settings = read_values(dir).unwrap_or_default();
    let keys = blob_keys(&settings);
    if keys.is_empty() {
        return EnvMode::Unknown;
    }
    let local = keys.iter().filter(|k| is_azurite(settings.get(*k).map(|s| s.as_str()).unwrap_or(""))).count();
    let azure = keys.iter().filter(|k| {
        let v = settings.get(*k).map(|s| s.as_str()).unwrap_or("");
        !v.is_empty() && !is_azurite(v)
    }).count();
    match (local, azure) {
        (l, 0) if l > 0 => EnvMode::Local,
        (0, a) if a > 0 => EnvMode::Azure,
        _ => EnvMode::Mixed,
    }
}

fn read_values(dir: &str) -> Option<HashMap<String, String>> {
    let dir_path = crate::services::workflows::resolve_logic_apps_dir(dir);
    let text = std::fs::read_to_string(dir_path.join("local.settings.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v["Values"].as_object().map(|m| {
        m.iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect()
    })
}
