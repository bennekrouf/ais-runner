//! Startup gate: refuse to open a project whose local configuration cannot
//! produce a working func host.
//!
//! The checks here are deliberately *blocking*, not advisory. Every one of them
//! covers a failure that is silent at startup and only shows up much later as
//! "the workflow never ran" — the trigger layer simply never arms, so there is
//! no run, no error, and nothing to read. Letting the app open in that state
//! costs hours of debugging in the wrong place, so it is better to stop here
//! and say exactly what to repair.
//!
//! Warnings belong in `localize`; this module only reports what must be fixed.

use std::path::Path;

use crate::services::mock::rewrite;

/// A problem that must be resolved before the project can be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocker {
    /// One line naming what is wrong.
    pub title:  String,
    /// Why it breaks the run — the symptom the user would otherwise chase.
    pub detail: String,
    /// The concrete action that resolves it.
    pub fix:    String,
}

/// Managed-API connection runtime URLs live on `*_connectionUrl` settings.
/// The runtime parses the api name and connection name out of the path, so the
/// value has to keep the apim/apihub shape.
fn is_managed_api_url(value: &str) -> bool {
    value.contains("azure-apim.net") || value.contains("azure-apihub.net")
}

fn settings_values(logic_apps_dir: &Path) -> Option<serde_json::Map<String, serde_json::Value>> {
    let raw = std::fs::read_to_string(logic_apps_dir.join("local.settings.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    json.get("Values")?.as_object().cloned()
}

/// Recover a setting the mock rewrite lost for good, without asking the
/// developer to go hunting for it by hand.
///
/// Deliberately source-agnostic: teams keep their canonical values in
/// different places — an Azure App Configuration export, a committed
/// `local.settings.<env>.json`, a `config/` tree of per-environment JSON — and
/// hardcoding one layout means the recovery silently does nothing for everyone
/// else. Sources are tried in order of how likely they are to hold a value
/// that is correct *for this machine*:
///
///   0. The live `local.settings.json` itself. `sanitize` repairs the backup
///      snapshot as well as the live file, and the two are rarely poisoned
///      together — a working live value is the most authoritative answer for
///      this machine. Self-lookup is harmless when repairing the live file:
///      a still-mocked value is rejected as a candidate.
///   1. Sibling `local.settings.*.json` — same shape as the file being
///      repaired and usually already localhost-shaped, so it needs no
///      translation.
///   2. Any JSON under a `config/` tree, in either the flat `{key: value}`
///      shape an App Configuration export uses or the `{"Values": {…}}` shape
///      Functions uses. Files whose name suggests a local/dev environment are
///      preferred over staging or production.
///   3. Derived from a sibling setting in the file itself — a connection
///      string very often embeds the endpoint that a `*_accountEndpoint` or
///      `*_serverName` sibling lost.
///   4. Settings whose *local* target is fixed regardless of environment —
///      blob endpoints are always Azurite, and `*_triggerUrl` settings route
///      through the mock server, so those are known rather than guessed.
///
/// A value recovered from a cloud-shaped source is handed to `localize` on the
/// next func start, which rewrites it to its local equivalent — so recovering
/// a production URL here is still progress, not a regression.
fn recover_lost_setting(logic_apps_dir: &Path, name: &str) -> Option<String> {
    lookup_in_json(&logic_apps_dir.join("local.settings.json"), name)
        .or_else(|| lookup_sibling_settings_files(logic_apps_dir, name))
        .or_else(|| lookup_config_tree(logic_apps_dir, name))
        .or_else(|| derive_from_sibling_setting(logic_apps_dir, name))
        .or_else(|| known_local_target(name))
}

/// Read `name` from a JSON document in either the Functions shape
/// (`{"Values": {…}}`) or a flat App-Configuration-style map.
fn lookup_in_json(path: &Path, name: &str) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let found = json
        .get("Values")
        .and_then(|v| v.get(name))
        .or_else(|| json.get(name))?
        .as_str()?
        .trim()
        .to_string();
    // An empty or still-mocked value is not a recovery.
    if found.is_empty() || found.contains("/__mock__/") {
        return None;
    }
    Some(found)
}

/// `local.settings.ci.json`, `local.settings.dev.json`, … beside the file
/// being repaired. Never the file itself.
fn lookup_sibling_settings_files(logic_apps_dir: &Path, name: &str) -> Option<String> {
    let mut candidates: Vec<_> = std::fs::read_dir(logic_apps_dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let Some(f) = p.file_name().and_then(|f| f.to_str()) else { return false };
            f.starts_with("local.settings.")
                && f.ends_with(".json")
                && f != "local.settings.json"
        })
        .collect();
    candidates.sort_by_key(|p| environment_rank(p));
    candidates.iter().find_map(|p| lookup_in_json(p, name))
}

/// Any JSON under a `config/` directory at or above the workflow folder.
fn lookup_config_tree(logic_apps_dir: &Path, name: &str) -> Option<String> {
    let mut roots = vec![logic_apps_dir.join("config")];
    if let Some(parent) = logic_apps_dir.parent() {
        roots.push(parent.join("config"));
    }
    let mut files = Vec::new();
    for root in roots {
        collect_json_files(&root, 0, &mut files);
    }
    files.sort_by_key(|p| environment_rank(p));
    files.iter().find_map(|p| lookup_in_json(p, name))
}

fn collect_json_files(dir: &Path, depth: usize, out: &mut Vec<std::path::PathBuf>) {
    // Bounded: a config tree is shallow, and an unbounded walk of an arbitrary
    // project directory would be both slow and surprising.
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_json_files(&path, depth + 1, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
            out.push(path);
        }
    }
}

/// Sort key preferring environments closest to a developer's machine. Values
/// from a local or dev export need the least translation to work here;
/// production is the last thing to copy a URL from.
fn environment_rank(path: &Path) -> (u8, String) {
    let name = path
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or_default()
        .to_lowercase();
    let rank = if name.contains("local") {
        0
    } else if name.contains("ci") || name.contains("dev") {
        1
    } else if name.contains("stg") || name.contains("staging") || name.contains("test") {
        2
    } else if name.contains("prod") {
        4
    } else {
        3
    };
    (rank, name)
}

/// Rebuild a lost value from a connection string that already contains it.
///
/// `cosmos_accountEndpoint` and `cosmos_connectionString` hold the same URL,
/// so losing one while the other survives is recoverable with no external
/// source at all. Matches on the suffix after the last `_`, against the
/// `Key=Value;` tokens of any sibling sharing the same prefix.
fn derive_from_sibling_setting(logic_apps_dir: &Path, name: &str) -> Option<String> {
    let values = settings_values(logic_apps_dir)?;
    let (prefix, suffix) = name.rsplit_once('_')?;

    for (k, v) in &values {
        if k == name || !k.starts_with(prefix) {
            continue;
        }
        let Some(text) = v.as_str() else { continue };
        if !text.contains('=') {
            continue;
        }
        for token in text.split(';') {
            let Some((tk, tv)) = token.split_once('=') else { continue };
            if tk.trim().eq_ignore_ascii_case(suffix) && !tv.trim().is_empty() {
                return Some(tv.trim().to_string());
            }
        }
    }
    None
}

fn known_local_target(name: &str) -> Option<String> {
    if name.ends_with("_blobStorageEndpoint") {
        return Some("http://127.0.0.1:10000/devstoreaccount1".to_string());
    }
    if name.ends_with("_triggerUrl") {
        // These are function-app callback URLs invoked as service-provider
        // connections; the local mock server is the correct local target, not
        // a stand-in for one we couldn't find.
        return Some(format!("{}/__mock__/{}", crate::services::run_readiness::MOCK_BASE_URL, name));
    }
    None
}

/// Repair what can be repaired, then report what still blocks the run.
///
/// Sanitising first matters: most leftover mock state is recoverable, and a
/// gate that blocks on problems it could have fixed itself is just an obstacle.
/// Only genuinely lost values are reported.
/// Returns the blockers that remain, the mock-sanitize report, and a list of
/// repairs performed automatically — the last so the caller can tell the user
/// what changed on disk rather than doing it silently.
pub fn check(logic_apps_dir: &str) -> (Vec<Blocker>, rewrite::SanitizeReport, Vec<String>) {
    let dir = crate::services::workflows::resolve_logic_apps_dir(logic_apps_dir);
    let mut blockers = Vec::new();
    let mut repairs: Vec<String> = Vec::new();

    // 1. Leftover mock state. `restore()` only runs on a clean shutdown, so a
    //    crash or force-quit leaves mock URLs on disk. The mock server resolves
    //    a request back to the original URL through the stash, so a stale entry
    //    leaves it with no upstream: calls return an empty body and the failure
    //    surfaces somewhere else entirely.
    let sanitized = rewrite::sanitize_workspace_with_fallback(&dir, |name| {
        recover_lost_setting(&dir, name)
    })
    .unwrap_or_default();
    if !sanitized.unrecoverable.is_empty() {
        blockers.push(Blocker {
            title: format!(
                "{} setting(s) still point at a stopped mock server",
                sanitized.unrecoverable.len()
            ),
            detail: format!(
                "{} — their real values were overwritten by an earlier run, and no recovery \
                 source had them: not the file's own snapshot, not a sibling \
                 local.settings.<env>.json, not any JSON under a config/ folder, and not \
                 derivable from a related setting. Workflows calling them get an empty \
                 response, which surfaces as an unrelated failure downstream.",
                sanitized.unrecoverable.join(", ")
            ),
            fix: "Set each one to its local target in logic_apps/local.settings.json. To let \
                  this repair itself next time, commit the value to a local.settings.<env>.json \
                  beside it, or to any JSON under a config/ folder — both are picked up \
                  automatically."
                .into(),
        });
    }

    // 2. Hijacked managed-API connection URLs. This one takes down the whole
    //    host: the workflow fails validation, every workflow that calls it as a
    //    child fails too, and no trigger is ever registered.
    if let Some(values) = settings_values(&dir) {
        let hijacked: Vec<String> = values
            .iter()
            .filter(|(k, _)| k.to_lowercase().ends_with("_connectionurl"))
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .filter(|(_, v)| !v.is_empty() && !is_managed_api_url(v))
            .map(|(k, v)| format!("{k} = {v}"))
            .collect();

        if !hijacked.is_empty() {
            blockers.push(Blocker {
                title: format!(
                    "{} managed-API connection URL(s) no longer point at apim",
                    hijacked.len()
                ),
                detail: format!(
                    "{}. The runtime parses the api and connection name out of this URL; \
                     anything else fails workflow validation with \"the api name and connection \
                     name should not be null or empty\". That cascades to every workflow calling \
                     it as a child, so no trigger is registered and nothing ever runs.",
                    hijacked.join(", ")
                ),
                fix: "Restore the https://<region>.azure-apim.net/apim/<api>/<connection>/ value. \
                      These settings must never be redirected at the mock server."
                    .into(),
            });
        }
    }

    // 3. The developer-local connection override.
    //
    //    Absence is not a blocker: `func start` already localizes
    //    connections.json in place (MSI → local connection strings), and this
    //    file only carries per-developer tweaks on top of that. Its scaffolded
    //    form is an empty template, so refusing to open the project until the
    //    user creates one demanded a file whose contents would change nothing.
    //    Create it and say so instead — it is gitignored, and having it present
    //    gives the user somewhere obvious to put an override later.
    let overrides = dir.join(crate::services::connections_local::FILENAME);
    if !overrides.exists() {
        match crate::services::connections_local::scaffold_override_file(&dir) {
            Ok(path) => repairs.push(format!("Created {} (gitignored)", path.display())),
            Err(e) => blockers.push(Blocker {
                title: "connections.local.json could not be created".into(),
                detail: format!(
                    "{e} — without it there is nowhere to put a developer-local connection \
                     override, and the folder may not be writable."
                ),
                fix: format!("Create {} by hand, or fix the permissions on its folder.",
                    overrides.display()),
            }),
        }
    } else if let Err(e) = crate::services::connections_local::load_overrides(&dir) {
        blockers.push(Blocker {
            title: "connections.local.json is not valid JSON".into(),
            detail: format!("{e} — the override is skipped entirely, so func starts against \
                             the Azure endpoints in connections.json."),
            fix: format!("Fix the syntax in {}.", overrides.display()),
        });
    }

    (blockers, sanitized, repairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(values: serde_json::Value) -> std::path::PathBuf {
        let ws = std::env::temp_dir()
            .join(format!("ais-preflight-{}-{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            ws.join("local.settings.json"),
            serde_json::to_string_pretty(&serde_json::json!({ "Values": values })).unwrap(),
        )
        .unwrap();
        std::fs::write(ws.join("connections.local.json"), "{}").unwrap();
        ws
    }

    /// A workspace shaped like the real repo: `<root>/logic_apps/` next to
    /// `<root>/config/appconfig/appconfig.dev.json`, needed to test the
    /// appconfig fallback since it looks at `logic_apps_dir.parent()`.
    fn workspace_with_appconfig(
        values: serde_json::Value,
        appconfig: serde_json::Value,
    ) -> std::path::PathBuf {
        let root = std::env::temp_dir()
            .join(format!("ais-preflight-{}-{}", std::process::id(), rand_suffix()));
        let logic_apps = root.join("logic_apps");
        std::fs::create_dir_all(&logic_apps).unwrap();
        std::fs::write(
            logic_apps.join("local.settings.json"),
            serde_json::to_string_pretty(&serde_json::json!({ "Values": values })).unwrap(),
        )
        .unwrap();
        std::fs::write(logic_apps.join("connections.local.json"), "{}").unwrap();
        let appcfg_dir = root.join("config/appconfig");
        std::fs::create_dir_all(&appcfg_dir).unwrap();
        std::fs::write(
            appcfg_dir.join("appconfig.dev.json"),
            serde_json::to_string_pretty(&appconfig).unwrap(),
        )
        .unwrap();
        root
    }

    /// Unique per call, not merely per instant. A purely time-based suffix
    /// collides when two tests build a workspace inside the same clock tick,
    /// and they then share a temp directory — which shows up as one test
    /// blocking on a setting that belongs to another.
    fn rand_suffix() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        format!(
            "{}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[test]
    fn clean_workspace_has_no_blockers() {
        let ws = workspace(serde_json::json!({
            "Teams_connectionUrl": "https://switzerlandnorth.azure-apim.net/apim/teams/teams-local/",
            "Jde_Url": "http://localhost:9000",
        }));
        let (blockers, _, _) = check(ws.to_str().unwrap());
        assert!(blockers.is_empty(), "unexpected blockers: {blockers:?}");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_connection_url_pointed_at_the_mock_blocks() {
        let ws = workspace(serde_json::json!({
            "Teams_connectionUrl": "http://localhost:53496/__mock__/Teams_connectionUrl",
        }));
        let (blockers, _, _) = check(ws.to_str().unwrap());
        assert!(
            blockers.iter().any(|b| b.title.contains("managed-API")),
            "a hijacked *_connectionUrl must block startup: {blockers:?}"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    /// Recoverable leftovers are repaired in place — blocking on those would
    /// stop the user for something the gate can fix itself.
    #[test]
    fn recoverable_mock_leftovers_are_healed_not_blocked() {
        let ws = workspace(serde_json::json!({
            "Jde_Url": "http://localhost:3333/__mock__/Jde_Url",
            "__mock_original__Jde_Url": "http://localhost:9000",
        }));
        let (blockers, sanitized, _) = check(ws.to_str().unwrap());
        assert!(blockers.is_empty(), "unexpected blockers: {blockers:?}");
        assert_eq!(sanitized.recovered, vec!["Jde_Url".to_string()]);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn an_unrecoverable_mock_url_blocks() {
        let ws = workspace(serde_json::json!({
            "Jde_Url": "http://localhost:3333/__mock__/Jde_Url",
            "__mock_original__Jde_Url": "http://localhost:2222/__mock__/Jde_Url",
        }));
        let (blockers, _, _) = check(ws.to_str().unwrap());
        assert!(
            blockers.iter().any(|b| b.title.contains("stopped mock server")),
            "a lost original must block startup: {blockers:?}"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    /// The scenario that motivated this fallback: a setting lost by the mock
    /// rewrite is still in the project's own appconfig record, so the gate
    /// heals it instead of stopping the developer with nothing to act on.
    #[test]
    fn a_lost_setting_is_recovered_from_appconfig_dev() {
        let root = workspace_with_appconfig(
            serde_json::json!({
                "JdeUrl": "http://localhost:3333/__mock__/JdeUrl",
                "__mock_original__JdeUrl": "http://localhost:2222/__mock__/JdeUrl",
            }),
            serde_json::json!({
                "JdeUrl": "https://jdeproxynativeconnectordev-oryxenergies.msappproxy.net",
            }),
        );
        let (blockers, sanitized, _) = check(root.to_str().unwrap());
        assert!(blockers.is_empty(), "unexpected blockers: {blockers:?}");
        assert_eq!(sanitized.recovered, vec!["JdeUrl".to_string()]);

        let raw = std::fs::read_to_string(root.join("logic_apps/local.settings.json")).unwrap();
        assert!(raw.contains("jdeproxynativeconnectordev"));
        std::fs::remove_dir_all(&root).ok();
    }

    /// A blob endpoint is not in appconfig.dev.json under that exact shape in
    /// every project, but its local target never varies — Azurite is always
    /// Azurite — so it must heal even without an appconfig hit.
    #[test]
    fn a_lost_blob_endpoint_falls_back_to_azurite() {
        let root = workspace_with_appconfig(
            serde_json::json!({
                "IgniteBlob_blobStorageEndpoint": "http://localhost:3333/__mock__/IgniteBlob_blobStorageEndpoint",
                "__mock_original__IgniteBlob_blobStorageEndpoint": "http://localhost:2222/__mock__/IgniteBlob_blobStorageEndpoint",
            }),
            serde_json::json!({}),
        );
        let (blockers, sanitized, _) = check(root.to_str().unwrap());
        assert!(blockers.is_empty(), "unexpected blockers: {blockers:?}");
        assert_eq!(sanitized.recovered, vec!["IgniteBlob_blobStorageEndpoint".to_string()]);

        let raw = std::fs::read_to_string(root.join("logic_apps/local.settings.json")).unwrap();
        assert!(raw.contains("127.0.0.1:10000"));
        std::fs::remove_dir_all(&root).ok();
    }

    /// A setting neither appconfig nor the known-local-target list can answer
    /// for must still block — silently guessing would be worse than asking.
    #[test]
    fn a_setting_missing_from_every_source_still_blocks() {
        let root = workspace_with_appconfig(
            serde_json::json!({
                "SomeBespokeUrl": "http://localhost:3333/__mock__/SomeBespokeUrl",
                "__mock_original__SomeBespokeUrl": "http://localhost:2222/__mock__/SomeBespokeUrl",
            }),
            serde_json::json!({ "UnrelatedKey": "https://example.com" }),
        );
        let (blockers, _, _) = check(root.to_str().unwrap());
        assert!(
            blockers.iter().any(|b| b.title.contains("stopped mock server")),
            "a setting with no recovery source anywhere must still block: {blockers:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_missing_local_override_is_scaffolded_not_blocked() {
        // `func start` already localizes connections.json in place, and the
        // scaffolded override is an empty template — so blocking here demanded
        // a file whose contents would change nothing. Create it and report it.
        let ws = workspace(serde_json::json!({ "Jde_Url": "http://localhost:9000" }));
        std::fs::remove_file(ws.join("connections.local.json")).unwrap();

        let (blockers, _, repairs) = check(ws.to_str().unwrap());
        assert!(blockers.is_empty(), "must not block: {blockers:?}");
        assert!(
            repairs.iter().any(|r| r.contains("connections.local.json")),
            "the automatic repair must be reported, not done silently: {repairs:?}"
        );
        assert!(ws.join("connections.local.json").exists());

        // Idempotent: a second pass has nothing left to repair.
        let (blockers, _, repairs) = check(ws.to_str().unwrap());
        assert!(blockers.is_empty());
        assert!(repairs.is_empty(), "nothing to redo: {repairs:?}");
        std::fs::remove_dir_all(&ws).ok();
    }

    // ── generic value recovery ──────────────────────────────────────────────

    /// Minimal workspace: a local.settings.json whose `name` was clobbered by
    /// the mock, plus whatever extra files the test wants beside it.
    fn recovery_workspace(tag: &str, values: serde_json::Value) -> std::path::PathBuf {
        let ws = std::env::temp_dir().join(format!("ais-recover-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            ws.join("local.settings.json"),
            serde_json::to_string_pretty(&serde_json::json!({ "Values": values })).unwrap(),
        )
        .unwrap();
        ws
    }

    #[test]
    fn recovers_from_a_sibling_local_settings_file() {
        // The real case: local.settings.ci.json was committed in the repo the
        // whole time and held the lost value.
        let ws = recovery_workspace("sibling", serde_json::json!({
            "ApimCallbackUrl": "http://localhost:5555/__mock__/ApimCallbackUrl"
        }));
        std::fs::write(
            ws.join("local.settings.ci.json"),
            r#"{ "Values": { "ApimCallbackUrl": "http://127.0.0.1:7079" } }"#,
        )
        .unwrap();

        assert_eq!(
            recover_lost_setting(&ws, "ApimCallbackUrl").as_deref(),
            Some("http://127.0.0.1:7079")
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn recovers_from_any_config_tree_json_in_either_shape() {
        // App Configuration exports are a flat map; a Functions-shaped file
        // nests under "Values". Both are common, so both must work.
        // The live value must itself be mocked — recovery only ever runs for a
        // setting the mock clobbered, and a clean live value would (correctly)
        // be preferred over any external source.
        let ws = recovery_workspace("cfg", serde_json::json!({
            "SomeUrl": "http://localhost:5555/__mock__/SomeUrl"
        }));
        std::fs::create_dir_all(ws.join("config/appconfig")).unwrap();
        std::fs::write(
            ws.join("config/appconfig/anything.dev.json"),
            r#"{ "SomeUrl": "https://real.example.com" }"#,
        )
        .unwrap();
        assert_eq!(
            recover_lost_setting(&ws, "SomeUrl").as_deref(),
            Some("https://real.example.com")
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn prefers_a_local_or_dev_source_over_production() {
        let ws = recovery_workspace("rank", serde_json::json!({
            "ApiUrl": "http://localhost:5555/__mock__/ApiUrl"
        }));
        std::fs::create_dir_all(ws.join("config")).unwrap();
        std::fs::write(ws.join("config/settings.prod.json"), r#"{ "ApiUrl": "https://prod" }"#).unwrap();
        std::fs::write(ws.join("config/settings.dev.json"),  r#"{ "ApiUrl": "https://dev" }"#).unwrap();
        assert_eq!(recover_lost_setting(&ws, "ApiUrl").as_deref(), Some("https://dev"));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn derives_an_endpoint_from_a_sibling_connection_string() {
        // The other real case: cosmos_accountEndpoint was lost while
        // cosmos_connectionString still carried the same URL. No external
        // source needed.
        let ws = recovery_workspace("derive", serde_json::json!({
            "cosmos_accountEndpoint":   "http://localhost:5555/__mock__/cosmos_accountEndpoint",
            "cosmos_connectionString":  "AccountEndpoint=http://localhost:8081/;AccountKey=abc=="
        }));
        assert_eq!(
            recover_lost_setting(&ws, "cosmos_accountEndpoint").as_deref(),
            Some("http://localhost:8081/")
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_still_mocked_or_empty_candidate_is_not_treated_as_a_recovery() {
        let ws = recovery_workspace("stale", serde_json::json!({
            "Thing": "http://localhost:5555/__mock__/Thing"
        }));
        std::fs::write(
            ws.join("local.settings.dev.json"),
            r#"{ "Values": { "Thing": "http://localhost:9/__mock__/Thing", "Other": "" } }"#,
        )
        .unwrap();
        assert_eq!(recover_lost_setting(&ws, "Thing"), None);
        assert_eq!(recover_lost_setting(&ws, "Other"), None);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_poisoned_backup_is_healed_from_the_live_file() {
        // `sanitize` repairs the backup snapshot too. When the live file has
        // already been fixed but the snapshot still holds a mock URL, that is
        // recoverable — blocking the user over a stale snapshot whose answer
        // is sitting in the live file next to it is pure friction.
        let root = std::env::temp_dir()
            .join(format!("ais-preflight-{}-{}", std::process::id(), rand_suffix()));
        let logic_apps = root.join("logic_apps");
        std::fs::create_dir_all(logic_apps.join(".ais-cache")).unwrap();
        std::fs::write(logic_apps.join("connections.local.json"), "{}").unwrap();
        std::fs::write(
            logic_apps.join("local.settings.json"),
            r#"{ "Values": { "ApimCallbackUrl": "http://127.0.0.1:7079" } }"#,
        )
        .unwrap();
        std::fs::write(
            logic_apps.join(".ais-cache/local.settings.json.original"),
            r#"{ "Values": { "ApimCallbackUrl": "http://localhost:59853/__mock__/ApimCallbackUrl" } }"#,
        )
        .unwrap();

        let (blockers, _, _) = check(root.to_str().unwrap());
        assert!(blockers.is_empty(), "unexpected blockers: {blockers:?}");

        let healed = std::fs::read_to_string(
            logic_apps.join(".ais-cache/local.settings.json.original"),
        )
        .unwrap();
        assert!(healed.contains("127.0.0.1:7079"), "backup not healed: {healed}");
        assert!(!healed.contains("__mock__"));
        std::fs::remove_dir_all(&root).ok();
    }
}
