use serde::{Deserialize, Serialize};

const BASE: &str = "http://localhost:7071/runtime/webhooks/workflow/api/management";

// ── Workflow list ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowItem {
    pub name: String,
    pub healthy: bool,
    pub disabled: bool,
    pub trigger_name: String, // JSON key — used in listCallbackUrl
    pub trigger_type: String, // type field — used for display icon
}

pub async fn list_workflows() -> Result<Vec<WorkflowItem>, String> {
    let url = format!("{}/workflows", BASE);
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| format!("Cannot reach func start: {}", e))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Parse error: {}", e))?;

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
            let trigger_type = triggers
                .and_then(|t| t.values().next())
                .and_then(|t| t["type"].as_str())
                .unwrap_or("Unknown")
                .to_string();
            Some(WorkflowItem { name, healthy, disabled, trigger_name, trigger_type })
        })
        .collect();

    items.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(items)
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
    let body: serde_json::Value = reqwest::Client::new()
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
    let resp = reqwest::Client::new()
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
        Err(extract_api_error(&b).unwrap_or_else(|| format!("HTTP {}", status)))
    }
}

pub async fn trigger_workflow(callback_url: &str, body: &str) -> Result<String, String> {
    let body_val: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let resp = reqwest::Client::new()
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
    parse_value_array(body)
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
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ActionError {
    pub code: Option<String>,
    pub message: Option<String>,
}

pub async fn list_actions(workflow: &str, run_id: &str) -> Result<Vec<ActionItem>, String> {
    // $expand=outputLinks makes the runtime include child actions of scopes
    let url = format!("{}/workflows/{}/runs/{}/actions?$expand=outputLinks", BASE, workflow, run_id);
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
