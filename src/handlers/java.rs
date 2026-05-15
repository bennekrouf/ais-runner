use std::sync::Arc;
use dioxus::prelude::*;

use crate::components::log_panel::{LogLevel, LogLine};
use crate::services::process::{ManagedProcess, ServiceState};
use crate::utils::make_push;

pub fn handle_start(
    mut state: Signal<ServiceState>,
    proc: Signal<Arc<ManagedProcess>>,
    log_lines: Signal<Vec<LogLine>>,
    func_apps_dir: &str,
) {
    let mut push = make_push(log_lines);
    state.set(ServiceState::Starting);
    push(format!("$ cd {} && mvn azure-functions:run", func_apps_dir), LogLevel::Info);
    match proc.read().start("mvn", &["azure-functions:run"], Some(func_apps_dir)) {
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
            if e.contains("does not exist") {
                push(format!(
                    "  hint: create a Maven Azure Functions project at '{}'  \
                     (mvn archetype:generate -DarchetypeGroupId=com.microsoft.azure \
                     -DarchetypeArtifactId=azure-functions-archetype)",
                    func_apps_dir,
                ), LogLevel::Warn);
            } else if e.contains("Failed to spawn") {
                push("  hint: install Maven — brew install maven".into(), LogLevel::Warn);
            }
        }
    }
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
