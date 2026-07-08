use regex::Regex;
use serde_json::{json, Value};

/// Reads a workflow.json and returns a pretty-printed sample JSON body for its trigger.
/// Tries trigger.inputs.schema first, then the first ParseJson that consumes @triggerBody().
pub fn suggest_payload(logic_apps_dir: &str, workflow_name: &str) -> String {
    let path = crate::services::workflows::resolve_logic_apps_dir(logic_apps_dir)
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

    // Detect whether this is a Service Bus triggered workflow.
    // For SB triggers the runtime wraps the message body in { contentData: ... }.
    // The user only needs to provide the inner content — unwrap the envelope below.
    let is_sb_trigger = defn["triggers"].as_object()
        .and_then(|t| t.values().next())
        .map(|trigger| {
            trigger["inputs"]["serviceProviderConfiguration"]["serviceProviderId"]
                .as_str() == Some("/serviceProviders/serviceBus")
        })
        .unwrap_or(false);

    // 1. First ParseJson action that reads triggerBody / triggerOutputs / contentData.
    //    Checked before the trigger schema because it tends to be stricter (e.g. it
    //    adds `items` constraints on arrays that the trigger schema leaves open), and
    //    validation failures happen against this schema, not the trigger schema.
    if let Some(actions) = defn["actions"].as_object() {
        if let Some(schema) = find_trigger_body_schema(actions) {
            let mut sample = schema_to_sample("", &schema);
            // Overlay the literal values the workflow's own If/Switch branches
            // require (e.g. content.event.module == "Companies"). The schema
            // alone can't tell us these — without them the suggested payload is
            // structurally valid but routes to no branch and does nothing.
            apply_branch_literals(actions, &mut sample);
            // Then fix up well-known platform fields (numeric entity IDs, real
            // resource paths, UUID-shaped correlation ids) that name-based
            // heuristics get wrong in ways downstream systems reject.
            apply_domain_overrides(&mut sample);
            // For SB triggers: if the sample has a top-level "contentData" key,
            // unwrap it — the user posts the inner content; the SB trigger adds the
            // envelope automatically.
            if is_sb_trigger {
                if let Value::Object(ref map) = sample {
                    if map.len() == 1 {
                        if let Some(inner) = map.get("contentData") {
                            return pretty(inner.clone());
                        }
                    }
                }
            }
            return pretty(sample);
        }
    }

    // 2. Trigger-level schema (HTTP triggers only — SB triggers have no schema here).
    //    Used as a fallback when no ParseJson action is present.
    if let Some(triggers) = defn["triggers"].as_object() {
        if let Some(trigger) = triggers.values().next() {
            let schema = &trigger["inputs"]["schema"];
            if schema.is_object() && !schema.as_object().map(|m| m.is_empty()).unwrap_or(true) {
                return pretty(schema_to_sample("", schema));
            }
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

/// Walk every action and overlay onto `sample` the literal values that the
/// workflow's branch conditions gate on, so the suggested payload actually
/// routes through a branch instead of falling through.
///
/// Handles:
///  - `If.expression` → `equals` comparisons (nested through and/or/not),
///    where one side is a `@body(…)?['a']?['b']` reference and the other a
///    string literal. The literal is written at path `a/b/…` in the sample.
///  - `Switch` → its `expression` path is set to the first case's key.
///
/// When several branches gate the same field (e.g. one workflow with
/// Companies / Strategies / SupplyChainTanks branches) the last one wins —
/// any of them is a valid, routable value, which is all we need.
fn apply_branch_literals(actions: &serde_json::Map<String, Value>, sample: &mut Value) {
    for (_name, action) in actions {
        match action["type"].as_str() {
            Some("If") => collect_equals(&action["expression"], sample),
            Some("Switch") => {
                if let (Some(expr), Some(cases)) =
                    (action["expression"].as_str(), action["cases"].as_object())
                {
                    if let (Some(path), Some((case_key, _))) =
                        (parse_ref_path(expr), cases.iter().next())
                    {
                        set_path(sample, &path, Value::String(case_key.clone()));
                    }
                }
            }
            _ => {}
        }
        // Recurse into nested scopes / branches / switch cases.
        for sub_key in &["actions", "else", "cases", "default"] {
            if let Some(nested) = action[sub_key].as_object() {
                apply_branch_literals(nested, sample);
            }
        }
    }
}

/// Walk a boolean condition tree collecting `equals(<ref path>, <literal>)`
/// pairs and writing each literal into `sample` at the referenced path.
fn collect_equals(expr: &Value, sample: &mut Value) {
    let Some(obj) = expr.as_object() else { return };
    for (op, val) in obj {
        match op.as_str() {
            "and" | "or" | "not" => match val {
                Value::Array(items) => {
                    for item in items {
                        collect_equals(item, sample);
                    }
                }
                other => collect_equals(other, sample),
            },
            "equals" => {
                if let Some(arr) = val.as_array() {
                    if arr.len() == 2 {
                        // Exactly one side should be a reference path; the other
                        // the literal we want to inject.
                        let (a, b) = (&arr[0], &arr[1]);
                        if let (Some(path), Some(lit)) =
                            (a.as_str().and_then(parse_ref_path), b.as_str())
                        {
                            set_path(sample, &path, Value::String(lit.to_string()));
                        } else if let (Some(path), Some(lit)) =
                            (b.as_str().and_then(parse_ref_path), a.as_str())
                        {
                            set_path(sample, &path, Value::String(lit.to_string()));
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Extract the `['key']` access chain from a Logic Apps reference expression
/// such as `@body('Parse_JSON_x')?['data']?['msg']?['module']`. The function
/// argument lives in `('…')` (parens) so it is never captured — only the
/// `['…']` bracket segments are. Returns `None` for non-reference strings
/// (plain literals like `"Companies"`).
fn parse_ref_path(expr: &str) -> Option<Vec<String>> {
    let looks_like_ref = expr.contains("triggerBody(")
        || expr.contains("triggerOutputs(")
        || expr.contains("body(")
        || expr.contains("outputs(")
        || expr.contains("item(");
    if !looks_like_ref {
        return None;
    }
    let seg_re = Regex::new(r"\['([^']+)'\]").ok()?;
    let segs: Vec<String> = seg_re
        .captures_iter(expr)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .collect();
    if segs.is_empty() {
        None
    } else {
        Some(segs)
    }
}

/// Overlay domain-correct values for well-known platform fields, wherever they
/// appear in the sample. Schema-shape + name heuristics produce values that are
/// structurally fine but rejected downstream — e.g. `"CompanyId": "TEST-001"`
/// where JDE's `where ABALP1 = <id>` lookup needs a number, or
/// `"resourceURI": "text"` which gets concatenated onto the Ignite base URL.
///
/// Only exact field-name matches are touched, and only when the current value
/// is a placeholder string — a value injected by `apply_branch_literals` or
/// declared non-string by the schema is left alone.
fn apply_domain_overrides(sample: &mut Value) {
    // If a branch literal set a module (Companies / Strategies / …), use it to
    // build a matching resourceURI; default to Companies otherwise.
    let module = find_string_field(sample, "module").unwrap_or_else(|| "Companies".into());

    fn override_for(key: &str, module: &str) -> Option<Value> {
        Some(match key {
            // Numeric entity keys — JDE/Ignite reject string IDs
            "CompanyId" => json!(90001),
            "StrategyId" => json!(50000001),
            "TankId" => json!(70001),
            "CounterpartyId" => json!(90001),
            // Ignite HTTP call building blocks
            "resourceURI" => json!(format!("/api/{module}/search")),
            "endpoint" => json!("https://ignite.example.test"),
            "protocol" => json!("HTTP"),
            "method" => json!("POST"),
            // Correlation fields — UUID-shaped so runs are traceable
            "correlationId" => json!("00000000-0000-0000-0000-000000000001"),
            "identifier" => json!("00000000-0000-0000-0000-0000000a0001"),
            "parentId" => json!("00000000-0000-0000-0000-00000000000a"),
            _ => return None,
        })
    }

    fn is_placeholder(v: &Value) -> bool {
        matches!(v.as_str(), Some("TEST-001") | Some("text") | Some(""))
    }

    fn walk(v: &mut Value, module: &str) {
        match v {
            Value::Object(map) => {
                for (k, child) in map.iter_mut() {
                    if is_placeholder(child) {
                        if let Some(better) = override_for(k, module) {
                            *child = better;
                            continue;
                        }
                    }
                    walk(child, module);
                }
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, module);
                }
            }
            _ => {}
        }
    }

    walk(sample, &module);
}

/// Depth-first search for the first string value under a key named `field`.
fn find_string_field(v: &Value, field: &str) -> Option<String> {
    match v {
        Value::Object(map) => {
            if let Some(s) = map.get(field).and_then(|x| x.as_str()) {
                return Some(s.to_string());
            }
            map.values().find_map(|c| find_string_field(c, field))
        }
        Value::Array(items) => items.iter().find_map(|c| find_string_field(c, field)),
        _ => None,
    }
}

/// Set `value` at `path` in `root`, creating intermediate objects as needed.
/// No-op if `root` (or an intermediate we need to descend into) isn't an object.
fn set_path(root: &mut Value, path: &[String], value: Value) {
    let Some(obj) = root.as_object_mut() else { return };
    let Some((first, rest)) = path.split_first() else { return };
    if rest.is_empty() {
        obj.insert(first.clone(), value);
    } else {
        let child = obj
            .entry(first.clone())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        set_path(child, rest, value);
    }
}

fn find_trigger_body_schema(actions: &serde_json::Map<String, Value>) -> Option<Value> {
    for (_name, action) in actions {
        if action["type"].as_str() == Some("ParseJson") {
            let content = action["inputs"]["content"].as_str().unwrap_or("");
            // triggerBody/triggerOutputs = HTTP/Recurrence trigger
            // contentData = Service Bus message body (item()?['contentData'])
            // items('...') = inside a For_each that iterates over triggerBody()
            if content.contains("triggerBody")
                || content.contains("triggerOutputs")
                || content.contains("contentData")
                || content.contains("items(")
            {
                let schema = &action["inputs"]["schema"];
                if schema.is_object() && !schema.as_object()?.is_empty() {
                    return Some(schema.clone());
                }
            }
        }
        // recurse into nested scopes / foreach / switch / conditions
        for sub_key in &["actions", "else", "cases", "default"] {
            if let Some(nested) = action[sub_key].as_object() {
                if let Some(s) = find_trigger_body_schema(nested) {
                    return Some(s);
                }
            }
        }
    }
    None
}

/// Extract the effective type string from a schema, handling both `"type": "foo"`
/// and `"type": ["foo", "null"]` (JSON Schema draft-04 nullable convention).
fn resolve_type(schema: &Value) -> &str {
    match &schema["type"] {
        Value::String(s) => s.as_str(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .find(|s| *s != "null")
            .unwrap_or(""),
        _ => "",
    }
}

/// `field_name` is the key whose schema this is — used to generate meaningful
/// samples when the schema is `{"type":"object"}` with no `properties`.
fn schema_to_sample(field_name: &str, schema: &Value) -> Value {
    match resolve_type(schema) {
        "object" => {
            if let Some(props) = schema["properties"].as_object() {
                let mut map = serde_json::Map::new();
                for (k, v) in props {
                    map.insert(k.clone(), sample_named(k, v));
                }
                Value::Object(map)
            } else {
                // No properties defined — use the field name to provide a meaningful sample
                sample_object_by_name(field_name)
            }
        }
        "array" => Value::Array(vec![schema_to_sample("item", &schema["items"])]),
        "integer" | "number" => Value::Number(serde_json::Number::from(0)),
        "boolean" => Value::Bool(false),
        _ => {
            // implicit object — no "type" field but has "properties"
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

/// Returns a meaningful sample object for well-known field names that have
/// `"type": "object"` but no `properties` in the schema (open-ended schemas).
fn sample_object_by_name(name: &str) -> Value {
    match name.to_lowercase().as_str() {
        // CloudEvent envelope — used as the main event carrier in this platform
        "cloudevent" => json!({
            "specversion": "1.0",
            "type": "com.oryx.event",
            "source": "manual",
            "id": "TEST-001",
            "time": "2026-01-01T00:00:00Z",
            "data": {
                "msg": {
                    "correlationId": "TEST-001",
                    "identifier":    "TEST-001",
                    "parentId":      "",
                    "schema":        "",
                    "content":       {}
                }
            }
        }),
        // Inner message envelope
        "msg" => json!({
            "correlationId": "TEST-001",
            "identifier":    "TEST-001",
            "parentId":      "",
            "schema":        "",
            "content":       {}
        }),
        // CloudEvent data wrapper
        "data" => json!({
            "msg": {
                "correlationId": "TEST-001",
                "identifier":    "TEST-001",
                "parentId":      "",
                "content":       {}
            }
        }),
        // ais.workflow.error — the standard error envelope produced by AIS-GenericCatch
        // and consumed by AIS-Error-Teams-Notif (CA03).
        "ais.workflow.error" => json!({
            "action": "Upload_to_Kyriba",
            "message": "The service provider action failed with error code 'EmptyFile'",
            "messageDetails": "",
            "workflowStack": [
                {
                    "name": "Send-Kyriba-files-broadcast",
                    "identifier": "TEST-RUN-001",
                    "startTime": "2026-01-01T10:00:00Z",
                    "input": { "name": "payments/test.txt" }
                }
            ]
        }),
        // connector and content are intentionally open — empty is valid
        "connector" | "content" => Value::Object(serde_json::Map::new()),
        // Anything else with no schema — leave empty
        _ => Value::Object(serde_json::Map::new()),
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

/// Generate a meaningful value based on the field name and its schema type.
fn sample_named(name: &str, schema: &Value) -> Value {
    let ty = resolve_type(schema);
    if !ty.is_empty() && ty != "string" {
        // Pass the field name so schema_to_sample can generate smart object samples
        return schema_to_sample(name, schema);
    }
    let n = name.to_lowercase();
    let s = if n.contains("date") || n.contains("time") {
        "2026-01-01T00:00:00Z"
    } else if n == "environment" || n.contains("env") {
        "dev"
    } else if n == "module" {
        "SageX3"
    } else if n == "source" {
        "manual"
    } else if n == "type" {
        "com.oryx.event"
    } else if n == "specversion" {
        "1.0"
    } else if n.contains("schema") {
        // inputschema / outputschema — use CloudEvent as default
        "CloudEvent"
    } else if n.contains("id") || n.contains("key") {
        "TEST-001"
    } else if n.contains("by") || n.contains("user") {
        "test-user"
    } else if n == "value" {
        "example"
    } else if n.contains("name") {
        name   // use the field name itself as a hint
    } else {
        "text"
    };
    Value::String(s.to_string())
}

/// Result of normalising a user-typed Send Message payload.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalisedSendBody {
    /// What to actually push down the AMQP wire.
    pub body:   String,
    /// True when we stripped a top-level `contentData` envelope so the UI can
    /// flag that to the user — silently double-wrapped messages cause hours
    /// of "param NULL" debugging downstream.
    pub stripped_envelope: bool,
}

/// Normalise a payload before sending it to a Service Bus queue.
///
/// **Why this exists.** The Logic Apps Service Bus trigger automatically
/// wraps the AMQP message body inside `{ contentData: …, contentType: …, … }`
/// for the workflow to consume. If the user pastes a payload that ALREADY
/// has a top-level `contentData` key — typically because they copied it from
/// a captured message — and we send it verbatim, the trigger wraps it again
/// and the workflow sees `contentData.contentData.data.msg.…` instead of
/// `contentData.data.msg.…`. The symptom is a cryptic "param NULL" SP error
/// or a JSON-schema validation failure deep inside the workflow.
///
/// We detect that shape conservatively — only when `contentData` is the
/// single top-level key (or paired with `contentType` / `userProperties`,
/// the canonical envelope shape) so payloads that legitimately use
/// `contentData` as a field name alongside other top-level data aren't
/// molested.
///
/// Non-JSON payloads pass through untouched.
pub fn normalise_send_body(raw: &str) -> NormalisedSendBody {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        // Not JSON — send as-is. Plain-text payloads are valid.
        return NormalisedSendBody { body: raw.to_string(), stripped_envelope: false };
    };
    let Value::Object(ref map) = v else {
        return NormalisedSendBody { body: raw.to_string(), stripped_envelope: false };
    };
    let is_envelope = map.contains_key("contentData") && match map.len() {
        1 => true,
        2 => map.keys().all(|k| k == "contentData" || k == "contentType"),
        3 => map.keys().all(|k| k == "contentData" || k == "contentType" || k == "userProperties"),
        _ => false,
    };
    if !is_envelope {
        return NormalisedSendBody { body: raw.to_string(), stripped_envelope: false };
    }
    let inner = map.get("contentData").cloned().unwrap_or(Value::Null);
    let body = serde_json::to_string_pretty(&inner)
        .unwrap_or_else(|_| raw.to_string());
    NormalisedSendBody { body, stripped_envelope: true }
}

#[cfg(test)]
mod branch_literal_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_ref_path_extracts_bracket_segments_only() {
        let expr = "@body('Parse_JSON_ais.event.ignite')?['data']?['msg']?['content']?['event']?['module']";
        assert_eq!(
            parse_ref_path(expr),
            Some(vec![
                "data".into(),
                "msg".into(),
                "content".into(),
                "event".into(),
                "module".into()
            ])
        );
    }

    #[test]
    fn parse_ref_path_ignores_plain_literals() {
        assert_eq!(parse_ref_path("Companies"), None);
    }

    #[test]
    fn if_equals_overwrites_placeholder_module() {
        // Mirrors Send-Http-Get-Ignite-AddressBook's Companies branch.
        let actions = json!({
            "Module_=_Companies": {
                "type": "If",
                "expression": { "and": [ { "equals": [
                    "@body('Parse_JSON_ais.event.ignite')?['data']?['msg']?['content']?['event']?['module']",
                    "Companies"
                ] } ] },
                "actions": {}
            }
        });
        let mut sample = json!({
            "data": { "msg": { "content": { "event": { "module": "SageX3" } } } }
        });
        apply_branch_literals(actions.as_object().unwrap(), &mut sample);
        assert_eq!(sample["data"]["msg"]["content"]["event"]["module"], "Companies");
    }

    #[test]
    fn equals_literal_first_operand_order_is_handled() {
        let actions = json!({
            "Cond": {
                "type": "If",
                "expression": { "equals": [
                    "COMPANY",
                    "@body('P')?['data']?['msg']?['content']?['PartyType']"
                ] },
                "actions": {}
            }
        });
        let mut sample = json!({ "data": { "msg": { "content": {} } } });
        apply_branch_literals(actions.as_object().unwrap(), &mut sample);
        assert_eq!(sample["data"]["msg"]["content"]["PartyType"], "COMPANY");
    }

    #[test]
    fn nested_branch_conditions_are_applied() {
        let actions = json!({
            "Outer": {
                "type": "If",
                "expression": { "equals": [
                    "@body('P')?['content']?['module']", "Companies"
                ] },
                "actions": {
                    "Inner": {
                        "type": "If",
                        "expression": { "equals": [
                            "@body('P')?['content']?['sub']", "Strategies"
                        ] },
                        "actions": {}
                    }
                }
            }
        });
        let mut sample = json!({ "content": {} });
        apply_branch_literals(actions.as_object().unwrap(), &mut sample);
        assert_eq!(sample["content"]["module"], "Companies");
        assert_eq!(sample["content"]["sub"], "Strategies");
    }

    #[test]
    fn domain_overrides_fix_ids_and_uri_using_branch_module() {
        let mut sample = json!({
            "data": { "msg": { "content": {
                "event":  { "module": "Companies" },
                "object": {
                    "resourceURI": "text",
                    "endpoint": "text",
                    "method": "text",
                    "CompanyId": "TEST-001",
                    "StrategyId": "TEST-001"
                }
            },
            "correlationId": "TEST-001"
            } }
        });
        apply_domain_overrides(&mut sample);
        let obj = &sample["data"]["msg"]["content"]["object"];
        assert_eq!(obj["resourceURI"], "/api/Companies/search");
        assert_eq!(obj["endpoint"], "https://ignite.example.test");
        assert_eq!(obj["CompanyId"], 90001);
        assert_eq!(obj["StrategyId"], 50000001);
        assert_eq!(
            sample["data"]["msg"]["correlationId"],
            "00000000-0000-0000-0000-000000000001"
        );
        // module itself is untouched — it came from a branch literal
        assert_eq!(sample["data"]["msg"]["content"]["event"]["module"], "Companies");
    }

    #[test]
    fn domain_overrides_leave_non_placeholder_values_alone() {
        let mut sample = json!({
            "object": { "CompanyId": 12345, "resourceURI": "/api/custom/path" }
        });
        apply_domain_overrides(&mut sample);
        assert_eq!(sample["object"]["CompanyId"], 12345);
        assert_eq!(sample["object"]["resourceURI"], "/api/custom/path");
    }

    #[test]
    fn switch_sets_first_case_key() {
        let actions = json!({
            "Route": {
                "type": "Switch",
                "expression": "@body('P')?['content']?['kind']",
                "cases": {
                    "First":  { "case": "ORDER",  "actions": {} },
                    "Second": { "case": "INVOICE", "actions": {} }
                },
                "default": { "actions": {} }
            }
        });
        let mut sample = json!({ "content": {} });
        apply_branch_literals(actions.as_object().unwrap(), &mut sample);
        assert!(sample["content"]["kind"].is_string());
    }
}

#[cfg(test)]
mod send_body_tests {
    use super::*;

    #[test]
    fn strips_pure_contentdata_envelope() {
        let raw = r#"{ "contentData": { "data": { "msg": { "id": 1 } } } }"#;
        let n = normalise_send_body(raw);
        assert!(n.stripped_envelope);
        let parsed: serde_json::Value = serde_json::from_str(&n.body).unwrap();
        assert_eq!(parsed["data"]["msg"]["id"], 1);
    }

    #[test]
    fn strips_canonical_two_key_envelope() {
        // What you get when you copy a peeked SB message verbatim.
        let raw = r#"{
            "contentData": { "x": 1 },
            "contentType": "application/json"
        }"#;
        let n = normalise_send_body(raw);
        assert!(n.stripped_envelope);
        let parsed: serde_json::Value = serde_json::from_str(&n.body).unwrap();
        assert_eq!(parsed["x"], 1);
    }

    #[test]
    fn leaves_payload_alone_when_contentdata_is_one_of_many_fields() {
        let raw = r#"{ "contentData": "abc", "otherField": "xyz", "another": 1 }"#;
        let n = normalise_send_body(raw);
        assert!(!n.stripped_envelope);
        assert_eq!(n.body, raw);
    }

    #[test]
    fn leaves_non_json_payloads_alone() {
        let raw = "this is plain text; not JSON";
        let n = normalise_send_body(raw);
        assert!(!n.stripped_envelope);
        assert_eq!(n.body, raw);
    }

    #[test]
    fn leaves_payload_without_contentdata_alone() {
        let raw = r#"{ "data": { "msg": { "id": 1 } } }"#;
        let n = normalise_send_body(raw);
        assert!(!n.stripped_envelope);
        assert_eq!(n.body, raw);
    }
}
