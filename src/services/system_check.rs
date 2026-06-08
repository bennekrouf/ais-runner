use std::process::Command;

use crate::services::runtime_manager;

#[derive(Debug, Clone, PartialEq)]
pub struct ToolStatus {
    pub name:         &'static str,
    pub available:    bool,
    pub version:      Option<String>,
    pub install_hint: &'static str,
    /// Step-by-step trace of resolution + spawn attempt. Surfaced in the UI
    /// tooltip and also written to a log file. Always populated so users can
    /// see what was tried even on success.
    pub diagnostic:   String,
}

pub fn check_tools() -> Vec<ToolStatus> {
    let results = vec![
        probe("func",    &["--version"], "npm install -g azure-functions-core-tools@4"),
        probe("azurite", &["--version"], "npm install -g azurite"),
        probe("az",      &["--version"], "https://aka.ms/installazurecli"),
        probe("node",    &["--version"], "https://nodejs.org"),
        probe("mvn",     &["--version"], "https://maven.apache.org/install.html"),
        probe_docker(),
    ];
    write_diagnostic_log(&results);
    results
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
        std::env::var("PATH").unwrap_or_default().split(if cfg!(windows) { ';' } else { ':' }).count()
    ));
    for entry in std::env::var("PATH").unwrap_or_default().split(if cfg!(windows) { ';' } else { ':' }) {
        if !entry.is_empty() {
            buf.push_str(&format!("  {}\n", entry));
        }
    }
    buf.push('\n');

    for t in results {
        buf.push_str(&format!(
            "── {} ── {}\n",
            t.name,
            if t.available { "✓ available" } else { "✗ unavailable" }
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

    // Step 1 — is the docker CLI present?
    let cli = Command::new("docker").args(["--version"]).output();
    let version = match &cli {
        Ok(out) if out.status.success() => {
            let raw = String::from_utf8_lossy(&out.stdout).to_string();
            trace.push_str(&format!("   docker --version: ok ({})\n", raw.trim()));
            raw.lines().map(|l| l.trim()).find(|l| !l.is_empty()).map(|l| l.to_string())
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
            name:         "docker",
            available:    false,
            version:      None,
            install_hint: "https://www.docker.com/products/docker-desktop/",
            diagnostic:   trace,
        };
    }

    // Step 2 — is the daemon running?
    match Command::new("docker").args(["info"]).output() {
        Ok(out) if out.status.success() => {
            trace.push_str("   docker info: ok (daemon running)\n");
            ToolStatus {
                name: "docker", available: true, version,
                install_hint: "", diagnostic: trace,
            }
        }
        Ok(out) => {
            trace.push_str(&format!(
                "   docker info: exit={} (daemon not running)\n",
                out.status.code().unwrap_or(-1)
            ));
            ToolStatus {
                name: "docker", available: false, version,
                install_hint: "Docker Desktop is installed but not running — start it from the Start Menu",
                diagnostic: trace,
            }
        }
        Err(e) => {
            trace.push_str(&format!("   docker info: spawn failed: {}\n", e));
            ToolStatus {
                name: "docker", available: false, version,
                install_hint: "Docker Desktop is installed but not running — start it from the Start Menu",
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
        Command::new("cmd").args(["/c", &resolved]).args(args).output()
    } else {
        Command::new(&resolved).args(args).output()
    };

    match result {
        Ok(out) if out.status.success() => {
            let raw = String::from_utf8_lossy(&out.stdout).to_string();
            let version = raw.lines().map(|l| l.trim()).find(|l| !l.is_empty()).map(String::from);
            diagnostic.push_str(&format!("Spawn: `{}` → ok\n", spawn_label));
            if let Some(v) = &version {
                diagnostic.push_str(&format!("Version: {}\n", v));
            }
            return ToolStatus { name, available: true, version, install_hint, diagnostic };
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
        match Command::new("cmd").args(["/c", name]).args(args).output() {
            Ok(out) if out.status.success() => {
                let raw = String::from_utf8_lossy(&out.stdout).to_string();
                let version = raw.lines().map(|l| l.trim()).find(|l| !l.is_empty()).map(String::from);
                diagnostic.push_str(&format!("Fallback: `{}` → ok\n", fallback_label));
                if let Some(v) = &version {
                    diagnostic.push_str(&format!("Version: {}\n", v));
                }
                return ToolStatus { name, available: true, version, install_hint, diagnostic };
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

    ToolStatus { name, available: false, version: None, install_hint, diagnostic }
}
