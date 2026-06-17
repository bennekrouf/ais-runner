use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

const BASE: &str = "http://localhost:7071/runtime/webhooks/workflow/api/management";

/// Shared async HTTP client — constructed once, reused for every request.
///
/// `reqwest::Client` is `Clone`-cheap (Arc under the hood) and connection-pools
/// automatically.  Building a new one per call was the root cause of the
/// EAGAIN / thread-exhaustion crash when "copy callback URL" fired alongside
/// the blob-watcher loop.
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

// ── Workflow list ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowItem {
    pub name: String,
    pub healthy: bool,
    pub disabled: bool,
    pub trigger_name:     String,       // JSON key — used in listCallbackUrl
    pub trigger_type:     String,       // type field — used for display icon
    pub trigger_provider: Option<String>, // serviceProviderId for ServiceProvider triggers
    pub health_error: Option<String>,
}

// ── Connector detection ────────────────────────────────────────────────────

/// A service a workflow depends on — shown as small icons in the workflow list.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ConnectorKind {
    ServiceBus,
    Blob,
    CosmosDb,
    Sql,
    Sftp,
    EventGrid,
    Http,
}

impl ConnectorKind {
    pub fn icon(&self) -> &'static str {
        match self {
            Self::ServiceBus => "📨",
            Self::Blob       => "📦",
            Self::CosmosDb   => "🌌",
            Self::Sql        => "🗄",
            Self::Sftp       => "📂",
            Self::EventGrid  => "⚡",
            Self::Http       => "🔗",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::ServiceBus => "Service Bus",
            Self::Blob       => "Blob Storage",
            Self::CosmosDb   => "Cosmos DB",
            Self::Sql        => "SQL",
            Self::Sftp       => "SFTP",
            Self::EventGrid  => "Event Grid",
            Self::Http       => "HTTP (outbound)",
        }
    }

    fn from_provider_id(id: &str) -> Option<Self> {
        let id = id.to_lowercase();
        if id.contains("servicebus")            { return Some(Self::ServiceBus); }
        if id.contains("azureblob") || id.contains("/blob") { return Some(Self::Blob); }
        if id.contains("documentdb") || id.contains("cosmos") { return Some(Self::CosmosDb); }
        if id.contains("/sql")                  { return Some(Self::Sql); }
        if id.contains("sftp")                  { return Some(Self::Sftp); }
        if id.contains("eventgrid")             { return Some(Self::EventGrid); }
        None
    }
}

/// Walk the entire workflow JSON tree and collect every connector the workflow uses.
fn collect_connectors(v: &serde_json::Value, out: &mut std::collections::HashSet<ConnectorKind>) {
    match v {
        serde_json::Value::Object(map) => {
            // serviceProviderConfiguration.serviceProviderId  (built-in connectors)
            if let Some(cfg) = map.get("serviceProviderConfiguration") {
                if let Some(id) = cfg["serviceProviderId"].as_str() {
                    if let Some(k) = ConnectorKind::from_provider_id(id) {
                        out.insert(k);
                    }
                }
            }
            // Outbound HTTP actions
            if map.get("type").and_then(|t| t.as_str())
                .map(|t| t.eq_ignore_ascii_case("http"))
                .unwrap_or(false)
            {
                out.insert(ConnectorKind::Http);
            }
            for val in map.values() { collect_connectors(val, out); }
        }
        serde_json::Value::Array(arr) => {
            for item in arr { collect_connectors(item, out); }
        }
        _ => {}
    }
}

/// Scans every workflow.json and returns the connector set for each workflow name.
pub fn scan_all_connectors(logic_apps_dir: &str) -> std::collections::HashMap<String, Vec<ConnectorKind>> {
    let dir = resolve_logic_apps_dir(logic_apps_dir);
    let mut result = std::collections::HashMap::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return result,
    };
    for entry in entries.flatten() {
        let wf_path = entry.path().join("workflow.json");
        if !wf_path.exists() { continue; }
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Ok(text) = std::fs::read_to_string(&wf_path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                let mut kinds = std::collections::HashSet::new();
                collect_connectors(&v, &mut kinds);
                let mut sorted: Vec<ConnectorKind> = kinds.into_iter().collect();
                sorted.sort();
                result.insert(name, sorted);
            }
        }
    }
    result
}

pub async fn list_workflows() -> Result<Vec<WorkflowItem>, String> {
    let url = format!("{}/workflows", BASE);
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| format!("Cannot reach func start: {}", e))?;

    let raw = resp
        .text()
        .await
        .map_err(|e| format!("Cannot read response: {}", e))?;

    // Known transient startup phrase — not a parse error, just not ready yet
    if raw.contains("Function host is not running") {
        return Err("host-not-ready".to_string());
    }

    // Strip ASCII control characters that the func host sometimes embeds in error payloads
    let sanitized: String = raw.chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\r' || *c == '\t').collect();

    let body: serde_json::Value = serde_json::from_str(&sanitized)
        .map_err(|e| {
            let snippet = sanitized.chars().take(400).collect::<String>();
            format!("Parse error: {} — host returned: {}", e, snippet)
        })?;

    // local func runtime returns a bare array; Azure mgmt API returns {"value":[...]}
    let arr = body
        .as_array()
        .cloned()
        .or_else(|| body["value"].as_array().cloned())
        .ok_or_else(|| { let s = body.to_string(); format!("Unexpected response shape: {}", &s[..s.len().min(200)]) })?;

    let mut items: Vec<WorkflowItem> = arr
        .iter()
        .filter_map(|v| {
            let name = v["name"].as_str()?.to_string();
            let healthy = v["health"]["state"].as_str().unwrap_or("Unhealthy") == "Healthy";
            let disabled = v["isDisabled"].as_bool().unwrap_or(false);
            // store the trigger *name* (JSON key) — used in listCallbackUrl API call
            // also store the trigger *type* for display icon
            let triggers = v["triggers"].as_object();
            let trigger_name = triggers
                .and_then(|t| t.keys().next().map(|s| s.as_str()))
                .unwrap_or("manual")
                .to_string();
            let trigger_entry = triggers.and_then(|t| t.values().next());
            let trigger_type = trigger_entry
                .and_then(|t| t["type"].as_str())
                .unwrap_or("Unknown")
                .to_string();
            let trigger_provider = trigger_entry
                .and_then(|t| t["inputs"]["serviceProviderConfiguration"]["serviceProviderId"].as_str())
                .map(|s| s.to_string());
            let health_error = v["health"]["error"]["message"].as_str()
                .or_else(|| v["health"]["error"].as_str())
                .map(|s| s.to_string());

            Some(WorkflowItem { name, healthy, disabled, trigger_name, trigger_type, trigger_provider, health_error })
        })
        .collect();

    items.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(items)
}

/// Polls `list_workflows` every 5 s until the host responds or `timeout_secs` expires.
/// Returns Ok on first success, Err with the last error message if the host never responds.
pub async fn wait_for_workflows(timeout_secs: u64) -> Result<Vec<WorkflowItem>, String> {
    let attempts = (timeout_secs / 5).max(1);
    let mut last_err = String::from("timeout");
    for _ in 0..attempts {
        match list_workflows().await {
            Ok(list) => return Ok(list),
            Err(e)   => { last_err = e; }
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
    Err(last_err)
}

/// Reads every workflow.json and returns each workflow's trigger serviceProviderId.
/// Used to enrich items returned by list_workflows(), which only gets type/kind from the API.
pub fn scan_trigger_providers(logic_apps_dir: &str) -> std::collections::HashMap<String, String> {
    let dir = resolve_logic_apps_dir(logic_apps_dir);
    let mut map = std::collections::HashMap::new();
    let Ok(entries) = std::fs::read_dir(&dir) else { return map };
    for entry in entries.flatten() {
        let wf_path = entry.path().join("workflow.json");
        let Ok(text) = std::fs::read_to_string(&wf_path) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(provider) = v["definition"]["triggers"]
            .as_object()
            .and_then(|t| t.values().next())
            .and_then(|t| t["inputs"]["serviceProviderConfiguration"]["serviceProviderId"].as_str())
        {
            map.insert(name, provider.to_string());
        }
    }
    map
}

/// Returns the actual directory containing the workflow folders.
/// It checks if the provided `base_dir` contains `logic_apps` or `logic-apps` subfolders.
pub fn resolve_logic_apps_dir(base_dir: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(base_dir);
    if p.join("logic_apps").exists() {
        p.join("logic_apps")
    } else if p.join("logic-apps").exists() {
        p.join("logic-apps")
    } else {
        p.to_path_buf()
    }
}

/// Scans every workflow.json under `logic_apps_dir` and builds WorkflowItems from disk.
/// Used to refresh the left-panel list after a pull when func is not running.
pub fn scan_local_workflows(logic_apps_dir: &str) -> Vec<WorkflowItem> {
    let dir = resolve_logic_apps_dir(logic_apps_dir);
    let mut items = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return items,
    };
    for entry in entries.flatten() {
        let wf_path = entry.path().join("workflow.json");
        if !wf_path.exists() { continue; }
        let name = entry.file_name().to_string_lossy().into_owned();
        let text = match std::fs::read_to_string(&wf_path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let triggers = v["definition"]["triggers"].as_object();
        let trigger_name = triggers
            .and_then(|t| t.keys().next().map(|s| s.to_string()))
            .unwrap_or_else(|| "manual".to_string());
        let trigger_entry = triggers.and_then(|t| t.values().next());
        let trigger_type = trigger_entry
            .and_then(|t| t["type"].as_str())
            .unwrap_or("Unknown")
            .to_string();
        let trigger_provider = trigger_entry
            .and_then(|t| t["inputs"]["serviceProviderConfiguration"]["serviceProviderId"].as_str())
            .map(|s| s.to_string());
        items.push(WorkflowItem { name, healthy: true, disabled: false, trigger_name, trigger_type, trigger_provider, health_error: None });
    }
    items.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    items
}

/// Scans every workflow.json under `logic_apps_dir` and returns names of files with JSON errors.
pub fn scan_broken_workflows(logic_apps_dir: &str) -> Vec<(String, String)> {
    let dir = resolve_logic_apps_dir(logic_apps_dir);
    let mut broken = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return broken,
    };
    for entry in entries.flatten() {
        let wf_path = entry.path().join("workflow.json");
        if !wf_path.exists() { continue; }
        match std::fs::read_to_string(&wf_path) {
            Err(e) => broken.push((entry.file_name().to_string_lossy().into_owned(), format!("cannot read: {}", e))),
            Ok(text) => {
                if let Err(e) = serde_json::from_str::<serde_json::Value>(&text) {
                    broken.push((entry.file_name().to_string_lossy().into_owned(), format!("JSON error at line {}: {}", e.line(), e)));
                }
            }
        }
    }
    broken.sort_by(|a, b| a.0.cmp(&b.0));
    broken
}

// ── Trigger ────────────────────────────────────────────────────────────────

fn extract_api_error(body: &serde_json::Value) -> Option<String> {
    body["error"]["message"].as_str().map(|s| s.to_string())
}

pub async fn get_callback_url(workflow: &str, trigger: &str) -> Result<String, String> {
    let url = format!(
        "{}/workflows/{}/triggers/{}/listCallbackUrl",
        BASE, workflow, trigger
    );
    let body: serde_json::Value = http_client()
        .post(&url)
        .header("Content-Type", "application/json")
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;

    if let Some(err) = extract_api_error(&body) {
        return Err(err);
    }
    body["value"]
        .as_str()
        .or_else(|| body.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| { let s = body.to_string(); format!("Unexpected callbackUrl shape: {}", &s[..s.len().min(300)]) })
}

/// For Recurrence / push triggers that have no callback URL — call /run directly.
pub async fn run_trigger_direct(workflow: &str, trigger: &str, body: &str) -> Result<(), String> {
    let url = format!("{}/workflows/{}/triggers/{}/run", BASE, workflow, trigger);
    let body_val: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let resp = http_client()
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&body_val)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    if status.is_success() || status.as_u16() == 202 {
        Ok(())
    } else {
        let b: serde_json::Value = resp.json().await.unwrap_or_default();
        let code = b["error"]["code"].as_str().unwrap_or("");
        let hint = if code == "WorkflowNotFound" {
            " — if all workflows fail, check AzureWebJobsStorage in Connections → Blob (set to 'UseDevelopmentStorage=true' for Azurite) and restart func"
        } else { "" };
        Err(format!("{}{}", extract_api_error(&b).unwrap_or_else(|| format!("HTTP {}", status)), hint))
    }
}

pub async fn trigger_workflow(callback_url: &str, body: &str) -> Result<String, String> {
    let body_val: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let resp = http_client()
        .post(callback_url)
        .header("Content-Type", "application/json")
        .json(&body_val)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let run_id = resp
        .headers()
        .get("x-ms-workflow-run-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();

    Ok(run_id)
}

// ── Run history ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RunItem {
    pub name: String,
    pub properties: RunProperties,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunProperties {
    pub status: String,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
}

fn parse_value_array<T: for<'de> Deserialize<'de>>(body: serde_json::Value) -> Result<Vec<T>, String> {
    if let Some(code) = body["error"]["code"].as_str() {
        let msg = body["error"]["message"].as_str().unwrap_or(code);
        if code == "WorkflowNotFound" {
            return Err(format!(
                "Workflow not found — if this happens for every workflow, \
                 AzureWebJobsStorage is likely pointing to Azure instead of Azurite. \
                 Open Connections → Blob, check AzureWebJobsStorage, set it to \
                 'UseDevelopmentStorage=true', save, and restart func. ({})", msg
            ));
        }
        return Err(format!("{}: {}", code, msg));
    }
    let arr = body.as_array().cloned()
        .or_else(|| body["value"].as_array().cloned())
        .ok_or_else(|| {
            let s = body.to_string();
            format!("Unexpected response shape: {}", &s[..s.len().min(300)])
        })?;
    Ok(arr.into_iter().filter_map(|v| serde_json::from_value(v).ok()).collect())
}

pub async fn list_runs(workflow: &str) -> Result<Vec<RunItem>, String> {
    let url = format!("{}/workflows/{}/runs", BASE, workflow);
    let body: serde_json::Value = reqwest::get(&url)
        .await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;

    if body["error"]["code"].as_str() == Some("WorkflowNotFound") {
        // Check whether the workflow itself is accessible (design-time vs runtime split).
        let wf_url = format!("{}/workflows/{}", BASE, workflow);
        let wf_body: serde_json::Value = if let Ok(resp) = reqwest::get(&wf_url).await {
            resp.json().await.unwrap_or_default()
        } else {
            serde_json::Value::Null
        };
        let in_runtime = wf_body["name"].as_str().is_some()
            && wf_body["error"].is_null();
        if in_runtime {
            return Err(format!(
                "Workflow is defined but its runtime state is missing — \
                 the Logic Apps runtime could not initialise its storage tables in Azurite. \
                 Stop func, stop Azurite, clear Azurite data (Connections → Blob → Reset Azurite Data), \
                 then restart both."
            ));
        } else {
            return Err(format!(
                "Workflow not in runtime registry — \
                 the Logic Apps runtime may not have finished starting, or a connection \
                 prevented it from registering this workflow. \
                 Check func start output for errors, or try restarting func."
            ));
        }
    }

    parse_value_array(body)
}

/// Probes the runtime to distinguish "workflow not found" causes and returns hint lines.
///
/// Three outcomes:
/// - Some(true)  → registered in runtime but something else is broken (e.g. run-table inaccessible)
/// - Some(false) → not registered in runtime at all (connection error blocked startup registration)
/// - None        → management API unreachable (func not running or still starting)
pub async fn not_found_hints(workflow: &str) -> Vec<String> {
    let url = format!("{}/workflows/{}", BASE, workflow);
    let in_runtime: Option<bool> = async {
        let body: serde_json::Value = reqwest::get(&url).await.ok()?.json().await.ok()?;
        if body["error"]["code"].as_str() == Some("WorkflowNotFound") {
            Some(false)
        } else if body["name"].as_str().is_some() {
            Some(true)
        } else {
            None
        }
    }.await;

    match in_runtime {
        Some(true) => vec![
            "  hint: workflow IS registered in the runtime but run history is inaccessible — \
             Azurite table storage may be corrupted. \
             Stop func, stop Azurite, clear Azurite data (Connections → Blob → Reset Azurite Data), then restart both.".to_string(),
        ],
        Some(false) => vec![
            "  hint: workflow is NOT in the runtime registry — a failing connection likely \
             blocked registration at startup. Check func start output for errors, \
             fix the connection, and restart func.".to_string(),
        ],
        None => vec![
            "  hint: func management API did not respond — is func still running?".to_string(),
        ],
    }
}

/// Lightweight existence check — fetches at most 1 run to minimise data transfer.
pub async fn check_has_runs(workflow: &str) -> bool {
    let url = format!("{}/workflows/{}/runs?$top=1", BASE, workflow);
    let Ok(resp) = reqwest::get(&url).await else { return false };
    let Ok(body) = resp.json::<serde_json::Value>().await else { return false };
    body.as_array()
        .or_else(|| body["value"].as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false)
}

// ── Action details ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ActionItem {
    pub name: String,
    pub properties: ActionProperties,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ActionProperties {
    pub status: String,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub error: Option<ActionError>,
    pub code: Option<String>,
    #[serde(rename = "type")]
    pub action_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ActionError {
    pub code: Option<String>,
    pub message: Option<String>,
}

/// Fetch the raw detail JSON for a single action, and inline the inputs/outputs
/// blobs referenced by `inputsLink.uri` / `outputsLink.uri`. The actual error
/// message for a failed connector call (e.g. SQL exception text) lives in the
/// outputs blob, not in the action record itself.
pub async fn get_action_detail(workflow: &str, run_id: &str, action: &str) -> Result<serde_json::Value, String> {
    let url = format!("{}/workflows/{}/runs/{}/actions/{}", BASE, workflow, run_id, action);
    let mut detail: serde_json::Value = reqwest::get(&url)
        .await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;

    async fn fetch_blob(uri: &str) -> serde_json::Value {
        match reqwest::get(uri).await {
            Ok(resp) => match resp.text().await {
                Ok(text) => serde_json::from_str(&text)
                    .unwrap_or_else(|_| serde_json::Value::String(text)),
                Err(e) => serde_json::Value::String(format!("<fetch error: {}>", e)),
            },
            Err(e) => serde_json::Value::String(format!("<fetch error: {}>", e)),
        }
    }

    let inputs_uri  = detail.pointer("/properties/inputsLink/uri").and_then(|v| v.as_str()).map(str::to_string);
    let outputs_uri = detail.pointer("/properties/outputsLink/uri").and_then(|v| v.as_str()).map(str::to_string);

    if let Some(uri) = inputs_uri {
        let body = fetch_blob(&uri).await;
        if let Some(props) = detail.get_mut("properties").and_then(|p| p.as_object_mut()) {
            props.insert("inputs".to_string(), body);
        }
    }
    if let Some(uri) = outputs_uri {
        let body = fetch_blob(&uri).await;
        if let Some(props) = detail.get_mut("properties").and_then(|p| p.as_object_mut()) {
            props.insert("outputs".to_string(), body);
        }
    }

    Ok(detail)
}

pub async fn list_actions(workflow: &str, run_id: &str) -> Result<Vec<ActionItem>, String> {
    // $expand=outputLinks makes the runtime include child actions of scopes
    let url = format!("{}/workflows/{}/runs/{}/actions?$expand=outputLinks", BASE, workflow, run_id);
    let body: serde_json::Value = reqwest::get(&url)
        .await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    parse_value_array(body)
}

// ── ForEach repetitions ────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RepetitionItem {
    pub name: String,
    pub properties: RepetitionProperties,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RepetitionProperties {
    pub status: String,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub error: Option<ActionError>,
}

/// Lists the iterations of a ForEach action.
pub async fn list_repetitions(workflow: &str, run_id: &str, action: &str) -> Result<Vec<RepetitionItem>, String> {
    let url = format!("{}/workflows/{}/runs/{}/actions/{}/repetitions", BASE, workflow, run_id, action);
    let body: serde_json::Value = reqwest::get(&url)
        .await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    parse_value_array(body)
}

/// Lists the actions executed within a single ForEach iteration.
pub async fn list_repetition_actions(workflow: &str, run_id: &str, action: &str, rep: &str) -> Result<Vec<ActionItem>, String> {
    let url = format!("{}/workflows/{}/runs/{}/actions/{}/repetitions/{}/actions", BASE, workflow, run_id, action, rep);
    let body: serde_json::Value = reqwest::get(&url)
        .await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    parse_value_array(body)
}

/// Lists the child actions of a Scope action.
pub async fn list_scoped_repetitions(workflow: &str, run_id: &str, action: &str) -> Result<Vec<ActionItem>, String> {
    let url = format!("{}/workflows/{}/runs/{}/actions/{}/scopedRepetitions", BASE, workflow, run_id, action);
    let body: serde_json::Value = reqwest::get(&url)
        .await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    parse_value_array(body)
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// Duration in ms between two ISO8601 timestamps.
pub fn duration_ms(start: &Option<String>, end: &Option<String>) -> Option<i64> {
    let s = start.as_deref()?;
    let e = end.as_deref()?;
    let start_dt = chrono::DateTime::parse_from_rfc3339(s).ok()?;
    let end_dt = chrono::DateTime::parse_from_rfc3339(e).ok()?;
    Some((end_dt - start_dt).num_milliseconds())
}

// ── Blob trigger support ───────────────────────────────────────────────────────

/// Returns all (container_name, workflow_name) pairs for blob-triggered workflows
/// found under logic_apps_dir. Used by the auto-trigger watcher.
pub fn scan_all_blob_triggers(logic_apps_dir: &str) -> Vec<(String, String)> {
    let dir = resolve_logic_apps_dir(logic_apps_dir);
    let mut result = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else { return result };
    for entry in entries.flatten() {
        let wf_path = entry.path().join("workflow.json");
        if !wf_path.exists() { continue; }
        let Ok(json) = std::fs::read_to_string(&wf_path) else { continue };
        if let Some((container, _)) = read_blob_trigger_info(&json) {
            result.push((container, entry.file_name().to_string_lossy().into_owned()));
        }
    }
    result.sort_by(|a, b| a.1.cmp(&b.1));
    result
}

/// Returns every hardcoded blob container name referenced in the workflow JSON —
/// trigger path, action containerName values, and parameter defaultValues that
/// look like container names (lowercase + hyphens).
/// Used to pre-flight check container existence before starting a test run.
pub fn extract_all_blob_containers(workflow_json: &str) -> Vec<String> {
    let v: serde_json::Value = match serde_json::from_str(workflow_json) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let mut out = std::collections::HashSet::new();

    // Trigger path
    if let Some(triggers) = v["definition"]["triggers"].as_object() {
        for t in triggers.values() {
            if let Some(p) = t["inputs"]["parameters"]["path"].as_str() {
                if !p.is_empty() && !p.starts_with('@') { out.insert(p.to_string()); }
            }
        }
    }

    // Walk all action inputs for hardcoded containerName values
    fn collect(val: &serde_json::Value, out: &mut std::collections::HashSet<String>) {
        match val {
            serde_json::Value::Object(map) => {
                if let Some(name) = map.get("containerName").and_then(|v| v.as_str()) {
                    if !name.is_empty() && !name.starts_with('@') { out.insert(name.to_string()); }
                }
                for v in map.values() { collect(v, out); }
            }
            serde_json::Value::Array(arr) => { for v in arr { collect(v, out); } }
            _ => {}
        }
    }
    collect(&v["definition"]["actions"], &mut out);

    // Parameter defaultValues that look like container names
    if let Some(params) = v["definition"]["parameters"].as_object() {
        for param in params.values() {
            if let Some(dv) = param["defaultValue"].as_str() {
                if !dv.is_empty() && !dv.starts_with('@')
                    && dv.len() >= 3 && dv.len() <= 63
                    && dv.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                {
                    out.insert(dv.to_string());
                }
            }
        }
    }

    out.into_iter().collect()
}

/// Reads blob trigger configuration from workflow.json.
/// Returns (container_name, connection_name) for ServiceProvider blob triggers.
pub fn read_blob_trigger_info(workflow_json: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(workflow_json).ok()?;
    let trigger = v["definition"]["triggers"].as_object()?.values().next()?;

    // Container lives at inputs.parameters.path for whenABlobIsAddedOrModified triggers
    let container = trigger["inputs"]["parameters"]["path"]
        .as_str()
        .or_else(|| trigger["inputs"]["serviceProviderConfiguration"]["containerName"].as_str())?
        .to_string();
    let conn = trigger["inputs"]["serviceProviderConfiguration"]["connectionName"]
        .as_str()
        .unwrap_or("AzureBlob")
        .to_string();
    Some((container, conn))
}


#[cfg(test)]
mod tests {
    include!("workflows/tests.rs");
}
