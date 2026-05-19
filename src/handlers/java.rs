use std::sync::Arc;
use dioxus::prelude::*;

use crate::components::log_panel::{LogLevel, LogLine};
use crate::services::process::{ManagedProcess, ServiceState, rich_path};
use crate::utils::make_push;

pub fn handle_start(
    mut state: Signal<ServiceState>,
    proc: Signal<Arc<ManagedProcess>>,
    log_lines: Signal<Vec<LogLine>>,
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

        // ── Step 2: mvn azure-functions:run ──────────────────────────────
        push(format!("$ cd {} && mvn azure-functions:run", dir), LogLevel::Info);

        match proc.read().start("mvn", &["azure-functions:run"], Some(&dir)) {
            Ok((stdout, stderr)) => {
                state.set(ServiceState::Running);
                push("Java Function App starting on port 7072…".into(), LogLevel::Ok);

                let mut push2 = make_push(log_lines);
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, bool)>();
                crate::services::process::stream_output(stdout, stderr, tx, true);

                spawn(async move {
                    while let Some((line, is_err)) = rx.recv().await {
                        push2(line, if is_err { LogLevel::Error } else { LogLevel::Info });
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
