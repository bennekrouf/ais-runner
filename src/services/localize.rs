//! Startup connection localization — the local-only guarantee.
//!
//! ais-runner runs workflows only against local emulators/mock. A workflow
//! whose connection still uses Managed Identity (MSI) or points at a real Azure
//! endpoint will fail at runtime, and the user can burn a lot of time debugging
//! the *workflow* before realising the *connection* is the problem. So on
//! startup we scan every connection, adapt what we safely can to local, and
//! report anything that still isn't local.
//!
//! Two axes:
//!   * `connections.json` — MSI connections (`parameterSetName:
//!     ManagedServiceIdentity`) are switched to connection-string auth against
//!     the matching emulator (blob→Azurite, ServiceBus→emulator, SQL/Cosmos→
//!     their emulators). Providers with no local equivalent are reported.
//!   * `local.settings.json` — setting values that point at the cloud
//!     (`*.database.windows.net`, a non-local https endpoint, …) are rewritten
//!     to their local target, and the connection-string keys the patch now
//!     references are stubbed with local defaults.
//!
//! This runs on the loading screen, on every project open — so it is
//! **read-only with respect to `connections.json`**. That file is committed
//! and cloud-facing; patching it here would leave a dirty working tree the
//! moment a project is opened, even if the user never starts anything. The
//! MSI analysis is done on an in-memory patch, and `func start` applies the
//! real one (bracketed by `connections_snapshot` save/restore) when the
//! runtime actually needs it.
//!
//! `local.settings.json` *is* written here — it is gitignored, so stubbing
//! local defaults into it costs the user nothing.

use std::collections::HashMap;

use crate::services::{run_readiness, setup_manager, workflows};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalizeReport {
    /// Connections switched from MSI to a local connection string.
    pub msi_localized: Vec<String>,
    /// MSI connections whose provider has no local equivalent — need attention.
    pub msi_unresolved: Vec<String>,
    /// local.settings.json keys whose cloud value was rewritten to a local one.
    pub settings_localized: Vec<String>,
    /// Connection-string keys stubbed with a local default because they were empty.
    pub keys_stubbed: Vec<String>,
    /// Non-fatal problems.
    pub errors: Vec<String>,
}

impl LocalizeReport {
    /// True when everything is already local — nothing changed, nothing pending.
    pub fn all_local(&self) -> bool {
        self.msi_localized.is_empty()
            && self.msi_unresolved.is_empty()
            && self.settings_localized.is_empty()
            && self.keys_stubbed.is_empty()
    }
}

/// The set of MSI connection names in a parsed connections.json, keyed by name
/// → provider id.
fn msi_connections(conn: &serde_json::Value) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(map) = conn["serviceProviderConnections"].as_object() {
        for (name, c) in map {
            if c["parameterSetName"].as_str() == Some("ManagedServiceIdentity") {
                out.insert(
                    name.clone(),
                    c["serviceProvider"]["id"]
                        .as_str()
                        .unwrap_or("")
                        .to_string(),
                );
            }
        }
    }
    out
}

/// Every `@appsetting('key')` referenced anywhere under connections.json.
fn referenced_keys(conn: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    collect(conn, &mut out);
    out.sort();
    out.dedup();
    return out;

    fn collect(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::String(s) => {
                if let Some(k) = s
                    .strip_prefix("@appsetting('")
                    .and_then(|s| s.strip_suffix("')"))
                {
                    out.push(k.to_string());
                }
            }
            serde_json::Value::Object(m) => m.values().for_each(|x| collect(x, out)),
            serde_json::Value::Array(a) => a.iter().for_each(|x| collect(x, out)),
            _ => {}
        }
    }
}

/// Localize every connection for `logic_apps_dir`. Idempotent — running it when
/// everything is already local changes nothing and returns an empty report.
pub fn localize(logic_apps_dir: &str) -> LocalizeReport {
    let dir = workflows::resolve_logic_apps_dir(logic_apps_dir);
    let conn_path = dir.join("connections.json");
    let mut report = LocalizeReport::default();

    // ── connections.json: MSI → local connection-string auth ─────────────
    if let Ok(raw) = std::fs::read_to_string(&conn_path) {
        let before: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
        let msi_before = msi_connections(&before);

        // Computed in memory only — never written here. This runs on the
        // loading screen, whose job is to report, not to mutate: writing would
        // dirty a committed, cloud-facing file the moment a project is opened,
        // even if the user never starts func. `func start` applies the same
        // patch itself (with snapshot/restore around it), so the file on disk
        // is correct by the time the runtime actually reads it.
        let patched =
            setup_manager::patch_connections_for_local(&setup_manager::fix_connections_json(&raw));

        let after: serde_json::Value = serde_json::from_str(&patched).unwrap_or_default();
        let msi_after = msi_connections(&after);
        for name in msi_before.keys() {
            if msi_after.contains_key(name) {
                report.msi_unresolved.push(name.clone()); // provider we can't localize
            } else {
                report.msi_localized.push(name.clone());
            }
        }
        report.msi_localized.sort();
        report.msi_unresolved.sort();

        // Stub any connection-string key the patched file now references but
        // that is empty/absent in local.settings.json, using local defaults.
        let settings_dir = logic_apps_dir.to_string();
        // `*_databaseName` has no standalone default — its value comes from the
        // scenarios — so it would never pass a smart_default check.
        let local_db =
            crate::services::scenario::local_database_name(std::path::Path::new(&settings_dir));
        let empty_keys: Vec<String> = referenced_keys(&after)
            .into_iter()
            .filter(|k| {
                !setup_manager::smart_default(k).is_empty()
                    || (local_db.is_some() && k.ends_with("_databaseName"))
            })
            .filter(|k| setting_is_empty(&dir, k))
            .collect();
        if !empty_keys.is_empty() {
            if let Err(e) = setup_manager::stub_missing_keys(&settings_dir, &empty_keys) {
                report.errors.push(format!("stub settings: {e}"));
            } else {
                report.keys_stubbed = empty_keys;
            }
        }
    }

    // ── local.settings.json: rewrite cloud endpoint values → local ───────
    let settings_path = dir.join("local.settings.json");
    if let Ok(text) = std::fs::read_to_string(&settings_path) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
            let mut updates: HashMap<String, String> = HashMap::new();
            if let Some(values) = json["Values"].as_object() {
                for (k, v) in values {
                    // Managed-API connector URLs are routing metadata the runtime
                    // parses (api/connection name), not endpoints to redirect. A
                    // well-formed value here (the user's real APIM URL or the
                    // smart_default placeholder) must be left alone — clobbering it
                    // with the mock URL breaks connector validation.
                    if k.ends_with("_connectionUrl") {
                        continue;
                    }
                    if let Some(s) = v.as_str() {
                        if run_readiness::is_cloud_value(s) {
                            updates.insert(k.clone(), run_readiness::local_target_for(k, s, &json));
                        }
                    }
                }
            }
            if !updates.is_empty() {
                report.settings_localized = updates.keys().cloned().collect();
                report.settings_localized.sort();
                if let Err(e) = setup_manager::apply_settings(logic_apps_dir, updates) {
                    report.errors.push(format!("rewrite settings: {e}"));
                }
            }
        }
    }

    report
}

/// True when `key` is missing or empty in local.settings.json `Values`.
fn setting_is_empty(dir: &std::path::Path, key: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(dir.join("local.settings.json")) else {
        return true;
    };
    let json: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    json["Values"][key]
        .as_str()
        .map(|s| s.is_empty())
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn msi_connections_detects_only_msi() {
        let conn = json!({
            "serviceProviderConnections": {
                "blob": { "parameterSetName": "ManagedServiceIdentity",
                          "serviceProvider": { "id": "/serviceProviders/AzureBlob" } },
                "sql":  { "parameterSetName": "connectionString",
                          "serviceProvider": { "id": "/serviceProviders/sql" } }
            }
        });
        let msi = msi_connections(&conn);
        assert_eq!(msi.len(), 1);
        assert!(msi.contains_key("blob"));
    }

    #[test]
    fn referenced_keys_are_collected_and_deduped() {
        let conn = json!({
            "serviceProviderConnections": {
                "a": { "parameterValues": { "connectionString": "@appsetting('K1')" } },
                "b": { "parameterValues": { "connectionString": "@appsetting('K1')",
                                            "endpoint": "@appsetting('K2')" } }
            }
        });
        assert_eq!(
            referenced_keys(&conn),
            vec!["K1".to_string(), "K2".to_string()]
        );
    }

    #[test]
    fn report_flags_unresolved_and_all_local() {
        let clean = LocalizeReport::default();
        assert!(clean.all_local());

        let mut r = LocalizeReport::default();
        r.msi_unresolved.push("keyvault".into());
        assert!(!r.all_local());
    }
}

#[cfg(test)]
mod localize_e2e {
    use super::*;
    use serde_json::json;

    #[test]
    fn localizes_msi_and_cloud_settings_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let base = dir.to_str().unwrap();

        // connections.json: MSI blob (mappable), MSI sql (mappable), MSI keyvault (NOT mappable)
        std::fs::write(dir.join("connections.json"), serde_json::to_string_pretty(&json!({
            "serviceProviderConnections": {
                "IgniteBlob": { "displayName": "blob", "parameterSetName": "ManagedServiceIdentity",
                    "parameterValues": { "blobStorageEndpoint": "@appsetting('IgniteBlob_blobStorageEndpoint')" },
                    "serviceProvider": { "id": "/serviceProviders/AzureBlob" } },
                "ais-sql":    { "displayName": "sql",  "parameterSetName": "ManagedServiceIdentity",
                    "parameterValues": { "serverName": "@appsetting('ais-sql_serverName')" },
                    "serviceProvider": { "id": "/serviceProviders/sql" } },
                "vault":      { "displayName": "kv",   "parameterSetName": "ManagedServiceIdentity",
                    "parameterValues": {}, "serviceProvider": { "id": "/serviceProviders/keyVault" } }
            }
        })).unwrap()).unwrap();

        // local.settings.json: one value pointing at a real cloud SQL endpoint.
        std::fs::write(
            dir.join("local.settings.json"),
            serde_json::to_string_pretty(&json!({
                "IsEncrypted": false,
                "Values": {
                    "AzureWebJobsStorage": "UseDevelopmentStorage=true",
                    "SomeDb_cs": "Server=tcp:corp.database.windows.net,1433;Database=x;"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let conn_before = std::fs::read_to_string(dir.join("connections.json")).unwrap();

        let r = localize(base);

        // MSI blob + sql localized; keyvault can't be and is flagged.
        assert!(r.msi_localized.contains(&"IgniteBlob".to_string()));
        assert!(r.msi_localized.contains(&"ais-sql".to_string()));
        assert_eq!(r.msi_unresolved, vec!["vault".to_string()]);

        // The cloud SQL setting was rewritten to the local emulator.
        assert!(r.settings_localized.contains(&"SomeDb_cs".to_string()));
        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("local.settings.json")).unwrap(),
        )
        .unwrap();
        let cs = settings["Values"]["SomeDb_cs"].as_str().unwrap();
        assert!(
            cs.contains("localhost,1433"),
            "cloud SQL should be redirected local, got: {cs}"
        );

        // connections.json on disk is UNTOUCHED. It is committed and
        // cloud-facing; opening a project must never dirty it. The MSI
        // analysis above came from an in-memory patch, and `func start`
        // applies the real one under snapshot/restore.
        let on_disk = std::fs::read_to_string(dir.join("connections.json")).unwrap();
        assert_eq!(
            on_disk, conn_before,
            "localize() must not write connections.json"
        );
        let conn: serde_json::Value = serde_json::from_str(&on_disk).unwrap();
        assert_eq!(
            conn["serviceProviderConnections"]["IgniteBlob"]["parameterSetName"],
            "ManagedServiceIdentity"
        );
        assert_eq!(
            conn["serviceProviderConnections"]["ais-sql"]["parameterSetName"],
            "ManagedServiceIdentity"
        );
        assert_eq!(
            conn["serviceProviderConnections"]["vault"]["parameterSetName"],
            "ManagedServiceIdentity"
        );

        // Pure analysis: a second pass reports the same thing rather than
        // going quiet, because nothing was mutated to make it quiet.
        let r2 = localize(base);
        assert_eq!(r2.msi_localized, r.msi_localized);
        assert_eq!(r2.msi_unresolved, r.msi_unresolved);
        // local.settings.json *was* written, so its cloud value is now local
        // and there is nothing left to redirect.
        assert!(r2.settings_localized.is_empty());
    }

    #[test]
    fn connector_urls_are_never_clobbered() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let base = dir.to_str().unwrap();

        std::fs::write(
            dir.join("connections.json"),
            serde_json::to_string_pretty(&json!({ "serviceProviderConnections": {} })).unwrap(),
        )
        .unwrap();

        // Real, well-formed managed-API connector URLs + one genuinely cloud value.
        let teams = "https://acme-prod.azure-apim.net/apim/teams/teams-1a2b/";
        let logan  = "https://logic-apis-northeurope.azure-apim.net/apim/azureloganalyticsdatacollector/conn/";
        std::fs::write(
            dir.join("local.settings.json"),
            serde_json::to_string_pretty(&json!({
                "IsEncrypted": false,
                "Values": {
                    "Teams_connectionUrl": teams,
                    "LogAnalytics_connectionUrl": logan,
                    "SomeDb_cs": "Server=tcp:corp.database.windows.net,1433;Database=x;"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let r = localize(base);

        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("local.settings.json")).unwrap(),
        )
        .unwrap();
        // Connector URLs left exactly as they were — not rewritten to the mock URL.
        assert_eq!(settings["Values"]["Teams_connectionUrl"], teams);
        assert_eq!(settings["Values"]["LogAnalytics_connectionUrl"], logan);
        assert!(!r
            .settings_localized
            .contains(&"Teams_connectionUrl".to_string()));
        assert!(!r
            .settings_localized
            .contains(&"LogAnalytics_connectionUrl".to_string()));
        // The genuinely-cloud SQL value was still redirected.
        assert!(r.settings_localized.contains(&"SomeDb_cs".to_string()));
    }
}
