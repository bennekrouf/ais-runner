//! Strip `ActiveDirectoryOAuth` from workflow HTTP actions for local runs.
//!
//! Workflows that call Ignite/JDE authenticate with:
//!
//! ```json
//! "authentication": {
//!     "type": "ActiveDirectoryOAuth",
//!     "tenant": "@{parameters('OryxTenantId')}",
//!     "clientId": "@{parameters('DabIgniteClientId')}",
//!     "secret": "@{parameters('DabIgniteSecret')}"
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

const CACHE_DIR: &str = ".ais-cache/workflows";
const OAUTH: &str = "ActiveDirectoryOAuth";

fn patched_dirs() -> &'static Mutex<HashSet<PathBuf>> {
    static DIRS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    DIRS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn cache_dir(logic_apps_dir: &Path) -> PathBuf {
    logic_apps_dir.join(CACHE_DIR)
}

fn backup_path(logic_apps_dir: &Path, workflow: &str) -> PathBuf {
    cache_dir(logic_apps_dir).join(format!("{workflow}.workflow.json.original"))
}

/// Remove every `"authentication": { … "ActiveDirectoryOAuth" … }` member.
///
/// Returns the text unchanged when there is nothing to strip, so
/// `strip(x) == x` doubles as the "already patched" test.
pub fn strip(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;

    while let Some(rel) = raw[i..].find("\"authentication\"") {
        let start = i + rel;
        // Walk to the member's value and brace-match it.
        let Some(open) = raw[start..].find('{').map(|o| start + o) else {
            break;
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
        if before.map(|p| bytes[p] == b',').unwrap_or(false) {
            cut_from = before.unwrap();
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
        std::fs::create_dir_all(cache_dir(logic_apps_dir))?;
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

/// Put every snapshotted workflow.json back. Returns how many files moved.
pub fn restore(logic_apps_dir: &Path) -> std::io::Result<usize> {
    let dir = cache_dir(logic_apps_dir);
    let mut restored = 0;
    if let Ok(entries) = std::fs::read_dir(&dir) {
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
            let target = logic_apps_dir.join(&name).join("workflow.json");
            let original = std::fs::read_to_string(&backup)?;
            if std::fs::read_to_string(&target).ok().as_deref() != Some(original.as_str()) {
                std::fs::write(&target, original)?;
                restored += 1;
            }
            let _ = std::fs::remove_file(&backup);
        }
    }
    let _ = std::fs::remove_dir(&dir);
    if let Ok(mut dirs) = patched_dirs().lock() {
        dirs.remove(logic_apps_dir);
    }
    Ok(restored)
}

/// Put back workflow.json files left patched by a session that never
/// restored them — a hard crash, a force-quit, or (as happened once) func
/// left running after ais-runner itself closed. `restore_all` only covers a
/// clean exit from *this* process; this covers opening a project and finding
/// someone else's mess, the case that bit us the day this function was
/// written: 17 workflow.json files sat OAuth-stripped in the working tree
/// with no ais-runner process left alive to put them back.
///
/// Skipped while something is serving :7071 — that's a func host from an
/// earlier session still running, and it re-reads these files on workflow
/// reload. Its exit isn't observable from here, so the patch waits for the
/// next open.
pub fn heal_stale_patch(logic_apps_dir: &Path) -> std::io::Result<usize> {
    let dir = cache_dir(logic_apps_dir);
    if !dir.is_dir() {
        return Ok(0);
    }
    if func_is_listening() {
        register(logic_apps_dir);
        return Ok(0);
    }
    restore(logic_apps_dir)
}

fn func_is_listening() -> bool {
    use std::net::{Ipv4Addr, SocketAddr, TcpStream};
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 7071));
    TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(200)).is_ok()
}

/// Restore every directory patched in this session — teardown, where there is
/// no project directory to hand.
pub fn restore_all() -> usize {
    let dirs: Vec<PathBuf> = match patched_dirs().lock() {
        Ok(d) => d.iter().cloned().collect(),
        Err(_) => return 0,
    };
    dirs.iter().map(|d| restore(d).unwrap_or(0)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn serialised() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The exact scenario that motivated this function: a patched workflow
    /// with no live ais-runner process — func not listening, nothing in the
    /// registry (fresh test process). Opening the project must put it back.
    #[test]
    fn heal_stale_patch_restores_when_func_is_not_listening() {
        let _g = serialised();
        let ws = std::env::temp_dir().join(format!(
            "ais-wfauth-heal-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let wf_dir = ws.join("Send-Http-Get-Ignite-AddressBook");
        std::fs::create_dir_all(&wf_dir).unwrap();
        let wf = wf_dir.join("workflow.json");
        std::fs::write(&wf, WF).unwrap();

        patch_all(&ws).unwrap();
        assert!(!std::fs::read_to_string(&wf).unwrap().contains(OAUTH));
        // simulate a crash: nothing registered for this dir in this process
        patched_dirs().lock().unwrap().remove(&ws);

        assert_eq!(heal_stale_patch(&ws).unwrap(), 1);
        assert_eq!(std::fs::read_to_string(&wf).unwrap(), WF);

        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn heal_stale_patch_is_a_no_op_with_no_backup() {
        let _g = serialised();
        let ws = std::env::temp_dir().join(format!(
            "ais-wfauth-heal-none-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&ws).unwrap();
        assert_eq!(heal_stale_patch(&ws).unwrap(), 0);
        std::fs::remove_dir_all(&ws).ok();
    }

    /// Verbatim shape from Send-Http-Get-Ignite-AddressBook.
    const WF: &str = r#"{
    "definition": {
        "actions": {
            "Execute_Strategy_stored_procedure": {
                "type": "Http",
                "inputs": {
                    "uri": "@concat(variables('IgniteBasePath'),'/x')",
                    "method": "POST",
                    "body": {
                        "StrategyId": "@body('P')?['StrategyId']"
                    },
                    "authentication": {
                        "type": "ActiveDirectoryOAuth",
                        "authority": "",
                        "tenant": "@{parameters('OryxTenantId')}",
                        "audience": "api://@{parameters('DabIgniteClientId')}",
                        "clientId": "@{parameters('DabIgniteClientId')}",
                        "secret": "@{parameters('DabIgniteSecret')}"
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
        let wf_dir = ws.join("Send-Http-Get-Ignite-AddressBook");
        std::fs::create_dir_all(&wf_dir).unwrap();
        let wf = wf_dir.join("workflow.json");
        std::fs::write(&wf, WF).unwrap();

        let patched = patch_all(&ws).unwrap();
        assert_eq!(
            patched,
            vec!["Send-Http-Get-Ignite-AddressBook".to_string()]
        );
        assert!(!std::fs::read_to_string(&wf).unwrap().contains(OAUTH));

        // a second start must not snapshot the patched file over the original
        assert!(patch_all(&ws).unwrap().is_empty());

        assert_eq!(restore(&ws).unwrap(), 1);
        assert_eq!(std::fs::read_to_string(&wf).unwrap(), WF);

        std::fs::remove_dir_all(&ws).ok();
    }
}
