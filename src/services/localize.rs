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
//!     (`*.database.windows.net`, a non-local https endpoint, …) are reported;
//!     `func start` rewrites them to their local target, and fills empty keys
//!     with local defaults, when the runtime actually needs them.
//!
//! This runs on the loading screen, on every project open — so it is
//! **read-only**. `connections.json` is committed and cloud-facing; patching
//! it here would leave a dirty working tree the moment a project is opened,
//! even if the user never starts anything. The MSI analysis is done on an
//! in-memory patch, and `func start` applies the real one (bracketed by
//! `connections_snapshot` save/restore) when the runtime actually needs it.
//! `local.settings.json` is left alone too, for the same reason: opening a
//! project should not change its files.

use std::collections::HashMap;

use crate::services::{run_readiness, setup_manager, workflows};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalizeReport {
    /// Connections switched from MSI to a local connection string.
    pub msi_localized: Vec<String>,
    /// MSI connections whose provider has no local equivalent — need attention.
    pub msi_unresolved: Vec<String>,
    /// local.settings.json keys whose value points at the cloud. `func start`
    /// rewrites them to a local target; this pass only reports them.
    pub settings_to_redirect: Vec<String>,
}

impl LocalizeReport {
    /// True when everything is already local — nothing changed, nothing pending.
    pub fn all_local(&self) -> bool {
        self.msi_localized.is_empty()
            && self.msi_unresolved.is_empty()
            && self.settings_to_redirect.is_empty()
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

/// Settings whose value points at the cloud, mapped to the local value they
/// should hold instead.
fn cloud_redirects(logic_apps_dir: &str) -> HashMap<String, String> {
    let mut updates = HashMap::new();
    let Ok(text) = crate::services::settings_file::read_local_settings(logic_apps_dir) else {
        return updates;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return updates;
    };
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
    updates
}

/// Rewrite every cloud-pointing setting in `local.settings.json` to its local
/// target, returning the keys changed. Called by `func start`, the moment the
/// runtime is about to read the file — never on project open.
pub fn redirect_cloud_settings(logic_apps_dir: &str) -> Result<Vec<String>, String> {
    let updates = cloud_redirects(logic_apps_dir);
    let mut keys: Vec<String> = updates.keys().cloned().collect();
    keys.sort();
    if !updates.is_empty() {
        setup_manager::apply_settings(logic_apps_dir, updates)?;
    }
    Ok(keys)
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
    }

    // ── local.settings.json: cloud endpoint values → local ───────────────
    // Reported only; `func start` does the rewrite (see `redirect_cloud_settings`).
    report.settings_to_redirect = cloud_redirects(logic_apps_dir).into_keys().collect();
    report.settings_to_redirect.sort();

    report
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
                "AcmeBlob": { "displayName": "blob", "parameterSetName": "ManagedServiceIdentity",
                    "parameterValues": { "blobStorageEndpoint": "@appsetting('AcmeBlob_blobStorageEndpoint')" },
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
        assert!(r.msi_localized.contains(&"AcmeBlob".to_string()));
        assert!(r.msi_localized.contains(&"ais-sql".to_string()));
        assert_eq!(r.msi_unresolved, vec!["vault".to_string()]);

        // The cloud SQL setting is reported, not yet rewritten.
        assert_eq!(r.settings_to_redirect, vec!["SomeDb_cs".to_string()]);
        let settings_before = std::fs::read_to_string(dir.join("local.settings.json")).unwrap();
        assert!(settings_before.contains("corp.database.windows.net"));

        // func start does the rewrite, to the local emulator.
        assert_eq!(
            redirect_cloud_settings(base).unwrap(),
            vec!["SomeDb_cs".to_string()]
        );
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
            conn["serviceProviderConnections"]["AcmeBlob"]["parameterSetName"],
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
        // The rewrite above left nothing to redirect.
        assert!(r2.settings_to_redirect.is_empty());
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

        let redirected = redirect_cloud_settings(base).unwrap();

        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("local.settings.json")).unwrap(),
        )
        .unwrap();
        // Connector URLs left exactly as they were — not rewritten to the mock URL.
        assert_eq!(settings["Values"]["Teams_connectionUrl"], teams);
        assert_eq!(settings["Values"]["LogAnalytics_connectionUrl"], logan);
        // Only the genuinely-cloud SQL value was redirected.
        assert_eq!(redirected, vec!["SomeDb_cs".to_string()]);
    }
}
