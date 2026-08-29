//! The `az` CLI wrapper — process plumbing and nothing else.
//!
//! Everything in this crate reaches Azure through [`az_command`] or the
//! `run` helper here. Keeping this module free of any one Azure service is what
//! lets a consumer link the query modules without also linking the interactive
//! login flow in [`crate::auth`].

use std::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub enum AzError {
    NotLoggedIn,
    Other(String),
}

/// Resolve the full path to the `az` CLI on Windows.
///
/// Azure CLI on Windows installs as `az.cmd` (a batch wrapper) in a
/// non-standard directory that is usually on the user PATH inside a terminal
/// but missing from the minimal PATH that GUI apps inherit when launched from
/// the desktop or Start Menu.
///
/// Common install locations (checked in order):
///   %ProgramFiles(x86)%\Microsoft SDKs\Azure\CLI2\wbin   ← MSI default
///   %ProgramFiles%\Microsoft SDKs\Azure\CLI2\wbin         ← 64-bit MSI
///   %LOCALAPPDATA%\Programs\Azure CLI\wbin                 ← per-user install
#[cfg(target_os = "windows")]
pub(crate) fn resolve_az_windows() -> String {
    let candidates: &[(&str, &str)] = &[
        (
            "ProgramFiles(x86)",
            r"Microsoft SDKs\Azure\CLI2\wbin\az.cmd",
        ),
        ("ProgramFiles", r"Microsoft SDKs\Azure\CLI2\wbin\az.cmd"),
        ("LOCALAPPDATA", r"Programs\Azure CLI\wbin\az.cmd"),
    ];
    for (env_var, suffix) in candidates {
        if let Ok(base) = std::env::var(env_var) {
            let full = std::path::Path::new(&base).join(suffix);
            if full.is_file() {
                return full.to_string_lossy().to_string();
            }
        }
    }
    // Last resort: hope it's on PATH
    "az".to_string()
}

pub fn az_command(args: &[&str]) -> Command {
    if cfg!(target_os = "windows") {
        #[cfg(target_os = "windows")]
        let az_path = resolve_az_windows();
        #[cfg(not(target_os = "windows"))]
        let _az_path = "az".to_string();

        let mut cmd = Command::new("cmd");
        cmd.args(["/c", "az"]).args(args);

        // If az was found outside the inherited PATH (e.g. C:\Program Files\...),
        // inject its directory into the child's PATH so cmd.exe resolves "az" by
        // name — avoids quoting a path with spaces which cmd /c misparses.
        #[cfg(target_os = "windows")]
        if az_path != "az" {
            if let Some(dir) = std::path::Path::new(&az_path).parent() {
                let current = std::env::var("PATH").unwrap_or_default();
                cmd.env("PATH", format!("{};{}", dir.display(), current));
            }
        }

        cmd
    } else {
        let mut cmd = Command::new("az");
        cmd.args(args);
        cmd
    }
}

pub(crate) fn run(args: &[&str]) -> Result<String, AzError> {
    let out = az_command(args)
        .output()
        .map_err(|e| AzError::Other(format!("az not found: {}", e)))?;

    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();

    if !out.status.success() {
        let combined = format!("{} {}", stdout, stderr);
        if combined.contains("AADSTS")
            || combined.contains("az login")
            || combined.contains("refresh token")
        {
            return Err(AzError::NotLoggedIn);
        }
        return Err(AzError::Other(stderr));
    }

    Ok(stdout)
}
