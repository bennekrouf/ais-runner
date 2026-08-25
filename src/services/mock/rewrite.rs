//! Rewrite the URL-shaped values in `local.settings.json` so workflows point
//! at the mock server instead of real services.
//!
//! Strategy
//! --------
//! For each setting classified as `SettingKind::Url`, we replace
//!
//! ```text
//! Jde_Url = "https://jde.example.com"
//! ```
//!
//! with
//!
//! ```text
//! Jde_Url = "http://localhost:<mock-port>/__mock__/Jde_Url"
//! ```
//!
//! The mock server strips the `/__mock__/<name>` prefix at request time, so it
//! knows which logical service the request was meant for. The remaining path
//! is matched against the contract.
//!
//! Backup / restore
//! ----------------
//! Before touching `local.settings.json`, a snapshot is saved to
//! `<workspace>/.ais-cache/local.settings.json.original`. `restore()` puts it
//! back.
//!
//! The backup is refreshed whenever the on-disk file is *not* already patched,
//! rather than only when no backup exists. "Backup exists" is the wrong guard:
//! after a start/stop cycle the backup survives, so a user who then edits their
//! settings and starts the mock again would have those edits silently reverted
//! to the previous session's snapshot on the next stop. Keying off "is the file
//! currently patched" keeps the operation idempotent (starting twice without an
//! intervening stop never clobbers the true original) while always restoring the
//! settings the user actually had.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::services::mock::contract::{MockContract, SettingKind};
use crate::services::mock::scanner::ScanError;
use crate::services::mock::writer::cache_dir;

const MOCK_PREFIX: &str = "/__mock__/";
const BACKUP_NAME: &str = "local.settings.json.original";
/// Key prefix used to stash a setting's pre-rewrite URL inside `Values`.
/// Its presence is also how we detect an already-patched settings file.
const ORIGINAL_KEY_PREFIX: &str = "__mock_original__";

/// True when `local.settings.json` still carries mock markers from an earlier
/// `rewrite()` — i.e. a previous run never restored (crash, force-quit).
fn is_patched(json: &Value) -> bool {
    json.get("Values")
        .and_then(|v| v.as_object())
        .map(|values| {
            values.keys().any(|k| k.starts_with(ORIGINAL_KEY_PREFIX))
                || values.values().any(|v| v.as_str().is_some_and(is_mocked))
        })
        .unwrap_or(false)
}

/// True when a value already points at *a* mock server.
///
/// Deliberately matches the `/__mock__/` marker rather than the current
/// `http://localhost:<port>` base: the mock port is chosen per run, so a
/// port-keyed check treats last run's mock URL as a real value, rewrites it
/// again, and stores a mock URL as the "original" — destroying it for good.
fn is_mocked(value: &str) -> bool {
    value.contains(MOCK_PREFIX)
}

/// Walk `__mock_original__…` chains to the first value that is not itself a
/// mock URL. Older builds nested the prefix once per run, so the true value can
/// sit several levels deep; returns `None` when every level is a mock URL and
/// the original is unrecoverable.
fn recover_original(values: &Map<String, Value>, name: &str) -> Option<String> {
    let mut key = format!("{}{}", ORIGINAL_KEY_PREFIX, name);
    for _ in 0..16 {
        match values.get(&key).and_then(|v| v.as_str()) {
            Some(v) if !is_mocked(v) => return Some(v.to_string()),
            Some(_) => key = format!("{}{}", ORIGINAL_KEY_PREFIX, key),
            None => return None,
        }
    }
    None
}

/// What a `sanitize()` pass changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SanitizeReport {
    /// Settings restored to their real value from a stash entry.
    pub recovered: Vec<String>,
    /// Settings still pointing at a mock server with no recoverable original.
    /// These need a human: the real value is gone from both file and backup.
    pub unrecoverable: Vec<String>,
    /// Number of `__mock_original__…` keys removed.
    pub stash_removed: usize,
}

impl SanitizeReport {
    pub fn is_clean(&self) -> bool {
        self.recovered.is_empty() && self.unrecoverable.is_empty() && self.stash_removed == 0
    }
}

/// Undo leftover mock state in a settings file.
///
/// `restore()` only helps when the previous session shut down cleanly. After a
/// crash or force-quit the mock URLs stay on disk, and because the mock server
/// resolves a request back to the *original* URL through the stash, a stale
/// entry leaves it with no upstream to call — requests then fail with an empty
/// body, which surfaces far away from the cause. Worse, the poisoned file can
/// be snapshotted as the next "original", making the damage permanent.
///
/// `fallback(setting_name)` is consulted when the stash itself has nothing
/// left to recover — e.g. a lookup in the project's own record of real
/// endpoints (App Configuration export, `.env`, …). Pass `|_| None` when there
/// is no such source.
///
/// Run this on any settings file before trusting it, including the `.ais-cache`
/// backup — otherwise `restore()` reintroduces exactly what was cleaned.
pub fn sanitize_with_fallback(
    path: &Path,
    fallback: impl Fn(&str) -> Option<String>,
) -> Result<SanitizeReport, ScanError> {
    let mut json: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let mut report = SanitizeReport::default();

    if let Some(values) = json.get_mut("Values").and_then(|v| v.as_object_mut()) {
        let snapshot = values.clone();
        let mocked: Vec<String> = snapshot
            .iter()
            .filter(|(k, v)| {
                !k.starts_with(ORIGINAL_KEY_PREFIX) && v.as_str().is_some_and(is_mocked)
            })
            .map(|(k, _)| k.clone())
            .collect();

        for name in mocked {
            match recover_original(&snapshot, &name).or_else(|| fallback(&name)) {
                Some(real) => {
                    values.insert(name.clone(), Value::String(real));
                    report.recovered.push(name);
                }
                None => report.unrecoverable.push(name),
            }
        }

        let stash: Vec<String> = values
            .keys()
            .filter(|k| k.starts_with(ORIGINAL_KEY_PREFIX))
            .cloned()
            .collect();
        report.stash_removed = stash.len();
        for k in stash {
            values.remove(&k);
        }
    }

    report.recovered.sort();
    report.unrecoverable.sort();
    if !report.is_clean() {
        write_pretty(path, &json)?;
    }
    Ok(report)
}

/// [`sanitize_with_fallback`] with no fallback source.
pub fn sanitize(path: &Path) -> Result<SanitizeReport, ScanError> {
    sanitize_with_fallback(path, |_| None)
}

/// Sanitize both the live settings file and the snapshot `restore()` reads.
/// Cleaning only one of the two lets the other put the mock URLs straight back.
pub fn sanitize_workspace(workspace: &Path) -> Result<SanitizeReport, ScanError> {
    sanitize_workspace_with_fallback(workspace, |_| None)
}

/// [`sanitize_workspace`] consulting `fallback` for settings the stash cannot
/// recover — see [`sanitize_with_fallback`].
pub fn sanitize_workspace_with_fallback(
    workspace: &Path,
    fallback: impl Fn(&str) -> Option<String>,
) -> Result<SanitizeReport, ScanError> {
    let mut report = sanitize_with_fallback(&workspace.join("local.settings.json"), &fallback)?;
    let backup = cache_dir(workspace).join(BACKUP_NAME);
    if backup.exists() {
        let b = sanitize_with_fallback(&backup, &fallback)?;
        for name in b.recovered {
            if !report.recovered.contains(&name) {
                report.recovered.push(name);
            }
        }
        for name in b.unrecoverable {
            if !report.unrecoverable.contains(&name) {
                report.unrecoverable.push(name);
            }
        }
        report.stash_removed += b.stash_removed;
        report.recovered.sort();
        report.unrecoverable.sort();
    }
    Ok(report)
}

pub struct RewriteOutcome {
    pub rewritten_count: usize,
    pub backup_path:     PathBuf,
}

/// Rewrite URL-kind settings to point at `http://localhost:{mock_port}`.
///
/// The backup is refreshed from the current file unless that file is already
/// patched, so between-session edits are never lost and re-running the rewrite
/// on an already-patched file cannot overwrite the true original with mock URLs.
pub fn rewrite(
    workspace: &Path,
    contract:  &MockContract,
    mock_port: u16,
) -> Result<RewriteOutcome, ScanError> {
    let settings_path = workspace.join("local.settings.json");
    let raw           = std::fs::read_to_string(&settings_path)?;
    let mut json: Value = serde_json::from_str(&raw)?;

    // 1. Snapshot the original. Refreshed whenever the on-disk file is clean,
    //    so edits made between sessions survive; skipped when the file is still
    //    patched from an earlier run, which would otherwise back up mock URLs
    //    and make the true original unrecoverable.
    let backup_dir   = cache_dir(workspace);
    std::fs::create_dir_all(&backup_dir)?;
    let backup_path  = backup_dir.join(BACKUP_NAME);
    if !is_patched(&json) {
        std::fs::write(&backup_path, &raw)?;
    }

    // 2. Walk Values and rewrite URL-kind entries.
    //    Two passes to keep the borrow checker happy: first collect what to
    //    change, then apply the changes.
    let mock_base = format!("http://localhost:{}", mock_port);
    let mut rewritten = 0usize;
    if let Some(values) = json.get_mut("Values").and_then(|v| v.as_object_mut()) {
        let mut pending: Vec<(String, String, String)> = vec![]; // (name, new_value, original)
        for (name, slot) in values.iter() {
            // Never rewrite our own stash entries. They hold real URLs and are
            // themselves URL-shaped, so without this they get mocked too and
            // re-stashed under a doubled prefix — one extra nesting level per
            // run, until the true original is buried and lost.
            if name.starts_with(ORIGINAL_KEY_PREFIX) { continue; }
            let kind = contract.app_settings.get(name).map(|s| s.kind);
            if kind != Some(SettingKind::Url) { continue; }
            let original = match slot.as_str() {
                Some(s) => s.to_string(),
                None    => continue,
            };
            if is_mocked(&original) { continue; }
            let new_value = format!("{}{}{}", mock_base, MOCK_PREFIX, name);
            pending.push((name.clone(), new_value, original));
        }
        for (name, new_value, original) in pending {
            values.insert(name.clone(), Value::String(new_value));
            // Stash the original inside Values so the mock server can resolve
            // "{setting_name} → original URL" on the fly. The Functions runtime
            // ignores unknown keys — this is safe.
            //
            // Only ever written when absent: an existing entry is the real URL
            // from an earlier rewrite that never got restored, and overwriting
            // it with the current (possibly already-mocked) value would lose it.
            let orig_key = format!("{}{}", ORIGINAL_KEY_PREFIX, name);
            values.entry(orig_key).or_insert(Value::String(original));
            rewritten += 1;
        }
    }

    write_pretty(&settings_path, &json)?;

    Ok(RewriteOutcome { rewritten_count: rewritten, backup_path })
}

/// Restore `local.settings.json` from the backup written by `rewrite()`.
/// No-op (and no error) if no backup exists.
pub fn restore(workspace: &Path) -> Result<bool, ScanError> {
    let settings_path = workspace.join("local.settings.json");
    let backup_path   = cache_dir(workspace).join(BACKUP_NAME);
    if !backup_path.exists() {
        return Ok(false);
    }
    let original = std::fs::read_to_string(&backup_path)?;
    std::fs::write(&settings_path, original)?;
    // Keep the backup file — multiple start/stop cycles are common, and we
    // never want to lose the true original. The cache dir is hidden + gitignored.
    Ok(true)
}

/// Look up the original URL for a logical setting name from the *patched*
/// `local.settings.json`. Returns `None` if the value was never stashed.
pub fn original_url_for(values: &Map<String, Value>, setting_name: &str) -> Option<String> {
    let key = format!("{}{}", ORIGINAL_KEY_PREFIX, setting_name);
    values.get(&key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

/// Extract `(setting_name, remaining_path)` from a request path that starts
/// with `/__mock__/<name>/<rest>`. Returns `None` if the path does not carry
/// the mock prefix — useful to detect direct calls that bypass the rewrite.
pub fn parse_mock_path(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix(MOCK_PREFIX)?;
    match rest.find('/') {
        Some(i) => Some((&rest[..i], &rest[i..])),
        None    => Some((rest, "/")),
    }
}

fn write_pretty(p: &Path, v: &Value) -> std::io::Result<()> {
    let s = serde_json::to_string_pretty(v)?;
    std::fs::write(p, s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::mock::contract::AppSetting;
    use std::collections::BTreeMap;

    #[test]
    fn parse_mock_path_with_remainder() {
        let (name, rest) = parse_mock_path("/__mock__/Jde_Url/api/bsfn/Foo/executeasync").unwrap();
        assert_eq!(name, "Jde_Url");
        assert_eq!(rest, "/api/bsfn/Foo/executeasync");
    }

    #[test]
    fn parse_mock_path_no_remainder() {
        let (name, rest) = parse_mock_path("/__mock__/Jde_Url").unwrap();
        assert_eq!(name, "Jde_Url");
        assert_eq!(rest, "/");
    }

    #[test]
    fn parse_mock_path_rejects_other_paths() {
        assert!(parse_mock_path("/api/x").is_none());
    }

    #[test]
    fn is_patched_detects_only_a_rewritten_file() {
        let clean: Value = serde_json::json!({ "Values": { "Jde_Url": "https://real.example.com" } });
        assert!(!is_patched(&clean));

        let patched: Value = serde_json::json!({ "Values": {
            "Jde_Url": "http://localhost:1234/__mock__/Jde_Url",
            "__mock_original__Jde_Url": "https://real.example.com",
        }});
        assert!(is_patched(&patched));

        // Shapes that must not panic or false-positive.
        assert!(!is_patched(&serde_json::json!({})));
        assert!(!is_patched(&serde_json::json!({ "Values": "not-an-object" })));
    }

    /// The bug this guards: the backup used to be written only when absent, so
    /// a start → stop → edit settings → start cycle kept the *first* session's
    /// snapshot, and the next stop silently reverted the user's edits.
    #[test]
    fn backup_refreshes_for_edits_made_between_sessions() {
        let ws = std::env::temp_dir().join(format!("ais-rw-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&ws).unwrap();
        let settings = ws.join("local.settings.json");

        // A contract that classifies `Api_Url` as a URL setting.
        let mut app_settings = BTreeMap::new();
        app_settings.insert("Api_Url".to_string(), AppSetting {
            raw_value:      "https://v1.example.com".into(),
            resolved_value: None,
            references:     vec![],
            kind:           SettingKind::Url,
        });
        let contract = MockContract {
            version: "1".into(), generated_at: String::new(),
            workspace: ws.display().to_string(),
            app_settings, endpoints: vec![], warnings: vec![],
        };

        let write = |url: &str| std::fs::write(
            &settings,
            serde_json::to_string_pretty(&serde_json::json!({ "Values": { "Api_Url": url } })).unwrap(),
        ).unwrap();
        let current_url = || -> String {
            let v: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
            v["Values"]["Api_Url"].as_str().unwrap().to_string()
        };

        // Session 1: start, stop.
        write("https://v1.example.com");
        rewrite(&ws, &contract, 1111).unwrap();
        assert!(current_url().starts_with("http://localhost:1111"));
        restore(&ws).unwrap();
        assert_eq!(current_url(), "https://v1.example.com");

        // User edits the settings between sessions.
        write("https://v2.example.com");

        // Session 2: the edit must survive the round trip.
        rewrite(&ws, &contract, 2222).unwrap();
        restore(&ws).unwrap();
        assert_eq!(current_url(), "https://v2.example.com", "edits made between sessions were lost");

        std::fs::remove_dir_all(&ws).ok();
    }

    /// Re-running the rewrite without an intervening restore (double-start, or
    /// a crash that left the file patched) must not back up the mock URLs.
    #[test]
    fn rewriting_an_already_patched_file_keeps_the_true_original() {
        let ws = std::env::temp_dir().join(format!("ais-rw-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&ws).unwrap();
        let settings = ws.join("local.settings.json");

        let mut app_settings = BTreeMap::new();
        app_settings.insert("Api_Url".to_string(), AppSetting {
            raw_value: "https://real.example.com".into(),
            resolved_value: None, references: vec![], kind: SettingKind::Url,
        });
        let contract = MockContract {
            version: "1".into(), generated_at: String::new(),
            workspace: ws.display().to_string(),
            app_settings, endpoints: vec![], warnings: vec![],
        };

        std::fs::write(&settings, serde_json::to_string_pretty(
            &serde_json::json!({ "Values": { "Api_Url": "https://real.example.com" } })
        ).unwrap()).unwrap();

        rewrite(&ws, &contract, 1111).unwrap();
        rewrite(&ws, &contract, 2222).unwrap(); // second start, no restore in between
        restore(&ws).unwrap();

        let v: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(v["Values"]["Api_Url"], "https://real.example.com");

        std::fs::remove_dir_all(&ws).ok();
    }

    fn url_contract(ws: &Path, name: &str) -> MockContract {
        let mut app_settings = BTreeMap::new();
        app_settings.insert(name.to_string(), AppSetting {
            raw_value: String::new(), resolved_value: None,
            references: vec![], kind: SettingKind::Url,
        });
        MockContract {
            version: "1".into(), generated_at: String::new(),
            workspace: ws.display().to_string(),
            app_settings, endpoints: vec![], warnings: vec![],
        }
    }

    fn write_values(path: &Path, values: Value) {
        std::fs::write(path, serde_json::to_string_pretty(
            &serde_json::json!({ "Values": values })).unwrap()).unwrap();
    }

    fn read_values(path: &Path) -> Value {
        serde_json::from_str::<Value>(&std::fs::read_to_string(path).unwrap()).unwrap()["Values"].clone()
    }

    /// The mock port changes every run, so a guard keyed on the current
    /// `http://localhost:<port>` base does not recognise last run's mock URL as
    /// already-mocked. It used to rewrite it again and stash *that* as the
    /// original, losing the real URL — and leaving the mock proxy with no
    /// upstream, which surfaces as empty responses far from the cause.
    #[test]
    fn a_mock_url_from_a_different_port_is_not_rewritten_again() {
        let ws = std::env::temp_dir().join(format!("ais-rw-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&ws).unwrap();
        let settings = ws.join("local.settings.json");
        let contract = url_contract(&ws, "Api_Url");

        write_values(&settings, serde_json::json!({ "Api_Url": "https://real.example.com" }));
        rewrite(&ws, &contract, 1111).unwrap();
        // Second session, different port, no restore in between.
        rewrite(&ws, &contract, 2222).unwrap();

        let v = read_values(&settings);
        assert_eq!(v["__mock_original__Api_Url"], "https://real.example.com",
                   "the stash must still hold the real URL, not a mock one");
        assert_eq!(v["Api_Url"], "http://localhost:1111/__mock__/Api_Url",
                   "an already-mocked value must be left alone");

        std::fs::remove_dir_all(&ws).ok();
    }

    /// Stash keys are URL-shaped, so they were themselves rewritten and
    /// re-stashed under a doubled prefix — one nesting level per run.
    #[test]
    fn stash_keys_are_never_rewritten_themselves() {
        let ws = std::env::temp_dir().join(format!("ais-rw-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&ws).unwrap();
        let settings = ws.join("local.settings.json");

        let mut contract = url_contract(&ws, "Api_Url");
        // Simulate a scanner that also picked up the stash key as a URL setting.
        contract.app_settings.insert("__mock_original__Api_Url".to_string(), AppSetting {
            raw_value: String::new(), resolved_value: None,
            references: vec![], kind: SettingKind::Url,
        });

        write_values(&settings, serde_json::json!({ "Api_Url": "https://real.example.com" }));
        rewrite(&ws, &contract, 1111).unwrap();
        rewrite(&ws, &contract, 2222).unwrap();
        rewrite(&ws, &contract, 3333).unwrap();

        let v = read_values(&settings);
        assert!(v.get("__mock_original____mock_original__Api_Url").is_none(),
                "stash keys must not accumulate prefixes");
        assert_eq!(v["__mock_original__Api_Url"], "https://real.example.com");

        std::fs::remove_dir_all(&ws).ok();
    }

    /// A file left patched by a crash must be recoverable, including through
    /// the nested prefixes older builds produced.
    #[test]
    fn sanitize_recovers_through_nested_stash_levels() {
        let ws = std::env::temp_dir().join(format!("ais-rw-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&ws).unwrap();
        let settings = ws.join("local.settings.json");

        write_values(&settings, serde_json::json!({
            "Api_Url": "http://localhost:3333/__mock__/Api_Url",
            "__mock_original__Api_Url": "http://localhost:2222/__mock__/Api_Url",
            "__mock_original____mock_original__Api_Url": "https://real.example.com",
            "Lost_Url": "http://localhost:3333/__mock__/Lost_Url",
            "__mock_original__Lost_Url": "http://localhost:2222/__mock__/Lost_Url",
            "Untouched": "plain-value",
        }));

        let report = sanitize(&settings).unwrap();
        let v = read_values(&settings);

        assert_eq!(v["Api_Url"], "https://real.example.com");
        assert_eq!(report.recovered, vec!["Api_Url".to_string()]);
        // Every level is a mock URL — nothing to recover, so it must be
        // reported rather than silently left looking valid.
        assert_eq!(report.unrecoverable, vec!["Lost_Url".to_string()]);
        assert_eq!(v["Untouched"], "plain-value");
        assert!(v.as_object().unwrap().keys().all(|k| !k.starts_with(ORIGINAL_KEY_PREFIX)));

        std::fs::remove_dir_all(&ws).ok();
    }

    /// Cleaning the live file but not the snapshot lets `restore()` put the
    /// mock URLs straight back, which is how the corruption became permanent.
    #[test]
    fn sanitize_workspace_also_cleans_the_backup() {
        let ws = std::env::temp_dir().join(format!("ais-rw-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&ws).unwrap();
        let settings = ws.join("local.settings.json");
        let backup_dir = cache_dir(&ws);
        std::fs::create_dir_all(&backup_dir).unwrap();
        let backup = backup_dir.join(BACKUP_NAME);

        let poisoned = serde_json::json!({
            "Api_Url": "http://localhost:3333/__mock__/Api_Url",
            "__mock_original__Api_Url": "https://real.example.com",
        });
        write_values(&settings, poisoned.clone());
        write_values(&backup, poisoned);

        sanitize_workspace(&ws).unwrap();
        restore(&ws).unwrap();

        assert_eq!(read_values(&settings)["Api_Url"], "https://real.example.com",
                   "restore() reintroduced the mock URL from an uncleaned backup");

        std::fs::remove_dir_all(&ws).ok();
    }
}
