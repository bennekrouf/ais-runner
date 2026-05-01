use serde_json::Value;
use crate::services::azure_cli::{az_command, AzError};

#[derive(Debug, Clone, PartialEq)]
pub struct AzureWorkflow {
    pub name:    String,
    pub healthy: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LogicAppSite {
    pub name:           String,
    pub resource_group: String,
    pub subscription:   String,
}

/// List all Logic Apps Standard sites accessible in the current subscription(s).
pub fn list_logic_app_sites() -> Result<Vec<LogicAppSite>, AzError> {
    let out = az_command(&[
        "resource", "list",
        "--resource-type", "Microsoft.Web/sites",
        "--query", "[?kind=='functionapp,workflowapp' || kind=='workflowapp'].{name:name,rg:resourceGroup,sub:subscriptionId}",
        "-o", "json",
    ])
    .output()
    .map_err(|e| AzError::Other(format!("az not found: {}", e)))?;

    let raw    = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    if !out.status.success() {
        if stderr.contains("AADSTS") || stderr.contains("az login") || stderr.contains("refresh token") {
            return Err(AzError::NotLoggedIn);
        }
        return Err(AzError::Other(stderr.trim().to_string()));
    }

    let arr: Vec<Value> = serde_json::from_str(raw.trim())
        .unwrap_or_default();

    let mut sites: Vec<LogicAppSite> = arr.iter().filter_map(|v| {
        let name = v["name"].as_str()?.to_string();
        let rg   = v["rg"].as_str()?.to_string();
        let sub  = v["sub"].as_str()?.to_string();
        if name.is_empty() { return None; }
        Some(LogicAppSite { name, resource_group: rg, subscription: sub })
    }).collect();
    sites.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(sites)
}

/// List all workflows in a Logic Apps Standard site.
pub fn list_azure_workflows(subscription: &str, rg: &str, site: &str)
    -> Result<Vec<AzureWorkflow>, AzError>
{
    let url = format!(
        "https://management.azure.com/subscriptions/{}/resourceGroups/{}/providers/Microsoft.Web/sites/{}/workflows?api-version=2022-03-01",
        subscription, rg, site
    );
    let out = az_command(&["rest", "--method", "GET", "--url", &url])
        .output()
        .map_err(|e| AzError::Other(format!("az not found: {}", e)))?;

    let raw    = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    if !out.status.success() {
        if stderr.contains("AADSTS") || stderr.contains("az login") || stderr.contains("refresh token") {
            return Err(AzError::NotLoggedIn);
        }
        return Err(AzError::Other(stderr.trim().to_string()));
    }

    let v: Value = serde_json::from_str(&raw)
        .map_err(|e| AzError::Other(format!("parse error: {}", e)))?;

    let mut result = Vec::new();
    if let Some(items) = v["value"].as_array() {
        for item in items {
            let name = item["name"].as_str().unwrap_or("").to_string();
            if name.is_empty() { continue; }
            let healthy = item["properties"]["health"]["state"].as_str()
                .unwrap_or("") == "Healthy";
            result.push(AzureWorkflow { name, healthy });
        }
    }
    result.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(result)
}

/// Download a workflow definition from Azure, retrying on 429.
/// Returns pretty-printed workflow.json content.
pub fn download_workflow(subscription: &str, rg: &str, site: &str, workflow: &str)
    -> Result<String, AzError>
{
    let url = format!(
        "https://management.azure.com/subscriptions/{}/resourceGroups/{}/providers/Microsoft.Web/sites/{}/workflows/{}?api-version=2022-03-01",
        subscription, rg, site, workflow
    );

    for attempt in 0..6u64 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_secs(10 * attempt));
        }

        let out = az_command(&["rest", "--method", "GET", "--url", &url])
            .output()
            .map_err(|e| AzError::Other(format!("az not found: {}", e)))?;

        let raw    = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();

        if !out.status.success() {
            if stderr.contains("429") || stderr.contains("Too Many Requests") {
                continue;
            }
            if stderr.contains("AADSTS") || stderr.contains("az login") {
                return Err(AzError::NotLoggedIn);
            }
            return Err(AzError::Other(stderr.trim().to_string()));
        }

        let v: Value = serde_json::from_str(&raw)
            .map_err(|e| AzError::Other(format!("parse error: {}", e)))?;

        let definition = v["properties"]["definition"].clone();
        let kind       = v["properties"]["kind"].as_str().unwrap_or("Stateful");

        if definition.is_null() {
            return Err(AzError::Other("No definition in Azure response".into()));
        }

        let output = serde_json::json!({ "definition": definition, "kind": kind });
        return serde_json::to_string_pretty(&output)
            .map_err(|e| AzError::Other(e.to_string()));
    }

    Err(AzError::Other("Still throttled after retries — try again in a minute".into()))
}

/// Read the subscription ID from local.settings.json.
pub fn detect_subscription(logic_apps_dir: &str) -> Option<String> {
    let text = std::fs::read_to_string(
        std::path::Path::new(logic_apps_dir).join("local.settings.json")
    ).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let sub = v["Values"]["WORKFLOWS_SUBSCRIPTION_ID"].as_str()?.to_string();
    if sub.is_empty() { None } else { Some(sub) }
}
