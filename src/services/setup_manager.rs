use std::path::Path;
use std::fs;
use std::collections::{HashMap, HashSet};
use serde_json::Value;
use regex::Regex;

#[derive(Debug, Clone, PartialEq)]
pub enum SetupStatus {
    Ready,
    NeedsInitialization,       // local.settings.json is missing, template exists
    NeedsConfiguration(usize), // X keys are "" or "$FETCH_FROM_AZURE"
    MissingKeys(Vec<String>),  // connections.json requires keys not found in settings
}

pub fn check_setup(dir: &str) -> SetupStatus {
    let p = Path::new(dir);
    let settings_path = p.join("local.settings.json");
    let template_path = p.join("local.settings.json.template");

    // 1. Initial Bootstrap
    if !settings_path.exists() {
        if template_path.exists() {
            return SetupStatus::NeedsInitialization;
        }
        return SetupStatus::Ready; 
    }

    // 2. Scan for TODOs
    let settings = read_settings(dir).unwrap_or_default();
    let todo_count = settings.values()
        .filter(|v| v.as_str().map_or(false, |s| s.is_empty() || s == "$FETCH_FROM_AZURE"))
        .count();
    
    if todo_count > 0 {
        return SetupStatus::NeedsConfiguration(todo_count);
    }

    // 3. Cross-reference with connections.json
    let missing = find_missing_keys_from_connections(dir, &settings);
    if !missing.is_empty() {
        return SetupStatus::MissingKeys(missing);
    }

    SetupStatus::Ready
}

pub fn initialize_from_template(dir: &str) -> Result<(), String> {
    let p = Path::new(dir);
    let settings_path = p.join("local.settings.json");
    let template_path = p.join("local.settings.json.template");

    if !template_path.exists() {
        return Err("Template file not found".into());
    }

    fs::copy(template_path, settings_path).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn find_missing_keys_from_connections(dir: &str, settings: &HashMap<String, Value>) -> Vec<String> {
    let connections_path = Path::new(dir).join("connections.json");
    if !connections_path.exists() {
        return vec![];
    }

    let mut missing = Vec::new();
    if let Ok(text) = fs::read_to_string(connections_path) {
        // Find all @appsetting('KEY_NAME')
        let re = Regex::new(r"@appsetting\('([^']+)'\)").unwrap();
        let mut required_keys = HashSet::new();
        for cap in re.captures_iter(&text) {
            required_keys.insert(cap[1].to_string());
        }

        for key in required_keys {
            if !settings.contains_key(&key) {
                missing.push(key);
            }
        }
    }
    missing.sort();
    missing
}

pub fn read_settings(dir: &str) -> Option<HashMap<String, Value>> {
    let path = Path::new(dir).join("local.settings.json");
    let text = fs::read_to_string(path).ok()?;
    let root: Value = serde_json::from_str(&text).ok()?;
    root["Values"].as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

pub fn apply_settings(dir: &str, updates: HashMap<String, String>) -> Result<(), String> {
    let path = Path::new(dir).join("local.settings.json");
    let text = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut root: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    
    if let Some(obj) = root["Values"].as_object_mut() {
        for (k, v) in updates {
            obj.insert(k, Value::String(v));
        }
    }

    let pretty = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    fs::write(path, pretty).map_err(|e| e.to_string())?;
    Ok(())
}
