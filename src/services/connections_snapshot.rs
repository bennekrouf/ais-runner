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

use crate::services::cache_dir;
use crate::services::net;
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
    cache_dir::root(logic_apps_dir)
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
    cache_dir::ensure(logic_apps_dir, &cache_dir(logic_apps_dir))?;
    std::fs::write(backup_path(logic_apps_dir), raw)?;
    Ok(true)
}

fn register(logic_apps_dir: &Path) {
    if let Ok(mut dirs) = patched_dirs().lock() {
        dirs.insert(logic_apps_dir.to_path_buf());
    }
}

/// What a restore pass did with the snapshot it found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restore {
    /// No snapshot on disk, or the file already matches it.
    Nothing,
    /// The pristine file is back.
    Restored,
    /// The file on disk is not the patch we wrote — edited, pulled, or
    /// replaced since. Left untouched and the snapshot kept.
    Foreign,
}

/// Put back a `connections.json` left patched by a session that never restored
/// it — a crash, a force-quit, or func left running after ais-runner itself
/// closed. `restore_all` only covers this process's own clean exit; this covers
/// opening a project and finding someone else's mess.
///
/// This is a safety net, not the primary mechanism: the panic hook and the
/// signal handler in `main` both call `restore_all`, so a crash normally cleans
/// up after itself and this finds nothing.
pub fn heal_stale_patch(logic_apps_dir: &Path) -> std::io::Result<Restore> {
    heal_stale_patch_with(logic_apps_dir, net::is_listening(net::FUNC_PORT))
}

/// The testable core. `func_running` is injected so unit tests do not depend on
/// whatever happens to hold :7071 on the machine running them — which, on a
/// developer box working on this project, is usually func.
pub fn heal_stale_patch_with(
    logic_apps_dir: &Path,
    func_running: bool,
) -> std::io::Result<Restore> {
    if !backup_path(logic_apps_dir).exists() {
        return Ok(Restore::Nothing);
    }
    if func_running {
        // Deferred, not skipped. A func host from an earlier session re-reads
        // this file on workflow reload, so swapping it now would change a
        // running workflow mid-flight. Registering hands the job to this
        // process's teardown, where `restore_all` picks it up on close.
        //
        // The probe cannot tell whose func that is; a host serving a different
        // project is a false positive that costs nothing but the deferral.
        register(logic_apps_dir);
        return Ok(Restore::Nothing);
    }
    restore(logic_apps_dir)
}

/// Restore every directory patched in this session. Returns how many files
/// actually moved. Used by teardown, where there is no `push` to log through.
pub fn restore_all() -> usize {
    let dirs: Vec<PathBuf> = match patched_dirs().lock() {
        Ok(d) => d.iter().cloned().collect(),
        Err(_) => return 0,
    };
    dirs.iter()
        .filter(|d| matches!(restore(d), Ok(Restore::Restored)))
        .count()
}

/// Put the pristine `connections.json` back, unless the developer has changed
/// it since we patched it.
pub fn restore(logic_apps_dir: &Path) -> std::io::Result<Restore> {
    let backup = backup_path(logic_apps_dir);
    if !backup.exists() {
        return Ok(Restore::Nothing);
    }
    let original = std::fs::read_to_string(&backup)?;
    let target = logic_apps_dir.join("connections.json");
    let outcome = match std::fs::read_to_string(&target).ok().as_deref() {
        // Already the way the developer had it.
        Some(c) if c == original => Restore::Nothing,
        // Byte-for-byte what the start path would have written from this
        // snapshot, so it is ours to put back.
        //
        // `is_patched` is not good enough here, and that distinction is the
        // whole point: a connections.json that already points at local
        // emulators is a fixed point of the patch without us ever having
        // touched it. Treating "is a fixed point" as "we patched this" hands a
        // stale snapshot the authority to overwrite work we never did.
        Some(c) if patched(&original) == c => {
            std::fs::write(&target, &original)?;
            Restore::Restored
        }
        // Deleted while we held a snapshot of it — putting it back is strictly
        // better than leaving the project without its connections.
        None => {
            std::fs::write(&target, &original)?;
            Restore::Restored
        }
        // Something we never wrote. Hands off, and keep the snapshot.
        Some(_) => Restore::Foreign,
    };
    if outcome != Restore::Foreign {
        // The snapshot has done its job. Leaving it behind makes every later
        // session believe a patch is still outstanding.
        let _ = std::fs::remove_file(&backup);
        let _ = std::fs::remove_dir(cache_dir(logic_apps_dir));
        if let Ok(mut dirs) = patched_dirs().lock() {
            dirs.remove(logic_apps_dir);
        }
    }
    Ok(outcome)
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

    /// A `TempDir`, not a hand-rolled pid+nanos path: it cleans up even when
    /// the test panics, which the old `remove_dir_all` on the success path did
    /// not. `tempfile` was already a dev-dependency.
    fn workspace() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
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
        let tmp = workspace();
        let ws = tmp.path().to_path_buf();
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
        assert_eq!(restore(&ws).unwrap(), Restore::Restored);
        assert_eq!(std::fs::read_to_string(&conn).unwrap(), CLOUD);
    }

    /// A crash leaves the file patched. Restarting must not snapshot that and
    /// lose the pristine content — the failure mode that made the settings
    /// corruption permanent.
    #[test]
    fn restarting_over_a_patched_file_keeps_the_pristine_snapshot() {
        let _g = serialised();
        let tmp = workspace();
        let ws = tmp.path().to_path_buf();
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
    }

    /// The Stop button was the only caller of `restore`, so closing the app or
    /// func dying left the file patched. Teardown must reach it without being
    /// told which directory it was.
    #[test]
    fn restore_all_cleans_up_a_run_that_never_stopped() {
        let _g = serialised();
        let tmp = workspace();
        let ws = tmp.path().to_path_buf();
        let conn = ws.join("connections.json");
        std::fs::write(&conn, CLOUD).unwrap();

        snapshot(&ws).unwrap();
        std::fs::write(&conn, patched(CLOUD)).unwrap();
        // no restore — the app is closed instead

        assert!(restore_all() >= 1);
        assert_eq!(std::fs::read_to_string(&conn).unwrap(), CLOUD);
        assert_eq!(restore_all(), 0, "nothing left registered");
    }

    /// A previous session that exited without restoring leaves a snapshot on
    /// disk and nothing in the registry. Starting again must re-register, or
    /// this session's teardown skips the file too.
    #[test]
    fn a_stale_snapshot_is_re_registered_on_the_next_start() {
        let _g = serialised();
        let tmp = workspace();
        let ws = tmp.path().to_path_buf();
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
    }

    #[test]
    fn restore_without_a_snapshot_is_a_no_op() {
        let _g = serialised();
        let tmp = workspace();
        let ws = tmp.path().to_path_buf();
        std::fs::write(ws.join("connections.json"), CLOUD).unwrap();
        assert_eq!(restore(&ws).unwrap(), Restore::Nothing);
        assert_eq!(
            std::fs::read_to_string(ws.join("connections.json")).unwrap(),
            CLOUD
        );
    }

    /// The scenario that motivated this function: a patched connections.json
    /// with no live ais-runner process to notice — func not listening,
    /// nothing registered in this (fresh) process. Opening the project must
    /// put it back rather than leave the committed file dirty forever.
    #[test]
    fn heal_stale_patch_restores_when_func_is_not_listening() {
        let _g = serialised();
        let tmp = workspace();
        let ws = tmp.path().to_path_buf();
        let conn = ws.join("connections.json");
        std::fs::write(&conn, CLOUD).unwrap();

        snapshot(&ws).unwrap();
        std::fs::write(&conn, patched(CLOUD)).unwrap();
        // simulate a crash: nothing registered for this dir in this process
        patched_dirs().lock().unwrap().remove(&ws);

        assert_eq!(
            heal_stale_patch_with(&ws, false).unwrap(),
            Restore::Restored
        );
        assert_eq!(std::fs::read_to_string(&conn).unwrap(), CLOUD);
    }

    /// A func host from an earlier session is still reading the file, so the
    /// swap waits — but the directory is registered, or this session's
    /// teardown would skip it too and the patch would outlive us again.
    #[test]
    fn heal_stale_patch_defers_to_teardown_while_func_is_up() {
        let _g = serialised();
        let tmp = workspace();
        let ws = tmp.path().to_path_buf();
        let conn = ws.join("connections.json");
        std::fs::write(&conn, CLOUD).unwrap();
        snapshot(&ws).unwrap();
        std::fs::write(&conn, patched(CLOUD)).unwrap();
        patched_dirs().lock().unwrap().remove(&ws);

        assert_eq!(heal_stale_patch_with(&ws, true).unwrap(), Restore::Nothing);
        assert_eq!(std::fs::read_to_string(&conn).unwrap(), patched(CLOUD));
        assert!(
            patched_dirs().lock().unwrap().contains(&ws),
            "deferred, so teardown has to know about it"
        );
        restore(&ws).unwrap();
    }

    #[test]
    fn heal_stale_patch_is_a_no_op_with_no_backup() {
        let _g = serialised();
        let tmp = workspace();
        let ws = tmp.path().to_path_buf();
        std::fs::write(ws.join("connections.json"), CLOUD).unwrap();
        assert_eq!(heal_stale_patch_with(&ws, false).unwrap(), Restore::Nothing);
    }

    /// The data-loss case. A crash leaves a snapshot behind; the developer then
    /// makes connections.json all-local themselves and commits it. That file is
    /// a fixed point of the patch, so an `is_patched` test calls it "patched"
    /// and the stale snapshot overwrites work we never did.
    #[test]
    fn a_file_we_did_not_patch_is_never_overwritten() {
        let _g = serialised();
        let tmp = workspace();
        let ws = tmp.path().to_path_buf();
        let conn = ws.join("connections.json");
        std::fs::write(&conn, CLOUD).unwrap();
        snapshot(&ws).unwrap();
        // crash, then the developer rewrites the file by hand and commits it
        let theirs = patched(CLOUD).replace("AzureBlob", "AzureBlobRenamedByHand");
        assert!(
            is_patched(&theirs),
            "test premise: their file is a fixed point"
        );
        std::fs::write(&conn, &theirs).unwrap();
        patched_dirs().lock().unwrap().remove(&ws);

        assert_eq!(heal_stale_patch_with(&ws, false).unwrap(), Restore::Foreign);
        assert_eq!(
            std::fs::read_to_string(&conn).unwrap(),
            theirs,
            "their work survived"
        );
        assert!(
            backup_path(&ws).exists(),
            "snapshot kept — we still have not restored it"
        );
    }
}
