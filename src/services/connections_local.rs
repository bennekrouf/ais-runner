//! `connections.local.json` — developer-local override for `connections.json`.
//!
//! Logic Apps Standard reads exactly one file (`connections.json`) for its
//! connection registry, and that file is also the one committed to source
//! control. Anything you change to make it work locally (emulator endpoints,
//! connection strings instead of MSI, mock entries that don't exist in cloud)
//! either pollutes the repo or has to be edited every time you sync.
//!
//! `connections.local.json` solves this with the same pattern as
//! `local.settings.json` vs. `host.json`: same JSON shape as the original,
//! but every key is optional and the file is gitignored. ais-runner deep-merges
//! it on top of `connections.json` right before launching `func start`. The
//! runtime never sees the unmerged file; the source-controlled
//! `connections.json` is left as-is on disk after we patch + merge (the user
//! can `git checkout connections.json` to undo).
//!
//! Merge semantics — deep merge, with these rules:
//! - Objects merge key-by-key, recursively.
//! - Arrays in the override REPLACE the original (the only sane choice for
//!   things like `parameterValues.auth.audiences: [...]` where partial merge
//!   would be ambiguous).
//! - Primitives (strings, numbers, bools) replace.
//! - `null` in the override is treated as "delete this key from the result"
//!   so a local override can intentionally remove a connection entry.

use std::path::Path;

pub const FILENAME: &str = "connections.local.json";

/// Read and parse the local-override file if it exists. Returns:
/// - `Ok(None)` — file not present (the normal case; not an error).
/// - `Ok(Some(json))` — parsed object.
/// - `Err(msg)` — file exists but is unreadable or not valid JSON; caller
///   should surface this prominently because silently ignoring it would
///   defeat the user's intent.
pub fn load_overrides(logic_apps_dir: &Path) -> Result<Option<serde_json::Value>, String> {
    let path = logic_apps_dir.join(FILENAME);
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let v: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| format!("parse {}: {e}", path.display()))?;
    if !v.is_object() {
        return Err(format!("{}: root must be a JSON object", path.display()));
    }
    Ok(Some(v))
}

/// Deep-merge `override` into `base` per the semantics in the module docs.
/// Mutates `base` in place and returns the count of leaf-level keys touched —
/// useful for the "N overrides applied" status line.
pub fn apply_overrides(base: &mut serde_json::Value, overrides: &serde_json::Value) -> usize {
    let mut touched = 0usize;
    merge_in(base, overrides, &mut touched);
    touched
}

fn merge_in(base: &mut serde_json::Value, ov: &serde_json::Value, touched: &mut usize) {
    match (base, ov) {
        (serde_json::Value::Object(b), serde_json::Value::Object(o)) => {
            for (k, ov_val) in o {
                // `_`-prefixed keys are documentation, not overrides — the
                // scaffolded file carries `_readme` and `_examples`, and
                // merging those would write commentary into the project's
                // connections.json.
                if k.starts_with('_') {
                    continue;
                }
                if ov_val.is_null() {
                    // Sentinel: delete this key from the result.
                    if b.remove(k).is_some() { *touched += 1; }
                    continue;
                }
                match b.get_mut(k) {
                    Some(existing) => {
                        if existing.is_object() && ov_val.is_object() {
                            merge_in(existing, ov_val, touched);
                        } else {
                            *existing = ov_val.clone();
                            *touched += 1;
                        }
                    }
                    None => {
                        b.insert(k.clone(), ov_val.clone());
                        *touched += 1;
                    }
                }
            }
        }
        // Type mismatch at this level: override wins outright.
        (b, o) => {
            *b = o.clone();
            *touched += 1;
        }
    }
}

/// Quick check used by the UI: how many connection entries does the override
/// touch? Counts entries under `serviceProviderConnections` and `managedApiConnections`
/// since those are the two shapes Logic Apps Standard cares about.
pub fn override_summary(overrides: &serde_json::Value) -> OverrideSummary {
    let count_under = |key: &str| -> usize {
        overrides.get(key)
            .and_then(|v| v.as_object())
            .map(|o| o.len())
            .unwrap_or(0)
    };
    OverrideSummary {
        service_provider: count_under("serviceProviderConnections"),
        managed_api:      count_under("managedApiConnections"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverrideSummary {
    pub service_provider: usize,
    pub managed_api:      usize,
}

impl OverrideSummary {
    pub fn total(&self) -> usize { self.service_provider + self.managed_api }
}

/// Create a starter `connections.local.json` next to `connections.json` if
/// one doesn't already exist, and make sure `.gitignore` mentions it. Returns
/// the path of the file (created or pre-existing). The template enumerates
/// every connection in the committed `connections.json` so the user can pick
/// and choose what to override without having to re-discover the shape. Those
/// placeholders sit under `_examples`; `_`-prefixed keys are documentation and
/// are skipped by the merge, so the user activates one by lifting it out.
pub fn scaffold_override_file(logic_apps_dir: &Path) -> Result<std::path::PathBuf, String> {
    let target = logic_apps_dir.join(FILENAME);
    if !target.exists() {
        let template = build_template(logic_apps_dir);
        std::fs::write(&target, template)
            .map_err(|e| format!("write {}: {e}", target.display()))?;
    }
    // Best-effort .gitignore patch — additive, idempotent. We don't fail the
    // scaffold if .gitignore is missing or unwritable; the file itself is the
    // important part and the user can add the entry by hand.
    let _ = ensure_gitignore_entry(logic_apps_dir);
    Ok(target)
}

/// Build a starter file. If `connections.json` exists, mirror its
/// `serviceProviderConnections` keys as commented-out placeholders so the
/// user sees their actual connector names without having to look them up.
fn build_template(logic_apps_dir: &Path) -> String {
    let connections_path = logic_apps_dir.join("connections.json");
    let existing: Option<serde_json::Value> = std::fs::read_to_string(&connections_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());

    // No `//` comment header: `load_overrides` parses this file with strict
    // serde_json, so a commented template scaffolds a file the loader then
    // rejects as invalid JSON — the scaffold has to produce something that
    // round-trips through our own reader. The guidance lives in `_readme`
    // instead, which is data and survives parsing.
    let mut examples = serde_json::Map::new();
    if let Some(existing) = existing {
        if let Some(sp) = existing.get("serviceProviderConnections").and_then(|v| v.as_object()) {
            let mut ex_sp = serde_json::Map::new();
            for (name, _) in sp.iter() {
                ex_sp.insert(name.clone(), serde_json::json!({
                    "parameterValues": {
                        "// override fields here, e.g.": "set connectionString or endpoint for local"
                    }
                }));
            }
            if !ex_sp.is_empty() {
                examples.insert("serviceProviderConnections".to_string(), serde_json::Value::Object(ex_sp));
            }
        }
    }
    if examples.is_empty() {
        // No connections.json yet — drop in a minimal skeleton so the user
        // has something to copy from.
        examples.insert("serviceProviderConnections".into(), serde_json::json!({
            "your_connection_name": {
                "parameterValues": { "connectionString": "local override here" }
            }
        }));
    }

    let body = serde_json::json!({
        "_readme": [
            "connections.local.json — developer-local overrides for connections.json.",
            "Applied at func start. Gitignored. Same shape as connections.json, every key optional.",
            "null means \"delete this entry from the merged result\".",
            "Lift an entry OUT of _examples to activate it. Keys starting with _ are ignored."
        ],
        "serviceProviderConnections": {},
        "_examples": examples,
    });
    let mut out = serde_json::to_string_pretty(&body).unwrap_or_default();
    out.push('\n');
    out
}

/// Append the override filename to `.gitignore` if it isn't already there.
/// Creates `.gitignore` if missing. No-op when the entry already exists.
fn ensure_gitignore_entry(logic_apps_dir: &Path) -> std::io::Result<()> {
    let path = logic_apps_dir.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let already_listed = existing.lines().any(|l| l.trim() == FILENAME);
    if already_listed { return Ok(()); }

    let mut new_contents = existing;
    if !new_contents.is_empty() && !new_contents.ends_with('\n') {
        new_contents.push('\n');
    }
    new_contents.push_str("# ais-runner: developer-local connection overrides\n");
    new_contents.push_str(FILENAME);
    new_contents.push('\n');
    std::fs::write(&path, new_contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deep_merge_replaces_leaf_strings_only_in_overridden_path() {
        let mut base = json!({
            "serviceProviderConnections": {
                "ais_sql": {
                    "displayName": "ais-sql",
                    "parameterValues": {
                        "connectionString": "@appsetting('SQL_CS')",
                        "authentication": { "type": "ManagedServiceIdentity" }
                    }
                }
            }
        });
        let ov = json!({
            "serviceProviderConnections": {
                "ais_sql": {
                    "parameterValues": {
                        "connectionString": "Server=localhost,1433;Database=ais;User Id=sa;Password=local!"
                    }
                }
            }
        });
        let touched = apply_overrides(&mut base, &ov);
        assert!(touched > 0);
        // Untouched fields preserved
        assert_eq!(
            base["serviceProviderConnections"]["ais_sql"]["displayName"],
            "ais-sql"
        );
        assert_eq!(
            base["serviceProviderConnections"]["ais_sql"]["parameterValues"]["authentication"]["type"],
            "ManagedServiceIdentity"
        );
        // Overridden field updated
        assert_eq!(
            base["serviceProviderConnections"]["ais_sql"]["parameterValues"]["connectionString"],
            "Server=localhost,1433;Database=ais;User Id=sa;Password=local!"
        );
    }

    #[test]
    fn null_in_override_deletes_key() {
        let mut base = json!({
            "serviceProviderConnections": {
                "cloud_only": { "displayName": "not for local" },
                "shared":      { "displayName": "keep me" }
            }
        });
        let ov = json!({
            "serviceProviderConnections": {
                "cloud_only": null
            }
        });
        apply_overrides(&mut base, &ov);
        assert!(base["serviceProviderConnections"].get("cloud_only").is_none());
        assert_eq!(base["serviceProviderConnections"]["shared"]["displayName"], "keep me");
    }

    #[test]
    fn arrays_are_replaced_not_merged() {
        let mut base = json!({ "audiences": ["a", "b", "c"] });
        let ov       = json!({ "audiences": ["x"] });
        apply_overrides(&mut base, &ov);
        assert_eq!(base["audiences"], json!(["x"]));
    }

    #[test]
    fn new_keys_in_override_are_added() {
        let mut base = json!({ "serviceProviderConnections": {} });
        let ov = json!({
            "serviceProviderConnections": {
                "ais_mock_sftp": { "displayName": "mock" }
            }
        });
        apply_overrides(&mut base, &ov);
        assert_eq!(
            base["serviceProviderConnections"]["ais_mock_sftp"]["displayName"],
            "mock"
        );
    }

    #[test]
    fn scaffold_creates_template_and_patches_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // Seed a connections.json so the template mirrors its connection keys.
        std::fs::write(dir.join("connections.json"), serde_json::to_string_pretty(&json!({
            "serviceProviderConnections": {
                "ais_sql":    {},
                "ais_cosmos": {},
            }
        })).unwrap()).unwrap();
        // Seed an existing .gitignore with unrelated entries.
        std::fs::write(dir.join(".gitignore"), "node_modules/\nlocal.settings.json\n").unwrap();

        let path = scaffold_override_file(dir).unwrap();
        assert!(path.exists());
        let text = std::fs::read_to_string(&path).unwrap();
        // Parses as-is: no comment header to strip. The previous template led
        // with `//` lines, which made the file unreadable to load_overrides.
        let body: serde_json::Value = serde_json::from_str(&text)
            .expect("scaffolded template must be valid JSON");
        assert!(body.get("_readme").is_some(), "guidance should ship as data");
        // Examples list both seeded connections
        let examples = &body["_examples"]["serviceProviderConnections"];
        assert!(examples.get("ais_sql").is_some());
        assert!(examples.get("ais_cosmos").is_some());

        // .gitignore got the entry added, untouched lines preserved
        let gi = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(gi.contains("node_modules/"), "preserved entry missing");
        assert!(gi.contains(FILENAME), "override filename not added");
    }

    #[test]
    fn scaffold_is_idempotent_when_file_already_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let target = dir.join(FILENAME);
        std::fs::write(&target, r#"{"serviceProviderConnections":{"keep":{}}}"#).unwrap();
        scaffold_override_file(dir).unwrap();
        // Existing file preserved — scaffold doesn't overwrite user content.
        let text = std::fs::read_to_string(&target).unwrap();
        assert!(text.contains("\"keep\""));
    }

    #[test]
    fn summary_counts_per_section() {
        let ov = json!({
            "serviceProviderConnections": { "a": {}, "b": {} },
            "managedApiConnections": { "c": {} }
        });
        let s = override_summary(&ov);
        assert_eq!(s.service_provider, 2);
        assert_eq!(s.managed_api,      1);
        assert_eq!(s.total(),          3);
    }

    #[test]
    fn the_scaffolded_file_round_trips_through_our_own_loader() {
        // Regression: the template used to start with `//` comment lines while
        // load_overrides parses with strict serde_json — so scaffolding
        // produced a file the very next check rejected as invalid JSON.
        let dir = std::env::temp_dir().join(format!("ais-cl-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("connections.json"),
            r#"{ "serviceProviderConnections": { "ais_sql": {}, "ais_blob": {} } }"#,
        )
        .unwrap();

        scaffold_override_file(&dir).unwrap();
        let loaded = load_overrides(&dir).expect("scaffolded file must parse");
        assert!(loaded.is_some(), "scaffolded file must not read as absent");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn documentation_keys_are_never_merged_into_connections_json() {
        // The scaffold ships `_readme` / `_examples`; merging them would write
        // commentary into the project's own connections.json.
        let mut base = serde_json::json!({ "serviceProviderConnections": { "ais_sql": { "a": 1 } } });
        let ov = serde_json::json!({
            "_readme":  ["ignore me"],
            "_examples": { "serviceProviderConnections": { "ais_sql": { "a": 999 } } },
            "serviceProviderConnections": { "ais_sql": { "a": 2 } }
        });
        apply_overrides(&mut base, &ov);

        assert!(base.get("_readme").is_none());
        assert!(base.get("_examples").is_none());
        assert_eq!(base["serviceProviderConnections"]["ais_sql"]["a"], 2);
    }
}
