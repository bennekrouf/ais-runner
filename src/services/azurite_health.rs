//! Startup integrity check for Azurite's on-disk state.
//!
//! Runs before the window opens, so a workspace that was left in a broken
//! state comes up usable instead of failing later with a 60 s test timeout and
//! a "runtime state is missing" message.
//!
//! Only the checks that are meaningful *before* anything is running live here.
//! Azurite is not up yet at that point and neither is func, so port probes and
//! "func is holding stale handles" comparisons say nothing — they belong to the
//! runtime diagnostics in `workflows.rs`. What is decidable from the files
//! alone is whether the table database still parses, and whether it was written
//! under a different site identity than the one we will use now.
//!
//! Deliberately NOT treated as corruption: a low count of `…runs` tables.
//! Those are created lazily on a workflow's first run, so "few run tables"
//! is the normal state of a fresh workspace, not damage — checking it would
//! wipe good data on nearly every cold start.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Findings from the pre-window check, held until the log panel exists to
/// show them. The check has to run before Dioxus launches, so there is no
/// signal to write to at the time it produces its output.
static STARTUP_NOTES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

fn notes() -> &'static Mutex<Vec<String>> {
    STARTUP_NOTES.get_or_init(|| Mutex::new(Vec::new()))
}

pub fn record_startup_note(line: String) {
    if let Ok(mut g) = notes().lock() {
        g.push(line);
    }
}

/// Drain the recorded notes. Returns them once; later calls see an empty list,
/// so a re-render cannot duplicate them in the log.
pub fn take_startup_notes() -> Vec<String> {
    notes().lock().map(|mut g| std::mem::take(&mut *g)).unwrap_or_default()
}

/// Azurite writes its table service to this LokiJS database.
const TABLE_DB: &str = "__azurite_db_table__.json";
const BLOB_DB: &str = "__azurite_db_blob__.json";
const DEBUG_LOG: &str = "debug.log";

/// Above this, `debug.log` is truncated at startup. A full disk causes exactly
/// the partial writes that corrupt the table database, so this is prevention,
/// not tidiness. Azurite logs every request, so this file reached 353 MB in a
/// single working session — 5 MB keeps enough recent history to debug with.
const MAX_DEBUG_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// Table-name segments that identify what a table is *for*, as opposed to
/// which site owns it. Longest first: `flowsubscriptionsummary` also ends with
/// a shorter kind name, and stripping the short one first would leave debris.
const TABLE_KINDS: &[&str] = &[
    "flowsubscriptionsummary",
    "flowsubscriptions",
    "flowruntimecontext",
    "flowaccesskeys",
    "jobdefinitions",
    "jobtriggers",
    "histories",
    "actions",
    "flows",
    "runs",
];

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Nothing on disk yet, or everything parses and belongs to one site.
    Healthy,
    /// The state cannot be used as-is; `reason` is shown to the user.
    Corrupt { reason: String },
}

/// Extract the site-identity prefix from an Azurite table collection name.
///
/// Names look like `devstoreaccount1$flow<site>cu00<run>runs` or
/// `devstoreaccount1$flow<site>flows`. Returns `None` for Azurite's own
/// bookkeeping collections (`$TABLES_COLLECTION$` and friends).
///
/// The site hash is not split off by scanning hex digits: a hash ending in `f`
/// runs straight into the `flows` suffix, and `cu00…` starts with `c`, so a
/// greedy hex scan yields a *different* prefix for two tables of the same site
/// — which would look exactly like the orphaned-site case this detects.
fn site_prefix(collection: &str) -> Option<String> {
    let rest = collection.strip_prefix("devstoreaccount1$")?;
    if !rest.starts_with("flow") {
        return None;
    }
    // Per-run tables carry a `cu00…` segment; everything after it is run scope.
    let mut s = match rest.find("cu00") {
        Some(i) => &rest[..i],
        None => rest,
    };
    // What remains may still end in a table-kind word.
    loop {
        let before = s;
        for kind in TABLE_KINDS {
            if let Some(trimmed) = s.strip_suffix(kind) {
                if !trimmed.is_empty() {
                    s = trimmed;
                    break;
                }
            }
        }
        if s == before {
            break;
        }
    }
    if s.len() > "flow".len() {
        Some(s.to_string())
    } else {
        None
    }
}

/// Decide whether the on-disk state is usable. Pure over the directory contents.
pub fn inspect(dir: &Path) -> Verdict {
    let table_db = dir.join(TABLE_DB);
    if !table_db.exists() {
        // Fresh workspace (or already wiped) — nothing to repair.
        return Verdict::Healthy;
    }

    let raw = match std::fs::read_to_string(&table_db) {
        Ok(r) => r,
        Err(e) => {
            return Verdict::Corrupt {
                reason: format!("{TABLE_DB} could not be read ({e})"),
            }
        }
    };

    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            return Verdict::Corrupt {
                reason: format!(
                    "{TABLE_DB} is not valid JSON ({e}) — usually a write cut short by a crash or a full disk"
                ),
            }
        }
    };

    let collections = match parsed.get("collections").and_then(|c| c.as_array()) {
        Some(c) => c,
        None => {
            return Verdict::Corrupt {
                reason: format!("{TABLE_DB} has no 'collections' array"),
            }
        }
    };

    // An empty table database next to populated blob storage means the tables
    // were lost while the rest of the state survived — func would start, list
    // its workflows, and then find no run history for any of them.
    if collections.is_empty() && dir.join(BLOB_DB).exists() {
        return Verdict::Corrupt {
            reason: "table storage is empty while blob storage still holds data".into(),
        };
    }

    let mut prefixes: Vec<String> = collections
        .iter()
        .filter_map(|c| c.get("name").and_then(|n| n.as_str()))
        .filter_map(site_prefix)
        .collect();
    prefixes.sort();
    prefixes.dedup();

    if prefixes.len() > 1 {
        return Verdict::Corrupt {
            reason: format!(
                "table storage holds {} different site identities ({}) — run history written \
                 under one is invisible to the other",
                prefixes.len(),
                prefixes.join(", ")
            ),
        };
    }

    Verdict::Healthy
}

/// Workflows func has registered that Azurite never provisioned.
#[derive(Debug, Clone, PartialEq)]
pub struct ProvisioningGap {
    pub registered: usize,
    pub provisioned: usize,
    pub missing: Vec<String>,
}

/// Workflow names present in Azurite's table storage.
///
/// Every registered workflow gets a `flows` row at provisioning time, so this
/// is a complete list of what the runtime actually has state for — unlike the
/// `…runs` tables, which appear only on a workflow's first execution and say
/// nothing about provisioning.
pub fn provisioned_flow_names(dir: &Path) -> BTreeSet<String> {
    let raw = match std::fs::read_to_string(dir.join(TABLE_DB)) {
        Ok(r) => r,
        Err(_) => return BTreeSet::new(),
    };
    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return BTreeSet::new(),
    };
    let mut out = BTreeSet::new();
    if let Some(cols) = parsed.get("collections").and_then(|c| c.as_array()) {
        for col in cols {
            let Some(rows) = col.get("data").and_then(|d| d.as_array()) else { continue };
            for row in rows {
                if let Some(name) = row
                    .get("properties")
                    .and_then(|p| p.get("FlowName"))
                    .and_then(|n| n.as_str())
                {
                    out.insert(name.to_string());
                }
            }
        }
    }
    out
}

/// Compare what func registered against what Azurite holds state for.
///
/// The failure this catches: provisioning stops part-way through a start and
/// never resumes, because later starts see existing state and consider the job
/// done. The affected workflows are registered — the management API lists them,
/// and their triggers look configured — but they have no runtime state, so they
/// never fire and `/runs` answers `WorkflowNotFound`. From the outside that is
/// indistinguishable from a workflow that simply has not run yet, which is why
/// it survives restarts unnoticed.
///
/// Returns `None` when there is nothing to compare (no registered list yet, or
/// table storage not written at all — a workspace where func has never run is
/// not "broken").
pub fn provisioning_gap(dir: &Path, registered: &[String]) -> Option<ProvisioningGap> {
    if registered.is_empty() {
        return None;
    }
    let provisioned = provisioned_flow_names(dir);
    if provisioned.is_empty() {
        return None;
    }
    let missing: Vec<String> = registered
        .iter()
        .filter(|n| !provisioned.contains(*n))
        .cloned()
        .collect();
    if missing.is_empty() {
        return None;
    }
    Some(ProvisioningGap {
        registered: registered.len(),
        provisioned: provisioned.len(),
        missing,
    })
}

/// Delete everything under `dir`, leaving the directory itself in place.
/// Returns how many entries were removed.
pub fn repair(dir: &Path) -> std::io::Result<usize> {
    std::fs::create_dir_all(dir)?;
    let mut n = 0usize;
    for entry in std::fs::read_dir(dir)?.flatten() {
        let p = entry.path();
        if p.is_dir() {
            std::fs::remove_dir_all(&p)?;
        } else {
            std::fs::remove_file(&p)?;
        }
        n += 1;
    }
    Ok(n)
}

/// Truncate an oversized `debug.log`. Returns its previous size when truncated.
pub fn trim_debug_log(dir: &Path) -> Option<u64> {
    let log = dir.join(DEBUG_LOG);
    let size = std::fs::metadata(&log).ok()?.len();
    if size <= MAX_DEBUG_LOG_BYTES {
        return None;
    }
    std::fs::write(&log, b"").ok()?;
    Some(size)
}

/// Startup entry point: inspect, repair if needed, and return lines to show
/// once the UI exists. Never panics and never blocks on the network — a
/// failure to repair is reported, not fatal, so the app still opens.
pub fn startup_check(dir: &PathBuf) -> Vec<String> {
    let mut out = Vec::new();

    if let Some(was) = trim_debug_log(dir) {
        out.push(format!(
            "Azurite debug.log was {} MB — truncated (max {} MB) to keep the disk from filling.",
            was / (1024 * 1024),
            MAX_DEBUG_LOG_BYTES / (1024 * 1024),
        ));
    }

    match inspect(dir) {
        Verdict::Healthy => {}
        Verdict::Corrupt { reason } => match repair(dir) {
            Ok(n) => out.push(format!(
                "⟳ Azurite state was unusable ({reason}). Wiped {n} item(s) at startup — \
                 it will come up clean."
            )),
            Err(e) => out.push(format!(
                "⚠ Azurite state is unusable ({reason}) and could not be wiped automatically ({e}). \
                 Click ⟳ Reset next to Azurite."
            )),
        },
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(collections: &str) -> String {
        format!(r#"{{"filename":"x","collections":[{collections}]}}"#)
    }

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), body).unwrap();
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ais-azhealth-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    // ── site_prefix ────────────────────────────────────────────────────────
    // Every table kind seen in a real workspace must reduce to the same site.

    #[test]
    fn all_table_kinds_of_one_site_share_a_prefix() {
        let site = "flow8e478029a1a5452";
        for name in [
            "devstoreaccount1$flow8e478029a1a5452flows",
            "devstoreaccount1$flow8e478029a1a5452flowaccesskeys",
            "devstoreaccount1$flow8e478029a1a5452flowsubscriptions",
            "devstoreaccount1$flow8e478029a1a5452flowsubscriptionsummary",
            "devstoreaccount1$flow8e478029a1a5452flowruntimecontext",
            "devstoreaccount1$flow8e478029a1a5452jobdefinitionscu00",
            "devstoreaccount1$flow8e478029a1a5452cu00abcf691dce9855bflows",
            "devstoreaccount1$flow8e478029a1a5452cu00abcf691dce9855bruns",
            "devstoreaccount1$flow8e478029a1a5452cu00abcf691dce9855bhistories",
            "devstoreaccount1$flow8e478029a1a5452cu00abcf691dce9855b20260813t000000zactions",
        ] {
            assert_eq!(site_prefix(name).as_deref(), Some(site), "for {name}");
        }
    }

    #[test]
    fn azurite_bookkeeping_collections_are_not_sites() {
        assert_eq!(site_prefix("$TABLES_COLLECTION$"), None);
        assert_eq!(site_prefix("$SERVICES_COLLECTION$"), None);
    }

    /// The regression this guards: a site hash ending in `f` butts against the
    /// `flows` suffix, and `cu00` starts with a hex digit. A hex-scanning split
    /// reports two prefixes here and would wipe a perfectly good workspace.
    #[test]
    fn hash_ending_in_hex_letter_does_not_split_into_two_sites() {
        let a = site_prefix("devstoreaccount1$flowabcdefflows");
        let b = site_prefix("devstoreaccount1$flowabcdefcu00123runs");
        assert_eq!(a, b);
        assert_eq!(a.as_deref(), Some("flowabcdef"));
    }

    // ── inspect ────────────────────────────────────────────────────────────

    #[test]
    fn missing_database_is_healthy_not_corrupt() {
        let d = tmp("missing");
        assert_eq!(inspect(&d), Verdict::Healthy);
    }

    #[test]
    fn single_site_is_healthy() {
        let d = tmp("single");
        write(&d, TABLE_DB, &db(
            r#"{"name":"devstoreaccount1$flow8e478029a1a5452flows"},
               {"name":"devstoreaccount1$flow8e478029a1a5452cu00abcruns"}"#,
        ));
        assert_eq!(inspect(&d), Verdict::Healthy);
    }

    #[test]
    fn truncated_json_is_corrupt() {
        let d = tmp("truncated");
        write(&d, TABLE_DB, r#"{"collections":[{"name":"devstore"#);
        assert!(matches!(inspect(&d), Verdict::Corrupt { .. }));
    }

    #[test]
    fn two_site_identities_are_corrupt() {
        let d = tmp("twosites");
        write(&d, TABLE_DB, &db(
            r#"{"name":"devstoreaccount1$flow8e478029a1a5452flows"},
               {"name":"devstoreaccount1$flow99998029a1a5452flows"}"#,
        ));
        match inspect(&d) {
            Verdict::Corrupt { reason } => assert!(reason.contains("site identities"), "{reason}"),
            v => panic!("expected corrupt, got {v:?}"),
        }
    }

    #[test]
    fn empty_tables_beside_populated_blobs_is_corrupt() {
        let d = tmp("emptytables");
        write(&d, TABLE_DB, &db(""));
        write(&d, BLOB_DB, "{}");
        assert!(matches!(inspect(&d), Verdict::Corrupt { .. }));
    }

    /// A brand-new workspace has an empty table db and no blob db yet; wiping
    /// there would be a pointless scare on first launch.
    #[test]
    fn empty_tables_without_blobs_is_healthy() {
        let d = tmp("emptyfresh");
        write(&d, TABLE_DB, &db(""));
        assert_eq!(inspect(&d), Verdict::Healthy);
    }

    // ── repair / startup_check ─────────────────────────────────────────────

    #[test]
    fn repair_empties_the_directory() {
        let d = tmp("repair");
        write(&d, TABLE_DB, "{}");
        std::fs::create_dir_all(d.join("__blobstorage__")).unwrap();
        let n = repair(&d).unwrap();
        assert_eq!(n, 2);
        assert!(d.exists());
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
    }

    #[test]
    fn startup_check_repairs_corrupt_state_and_reports_it() {
        let d = tmp("startup");
        write(&d, TABLE_DB, r#"{"collections":[{"nam"#);
        let msgs = startup_check(&d);
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert!(msgs[0].contains("unusable"), "{}", msgs[0]);
        assert_eq!(inspect(&d), Verdict::Healthy, "state must be usable afterwards");
    }

    // ── provisioning gap ───────────────────────────────────────────────────

    fn flows_db(names: &[&str]) -> String {
        let rows: Vec<String> = names
            .iter()
            .map(|n| format!(r#"{{"properties":{{"FlowName":"{n}","State":"Enabled"}}}}"#))
            .collect();
        format!(
            r#"{{"collections":[{{"name":"devstoreaccount1$flowabcflows","data":[{}]}}]}}"#,
            rows.join(",")
        )
    }

    #[test]
    fn no_gap_when_everything_is_provisioned() {
        let d = tmp("gap-none");
        write(&d, TABLE_DB, &flows_db(&["A", "B"]));
        assert_eq!(provisioning_gap(&d, &["A".into(), "B".into()]), None);
    }

    /// The real shape of the failure: func lists 51 workflows, Azurite has
    /// state for 15, and everything else silently never runs.
    #[test]
    fn reports_workflows_registered_but_never_provisioned() {
        let d = tmp("gap-partial");
        write(&d, TABLE_DB, &flows_db(&["AIS-GenericCatch", "Test-AppConfig"]));
        let registered = vec![
            "AIS-GenericCatch".to_string(),
            "Test-AppConfig".to_string(),
            "Check-Ignite-Payment-File".to_string(),
            "Send-Kyriba-files".to_string(),
        ];
        let gap = provisioning_gap(&d, &registered).expect("gap expected");
        assert_eq!(gap.registered, 4);
        assert_eq!(gap.provisioned, 2);
        assert_eq!(gap.missing, vec!["Check-Ignite-Payment-File", "Send-Kyriba-files"]);
    }

    /// A workspace where func has never started has no table storage at all.
    /// Reporting "36 workflows missing" there would be pure noise.
    #[test]
    fn no_gap_reported_before_func_has_ever_provisioned() {
        let d = tmp("gap-fresh");
        assert_eq!(provisioning_gap(&d, &["A".into()]), None);
        write(&d, TABLE_DB, &db(""));
        assert_eq!(provisioning_gap(&d, &["A".into()]), None);
    }

    #[test]
    fn no_gap_without_a_registered_list() {
        let d = tmp("gap-noreg");
        write(&d, TABLE_DB, &flows_db(&["A"]));
        assert_eq!(provisioning_gap(&d, &[]), None);
    }

    /// FlowName also appears in the per-workflow `cu00…flows` tables; a name
    /// found anywhere counts as provisioned, and must not be double-counted.
    #[test]
    fn flow_names_are_collected_across_tables_and_deduplicated() {
        let d = tmp("gap-dedup");
        write(&d, TABLE_DB, &format!(
            r#"{{"collections":[
                {{"name":"devstoreaccount1$flowabcflows","data":[
                    {{"properties":{{"FlowName":"A"}}}}]}},
                {{"name":"devstoreaccount1$flowabccu0012flows","data":[
                    {{"properties":{{"FlowName":"A"}}}},
                    {{"properties":{{"FlowName":"B"}}}}]}}
            ]}}"#
        ));
        let names = provisioned_flow_names(&d);
        assert_eq!(names.len(), 2);
        assert!(names.contains("A") && names.contains("B"));
    }

    #[test]
    fn oversized_debug_log_is_truncated_but_kept() {
        let d = tmp("biglog");
        std::fs::write(d.join(DEBUG_LOG), vec![b'x'; (MAX_DEBUG_LOG_BYTES + 1) as usize]).unwrap();
        let was = trim_debug_log(&d).expect("should truncate");
        assert!(was > MAX_DEBUG_LOG_BYTES);
        assert!(d.join(DEBUG_LOG).exists(), "Azurite keeps writing to it — must not be deleted");
        assert_eq!(std::fs::metadata(d.join(DEBUG_LOG)).unwrap().len(), 0);
    }

    #[test]
    fn debug_log_under_the_limit_is_left_alone() {
        let d = tmp("smalllog");
        std::fs::write(d.join(DEBUG_LOG), b"recent history worth keeping").unwrap();
        assert_eq!(trim_debug_log(&d), None);
        assert_eq!(std::fs::metadata(d.join(DEBUG_LOG)).unwrap().len(), 28);
    }

    #[test]
    fn startup_check_is_silent_on_a_healthy_workspace() {
        let d = tmp("silent");
        write(&d, TABLE_DB, &db(r#"{"name":"devstoreaccount1$flow8e478029a1a5452flows"}"#));
        assert!(startup_check(&d).is_empty());
    }
}
