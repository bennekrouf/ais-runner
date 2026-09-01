use std::io::BufRead;
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// Every child this process has spawned, weakly held so one dropped normally
/// leaves nothing behind here.
///
/// Exists because closing the window stops nothing on its own: `Child` has no
/// `Drop` that kills, so emulators, func hosts and stubs were reparented to
/// init and kept running — orphans with `ppid=1` surviving for days, `<defunct>`
/// children never reaped, Docker containers still up long after the app quit.
type ChildHandle = Arc<Mutex<Option<Child>>>;

fn registry() -> &'static Mutex<Vec<Weak<Mutex<Option<Child>>>>> {
    static REGISTRY: OnceLock<Mutex<Vec<Weak<Mutex<Option<Child>>>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

fn register(handle: &ChildHandle) {
    if let Ok(mut reg) = registry().lock() {
        reg.retain(|w| w.strong_count() > 0);
        reg.push(Arc::downgrade(handle));
    }
}

/// Ask a child to exit, escalating to SIGKILL only if it ignores the request.
///
/// `Child::kill` is SIGKILL, which `docker compose up` cannot act on — its
/// containers would keep running. SIGTERM lets it tear them down first.
fn terminate(child: &mut Child) {
    let pid = child.id();

    // Signal the whole tree, not just the child we hold: `func host start`
    // spawns a language worker (a Java process for this project), and killing
    // only the parent leaves that worker orphaned. Children are spawned into
    // their own process group so a negative pid reaches the group on Unix;
    // Windows gets the same reach via `taskkill /T`.
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-TERM", &format!("-{pid}")])
            .status();
        // Not a group leader after all — fall back to the process itself.
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/PID", &pid.to_string()])
            .output();
    }

    for _ in 0..40 {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(_) => break,
        }
    }

    // Ignored the polite request — force it, tree included.
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-KILL", &format!("-{pid}")])
            .status();
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .output();
    }
    let _ = child.kill();
    let _ = child.wait(); // reap, or the kernel keeps a <defunct> entry
}

/// Stop every child still running. Call on shutdown, once the UI has exited.
pub fn stop_all() -> usize {
    let handles: Vec<ChildHandle> = match registry().lock() {
        Ok(reg) => reg.iter().filter_map(|w| w.upgrade()).collect(),
        Err(_) => return 0,
    };
    let mut stopped = 0;
    for handle in handles {
        if let Ok(mut guard) = handle.lock() {
            if let Some(mut child) = guard.take() {
                terminate(&mut child);
                stopped += 1;
            }
        }
    }
    stopped
}

/// Resolve `program` to an absolute path by searching `rich_path()`.
/// `Command::new(program)` searches the *parent* process PATH, not the env we
/// set on the child, so binaries in non-standard locations aren't found when
/// ais-runner is launched from the desktop without a full shell PATH.
pub fn resolve_bin(program: &str) -> String {
    let sep = if cfg!(target_os = "windows") {
        ';'
    } else {
        ':'
    };
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
        "/opt/homebrew/bin", // Homebrew on Apple Silicon
        "/opt/homebrew/sbin",
        "/usr/local/bin", // Homebrew on Intel + npm global + func CLI
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
    push("APPDATA", r"npm"); // npm global on Windows
    push("ProgramFiles", r"nodejs"); // Node / npm bundled

    // Azure CLI
    push("ProgramFiles(x86)", r"Microsoft SDKs\Azure\CLI2\wbin");
    push("ProgramFiles", r"Microsoft SDKs\Azure\CLI2\wbin");
    push("LOCALAPPDATA", r"Programs\Azure CLI\wbin");

    // Azurite (npm global or standalone)
    push("APPDATA", r"npm"); // already added above, idempotent
    push("ProgramFiles", r"Microsoft\Azurite");

    // Node.js itself (needed by func)
    push("ProgramFiles", r"nodejs");
    push("ProgramFiles(x86)", r"nodejs");

    // Common catch-all locations
    push("ProgramFiles", r"Git\usr\bin"); // Git-bash utilities
    push("SystemRoot", r"System32");
    push("SystemRoot", "");

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
    pub fn start(
        &self,
        program: &str,
        args: &[&str],
        workdir: Option<&str>,
    ) -> Result<(ChildStdout, ChildStderr), String> {
        let mut guard = self.child.lock().map_err(|e| e.to_string())?;
        if guard.is_some() {
            return Err("Process already started".into());
        }
        if let Some(dir) = workdir {
            if !std::path::Path::new(dir).is_dir() {
                return Err(format!(
                    "Working directory '{}' does not exist — create it first.",
                    dir
                ));
            }
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
        // Own process group, so terminate() can signal the whole tree.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn '{}': {}", resolved, e))?;
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        *guard = Some(child);
        drop(guard);
        register(&self.child);
        Ok((stdout, stderr))
    }

    /// Like `start` but also injects extra environment variables.
    pub fn start_with_env(
        &self,
        program: &str,
        args: &[&str],
        workdir: Option<&str>,
        extra_env: &[(String, String)],
    ) -> Result<(ChildStdout, ChildStderr), String> {
        let mut guard = self.child.lock().map_err(|e| e.to_string())?;
        if guard.is_some() {
            return Err("Process already started".into());
        }
        if let Some(dir) = workdir {
            if !std::path::Path::new(dir).is_dir() {
                return Err(format!("Working directory '{}' does not exist.", dir));
            }
        }
        let resolved = resolve_bin(program);
        let mut cmd = Command::new(&resolved);
        cmd.args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("PATH", rich_path());
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        if let Some(dir) = workdir {
            cmd.current_dir(dir);
        }
        // Own process group, so terminate() can signal the whole tree.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn '{}': {}", resolved, e))?;
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        *guard = Some(child);
        drop(guard);
        register(&self.child);
        Ok((stdout, stderr))
    }

    pub fn stop(&self) -> Result<(), String> {
        let mut guard = self.child.lock().map_err(|e| e.to_string())?;
        if let Some(mut child) = guard.take() {
            terminate(&mut child);
        }
        Ok(())
    }
}

/// Last line of defence: a handle going out of scope must not leave the child
/// running. `stop_all` covers the normal shutdown path; this covers everything
/// else (a panic, a signal handled elsewhere, a screen dropping its state).
impl Drop for ManagedProcess {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.child.lock() {
            if let Some(mut child) = guard.take() {
                terminate(&mut child);
            }
        }
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
    l.contains("error")
        || l.contains("warn")
        || l.contains("fail")
        || l.contains("exception")
        || l.contains("loaded")
        || l.contains("listening")
        || l.contains("started")
        || l.contains("starting")
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;

    /// `stop_all` acts on a process-wide registry, so these tests cannot run
    /// concurrently — one would stop another's child mid-assertion.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Spawn something long-running the same way the app does.
    fn long_running() -> ManagedProcess {
        let p = ManagedProcess::new();
        p.start("sh", &["-c", "sleep 300"], None).expect("spawn");
        p
    }

    fn alive(pid: u32) -> bool {
        // signal 0 only checks for existence; a zombie is NOT alive for our
        // purposes, so exclude anything the parent has already reaped.
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// The whole point: closing the app must not leave the child running.
    /// Before this, `Child` had no killing `Drop`, so it was reparented to init.
    #[test]
    fn dropping_the_handle_kills_the_child() {
        let _guard = serial();
        let proc = long_running();
        let pid = proc.child.lock().unwrap().as_ref().unwrap().id();
        assert!(alive(pid), "child should be running before drop");

        drop(proc);
        assert!(!alive(pid), "child {pid} survived the drop");
    }

    /// The case that was actually broken in the field: `func host start` spawns
    /// a language worker, so killing only the handle we hold left that worker
    /// running. The child is its own process group, so the signal reaches it.
    #[cfg(unix)]
    #[test]
    fn killing_the_child_also_kills_its_grandchild() {
        let _guard = serial();
        let pidfile = std::env::temp_dir()
            .join(format!("ais_gc_{}", std::process::id()))
            .to_string_lossy()
            .to_string();
        let proc = ManagedProcess::new();
        // Parent sleeps; the grandchild is a separate long-lived process.
        proc.start(
            "sh",
            &["-c", &format!("sleep 300 & echo $! > {pidfile}; sleep 300")],
            None,
        )
        .expect("spawn");
        std::thread::sleep(std::time::Duration::from_millis(400));
        let gc: u32 = std::fs::read_to_string(&pidfile)
            .expect("grandchild pid file")
            .trim()
            .parse()
            .expect("pid");
        assert!(alive(gc), "grandchild should be running");

        drop(proc);
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            !alive(gc),
            "grandchild {gc} survived — the tree was not signalled"
        );
        let _ = std::fs::remove_file(&pidfile);
    }

    /// `stop_all` is what the shutdown path calls, and it must reap too —
    /// a killed-but-unreaped child shows up as <defunct>.
    #[test]
    fn stop_all_stops_and_reaps() {
        let _guard = serial();
        let proc = long_running();
        let pid = proc.child.lock().unwrap().as_ref().unwrap().id();

        assert!(stop_all() >= 1, "stop_all should report stopping it");
        assert!(!alive(pid), "child {pid} survived stop_all");
        // Taken from the handle, so a later drop cannot double-kill.
        assert!(proc.child.lock().unwrap().is_none());
    }
}
