//! Resolves URL templates from workflows to concrete URLs.
//!
//! Logic Apps URLs typically look like `@{parameters('Jde_Url')}/api/bsfn/...`.
//! The chain is:
//!   workflow URL template
//!     → `parameters.json` (`"Jde_Url": { "value": "@appsetting('Jde_Url')" }`)
//!     → `local.settings.json` (`"Jde_Url": "https://..."`)
//!
//! Resolver walks that chain once at construction, then `resolve()` does string
//! interpolation per URL discovered by the scanner.

use std::collections::BTreeMap;
use std::path::Path;

use crate::services::mock::contract::{AppSetting, SettingKind};
use crate::services::mock::scanner::ScanError;

pub struct Resolver {
    /// Final resolved values, keyed by app setting name.
    pub settings:          BTreeMap<String, AppSetting>,
    /// Map of logic-app parameter name → the app setting it ultimately resolves to.
    pub param_to_setting:  BTreeMap<String, String>,
}

impl Resolver {
    pub fn load(workspace: &Path) -> Result<Self, ScanError> {
        let local = read_json(&workspace.join("local.settings.json"))
            .map_err(|_| ScanError::NoSettings)?;

        // parameters.json is optional — a workspace without it is still valid
        let params = read_json(&workspace.join("parameters.json")).ok();

        let raw_settings = local.get("Values")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();

        let mut settings = BTreeMap::new();
        for (name, value) in raw_settings {
            let raw = value.as_str().unwrap_or("").to_string();
            settings.insert(name.clone(), AppSetting {
                kind:           classify(&name, &raw),
                resolved_value: Some(raw.clone()),
                raw_value:      raw,
                references:     vec![],
            });
        }

        let mut param_to_setting = BTreeMap::new();
        if let Some(params) = params {
            if let Some(obj) = params.as_object() {
                for (param_name, param) in obj {
                    if let Some(s) = param.get("value").and_then(|v| v.as_str()) {
                        if let Some(setting) = extract_appsetting_name(s) {
                            param_to_setting.insert(param_name.clone(), setting);
                        }
                    }
                }
            }
        }

        Ok(Self { settings, param_to_setting })
    }

    /// Substitute `@{parameters('X')}` and `@{appsetting('X')}` interpolations
    /// with their concrete values. Returns `None` if any placeholder is unresolvable.
    pub fn resolve(&self, template: &str) -> Option<String> {
        let mut out = template.to_string();

        // @{parameters('X')}
        let re_param = regex::Regex::new(r"@\{parameters\('([^']+)'\)\}").ok()?;
        for caps in re_param.captures_iter(template).collect::<Vec<_>>() {
            let param        = &caps[1];
            let setting_name = self.param_to_setting.get(param)?;
            let value        = self.settings.get(setting_name)
                                  .and_then(|s| s.resolved_value.as_ref())?;
            out = out.replace(&caps[0], value);
        }

        // @{appsetting('X')}
        let re_app = regex::Regex::new(r"@\{appsetting\('([^']+)'\)\}").ok()?;
        for caps in re_app.captures_iter(&out.clone()).collect::<Vec<_>>() {
            let setting_name = &caps[1];
            let value        = self.settings.get(setting_name)
                                  .and_then(|s| s.resolved_value.as_ref())?;
            out = out.replace(&caps[0], value);
        }

        if out.contains("@{") { None } else { Some(out) }
    }

    /// Mark a workflow as referencing a given app setting, for the final report.
    pub fn record_reference(&mut self, setting: &str, workflow: &str) {
        if let Some(s) = self.settings.get_mut(setting) {
            if !s.references.iter().any(|r| r == workflow) {
                s.references.push(workflow.to_string());
            }
        }
    }

    pub fn into_settings(self) -> BTreeMap<String, AppSetting> { self.settings }
}

/// Heuristic: classify an app setting value so the UI can group them.
fn classify(name: &str, value: &str) -> SettingKind {
    let lname = name.to_lowercase();
    if lname.contains("secret") || lname.contains("password") || lname.contains("key")
        && !lname.contains("uri") && !lname.contains("url")
    {
        return SettingKind::Secret;
    }
    if value.starts_with("http://") || value.starts_with("https://") {
        return SettingKind::Url;
    }
    if value.contains("Endpoint=sb://") || value.contains("AccountName=") || value.contains("AccountKey=") {
        return SettingKind::ConnectionString;
    }
    SettingKind::Other
}

fn extract_appsetting_name(value: &str) -> Option<String> {
    // matches both "@appsetting('Foo')" and "@{appsetting('Foo')}"
    let re = regex::Regex::new(r"@\{?appsetting\('([^']+)'\)\}?").ok()?;
    re.captures(value).map(|c| c[1].to_string())
}

fn read_json(p: &Path) -> std::io::Result<serde_json::Value> {
    let s = std::fs::read_to_string(p)?;
    serde_json::from_str(&s).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
