//! Per-run local-readiness gate.
//!
//! ais-runner is strictly **local-only**: workflows run against the bundled
//! emulators (Azurite, Service Bus, SQL, Cosmos) and the local mock server —
//! never the cloud. Before firing a workflow this gate verifies that every
//! connection it uses resolves to a local target, and blocks the run otherwise.
//!
//! Logic Apps Standard consumes connection config at `func start`, so anything
//! changed here only takes effect after the user restarts func — the gate
//! therefore BLOCKS the run and tells the user exactly what to do, rather than
//! wasting a trigger on a run that will fail.
//!
//! Problems fall into three classes:
//!   • `auto_fixable`   — an empty `local.settings.json` key we know a local
//!     default for (emulator endpoints / connection strings). We can fill it.
//!   • `cloud_pointing` — a setting whose CURRENT value points at the cloud
//!     (`*.database.windows.net`, `*.servicebus.windows.net`, a non-local
//!     https endpoint, …). We rewrite it to the matching local emulator, or to
//!     the local mock server when nothing else fits.
//!   • `blocking`       — an empty key with no known local mapping, or a
//!     connection referenced by the workflow but absent from connections.json.
//!     Only the developer can point these at a local target.

use std::collections::{HashMap, HashSet};

use crate::handlers::{cosmos_emulator, sb_emulator, sql_emulator};
use crate::services::{connection_diag, connections_local, setup_manager, workflows};

/// Where otherwise-cloud external URLs are redirected. The mock HTTP server
/// (see `services::mock`) serves these locally so no request ever leaves the
/// machine. Kept on a fixed port so rewritten settings stay stable.
pub const MOCK_BASE_URL: &str = "http://127.0.0.1:7079";

/// The local-readiness verdict for a single workflow.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunReadiness {
    pub workflow: String,
    /// (connection, setting_key, local_default) — empty settings we can fill.
    pub auto_fixable: Vec<(String, String, String)>,
    /// (setting_key, current_cloud_value, local_target) — settings pointing at
    /// the cloud that we will rewrite to a local target.
    pub cloud_pointing: Vec<(String, String, String)>,
    /// (connection, setting_key) — empty settings with no known local mapping.
    pub blocking_settings: Vec<(String, String)>,
    /// Connection names the workflow references but that are absent from
    /// connections.json entirely.
    pub missing_connections: Vec<String>,
}

impl RunReadiness {
    /// True when the workflow can run locally as-is — nothing to fix or block.
    pub fn is_ready(&self) -> bool {
        self.auto_fixable.is_empty()
            && self.cloud_pointing.is_empty()
            && self.blocking_settings.is_empty()
            && self.missing_connections.is_empty()
    }

    /// True when there is something only the developer can resolve.
    pub fn needs_manual(&self) -> bool {
        !self.blocking_settings.is_empty() || !self.missing_connections.is_empty()
    }
}

/// True when `v` looks like a cloud endpoint that must be redirected locally.
pub fn is_cloud_value(v: &str) -> bool {
    let l = v.to_lowercase();
    const MARKERS: &[&str] = &[
        ".database.windows.net",
        ".servicebus.windows.net",
        ".core.windows.net", // blob / queue / table / file
        ".documents.azure.com",
        ".vault.azure.net",
        ".azurewebsites.net",
    ];
    if MARKERS.iter().any(|m| l.contains(m)) {
        return true;
    }
    // Managed-API connector URLs (`*.azure-apim.net/apim/<api>/<conn>/`) are NOT
    // redirectable endpoints — the Logic Apps runtime parses the api/connection
    // name out of that URL, and there is no local emulator or mock that speaks
    // the connector protocol. Rewriting them to the mock URL breaks connector
    // validation, so they are never treated as cloud values to redirect.
    if l.contains("azure-apim.net") {
        return false;
    }
    // Any non-local https endpoint that isn't an already-neutralised placeholder.
    l.starts_with("https://")
        && !l.contains("localhost")
        && !l.contains("127.0.0.1")
        && !l.contains("placeholder")
}

/// The database a SQL connection-string key belongs to, read from its sibling
/// `*_databaseName` setting: `sqlServerAIS_connectionString` pairs with
/// `sqlServerAIS_databaseName`.
///
/// Logic Apps projects carry both, and that pair is the only place the local
/// setup can learn which database a workflow's tables and stored procedures
/// actually live in. Without it every SQL connection was pointed at `master`,
/// which opens fine and then fails every lookup — a workflow calling a stored
/// procedure gets "the stored procedure 'X' doesn't exist" even though X was
/// created correctly in the project's own database.
pub fn sibling_database(key: &str, settings: &serde_json::Value) -> Option<String> {
    let prefix = key.strip_suffix("_connectionString")?;
    let name = settings["Values"][format!("{prefix}_databaseName")]
        .as_str()?
        .trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// The local target a cloud value should be rewritten to.
///
/// Aware of which setting is being rewritten so a SQL connection string keeps
/// its database (read from the sibling `*_databaseName` key) rather than being
/// flattened to `master`.
pub fn local_target_for(key: &str, value: &str, settings: &serde_json::Value) -> String {
    local_target_in(value, sibling_database(key, settings).as_deref())
}

fn local_target_in(value: &str, database: Option<&str>) -> String {
    let l = value.to_lowercase();
    if l.contains(".database.windows.net") {
        format!(
            "Server=localhost,{};Database={};User Id=sa;Password={};\
             Encrypt=false;TrustServerCertificate=true;",
            sql_emulator::SQL_PORT,
            // `master` only as a last resort: it always exists, so the
            // connection at least opens when the project names no database.
            database.unwrap_or("master"),
            sql_emulator::SA_PASSWORD,
        )
    } else if l.contains(".documents.azure.com") {
        cosmos_emulator::local_connection_string()
    } else if l.contains(".servicebus.windows.net") {
        sb_emulator::EMULATOR_CONN_STR.to_string()
    } else if l.contains(".core.windows.net") {
        "UseDevelopmentStorage=true".to_string()
    } else {
        // Key Vault, custom partner APIs, anything else → the local mock server.
        MOCK_BASE_URL.to_string()
    }
}

/// Inspect one workflow's connections against the current on-disk config.
pub fn check(logic_apps_dir: &str, workflow: &str) -> RunReadiness {
    let mut auto_fixable = Vec::new();
    let mut blocking_settings = Vec::new();

    // Read once so a SQL default can pick up its sibling `*_databaseName`
    // rather than falling back to `master`.
    let settings: serde_json::Value = crate::services::settings_file::read_local_settings(logic_apps_dir)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(serde_json::Value::Null);

    for (conn, key) in connection_diag::missing_endpoints_for_workflow(logic_apps_dir, workflow) {
        let default = setup_manager::smart_default_for(&key, &settings);
        if default.is_empty() {
            blocking_settings.push((conn, key));
        } else {
            auto_fixable.push((conn, key, default));
        }
    }

    RunReadiness {
        workflow: workflow.to_string(),
        auto_fixable,
        cloud_pointing: cloud_pointing_for(logic_apps_dir, workflow),
        blocking_settings,
        missing_connections: missing_connections_for(logic_apps_dir, workflow),
    }
}

/// Collect every `@appsetting('key')` reference under a JSON value.
fn collect_appsetting_refs(val: &serde_json::Value, out: &mut Vec<String>) {
    match val {
        serde_json::Value::String(s) => {
            if let Some(k) = s.strip_prefix("@appsetting('").and_then(|s| s.strip_suffix("')")) {
                out.push(k.to_string());
            }
        }
        serde_json::Value::Object(m) => m.values().for_each(|v| collect_appsetting_refs(v, out)),
        serde_json::Value::Array(a) => a.iter().for_each(|v| collect_appsetting_refs(v, out)),
        _ => {}
    }
}

/// Settings used by the workflow whose current value points at the cloud,
/// paired with the local target they'll be rewritten to.
fn cloud_pointing_for(logic_apps_dir: &str, workflow: &str) -> Vec<(String, String, String)> {
    let dir = workflows::resolve_logic_apps_dir(logic_apps_dir);
    let wf_text = std::fs::read_to_string(dir.join(workflow).join("workflow.json")).unwrap_or_default();
    if wf_text.is_empty() {
        return Vec::new();
    }
    let conn: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("connections.json")).unwrap_or_default())
            .unwrap_or_default();
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("local.settings.json")).unwrap_or_default())
            .unwrap_or_default();

    // Connection names the workflow references.
    let re = regex::Regex::new(r#""connectionName"\s*:\s*"([^"]+)""#).unwrap();
    let used: HashSet<String> = re.captures_iter(&wf_text).map(|c| c[1].to_string()).collect();

    // All appsetting keys reachable from those connections.
    let mut keys: Vec<String> = Vec::new();
    if let Some(providers) = conn["serviceProviderConnections"].as_object() {
        for (name, provider) in providers {
            if used.contains(name) {
                collect_appsetting_refs(&provider["parameterValues"], &mut keys);
            }
        }
    }

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for key in keys {
        if !seen.insert(key.clone()) {
            continue;
        }
        let val = settings["Values"][&key].as_str().unwrap_or("");
        if is_cloud_value(val) {
            let target = local_target_for(&key, val, &settings);
            out.push((key, val.to_string(), target));
        }
    }
    out.sort();
    out
}

/// Connection names referenced in the workflow JSON that have no matching entry
/// under any connection section of connections.json.
fn missing_connections_for(logic_apps_dir: &str, workflow: &str) -> Vec<String> {
    let dir = workflows::resolve_logic_apps_dir(logic_apps_dir);
    let wf_text = std::fs::read_to_string(dir.join(workflow).join("workflow.json")).unwrap_or_default();
    if wf_text.is_empty() {
        return Vec::new();
    }
    let conn_text = std::fs::read_to_string(dir.join("connections.json")).unwrap_or_default();
    let conn: serde_json::Value = serde_json::from_str(&conn_text).unwrap_or_default();

    let mut known: HashSet<String> = HashSet::new();
    for section in ["serviceProviderConnections", "managedApiConnections", "functionConnections"] {
        if let Some(o) = conn[section].as_object() {
            known.extend(o.keys().cloned());
        }
    }

    let re = regex::Regex::new(r#""connectionName"\s*:\s*"([^"]+)""#).unwrap();
    let mut missing: Vec<String> = re
        .captures_iter(&wf_text)
        .map(|c| c[1].to_string())
        .filter(|name| !known.contains(name))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    missing.sort();
    missing
}

/// What `apply_fixes` actually did, for surfacing to the user.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FixReport {
    /// Setting keys filled with known local defaults.
    pub auto_filled: Vec<String>,
    /// Setting keys rewritten from a cloud endpoint to a local target.
    pub redirected: Vec<String>,
    /// Path to the scaffolded connections.local.json, if it was created.
    pub scaffolded: Option<String>,
    /// Non-fatal problems encountered while writing.
    pub errors: Vec<String>,
}

/// Apply every fix ais-runner can make on its own to keep the run local:
///   1. Fill empty settings that have a known local default.
///   2. Rewrite cloud-pointing settings to their local emulator / mock target.
///   3. Scaffold a gitignored connections.local.json for persistent overrides.
///
/// Blocking settings and missing connections are intentionally NOT stubbed: a
/// placeholder would defeat the empty-check and let a doomed run proceed. They
/// are reported so the caller can tell the user what local target to set.
pub fn apply_fixes(logic_apps_dir: &str, r: &RunReadiness) -> FixReport {
    let mut report = FixReport::default();
    let mut updates: HashMap<String, String> = HashMap::new();

    for (_conn, key, default) in &r.auto_fixable {
        updates.insert(key.clone(), default.clone());
        report.auto_filled.push(key.clone());
    }
    for (key, _cloud, target) in &r.cloud_pointing {
        updates.insert(key.clone(), target.clone());
        report.redirected.push(key.clone());
    }
    report.auto_filled.sort();
    report.redirected.sort();

    if !updates.is_empty() {
        if let Err(e) = setup_manager::apply_settings(logic_apps_dir, updates) {
            report.errors.push(format!("local.settings.json: {e}"));
        }
    }

    let dir = workflows::resolve_logic_apps_dir(logic_apps_dir);
    match connections_local::scaffold_override_file(&dir) {
        Ok(p) => report.scaffolded = Some(p.display().to_string()),
        Err(e) => report.errors.push(format!("connections.local.json: {e}")),
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_when_nothing_pending() {
        let r = RunReadiness { workflow: "W".into(), ..Default::default() };
        assert!(r.is_ready());
        assert!(!r.needs_manual());
    }

    #[test]
    fn needs_manual_on_blocking_or_missing() {
        let r = RunReadiness {
            workflow: "W".into(),
            blocking_settings: vec![("sql".into(), "SQL_CS".into())],
            ..Default::default()
        };
        assert!(!r.is_ready());
        assert!(r.needs_manual());
    }

    #[test]
    fn cloud_pointing_blocks_but_is_not_manual() {
        let r = RunReadiness {
            workflow: "W".into(),
            cloud_pointing: vec![("K".into(), "https://x.database.windows.net".into(), "local".into())],
            ..Default::default()
        };
        assert!(!r.is_ready());
        assert!(!r.needs_manual());
    }

    #[test]
    fn detects_cloud_endpoints() {
        assert!(is_cloud_value("Server=tcp:foo.database.windows.net,1433;"));
        assert!(is_cloud_value("Endpoint=sb://bar.servicebus.windows.net/;"));
        assert!(is_cloud_value("https://acct.blob.core.windows.net"));
        assert!(is_cloud_value("AccountEndpoint=https://c.documents.azure.com:443/;"));
        assert!(is_cloud_value("https://partner-api.example.com/v1"));
        // Local / neutral values are NOT cloud.
        assert!(!is_cloud_value("UseDevelopmentStorage=true"));
        assert!(!is_cloud_value("http://localhost:8081"));
        assert!(!is_cloud_value("https://placeholder.azure-apim.net/apim/teams/teams-local/"));
        // Real managed-API connector URLs must NOT be treated as redirectable cloud.
        assert!(!is_cloud_value("https://acme-prod.azure-apim.net/apim/teams/teams-1a2b/"));
        assert!(!is_cloud_value("https://logic-apis-northeurope.azure-apim.net/apim/office365/conn/"));
        assert!(!is_cloud_value(""));
    }

    #[test]
    fn cloud_maps_to_local_emulators() {
        let no_settings = serde_json::Value::Null;
        let target = |v: &str| local_target_for("unrelated_key", v, &no_settings);
        assert!(target("x.database.windows.net").contains("Server=localhost,1433"));
        assert!(target("x.documents.azure.com").contains("localhost:8081"));
        assert!(target("x.servicebus.windows.net").contains("localhost"));
        assert_eq!(target("x.blob.core.windows.net"), "UseDevelopmentStorage=true");
        assert_eq!(target("https://partner.example.com"), MOCK_BASE_URL);
    }

    #[test]
    fn a_sql_connection_keeps_the_database_its_sibling_setting_names() {
        // Pointing every SQL connection at `master` opens fine and then fails
        // every lookup: a workflow calling a stored procedure gets "doesn't
        // exist" even though the procedure was created in the project's own
        // database. The database name is right there in local.settings.json.
        let settings = serde_json::json!({
            "Values": {
                "sqlServerAIS_connectionString": "Server=tcp:corp.database.windows.net,1433;Database=ais;",
                "sqlServerAIS_databaseName": "aisdev",
                "sqlServerODS_connectionString": "Server=tcp:corp.database.windows.net,1433;Database=ods;",
                "sqlServerODS_databaseName": "  ",
            }
        });

        assert_eq!(
            sibling_database("sqlServerAIS_connectionString", &settings).as_deref(),
            Some("aisdev")
        );
        let target = local_target_for(
            "sqlServerAIS_connectionString",
            "Server=tcp:corp.database.windows.net,1433;Database=ais;",
            &settings,
        );
        assert!(target.contains("Database=aisdev;"), "got {target}");

        // A blank or absent sibling falls back to master, which at least opens.
        assert_eq!(
            sibling_database("sqlServerODS_connectionString", &settings),
            None
        );
        assert!(local_target_for(
            "sqlServerODS_connectionString",
            "Server=tcp:corp.database.windows.net,1433;Database=ods;",
            &settings,
        )
        .contains("Database=master;"));
        assert_eq!(sibling_database("someOther_key", &settings), None);
    }
}
