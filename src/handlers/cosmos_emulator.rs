use std::sync::Arc;
use dioxus::prelude::*;

use crate::components::log_panel::{LogLevel, LogLine};
use crate::services::{process::{ManagedProcess, ServiceState}, runtime_manager};
use crate::utils::make_push;

pub const COSMOS_IMAGE:     &str = "mcr.microsoft.com/cosmosdb/linux/azure-cosmos-emulator:vnext-preview";
pub const COSMOS_API_PORT:  u16  = 8081;
pub const COSMOS_UI_PORT:   u16  = 1234;
pub const CONTAINER_NAME:   &str = "ais-cosmos-emulator";

/// Emulator account key (well-known fixed key for the vnext-preview image).
pub const EMULATOR_KEY: &str =
    "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==";

/// Connection string to paste into local.settings.json for local function testing.
pub fn local_connection_string() -> String {
    format!(
        "AccountEndpoint=http://localhost:{};AccountKey={};",
        COSMOS_API_PORT, EMULATOR_KEY
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

        // ── Check if already running on port 8081 ────────────────────────
        if tokio::net::TcpStream::connect(("127.0.0.1", COSMOS_API_PORT)).await.is_ok() {
            push(format!("Cosmos emulator already reachable on port {} — marking as running.", COSMOS_API_PORT), LogLevel::Ok);
            state.set(ServiceState::Running);
            return;
        }

        // ── Remove stale stopped container if it exists ──────────────────
        let _ = tokio::task::spawn_blocking(|| {
            let _ = runtime_manager::docker_cmd(&["rm", "-f", CONTAINER_NAME]).output();
        }).await;

        // ── docker run ───────────────────────────────────────────────────
        push(format!(
            "$ docker run -d --name {} -p {}:{} -p {}:{} {}",
            CONTAINER_NAME,
            COSMOS_API_PORT, COSMOS_API_PORT,
            COSMOS_UI_PORT,  COSMOS_UI_PORT,
            COSMOS_IMAGE
        ), LogLevel::Info);
        push(format!("  Connection string for local.settings.json:"), LogLevel::Info);
        push(format!("  CosmosDBConnection = {}", local_connection_string()), LogLevel::Info);

        let api_arg = format!("{}:{}", COSMOS_API_PORT, COSMOS_API_PORT);
        let ui_arg  = format!("{}:{}", COSMOS_UI_PORT,  COSMOS_UI_PORT);
        let run_result = tokio::task::spawn_blocking(move || {
            runtime_manager::docker_cmd(&[
                "run", "-d",
                "--name", CONTAINER_NAME,
                "-p", &api_arg,
                "-p", &ui_arg,
                COSMOS_IMAGE,
            ]).output()
        }).await;

        match run_result {
            Ok(Ok(out)) if out.status.success() => {
                push("Cosmos emulator container started — waiting for API to be ready…".into(), LogLevel::Ok);
            }
            Ok(Ok(out)) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                push(format!("❌ docker run failed: {}", stderr.trim()), LogLevel::Error);
                state.set(ServiceState::Stopped);
                return;
            }
            Ok(Err(e)) => {
                push(format!("❌ Could not run docker: {}", e), LogLevel::Error);
                push("  hint: make sure Docker Desktop is running.".into(), LogLevel::Warn);
                state.set(ServiceState::Stopped);
                return;
            }
            Err(e) => {
                push(format!("❌ spawn error: {}", e), LogLevel::Error);
                state.set(ServiceState::Stopped);
                return;
            }
        }

        // ── Poll port 8081 until the emulator is ready (up to ~60s) ─────
        let mut ready = false;
        for attempt in 1..=20 {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            if tokio::net::TcpStream::connect(("127.0.0.1", COSMOS_API_PORT)).await.is_ok() {
                ready = true;
                break;
            }
            if attempt % 5 == 0 {
                push(format!("Still waiting for Cosmos emulator… ({}/20)", attempt), LogLevel::Info);
            }
        }

        if ready {
            push(format!(
                "✅ Cosmos emulator ready — API: http://localhost:{}  Explorer: http://localhost:{}",
                COSMOS_API_PORT, COSMOS_UI_PORT
            ), LogLevel::Ok);
            state.set(ServiceState::Running);
        } else {
            push("❌ Cosmos emulator did not become ready in time. Check Docker logs.".into(), LogLevel::Error);
            push(format!("  $ docker logs {}", CONTAINER_NAME), LogLevel::Warn);
            state.set(ServiceState::Stopped);
        }
    });
}

pub fn handle_stop(
    mut state: Signal<ServiceState>,
    _proc: Signal<Arc<ManagedProcess>>,
    log_lines: Signal<Vec<LogLine>>,
) {
    let mut push = make_push(log_lines);
    push(format!("Stopping Cosmos emulator ({})…", CONTAINER_NAME), LogLevel::Warn);

    spawn(async move {
        let mut push = make_push(log_lines);
        let result = tokio::task::spawn_blocking(|| {
            runtime_manager::docker_cmd(&["rm", "-f", CONTAINER_NAME]).output()
        }).await;

        match result {
            Ok(Ok(out)) if out.status.success() => {
                push("Cosmos emulator stopped.".into(), LogLevel::Ok);
                state.set(ServiceState::Stopped);
            }
            Ok(Ok(out)) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                push(format!("⚠ docker rm: {}", stderr.trim()), LogLevel::Warn);
                state.set(ServiceState::Stopped);
            }
            Ok(Err(e)) => push(format!("❌ {}", e), LogLevel::Error),
            Err(e)     => push(format!("❌ {}", e), LogLevel::Error),
        }
    });
}
