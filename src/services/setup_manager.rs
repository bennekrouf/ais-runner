use std::fs;
use std::collections::HashMap;
use crate::services::{
    azure_cli,
    settings_file,
};

#[derive(Debug, Clone, PartialEq)]
pub enum SetupStatus {
    MissingSettings,
    NeedsInitialization,
    NeedsConfiguration(usize),
    MissingKeys(Vec<String>),
    Ready,
}

pub fn check_setup(dir: &str) -> SetupStatus {
    let p = crate::services::workflows::resolve_logic_apps_dir(dir);
    let settings_path = p.join("local.settings.json");
    let template_path = p.join("local.settings.json.template");

    if !settings_path.exists() {
        if template_path.exists() {
            return SetupStatus::NeedsInitialization;
        } else {
            return SetupStatus::MissingSettings;
        }
    }

    let settings_text = fs::read_to_string(&settings_path).unwrap_or_default();
    let settings: serde_json::Value = serde_json::from_str(&settings_text).unwrap_or_default();
    let vals = settings["Values"].as_object();

    let mut missing_count = 0;
    if let Some(v) = vals {
        for (key, val) in v {
            if let Some(s) = val.as_str() {
                let is_missing = s.contains("TODO") || (s.is_empty() && (
                    key.contains("KEY") ||
                    key.contains("CONNECTION") ||
                    key.contains("SUBSCRIPTION") ||
                    key.contains("RESOURCE_GROUP") ||
                    key.contains("siteName") ||
                    // WEBSITE_SITE_NAME must be non-empty: the Logic Apps runtime derives the
                    // Azurite table hash from it and only writes FLOWLOOKUP entries when set.
                    key == "WEBSITE_SITE_NAME"
                ));
                if is_missing {
                    missing_count += 1;
                }
            }
        }
    }

    if missing_count > 0 {
        return SetupStatus::NeedsConfiguration(missing_count);
    }

    // Check for missing keys required by connections.json
    let conn_path = p.join("connections.json");
    if conn_path.exists() {
        let conn_text = fs::read_to_string(conn_path).unwrap_or_default();
        let conn_json: serde_json::Value = serde_json::from_str(&conn_text).unwrap_or_default();
        let mut missing_keys = Vec::new();
        
        // Scan for both @appsetting('key') and @{appsetting('key')} forms
        let conn_str = conn_json.to_string();
        for cap in regex::Regex::new(r"@\{?appsetting\('([^']+)'\)\}?").unwrap().captures_iter(&conn_str) {
            let key = &cap[1];
            if let Some(v) = vals {
                if !v.contains_key(key) && !missing_keys.contains(&key.to_string()) {
                    missing_keys.push(key.to_string());
                }
            } else if !missing_keys.contains(&key.to_string()) {
                missing_keys.push(key.to_string());
            }
        }
        
        if !missing_keys.is_empty() {
            return SetupStatus::MissingKeys(missing_keys);
        }
    }

    SetupStatus::Ready
}

pub fn initialize_from_template(dir: &str) -> Result<(), String> {
    let p = crate::services::workflows::resolve_logic_apps_dir(dir);
    let settings_path = p.join("local.settings.json");
    let template_path = p.join("local.settings.json.template");

    if !template_path.exists() {
        return Err("Template not found".into());
    }

    fs::copy(template_path, settings_path).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn initialize_default(dir: &str) -> Result<(), String> {
    let p = crate::services::workflows::resolve_logic_apps_dir(dir);

    // 1. local.settings.json
    let settings_path = p.join("local.settings.json");
    let settings_content = r#"{
  "IsEncrypted": false,
  "Values": {
    "AzureWebJobsStorage": "UseDevelopmentStorage=true",
    "AzureWebJobsSecretStorageType": "Files",
    "FUNCTIONS_WORKER_RUNTIME": "node",
    "WEBSITE_SITE_NAME": "",
    "WORKFLOWS_SUBSCRIPTION_ID": "",
    "WORKFLOWS_RESOURCE_GROUP_NAME": "",
    "WORKFLOWS_LOCATION_NAME": "switzerlandnorth"
  }
}"#;
    if !settings_path.exists() {
        fs::write(settings_path, settings_content).map_err(|e| e.to_string())?;
    }

    // 2. package.json (required by Node worker)
    let pkg_path = p.join("package.json");
    let pkg_content = r#"{
  "name": "logic-apps",
  "version": "1.0.0",
  "dependencies": {}
}"#;
    if !pkg_path.exists() {
        fs::write(pkg_path, pkg_content).map_err(|e| e.to_string())?;
    }

    // 3. Fix connections.json if present: @{appsetting('key')} → @appsetting('key')
    //    The Logic Apps runtime rejects the ARM-template curly-brace form when running
    //    locally.  This causes a "functionConnections cannot be parsed" error that blocks
    //    ALL workflow registrations and therefore prevents FLOWLOOKUP entries from being
    //    written, making every trigger fail with "WorkflowNotFound".
    let conn_path = p.join("connections.json");
    if conn_path.exists() {
        if let Ok(raw) = fs::read_to_string(&conn_path) {
            let fixed = fix_connections_json(&raw);
            if fixed != raw {
                fs::write(&conn_path, fixed).map_err(|e| e.to_string())?;
            }
        }
    }

    Ok(())
}

/// Convert `@{appsetting('key')}` → `@appsetting('key')` throughout connections.json.
///
/// Azure Portal / VS Code sometimes exports connection references using ARM-template
/// interpolation syntax (`@{...}`).  The Logic Apps Standard local runtime only
/// understands the expression form without braces.  Mixed files cause the entire
/// `functionConnections` or `managedApiConnections` section to fail to parse.
pub fn fix_connections_json(raw: &str) -> String {
    // Validate it's JSON first; if not, return unchanged.
    if serde_json::from_str::<serde_json::Value>(raw).is_err() {
        return raw.to_string();
    }
    let fixed = regex::Regex::new(r"@\{appsetting\(([^)]+)\)\}")
        .unwrap()
        .replace_all(raw, "@appsetting($1)")
        .to_string();
    fixed
}

/// Attempts to auto-discover SB namespace and SQL server in the given resource group
/// and updates local.settings.json with the values.
///
/// `logic_app_name` is written to `WEBSITE_SITE_NAME` — the Logic Apps runtime needs this
/// to be non-empty so it can write FLOWLOOKUP entries into Azurite table storage on startup.
/// Without it all workflow triggers return "WorkflowNotFound".
pub fn auto_detect_resources(
    dir: &str,
    subscription_id: Option<&str>,
    resource_group: &str,
    logic_app_name: Option<&str>,
) -> Result<String, String> {
    let mut messages = Vec::new();

    // 0. Update identity fields: subscription, resource group, and site name.
    //    WEBSITE_SITE_NAME and WORKFLOWS_RESOURCE_GROUP_NAME *must* be non-empty for the
    //    Logic Apps runtime to write FLOWLOOKUP entries into Azurite on startup.
    if let Ok(text) = settings_file::read_local_settings(dir) {
        if let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(vals) = root["Values"].as_object_mut() {
                let mut changed = false;

                if let Some(sub_id) = subscription_id {
                    let key = "WORKFLOWS_SUBSCRIPTION_ID";
                    if vals.get(key).and_then(|v| v.as_str()).unwrap_or("").is_empty() {
                        vals.insert(key.to_string(), serde_json::Value::String(sub_id.to_string()));
                        messages.push(format!("✅ Set subscription: {}", sub_id));
                        changed = true;
                    }
                }

                {
                    let key = "WORKFLOWS_RESOURCE_GROUP_NAME";
                    if vals.get(key).and_then(|v| v.as_str()).unwrap_or("").is_empty() && !resource_group.is_empty() {
                        vals.insert(key.to_string(), serde_json::Value::String(resource_group.to_string()));
                        messages.push(format!("✅ Set resource group: {}", resource_group));
                        changed = true;
                    }
                }

                if let Some(name) = logic_app_name {
                    if !name.is_empty() {
                        let key = "WEBSITE_SITE_NAME";
                        if vals.get(key).and_then(|v| v.as_str()).unwrap_or("").is_empty() {
                            vals.insert(key.to_string(), serde_json::Value::String(name.to_string()));
                            messages.push(format!("✅ Set site name (WEBSITE_SITE_NAME): {}", name));
                            changed = true;
                        }
                    }
                }

                if changed {
                    let new_text = serde_json::to_string_pretty(&root).unwrap_or_default();
                    let _ = settings_file::write_local_settings(dir, &new_text);
                }
            }
        }
    }

    // 1. Service Bus
    match azure_cli::sb_list_namespaces() {
        Ok(list) => {
            let matches: Vec<_> = list.iter()
                .filter(|(_, _, rg)| rg.to_lowercase() == resource_group.to_lowercase())
                .collect();
            
            if matches.len() == 1 {
                let fqdn = &matches[0].1;
                if let Ok(text) = settings_file::read_local_settings(dir) {
                    if let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Some(vals) = root["Values"].as_object_mut() {
                            // Find the key used in connections.json for SB
                            let sb_key = "serviceBus_fullyQualifiedNamespace"; 
                            vals.insert(sb_key.to_string(), serde_json::Value::String(fqdn.clone()));
                            
                            let new_text = serde_json::to_string_pretty(&root).unwrap_or_default();
                            let _ = settings_file::write_local_settings(dir, &new_text);
                            messages.push(format!("✅ Auto-detected SB namespace: {}", fqdn));
                        }
                    }
                }
            } else if matches.len() > 1 {
                messages.push(format!("ℹ Found {} SB namespaces in RG — please pick one manually.", matches.len()));
            }
        }
        Err(e) => { messages.push(format!("❌ Failed to list SB namespaces: {:?}", e)); }
    }

    // Fix connections.json @{appsetting} syntax if present
    let conn_path = crate::services::workflows::resolve_logic_apps_dir(dir).join("connections.json");
    if conn_path.exists() {
        if let Ok(raw) = std::fs::read_to_string(&conn_path) {
            let fixed = fix_connections_json(&raw);
            if fixed != raw {
                let _ = std::fs::write(&conn_path, fixed);
                messages.push("✅ Fixed connections.json: @{appsetting(...)} → @appsetting(...) (ARM-template syntax is not supported locally)".into());
            }
        }
    }

    if messages.is_empty() {
        Ok("No resources found to auto-detect.".into())
    } else {
        Ok(messages.join("\n"))
    }
}

/// Writes smart defaults for every key in `missing_keys` that isn't already in local.settings.json.
/// Idempotent — never overwrites existing values.
pub fn stub_missing_keys(dir: &str, missing_keys: &[String]) -> Result<(), String> {
    let stubs: HashMap<String, String> = missing_keys.iter()
        .map(|k| (k.clone(), smart_default(k)))
        .collect();
    apply_settings(dir, stubs)
}

/// Returns a sensible local-dev default for a known key pattern, or empty string.
///
/// Rules discovered by comparing a working project (ais_platform) against a broken one
/// (ais_tom_platform) and tracing every func-startup validation failure:
///
/// 1. `*_blobStorageEndpoint` → Azurite blob endpoint
/// 2. `azureFunction_*_appKey` → `"placeholder"` (empty string causes functionConnections parse error)
/// 3. `azureFunction_*_triggerUrl` → inferred local Java-func URL
/// 4. `*_connectionUrl` → APIM-formatted placeholder URL containing the api name so the
///    Logic Apps runtime validator can extract api-name + connection-name from the path.
///    Format: `https://placeholder.azure-apim.net/apim/{api-name}/placeholder/`
///    Empty string = "missing required property"; `https://placeholder.logic.azure.com` = "invalid
///    connection runtime url — api name and connection name should not be null or empty".
pub fn smart_default(key: &str) -> String {
    const AZURITE: &str = "http://127.0.0.1:10000/devstoreaccount1";

    match key {
        // ── Blob endpoints ────────────────────────────────────────────────────
        "AzureBlob_blobStorageEndpoint"    => AZURITE.into(),
        "IgniteBlob_blobStorageEndpoint"   => AZURITE.into(),

        // ── Standard func-host settings ───────────────────────────────────────
        "AzureWebJobsStorage"      => "UseDevelopmentStorage=true".into(),
        "FUNCTIONS_WORKER_RUNTIME" => "node".into(),
        "WORKFLOWS_LOCATION_NAME"  => "switzerlandnorth".into(),

        // ── Java function connections ─────────────────────────────────────────
        // appKey MUST be non-empty ("placeholder") — empty string triggers a
        // functionConnections parse error that blocks ALL workflow registrations.
        k if k.starts_with("azureFunction_") && k.ends_with("_appKey") => {
            "placeholder".into()
        }

        // triggerUrl — infer from key name: "azureFunction_{Name}_triggerUrl"
        k if k.starts_with("azureFunction_") && k.ends_with("_triggerUrl") => {
            let name = k
                .strip_prefix("azureFunction_").unwrap_or(k)
                .strip_suffix("_triggerUrl").unwrap_or(k);
            format!("http://localhost:7072/api/{}", name)
        }

        // ── Managed API connection URLs ────────────────────────────────────────
        // Must be a valid APIM-style URL: https://…/apim/{api-name}/{conn-name}/
        // The Logic Apps runtime parses this URL to extract api-name and conn-name.
        // A generic placeholder like "https://placeholder.logic.azure.com" fails
        // validation with "api name and connection name should not be null or empty".
        "Teams_connectionUrl" =>
            "https://placeholder.azure-apim.net/apim/teams/teams-local/".into(),
        "LogAnalytics_connectionUrl" =>
            "https://placeholder.azure-apim.net/apim/azureloganalyticsdatacollector/loganalytics-local/".into(),
        "Sharepoint_connectionUrl" =>
            "https://placeholder.azure-apim.net/apim/sharepointonline/sharepoint-local/".into(),
        "Office365_connectionUrl" =>
            "https://placeholder.azure-apim.net/apim/office365/office365-local/".into(),
        "Salesforce_connectionUrl" =>
            "https://placeholder.azure-apim.net/apim/salesforce/salesforce-local/".into(),
        "AcsEmail_connectionUrl" =>
            "https://placeholder.azure-apim.net/apim/acsemail/acsemail-local/".into(),
        // Generic pattern: if the key ends in _connectionUrl, infer the api name
        k if k.ends_with("_connectionUrl") => {
            let api = k.strip_suffix("_connectionUrl").unwrap_or(k).to_lowercase();
            // Strip common suffixes to get the api name
            let api = api.strip_suffix("_url").unwrap_or(&api);
            format!("https://placeholder.azure-apim.net/apim/{}/{}-local/", api, api)
        }

        // ── Everything else ────────────────────────────────────────────────────
        _ => String::new(),
    }
}

pub fn apply_settings(dir: &str, updates: HashMap<String, String>) -> Result<(), String> {
    let text = settings_file::read_local_settings(dir)?;
    let mut root: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    
    if let Some(vals) = root["Values"].as_object_mut() {
        for (k, v) in updates {
            vals.insert(k, serde_json::Value::String(v));
        }
    }

    let new_text = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    settings_file::write_local_settings(dir, &new_text)?;
    Ok(())
}
