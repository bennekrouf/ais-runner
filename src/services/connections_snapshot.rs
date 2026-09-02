//! Keep `connections.json` out of the working tree.
//!
//! `connections.json` is the committed, cloud-facing file. Local runs need a
//! patched version of it (ARM syntax fixed, MSI swapped for local emulators,
//! `connections.local.json` layered on top), and the Logic Apps runtime reads
//! that file and only that file — there is no local-override mechanism to point
//! it elsewhere. So the patched content has to land there.
//!
//! What must not happen is the patched content *staying* there: the developer
//! then has a permanently dirty file they never edited and must remember not to
//! commit. This module snapshots the pristine file before the patch and puts it
//! back when func stops, so the working tree is only dirty while func runs.
//!
//! Detecting "already patched" uses the patch's own idempotence rather than a
//! marker: `patch(x) == x` means x is a fixed point, so it is already patched
//! (or needed no patching, in which case snapshotting it is harmless). That
//! keeps a crash-then-restart cycle from snapshotting the patched file and
//! making the damage permanent — the same failure that made the mock rewrite
//! lose settings for good.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::services::setup_manager;

const BACKUP_NAME: &str = "connections.json.original";

/// Directories whose `connections.json` is currently patched.
///
/// `restore` used to be reachable only from the Stop button, so func dying on
/// its own — or the app closing — left the file patched with no snapshot in
/// sight. The registry lets teardown put every patched file back without
/// having to thread the project directory down to `main`.
fn patched_dirs() -> &'static Mutex<HashSet<PathBuf>> {
    static DIRS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    DIRS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn cache_dir(logic_apps_dir: &Path) -> PathBuf {
    logic_apps_dir.join(".ais-cache")
}

pub fn backup_path(logic_apps_dir: &Path) -> PathBuf {
    cache_dir(logic_apps_dir).join(BACKUP_NAME)
}

/// Apply the local patches the way `func start` does, without writing.
/// Kept here so the snapshot logic and the start path cannot drift apart.
pub fn patched(raw: &str) -> String {
    setup_manager::patch_connections_for_local(&setup_manager::fix_connections_json(raw))
}

/// True when `raw` is already in patched form — patching it changes nothing.
fn is_patched(raw: &str) -> bool {
    patched(raw) == raw
}

/// Snapshot the pristine `connections.json` so `restore` can put it back.
///
/// No-op when the file is already patched: overwriting the snapshot then would
/// store the patched content as the "original" and lose the real one.
pub fn snapshot(logic_apps_dir: &Path) -> std::io::Result<bool> {
    let src = logic_apps_dir.join("connections.json");
    if !src.exists() {
        return Ok(false);
    }
    let raw = std::fs::read_to_string(&src)?;
    // Register even when already patched: a snapshot from an earlier run is
    // still the pristine copy, and that run may have exited without restoring.
    if backup_path(logic_apps_dir).exists() || !is_patched(&raw) {
        register(logic_apps_dir);
    }
    if is_patched(&raw) {
        return Ok(false);
    }
    std::fs::create_dir_all(cache_dir(logic_apps_dir))?;
    std::fs::write(backup_path(logic_apps_dir), raw)?;
    Ok(true)
}

fn register(logic_apps_dir: &Path) {
    if let Ok(mut dirs) = patched_dirs().lock() {
        dirs.insert(logic_apps_dir.to_path_buf());
    }
}

/// Restore every directory patched in this session. Returns how many files
/// actually moved. Used by teardown, where there is no `push` to log through.
pub fn restore_all() -> usize {
    let dirs: Vec<PathBuf> = match patched_dirs().lock() {
        Ok(d) => d.iter().cloned().collect(),
        Err(_) => return 0,
    };
    dirs.iter().filter(|d| restore(d).unwrap_or(false)).count()
}

/// Put the pristine `connections.json` back. Returns whether anything changed,
/// so the caller only logs when the file actually moved.
pub fn restore(logic_apps_dir: &Path) -> std::io::Result<bool> {
    let backup = backup_path(logic_apps_dir);
    if !backup.exists() {
        return Ok(false);
    }
    let original = std::fs::read_to_string(&backup)?;
    let target = logic_apps_dir.join("connections.json");
    if std::fs::read_to_string(&target).ok().as_deref() == Some(original.as_str()) {
        if let Ok(mut dirs) = patched_dirs().lock() {
            dirs.remove(logic_apps_dir);
        }
        return Ok(false);
    }
    std::fs::write(&target, original)?;
    if let Ok(mut dirs) = patched_dirs().lock() {
        dirs.remove(logic_apps_dir);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `patched_dirs` is process-global, so these tests would otherwise see
    /// each other's registrations — `restore_all` in one run drains another's.
    fn serialised() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn workspace() -> PathBuf {
        let ws = std::env::temp_dir().join(format!(
            "ais-connsnap-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&ws).unwrap();
        ws
    }

    /// A connections.json that the local patch actually rewrites, so the tests
    /// exercise a real pristine → patched transition.
    const CLOUD: &str = r#"{
  "serviceProviderConnections": {
    "AzureBlob": {
      "parameterSetName": "ManagedServiceIdentity",
      "parameterValues": {
        "authProvider": { "Type": "ManagedServiceIdentity" },
        "blobStorageEndpoint": "@appsetting('AzureBlob_blobStorageEndpoint')"
      },
      "serviceProvider": { "id": "/serviceProviders/AzureBlob" }
    }
  }
}"#;

    #[test]
    fn a_run_leaves_the_working_tree_clean() {
        let _g = serialised();
        let ws = workspace();
        let conn = ws.join("connections.json");
        std::fs::write(&conn, CLOUD).unwrap();

        // start: snapshot, then patch in place
        assert!(snapshot(&ws).unwrap());
        std::fs::write(&conn, patched(CLOUD)).unwrap();
        assert_ne!(
            std::fs::read_to_string(&conn).unwrap(),
            CLOUD,
            "test premise: the patch changes the file"
        );

        // stop: the developer gets their file back untouched
        assert!(restore(&ws).unwrap());
        assert_eq!(std::fs::read_to_string(&conn).unwrap(), CLOUD);

        std::fs::remove_dir_all(&ws).ok();
    }

    /// A crash leaves the file patched. Restarting must not snapshot that and
    /// lose the pristine content — the failure mode that made the settings
    /// corruption permanent.
    #[test]
    fn restarting_over_a_patched_file_keeps_the_pristine_snapshot() {
        let _g = serialised();
        let ws = workspace();
        let conn = ws.join("connections.json");
        std::fs::write(&conn, CLOUD).unwrap();

        snapshot(&ws).unwrap();
        std::fs::write(&conn, patched(CLOUD)).unwrap();
        // crash — no restore. Next start:
        assert!(
            !snapshot(&ws).unwrap(),
            "must not snapshot an already-patched file"
        );

        restore(&ws).unwrap();
        assert_eq!(std::fs::read_to_string(&conn).unwrap(), CLOUD);

        std::fs::remove_dir_all(&ws).ok();
    }

    /// The Stop button was the only caller of `restore`, so closing the app or
    /// func dying left the file patched. Teardown must reach it without being
    /// told which directory it was.
    #[test]
    fn restore_all_cleans_up_a_run_that_never_stopped() {
        let _g = serialised();
        let ws = workspace();
        let conn = ws.join("connections.json");
        std::fs::write(&conn, CLOUD).unwrap();

        snapshot(&ws).unwrap();
        std::fs::write(&conn, patched(CLOUD)).unwrap();
        // no restore — the app is closed instead

        assert!(restore_all() >= 1);
        assert_eq!(std::fs::read_to_string(&conn).unwrap(), CLOUD);
        assert_eq!(restore_all(), 0, "nothing left registered");

        std::fs::remove_dir_all(&ws).ok();
    }

    /// A previous session that exited without restoring leaves a snapshot on
    /// disk and nothing in the registry. Starting again must re-register, or
    /// this session's teardown skips the file too.
    #[test]
    fn a_stale_snapshot_is_re_registered_on_the_next_start() {
        let _g = serialised();
        let ws = workspace();
        let conn = ws.join("connections.json");
        std::fs::write(&conn, CLOUD).unwrap();
        snapshot(&ws).unwrap();
        std::fs::write(&conn, patched(CLOUD)).unwrap();
        // simulate the registry being empty, as it is in a fresh process
        patched_dirs().lock().unwrap().remove(&ws);

        assert!(!snapshot(&ws).unwrap(), "already patched, no new snapshot");
        assert!(
            patched_dirs().lock().unwrap().contains(&ws),
            "must re-register so teardown restores it"
        );

        restore(&ws).unwrap();
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn restore_without_a_snapshot_is_a_no_op() {
        let _g = serialised();
        let ws = workspace();
        std::fs::write(ws.join("connections.json"), CLOUD).unwrap();
        assert!(!restore(&ws).unwrap());
        assert_eq!(
            std::fs::read_to_string(ws.join("connections.json")).unwrap(),
            CLOUD
        );
        std::fs::remove_dir_all(&ws).ok();
    }
}
