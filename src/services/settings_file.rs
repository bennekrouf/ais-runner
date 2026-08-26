use crate::services::config;
use std::fs;

pub fn read_local_settings(logic_apps_dir: &str) -> Result<String, String> {
    let mut path = crate::services::workflows::resolve_logic_apps_dir(logic_apps_dir);
    path.push("local.settings.json");

    if !path.exists() {
        return Ok(String::new());
    }

    fs::read_to_string(&path).map_err(|e| format!("Failed to read file: {}", e))
}

pub fn write_local_settings(logic_apps_dir: &str, content: &str) -> Result<(), String> {
    // Validate JSON format
    let _: serde_json::Value =
        serde_json::from_str(content).map_err(|e| format!("Invalid JSON format: {}", e))?;

    let mut path = crate::services::workflows::resolve_logic_apps_dir(logic_apps_dir);
    path.push("local.settings.json");

    fs::write(&path, content).map_err(|e| format!("Failed to write file: {}", e))
}

/// Reads WORKFLOWS_SUBSCRIPTION_ID, WORKFLOWS_RESOURCE_GROUP_NAME, and WEBSITE_SITE_NAME
/// from local.settings.json and returns a WorkspaceLink if both required fields are present.
pub fn try_bootstrap_link(logic_apps_dir: &str) -> Option<config::WorkspaceLink> {
    let text = read_local_settings(logic_apps_dir).ok()?;
    if text.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let sub_id = v["Values"]["WORKFLOWS_SUBSCRIPTION_ID"]
        .as_str()?
        .to_string();
    let rg = v["Values"]["WORKFLOWS_RESOURCE_GROUP_NAME"]
        .as_str()?
        .to_string();
    if sub_id.is_empty() || rg.is_empty() {
        return None;
    }
    let site_name = v["Values"]["WEBSITE_SITE_NAME"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    // Normalise: users often write the short name ("sbns-foo") instead of the full FQDN.
    let sb_ns = v["Values"]["serviceBus_fullyQualifiedNamespace"]
        .as_str()
        .or_else(|| v["Values"]["serviceBus_namespace"].as_str())
        .filter(|s| !s.is_empty())
        .map(|s| crate::services::sb_check::normalise_sb_fqdn(s));
    Some(config::WorkspaceLink {
        subscription_id: sub_id,
        resource_group: rg,
        tenant_id: None,
        logic_app_name: site_name,
        sb_namespace: sb_ns,
        devops_org: None,
        devops_project: None,
    })
}
