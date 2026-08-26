use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct MapFile {
    pub name: String,     // filename without extension, e.g. "my-transform"
    pub filename: String, // full filename, e.g. "my-transform.liquid"
    pub path: String,     // absolute path
    pub kind: MapKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MapKind {
    Liquid,
    Xslt,
    #[allow(dead_code)]
    Other,
}

impl MapKind {
    #[allow(dead_code)]
    pub fn label(&self) -> &'static str {
        match self {
            MapKind::Liquid => "Liquid",
            MapKind::Xslt => "XSLT",
            MapKind::Other => "Map",
        }
    }
    pub fn icon(&self) -> &'static str {
        match self {
            MapKind::Liquid => "🔄",
            MapKind::Xslt => "📄",
            MapKind::Other => "🗂",
        }
    }
}

/// Recursively scan under `logic_apps_dir` for map files (.liquid, .xslt, .xsl).
/// Works regardless of where the customer stores their maps.
/// Skips `node_modules`, `.git`, and hidden directories.
pub fn scan_maps(logic_apps_dir: &str) -> Vec<MapFile> {
    let root = Path::new(logic_apps_dir);
    let mut result = Vec::new();
    walk(root, root, 0, &mut result);
    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<MapFile>) {
    if depth > 6 {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let fname = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        // skip hidden dirs, node_modules, .git
        if fname.starts_with('.') || fname == "node_modules" {
            continue;
        }

        if path.is_dir() {
            walk(root, &path, depth + 1, out);
        } else if path.is_file() {
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let kind = match ext.as_str() {
                "liquid" => MapKind::Liquid,
                "xslt" | "xsl" => MapKind::Xslt,
                _ => continue, // skip non-map files
            };
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            // show path relative to root for clarity
            let rel = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| fname.clone());

            out.push(MapFile {
                name,
                filename: rel,
                path: path.to_string_lossy().to_string(),
                kind,
            });
        }
    }
}

use crate::services::workflow_analysis;
use std::collections::HashMap;

/// Scan all workflow.json files under `logic_apps_dir` and return
/// an inverted index: map_name → Vec<workflow_name>.
pub fn scan_workflow_map_usages(logic_apps_dir: &str) -> HashMap<String, Vec<String>> {
    let root = Path::new(logic_apps_dir);
    let mut index: HashMap<String, Vec<String>> = HashMap::new();

    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return index,
    };

    for entry in entries.flatten() {
        let wf_dir = entry.path();
        if !wf_dir.is_dir() {
            continue;
        }
        let wf_json = wf_dir.join("workflow.json");
        let Ok(src) = std::fs::read_to_string(&wf_json) else {
            continue;
        };
        let analysis = workflow_analysis::analyse(&src);
        if analysis.liquid_maps.is_empty() {
            continue;
        }
        let wf_name = wf_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        for map in analysis.liquid_maps {
            index.entry(map).or_default().push(wf_name.clone());
        }
    }
    index
}

/// Which engine was used for the last eval.
#[derive(Debug, Clone, PartialEq)]
pub enum LiquidEngine {
    DotLiquid, // via `dotnet` CLI — exact Azure Logic Apps behaviour
    Stdlib,    // liquid 0.26 + DotLiquid-compat filters — covers most cases
}

/// Returns true if `dotnet` CLI is available on PATH.
pub fn dotnet_available() -> bool {
    std::process::Command::new("dotnet")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Returns true if `dotnet-script` global tool is installed.
pub fn dotnet_script_available() -> bool {
    std::process::Command::new("dotnet")
        .args(["script", "--version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Install `dotnet-script` via `dotnet tool install -g dotnet-script`.
/// Returns Ok(()) on success or Err(message) on failure.
pub fn install_dotnet_script() -> Result<(), String> {
    let out = std::process::Command::new("dotnet")
        .args(["tool", "install", "-g", "dotnet-script"])
        .output()
        .map_err(|e| format!("Failed to run dotnet: {}", e))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Evaluate a Liquid template against a JSON input string.
/// Tries DotLiquid via `dotnet` first; falls back to the Rust liquid crate
/// with DotLiquid-compatible filters added.
/// Returns (rendered_output, engine_used).
pub fn eval_liquid(template_src: &str, input_json: &str) -> Result<(String, LiquidEngine), String> {
    // Validate JSON first
    serde_json::from_str::<serde_json::Value>(input_json)
        .map_err(|e| format!("Invalid JSON input: {}", e))?;

    if dotnet_available() {
        if let Ok(out) = eval_liquid_dotnet(template_src, input_json) {
            return Ok((out, LiquidEngine::DotLiquid));
        }
    }

    eval_liquid_stdlib(template_src, input_json).map(|out| (out, LiquidEngine::Stdlib))
}

// ── DotLiquid via dotnet ──────────────────────────────────────────────────────

fn eval_liquid_dotnet(template_src: &str, input_json: &str) -> Result<String, String> {
    // Write a self-contained C# program to a temp file and run it with `dotnet-script`
    // or a temp project. We use `dotnet script` (dotnet-script global tool) if available,
    // otherwise fall back to writing a csx and running it.
    let tmp_dir = std::env::temp_dir().join("ais-runner-liquid");
    std::fs::create_dir_all(&tmp_dir).map_err(|e| e.to_string())?;

    // Write template and input to temp files so we don't need shell escaping
    let tpl_path = tmp_dir.join("template.liquid");
    let json_path = tmp_dir.join("input.json");
    std::fs::write(&tpl_path, template_src).map_err(|e| e.to_string())?;
    std::fs::write(&json_path, input_json).map_err(|e| e.to_string())?;

    // C# script using DotLiquid via NuGet
    let csx = format!(
        r#"
#r "nuget: DotLiquid, 2.1.0"
using DotLiquid;
using System.IO;

var template = Template.Parse(File.ReadAllText("{tpl}"));
var input    = File.ReadAllText("{json}");
var hash     = Hash.FromJson(input);
Console.Write(template.Render(hash));
"#,
        tpl = tpl_path.to_string_lossy().replace('\\', "/"),
        json = json_path.to_string_lossy().replace('\\', "/"),
    );

    let csx_path = tmp_dir.join("run.csx");
    std::fs::write(&csx_path, &csx).map_err(|e| e.to_string())?;

    let out = std::process::Command::new("dotnet")
        .args(["script", csx_path.to_str().unwrap_or("")])
        .output()
        .map_err(|e| e.to_string())?;

    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).to_string())
    }
}

// ── liquid 0.26 + DotLiquid-compat filters ───────────────────────────────────

fn eval_liquid_stdlib(template_src: &str, input_json: &str) -> Result<String, String> {
    let input: serde_json::Value =
        serde_json::from_str(input_json).map_err(|e| format!("Invalid JSON input: {}", e))?;

    // Strip empty whitespace-control tags like {{-  -}} or {{  }} that are valid
    // in Azure's DotLiquid but rejected by the liquid 0.26 parser.
    let re_empty = regex::Regex::new(r"\{\{-?\s*-?\}\}").unwrap();
    let template_src = re_empty.replace_all(template_src, "");

    let parser = liquid::ParserBuilder::with_stdlib()
        .filter(JsonFilterParser)
        .filter(Base64EncodeParser)
        .filter(Base64DecodeParser)
        .build()
        .map_err(|e| format!("Parser build: {}", e))?;

    let template = parser
        .parse(template_src.as_ref())
        .map_err(|e| format!("Template parse: {}", e))?;

    let globals = json_to_liquid(&input).map_err(|e| format!("Input conversion: {}", e))?;

    template
        .render(&globals)
        .map_err(|e| format!("Render: {}", e))
}

// ── DotLiquid-compat filter implementations (manual — no derive macros) ────────

use liquid_core::{Filter, FilterReflection, ParseFilter, Runtime, Value, ValueView};

macro_rules! simple_filter {
    ($parser:ident, $eval:ident, $name:literal, $desc:literal, $body:expr) => {
        #[derive(Clone)]
        struct $parser;
        impl FilterReflection for $parser {
            fn name(&self) -> &'static str {
                $name
            }
            fn description(&self) -> &'static str {
                $desc
            }
            fn positional_parameters(&self) -> &'static [liquid_core::parser::ParameterReflection] {
                &[]
            }
            fn keyword_parameters(&self) -> &'static [liquid_core::parser::ParameterReflection] {
                &[]
            }
        }
        impl ParseFilter for $parser {
            fn parse(
                &self,
                _: liquid_core::parser::FilterArguments,
            ) -> liquid_core::Result<Box<dyn Filter>> {
                Ok(Box::new($eval))
            }
            fn reflection(&self) -> &dyn FilterReflection {
                self
            }
        }
        #[derive(Debug)]
        struct $eval;
        impl std::fmt::Display for $eval {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", $name)
            }
        }
        impl Filter for $eval {
            fn evaluate(
                &self,
                input: &dyn ValueView,
                _: &dyn Runtime,
            ) -> liquid_core::Result<Value> {
                $body(input)
            }
        }
    };
}

simple_filter!(
    JsonFilterParser,
    JsonFilterEval,
    "json",
    "Serialize as JSON string (DotLiquid compat)",
    |input: &dyn ValueView| {
        let v = input.to_value();
        let s = serde_json::to_string(&liquid_val_to_json(&v)).unwrap_or_default();
        Ok(Value::Scalar(s.into()))
    }
);

simple_filter!(
    Base64EncodeParser,
    Base64EncodeEval,
    "Base64Encode",
    "Base64-encode a string (DotLiquid compat)",
    |input: &dyn ValueView| {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(input.to_kstr().as_bytes());
        Ok(Value::Scalar(encoded.into()))
    }
);

simple_filter!(
    Base64DecodeParser,
    Base64DecodeEval,
    "Base64Decode",
    "Base64-decode a string (DotLiquid compat)",
    |input: &dyn ValueView| {
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(input.to_kstr().as_bytes())
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .unwrap_or_default();
        Ok(Value::Scalar(decoded.into()))
    }
);

fn liquid_val_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Nil => serde_json::Value::Null,
        Value::Scalar(s) => {
            if let Some(b) = s.to_bool() {
                return serde_json::Value::Bool(b);
            }
            if let Some(i) = s.to_integer() {
                return serde_json::json!(i);
            }
            if let Some(f) = s.to_float() {
                return serde_json::json!(f);
            }
            serde_json::Value::String(s.to_kstr().to_string())
        }
        Value::Array(arr) => serde_json::Value::Array(arr.iter().map(liquid_val_to_json).collect()),
        Value::Object(obj) => {
            let mut map = serde_json::Map::new();
            for (k, v) in obj.iter() {
                map.insert(k.to_string(), liquid_val_to_json(v));
            }
            serde_json::Value::Object(map)
        }
        Value::State(_) => serde_json::Value::Null,
    }
}

fn json_to_liquid(v: &serde_json::Value) -> Result<liquid::Object, String> {
    match v {
        serde_json::Value::Object(map) => {
            let mut obj = liquid::Object::new();
            for (k, val) in map {
                obj.insert(k.to_string().into(), json_val_to_liquid(val)?);
            }
            Ok(obj)
        }
        _ => {
            let mut obj = liquid::Object::new();
            obj.insert("content".into(), json_val_to_liquid(v)?);
            Ok(obj)
        }
    }
}

fn json_val_to_liquid(v: &serde_json::Value) -> Result<liquid::model::Value, String> {
    use liquid::model::Value;
    Ok(match v {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(b) => Value::Scalar((*b).into()),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Scalar(i.into())
            } else {
                Value::Scalar(n.as_f64().unwrap_or(0.0).into())
            }
        }
        serde_json::Value::String(s) => Value::Scalar(s.clone().into()),
        serde_json::Value::Array(arr) => {
            let items: Result<Vec<_>, _> = arr.iter().map(json_val_to_liquid).collect();
            Value::Array(items?)
        }
        serde_json::Value::Object(map) => {
            let mut obj = liquid::Object::new();
            for (k, val) in map {
                obj.insert(k.to_string().into(), json_val_to_liquid(val)?);
            }
            Value::Object(obj)
        }
    })
}

/// Scan a Liquid template and suggest a skeleton JSON input based on
/// the variable paths referenced (e.g. `{{ content.event.source }}`
/// → `{"content":{"event":{"source":""}}}`).
pub fn suggest_liquid_input(template_src: &str) -> String {
    let mut root = serde_json::Map::new();

    // Match {{ ... }} blocks (including whitespace variants)
    let re = regex::Regex::new(r"\{\{-?\s*(.*?)\s*-?\}\}").unwrap();

    for cap in re.captures_iter(template_src) {
        let expr = &cap[1];

        // Strip filter chain: take only the part before the first `|`
        let var_part = expr.split('|').next().unwrap_or("").trim();

        // Skip Logic Apps / Liquid built-ins and special chars
        if var_part.is_empty()
            || var_part.contains('(')   // function call
            || var_part.contains('\'')  // string literal
            || var_part.contains('"')
            || var_part.starts_with('@')
            || var_part.starts_with("for ")
            || var_part.starts_with("if ")
        {
            continue;
        }

        // Build nested path: "content.event.source" → nested object
        let parts: Vec<&str> = var_part
            .split('.')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if parts.is_empty() {
            continue;
        }

        insert_path(&mut root, &parts);
    }

    if root.is_empty() {
        "{}".to_string()
    } else {
        serde_json::to_string_pretty(&serde_json::Value::Object(root))
            .unwrap_or_else(|_| "{}".to_string())
    }
}

fn insert_path(obj: &mut serde_json::Map<String, serde_json::Value>, parts: &[&str]) {
    if parts.is_empty() {
        return;
    }
    let key = parts[0].to_string();
    if parts.len() == 1 {
        obj.entry(key.clone()).or_insert_with(|| fake_value(&key));
    } else {
        let child = obj
            .entry(key)
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        if let serde_json::Value::Object(nested) = child {
            insert_path(nested, &parts[1..]);
        }
    }
}

/// Produce a realistic placeholder value based on the field name.
fn fake_value(field: &str) -> serde_json::Value {
    use serde_json::Value;
    let k = field.to_lowercase();

    // UUIDs / correlation IDs
    if k.contains("id")
        && (k.contains("correlation") || k.contains("trace") || k.contains("request"))
    {
        return Value::String("3fa85f64-5717-4562-b3fc-2c963f66afa6".into());
    }
    if k == "id" || k.ends_with("id") {
        return Value::String("00000000-0000-0000-0000-000000000001".into());
    }

    // Dates / times
    if k.contains("date")
        || k.contains("time")
        || k.contains("createdon")
        || k.contains("modifiedon")
    {
        return Value::String("2026-01-15T10:00:00Z".into());
    }

    // Booleans
    if k.starts_with("is")
        || k.starts_with("has")
        || k.starts_with("can")
        || k == "enabled"
        || k == "active"
        || k == "success"
        || k == "valid"
    {
        return Value::Bool(true);
    }

    // Numbers
    if k == "count"
        || k == "total"
        || k == "amount"
        || k == "size"
        || k == "version"
        || k == "revision"
        || k == "rank"
    {
        return Value::Number(0.into());
    }

    // URLs / endpoints
    if k.contains("url") || k.contains("endpoint") || k.contains("uri") || k.contains("href") {
        return Value::String("https://example.com/api".into());
    }

    // Email
    if k.contains("email") || k.contains("mail") {
        return Value::String("user@example.com".into());
    }

    // Environment
    if k == "environment" || k == "env" {
        return Value::String("DEV".into());
    }

    // Source / type / status
    if k == "source" {
        return Value::String("LogicApp".into());
    }
    if k == "type" {
        return Value::String("Event".into());
    }
    if k == "status" {
        return Value::String("active".into());
    }
    if k == "module" {
        return Value::String("Companies".into());
    }
    if k == "method" {
        return Value::String("POST".into());
    }
    if k == "protocol" {
        return Value::String("https".into());
    }

    // Name / display / description
    if k.contains("name") || k.contains("title") || k.contains("label") {
        return Value::String(format!("Sample {}", capitalise(field)));
    }
    if k.contains("description")
        || k.contains("message")
        || k.contains("text")
        || k.contains("content")
    {
        return Value::String(format!("Sample {} value", field));
    }

    // Default: empty string
    Value::String(String::new())
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}
