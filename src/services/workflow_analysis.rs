use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct WorkflowAnalysis {
    pub trigger:       TriggerKind,
    pub input_queues:  Vec<String>,   // SB queues consumed (trigger or receive)
    pub output_queues: Vec<String>,   // SB queues produced (send)
    pub input_blobs:   Vec<String>,   // blob containers read
    pub output_blobs:  Vec<String>,   // blob containers written
    pub http_calls:    Vec<String>,   // outbound HTTP hosts (deduplicated)
    pub liquid_maps:   Vec<String>,   // Liquid transform map names used
    pub sql_sprocs:    Vec<SqlSproc>, // stored procedures invoked
}

#[derive(Debug, Clone, PartialEq)]
pub struct SqlSproc {
    pub name:   String,        // qualified, e.g. "dbo.MySp"
    pub params: Vec<String>,   // parameter names (without leading @), in declaration order
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum TriggerKind {
    #[default]
    Unknown,
    Http,
    ServiceBus { queue: String },
    Blob       { container: String },
    Timer { schedule: String },  // e.g. "every 5 Minute"
}

impl WorkflowAnalysis {
    pub fn is_empty(&self) -> bool {
        matches!(self.trigger, TriggerKind::Unknown | TriggerKind::Http | TriggerKind::Timer { .. })
            && self.input_queues.is_empty()
            && self.output_queues.is_empty()
            && self.input_blobs.is_empty()
            && self.output_blobs.is_empty()
            && self.http_calls.is_empty()
            && self.liquid_maps.is_empty()
            && self.sql_sprocs.is_empty()
    }

    /// All SB queues referenced (input + output, deduplicated).
    #[allow(dead_code)]
    pub fn all_queues(&self) -> Vec<String> {
        let mut q = self.input_queues.clone();
        for v in &self.output_queues {
            if !q.contains(v) { q.push(v.clone()); }
        }
        q
    }
}

pub fn analyse(workflow_json: &str) -> WorkflowAnalysis {
    let v: Value = match serde_json::from_str(workflow_json) {
        Ok(v) => v,
        Err(_) => return WorkflowAnalysis::default(),
    };

    let def = &v["definition"];
    let mut analysis = WorkflowAnalysis::default();

    // ── Trigger ───────────────────────────────────────────────────────────
    if let Some(triggers) = def["triggers"].as_object() {
        for trigger in triggers.values() {
            let kind = trigger["type"].as_str().unwrap_or("").to_lowercase();
            let provider = trigger["inputs"]["serviceProviderConfiguration"]["serviceProviderId"]
                .as_str().unwrap_or("").to_lowercase();
            let op = trigger["inputs"]["serviceProviderConfiguration"]["operationId"]
                .as_str().unwrap_or("").to_lowercase();

            analysis.trigger = if kind == "request" || kind == "http" {
                TriggerKind::Http
            } else if kind == "recurrence" {
                let interval  = trigger["recurrence"]["interval"].as_u64().unwrap_or(1);
                let frequency = trigger["recurrence"]["frequency"].as_str().unwrap_or("?");
                let schedule  = format!("every {} {}", interval, frequency);
                TriggerKind::Timer { schedule }
            } else if provider.contains("servicebus") || kind == "servicebustrigger" {
                let queue = literal_str(&trigger["inputs"]["parameters"]["entityName"])
                    .or_else(|| literal_str(&trigger["inputs"]["parameters"]["queueName"]))
                    .unwrap_or_default();
                if !queue.is_empty() && !analysis.input_queues.contains(&queue) {
                    analysis.input_queues.push(queue.clone());
                }
                TriggerKind::ServiceBus { queue }
            } else if provider.contains("azureblob") || provider.contains("blob") {
                let container = literal_str(&trigger["inputs"]["parameters"]["path"])
                    .or_else(|| literal_str(&trigger["inputs"]["parameters"]["containerName"]))
                    .unwrap_or_default();
                if !container.is_empty() && !analysis.input_blobs.contains(&container) {
                    analysis.input_blobs.push(container.clone());
                }
                TriggerKind::Blob { container }
            } else {
                TriggerKind::Unknown
            };

            // also capture recurrence-on-SB patterns
            let _ = (kind, op);
        }
    }

    // ── Actions ───────────────────────────────────────────────────────────
    scan_actions(&def["actions"], &mut analysis);

    analysis
}

fn scan_actions(node: &Value, out: &mut WorkflowAnalysis) {
    match node {
        Value::Object(map) => {
            for action in map.values() {
                process_action(action, out);
                // recurse into branches (If/Switch/Foreach/Until)
                for branch_key in &["actions", "else", "cases", "default"] {
                    scan_actions(&action[branch_key], out);
                }
                // Switch cases
                if let Some(cases) = action["cases"].as_object() {
                    for case in cases.values() {
                        scan_actions(&case["actions"], out);
                    }
                }
            }
        }
        Value::Array(arr) => {
            for v in arr { scan_actions(v, out); }
        }
        _ => {}
    }
}

fn process_action(action: &Value, out: &mut WorkflowAnalysis) {
    let provider = action["inputs"]["serviceProviderConfiguration"]["serviceProviderId"]
        .as_str().unwrap_or("").to_lowercase();
    let op = action["inputs"]["serviceProviderConfiguration"]["operationId"]
        .as_str().unwrap_or("").to_lowercase();
    let kind = action["type"].as_str().unwrap_or("").to_lowercase();

    // ── Service Bus — ServiceProvider style ───────────────────────────────
    if provider.contains("servicebus") {
        let entity = &action["inputs"]["parameters"]["entityName"];
        let queue_f = &action["inputs"]["parameters"]["queueName"];

        // only proceed if either field is present (not null/missing)
        if entity.is_null() && queue_f.is_null() { return; }

        // literal value, or "(dynamic)" if field is an expression
        let queue = literal_str(entity)
            .or_else(|| literal_str(queue_f))
            .unwrap_or_else(|| "(dynamic)".to_string());

        let is_send = op.contains("send") || op.contains("publish");
        if is_send {
            if !out.output_queues.contains(&queue) { out.output_queues.push(queue); }
        } else {
            if !out.input_queues.contains(&queue) { out.input_queues.push(queue); }
        }
        return;
    }

    // ── Service Bus — ApiConnection / path-based style ────────────────────
    // path looks like: "/queues/@{encodeURIComponent('my-queue')}/messages"
    if kind == "apiconnection" || kind == "servicebus" {
        let path = action["inputs"]["path"].as_str().unwrap_or("");
        if path.contains("/queues/") || path.contains("/topics/") {
            if let Some(queue) = extract_sb_queue_from_path(path) {
                let method = action["inputs"]["method"].as_str().unwrap_or("post").to_lowercase();
                let is_send = method == "post" || path.ends_with("/messages");
                if is_send {
                    if !out.output_queues.contains(&queue) { out.output_queues.push(queue); }
                } else {
                    if !out.input_queues.contains(&queue) { out.input_queues.push(queue); }
                }
            }
        }
        return;
    }

    // ── Blob storage — ServiceProvider style ──────────────────────────────
    if provider.contains("azureblob") || provider.contains("blob") {
        let container = literal_str(&action["inputs"]["parameters"]["containerName"])
            .or_else(|| literal_str(&action["inputs"]["parameters"]["path"]))
            .unwrap_or_default();
        if !container.is_empty() {
            let is_write = op.contains("create") || op.contains("upload")
                || op.contains("write") || op.contains("put") || op.contains("copy");
            if is_write {
                if !out.output_blobs.contains(&container) { out.output_blobs.push(container); }
            } else {
                if !out.input_blobs.contains(&container) { out.input_blobs.push(container); }
            }
        }
        return;
    }

    // ── SQL stored procedure ──────────────────────────────────────────────
    if provider.contains("/serviceproviders/sql") || provider == "sql" {
        if op.contains("storedprocedure") || op.contains("executequery") {
            let name = literal_str(&action["inputs"]["parameters"]["storedProcedureName"])
                .or_else(|| literal_str(&action["inputs"]["parameters"]["storedProcedureFullName"]))
                .or_else(|| literal_str(&action["inputs"]["parameters"]["procedure"]))
                .map(|s| normalize_sproc_name(&s));
            if let Some(n) = name {
                if !n.is_empty() {
                    let params: Vec<String> = action["inputs"]["parameters"]["storedProcedureParameters"]
                        .as_object()
                        .map(|m| m.keys()
                            .map(|k| k.trim_start_matches('@').to_string())
                            .collect())
                        .unwrap_or_default();
                    if !out.sql_sprocs.iter().any(|sp| sp.name == n) {
                        out.sql_sprocs.push(SqlSproc { name: n, params });
                    }
                }
            }
        }
        return;
    }

    // ── Liquid transform ──────────────────────────────────────────────────
    if kind == "liquid" {
        // map name lives at inputs.map.name (newer) or inputs.integrationAccount.map.name (older)
        let map_name = literal_str(&action["inputs"]["map"]["name"])
            .or_else(|| literal_str(&action["inputs"]["integrationAccount"]["map"]["name"]));
        if let Some(name) = map_name {
            if !out.liquid_maps.contains(&name) { out.liquid_maps.push(name); }
        }
        return;
    }

    // ── HTTP calls ────────────────────────────────────────────────────────
    if kind == "http" {
        if let Some(uri) = literal_str(&action["inputs"]["uri"])
            .or_else(|| literal_str(&action["inputs"]["url"]))
        {
            if let Some(host) = extract_host(&uri) {
                if !out.http_calls.contains(&host) { out.http_calls.push(host); }
            }
        }
    }
}

/// Extract a queue/topic name from a Logic Apps API connection path like:
/// `/queues/@{encodeURIComponent('my-queue')}/messages`
/// `/queues/my-queue/messages`
fn extract_sb_queue_from_path(path: &str) -> Option<String> {
    // find segment after /queues/ or /topics/
    let after = path.split("/queues/").nth(1)
        .or_else(|| path.split("/topics/").nth(1))?;
    let segment = after.split('/').next()?;

    // strip encodeURIComponent wrapper if present
    let name = if segment.contains("encodeURIComponent") {
        segment
            .split('\'').nth(1)
            .or_else(|| segment.split('"').nth(1))?
            .to_string()
    } else if segment.starts_with('@') {
        return None; // pure expression, can't resolve statically
    } else {
        segment.to_string()
    };

    if name.is_empty() { None } else { Some(name) }
}

/// Normalize a stored-procedure name by stripping brackets:
/// "[dbo].[WfFanInOut_Begin_sp]" → "dbo.WfFanInOut_Begin_sp"
fn normalize_sproc_name(raw: &str) -> String {
    raw.replace(['[', ']'], "").trim().to_string()
}

/// Extract a literal (non-expression) string from a JSON value.
fn literal_str(v: &Value) -> Option<String> {
    let s = v.as_str()?;
    if s.starts_with('@') || s.is_empty() { None } else { Some(s.to_string()) }
}

fn extract_host(uri: &str) -> Option<String> {
    let without_scheme = uri.split("://").nth(1).unwrap_or(uri);
    let host = without_scheme.split('/').next()?;
    let host = host.split('?').next()?;
    if host.is_empty() || host.starts_with('@') { return None; }
    Some(host.to_string())
}
