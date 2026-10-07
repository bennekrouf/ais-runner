use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use crate::services::runtime_manager;

/// How long one probe may take before it counts as unavailable. `az --version`
/// is the slow one (a Python start plus an online version lookup); everything
/// else answers in well under a second.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// How to install the Azure Functions Core Tools, per platform.
///
/// macOS gets the Homebrew tap rather than npm: the npm package ships a stub
/// that downloads and unzips the real CLI in a postinstall step, and when that
/// download fails it still leaves a `func` on PATH that errors on every run
/// (`Error extracting zip file: ENOENT …`). The tap installs a real binary.
///
/// Note there is no `--unsafe-perm`: npm 9 removed it, and npm 11 now warns
/// `Unknown cli config "--unsafe-perm"`.
#[cfg(target_os = "macos")]
pub const FUNC_INSTALL_HINT: &str =
    "brew tap azure/functions && brew install azure-functions-core-tools@4";
#[cfg(not(target_os = "macos"))]
pub const FUNC_INSTALL_HINT: &str = "npm install -g azure-functions-core-tools@4";

#[derive(Debug, Clone, PartialEq)]
pub struct ToolStatus {
    pub name: &'static str,
    pub available: bool,
    pub version: Option<String>,
    pub install_hint: &'static str,
    /// Step-by-step trace of resolution + spawn attempt. Surfaced in the UI
    /// tooltip and also written to a log file. Always populated so users can
    /// see what was tried even on success.
    pub diagnostic: String,
}

/// Probes run side by side: each is bounded by `PROBE_TIMEOUT`, so opening a
/// project waits for the slowest tool rather than the sum of all of them.
pub fn check_tools() -> Vec<ToolStatus> {
    let results = std::thread::scope(|s| {
        let probes = [
            s.spawn(|| probe("func", &["--version"], FUNC_INSTALL_HINT)),
            s.spawn(|| probe("azurite", &["--version"], "npm install -g azurite")),
            s.spawn(|| probe("az", &["--version"], "https://aka.ms/installazurecli")),
            s.spawn(|| probe("node", &["--version"], "https://nodejs.org")),
            s.spawn(|| {
                probe(
                    "mvn",
                    &["--version"],
                    "https://maven.apache.org/install.html",
                )
            }),
            s.spawn(probe_docker),
        ];
        probes
            .into_iter()
            .filter_map(|h| h.join().ok())
            .collect::<Vec<_>>()
    });
    write_diagnostic_log(&results);
    results
}

/// `Command::output`, but giving up after `timeout`.
///
/// A plain `output()` waits for the child to exit *and* for its pipes to
/// close, with no limit. On Windows `docker info` blocks for as long as Docker
/// Desktop is starting or its WSL engine is wedged, and a `.cmd` shim can leave
/// a grandchild holding the pipes open — either one left the project window on
/// its "Initializing…" spinner for good. On timeout the process tree is killed
/// and the reader threads are abandoned rather than joined, since a surviving
/// grandchild may keep them blocked.
fn output_with_timeout(cmd: &mut Command, timeout: Duration) -> std::io::Result<Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // No console window flashing up for each probe, and none for a child to
    // sit waiting on.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn()?;

    let (tx, rx) = mpsc::channel();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let err_reader = std::thread::spawn(move || {
            if let Some(e) = stderr.as_mut() {
                let _ = e.read_to_end(&mut err);
            }
            err
        });
        if let Some(o) = stdout.as_mut() {
            let _ = o.read_to_end(&mut out);
        }
        let err = err_reader.join().unwrap_or_default();
        let _ = tx.send((out, err));
    });

    match rx.recv_timeout(timeout) {
        Ok((stdout, stderr)) => {
            let status = child.wait()?;
            Ok(Output {
                status,
                stdout,
                stderr,
            })
        }
        Err(_) => {
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                let mut kill = Command::new("taskkill");
                kill.args(["/F", "/T", "/PID", &child.id().to_string()]);
                kill.creation_flags(0x0800_0000);
                let _ = kill.output();
            }
            let _ = child.kill();
            let _ = child.wait();
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("no answer after {}s", timeout.as_secs()),
            ))
        }
    }
}

/// Persist every tool's diagnostic to disk so the user can grab it for support.
/// Path: `<data-local>/AIS Runner/tool-check.log`. Best-effort — failures are silent.
fn write_diagnostic_log(results: &[ToolStatus]) {
    let log_dir = dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("AIS Runner");
    let _ = std::fs::create_dir_all(&log_dir);

    let mut buf = format!(
        "── ais-runner tool check @ {} ──\n\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    );
    buf.push_str(&format!(
        "PATH ({} entries):\n",
        std::env::var("PATH")
            .unwrap_or_default()
            .split(if cfg!(windows) { ';' } else { ':' })
            .count()
    ));
    for entry in std::env::var("PATH")
        .unwrap_or_default()
        .split(if cfg!(windows) { ';' } else { ':' })
    {
        if !entry.is_empty() {
            buf.push_str(&format!("  {}\n", entry));
        }
    }
    buf.push('\n');

    for t in results {
        buf.push_str(&format!(
            "── {} ── {}\n",
            t.name,
            if t.available {
                "✓ available"
            } else {
                "✗ unavailable"
            }
        ));
        if let Some(v) = &t.version {
            buf.push_str(&format!("   version: {}\n", v));
        }
        buf.push_str(&format!("   install hint: {}\n", t.install_hint));
        buf.push_str(&t.diagnostic);
        buf.push_str("\n\n");
    }

    let _ = std::fs::write(log_dir.join("tool-check.log"), buf);
}

/// Check Docker: distinguish between not installed, installed-but-stopped,
/// and running.  The install_hint guides the user accordingly.
fn probe_docker() -> ToolStatus {
    let mut trace = String::new();

    // Resolve through the platform-aware probe so GUI launches that don't
    // inherit the user's shell PATH still find Docker Desktop's CLI (lives
    // under `/usr/local/bin`, `~/.docker/bin`, or
    // `/Applications/Docker.app/Contents/Resources/bin` depending on the
    // install vintage).
    let (resolved, resolve_trace) = runtime_manager::resolve_tool_traced("docker");
    for line in resolve_trace {
        trace.push_str(&line);
        trace.push('\n');
    }

    // Step 1 — is the docker CLI present?
    let cli = output_with_timeout(Command::new(&resolved).args(["--version"]), PROBE_TIMEOUT);
    let version = match &cli {
        Ok(out) if out.status.success() => {
            let raw = String::from_utf8_lossy(&out.stdout).to_string();
            trace.push_str(&format!("   docker --version: ok ({})\n", raw.trim()));
            raw.lines()
                .map(|l| l.trim())
                .find(|l| !l.is_empty())
                .map(|l| l.to_string())
        }
        Ok(out) => {
            trace.push_str(&format!(
                "   docker --version: exit={} stderr={}\n",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
            None
        }
        Err(e) => {
            trace.push_str(&format!("   docker --version: spawn failed: {}\n", e));
            None
        }
    };

    if version.is_none() {
        return ToolStatus {
            name: "docker",
            available: false,
            version: None,
            install_hint: "https://www.docker.com/products/docker-desktop/",
            diagnostic: trace,
        };
    }

    // Step 2 — is the daemon running?
    match output_with_timeout(Command::new(&resolved).args(["info"]), PROBE_TIMEOUT) {
        Ok(out) if out.status.success() => {
            trace.push_str("   docker info: ok (daemon running)\n");
            ToolStatus {
                name: "docker",
                available: true,
                version,
                install_hint: "",
                diagnostic: trace,
            }
        }
        Ok(out) => {
            trace.push_str(&format!(
                "   docker info: exit={} (daemon not running)\n",
                out.status.code().unwrap_or(-1)
            ));
            ToolStatus {
                name: "docker",
                available: false,
                version,
                install_hint:
                    "Docker Desktop is installed but not running — start it from the Start Menu",
                diagnostic: trace,
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            trace.push_str(&format!("   docker info: {}\n", e));
            ToolStatus {
                name: "docker",
                available: false,
                version,
                install_hint:
                    "Docker Desktop is not responding — wait for it to finish starting, or restart it",
                diagnostic: trace,
            }
        }
        Err(e) => {
            trace.push_str(&format!("   docker info: spawn failed: {}\n", e));
            ToolStatus {
                name: "docker",
                available: false,
                version,
                install_hint:
                    "Docker Desktop is installed but not running — start it from the Start Menu",
                diagnostic: trace,
            }
        }
    }
}

fn probe(name: &'static str, args: &[&str], install_hint: &'static str) -> ToolStatus {
    let (resolved, trace_steps) = runtime_manager::resolve_tool_traced(name);
    let mut diagnostic = trace_steps.join("\n");
    diagnostic.push('\n');

    let needs_shell = cfg!(windows)
        && (resolved.to_lowercase().ends_with(".cmd") || resolved.to_lowercase().ends_with(".bat"));
    let spawn_label = if needs_shell {
        format!("cmd /c {} {}", resolved, args.join(" "))
    } else {
        format!("{} {}", resolved, args.join(" "))
    };

    let result = if needs_shell {
        output_with_timeout(
            Command::new("cmd").args(["/c", &resolved]).args(args),
            PROBE_TIMEOUT,
        )
    } else {
        output_with_timeout(Command::new(&resolved).args(args), PROBE_TIMEOUT)
    };

    match result {
        Ok(out) if out.status.success() => {
            let raw = String::from_utf8_lossy(&out.stdout).to_string();
            let version = raw
                .lines()
                .map(|l| l.trim())
                .find(|l| !l.is_empty())
                .map(String::from);
            diagnostic.push_str(&format!("Spawn: `{}` → ok\n", spawn_label));
            if let Some(v) = &version {
                diagnostic.push_str(&format!("Version: {}\n", v));
            }
            return ToolStatus {
                name,
                available: true,
                version,
                install_hint,
                diagnostic,
            };
        }
        Ok(out) => {
            diagnostic.push_str(&format!(
                "Spawn: `{}` → exit={} stderr={}\n",
                spawn_label,
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Err(e) => {
            diagnostic.push_str(&format!("Spawn: `{}` → error: {}\n", spawn_label, e));
        }
    }

    // Windows last-ditch: cmd /c <name> (PATHEXT-aware, catches anything missed)
    if cfg!(windows) && resolved == name {
        let fallback_label = format!("cmd /c {} {}", name, args.join(" "));
        match output_with_timeout(
            Command::new("cmd").args(["/c", name]).args(args),
            PROBE_TIMEOUT,
        ) {
            Ok(out) if out.status.success() => {
                let raw = String::from_utf8_lossy(&out.stdout).to_string();
                let version = raw
                    .lines()
                    .map(|l| l.trim())
                    .find(|l| !l.is_empty())
                    .map(String::from);
                diagnostic.push_str(&format!("Fallback: `{}` → ok\n", fallback_label));
                if let Some(v) = &version {
                    diagnostic.push_str(&format!("Version: {}\n", v));
                }
                return ToolStatus {
                    name,
                    available: true,
                    version,
                    install_hint,
                    diagnostic,
                };
            }
            Ok(out) => {
                diagnostic.push_str(&format!(
                    "Fallback: `{}` → exit={} stderr={}\n",
                    fallback_label,
                    out.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            Err(e) => {
                diagnostic.push_str(&format!("Fallback: `{}` → error: {}\n", fallback_label, e));
            }
        }
    }

    ToolStatus {
        name,
        available: false,
        version: None,
        install_hint,
        diagnostic,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_probe_that_never_answers_is_given_up_on() {
        let started = std::time::Instant::now();
        let res = output_with_timeout(Command::new("sleep").arg("30"), Duration::from_millis(300));
        assert_eq!(res.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_probe_that_answers_keeps_its_output() {
        let out = output_with_timeout(
            Command::new("sh").args(["-c", "echo out; echo err >&2"]),
            PROBE_TIMEOUT,
        )
        .unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "out");
        assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "err");
    }
}
