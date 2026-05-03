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

/// List all Logic Apps Standard sites in the given subscription (or the active az account).
///
/// Matches any site whose `kind` contains "workflowapp" — covers:
///   • Windows: `functionapp,workflowapp`
///   • Linux:   `functionapp,workflowapp,linux`
///   • Rare:    `workflowapp`
pub fn list_logic_app_sites(subscription: Option<&str>) -> Result<Vec<LogicAppSite>, AzError> {
    let mut args = vec![
        "resource", "list",
        "--resource-type", "Microsoft.Web/sites",
        "--query", "[?contains(kind, 'workflowapp')].{name:name,rg:resourceGroup,id:id,kind:kind}",
        "-o", "json",
    ];
    let sub_owned: String;
    if let Some(sub) = subscription {
        sub_owned = sub.to_string();
        args.push("--subscription");
        args.push(&sub_owned);
    }

    let out = az_command(&args)
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

    let arr: Vec<Value> = serde_json::from_str(raw.trim()).unwrap_or_default();

    let mut sites: Vec<LogicAppSite> = arr.iter().filter_map(|v| {
        let name = v["name"].as_str()?.to_string();
        let rg   = v["rg"].as_str()?.to_string();
        // subscriptionId is not a top-level field — parse it from the resource id:
        // "/subscriptions/{sub}/resourceGroups/..."
        let id   = v["id"].as_str().unwrap_or("");
        let sub  = id.split('/').nth(2).unwrap_or("").to_string();
        if name.is_empty() || sub.is_empty() { return None; }
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
            let raw_name = item["name"].as_str().unwrap_or("");
            if raw_name.is_empty() { continue; }
            // API returns "site-name/workflow-name" — strip the site prefix
            let name = raw_name.splitn(2, '/').nth(1).unwrap_or(raw_name).to_string();
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

        // The REST API nests the content under properties.files["workflow.json"]
        let wf_file    = &v["properties"]["files"]["workflow.json"];
        let definition = wf_file["definition"].clone();
        let kind       = wf_file["kind"].as_str().unwrap_or("Stateful");

        if definition.is_null() {
            return Err(AzError::Other("No definition in Azure response".into()));
        }

        let output = serde_json::json!({ "definition": definition, "kind": kind });
        return serde_json::to_string_pretty(&output)
            .map_err(|e| AzError::Other(e.to_string()));
    }

    Err(AzError::Other("Still throttled after retries — try again in a minute".into()))
}

// ── diff helpers ──────────────────────────────────────────────────────────────

/// Compare the local `{local_dir}/{workflow}/workflow.json` with the live Azure version.
/// Returns `Ok(0)` when identical, `Ok(n)` with the number of changed lines when different,
/// `Err` when the local file is missing or the Azure fetch fails.
pub fn diff_workflow_vs_local(
    subscription: &str, rg: &str, site: &str, workflow: &str, local_dir: &str,
) -> Result<usize, AzError> {
    let local_path = std::path::Path::new(local_dir).join(workflow).join("workflow.json");
    let local_str  = std::fs::read_to_string(&local_path)
        .map_err(|e| AzError::Other(format!("read local: {}", e)))?;

    let remote_str = download_workflow(subscription, rg, site, workflow)?;

    // Parse both so formatting differences don't count as changes
    let local_val: Value  = serde_json::from_str(&local_str)
        .map_err(|e| AzError::Other(format!("parse local: {}", e)))?;
    let remote_val: Value = serde_json::from_str(&remote_str)
        .map_err(|e| AzError::Other(format!("parse remote: {}", e)))?;

    if local_val == remote_val {
        return Ok(0);
    }

    let local_pp  = serde_json::to_string_pretty(&local_val).unwrap_or_default();
    let remote_pp = serde_json::to_string_pretty(&remote_val).unwrap_or_default();
    Ok(line_diff_count(&local_pp, &remote_pp).max(1))
}

/// Count lines that appear in `a` but not `b` plus lines in `b` but not `a`
/// (multiset symmetric difference — fast, good enough for JSON blobs).
fn line_diff_count(a: &str, b: &str) -> usize {
    use std::collections::HashMap;
    let mut freq: HashMap<&str, i32> = HashMap::new();
    for l in a.lines() { *freq.entry(l).or_default() += 1; }
    let mut diff = 0usize;
    for l in b.lines() {
        match freq.get_mut(l) {
            Some(n) if *n > 0 => *n -= 1,
            _ => diff += 1,
        }
    }
    diff += freq.values().filter(|&&n| n > 0).map(|&n| n as usize).sum::<usize>();
    diff
}

/// Read the subscription ID from local.settings.json (WORKFLOWS_SUBSCRIPTION_ID).
pub fn detect_subscription(logic_apps_dir: &str) -> Option<String> {
    let text = std::fs::read_to_string(
        std::path::Path::new(logic_apps_dir).join("local.settings.json")
    ).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let sub = v["Values"]["WORKFLOWS_SUBSCRIPTION_ID"].as_str()?.to_string();
    if sub.is_empty() { None } else { Some(sub) }
}
