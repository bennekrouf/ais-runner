use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Link {
    pub kind: String,   // "queue", "blob", "http", "recurrence", "eventgrid", "invoke", "function"
    pub target: String, // queue name, blob path, workflow id, etc.
}

#[derive(Debug, Clone)]
pub struct Workflow {
    pub name: String,
    pub triggers: Vec<Link>,
    pub sends: Vec<Link>,
    pub calls: Vec<Link>,
}

pub fn parse_all(dir: &Path) -> Vec<Workflow> {
    let mut workflows = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return workflows,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let wf_path = path.join("workflow.json");
        if !wf_path.exists() {
            continue;
        }

        let name = path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        match parse_workflow(&wf_path) {
            Some(mut wf) => {
                wf.name = name;
                workflows.push(wf);
            }
            None => {
                eprintln!("  ⚠ Could not parse {}", wf_path.display());
            }
        }
    }

    workflows
}

/// Parse a workflow from a JSON value (workflow definition).
/// The value can be either the full workflow JSON (with a "definition" key)
/// or the definition directly.
pub fn parse_workflow_json(name: &str, root: &Value) -> Option<Workflow> {
    let definition = root.get("definition")
        .or_else(|| root.get("properties").and_then(|p| p.get("definition")))
        .unwrap_or(root);
    let mut wf = parse_definition(definition)?;
    wf.name = name.to_string();
    Some(wf)
}

fn parse_workflow(path: &Path) -> Option<Workflow> {
    let content = std::fs::read_to_string(path).ok()?;
    let root: Value = serde_json::from_str(&content).ok()?;
    let definition = root.get("definition").unwrap_or(&root);
    parse_definition(definition)
}

fn parse_definition(definition: &Value) -> Option<Workflow> {
    let mut triggers = Vec::new();
    let mut sends = Vec::new();
    let mut calls = Vec::new();

    if let Some(trigs) = definition.get("triggers").and_then(|t| t.as_object()) {
        for (_name, trig) in trigs {
            extract_trigger(trig, &mut triggers);
        }
    }

    if let Some(actions) = definition.get("actions").and_then(|a| a.as_object()) {
        walk_actions(actions, &mut sends, &mut calls);
    }

    Some(Workflow {
        name: String::new(),
        triggers,
        sends,
        calls,
    })
}

fn extract_trigger(trig: &Value, triggers: &mut Vec<Link>) {
    let typ = trig.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match typ {
        "ServiceProvider" => {
            let spc = &trig["inputs"]["serviceProviderConfiguration"];
            let op = spc.get("operationId").and_then(|o| o.as_str()).unwrap_or("");
            let params = &trig["inputs"]["parameters"];

            if op.contains("peekLockQueueMessages") || op.contains("receiveQueueMessages") {
                if let Some(q) = params.get("queueName").and_then(|q| q.as_str()) {
                    triggers.push(Link { kind: "queue".into(), target: q.into() });
                }
            }
            if op == "whenABlobIsAddedOrModified" {
                let conn = spc.get("connectionName").and_then(|c| c.as_str()).unwrap_or("?");
                let path = params.get("path").and_then(|p| p.as_str()).unwrap_or("?");
                triggers.push(Link { kind: "blob".into(), target: format!("{conn}:{path}") });
            }
        }
        "Request" => {
            triggers.push(Link { kind: "http".into(), target: "POST".into() });
        }
        "Recurrence" => {
            let freq = trig.get("recurrence")
                .and_then(|r| r.get("frequency"))
                .and_then(|f| f.as_str())
                .unwrap_or("?");
            triggers.push(Link { kind: "recurrence".into(), target: freq.into() });
        }
        "EventGridTrigger" => {
            triggers.push(Link { kind: "eventgrid".into(), target: "subscription".into() });
        }
        _ => {}
    }
}

fn walk_actions(actions: &serde_json::Map<String, Value>, sends: &mut Vec<Link>, calls: &mut Vec<Link>) {
    for (_name, action) in actions {
        extract_action(action, sends, calls);

        // Recurse into nested structures
        for key in &["actions", "else"] {
            if let Some(nested) = action.get(key) {
                if let Some(obj) = nested.as_object() {
                    // Check if it's actions directly or has an "actions" key
                    if obj.values().any(|v| v.get("type").is_some()) {
                        walk_actions(obj, sends, calls);
                    }
                }
                // else branch has { "actions": { ... } }
                if let Some(inner_actions) = nested.get("actions").and_then(|a| a.as_object()) {
                    walk_actions(inner_actions, sends, calls);
                }
            }
        }
    }
}

fn extract_action(action: &Value, sends: &mut Vec<Link>, calls: &mut Vec<Link>) {
    let typ = action.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match typ {
        "ServiceProvider" => {
            let spc = &action["inputs"]["serviceProviderConfiguration"];
            let op = spc.get("operationId").and_then(|o| o.as_str()).unwrap_or("");
            let params = &action["inputs"]["parameters"];

            if op == "sendMessage" {
                if let Some(q) = params.get("entityName").and_then(|q| q.as_str()) {
                    // Skip dynamic queue names (expressions)
                    if !q.contains("@{") && !q.contains("triggerBody") {
                        sends.push(Link { kind: "queue".into(), target: q.into() });
                    }
                }
            }
        }
        "Http" => {
            let headers = &action["inputs"]["headers"];
            if headers.get("aeg-sas-key").is_some() {
                sends.push(Link { kind: "eventgrid".into(), target: "EventGrid".into() });
            }
        }
        "Workflow" => {
            if let Some(id) = action["inputs"]["host"]["workflow"].get("id").and_then(|i| i.as_str()) {
                calls.push(Link { kind: "invoke".into(), target: id.into() });
            }
        }
        "Function" => {
            if let Some(conn) = action["inputs"]["function"].get("connectionName").and_then(|c| c.as_str()) {
                calls.push(Link { kind: "function".into(), target: format!("func:{conn}") });
            }
        }
        "Scope" | "Foreach" | "Until" | "If" => {
            // These contain nested actions — handled by walk_actions recursion
        }
        _ => {}
    }
}
