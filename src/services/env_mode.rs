use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum EnvMode {
    Local,   // all blob endpoints → Azurite
    Azure,   // all blob endpoints → real Azure
    Mixed,   // some local, some azure
    Unknown, // no blob endpoint keys found
}

/// The Azurite blob service URL used locally.
pub const AZURITE_BLOB: &str = "http://127.0.0.1:10000/devstoreaccount1";

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

pub struct SwitchResult {
    /// Keys that were successfully set.
    pub _switched: Vec<String>,
    /// Keys with no cached Azure URL — need CLI fetch before they can go Azure.
    pub needs_fetch: Vec<String>,
}

/// Switch all blob endpoints to Azurite. Backs up current Azure URLs as `{key}_azure`.
pub fn switch_to_local(dir: &str) -> Result<SwitchResult, String> {
    let (mut root, mut values) = read_root(dir)?;
    let keys = blob_keys(&values);
    let mut switched = Vec::new();

    for key in &keys {
        let current = values.get(key).cloned().unwrap_or_default();
        if !is_azurite(&current) && !current.is_empty() {
            values.insert(format!("{}_azure", key), current);
        }
        values.insert(key.clone(), AZURITE_BLOB.to_string());
        switched.push(key.clone());
    }

    write_root(dir, &mut root, values)?;
    Ok(SwitchResult { _switched: switched, needs_fetch: vec![] })
}

/// Switch all blob endpoints to Azure, restoring from `{key}_azure` cache where available.
/// Returns keys that were restored and keys that still need a CLI fetch.
pub fn switch_to_azure(dir: &str) -> Result<SwitchResult, String> {
    let (mut root, mut values) = read_root(dir)?;
    let keys = blob_keys(&values);
    let mut switched = Vec::new();
    let mut needs_fetch = Vec::new();

    for key in &keys {
        let backup_key = format!("{}_azure", key);
        match values.get(&backup_key).filter(|v| !v.is_empty()).cloned() {
            Some(azure_url) => {
                values.insert(key.clone(), azure_url);
                switched.push(key.clone());
            }
            None => {
                needs_fetch.push(key.clone());
            }
        }
    }

    write_root(dir, &mut root, values)?;
    Ok(SwitchResult { _switched: switched, needs_fetch })
}

/// Store a fetched Azure endpoint for `key`, update the active value and `_azure` cache.
pub fn apply_fetched_endpoint(dir: &str, key: &str, endpoint: &str) -> Result<(), String> {
    let (mut root, mut values) = read_root(dir)?;
    let trimmed = endpoint.trim_end_matches('/').to_string();
    values.insert(key.to_string(), trimmed.clone());
    values.insert(format!("{}_azure", key), trimmed);
    write_root(dir, &mut root, values)?;
    Ok(())
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn read_values(dir: &str) -> Option<HashMap<String, String>> {
    let text = std::fs::read_to_string(
        std::path::Path::new(dir).join("local.settings.json"),
    ).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v["Values"].as_object().map(|m| {
        m.iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect()
    })
}

fn read_root(dir: &str) -> Result<(Value, HashMap<String, String>), String> {
    let text = std::fs::read_to_string(
        std::path::Path::new(dir).join("local.settings.json"),
    ).map_err(|e| e.to_string())?;
    let root: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let values: HashMap<String, String> = root["Values"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Ok((root, values))
}

fn write_root(dir: &str, root: &mut Value, values: HashMap<String, String>) -> Result<(), String> {
    if let Some(obj) = root["Values"].as_object_mut() {
        for (k, v) in &values {
            obj.insert(k.clone(), Value::String(v.clone()));
        }
    }
    let text = serde_json::to_string_pretty(root).map_err(|e| e.to_string())?;
    std::fs::write(
        std::path::Path::new(dir).join("local.settings.json"),
        text,
    ).map_err(|e| e.to_string())
}
