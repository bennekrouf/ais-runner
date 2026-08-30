//! Provision `function_apps/local.settings.json`.
//!
//! The Logic Apps side of a project gets its settings seeded from
//! `connections.json`'s `@appsetting()` refs (see `setup_manager`). A Java
//! function app declares its keys somewhere else entirely — `System.getenv`,
//! the `connection` / `connectionStringSetting` attributes on trigger
//! annotations, and `%NAME%` binding placeholders — so none of that machinery
//! ever saw it. With no file to copy, the azure-functions Maven plugin stages a
//! stub holding only `FUNCTIONS_WORKER_RUNTIME`, and the host refuses to boot:
//! "Missing value for AzureWebJobsStorage in local.settings.json".
//!
//! Values reuse `setup_manager::smart_default`, which already knows the
//! emulator endpoints. Keys it cannot resolve are written empty rather than
//! guessed, matching how the settings editor surfaces unresolved keys.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::services::setup_manager::smart_default;

/// `AzureWebJobsStorage` is required unless every trigger is one of these.
/// Listed by the Functions host itself in the error it raises.
const STORAGE_FREE_TRIGGERS: [&str; 6] = [
    "HttpTrigger",
    "KafkaTrigger",
    "RabbitMQTrigger",
    "OrchestrationTrigger",
    "ActivityTrigger",
    "EntityTrigger",
];

/// `<project>/function_apps`, or the project root when there is no such dir.
/// Mirrors `workflows::resolve_logic_apps_dir`.
pub fn resolve_function_apps_dir(base_dir: &str) -> PathBuf {
    let p = Path::new(base_dir);
    for candidate in ["function_apps", "function-apps"] {
        if p.join(candidate).exists() {
            return p.join(candidate);
        }
    }
    p.to_path_buf()
}

/// Every app-setting key the Java sources reference, plus the runtime keys the
/// host needs regardless. Empty when the app has no Java sources.
pub fn required_keys(function_apps_dir: &Path) -> Vec<String> {
    let sources = java_sources(function_apps_dir);
    if sources.is_empty() {
        return Vec::new();
    }

    let mut keys: HashSet<String> = HashSet::new();
    let mut needs_storage = false;
    for text in &sources {
        keys.extend(referenced_keys(text));
        needs_storage |= has_storage_backed_trigger(text);
    }

    keys.insert("FUNCTIONS_WORKER_RUNTIME".to_string());
    if needs_storage {
        keys.insert("AzureWebJobsStorage".to_string());
    }

    let mut out: Vec<String> = keys.into_iter().collect();
    out.sort();
    out
}

/// Keys named by one source file: `System.getenv("X")`, the `connection` and
/// `connectionStringSetting` binding attributes, and `%X%` placeholders.
fn referenced_keys(text: &str) -> HashSet<String> {
    let mut keys = HashSet::new();

    for rest in text.split("System.getenv(").skip(1) {
        if let Some(key) = leading_string_literal(rest) {
            keys.insert(key);
        }
    }
    for attr in ["connection", "connectionStringSetting"] {
        keys.extend(attribute_values(text, attr));
    }
    // %NAME% binding placeholders resolve against app settings.
    for candidate in text.split('%').skip(1).step_by(2) {
        if is_setting_name(candidate) {
            keys.insert(candidate.to_string());
        }
    }

    keys.retain(|k| is_setting_name(k));
    keys
}

/// The string literal that opens `rest`, ignoring leading whitespace.
fn leading_string_literal(rest: &str) -> Option<String> {
    let after_quote = rest.trim_start().strip_prefix('"')?;
    let close = after_quote.find('"')?;
    Some(after_quote[..close].to_string())
}

/// Values assigned to `attr = "..."`. Matched as a whole word so `connection`
/// does not also pick up `connectionStringSetting`'s own value.
fn attribute_values(text: &str, attr: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (at, _) in text.match_indices(attr) {
        let preceded_by_ident = matches!(
            text[..at].chars().last(),
            Some(c) if c.is_ascii_alphanumeric() || c == '_'
        );
        let rest = &text[at + attr.len()..];
        let followed_by_ident = matches!(
            rest.chars().next(),
            Some(c) if c.is_ascii_alphanumeric() || c == '_'
        );
        if preceded_by_ident || followed_by_ident {
            continue;
        }
        let Some(after_eq) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        if let Some(key) = leading_string_literal(after_eq) {
            out.push(key);
        }
    }
    out
}

/// Guards the crude string scan above: a setting name, not a sentence, a path,
/// or an expression that happened to sit next to a quote.
fn is_setting_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':' | '.'))
        && s.chars().any(|c| c.is_ascii_alphabetic())
}

fn has_storage_backed_trigger(text: &str) -> bool {
    text.match_indices("Trigger(").any(|(at, _)| {
        let prefix: String = text[..at]
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        let name: String = prefix.chars().rev().collect();
        !STORAGE_FREE_TRIGGERS.contains(&format!("{name}Trigger").as_str())
    })
}

fn java_sources(function_apps_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    collect_java(&function_apps_dir.join("src"), &mut out);
    out
}

fn collect_java(dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_java(&path, out);
        } else if path.extension().is_some_and(|e| e == "java") {
            if let Ok(text) = std::fs::read_to_string(&path) {
                out.push(text);
            }
        }
    }
}

/// Seed `function_apps/local.settings.json` with every key the Java sources
/// need. Existing values are preserved — this fills gaps, it never overwrites
/// a value a developer set. Returns the keys it added.
pub fn ensure_settings(project_dir: &str) -> Result<Vec<String>, String> {
    let dir = resolve_function_apps_dir(project_dir);
    let keys = required_keys(&dir);
    if keys.is_empty() {
        return Ok(Vec::new());
    }

    let path = dir.join("local.settings.json");
    let mut values: BTreeMap<String, String> = BTreeMap::new();
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(root) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(map) = root["Values"].as_object() {
                for (k, v) in map {
                    values.insert(k.clone(), v.as_str().unwrap_or_default().to_string());
                }
            }
        }
    }

    let mut added = Vec::new();
    for key in keys {
        if values.get(&key).is_some_and(|v| !v.is_empty()) {
            continue;
        }
        values.insert(key.clone(), default_for(&key, &dir));
        added.push(key);
    }
    if added.is_empty() {
        return Ok(added);
    }

    let body = serde_json::json!({ "IsEncrypted": false, "Values": values });
    let text = serde_json::to_string_pretty(&body).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(&path, format!("{text}\n")).map_err(|e| e.to_string())?;
    Ok(added)
}

fn default_for(key: &str, function_apps_dir: &Path) -> String {
    match key {
        "FUNCTIONS_WORKER_RUNTIME" => "java".to_string(),
        "AzureWebJobsStorage" => "UseDevelopmentStorage=true".to_string(),
        other => {
            let smart = smart_default(other);
            if smart.is_empty() {
                // Plain config values (a database name, a container) have no
                // emulator default but are often already checked in.
                return checked_in_value(other, function_apps_dir).unwrap_or_default();
            }
            smart
        }
    }
}

/// A non-secret value for `key` from the project's committed App Configuration
/// export, if one is there. Key Vault references and cloud endpoints are
/// skipped — those must resolve to an emulator, never be copied verbatim.
fn checked_in_value(key: &str, function_apps_dir: &Path) -> Option<String> {
    let project_root = function_apps_dir.parent()?;
    let text =
        std::fs::read_to_string(project_root.join("config/appconfig/appconfig.dev.json")).ok()?;
    let root: serde_json::Value = serde_json::from_str(&text).ok()?;
    let value = root.get(key)?.as_str()?.trim();

    let cloudy = value.contains("@Microsoft.KeyVault")
        || value.contains(".windows.net")
        || value.contains(".azure.com");
    (!value.is_empty() && !cloudy).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(files: &[(&str, &str)]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("function_apps/src/main/java/com/x");
        std::fs::create_dir_all(&src).unwrap();
        for (name, body) in files {
            std::fs::write(src.join(name), body).unwrap();
        }
        tmp
    }

    const COSMOS_FN: &str = r#"
        @FunctionName("SessionStatusDenorm")
        public void run(
            @CosmosDBTrigger(
                name = "sessions",
                databaseName = "%COSMOS_DATABASE%",
                containerName = "Sessions",
                connection = "CosmosDBConnection")
            String input) {
            String sql = System.getenv("SqlConnectionString");
        }
    "#;

    #[test]
    fn discovers_keys_from_getenv_bindings_and_placeholders() {
        let tmp = app(&[("F.java", COSMOS_FN)]);
        let keys = required_keys(&resolve_function_apps_dir(tmp.path().to_str().unwrap()));

        for expected in [
            "CosmosDBConnection",
            "SqlConnectionString",
            "COSMOS_DATABASE",
            "AzureWebJobsStorage",
            "FUNCTIONS_WORKER_RUNTIME",
        ] {
            assert!(keys.contains(&expected.to_string()), "{keys:?}");
        }
    }

    /// The exact failure this module exists to prevent: an HTTP-only app does
    /// not need AzureWebJobsStorage, so demanding it would be noise.
    #[test]
    fn http_only_app_does_not_need_storage() {
        let tmp = app(&[(
            "H.java",
            r#"@FunctionName("Convert")
               public HttpResponseMessage run(@HttpTrigger(name = "req") String r) { return null; }"#,
        )]);
        let keys = required_keys(&resolve_function_apps_dir(tmp.path().to_str().unwrap()));
        assert!(
            !keys.contains(&"AzureWebJobsStorage".to_string()),
            "{keys:?}"
        );
    }

    #[test]
    fn writes_emulator_defaults_and_keeps_existing_values() {
        let tmp = app(&[("F.java", COSMOS_FN)]);
        let dir = tmp.path().join("function_apps");
        std::fs::write(
            dir.join("local.settings.json"),
            r#"{"IsEncrypted":false,"Values":{"SqlConnectionString":"KEEP-ME"}}"#,
        )
        .unwrap();

        let added = ensure_settings(tmp.path().to_str().unwrap()).unwrap();
        assert!(
            added.contains(&"AzureWebJobsStorage".to_string()),
            "{added:?}"
        );
        assert!(
            !added.contains(&"SqlConnectionString".to_string()),
            "{added:?}"
        );

        let text = std::fs::read_to_string(dir.join("local.settings.json")).unwrap();
        let root: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(root["Values"]["SqlConnectionString"], "KEEP-ME");
        assert_eq!(
            root["Values"]["AzureWebJobsStorage"],
            "UseDevelopmentStorage=true"
        );
        assert_eq!(root["Values"]["FUNCTIONS_WORKER_RUNTIME"], "java");
    }

    #[test]
    fn no_java_sources_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(ensure_settings(tmp.path().to_str().unwrap())
            .unwrap()
            .is_empty());
    }
}
