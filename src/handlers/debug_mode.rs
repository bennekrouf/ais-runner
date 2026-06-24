// Debug mode: temporarily flip Stateless workflows to Stateful so their run
// history persists and is inspectable from the Run tab. Stateless workflows
// don't write any run record by design, which means failures show up only as
// "Run error: '<null>'" in the host stdout with no per-action detail.
//
// Safety model:
// - Before patching, write a sidecar `workflow.json.aisrunner-bak` capturing
//   the original kind + a sha of the file at backup time + this process's PID.
// - On disable: walk sidecars, restore the original kind, delete sidecar.
// - On ais-runner startup: cleanup_orphans() restores any sidecars left from
//   a previous crashed/killed session.
// - If the user has edited the file since we patched (sha mismatch), we skip
//   the auto-revert and leave the sidecar for the user to resolve.
// - Per-workflow opt-out: presence of `.ais-runner-keep-stateless` in the
//   workflow directory skips that workflow entirely.

use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::services::workflows::resolve_logic_apps_dir;

const SIDECAR_EXT: &str = "aisrunner-bak";
const OPT_OUT_MARKER: &str = ".ais-runner-keep-stateless";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Sidecar {
    pid: u32,
    original_kind: String,
    sha_at_patch: String,
}

pub struct PatchOutcome {
    pub patched: Vec<String>,
    pub skipped: Vec<(String, String)>,
}

pub struct RevertOutcome {
    pub reverted: Vec<String>,
    pub skipped: Vec<(String, String)>,
}

fn sha_of(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

fn sidecar_path_for(wf_json: &Path) -> PathBuf {
    let mut p = wf_json.as_os_str().to_os_string();
    p.push(".");
    p.push(SIDECAR_EXT);
    PathBuf::from(p)
}

fn list_workflow_jsons(base_dir: &str) -> Vec<PathBuf> {
    let root = resolve_logic_apps_dir(base_dir);
    let Ok(entries) = fs::read_dir(&root) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if !p.is_dir() { return None; }
            let wf = p.join("workflow.json");
            if wf.is_file() { Some(wf) } else { None }
        })
        .collect()
}

fn replace_kind(text: &str, from: &str, to: &str) -> Option<String> {
    let pat = format!("\"kind\"");
    let kind_idx = text.find(&pat)?;
    let rest = &text[kind_idx..];
    let colon = kind_idx + rest.find(':')?;
    let after_colon = &text[colon + 1..];
    let q1 = colon + 1 + after_colon.find('"')?;
    let after_q1 = &text[q1 + 1..];
    let q2 = q1 + 1 + after_q1.find('"')?;
    let current = &text[q1 + 1..q2];
    if !current.eq_ignore_ascii_case(from) { return None; }
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..q1 + 1]);
    out.push_str(to);
    out.push_str(&text[q2..]);
    Some(out)
}

pub fn enable(base_dir: &str) -> PatchOutcome {
    let mut patched = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
    let pid = std::process::id();

    for wf_json in list_workflow_jsons(base_dir) {
        let name = wf_json
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| wf_json.to_string_lossy().into_owned());

        if wf_json.parent().map(|p| p.join(OPT_OUT_MARKER).exists()).unwrap_or(false) {
            skipped.push((name, "opt-out marker present".into()));
            continue;
        }

        let sidecar = sidecar_path_for(&wf_json);
        if sidecar.exists() {
            skipped.push((name, "sidecar already present (already debug-patched?)".into()));
            continue;
        }

        let bytes = match fs::read(&wf_json) {
            Ok(b) => b,
            Err(e) => { skipped.push((name, format!("read failed: {e}"))); continue; }
        };
        let text = match String::from_utf8(bytes.clone()) {
            Ok(t) => t,
            Err(_) => { skipped.push((name, "not UTF-8".into())); continue; }
        };
        let Some(patched_text) = replace_kind(&text, "Stateless", "Stateful") else {
            // Not stateless, nothing to do.
            continue;
        };
        let sha = sha_of(patched_text.as_bytes());
        let card = Sidecar { pid, original_kind: "Stateless".into(), sha_at_patch: sha };
        let card_json = match serde_json::to_string_pretty(&card) {
            Ok(s) => s,
            Err(e) => { skipped.push((name, format!("sidecar serialize: {e}"))); continue; }
        };
        if let Err(e) = fs::write(&sidecar, card_json) {
            skipped.push((name, format!("sidecar write failed (permission?): {e}")));
            continue;
        }
        if let Err(e) = fs::write(&wf_json, patched_text.as_bytes()) {
            let _ = fs::remove_file(&sidecar);
            skipped.push((name, format!("workflow write failed (permission?): {e}")));
            continue;
        }
        patched.push(name);
    }

    PatchOutcome { patched, skipped }
}

fn read_sidecar(path: &Path) -> Option<Sidecar> {
    let s = fs::read_to_string(path).ok()?;
    serde_json::from_str(&s).ok()
}

fn revert_one(wf_json: &Path, sidecar_path: &Path, ignore_pid: bool) -> Result<bool, String> {
    let Some(card) = read_sidecar(sidecar_path) else {
        return Err("sidecar unreadable".into());
    };
    if !ignore_pid && card.pid != std::process::id() {
        return Err(format!("sidecar PID {} != current — leaving alone", card.pid));
    }
    let text = fs::read_to_string(wf_json).map_err(|e| e.to_string())?;
    let sha_now = sha_of(text.as_bytes());
    if sha_now != card.sha_at_patch {
        return Err("file changed since patch (sha mismatch) — preserving user edits".into());
    }
    let Some(reverted) = replace_kind(&text, "Stateful", &card.original_kind) else {
        return Err("kind field not found in expected shape".into());
    };
    fs::write(wf_json, reverted.as_bytes()).map_err(|e| e.to_string())?;
    fs::remove_file(sidecar_path).ok();
    Ok(true)
}

pub fn disable(base_dir: &str) -> RevertOutcome {
    let mut reverted = Vec::new();
    let mut skipped = Vec::new();
    for wf_json in list_workflow_jsons(base_dir) {
        let sidecar = sidecar_path_for(&wf_json);
        if !sidecar.exists() { continue; }
        let name = wf_json
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        match revert_one(&wf_json, &sidecar, false) {
            Ok(_)  => reverted.push(name),
            Err(e) => skipped.push((name, e)),
        }
    }
    RevertOutcome { reverted, skipped }
}

/// Walks the logic_apps directory at startup and restores any orphan sidecars
/// left behind from a previous (crashed/killed) ais-runner session. Ignores
/// the PID check — the previous session's PID is never the current one.
pub fn cleanup_orphans(base_dir: &str) -> RevertOutcome {
    let mut reverted = Vec::new();
    let mut skipped = Vec::new();
    for wf_json in list_workflow_jsons(base_dir) {
        let sidecar = sidecar_path_for(&wf_json);
        if !sidecar.exists() { continue; }
        let name = wf_json
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        match revert_one(&wf_json, &sidecar, true) {
            Ok(_)  => reverted.push(name),
            Err(e) => skipped.push((name, e)),
        }
    }
    RevertOutcome { reverted, skipped }
}

/// Returns the set of workflow names currently in debug-mode (sidecar present).
pub fn active(base_dir: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for wf_json in list_workflow_jsons(base_dir) {
        if sidecar_path_for(&wf_json).exists() {
            if let Some(name) = wf_json.parent().and_then(|p| p.file_name()) {
                out.insert(name.to_string_lossy().into_owned());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_kind_stateless_to_stateful() {
        let src = r#"{ "kind": "Stateless", "definition": {} }"#;
        let out = replace_kind(src, "Stateless", "Stateful").unwrap();
        assert_eq!(out, r#"{ "kind": "Stateful", "definition": {} }"#);
    }

    #[test]
    fn replace_kind_no_op_when_already_stateful() {
        let src = r#"{ "kind": "Stateful" }"#;
        assert!(replace_kind(src, "Stateless", "Stateful").is_none());
    }

    #[test]
    fn replace_kind_preserves_surrounding_json() {
        let src = "{\n  \"kind\"  :  \"Stateless\"  ,\n  \"x\": 1\n}";
        let out = replace_kind(src, "Stateless", "Stateful").unwrap();
        assert!(out.contains("\"Stateful\""));
        assert!(out.contains("\"x\": 1"));
    }
}
