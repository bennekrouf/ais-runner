//! Extension-bundle cache repair.
//!
//! Logic Apps Standard / Functions downloads its extension bundle
//! (`Microsoft.Azure.Functions.ExtensionBundle.Workflows`) into a per-user
//! cache. A partial download, an SSL failure, or a version clash leaves the
//! cache corrupt, and `func start` then fails with errors like
//! `File already exists in ExtensionBundles`, SSL handshake failures, or
//! missing DLLs. The fix is always the same: delete the cache so func
//! re-downloads it cleanly. This module locates the cache cross-platform and
//! clears it — the Service-Bus/Azurite "Reset" pattern, for bundles.

use std::path::PathBuf;

/// True for a func-output line that indicates a corrupt/failed extension bundle.
///
/// Deliberately excludes missing native prebuilds. Every path inside the bundle
/// contains "ExtensionBundles", so a line like
/// `Cannot find module '…/isolated-vm/prebuilds/darwin-arm64/node-v127.node'`
/// used to match on the path alone and produce a "clear the cache" prompt. That
/// advice is worse than useless there: the bundle ships prebuilds only for the
/// platforms and Node ABIs it was built against, so re-downloading fetches the
/// identical missing file and the user loops. See [`is_missing_prebuild`].
pub fn is_bundle_error(line: &str) -> bool {
    if is_missing_prebuild(line) {
        return false;
    }
    let l = line.to_lowercase();
    let mentions_bundle = l.contains("extensionbundle") || l.contains("extension bundle");
    let failed = l.contains("already exists")
        || l.contains("could not")
        || l.contains("failed")
        || l.contains("ssl")
        || l.contains("handshake")
        || l.contains("unable to")
        || l.contains("error");
    mentions_bundle && failed
}

/// True for a line reporting a native module the bundle has no build of for
/// this platform/Node ABI — the `isolated-vm` binding that inline-JavaScript
/// actions need is the one that bites in practice.
///
/// Not a cache problem and not fixable by clearing anything: the bundle simply
/// doesn't ship that combination.
pub fn is_missing_prebuild(line: &str) -> bool {
    let l = line.to_lowercase();
    l.contains("prebuilds") && (l.contains("cannot find module") || l.contains("not available"))
}

/// Candidate extension-bundle cache directories, most-likely first.
///
/// `~/.azure-functions-core-tools/Functions/ExtensionBundles` is used on every
/// platform; Windows additionally caches under the temp/LocalAppData trees.
pub fn cache_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = dirs::home_dir() {
        dirs.push(
            home.join(".azure-functions-core-tools")
                .join("Functions")
                .join("ExtensionBundles"),
        );
    }
    #[cfg(windows)]
    {
        // %TMP%\Functions\ExtensionBundles and %LOCALAPPDATA%\Temp\Functions\...
        if let Some(tmp) = std::env::var_os("TMP").or_else(|| std::env::var_os("TEMP")) {
            dirs.push(
                PathBuf::from(tmp)
                    .join("Functions")
                    .join("ExtensionBundles"),
            );
        }
        if let Some(la) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(
                PathBuf::from(la)
                    .join("Temp")
                    .join("Functions")
                    .join("ExtensionBundles"),
            );
        }
    }
    dirs
}

/// The subset of `cache_dirs` that actually exists on disk right now.
pub fn existing_cache_dirs() -> Vec<PathBuf> {
    cache_dirs().into_iter().filter(|d| d.exists()).collect()
}

/// Delete every existing extension-bundle cache dir so func re-downloads it.
/// Returns the paths cleared. Errs only if a cache exists but none could be
/// removed (a partial clear still returns Ok with what it managed to delete).
pub fn clear() -> Result<Vec<String>, String> {
    let mut cleared = Vec::new();
    let mut errors = Vec::new();
    for d in existing_cache_dirs() {
        match std::fs::remove_dir_all(&d) {
            Ok(()) => cleared.push(d.display().to_string()),
            Err(e) => errors.push(format!("{}: {}", d.display(), e)),
        }
    }
    if cleared.is_empty() && !errors.is_empty() {
        return Err(errors.join("; "));
    }
    Ok(cleared)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_bundle_failures_only() {
        assert!(is_bundle_error(
            "A host error has occurred: File already exists in ExtensionBundles"
        ));
        assert!(is_bundle_error(
            "Could not download Extension bundle: SSL handshake failed"
        ));
        assert!(is_bundle_error(
            "Unable to find ExtensionBundle Microsoft...Workflows"
        ));
        // Unrelated lines must not trip it.
        assert!(!is_bundle_error("Loaded 42 workflows"));
        assert!(!is_bundle_error("Host unavailable after check"));
        assert!(!is_bundle_error("ExtensionBundle loaded successfully")); // mentions bundle but no failure word
    }

    #[test]
    fn a_missing_native_prebuild_is_not_a_corrupt_cache() {
        // Every path under the bundle contains "ExtensionBundles", so this line
        // matched on its path alone and told the user to clear a cache that was
        // perfectly fine — re-downloading fetches the same missing file.
        let line = "isolated-vm not available, module failed to load: Cannot find module \
                    '/Users/x/.azure-functions-core-tools/Functions/ExtensionBundles/\
                    Microsoft.Azure.Functions.ExtensionBundle.Workflows/1.170.43/JS/node_modules/\
                    isolated-vm/prebuilds/darwin-arm64/node-v127.node'";
        assert!(is_missing_prebuild(line));
        assert!(!is_bundle_error(line));

        // A genuine cache failure still reports as one.
        assert!(!is_missing_prebuild(
            "A host error has occurred: File already exists in ExtensionBundles"
        ));
    }

    #[test]
    fn cache_dirs_includes_the_core_tools_path() {
        let dirs = cache_dirs();
        assert!(dirs
            .iter()
            .any(|d| d.ends_with("Functions/ExtensionBundles")
                || d.ends_with("Functions\\ExtensionBundles")));
    }
}
