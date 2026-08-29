//! Azure **App Configuration** support (the standalone
//! `Microsoft.AppConfiguration/configurationStores` resource — NOT the
//! per-site Application Settings on the Function App).
//!
//! This module deliberately uses the official Azure name "App Configuration"
//! throughout. App Settings and App Configuration are different products with
//! different storage, scoping rules, and CLIs; mixing them under a generic
//! "environment variables" umbrella loses the semantics customers rely on
//! (labels for per-env scoping, feature flags, etc.).
//!
//! Two flavours of references appear in Logic Apps Standard projects:
//!
//! 1. **Indirect via Application Settings** — the most common case:
//!    ```text
//!    "MySetting": "@Microsoft.AppConfiguration(Endpoint=https://mystore.azconfig.io;Key=Section:Name;Label=dev)"
//!    ```
//!    The Function App resolves the reference at runtime against the named
//!    store; the workflow sees only the resolved value via `@appsetting('MySetting')`.
//!
//! 2. **Direct endpoint** — a project may carry a bare endpoint setting
//!    (e.g. `AzureAppConfigurationEndpoint`) that the host SDK reads at
//!    startup to bulk-import keys. We surface that as a detected store too.

use crate::cli::{az_command, AzError};
use indexmap::IndexMap;

/// A key→value map keyed by App Configuration key — same shape as
/// `env_compare::EnvValues` so the existing diff renderer can reuse it.
pub type AppConfigValues = IndexMap<String, String>;

/// A reference to an App Configuration store discovered in a project's
/// `local.settings.json` or in an Azure Function App's app-settings list.
/// We track the originating setting key so the UI can show *why* a store
/// is in play ("referenced by `MySetting`").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfigRef {
    /// `https://<name>.azconfig.io` — what the `az appconfig kv list` command needs.
    pub endpoint: String,
    /// Short store name (`<name>` from the endpoint), for UI display.
    pub store_name: String,
    /// Specific key referenced, if the reference is a `@Microsoft.AppConfiguration(...)`
    /// pointer to one key (vs. a bulk-import endpoint).
    pub key: Option<String>,
    /// Label (`dev`/`qa`/`prod`/…) hard-coded into the reference, if any.
    pub label: Option<String>,
    /// The Application-Settings key that triggered the detection — e.g.
    /// `"MySetting"` or `"AzureAppConfigurationEndpoint"`. Lets the UI tell
    /// the user *which* setting brings the store into scope.
    pub setting_key: String,
}

/// Walk a map of `setting_key → value` (e.g. local.settings.json's Values
/// section, or the output of `az webapp config appsettings list`) and
/// return every App Configuration store referenced. Deduped by endpoint
/// but distinct entries when a store appears via multiple settings or with
/// different keys/labels.
pub fn detect_stores(settings: &IndexMap<String, String>) -> Vec<AppConfigRef> {
    let mut out: Vec<AppConfigRef> = Vec::new();

    for (k, v) in settings {
        // ── Pattern 1: @Microsoft.AppConfiguration(Endpoint=…;Key=…;Label=…) ──
        if let Some(r) = parse_microsoft_appconfig_ref(v, k) {
            push_unique(&mut out, r);
            continue;
        }
        // ── Pattern 2: bare endpoint settings the host SDK reads at startup ──
        // The canonical key names vary by SDK version. We match by suffix so we
        // don't have to enumerate every spelling (`Azure_AppConfiguration_Endpoint`,
        // `AzureAppConfigurationEndpoint`, `AppConfig:Endpoint`, etc).
        let key_lower = k.to_ascii_lowercase();
        if (key_lower.contains("appconfig") || key_lower.contains("app_configuration"))
            && key_lower.ends_with("endpoint")
            && v.starts_with("https://")
            && v.contains(".azconfig.io")
        {
            let store_name = extract_store_name(v).unwrap_or_else(|| v.clone());
            push_unique(
                &mut out,
                AppConfigRef {
                    endpoint: v.trim_end_matches('/').to_string(),
                    store_name,
                    key: None,
                    label: None,
                    setting_key: k.clone(),
                },
            );
        }
    }

    out
}

/// Parse `@Microsoft.AppConfiguration(Endpoint=…;Key=…;Label=…)` into a ref.
/// Spec-tolerant: handles missing/reordered fields, surrounding whitespace,
/// and optional braces some templates use (`@Microsoft.AppConfiguration(…)` or
/// `@{Microsoft.AppConfiguration(…)}`).
fn parse_microsoft_appconfig_ref(raw: &str, setting_key: &str) -> Option<AppConfigRef> {
    let s = raw
        .trim()
        .trim_start_matches("@{")
        .trim_start_matches('@')
        .trim_end_matches('}');
    let s = s.strip_prefix("Microsoft.AppConfiguration(")?;
    let s = s.strip_suffix(')')?;

    let mut endpoint: Option<String> = None;
    let mut key: Option<String> = None;
    let mut label: Option<String> = None;

    for part in s.split(';') {
        let mut it = part.splitn(2, '=');
        let name = it.next()?.trim();
        let val = it.next().unwrap_or("").trim();
        match name {
            "Endpoint" => endpoint = Some(val.trim_end_matches('/').to_string()),
            "Key" => key = Some(val.to_string()),
            "Label" => label = Some(val.to_string()),
            _ => {}
        }
    }
    let endpoint = endpoint?;
    Some(AppConfigRef {
        store_name: extract_store_name(&endpoint).unwrap_or_else(|| endpoint.clone()),
        endpoint,
        key,
        label,
        setting_key: setting_key.to_string(),
    })
}

/// Extract `mystore` from `https://mystore.azconfig.io`. Returns `None` for
/// malformed endpoints — caller falls back to using the full URL as the
/// display name.
fn extract_store_name(endpoint: &str) -> Option<String> {
    let after_scheme = endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"))?;
    let host = after_scheme.split('/').next()?;
    let name = host.strip_suffix(".azconfig.io")?;
    Some(name.to_string())
}

/// Append to `out` unless an equivalent entry already exists. Two refs are
/// equivalent when they target the same endpoint + key + label triple — the
/// `setting_key` doesn't matter for store-level dedup.
fn push_unique(out: &mut Vec<AppConfigRef>, r: AppConfigRef) {
    let dup = out
        .iter()
        .any(|e| e.endpoint == r.endpoint && e.key == r.key && e.label == r.label);
    if !dup {
        out.push(r);
    }
}

/// Fetch every key/value pair from an Azure App Configuration store.
///
/// `label` is the App Configuration **label** filter ("dev" / "qa" / "prod"
/// / …). Most enterprise customers scope by label rather than by store, so
/// per-environment comparison reduces to "same store, three labels". Pass
/// `None` to fetch the no-label entries.
///
/// Blocking — invoke under `tokio::task::spawn_blocking`.
pub fn fetch_kv(endpoint: &str, label: Option<&str>) -> Result<AppConfigValues, AzError> {
    let mut args: Vec<&str> = vec![
        "appconfig",
        "kv",
        "list",
        "--endpoint",
        endpoint,
        "--auth-mode",
        "login",
        "--fields",
        "key",
        "value",
        "label",
        "-o",
        "json",
    ];
    // App Configuration's CLI uses `null` as a sentinel for "no label". The
    // backtick form below would be an empty-string filter (which means
    // something different and won't match).
    let label_arg;
    if let Some(l) = label {
        args.push("--label");
        label_arg = l.to_string();
        args.push(&label_arg);
    }

    let out = az_command(&args)
        .output()
        .map_err(|e| AzError::Other(format!("az not found: {}", e)))?;

    let raw = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    if !out.status.success() {
        // AppConfig surfaces both Azure-AD failures (AADSTS) and per-RBAC
        // failures (403 from the store). Map AAD ones to NotLoggedIn so the
        // existing login-banner UX kicks in; everything else stays Other.
        if stderr.contains("AADSTS")
            || stderr.contains("az login")
            || stderr.contains("refresh token")
        {
            return Err(AzError::NotLoggedIn);
        }
        return Err(AzError::Other(stderr.trim().to_string()));
    }

    let arr: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap_or_default();
    let mut map = AppConfigValues::new();
    for item in &arr {
        if let (Some(k), Some(v)) = (item["key"].as_str(), item["value"].as_str()) {
            map.insert(k.to_string(), v.to_string());
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(pairs: &[(&str, &str)]) -> IndexMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn detects_microsoft_appconfig_reference_with_all_fields() {
        let s = settings(&[
            ("FeatureFlag",
             "@Microsoft.AppConfiguration(Endpoint=https://acme-config.azconfig.io;Key=Features:NewFlow;Label=qa)"),
        ]);
        let refs = detect_stores(&s);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].endpoint, "https://acme-config.azconfig.io");
        assert_eq!(refs[0].store_name, "acme-config");
        assert_eq!(refs[0].key.as_deref(), Some("Features:NewFlow"));
        assert_eq!(refs[0].label.as_deref(), Some("qa"));
        assert_eq!(refs[0].setting_key, "FeatureFlag");
    }

    #[test]
    fn detects_bare_endpoint_setting_keys() {
        // Several spellings of "this is the endpoint the SDK reads at startup"
        // should all be detected via the keyword-+-suffix match.
        let s = settings(&[
            (
                "AzureAppConfigurationEndpoint",
                "https://acme-config.azconfig.io",
            ),
            (
                "Azure_AppConfiguration_Endpoint",
                "https://other-store.azconfig.io",
            ),
            ("AppConfig:Endpoint", "https://third.azconfig.io"),
            // Unrelated setting that mentions "config" but isn't an endpoint:
            ("AppConfigLabel", "dev"),
        ]);
        let refs = detect_stores(&s);
        let names: Vec<&str> = refs.iter().map(|r| r.store_name.as_str()).collect();
        assert!(names.contains(&"acme-config"));
        assert!(names.contains(&"other-store"));
        assert!(names.contains(&"third"));
        assert_eq!(
            refs.len(),
            3,
            "false positive — every detected ref: {refs:#?}"
        );
    }

    #[test]
    fn dedup_by_endpoint_key_label_triple_but_keep_distinct_labels() {
        let s = settings(&[
            (
                "A",
                "@Microsoft.AppConfiguration(Endpoint=https://s.azconfig.io;Key=k;Label=dev)",
            ),
            // Same endpoint+key+label via a different setting — dedup.
            (
                "B",
                "@Microsoft.AppConfiguration(Endpoint=https://s.azconfig.io;Key=k;Label=dev)",
            ),
            // Same endpoint+key but different label — keep.
            (
                "C",
                "@Microsoft.AppConfiguration(Endpoint=https://s.azconfig.io;Key=k;Label=prod)",
            ),
        ]);
        let refs = detect_stores(&s);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].label.as_deref(), Some("dev"));
        assert_eq!(refs[1].label.as_deref(), Some("prod"));
    }

    #[test]
    fn handles_braced_form_and_whitespace() {
        let s = settings(&[
            ("X",
             " @{Microsoft.AppConfiguration(Endpoint=https://acme.azconfig.io ; Key=Section:Name )} "),
        ]);
        let refs = detect_stores(&s);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].store_name, "acme");
        assert_eq!(refs[0].key.as_deref(), Some("Section:Name"));
        assert!(refs[0].label.is_none());
    }

    #[test]
    fn unrelated_settings_yield_nothing() {
        let s = settings(&[
            ("AzureWebJobsStorage", "UseDevelopmentStorage=true"),
            ("RandomKey", "some value"),
            ("EndpointButNotAppConfig", "https://api.example.com"),
        ]);
        assert!(detect_stores(&s).is_empty());
    }

    #[test]
    fn extract_store_name_works_for_canonical_endpoint() {
        assert_eq!(
            extract_store_name("https://my-store.azconfig.io").as_deref(),
            Some("my-store")
        );
        // Trailing slash tolerated.
        assert_eq!(
            extract_store_name("https://my-store.azconfig.io/").as_deref(),
            Some("my-store")
        );
        // Non-azconfig hostname returns None — protects against false positives.
        assert!(extract_store_name("https://my-store.example.com").is_none());
    }
}
