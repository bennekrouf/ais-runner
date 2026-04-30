use regex::Regex;
use serde_json::Value;

/// Reads a workflow.json and returns a pretty-printed sample JSON body for its trigger.
/// Tries trigger.inputs.schema first, then the first ParseJson that consumes @triggerBody().
pub fn suggest_payload(logic_apps_dir: &str, workflow_name: &str) -> String {
    let path = std::path::Path::new(logic_apps_dir)
        .join(workflow_name)
        .join("workflow.json");

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return "{}".to_string(),
    };
    let workflow: Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return "{}".to_string(),
    };

    let defn = workflow.get("definition").unwrap_or(&workflow);

    // 1. Trigger-level schema
    if let Some(triggers) = defn["triggers"].as_object() {
        if let Some(trigger) = triggers.values().next() {
            let schema = &trigger["inputs"]["schema"];
            if schema.is_object() && !schema.as_object().map(|m| m.is_empty()).unwrap_or(true) {
                return pretty(schema_to_sample(schema));
            }
        }
    }

    // 2. First ParseJson action that reads triggerBody / triggerOutputs
    if let Some(actions) = defn["actions"].as_object() {
        if let Some(schema) = find_trigger_body_schema(actions) {
            return pretty(schema_to_sample(&schema));
        }
    }

    // 3. Regex-scan the raw workflow text for @triggerBody()?['field'] access chains
    if let Some(skeleton) = scan_trigger_body_refs(&content) {
        return pretty(skeleton);
    }

    "{}".to_string()
}

fn pretty(v: Value) -> String {
    serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".to_string())
}

fn find_trigger_body_schema(actions: &serde_json::Map<String, Value>) -> Option<Value> {
    for (_name, action) in actions {
        if action["type"].as_str() == Some("ParseJson") {
            let content = action["inputs"]["content"].as_str().unwrap_or("");
            if content.contains("triggerBody") || content.contains("triggerOutputs") {
                let schema = &action["inputs"]["schema"];
                if schema.is_object() && !schema.as_object()?.is_empty() {
                    return Some(schema.clone());
                }
            }
        }
        // recurse into nested scopes / foreach / conditions
        for sub_key in &["actions", "else"] {
            if let Some(nested) = action[sub_key].as_object() {
                if let Some(s) = find_trigger_body_schema(nested) {
                    return Some(s);
                }
            }
        }
    }
    None
}

fn schema_to_sample(schema: &Value) -> Value {
    let ty = schema["type"].as_str().unwrap_or("");
    match ty {
        "object" => {
            let mut map = serde_json::Map::new();
            if let Some(props) = schema["properties"].as_object() {
                for (k, v) in props {
                    map.insert(k.clone(), sample_named(k, v));
                }
            }
            Value::Object(map)
        }
        "array" => Value::Array(vec![schema_to_sample(&schema["items"])]),
        "integer" | "number" => Value::Number(serde_json::Number::from(0)),
        "boolean" => Value::Bool(false),
        _ => {
            // implicit object (no "type" but has "properties")
            if schema["properties"].is_object() {
                let mut map = serde_json::Map::new();
                if let Some(props) = schema["properties"].as_object() {
                    for (k, v) in props {
                        map.insert(k.clone(), sample_named(k, v));
                    }
                }
                Value::Object(map)
            } else {
                Value::String("text".to_string())
            }
        }
    }
}

/// Scan raw workflow JSON text for @triggerBody()?['k1']?['k2']... chains,
/// then build a nested skeleton object from all unique paths found.
fn scan_trigger_body_refs(content: &str) -> Option<Value> {
    // Match: triggerBody() optionally followed by one or more ?['key'] segments
    let chain_re = Regex::new(r"triggerBody\(\)\??(?:\[?'([^']+)'\]?\??)+").ok()?;
    let seg_re = Regex::new(r"\[?'([^']+)'\]?").ok()?;

    let mut paths: Vec<Vec<String>> = Vec::new();

    for cap in chain_re.find_iter(content) {
        let matched = cap.as_str();
        // strip the triggerBody() prefix
        let after = &matched["triggerBody()".len()..];
        let segs: Vec<String> = seg_re
            .captures_iter(after)
            .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
            .collect();
        if !segs.is_empty() && !paths.contains(&segs) {
            paths.push(segs);
        }
    }

    if paths.is_empty() {
        return None;
    }

    // Build a nested serde_json object from the collected paths
    let mut root = serde_json::Map::new();
    for path in &paths {
        insert_path(&mut root, path);
    }
    Some(Value::Object(root))
}

fn insert_path(map: &mut serde_json::Map<String, Value>, path: &[String]) {
    if path.is_empty() {
        return;
    }
    let key = &path[0];
    if path.len() == 1 {
        // Leaf — only set if not already a richer object
        map.entry(key.clone()).or_insert_with(|| leaf_value(key));
    } else {
        let child = map
            .entry(key.clone())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Value::Object(ref mut m) = child {
            insert_path(m, &path[1..]);
        }
    }
}

fn leaf_value(name: &str) -> Value {
    let n = name.to_lowercase();
    let s = if n.contains("error") || n.contains("message") {
        "Something went wrong"
    } else if n.contains("code") {
        "TEST-001"
    } else if n.contains("id") || n.contains("key") {
        "TEST-001"
    } else if n.contains("date") || n.contains("time") {
        "2026-04-29T10:00:00Z"
    } else {
        "text"
    };
    Value::String(s.to_string())
}

/// Generate a meaningful value based on the field name when type is string.
fn sample_named(name: &str, schema: &Value) -> Value {
    let ty = schema["type"].as_str().unwrap_or("string");
    if ty != "string" && ty != "" {
        return schema_to_sample(schema);
    }
    let n = name.to_lowercase();
    let s = if n.contains("date") || n.contains("time") {
        "2026-04-29T10:00:00Z"
    } else if n == "environment" || n.contains("env") {
        "dev"
    } else if n == "module" {
        "SageX3"
    } else if n == "source" {
        "manual"
    } else if n == "type" {
        "Invoice"
    } else if n.contains("id") || n.contains("key") {
        "TEST-001"
    } else if n.contains("by") || n.contains("user") {
        "test-user"
    } else if n == "value" {
        "example"
    } else {
        "text"
    };
    Value::String(s.to_string())
}
