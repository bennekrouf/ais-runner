use std::sync::Arc;
use dioxus::prelude::*;

use crate::components::log_panel::{LogLevel, LogLine};
use crate::services::{process::{ManagedProcess, ServiceState}, runtime_manager};
use crate::utils::make_push;

pub const SQL_IMAGE:      &str = "mcr.microsoft.com/azure-sql-edge:latest";
pub const CONTAINER_NAME: &str = "ais-sql-dev";
pub const SA_PASSWORD:    &str = "AisRunner_Emulator1!";
pub const SQL_PORT:       u16  = 1433;

/// Generic connection string template logged on start so developers can
/// copy it into their project's local.settings.json.
pub fn local_connection_string() -> String {
    format!(
        "Server=localhost,{SQL_PORT};Database=<your-db>;User Id=sa;\
         Password={SA_PASSWORD};Encrypt=false;TrustServerCertificate=true;"
    )
}

pub fn handle_start(
    mut state: Signal<ServiceState>,
    _proc: Signal<Arc<ManagedProcess>>,
    log_lines: Signal<Vec<LogLine>>,
) {
    state.set(ServiceState::Starting);

    spawn(async move {
        let mut push = make_push(log_lines);

        // ── Already running? ─────────────────────────────────────────────
        if tokio::net::TcpStream::connect(("127.0.0.1", SQL_PORT)).await.is_ok() {
            push(format!("SQL Edge already reachable on port {SQL_PORT}."), LogLevel::Ok);
            push(format!("  Connection string: {}", local_connection_string()), LogLevel::Info);
            state.set(ServiceState::Running);
            return;
        }

        // ── Remove stale stopped container ───────────────────────────────
        let _ = tokio::task::spawn_blocking(|| {
            let _ = runtime_manager::docker_cmd(&["rm", "-f", CONTAINER_NAME]).output();
        }).await;

        // ── docker run ───────────────────────────────────────────────────
        push(format!(
            "$ docker run -d --name {CONTAINER_NAME} -p {SQL_PORT}:{SQL_PORT} \
             -e ACCEPT_EULA=Y -e MSSQL_SA_PASSWORD=*** {SQL_IMAGE}"
        ), LogLevel::Info);

        let port_arg = format!("{SQL_PORT}:{SQL_PORT}");
        let pw_arg   = format!("MSSQL_SA_PASSWORD={SA_PASSWORD}");
        let run_result = tokio::task::spawn_blocking(move || {
            runtime_manager::docker_cmd(&[
                "run", "-d",
                "--name", CONTAINER_NAME,
                "-p", &port_arg,
                "-e", "ACCEPT_EULA=Y",
                "-e", &pw_arg,
                SQL_IMAGE,
            ]).output()
        }).await;

        match run_result {
            Ok(Ok(out)) if out.status.success() => {
                push("SQL Edge container started — waiting for SQL to be ready…".into(), LogLevel::Ok);
            }
            Ok(Ok(out)) => {
                push(format!("❌ docker run failed: {}", String::from_utf8_lossy(&out.stderr).trim()), LogLevel::Error);
                state.set(ServiceState::Stopped);
                return;
            }
            Ok(Err(e)) => {
                push(format!("❌ Could not run docker: {e}"), LogLevel::Error);
                push("  hint: make sure Docker Desktop is running.".into(), LogLevel::Warn);
                state.set(ServiceState::Stopped);
                return;
            }
            Err(e) => {
                push(format!("❌ spawn error: {e}"), LogLevel::Error);
                state.set(ServiceState::Stopped);
                return;
            }
        }

        // ── Poll port 1433 until SQL is ready (up to ~90s) ──────────────
        let mut ready = false;
        for attempt in 1..=30 {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            if tokio::net::TcpStream::connect(("127.0.0.1", SQL_PORT)).await.is_ok() {
                // SQL Edge accepts TCP before it's fully ready — give it a
                // couple extra seconds before handing off to the user.
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                ready = true;
                break;
            }
            if attempt % 5 == 0 {
                push(format!("Still waiting for SQL Edge… ({attempt}/30)"), LogLevel::Info);
            }
        }

        if !ready {
            push("❌ SQL Edge did not become ready in time.".into(), LogLevel::Error);
            push(format!("  $ docker logs {CONTAINER_NAME}"), LogLevel::Warn);
            state.set(ServiceState::Stopped);
            return;
        }

        push("✅ SQL Edge ready on port 1433 (sa / AisRunner_Emulator1!).".into(), LogLevel::Ok);
        push(format!("  Connection string: {}", local_connection_string()), LogLevel::Info);
        state.set(ServiceState::Running);
    });
}

pub fn handle_stop(
    mut state: Signal<ServiceState>,
    _proc: Signal<Arc<ManagedProcess>>,
    log_lines: Signal<Vec<LogLine>>,
) {
    let mut push = make_push(log_lines);
    push(format!("Stopping SQL Edge ({CONTAINER_NAME})…"), LogLevel::Warn);

    spawn(async move {
        let mut push = make_push(log_lines);
        let result = tokio::task::spawn_blocking(|| {
            runtime_manager::docker_cmd(&["rm", "-f", CONTAINER_NAME]).output()
        }).await;

        match result {
            Ok(Ok(out)) if out.status.success() => {
                push("SQL Edge stopped.".into(), LogLevel::Ok);
                state.set(ServiceState::Stopped);
            }
            Ok(Ok(out)) => {
                push(format!("⚠ {}", String::from_utf8_lossy(&out.stderr).trim()), LogLevel::Warn);
                state.set(ServiceState::Stopped);
            }
            Ok(Err(e)) => push(format!("❌ {e}"), LogLevel::Error),
            Err(e)     => push(format!("❌ {e}"), LogLevel::Error),
        }
    });
}
