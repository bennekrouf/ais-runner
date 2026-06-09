use std::path::PathBuf;
#[cfg(feature = "bundle-azurite")]
use std::fs;
use std::env;

/// Resolves the path to a tool (like azurite or func).
/// For Azurite: prefers the system-installed version (likely newer) over the bundled one.
/// For other tools: checks the sidecar bin/ next to the exe, then project bin/, then PATH.
pub fn resolve_tool(name: &str) -> String {
    // Special case: always prefer system Azurite — it's usually newer and more compatible
    // than the bundled binary. Bundled is a fallback for machines without Node.js.
    if name == "azurite" {
        if let Some(sys) = find_on_path("azurite") {
            return sys;
        }
        // Embedded single-EXE fallback
        if let Ok(path) = extract_embedded_azurite() {
            return path;
        }
    }

    // Check next to the executable (Sidecar / distribution mode)
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(parent) = exe_path.parent() {
            let exe_name = if cfg!(windows) { format!("{}.exe", name) } else { name.to_string() };

            // bin/<name>.exe  (flat layout)
            let flat = parent.join("bin").join(&exe_name);
            if flat.exists() {
                return flat.to_str().unwrap_or(name).to_string();
            }

            // bin/<name>/<name>.exe  (sub-folder layout, e.g. func ships with workers/)
            let nested = parent.join("bin").join(name).join(&exe_name);
            if nested.exists() {
                return nested.to_str().unwrap_or(name).to_string();
            }
        }
    }

    // Check in the project root bin/ (Development mode)
    let dev_bin = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("bin")
        .join(if cfg!(windows) { format!("{}.exe", name) } else { name.to_string() });

    if dev_bin.exists() {
        return dev_bin.to_str().unwrap_or(name).to_string();
    }

    // PATH lookup with platform-aware known-dir probes (incl. Windows .cmd/.bat).
    // This is what catches `func` / `azurite` / `mvn` on Windows GUI launches.
    if let Some(found) = find_on_path(name) {
        return found;
    }

    // Final fallback: bare name (spawn will likely fail; caller produces install hint)
    name.to_string()
}

/// Returns the full path of `name` if it can be found, else None.
/// Build a `Command` that runs `docker <args>`.
/// On Windows, wraps through `cmd /c docker` so GUI-launched apps find docker.exe
/// even when it isn't on the inherited system PATH.
pub fn docker_cmd(args: &[&str]) -> std::process::Command {
    let docker = resolve_tool("docker");
    if cfg!(target_os = "windows") {
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/c", &docker]).args(args);
        cmd
    } else {
        let mut cmd = std::process::Command::new(&docker);
        cmd.args(args);
        cmd
    }
}

/// Like `resolve_tool` but also returns a step-by-step trace of every
/// candidate path that was checked. Used by `system_check` to populate
/// diagnostics so users see exactly why a tool was/wasn't found.
pub fn resolve_tool_traced(name: &str) -> (String, Vec<String>) {
    let mut trace: Vec<String> = Vec::new();
    trace.push(format!("Resolving tool: {}", name));

    // Sidecar bin/ next to the exe
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(parent) = exe_path.parent() {
            let exe_name = if cfg!(windows) { format!("{}.exe", name) } else { name.to_string() };
            let flat = parent.join("bin").join(&exe_name);
            trace.push(format!("  sidecar bin/  → {} ({})", flat.display(), if flat.exists() { "FOUND" } else { "miss" }));
            if flat.exists() {
                return (flat.to_str().unwrap_or(name).to_string(), trace);
            }
            let nested = parent.join("bin").join(name).join(&exe_name);
            trace.push(format!("  sidecar nested → {} ({})", nested.display(), if nested.exists() { "FOUND" } else { "miss" }));
            if nested.exists() {
                return (nested.to_str().unwrap_or(name).to_string(), trace);
            }
        }
    }

    // Dev bin/
    let dev_bin = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("bin")
        .join(if cfg!(windows) { format!("{}.exe", name) } else { name.to_string() });
    trace.push(format!("  dev bin/ → {} ({})", dev_bin.display(), if dev_bin.exists() { "FOUND" } else { "miss" }));
    if dev_bin.exists() {
        return (dev_bin.to_str().unwrap_or(name).to_string(), trace);
    }

    // Platform-aware PATH lookup
    let candidates: Vec<String> = if cfg!(windows) {
        vec![format!("{}.exe", name), format!("{}.cmd", name), format!("{}.bat", name), name.to_string()]
    } else {
        vec![name.to_string()]
    };
    let extra_dirs: Vec<PathBuf> = if cfg!(target_os = "macos") {
        vec!["/opt/homebrew/bin".into(), "/usr/local/bin".into(), "/opt/local/bin".into()]
    } else if cfg!(target_os = "linux") {
        vec!["/usr/local/bin".into(), "/usr/bin".into()]
    } else if cfg!(target_os = "windows") {
        windows_extra_dirs()
    } else {
        vec![]
    };
    trace.push(format!("  probing {} dir(s) × {} candidate name(s)…", extra_dirs.len(), candidates.len()));
    for dir in &extra_dirs {
        for cand in &candidates {
            let p = dir.join(cand);
            if p.exists() {
                trace.push(format!("    {} → FOUND", p.display()));
                return (p.to_str().unwrap_or(name).to_string(), trace);
            }
        }
    }
    trace.push("  no known-dir hit".to_string());

    // Shell fallback
    let lookup_cmd = if cfg!(windows) { "where" } else { "which" };
    match std::process::Command::new(lookup_cmd).arg(name).output() {
        Ok(out) if out.status.success() => {
            let raw = String::from_utf8_lossy(&out.stdout).to_string();
            if let Some(first) = raw.lines().map(str::trim).find(|l| !l.is_empty()) {
                trace.push(format!("  `{} {}` → {}", lookup_cmd, name, first));
                return (first.to_string(), trace);
            }
            trace.push(format!("  `{} {}` returned empty", lookup_cmd, name));
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            trace.push(format!("  `{} {}` exit={} stderr={}", lookup_cmd, name, out.status.code().unwrap_or(-1), stderr));
        }
        Err(e) => trace.push(format!("  `{} {}` spawn failed: {}", lookup_cmd, name, e)),
    }

    trace.push(format!("  → falling back to bare name `{}`", name));
    (name.to_string(), trace)
}

/// GUI apps on macOS don't inherit the shell PATH (missing Homebrew &
/// npm-global), and Windows GUI apps inherit a stripped PATH that's missing
/// `%APPDATA%\npm` (where `func.cmd` / `azurite.cmd` live), the Azure CLI
/// `wbin`, Maven, Chocolatey shims, etc. This function probes well-known
/// install directories first, then falls back to `which` / `where`.
///
/// On Windows it also tries the `.exe`, `.cmd`, and `.bat` extensions because
/// Rust's `Command` only auto-resolves `.exe` — npm-installed CLI tools are
/// `.cmd` batch wrappers and otherwise look "not found".
pub fn find_on_path(name: &str) -> Option<String> {
    // Per-platform candidate filenames.
    let candidates: Vec<String> = if cfg!(windows) {
        vec![
            format!("{}.exe", name),
            format!("{}.cmd", name),
            format!("{}.bat", name),
            name.to_string(),
        ]
    } else {
        vec![name.to_string()]
    };

    // Per-platform well-known install dirs (in priority order).
    let extra_dirs: Vec<PathBuf> = if cfg!(target_os = "macos") {
        vec![
            "/opt/homebrew/bin".into(),  // Apple Silicon Homebrew
            "/usr/local/bin".into(),     // Intel Homebrew / npm-global
            "/opt/local/bin".into(),     // MacPorts
        ]
    } else if cfg!(target_os = "linux") {
        vec!["/usr/local/bin".into(), "/usr/bin".into()]
    } else if cfg!(target_os = "windows") {
        windows_extra_dirs()
    } else {
        vec![]
    };

    for dir in &extra_dirs {
        for cand in &candidates {
            let p = dir.join(cand);
            if p.exists() {
                return p.to_str().map(|s| s.to_string());
            }
        }
    }

    // Fallback: ask the shell. `where` on Windows returns paths of all
    // PATHEXT-matched files, including .cmd/.bat — handles npm-installed tools.
    let lookup_cmd = if cfg!(windows) { "where" } else { "which" };
    let out = std::process::Command::new(lookup_cmd).arg(name).output().ok()?;
    if !out.status.success() { return None; }
    let raw = String::from_utf8_lossy(&out.stdout);
    let first = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(first.to_string())
}

/// Well-known Windows bin directories. Public so the startup PATH-enrichment
/// in main.rs can prepend the same set, ensuring child processes inherit them.
#[cfg(target_os = "windows")]
pub fn windows_extra_dirs() -> Vec<PathBuf> {
    // Helper that takes `&mut Vec<PathBuf>` explicitly — avoids the closure
    // capturing `dirs` mutably and blocking later direct mutations.
    fn env_join(dirs: &mut Vec<PathBuf>, env_var: &str, sub: &str) {
        if let Ok(base) = std::env::var(env_var) {
            let p = std::path::Path::new(&base).join(sub);
            if p.is_dir() { dirs.push(p); }
        }
    }

    let mut dirs: Vec<PathBuf> = Vec::new();

    // npm-global — where `func.cmd` and `azurite.cmd` live after `npm install -g`
    env_join(&mut dirs, "APPDATA", "npm");
    // Node itself
    env_join(&mut dirs, "ProgramFiles",      "nodejs");
    env_join(&mut dirs, "ProgramFiles(x86)", "nodejs");
    // Azure CLI — three known installer layouts
    env_join(&mut dirs, "ProgramFiles(x86)", r"Microsoft SDKs\Azure\CLI2\wbin");
    env_join(&mut dirs, "ProgramFiles",      r"Microsoft SDKs\Azure\CLI2\wbin");
    env_join(&mut dirs, "LOCALAPPDATA",      r"Programs\Azure CLI\wbin");
    // Chocolatey shims
    env_join(&mut dirs, "ChocolateyInstall", "bin");
    // Scoop shims (user-level)
    env_join(&mut dirs, "USERPROFILE", r"scoop\shims");
    // Docker Desktop CLI
    env_join(&mut dirs, "ProgramFiles", r"Docker\Docker\resources\bin");

    // Maven has no fixed install location — scan Program Files for
    // apache-maven-* or Maven directories and add their bin/.
    for env in &["ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(pf) = std::env::var(env) {
            if let Ok(entries) = std::fs::read_dir(&pf) {
                for entry in entries.flatten() {
                    let n = entry.file_name();
                    let nm = n.to_string_lossy().to_lowercase();
                    if nm.starts_with("apache-maven") || nm == "maven" {
                        let bin = entry.path().join("bin");
                        if bin.is_dir() { dirs.push(bin); }
                    }
                }
            }
        }
    }
    // Some setups drop Maven under the user profile
    env_join(&mut dirs, "USERPROFILE", r".maven\bin");

    dirs
}

#[cfg(not(target_os = "windows"))]
#[allow(dead_code)]
pub fn windows_extra_dirs() -> Vec<PathBuf> { Vec::new() }

/// Extracts the embedded Azurite binary to a temp folder if it exists.
/// This enables "Single-EXE" distribution.
fn extract_embedded_azurite() -> Result<String, String> {
    #[cfg(feature = "bundle-azurite")]
    {
        // We use include_bytes! to bake the binary into our app.
        // bin/azurite must exist during compilation for this to work.
        const BYTES: &[u8] = include_bytes!("../../bin/azurite");
        
        let mut temp_path = env::temp_dir();
        // Use a versioned name so updates trigger a re-extraction
        temp_path.push(format!("ais_runner_azurite_{}", BYTES.len()));
        if cfg!(windows) { temp_path.set_extension("exe"); }

        if !temp_path.exists() || fs::metadata(&temp_path).map(|m| m.len()).unwrap_or(0) != BYTES.len() as u64 {
            fs::write(&temp_path, BYTES).map_err(|e| e.to_string())?;
            
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o755)).ok();
            }
        }
        return Ok(temp_path.to_string_lossy().into_owned());
    }

    #[cfg(not(feature = "bundle-azurite"))]
    Err("Embedded binary not enabled in this build".to_string())
}

