use std::io::BufRead;
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

/// Resolve `program` to an absolute path by searching `rich_path()`.
/// `Command::new(program)` searches the *parent* process PATH, not the env we
/// set on the child, so binaries in /opt/homebrew/bin aren't found when
/// ais-runner is launched without a full shell PATH.
pub fn resolve_bin(program: &str) -> String {
    if program.starts_with('/') {
        return program.to_string();
    }
    for dir in rich_path().split(':') {
        let candidate = std::path::Path::new(dir).join(program);
        if candidate.is_file() {
            return candidate.to_string_lossy().to_string();
        }
    }
    program.to_string()
}

/// Build a PATH that works whether ais-runner was launched from a terminal or
/// from the macOS .app bundle (which inherits only the system minimal PATH).
pub fn rich_path() -> String {
    let inherited = std::env::var("PATH").unwrap_or_default();
    let extras = [
        "/opt/homebrew/bin",   // Homebrew on Apple Silicon
        "/opt/homebrew/sbin",
        "/usr/local/bin",      // Homebrew on Intel + npm global + func CLI
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
            child.kill().map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

/// Spawn two background threads that read stdout/stderr line-by-line and send to `tx`.
/// `stderr_only` filters stdout to lines with error/warning keywords to reduce noise.
pub fn stream_output(
    stdout: ChildStdout,
    stderr: ChildStderr,
    tx: tokio::sync::mpsc::UnboundedSender<(String, bool)>, // (line, is_err)
    stdout_filter: bool,
) {
    let tx_out = tx.clone();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines().flatten() {
            if !stdout_filter || line_is_notable(&line) {
                let _ = tx_out.send((line, false));
            }
        }
    });
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stderr).lines().flatten() {
            let _ = tx.send((line, true));
        }
    });
}

fn line_is_notable(line: &str) -> bool {
    let l = line.to_lowercase();
    l.contains("error") || l.contains("warn") || l.contains("fail")
        || l.contains("exception") || l.contains("loaded") || l.contains("listening")
        || l.contains("started") || l.contains("starting")
}
