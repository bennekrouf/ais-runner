use std::sync::Arc;
use dioxus::prelude::*;

use crate::components::log_panel::{LogLevel, LogLine};
use crate::services::{process::{ManagedProcess, ServiceState}, runtime_manager};
use crate::utils::make_push;

pub fn handle_start(
    mut state: Signal<ServiceState>,
    proc: Signal<Arc<ManagedProcess>>,
    log_lines: Signal<Vec<LogLine>>,
) {
    state.set(ServiceState::Starting);
    let az_bin = runtime_manager::resolve_tool("azurite");
    let mut push = make_push(log_lines);
    push(
        format!("$ {} --location /tmp/azurite --debug /tmp/azurite/debug.log --skipApiVersionCheck", az_bin),
        LogLevel::Info,
    );
    match proc.read().start(
        &az_bin,
        &["--location", "/tmp/azurite", "--debug", "/tmp/azurite/debug.log", "--skipApiVersionCheck"],
        None,
    ) {
        Ok((az_stdout, az_stderr)) => {
            // Drain azurite pipes — dropping the read-ends causes SIGPIPE mid-startup,
            // killing the process between blob.start() and queue.start().
            let (az_tx, mut az_rx) = tokio::sync::mpsc::unbounded_channel::<(String, bool)>();
            crate::services::process::stream_output(az_stdout, az_stderr, az_tx, false);
            let mut push2 = make_push(log_lines);
            spawn(async move {
                while let Some((line, _)) = az_rx.recv().await {
                    if !line.trim().is_empty() {
                        push2(line, LogLevel::Info);
                    }
                }
            });

            // Mark Running only once all three ports are bound.
            let mut state2 = state;
            let mut push3  = make_push(log_lines);
            spawn(async move {
                let mut up = false;
                for _ in 0..30 {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let all = async {
                        for port in [10000u16, 10001, 10002] {
                            if tokio::net::TcpStream::connect(
                                std::net::SocketAddr::from(([127, 0, 0, 1], port))
                            ).await.is_err() { return false; }
                        }
                        true
                    }.await;
                    if all { up = true; break; }
                }
                if up {
                    state2.set(ServiceState::Running);
                    push3("Azurite ready — blob :10000  queue :10001  table :10002".into(), LogLevel::Ok);
                } else {
                    state2.set(ServiceState::Stopped);
                    push3(
                        "⚠ Azurite process started but ports didn't bind in 15 s. Check the Azurite tab for errors.".into(),
                        LogLevel::Error,
                    );
                }
            });
        }
        Err(e) => {
            state.set(ServiceState::Stopped);
            make_push(log_lines)(format!("Azurite error: {}", e), LogLevel::Error);
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
        Ok(_)  => { state.set(ServiceState::Stopped); push("Azurite stopped.".into(), LogLevel::Warn); }
        Err(e) => push(format!("Error: {}", e), LogLevel::Error),
    }
}
