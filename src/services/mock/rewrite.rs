//! Rewrite the URL-shaped values in `local.settings.json` so workflows point
//! at the mock server instead of real services.
//!
//! Strategy
//! --------
//! For each setting classified as `SettingKind::Url`, we replace
//!
//!     Jde_Url = "https://jde.example.com"
//!
//! with
//!
//!     Jde_Url = "http://localhost:<mock-port>/__mock__/Jde_Url"
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
        .map(|values| values.keys().any(|k| k.starts_with(ORIGINAL_KEY_PREFIX)))
        .unwrap_or(false)
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
            let kind = contract.app_settings.get(name).map(|s| s.kind);
            if kind != Some(SettingKind::Url) { continue; }
            let original = match slot.as_str() {
                Some(s) => s.to_string(),
                None    => continue,
            };
            if original.starts_with(&mock_base) { continue; }
            let new_value = format!("{}{}{}", mock_base, MOCK_PREFIX, name);
            pending.push((name.clone(), new_value, original));
        }
        for (name, new_value, original) in pending {
            values.insert(name.clone(), Value::String(new_value));
            // Stash the original inside Values so the mock server can resolve
            // "{setting_name} → original URL" on the fly. The Functions runtime
            // ignores unknown keys — this is safe.
            let orig_key = format!("{}{}", ORIGINAL_KEY_PREFIX, name);
            values.insert(orig_key, Value::String(original));
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
}
