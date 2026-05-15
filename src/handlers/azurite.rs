use std::sync::Arc;
use dioxus::prelude::*;

use crate::components::log_panel::{LogLevel, LogLine};
use crate::services::{process::{ManagedProcess, ServiceState}, runtime_manager};
use crate::utils::{azurite_dir, azurite_log};
use crate::utils::make_push;

pub fn handle_start(
    mut state: Signal<ServiceState>,
    proc: Signal<Arc<ManagedProcess>>,
    log_lines: Signal<Vec<LogLine>>,
) {
    state.set(ServiceState::Starting);
    let az_bin  = runtime_manager::resolve_tool("azurite");
    let az_dir  = azurite_dir();
    let az_log  = azurite_log();
    let az_dir_s = az_dir.to_string_lossy().to_string();
    let az_log_s = az_log.to_string_lossy().to_string();
    let _ = std::fs::create_dir_all(&az_dir);
    let mut push = make_push(log_lines);
    push(
        format!("$ {} --location {} --debug {} --skipApiVersionCheck", az_bin, az_dir_s, az_log_s),
        LogLevel::Info,
    );
    match proc.read().start(
        &az_bin,
        &["--location", &az_dir_s, "--debug", &az_log_s, "--skipApiVersionCheck"],
        None,
    ) {
        Ok((az_stdout, az_stderr)) => {
            // Drain azurite pipes — dropping the read-ends causes SIGPIPE mid-startup,
            // killing the process between blob.start() and queue.start().
            let (az_tx, mut az_rx) = tokio::sync::mpsc::unbounded_channel::<(String, bool)>();
            crate::services::process::stream_output(az_stdout, az_stderr, az_tx, true);
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

/// Stop Azurite + func, wipe /tmp/azurite, restart Azurite.
/// Func must be restarted manually afterwards so it recreates the run-history tables
/// against the clean storage instance.
///
/// Root causes this fixes:
/// - child.kill() returns before the OS releases port 10000 → EADDRINUSE on restart
/// - func still holds stale table connections after an Azurite data wipe
pub fn handle_reset(
    az_state:   Signal<ServiceState>,
    az_proc:    Signal<Arc<ManagedProcess>>,
    func_state: Signal<ServiceState>,
    func_proc:  Signal<Arc<ManagedProcess>>,
    log_lines:  Signal<Vec<LogLine>>,
) {
    let mut push = make_push(log_lines);
    push("⟳ Resetting Azurite — stopping func and Azurite…".into(), LogLevel::Warn);

    // stop() now calls wait() internally, so both processes are fully dead (and their
    // ports released by the kernel) before these lines return.
    let _ = func_proc.read().stop();
    let _ = az_proc.read().stop();

    let mut func_state2 = func_state;
    let az_state2   = az_state;
    func_state2.set(ServiceState::Stopped);

    spawn(async move {
        let mut push2 = make_push(log_lines);

        // Port 10000 should be free immediately since stop() now reaps the child.
        // A short check guards against an external Azurite process on the same port.
        let mut freed = tokio::net::TcpStream::connect("127.0.0.1:10000").await.is_err();
        if !freed {
            for _ in 0..3 {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                if tokio::net::TcpStream::connect("127.0.0.1:10000").await.is_err() {
                    freed = true;
                    break;
                }
            }
        }
        if !freed {
            push2(
                "⚠ Port 10000 still in use — another Azurite instance may be running. \
                 Kill it manually (lsof -i :10000), then click ⟳ Reset again.".into(),
                LogLevel::Error,
            );
            return;
        }

        // Clear /tmp/azurite contents
        let cleared = tokio::task::spawn_blocking(|| -> std::io::Result<usize> {
            let dir = azurite_dir();
            std::fs::create_dir_all(&dir)?;
            let mut n = 0usize;
            for entry in std::fs::read_dir(&dir)?.flatten() {
                let p = entry.path();
                if p.is_dir() { std::fs::remove_dir_all(&p)?; }
                else          { std::fs::remove_file(&p)?; }
                n += 1;
            }
            Ok(n)
        }).await.unwrap_or(Ok(0));

        match cleared {
            Err(e) => { push2(format!("Could not clear {}: {}", azurite_dir().display(), e), LogLevel::Error); return; }
            Ok(n)  => push2(format!("Azurite data wiped ({} item(s)) — starting fresh…", n), LogLevel::Ok),
        }

        handle_start(az_state2, az_proc, log_lines);

        push2(
            "✅ Azurite restarted clean. Now click ▶ Start on func start to recreate run-history tables.".into(),
            LogLevel::Ok,
        );
    });
}
