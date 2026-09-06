//! Cross-project scan for `@appsetting('X')` references that have no
//! corresponding entry in `local.settings.json`.
//!
//! The existing `setup_manager::check_setup` only scans `connections.json`.
//! In practice missing-key errors more often come from the workflows
//! themselves — an HTTP action's `uri` template uses `@{parameters('Erp_Url')}`,
//! a SQL action's `connectionString` resolves through an `@appsetting('X')`,
//! a Service Bus queue name is a literal `@appsetting('Y')` — and when one of
//! those keys is absent the runtime spits a generic "param NULL" error
//! buried under cascading validation messages.
//!
//! This module walks every `workflow.json` under the project, plus the
//! parameters.json / connections.json files at the project root, harvests
//! every `@appsetting('K')` reference, cross-references against the local
//! settings, and returns a structured report grouping each missing key by
//! every workflow / file that referenced it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::services::workflows;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSetting {
    /// The app-setting key the workflow expects but `local.settings.json`
    /// doesn't define. Stripped of the `@appsetting('…')` envelope.
    pub key: String,
    /// Every workflow / file that references this key, sorted alphabetically
    /// so the report is stable across runs. Workflow entries are bare names
    /// (e.g. `Verify-Acme-Trade`); root-level files are surfaced with their
    /// filename (`connections.json`, `parameters.json`).
    pub referenced_by: Vec<String>,
}

/// Walk the project's Logic Apps directory and return every `@appsetting('K')`
/// reference whose key is missing from `local.settings.json`. Empty vec means
/// "everything resolves".
pub fn scan(project_dir: &str) -> Vec<MissingSetting> {
    let logic = workflows::resolve_logic_apps_dir(project_dir);

    // Settings already defined locally — used as the "is this key missing?" filter.
    let local_keys = load_local_keys(&logic);

    // key → set of (workflow_or_file) names. BTree to keep both deterministic.
    let mut refs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    let mut record = |key: String, source: String| {
        if local_keys.contains(&key) {
            return;
        }
        refs.entry(key).or_default().insert(source);
    };

    // ── Walk every workflow.json under the logic-apps dir ─────────────────
    if let Ok(entries) = std::fs::read_dir(&logic) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let wf_file = path.join("workflow.json");
            if !wf_file.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Ok(text) = std::fs::read_to_string(&wf_file) {
                for key in extract_appsetting_keys(&text) {
                    record(key, name.to_string());
                }
            }
        }
    }

    // ── Root-level files at the same level as host.json ───────────────────
    for fname in &["connections.json", "parameters.json", "host.json"] {
        let path = logic.join(fname);
        if let Ok(text) = std::fs::read_to_string(&path) {
            for key in extract_appsetting_keys(&text) {
                record(key, (*fname).to_string());
            }
        }
    }

    refs.into_iter()
        .map(|(key, sources)| MissingSetting {
            key,
            referenced_by: sources.into_iter().collect(),
        })
        .collect()
}

/// Read the keys defined under `Values:` in `local.settings.json`. Returns an
/// empty set on any read/parse failure — a fresh project with no settings
/// file should show **every** referenced key as missing, not silently report
/// nothing.
fn load_local_keys(logic_dir: &Path) -> BTreeSet<String> {
    let path = logic_dir.join("local.settings.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return BTreeSet::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return BTreeSet::new();
    };
    v.get("Values")
        .and_then(|x| x.as_object())
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

/// Pull every `@appsetting('KEY')`, `@{appsetting('KEY')}`, and double-quoted
/// `@appsetting("KEY")` reference out of a blob of JSON or text. Deduplicated
/// per scan-site by the BTreeMap upstream, so callers don't need to dedup.
fn extract_appsetting_keys(text: &str) -> Vec<String> {
    // Two-pass scan, single-quote and double-quote envelopes. The braced form
    // (`@{appsetting('K')}`) is a strict superset of the bare form because we
    // only match what's inside the parens.
    let mut out: Vec<String> = Vec::new();
    let mut push = |k: &str| {
        out.push(k.to_string());
    };

    for re in [
        r"@\{?appsetting\('([^']+)'\)\}?",
        r#"@\{?appsetting\("([^"]+)"\)\}?"#,
    ] {
        if let Ok(re) = regex::Regex::new(re) {
            for cap in re.captures_iter(text) {
                if let Some(m) = cap.get(1) {
                    push(m.as_str());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Build a tiny project tree under `dir` and return its path as a string
    /// the way `workflows::resolve_logic_apps_dir` would consume it.
    fn make_project(
        values: &[(&str, &str)],
        workflows_text: &[(&str, &str)],
        connections: Option<&str>,
    ) -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        // Logic Apps Standard's standard layout: workflow folders + settings live
        // at the root the user picks (no `logic_apps/` subfolder).
        let mut vals_map = serde_json::Map::new();
        for (k, v) in values {
            vals_map.insert(
                (*k).to_string(),
                serde_json::Value::String((*v).to_string()),
            );
        }
        let local_settings = serde_json::json!({
            "IsEncrypted": false,
            "Values": vals_map,
        });
        fs::write(
            dir.path().join("local.settings.json"),
            serde_json::to_string_pretty(&local_settings).unwrap(),
        )
        .unwrap();
        for (wf_name, body) in workflows_text {
            let wf_dir = dir.path().join(wf_name);
            fs::create_dir_all(&wf_dir).unwrap();
            fs::write(wf_dir.join("workflow.json"), body).unwrap();
        }
        if let Some(c) = connections {
            fs::write(dir.path().join("connections.json"), c).unwrap();
        }
        dir
    }

    #[test]
    fn returns_empty_when_every_key_is_defined() {
        let tmp = make_project(
            &[("Erp_Url", "https://erp")],
            &[(
                "Wf",
                r#"{"definition":{"actions":{"H":{"type":"Http","inputs":{"uri":"@{appsetting('Erp_Url')}"}}}}}"#,
            )],
            None,
        );
        let r = scan(tmp.path().to_str().unwrap());
        assert!(r.is_empty(), "expected no missing keys, got {r:#?}");
    }

    #[test]
    fn surfaces_each_missing_key_with_referencing_workflow() {
        let tmp = make_project(
            &[], // no values
            &[(
                "Wf",
                r#"{"definition":{"actions":{"H":{"type":"Http","inputs":{"uri":"@{appsetting('Erp_Url')}"}}}}}"#,
            )],
            None,
        );
        let r = scan(tmp.path().to_str().unwrap());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].key, "Erp_Url");
        assert_eq!(r[0].referenced_by, vec!["Wf".to_string()]);
    }

    #[test]
    fn groups_a_key_referenced_by_multiple_workflows() {
        let body = r#"{"definition":{"actions":{"H":{"type":"Http","inputs":{"uri":"@{appsetting('Shared_Key')}"}}}}}"#;
        let tmp = make_project(&[], &[("Wf_A", body), ("Wf_B", body)], None);
        let r = scan(tmp.path().to_str().unwrap());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].key, "Shared_Key");
        // Sources are alphabetised by the BTreeSet so the report is stable.
        assert_eq!(
            r[0].referenced_by,
            vec!["Wf_A".to_string(), "Wf_B".to_string()]
        );
    }

    #[test]
    fn picks_up_connections_json_references_at_root() {
        let conn =
            r#"{"sb": {"parameterValues": {"connectionString": "@appsetting('SB_ConnStr')"}}}"#;
        let tmp = make_project(&[], &[], Some(conn));
        let r = scan(tmp.path().to_str().unwrap());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].key, "SB_ConnStr");
        assert_eq!(r[0].referenced_by, vec!["connections.json".to_string()]);
    }

    #[test]
    fn handles_both_braced_and_bare_single_quoted_forms() {
        // Single quotes are the canonical form inside Logic Apps expressions
        // (e.g. `@appsetting('X')` or `@{appsetting('X')}` interpolated
        // inside a string value). Double-quoted `@appsetting("X")` inside a
        // JSON file would require escaping the `"` and is vanishingly rare
        // in practice — the regex still accepts it on the unescaped path,
        // which we don't unit-test here because constructing a representative
        // input would mean fighting JSON escaping rather than testing intent.
        let body = r#"{"definition":{"x":"@appsetting('A')","y":"@{appsetting('B')}"}}"#;
        let tmp = make_project(&[], &[("Wf", body)], None);
        let r = scan(tmp.path().to_str().unwrap());
        let keys: Vec<&str> = r.iter().map(|m| m.key.as_str()).collect();
        assert_eq!(keys, vec!["A", "B"]);
    }

    #[test]
    fn missing_local_settings_treats_every_reference_as_missing() {
        let dir = tempdir().unwrap();
        let wf_dir = dir.path().join("Wf");
        fs::create_dir_all(&wf_dir).unwrap();
        fs::write(
            wf_dir.join("workflow.json"),
            r#"{"x":"@appsetting('Anything')"}"#,
        )
        .unwrap();
        let r = scan(dir.path().to_str().unwrap());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].key, "Anything");
    }
}
