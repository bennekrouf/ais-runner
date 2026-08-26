//! Persists the mock contract to the per-workspace cache directory.
//!
//! Cache layout (hidden from the user, not committed to the workspace repo):
//!
//!   <workspace>/.ais-cache/
//!     mock-contract.json     ← latest scan output
//!     mock-contract.<ts>.json (optional history snapshots — Phase 3+)

use std::path::{Path, PathBuf};

use crate::services::mock::contract::MockContract;
use crate::services::mock::scanner::ScanError;

pub const CACHE_DIR_NAME: &str = ".ais-cache";
pub const CONTRACT_FILE_NAME: &str = "mock-contract.json";

pub fn cache_dir(workspace: &Path) -> PathBuf {
    workspace.join(CACHE_DIR_NAME)
}

pub fn contract_path(workspace: &Path) -> PathBuf {
    cache_dir(workspace).join(CONTRACT_FILE_NAME)
}

/// Serialize the contract (pretty JSON) and write it to the workspace cache.
/// Creates the `.ais-cache/` directory if missing and a `.gitignore` inside
/// so accidental commits are prevented even if the user later toggles git
/// tracking on the cache dir.
pub fn write(contract: &MockContract, workspace: &Path) -> Result<PathBuf, ScanError> {
    let dir = cache_dir(workspace);
    std::fs::create_dir_all(&dir)?;

    // Best-effort gitignore — ignore errors, the cache is hidden anyway.
    let _ = std::fs::write(dir.join(".gitignore"), "*\n");

    let path = dir.join(CONTRACT_FILE_NAME);
    let json = serde_json::to_string_pretty(contract)?;
    std::fs::write(&path, json)?;
    Ok(path)
}

/// Load a cached contract, if one exists. Returns `None` if absent or invalid.
pub fn read(workspace: &Path) -> Option<MockContract> {
    let path = contract_path(workspace);
    let raw = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&raw).ok()
}
