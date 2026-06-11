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
//! back. Both operations are idempotent — re-running the rewrite when the
//! backup already exists does NOT clobber the original.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::services::mock::contract::{MockContract, SettingKind};
use crate::services::mock::scanner::ScanError;
use crate::services::mock::writer::cache_dir;

const MOCK_PREFIX: &str = "/__mock__/";
const BACKUP_NAME: &str = "local.settings.json.original";

pub struct RewriteOutcome {
    pub rewritten_count: usize,
    pub backup_path:     PathBuf,
}

/// Rewrite URL-kind settings to point at `http://localhost:{mock_port}`.
/// Idempotent w.r.t. the backup: if a backup already exists, it is preserved
/// (assumed to be the true original).
pub fn rewrite(
    workspace: &Path,
    contract:  &MockContract,
    mock_port: u16,
) -> Result<RewriteOutcome, ScanError> {
    let settings_path = workspace.join("local.settings.json");
    let raw           = std::fs::read_to_string(&settings_path)?;
    let mut json: Value = serde_json::from_str(&raw)?;

    // 1. Snapshot the original (only if no backup yet).
    let backup_dir   = cache_dir(workspace);
    std::fs::create_dir_all(&backup_dir)?;
    let backup_path  = backup_dir.join(BACKUP_NAME);
    if !backup_path.exists() {
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
            let orig_key = format!("__mock_original__{}", name);
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
    let key = format!("__mock_original__{}", setting_name);
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
}
