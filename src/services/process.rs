use std::io::BufRead;
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

/// Resolve `program` to an absolute path by searching `rich_path()`.
/// `Command::new(program)` searches the *parent* process PATH, not the env we
/// set on the child, so binaries in non-standard locations aren't found when
/// ais-runner is launched from the desktop without a full shell PATH.
pub fn resolve_bin(program: &str) -> String {
    let sep = if cfg!(target_os = "windows") { ';' } else { ':' };
    // Already absolute
    if std::path::Path::new(program).is_absolute() {
        return program.to_string();
    }
    // On Windows, try common extensions if none given
    #[cfg(target_os = "windows")]
    let suffixes: &[&str] = &[".cmd", ".exe", ".bat", ""];
    #[cfg(not(target_os = "windows"))]
    let suffixes: &[&str] = &[""];

    for dir in rich_path().split(sep) {
        for suffix in suffixes {
            let name = format!("{}{}", program, suffix);
            let candidate = std::path::Path::new(dir).join(&name);
            if candidate.is_file() {
                return candidate.to_string_lossy().to_string();
            }
        }
    }
    program.to_string()
}

/// Build a PATH that works whether ais-runner was launched from a terminal,
/// the macOS .app bundle, or a Windows desktop shortcut — all of which may
/// inherit a minimal PATH that omits package-manager and SDK directories.
pub fn rich_path() -> String {
    if cfg!(target_os = "windows") {
        rich_path_windows()
    } else {
        rich_path_unix()
    }
}

fn rich_path_unix() -> String {
    let inherited = std::env::var("PATH").unwrap_or_default();
    let extras = [
        "/opt/homebrew/bin",    // Homebrew on Apple Silicon
        "/opt/homebrew/sbin",
        "/usr/local/bin",       // Homebrew on Intel + npm global + func CLI
        "/usr/local/sbin",
        "/usr/bin",
        "/usr/sbin",
        "/bin",
        "/sbin",
    ];
    let mut parts: Vec<&str> = inherited.split(':').filter(|s| !s.is_empty()).collect();
    for extra in &extras {
        if !parts.contains(extra) {
            parts.push(extra);
        }
    }
    parts.join(":")
}

#[cfg(target_os = "windows")]
fn rich_path_windows() -> String {
    let inherited = std::env::var("PATH").unwrap_or_default();
    let mut parts: Vec<String> = inherited
        .split(';')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    // Helper: expand an env-var prefix and append a suffix, push if dir exists.
    let mut push = |env_var: &str, suffix: &str| {
        if let Ok(base) = std::env::var(env_var) {
            let p = std::path::Path::new(&base).join(suffix);
            let s = p.to_string_lossy().to_string();
            if p.is_dir() && !parts.contains(&s) {
                parts.push(s);
            }
        }
    };

    // Azure Functions Core Tools (npm global install)
    push("APPDATA",      r"npm");                          // npm global on Windows
    push("ProgramFiles", r"nodejs");                       // Node / npm bundled

    // Azure CLI
    push("ProgramFiles(x86)", r"Microsoft SDKs\Azure\CLI2\wbin");
    push("ProgramFiles",      r"Microsoft SDKs\Azure\CLI2\wbin");
    push("LOCALAPPDATA",      r"Programs\Azure CLI\wbin");

    // Azurite (npm global or standalone)
    push("APPDATA",      r"npm");                          // already added above, idempotent
    push("ProgramFiles", r"Microsoft\Azurite");

    // Node.js itself (needed by func)
    push("ProgramFiles",      r"nodejs");
    push("ProgramFiles(x86)", r"nodejs");

    // Common catch-all locations
    push("ProgramFiles",      r"Git\usr\bin");             // Git-bash utilities
    push("SystemRoot",        r"System32");
    push("SystemRoot",        "");

    parts.join(";")
}

#[cfg(not(target_os = "windows"))]
fn rich_path_windows() -> String {
    // Never called on non-Windows; satisfies the compiler.
    std::env::var("PATH").unwrap_or_default()
}

#[derive(Clone, PartialEq, Debug)]
pub enum ServiceState {
    Stopped,
    Starting,
    Running,
}

/// A managed background process (Azurite or func start).
pub struct ManagedProcess {
    pub child: Arc<Mutex<Option<Child>>>,
}

impl ManagedProcess {
    pub fn new() -> Self {
        Self {
            child: Arc::new(Mutex::new(None)),
        }
    }

    /// Spawn the process and return its stdout/stderr handles for the caller to stream.
    pub fn start(&self, program: &str, args: &[&str], workdir: Option<&str>)
        -> Result<(ChildStdout, ChildStderr), String>
    {
        let mut guard = self.child.lock().map_err(|e| e.to_string())?;
        if guard.is_some() {
            return Err("Process already started".into());
        }
        let resolved = resolve_bin(program);
        let mut cmd = Command::new(&resolved);
        cmd.args(args)
           .stdout(Stdio::piped())
           .stderr(Stdio::piped())
           .env("PATH", rich_path());
        if let Some(dir) = workdir {
            cmd.current_dir(dir);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn '{}': {}", program, e))?;
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        *guard = Some(child);
        Ok((stdout, stderr))
    }

    pub fn stop(&self) -> Result<(), String> {
        let mut guard = self.child.lock().map_err(|e| e.to_string())?;
        if let Some(mut child) = guard.take() {
            let _ = child.kill();
            let _ = child.wait(); // reap exit status so the kernel doesn't zombie the process
        }
        Ok(())
    }
}

/// Spawn two background threads that read stdout/stderr line-by-line and send to `tx`.
/// `stderr_only` filters stdout to lines with error/warning keywords to reduce noise.
/// Returns the thread handles so callers can join them on shutdown if needed.
pub fn stream_output(
    stdout: ChildStdout,
    stderr: ChildStderr,
    tx: tokio::sync::mpsc::UnboundedSender<(String, bool)>, // (line, is_err)
    stdout_filter: bool,
) -> (std::thread::JoinHandle<()>, std::thread::JoinHandle<()>) {
    let tx_out = tx.clone();
    let h_out = std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines().flatten() {
            if !line_is_suppressed(&line) && (!stdout_filter || line_is_notable(&line)) {
                let _ = tx_out.send((line, false));
            }
        }
    });
    let h_err = std::thread::spawn(move || {
        for line in std::io::BufReader::new(stderr).lines().flatten() {
            if !line_is_suppressed(&line) {
                let _ = tx.send((line, true));
            }
        }
    });
    (h_out, h_err)
}

/// Returns true for lines that are never worth showing regardless of stream.
fn line_is_suppressed(line: &str) -> bool {
    // Strip optional [ISO-timestamp] prefix emitted by the func host.
    let s = line.trim_start();
    let s = if s.starts_with('[') {
        s.find("] ").map(|i| s[i + 2..].trim_start()).unwrap_or(s)
    } else {
        s
    };
    // .NET stack-frame lines: "at Namespace.Class.Method(args)"
    s.starts_with("at ") && s.contains('.') && s.contains('(')
}

fn line_is_notable(line: &str) -> bool {
    let l = line.to_lowercase();
    l.contains("error") || l.contains("warn") || l.contains("fail")
        || l.contains("exception") || l.contains("loaded") || l.contains("listening")
        || l.contains("started") || l.contains("starting")
}
