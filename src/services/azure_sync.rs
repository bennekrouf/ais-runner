use crate::services::azure_cli::{az_command, AzError};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct AzureWorkflow {
    pub name: String,
    pub healthy: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LogicAppSite {
    pub name: String,
    pub resource_group: String,
    pub subscription: String,
}

/// List all Logic Apps Standard sites in the given subscription (or the active az account).
///
/// Matches any site whose `kind` contains "workflowapp" — covers:
///   • Windows: `functionapp,workflowapp`
///   • Linux:   `functionapp,workflowapp,linux`
///   • Rare:    `workflowapp`
pub fn list_logic_app_sites(subscription: Option<&str>) -> Result<Vec<LogicAppSite>, AzError> {
    let mut args = vec![
        "resource",
        "list",
        "--resource-type",
        "Microsoft.Web/sites",
        "--query",
        "[?contains(kind, 'workflowapp')].{name:name,rg:resourceGroup,id:id,kind:kind}",
        "-o",
        "json",
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

    let raw = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    if !out.status.success() {
        if stderr.contains("AADSTS")
            || stderr.contains("az login")
            || stderr.contains("refresh token")
        {
            return Err(AzError::NotLoggedIn);
        }
        return Err(AzError::Other(stderr.trim().to_string()));
    }

    let arr: Vec<Value> = serde_json::from_str(raw.trim()).unwrap_or_default();

    let mut sites: Vec<LogicAppSite> = arr
        .iter()
        .filter_map(|v| {
            let name = v["name"].as_str()?.to_string();
            let rg = v["rg"].as_str()?.to_string();
            // subscriptionId is not a top-level field — parse it from the resource id:
            // "/subscriptions/{sub}/resourceGroups/..."
            let id = v["id"].as_str().unwrap_or("");
            let sub = id.split('/').nth(2).unwrap_or("").to_string();
            if name.is_empty() || sub.is_empty() {
                return None;
            }
            Some(LogicAppSite {
                name,
                resource_group: rg,
                subscription: sub,
            })
        })
        .collect();
    sites.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(sites)
}

/// List all Logic Apps Standard sites in a specific Resource Group.
pub fn list_logic_app_sites_in_rg(
    subscription: &str,
    resource_group: &str,
) -> Result<Vec<LogicAppSite>, AzError> {
    let out = az_command(&[
        "resource",
        "list",
        "--subscription",
        subscription,
        "--resource-group",
        resource_group,
        "--resource-type",
        "Microsoft.Web/sites",
        "--query",
        "[?contains(kind, 'workflowapp')].{name:name,rg:resourceGroup,id:id,kind:kind}",
        "-o",
        "json",
    ])
    .output()
    .map_err(|e| AzError::Other(format!("az not found: {}", e)))?;

    let raw = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    if !out.status.success() {
        if stderr.contains("AADSTS")
            || stderr.contains("az login")
            || stderr.contains("refresh token")
        {
            return Err(AzError::NotLoggedIn);
        }
        return Err(AzError::Other(stderr.trim().to_string()));
    }

    let arr: Vec<Value> = serde_json::from_str(raw.trim()).unwrap_or_default();

    let mut sites: Vec<LogicAppSite> = arr
        .iter()
        .filter_map(|v| {
            let name = v["name"].as_str()?.to_string();
            let rg = v["rg"].as_str()?.to_string();
            let id = v["id"].as_str().unwrap_or("");
            let sub = id.split('/').nth(2).unwrap_or("").to_string();
            if name.is_empty() || sub.is_empty() {
                return None;
            }
            Some(LogicAppSite {
                name,
                resource_group: rg,
                subscription: sub,
            })
        })
        .collect();
    sites.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(sites)
}

/// List all workflows in a Logic Apps Standard site.
pub fn list_azure_workflows(
    subscription: &str,
    rg: &str,
    site: &str,
) -> Result<Vec<AzureWorkflow>, AzError> {
    let url = format!(
        "https://management.azure.com/subscriptions/{}/resourceGroups/{}/providers/Microsoft.Web/sites/{}/workflows?api-version=2022-03-01",
        subscription, rg, site
    );
    let out = az_command(&["rest", "--method", "GET", "--url", &url])
        .output()
        .map_err(|e| AzError::Other(format!("az not found: {}", e)))?;

    let raw = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    if !out.status.success() {
        if stderr.contains("AADSTS")
            || stderr.contains("az login")
            || stderr.contains("refresh token")
        {
            return Err(AzError::NotLoggedIn);
        }
        return Err(AzError::Other(stderr.trim().to_string()));
    }

    let v: Value =
        serde_json::from_str(&raw).map_err(|e| AzError::Other(format!("parse error: {}", e)))?;

    let mut result = Vec::new();
    if let Some(items) = v["value"].as_array() {
        for item in items {
            let raw_name = item["name"].as_str().unwrap_or("");
            if raw_name.is_empty() {
                continue;
            }
            // API returns "site-name/workflow-name" — strip the site prefix
            let name = raw_name
                .splitn(2, '/')
                .nth(1)
                .unwrap_or(raw_name)
                .to_string();
            let healthy = item["properties"]["health"]["state"].as_str().unwrap_or("") == "Healthy";
            result.push(AzureWorkflow { name, healthy });
        }
    }
    result.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(result)
}

/// Download a workflow definition from Azure, retrying on 429.
/// Returns pretty-printed workflow.json content.
pub fn download_workflow(
    subscription: &str,
    rg: &str,
    site: &str,
    workflow: &str,
) -> Result<String, AzError> {
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

        let raw = String::from_utf8_lossy(&out.stdout).to_string();
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
        let wf_file = &v["properties"]["files"]["workflow.json"];
        let definition = wf_file["definition"].clone();
        let kind = wf_file["kind"].as_str().unwrap_or("Stateful");

        if definition.is_null() {
            return Err(AzError::Other("No definition in Azure response".into()));
        }

        let output = serde_json::json!({ "definition": definition, "kind": kind });
        return serde_json::to_string_pretty(&output).map_err(|e| AzError::Other(e.to_string()));
    }

    Err(AzError::Other(
        "Still throttled after retries — try again in a minute".into(),
    ))
}

// ── diff helpers ──────────────────────────────────────────────────────────────

/// Compare the local `{local_dir}/logic_apps/{workflow}/workflow.json` with the live Azure version.
/// Returns `Ok(0)` when identical, `Ok(n)` with the number of changed lines when different,
/// `Err` when the local file is missing or the Azure fetch fails.
pub fn diff_workflow_vs_local(
    subscription: &str,
    rg: &str,
    site: &str,
    workflow: &str,
    local_dir: &str,
) -> Result<usize, AzError> {
    let resolved = crate::services::workflows::resolve_logic_apps_dir(local_dir);
    let local_path = resolved.join(workflow).join("workflow.json");
    let local_str = std::fs::read_to_string(&local_path)
        .map_err(|e| AzError::Other(format!("read local: {}", e)))?;

    let remote_str = download_workflow(subscription, rg, site, workflow)?;

    // Parse both so formatting differences don't count as changes
    let local_val: Value = serde_json::from_str(&local_str)
        .map_err(|e| AzError::Other(format!("parse local: {}", e)))?;
    let remote_val: Value = serde_json::from_str(&remote_str)
        .map_err(|e| AzError::Other(format!("parse remote: {}", e)))?;

    if local_val == remote_val {
        return Ok(0);
    }

    let local_pp = serde_json::to_string_pretty(&local_val).unwrap_or_default();
    let remote_pp = serde_json::to_string_pretty(&remote_val).unwrap_or_default();
    Ok(line_diff_count(&local_pp, &remote_pp).max(1))
}

/// Count lines that appear in `a` but not `b` plus lines in `b` but not `a`
/// (multiset symmetric difference — fast, good enough for JSON blobs).
fn line_diff_count(a: &str, b: &str) -> usize {
    use std::collections::HashMap;
    let mut freq: HashMap<&str, i32> = HashMap::new();
    for l in a.lines() {
        *freq.entry(l).or_default() += 1;
    }
    let mut diff = 0usize;
    for l in b.lines() {
        match freq.get_mut(l) {
            Some(n) if *n > 0 => *n -= 1,
            _ => diff += 1,
        }
    }
    diff += freq
        .values()
        .filter(|&&n| n > 0)
        .map(|&n| n as usize)
        .sum::<usize>();
    diff
}

// ── config file diff helpers ──────────────────────────────────────────────────

/// Download a file from the Logic Apps site's wwwroot via the ARM hostruntime VFS proxy.
/// Works for parameters.json and connections.json.
fn download_la_file(
    subscription: &str,
    rg: &str,
    site: &str,
    filename: &str,
) -> Result<String, AzError> {
    let url = format!(
        "https://management.azure.com/subscriptions/{}/resourceGroups/{}/providers/Microsoft.Web/sites/{}/hostruntime/admin/vfs/site/wwwroot/{}?api-version=2022-03-01",
        subscription, rg, site, filename
    );
    let out = az_command(&["rest", "--method", "GET", "--url", &url])
        .output()
        .map_err(|e| AzError::Other(format!("az not found: {}", e)))?;
    let raw = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() {
        if stderr.contains("AADSTS")
            || stderr.contains("az login")
            || stderr.contains("refresh token")
        {
            return Err(AzError::NotLoggedIn);
        }
        return Err(AzError::Other(stderr.trim().to_string()));
    }
    Ok(raw)
}

/// Public wrapper: download a config file (parameters.json or connections.json) from Azure.
pub fn download_config_file(
    subscription: &str,
    rg: &str,
    site: &str,
    filename: &str,
) -> Result<String, AzError> {
    download_la_file(subscription, rg, site, filename)
}

/// Count top-level keys present in one JSON object but not the other.
fn key_diff_count(a: &Value, b: &Value) -> usize {
    use std::collections::HashSet;
    let ak: HashSet<&str> = a
        .as_object()
        .map(|o| o.keys().map(|k| k.as_str()).collect())
        .unwrap_or_default();
    let bk: HashSet<&str> = b
        .as_object()
        .map(|o| o.keys().map(|k| k.as_str()).collect())
        .unwrap_or_default();
    ak.symmetric_difference(&bk).count()
}

/// Compare local `parameters.json` with Azure by key set.
/// Values are environment-specific (e.g. dev vs prod endpoints) so only structure is checked.
pub fn diff_parameters_vs_local(
    subscription: &str,
    rg: &str,
    site: &str,
    local_dir: &str,
) -> Result<usize, AzError> {
    let resolved = crate::services::workflows::resolve_logic_apps_dir(local_dir);
    let local_path = resolved.join("parameters.json");
    if !local_path.exists() {
        return Err(AzError::Other("No local parameters.json".into()));
    }
    let local_str = std::fs::read_to_string(&local_path)
        .map_err(|e| AzError::Other(format!("read local: {}", e)))?;
    let remote_str = download_la_file(subscription, rg, site, "parameters.json")?;

    let local_val: Value = serde_json::from_str(&local_str)
        .map_err(|e| AzError::Other(format!("parse local: {}", e)))?;
    let remote_val: Value = serde_json::from_str(&remote_str)
        .map_err(|e| AzError::Other(format!("parse remote: {}", e)))?;

    if local_val == remote_val {
        return Ok(0);
    }
    Ok(key_diff_count(&local_val, &remote_val).max(1))
}

/// Compare local `connections.json` with Azure, ignoring auth method differences.
/// MSI (cloud) vs connectionString (local) is by design — only which connections
/// exist per section is compared.
pub fn diff_connections_vs_local(
    subscription: &str,
    rg: &str,
    site: &str,
    local_dir: &str,
) -> Result<usize, AzError> {
    let resolved = crate::services::workflows::resolve_logic_apps_dir(local_dir);
    let local_path = resolved.join("connections.json");
    if !local_path.exists() {
        return Err(AzError::Other("No local connections.json".into()));
    }
    let local_str = std::fs::read_to_string(&local_path)
        .map_err(|e| AzError::Other(format!("read local: {}", e)))?;
    let remote_str = download_la_file(subscription, rg, site, "connections.json")?;

    let local_val: Value = serde_json::from_str(&local_str)
        .map_err(|e| AzError::Other(format!("parse local: {}", e)))?;
    let remote_val: Value = serde_json::from_str(&remote_str)
        .map_err(|e| AzError::Other(format!("parse remote: {}", e)))?;

    let sections = [
        "functionConnections",
        "managedApiConnections",
        "serviceProviderConnections",
    ];
    let total: usize = sections
        .iter()
        .map(|s| key_diff_count(&local_val[s], &remote_val[s]))
        .sum();
    Ok(total)
}

/// Read the subscription ID from local.settings.json (WORKFLOWS_SUBSCRIPTION_ID).
pub fn detect_subscription(logic_apps_dir: &str) -> Option<String> {
    let sub = if let Ok(text) =
        std::fs::read_to_string(std::path::Path::new(logic_apps_dir).join("local.settings.json"))
    {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            v["Values"]["WORKFLOWS_SUBSCRIPTION_ID"]
                .as_str()
                .map(|s| s.to_string())
        } else {
            None
        }
    } else {
        None
    };

    if let Some(s) = sub {
        if !s.is_empty() {
            return Some(s);
        }
    }

    // Fallback to WorkspaceLink
    crate::services::config::load()
        .get_link(logic_apps_dir)
        .map(|l| l.subscription_id.clone())
}
