//! Strip `ActiveDirectoryOAuth` from workflow HTTP actions for local runs.
//!
//! Workflows that call Acme/ERP authenticate with:
//!
//! ```json
//! "authentication": {
//!     "type": "ActiveDirectoryOAuth",
//!     "tenant": "@{parameters('PartnerTenantId')}",
//!     "clientId": "@{parameters('AcmeClientId')}",
//!     "secret": "@{parameters('AcmeSecret')}"
//! }
//! ```
//!
//! None of that can work locally. The parameters resolve through
//! `@appsetting()`, and those keys are absent from `local.settings.json` — so
//! the action fails before it ever reaches the stub:
//!
//! ```text
//! Execute_Strategy_stored_procedure  Failed
//!   The required OAuth authentication property 'tenant' is missing.
//! ```
//!
//! Filling the keys in is worse, not better: with real values the runtime
//! would fetch a real token from AAD (a network call, and a live secret
//! sitting in a developer's working tree) purely to send an `Authorization`
//! header that a localhost stub discards. With fake values the token request
//! fails and the action still dies.
//!
//! So the block is removed while func runs. Same contract as
//! [`crate::services::connections_snapshot`]: snapshot the pristine file
//! first, restore it on stop, and detect "already patched" through the
//! patch's own idempotence rather than a marker.
//!
//! The edit is textual, not a JSON round-trip. Re-serializing would reindent
//! whole files and bury the developer in a diff they never made.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::services::cache_dir;
use crate::services::net;

const CACHE_SUBDIR: &str = "workflows";
const OAUTH: &str = "ActiveDirectoryOAuth";
const KEY: &str = "\"authentication\"";

fn patched_dirs() -> &'static Mutex<HashSet<PathBuf>> {
    static DIRS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    DIRS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn cache_dir(logic_apps_dir: &Path) -> PathBuf {
    cache_dir::root(logic_apps_dir).join(CACHE_SUBDIR)
}

fn backup_path(logic_apps_dir: &Path, workflow: &str) -> PathBuf {
    cache_dir(logic_apps_dir).join(format!("{workflow}.workflow.json.original"))
}

/// Byte offset of the `{` that opens this member's value, when the value is an
/// object. `from` is the offset just past the key. `None` for every other value
/// shape — a string, a number, `null` — which is what keeps the brace matcher
/// from wandering into the next action.
fn object_value_at(raw: &str, from: usize) -> Option<usize> {
    let bytes = raw.as_bytes();
    let mut k = from;
    while bytes.get(k).is_some_and(u8::is_ascii_whitespace) {
        k += 1;
    }
    if bytes.get(k)? != &b':' {
        return None;
    }
    k += 1;
    while bytes.get(k).is_some_and(u8::is_ascii_whitespace) {
        k += 1;
    }
    (bytes.get(k)? == &b'{').then_some(k)
}

/// Remove every `"authentication": { … "ActiveDirectoryOAuth" … }` member.
///
/// Returns the text unchanged when there is nothing to strip, so
/// `strip(x) == x` doubles as the "already patched" test.
pub fn strip(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;

    while let Some(rel) = raw[i..].find(KEY) {
        let start = i + rel;
        // The value has to be an object *right here*. `"authentication":
        // "@parameters('$authentication')"` is a documented shape, and an
        // unbounded search for the next '{' would run straight past it and
        // brace-match a later action's object instead — deleting every action
        // in between and leaving invalid JSON behind.
        let Some(open) = object_value_at(raw, start + KEY.len()) else {
            out.push_str(&raw[i..start + 1]);
            i = start + 1;
            continue;
        };
        let mut depth = 0usize;
        let mut end = open;
        let mut in_str = false;
        let mut esc = false;
        for (k, ch) in raw[open..].char_indices() {
            if esc {
                esc = false;
                continue;
            }
            match ch {
                '\\' if in_str => esc = true,
                '"' => in_str = !in_str,
                '{' if !in_str => depth += 1,
                '}' if !in_str => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + k + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        if depth != 0 || !raw[open..end].contains(OAUTH) {
            // Unbalanced, or some other auth scheme we must not touch.
            out.push_str(&raw[i..end.max(start + 1)]);
            i = end.max(start + 1);
            continue;
        }

        // Drop the separating comma too, whichever side carries it, so the
        // surrounding object stays valid JSON.
        let mut cut_from = start;
        let mut cut_to = end;
        let before = bytes[..start]
            .iter()
            .rposition(|b| !b.is_ascii_whitespace());
        if let Some(p) = before.filter(|p| bytes[*p] == b',') {
            // Never reach back past text already emitted. Two `"authentication"`
            // members separated by one comma would otherwise both claim it —
            // the second producing a backwards range, which panics.
            cut_from = p.max(i);
        } else if let Some(p) = bytes[end..].iter().position(|b| !b.is_ascii_whitespace()) {
            if bytes[end + p] == b',' {
                cut_to = end + p + 1;
            }
        }
        out.push_str(&raw[i..cut_from]);
        i = cut_to;
    }
    out.push_str(&raw[i..]);
    out
}

/// Patch every `*/workflow.json` under `logic_apps_dir`, snapshotting first.
/// Returns the workflow names that changed.
pub fn patch_all(logic_apps_dir: &Path) -> std::io::Result<Vec<String>> {
    let mut patched = Vec::new();
    let Ok(entries) = std::fs::read_dir(logic_apps_dir) else {
        return Ok(patched);
    };
    for entry in entries.flatten() {
        let wf = entry.path().join("workflow.json");
        if !wf.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let raw = std::fs::read_to_string(&wf)?;
        let stripped = strip(&raw);
        if stripped == raw {
            // Either nothing to strip, or already stripped by an earlier run
            // that exited without restoring — re-register the latter so this
            // session's teardown still puts it back.
            if backup_path(logic_apps_dir, &name).exists() {
                register(logic_apps_dir);
            }
            continue;
        }
        cache_dir::ensure(logic_apps_dir, &cache_dir(logic_apps_dir))?;
        let backup = backup_path(logic_apps_dir, &name);
        if !backup.exists() {
            std::fs::write(&backup, &raw)?;
        }
        std::fs::write(&wf, stripped)?;
        register(logic_apps_dir);
        patched.push(name);
    }
    patched.sort();
    Ok(patched)
}

fn register(logic_apps_dir: &Path) {
    if let Ok(mut dirs) = patched_dirs().lock() {
        dirs.insert(logic_apps_dir.to_path_buf());
    }
}

/// What a restore pass did with the snapshots it found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    /// Files put back the way the developer had them.
    pub restored: usize,
    /// Workflows left untouched because the file on disk is not the patch we
    /// wrote — edited, pulled, or replaced since. Their snapshot is kept.
    ///
    /// A snapshot records what we changed; it is not a licence to overwrite
    /// whatever happens to be there now. Without this check, a crash that left
    /// `.ais-cache/workflows` behind turns the next project open into a silent
    /// `git checkout` of the developer's work.
    pub foreign: Vec<String>,
    /// Per-file failures, as (workflow, reason). The pass keeps going: one
    /// unreadable snapshot must not strand every later file in its patched
    /// state.
    pub failed: Vec<(String, String)>,
}

impl RestoreReport {
    /// Anything a caller would want to tell the user about.
    pub fn is_quiet(&self) -> bool {
        self.restored == 0 && self.foreign.is_empty() && self.failed.is_empty()
    }
}

/// Put every snapshotted workflow.json back, skipping any the developer has
/// since changed.
pub fn restore(logic_apps_dir: &Path) -> RestoreReport {
    let dir = cache_dir(logic_apps_dir);
    let mut report = RestoreReport::default();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return report;
    };
    for entry in entries.flatten() {
        let backup = entry.path();
        let Some(name) = entry
            .file_name()
            .to_string_lossy()
            .strip_suffix(".workflow.json.original")
            .map(str::to_string)
        else {
            continue;
        };
        let original = match std::fs::read_to_string(&backup) {
            Ok(o) => o,
            Err(e) => {
                report.failed.push((name, e.to_string()));
                continue;
            }
        };
        let target = logic_apps_dir.join(&name).join("workflow.json");
        let current = std::fs::read_to_string(&target).ok();
        match current.as_deref() {
            // Already the way the developer had it.
            Some(c) if c == original => {}
            // Byte-for-byte what `patch_all` would have written from this
            // snapshot, so it is ours and ours alone to put back. Comparing
            // against `strip(original)` rather than testing the file for
            // "looks patched" is the whole point: a file with no OAuth block
            // in it is indistinguishable from a stripped one by inspection.
            Some(c) if strip(&original) == c => {
                if let Err(e) = std::fs::write(&target, &original) {
                    report.failed.push((name, e.to_string()));
                    continue;
                }
                report.restored += 1;
            }
            // Deleted while we held a snapshot of it — putting it back is
            // strictly better than leaving the project a workflow short.
            None => match std::fs::write(&target, &original) {
                Ok(()) => report.restored += 1,
                Err(e) => {
                    report.failed.push((name, e.to_string()));
                    continue;
                }
            },
            // Something we never wrote. Hands off, and keep the snapshot.
            Some(_) => {
                report.foreign.push(name);
                continue;
            }
        }
        let _ = std::fs::remove_file(&backup);
    }
    // Fails while any snapshot is still held back; that is the intent.
    let _ = std::fs::remove_dir(&dir);
    if report.foreign.is_empty() {
        if let Ok(mut dirs) = patched_dirs().lock() {
            dirs.remove(logic_apps_dir);
        }
    }
    report
}

/// Put back workflow.json files left patched by a session that never restored
/// them — a hard crash, a force-quit, or func left running after ais-runner
/// itself closed. `restore_all` only covers a clean exit from *this* process;
/// this covers opening a project and finding someone else's mess.
///
/// This is a safety net, not the primary mechanism: the panic hook and the
/// signal handler in `main` both call `restore_all`, so a crash normally cleans
/// up after itself and this finds nothing.
pub fn heal_stale_patch(logic_apps_dir: &Path) -> RestoreReport {
    heal_stale_patch_with(logic_apps_dir, net::is_listening(net::FUNC_PORT))
}

/// The testable core. `func_running` is injected so unit tests do not depend on
/// whatever happens to hold :7071 on the machine running them — which, on a
/// developer box working on this project, is usually func.
pub fn heal_stale_patch_with(logic_apps_dir: &Path, func_running: bool) -> RestoreReport {
    let report = RestoreReport::default();
    if !cache_dir(logic_apps_dir).is_dir() {
        return report;
    }
    if func_running {
        // Deferred, not skipped. A func host from an earlier session re-reads
        // these files on workflow reload, so swapping them now would change a
        // running workflow mid-flight. Registering hands the job to this
        // process's teardown, where `restore_all` picks it up on close.
        //
        // The probe cannot tell whose func that is; a host serving a different
        // project is a false positive that costs nothing but the deferral.
        register(logic_apps_dir);
        return report;
    }
    restore(logic_apps_dir)
}

/// Restore every directory patched in this session — teardown, where there is
/// no project directory to hand.
pub fn restore_all() -> usize {
    let dirs: Vec<PathBuf> = match patched_dirs().lock() {
        Ok(d) => d.iter().cloned().collect(),
        Err(_) => return 0,
    };
    dirs.iter().map(|d| restore(d).restored).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn serialised() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A `TempDir`, not a hand-rolled pid+nanos path: it cleans up even when
    /// the test panics. `tempfile` was already a dev-dependency.
    fn workspace() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// The exact scenario that motivated this function: a patched workflow
    /// with no live ais-runner process — func not listening, nothing in the
    /// registry (fresh test process). Opening the project must put it back.
    #[test]
    fn heal_stale_patch_restores_when_func_is_not_listening() {
        let _g = serialised();
        let tmp = workspace();
        let ws = tmp.path().to_path_buf();
        let wf_dir = ws.join("Send-Http-Get-Acme-AddressBook");
        std::fs::create_dir_all(&wf_dir).unwrap();
        let wf = wf_dir.join("workflow.json");
        std::fs::write(&wf, WF).unwrap();

        patch_all(&ws).unwrap();
        assert!(!std::fs::read_to_string(&wf).unwrap().contains(OAUTH));
        // simulate a crash: nothing registered for this dir in this process
        patched_dirs().lock().unwrap().remove(&ws);

        assert_eq!(heal_stale_patch_with(&ws, false).restored, 1);
        assert_eq!(std::fs::read_to_string(&wf).unwrap(), WF);
    }

    /// A func host from an earlier session is still reading these files, so the
    /// swap waits — but the directory is registered, or this session's teardown
    /// would skip it too and the patch would outlive us again.
    #[test]
    fn heal_stale_patch_defers_to_teardown_while_func_is_up() {
        let _g = serialised();
        let tmp = workspace();
        let ws = tmp.path().to_path_buf();
        std::fs::create_dir_all(ws.join("W")).unwrap();
        std::fs::write(ws.join("W/workflow.json"), WF).unwrap();
        patch_all(&ws).unwrap();
        patched_dirs().lock().unwrap().remove(&ws);

        let report = heal_stale_patch_with(&ws, true);
        assert!(report.is_quiet());
        assert!(!std::fs::read_to_string(ws.join("W/workflow.json"))
            .unwrap()
            .contains(OAUTH));
        assert!(
            patched_dirs().lock().unwrap().contains(&ws),
            "deferred, so teardown has to know about it"
        );
        restore(&ws);
    }

    #[test]
    fn heal_stale_patch_is_a_no_op_with_no_backup() {
        let _g = serialised();
        let tmp = workspace();
        assert!(heal_stale_patch_with(tmp.path(), false).is_quiet());
    }

    /// The data-loss case, and the reason `restore` compares against
    /// `strip(original)` instead of trusting the snapshot's existence. A crash
    /// leaves `.ais-cache/workflows` behind; the developer then does the
    /// obvious thing with 17 dirty files — `git checkout .` — and pulls. The
    /// next project open must not quietly revert what they pulled.
    #[test]
    fn a_file_we_did_not_patch_is_never_overwritten() {
        let _g = serialised();
        let tmp = workspace();
        let ws = tmp.path().to_path_buf();
        std::fs::create_dir_all(ws.join("W")).unwrap();
        let wf = ws.join("W/workflow.json");
        std::fs::write(&wf, WF).unwrap();
        patch_all(&ws).unwrap();
        patched_dirs().lock().unwrap().remove(&ws);

        let theirs = WF.replace("Execute_Strategy_stored_procedure", "Renamed_By_Hand");
        std::fs::write(&wf, &theirs).unwrap();

        let report = heal_stale_patch_with(&ws, false);
        assert_eq!(report.restored, 0);
        assert_eq!(report.foreign, ["W"]);
        assert_eq!(
            std::fs::read_to_string(&wf).unwrap(),
            theirs,
            "their work survived"
        );
        assert!(
            backup_path(&ws, "W").exists(),
            "snapshot kept — we still have not restored it"
        );
    }

    /// One unreadable snapshot used to abort the whole pass with `?`, leaving
    /// every later workflow stranded in its patched state.
    #[test]
    fn one_bad_snapshot_does_not_strand_the_others() {
        let _g = serialised();
        let tmp = workspace();
        let ws = tmp.path().to_path_buf();
        for name in ["A", "B"] {
            std::fs::create_dir_all(ws.join(name)).unwrap();
            std::fs::write(ws.join(name).join("workflow.json"), WF).unwrap();
        }
        patch_all(&ws).unwrap();
        // A's snapshot becomes unreadable: a directory where a file should be.
        std::fs::remove_file(backup_path(&ws, "A")).unwrap();
        std::fs::create_dir(backup_path(&ws, "A")).unwrap();

        let report = restore(&ws);
        assert_eq!(report.restored, 1, "B was still put back");
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, "A");
        assert_eq!(
            std::fs::read_to_string(ws.join("B/workflow.json")).unwrap(),
            WF
        );
    }

    /// `strip` used to search for the next `{` anywhere in the file, so a
    /// string-valued `"authentication"` — a documented Logic Apps shape — made
    /// it brace-match a *later* action and delete everything in between.
    #[test]
    fn a_string_valued_authentication_does_not_swallow_the_next_action() {
        let raw = r#"{
  "A": { "inputs": { "authentication": "@parameters('$authentication')" } },
  "B": { "inputs": { "authentication": { "type": "ActiveDirectoryOAuth", "tenant": "t" } } }
}"#;
        let out = strip(raw);
        assert!(out.contains("\"A\""), "action A survived: {out}");
        assert!(!out.contains(OAUTH), "B's OAuth block still went: {out}");
        let v: serde_json::Value = serde_json::from_str(&out).expect("still valid JSON");
        assert_eq!(
            v["A"]["inputs"]["authentication"], "@parameters('$authentication')",
            "the string-valued member is left exactly as it was"
        );
    }

    /// Two `"authentication"` members sharing one comma: the second used to
    /// claim a comma the first had already consumed, producing a backwards
    /// slice range and a panic.
    #[test]
    fn adjacent_authentication_members_do_not_panic() {
        let raw = concat!(
            r#"{"a":1,"authentication":{"type":"ActiveDirectoryOAuth"},"#,
            r#""authentication":{"type":"ActiveDirectoryOAuth"}}"#
        );
        let out = strip(raw);
        assert!(!out.contains(OAUTH));
        serde_json::from_str::<serde_json::Value>(&out).expect("still valid JSON");
    }

    /// Verbatim shape from Send-Http-Get-Acme-AddressBook.
    const WF: &str = r#"{
    "definition": {
        "actions": {
            "Execute_Strategy_stored_procedure": {
                "type": "Http",
                "inputs": {
                    "uri": "@concat(variables('AcmeBasePath'),'/x')",
                    "method": "POST",
                    "body": {
                        "StrategyId": "@body('P')?['StrategyId']"
                    },
                    "authentication": {
                        "type": "ActiveDirectoryOAuth",
                        "authority": "",
                        "tenant": "@{parameters('PartnerTenantId')}",
                        "audience": "api://@{parameters('AcmeClientId')}",
                        "clientId": "@{parameters('AcmeClientId')}",
                        "secret": "@{parameters('AcmeSecret')}"
                    },
                    "runtimeConfiguration": {
                        "contentTransfer": {
                            "transferMode": "Chunked"
                        }
                    }
                }
            }
        }
    }
}"#;

    #[test]
    fn strips_oauth_and_leaves_valid_json() {
        let out = strip(WF);
        assert!(!out.contains(OAUTH), "auth block still present");
        let v: serde_json::Value = serde_json::from_str(&out).expect("still valid JSON");
        let inputs = &v["definition"]["actions"]["Execute_Strategy_stored_procedure"]["inputs"];
        assert!(inputs.get("authentication").is_none());
        // everything around it survives
        assert_eq!(inputs["method"], "POST");
        assert_eq!(inputs["body"]["StrategyId"], "@body('P')?['StrategyId']");
        assert_eq!(
            inputs["runtimeConfiguration"]["contentTransfer"]["transferMode"],
            "Chunked"
        );
    }

    /// `strip(x) == x` is the "already patched" test, so it must be a true
    /// fixed point — a second pass that kept changing the file would make the
    /// snapshot logic store patched content as the original.
    #[test]
    fn strip_is_idempotent() {
        let once = strip(WF);
        assert_eq!(strip(&once), once);
    }

    /// Only OAuth goes. A Basic block belongs to a connector we do not touch.
    #[test]
    fn leaves_other_auth_schemes_alone() {
        let basic = r#"{"inputs":{"uri":"x","authentication":{"type":"Basic","username":"u"}}}"#;
        assert_eq!(strip(basic), basic);
    }

    #[test]
    fn handles_auth_as_the_last_member() {
        let last = r#"{"inputs":{"uri":"x","authentication":{"type":"ActiveDirectoryOAuth","tenant":"t"}}}"#;
        let out = strip(last);
        assert!(!out.contains(OAUTH));
        serde_json::from_str::<serde_json::Value>(&out).expect("valid JSON");
    }

    #[test]
    fn a_run_leaves_the_working_tree_clean() {
        let _g = serialised();
        let ws = std::env::temp_dir().join(format!(
            "ais-wfauth-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let wf_dir = ws.join("Send-Http-Get-Acme-AddressBook");
        std::fs::create_dir_all(&wf_dir).unwrap();
        let wf = wf_dir.join("workflow.json");
        std::fs::write(&wf, WF).unwrap();

        let patched = patch_all(&ws).unwrap();
        assert_eq!(patched, vec!["Send-Http-Get-Acme-AddressBook".to_string()]);
        assert!(!std::fs::read_to_string(&wf).unwrap().contains(OAUTH));

        // a second start must not snapshot the patched file over the original
        assert!(patch_all(&ws).unwrap().is_empty());

        assert_eq!(restore(&ws).restored, 1);
        assert_eq!(std::fs::read_to_string(&wf).unwrap(), WF);

        std::fs::remove_dir_all(&ws).ok();
    }
}
