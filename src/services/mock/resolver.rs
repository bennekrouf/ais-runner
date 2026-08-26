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
    pub settings: BTreeMap<String, AppSetting>,
    /// Map of logic-app parameter name → the app setting it ultimately resolves to.
    pub param_to_setting: BTreeMap<String, String>,
}

impl Resolver {
    pub fn load(workspace: &Path) -> Result<Self, ScanError> {
        let local =
            read_json(&workspace.join("local.settings.json")).map_err(|_| ScanError::NoSettings)?;

        // parameters.json is optional — a workspace without it is still valid
        let params = read_json(&workspace.join("parameters.json")).ok();

        let raw_settings = local
            .get("Values")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();

        let mut settings = BTreeMap::new();
        for (name, value) in raw_settings {
            let raw = value.as_str().unwrap_or("").to_string();
            settings.insert(
                name.clone(),
                AppSetting {
                    kind: classify(&name, &raw),
                    resolved_value: Some(raw.clone()),
                    raw_value: raw,
                    references: vec![],
                },
            );
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

        Ok(Self {
            settings,
            param_to_setting,
        })
    }

    /// Substitute `@{parameters('X')}` and `@{appsetting('X')}` interpolations
    /// with their concrete values. Returns `None` if any placeholder is unresolvable.
    pub fn resolve(&self, template: &str) -> Option<String> {
        let mut out = template.to_string();

        // @{parameters('X')}
        let re_param = regex::Regex::new(r"@\{parameters\('([^']+)'\)\}").ok()?;
        for caps in re_param.captures_iter(template).collect::<Vec<_>>() {
            let param = &caps[1];
            let setting_name = self.param_to_setting.get(param)?;
            let value = self
                .settings
                .get(setting_name)
                .and_then(|s| s.resolved_value.as_ref())?;
            out = out.replace(&caps[0], value);
        }

        // @{appsetting('X')}
        let re_app = regex::Regex::new(r"@\{appsetting\('([^']+)'\)\}").ok()?;
        for caps in re_app.captures_iter(&out.clone()).collect::<Vec<_>>() {
            let setting_name = &caps[1];
            let value = self
                .settings
                .get(setting_name)
                .and_then(|s| s.resolved_value.as_ref())?;
            out = out.replace(&caps[0], value);
        }

        if out.contains("@{") {
            None
        } else {
            Some(out)
        }
    }

    /// Mark a workflow as referencing a given app setting, for the final report.
    pub fn record_reference(&mut self, setting: &str, workflow: &str) {
        if let Some(s) = self.settings.get_mut(setting) {
            if !s.references.iter().any(|r| r == workflow) {
                s.references.push(workflow.to_string());
            }
        }
    }

    pub fn into_settings(self) -> BTreeMap<String, AppSetting> {
        self.settings
    }
}

/// True when a URL points back at this machine, in any of the spellings a
/// settings file uses in practice.
fn is_local_url(value: &str) -> bool {
    let v = value.to_lowercase();
    let Some(rest) = v
        .strip_prefix("http://")
        .or_else(|| v.strip_prefix("https://"))
    else {
        return false;
    };
    // Host is everything before the first `/`, `:` or `?`.
    let host = rest.split(['/', ':', '?']).next().unwrap_or_default();
    matches!(
        host,
        "localhost" | "127.0.0.1" | "0.0.0.0" | "[::1]" | "::1"
    ) || host.ends_with(".localhost")
}

/// Heuristic: classify an app setting value so the UI can group them.
///
/// `SettingKind::Url` is what `rewrite` redirects at the mock server, so
/// anything that must not be redirected has to be classified as something else
/// here rather than filtered downstream.
fn classify(name: &str, value: &str) -> SettingKind {
    let lname = name.to_lowercase();
    if lname.contains("secret")
        || lname.contains("password")
        || lname.contains("key") && !lname.contains("uri") && !lname.contains("url")
    {
        return SettingKind::Secret;
    }
    // Managed-API connector URLs (`*_connectionUrl`) look like plain URLs but
    // are routing metadata: the Logic Apps runtime parses the api name and
    // connection name out of the path. Point one at the mock server and the
    // host rejects the workflow outright — "The API connection reference name
    // 'x' has invalid connection runtime url … api name and connection name
    // should not be null or empty" — which cascades to every workflow that
    // calls it as a child, and takes down host startup. `localize::localize`
    // skips these for the same reason.
    if lname.ends_with("_connectionurl") {
        return SettingKind::Other;
    }
    // A URL that already resolves locally is not an outbound dependency, so
    // there is nothing for the mock to stand in for — the service is right
    // here. Redirecting one swaps a working local endpoint for a stub that
    // does not implement it, and the workflow gets a 404 from a path the
    // contract never saw. `AIS_Functions_BaseUrl` pointing at the project's
    // own function host on :7072 is the case that bites; the emulator
    // endpoints (Azurite, Cosmos) are the same shape.
    //
    // Deliberately checked before the generic http(s) arm and independently of
    // the setting's name, so it holds for any naming convention.
    if is_local_url(value) {
        return SettingKind::Other;
    }
    if value.starts_with("http://") || value.starts_with("https://") {
        return SettingKind::Url;
    }
    if value.contains("Endpoint=sb://")
        || value.contains("AccountName=")
        || value.contains("AccountKey=")
    {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_api_connector_urls_are_never_classified_as_redirectable() {
        // Regression: the mock runtime rewrote these to its own base URL, and
        // the Logic Apps host then refused every workflow using the connector
        // ("invalid connection runtime url … api name and connection name
        // should not be null or empty"). That cascaded through every parent
        // workflow and aborted host startup entirely.
        for name in [
            "Teams_connectionUrl",
            "LogAnalytics_connectionUrl",
            "Sharepoint_connectionUrl",
            "Office365_connectionUrl",
        ] {
            assert_eq!(
                classify(
                    name,
                    "https://switzerlandnorth.azure-apim.net/apim/teams/teams-local/"
                ),
                SettingKind::Other,
                "{name} must not be redirected to the mock server"
            );
        }
    }

    #[test]
    fn ordinary_http_settings_are_still_redirectable() {
        // The exclusion above must stay narrow — a normal outbound API base URL
        // is exactly what the mock exists to intercept.
        assert_eq!(
            classify("JdeUrl", "https://jde.example.com"),
            SettingKind::Url
        );
        assert_eq!(
            classify(
                "AIS_Functions_BaseUrl",
                "https://func-tom-dev.azurewebsites.net"
            ),
            SettingKind::Url,
            "a cloud function host has no local equivalent — mock it"
        );
    }

    #[test]
    fn already_local_urls_are_never_redirected_to_the_mock() {
        // Regression: AIS_Functions_BaseUrl pointed at the project's own
        // function host on :7072, the mock redirected it, and every call to
        // /api/ConvertXlsxToTxt came back 404 from a path the contract never
        // saw — surfacing as an unrelated workflow failure two levels up.
        for v in [
            "http://localhost:7072",
            "http://127.0.0.1:10000/devstoreaccount1",
            "https://localhost:8081/",
            "http://0.0.0.0:9000/api",
            "http://app.localhost:3000",
        ] {
            assert_eq!(
                classify("AIS_Functions_BaseUrl", v),
                SettingKind::Other,
                "{v} resolves locally — nothing for the mock to stand in for"
            );
        }
    }

    #[test]
    fn remote_urls_are_still_redirected() {
        // The exclusion must not swallow genuine outbound dependencies, and
        // must not be fooled by a hostname that merely contains "localhost".
        for v in [
            "https://jde.example.com",
            "https://api.partner.io/v1",
            "https://localhost.evil.example.com/api",
        ] {
            assert_eq!(
                classify("JdeUrl", v),
                SettingKind::Url,
                "{v} should be mockable"
            );
        }
    }
}
