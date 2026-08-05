use std::sync::Arc;
use dioxus::prelude::*;

use crate::components::log_panel::{LogLevel, LogLine};
use crate::services::process::{ManagedProcess, ServiceState, rich_path};
use crate::utils::make_push;

pub fn handle_start(
    mut state: Signal<ServiceState>,
    proc: Signal<Arc<ManagedProcess>>,
    log_lines: Signal<Vec<LogLine>>,
    mut java_lines: Signal<Vec<String>>,
    func_apps_dir: &str,
) {
    let mut push = make_push(log_lines);
    let dir = func_apps_dir.to_string();

    // Check the directory exists before doing anything
    if !std::path::Path::new(&dir).exists() {
        state.set(ServiceState::Stopped);
        push(format!("⚠ function_apps directory not found: {}", dir), LogLevel::Error);
        push("  hint: create a Maven Azure Functions project in that folder.".into(), LogLevel::Warn);
        return;
    }

    state.set(ServiceState::Starting);

    spawn(async move {
        let mut push = make_push(log_lines);

        // ── Port 7072 already in use? Reclaim only if it's OUR project. ───
        if tokio::net::TcpStream::connect("127.0.0.1:7072").await.is_ok() {
            // Identify the owner so we don't silently kill a *different*
            // project's function host (the exact multi-clone footgun: one repo
            // holds :7072 while you're trying to run another).
            let owner_dir = dir.clone();
            let foreign = tokio::task::spawn_blocking(move || {
                crate::services::port_owner::owner(7072).map(|o| {
                    let ours = crate::services::port_owner::belongs_to(&o, &owner_dir);
                    (ours, o.pid, o.detail)
                })
            }).await.ok().flatten();

            if let Some((false, pid, detail)) = &foreign {
                // A different project owns 7072 — do NOT kill it.
                push(format!(
                    "⛔ Port 7072 is held by a DIFFERENT project's function host (PID {pid}){}. \
                     Stop that one first (or run this project's functions there) — \
                     ais-runner won't kill another project's host for you.",
                    if detail.is_empty() { String::new() } else { format!(":\n     {}", detail) }
                ), LogLevel::Error);
                state.set(ServiceState::Stopped);
                return;
            }

            push("Port 7072 in use by a stale instance of this project — reclaiming…".into(), LogLevel::Warn);
            tokio::task::spawn_blocking(|| {
                if cfg!(target_os = "windows") {
                    if let Ok(out) = std::process::Command::new("cmd")
                        .args(["/c", "for /f \"tokens=5\" %a in ('netstat -aon ^| findstr :7072') do taskkill /F /PID %a"])
                        .output() { let _ = out; }
                } else if let Ok(out) = std::process::Command::new("lsof")
                    .args(["-ti", ":7072"])
                    .output()
                {
                    let pids = String::from_utf8_lossy(&out.stdout);
                    for pid in pids.split_whitespace() {
                        let _ = std::process::Command::new("kill").args(["-9", pid]).status();
                    }
                }
            }).await.ok();
            // Wait for the port to be released
            for _ in 0..10 {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                if tokio::net::TcpStream::connect("127.0.0.1:7072").await.is_err() { break; }
            }
        }

        // ── Step 1: mvn package -DskipTests ──────────────────────────────
        push(format!("$ cd {} && mvn package -DskipTests", dir), LogLevel::Info);

        let pkg_result = tokio::task::spawn_blocking({
            let d = dir.clone();
            move || {
                std::process::Command::new("mvn")
                    .args(["package", "-DskipTests"])
                    .current_dir(&d)
                    .env("PATH", rich_path())
                    .output()
            }
        }).await;

        let pkg_ok = match pkg_result {
            Ok(Ok(out)) => {
                // Stream notable lines from package output (errors/warnings only)
                let combined = format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr),
                );
                for line in combined.lines() {
                    let l = line.trim();
                    if l.is_empty() { continue; }
                    // Show errors and build summary, suppress download noise
                    if l.starts_with("[ERROR]") {
                        push(l.to_string(), LogLevel::Error);
                    } else if l.starts_with("[WARNING]") && !l.contains("deprecated") {
                        push(l.to_string(), LogLevel::Warn);
                    } else if l.contains("BUILD SUCCESS") || l.contains("BUILD FAILURE") {
                        push(l.to_string(), if l.contains("SUCCESS") { LogLevel::Ok } else { LogLevel::Error });
                    }
                }
                if out.status.success() {
                    push("✅ mvn package complete — starting Java Function App…".into(), LogLevel::Ok);
                    true
                } else {
                    push("❌ mvn package failed — fix the errors above before retrying.".into(), LogLevel::Error);
                    false
                }
            }
            Ok(Err(e)) => {
                push(format!("❌ Could not run mvn: {}", e), LogLevel::Error);
                push("  hint: install Maven — brew install maven".into(), LogLevel::Warn);
                false
            }
            Err(e) => {
                push(format!("❌ spawn error: {}", e), LogLevel::Error);
                false
            }
        };

        if !pkg_ok {
            state.set(ServiceState::Stopped);
            return;
        }

        // ── Step 2: func host start --port 7072 ──────────────────────────
        // Run func directly in the staging dir to avoid the Maven plugin's
        // hardcoded port 7071 which conflicts with Logic Apps func start.
        let staging  = format!("{}/target/azure-functions/ais-functions", dir);
        let func     = crate::services::runtime_manager::resolve_tool("func");
        let java_home = detect_java_home();
        if let Some(ref jh) = java_home {
            let msg = format!("JAVA_HOME={}", jh);
            push(msg.clone(), LogLevel::Info);
            java_lines.write().push(msg);
        }
        let cmd_msg = format!("$ cd {} && {} host start --port 7072", staging, func);
        push(cmd_msg.clone(), LogLevel::Info);
        java_lines.write().push(cmd_msg);

        let mut cmd_env: Vec<(String, String)> = vec![];
        if let Some(jh) = java_home {
            cmd_env.push(("JAVA_HOME".into(), jh));
        }

        match proc.read().start_with_env(&func, &["host", "start", "--port", "7072"], Some(&staging), &cmd_env) {
            Ok((stdout, stderr)) => {
                state.set(ServiceState::Running);
                let start_msg = "Java Function App starting on port 7072…".to_string();
                push(start_msg.clone(), LogLevel::Ok);
                java_lines.write().push(start_msg);

                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, bool)>();
                crate::services::process::stream_output(stdout, stderr, tx, true);

                spawn(async move {
                    while let Some((line, _is_err)) = rx.recv().await {
                        if !crate::components::log_panel::is_mvn_noise(&line) {
                            let mut w = java_lines.write();
                            w.push(line);
                            let len = w.len();
                            if len > 2000 { let d = len - 2000; w.drain(..d); }
                        }
                    }
                });
            }
            Err(e) => {
                state.set(ServiceState::Stopped);
                push(format!("Java Function App error: {}", e), LogLevel::Error);
            }
        }
    });
}

/// Detect JAVA_HOME so `func host start` can launch the Java worker.
/// GUI apps on macOS don't inherit the shell PATH, so we probe common locations.
fn detect_java_home() -> Option<String> {
    // 1. Already set in environment
    if let Ok(jh) = std::env::var("JAVA_HOME") {
        if !jh.is_empty() && std::path::Path::new(&jh).exists() {
            return Some(jh);
        }
    }
    // 2. Ask java_home utility (macOS)
    if let Ok(out) = std::process::Command::new("/usr/libexec/java_home").output() {
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !path.is_empty() && std::path::Path::new(&path).exists() {
            return Some(path);
        }
    }
    // 3. Derive from `java` binary on PATH
    let java = crate::services::runtime_manager::resolve_tool("java");
    if let Ok(canonical) = std::fs::canonicalize(&java) {
        // .../jdk/bin/java → parent of bin/ is JAVA_HOME
        if let Some(bin) = canonical.parent() {
            if let Some(home) = bin.parent() {
                if home.join("lib").exists() || home.join("include").exists() {
                    return Some(home.to_string_lossy().to_string());
                }
            }
        }
    }
    // 4. Common Homebrew / SDKMAN paths
    let candidates = [
        "/opt/homebrew/opt/openjdk/libexec/openjdk.jdk/Contents/Home",
        "/usr/local/opt/openjdk/libexec/openjdk.jdk/Contents/Home",
        "/Library/Java/JavaVirtualMachines/temurin-17.jdk/Contents/Home",
        "/Library/Java/JavaVirtualMachines/openjdk-17.jdk/Contents/Home",
    ];
    for c in &candidates {
        if std::path::Path::new(c).exists() {
            return Some(c.to_string());
        }
    }
    None
}

pub fn handle_stop(
    mut state: Signal<ServiceState>,
    proc: Signal<Arc<ManagedProcess>>,
    log_lines: Signal<Vec<LogLine>>,
) {
    let mut push = make_push(log_lines);
    match proc.read().stop() {
        Ok(_)  => { state.set(ServiceState::Stopped); push("Java Function App stopped.".into(), LogLevel::Warn); }
        Err(e) => push(format!("Error: {}", e), LogLevel::Error),
    }
}
