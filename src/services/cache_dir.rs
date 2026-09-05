//! The `.ais-cache` scratch directory inside a developer's Logic Apps folder.
//!
//! It holds the pristine copies of files we patch for a local run. Those files
//! live in the developer's own repository, so creating the directory quietly
//! adds untracked paths to a working tree that the snapshot machinery exists to
//! keep clean. A `.gitignore` of its own settles that: `git status` stays quiet
//! whether or not the project thought to ignore us.

use std::path::{Path, PathBuf};

pub const DIR_NAME: &str = ".ais-cache";

pub fn root(logic_apps_dir: &Path) -> PathBuf {
    logic_apps_dir.join(DIR_NAME)
}

/// Create `dir` — which must be [`root`] or a path below it — and make git
/// ignore everything under `.ais-cache`.
pub fn ensure(logic_apps_dir: &Path, dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let ignore = root(logic_apps_dir).join(".gitignore");
    if !ignore.exists() {
        std::fs::write(ignore, "# ais-runner scratch. Never commit this.\n*\n")?;
    }
    Ok(())
}
