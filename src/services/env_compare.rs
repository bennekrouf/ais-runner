use crate::services::azure_cli::{az_command, AzError};
use indexmap::IndexMap;

pub type EnvValues = IndexMap<String, String>;

// ── Azure DevOps ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct VarGroup {
    pub name: String,
    pub variables: EnvValues,
}

/// Try to derive a DevOps project URL from the git remote URL.
/// Handles both SSH and HTTPS Azure DevOps remote formats.
pub fn detect_ado_url(project_dir: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(project_dir)
        .output()
        .ok()?;
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    parse_ado_url_from_remote(&raw)
}

fn parse_ado_url_from_remote(remote: &str) -> Option<String> {
    // SSH:   git@ssh.dev.azure.com:v3/{org}/{project}/{repo}
    if let Some(rest) = remote.strip_prefix("git@ssh.dev.azure.com:v3/") {
        let parts: Vec<&str> = rest.splitn(3, '/').collect();
        if parts.len() >= 2 {
            let org = url_decode(parts[0]);
            let proj = url_decode(parts[1]);
            return Some(format!("https://dev.azure.com/{}/{}", org, proj));
        }
    }
    // HTTPS: https://{user}@dev.azure.com/{org}/{project}/_git/{repo}
    //   or   https://dev.azure.com/{org}/{project}/_git/{repo}
    if remote.contains("dev.azure.com") {
        let stripped = remote.strip_prefix("https://").unwrap_or(remote);
        let stripped = if let Some(at) = stripped.find('@') {
            &stripped[at + 1..]
        } else {
            stripped
        };
        let rest = stripped.strip_prefix("dev.azure.com/").unwrap_or(stripped);
        let parts: Vec<&str> = rest.splitn(3, '/').collect();
        if parts.len() >= 2 {
            let org = url_decode(parts[0]);
            let proj = url_decode(parts[1].split("/_").next().unwrap_or(parts[1]));
            return Some(format!("https://dev.azure.com/{}/{}", org, proj));
        }
    }
    None
}

fn url_decode(s: &str) -> String {
    s.replace("%20", " ")
        .replace("%21", "!")
        .replace("%40", "@")
        .replace("%26", "&")
}

fn url_encode(s: &str) -> String {
    s.replace(' ', "%20")
        .replace('&', "%26")
        .replace('+', "%2B")
}

/// Parse a DevOps project URL into (org, project).
fn parse_project_url(ado_url: &str) -> Option<(String, String)> {
    let url = ado_url.trim().trim_end_matches('/');
    let rest = url.strip_prefix("https://dev.azure.com/")?;
    let mut parts = rest.splitn(2, '/');
    let org = parts.next()?.to_string();
    let proj = parts
        .next()
        .map(|p| p.split("/_").next().unwrap_or(p).to_string())
        .unwrap_or_default();
    if org.is_empty() || proj.is_empty() {
        return None;
    }
    Some((org, proj))
}

/// Fetch all variable groups (names + values) from Azure DevOps.
/// Secret variable values are returned as empty strings by the API.
pub fn list_ado_variable_groups(ado_url: &str) -> Result<Vec<VarGroup>, String> {
    let (org, project) = parse_project_url(ado_url)
        .ok_or_else(|| "Invalid URL — expected https://dev.azure.com/org/project".to_string())?;

    let api_url = format!(
        "https://dev.azure.com/{}/{}/_apis/distributedtask/variablegroups?api-version=7.1-preview.2",
        org, url_encode(&project)
    );

    let out = az_command(&[
        "rest",
        "--method",
        "GET",
        "--url",
        &api_url,
        "--resource",
        "499b84ac-1321-427f-aa17-267ca6975798",
    ])
    .output()
    .map_err(|e| format!("az not found: {}", e))?;

    let raw = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    if !out.status.success() {
        if stderr.contains("AADSTS")
            || stderr.contains("az login")
            || stderr.contains("refresh token")
        {
            return Err("Session expired — click 🔐 Re-login".to_string());
        }
        return Err(stderr.trim().to_string());
    }

    let v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("parse error: {}", e))?;

    let mut groups = Vec::new();
    if let Some(arr) = v["value"].as_array() {
        for item in arr {
            let name = item["name"].as_str().unwrap_or("").to_string();
            if name.is_empty() {
                continue;
            }
            let mut vars: EnvValues = IndexMap::new();
            if let Some(variables) = item["variables"].as_object() {
                let mut pairs: Vec<(String, String)> = variables
                    .iter()
                    .map(|(k, v)| {
                        let val = v["value"].as_str().unwrap_or("").to_string();
                        let is_secret = v["isSecret"].as_bool().unwrap_or(false);
                        (k.clone(), if is_secret { String::new() } else { val })
                    })
                    .collect();
                pairs.sort_by(|a, b| a.0.cmp(&b.0));
                vars.extend(pairs);
            }
            groups.push(VarGroup {
                name,
                variables: vars,
            });
        }
    }
    groups.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(groups)
}

// ── Azure App Settings ────────────────────────────────────────────────────────

/// Fetch Azure Logic App app settings via `az webapp config appsettings list`.
pub fn fetch_azure_app_settings(
    subscription: &str,
    rg: &str,
    site: &str,
) -> Result<EnvValues, AzError> {
    let out = az_command(&[
        "webapp",
        "config",
        "appsettings",
        "list",
        "--subscription",
        subscription,
        "--name",
        site,
        "--resource-group",
        rg,
        "--query",
        "[].{name:name,value:value}",
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

    let arr: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap_or_default();
    let mut map = IndexMap::new();
    for item in &arr {
        if let (Some(k), Some(v)) = (item["name"].as_str(), item["value"].as_str()) {
            map.insert(k.to_string(), v.to_string());
        }
    }
    Ok(map)
}
