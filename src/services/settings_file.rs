use std::fs;
use std::path::PathBuf;

pub fn read_local_settings(logic_apps_dir: &str) -> Result<String, String> {
    let mut path = PathBuf::from(logic_apps_dir);
    path.push("local.settings.json");

    if !path.exists() {
        return Ok(String::new());
    }

    fs::read_to_string(&path).map_err(|e| format!("Failed to read file: {}", e))
}

pub fn write_local_settings(logic_apps_dir: &str, content: &str) -> Result<(), String> {
    // Validate JSON format
    let _: serde_json::Value = serde_json::from_str(content)
        .map_err(|e| format!("Invalid JSON format: {}", e))?;

    let mut path = PathBuf::from(logic_apps_dir);
    path.push("local.settings.json");

    fs::write(&path, content).map_err(|e| format!("Failed to write file: {}", e))
}

/// Reads WORKFLOWS_SUBSCRIPTION_ID from local.settings.json.
pub fn read_subscription_id(logic_apps_dir: &str) -> Option<String> {
    let text = read_local_settings(logic_apps_dir).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let id = v["Values"]["WORKFLOWS_SUBSCRIPTION_ID"].as_str()?.to_string();
    if id.is_empty() { None } else { Some(id) }
}
