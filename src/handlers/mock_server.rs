//! Toolbar handler for the embedded mock HTTP server.
//!
//! Unlike the other service handlers, this one manages no child process — the
//! server is an in-process axum task owned by `MockRuntime`. What it does share
//! with them is the `ServiceState` lifecycle, so the toolbar block behaves the
//! same way.
//!
//! Two things make the shape here different from `azurite`/`cosmos_emulator`:
//!
//! 1. `MockRuntime::stop()` consumes `self`, so the runtime lives behind an
//!    `Arc<Mutex<Option<_>>>` that `handle_stop` can `take()` out of.
//! 2. Starting rewrites the workspace's `local.settings.json`. func reads that
//!    file once at startup, so the ordering (mock first, then func) matters and
//!    is surfaced to the user rather than enforced silently.

use std::sync::Arc;

use dioxus::prelude::*;
use tokio::sync::Mutex;

use crate::components::log_panel::{LogLevel, LogLine};
use crate::services::mock::{EventBus, MockEvent, MockRuntime, ResponseSource};
use crate::services::mock::events::LogLevel as MockLevel;
use crate::services::process::ServiceState;
use crate::services::workflows;
use crate::utils::make_push;

/// Shared handle to the live runtime. `None` when stopped.
pub type MockHandle = Arc<Mutex<Option<MockRuntime>>>;

pub fn new_handle() -> MockHandle {
    Arc::new(Mutex::new(None))
}

pub fn handle_start(
    dir: String,
    mut state: Signal<ServiceState>,
    handle: Signal<MockHandle>,
    log_lines: Signal<Vec<LogLine>>,
) {
    state.set(ServiceState::Starting);

    spawn(async move {
        let mut push = make_push(log_lines);
        let workspace = workflows::resolve_logic_apps_dir(&dir);

        // Subscribe before starting: `MockRuntime::start` publishes its scan
        // results and the ServerStarted event during the call, and those are
        // the most useful lines in the whole session.
        let bus = EventBus::new();
        pump_events(bus.subscribe(), log_lines);

        push("🎭 Starting mock server — scanning workspace for outbound HTTP calls…".into(), LogLevel::Info);

        match MockRuntime::start(&workspace, bus).await {
            Ok(runtime) => {
                let port = runtime.port().unwrap_or(0);
                *handle.read().lock().await = Some(runtime);
                state.set(ServiceState::Running);
                push(format!("🎭 Mock server running on :{port}."), LogLevel::Ok);
                push(
                    "   local.settings.json now points URL settings at the mock. \
                     Restart func so it picks them up — settings are read once at startup."
                        .into(),
                    LogLevel::Warn,
                );
            }
            Err(e) => {
                state.set(ServiceState::Stopped);
                push(format!("❌ Mock server failed to start: {e}"), LogLevel::Error);
                push(
                    "   local.settings.json was not modified. Check the folder is a \
                     Logic Apps workspace (local.settings.json + */workflow.json)."
                        .into(),
                    LogLevel::Info,
                );
            }
        }
    });
}

pub fn handle_stop(
    mut state: Signal<ServiceState>,
    handle: Signal<MockHandle>,
    log_lines: Signal<Vec<LogLine>>,
) {
    spawn(async move {
        let mut push = make_push(log_lines);

        let runtime = handle.read().lock().await.take();
        match runtime {
            Some(runtime) => {
                runtime.stop().await;
                push("🎭 Mock server stopped — local.settings.json restored.".into(), LogLevel::Ok);
                push("   Restart func to go back to the real endpoints.".into(), LogLevel::Warn);
            }
            // Reachable if a start failed after the button already flipped.
            None => push("🎭 Mock server was not running.".into(), LogLevel::Info),
        }
        state.set(ServiceState::Stopped);
    });
}

/// Forward `MockEvent`s onto the Console log until the server stops.
///
/// The loop ends on `ServerStopped` (or when the bus closes) so a start/stop
/// cycle doesn't leak a task per session. `Lagged` is survivable — a burst of
/// traffic drops diagnostics, never requests — so it keeps reading.
fn pump_events(mut rx: tokio::sync::broadcast::Receiver<MockEvent>, log_lines: Signal<Vec<LogLine>>) {
    spawn(async move {
        let mut push = make_push(log_lines);
        loop {
            match rx.recv().await {
                Ok(MockEvent::ServerStopped) => break,
                Ok(event) => {
                    if let Some((msg, level)) = render(event) {
                        push(msg, level);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    push(format!("🎭 … {n} mock event(s) dropped (UI behind)"), LogLevel::Warn);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// One log line per event. `None` for events whose information is already
/// carried by the handler's own messages.
fn render(event: MockEvent) -> Option<(String, LogLevel)> {
    match event {
        // Emitted by the handler with the port already known.
        MockEvent::ServerStarted { .. } | MockEvent::ServerStopped => None,

        MockEvent::SettingsRewritten { rewritten_count, .. } => Some((
            format!("🎭 Rewrote {rewritten_count} URL setting(s) → mock"),
            LogLevel::Info,
        )),
        MockEvent::SettingsRestored => None,

        MockEvent::Request { method, url, .. } => {
            Some((format!("🎭 → {method} {url}"), LogLevel::Info))
        }

        MockEvent::Response { status, source, elapsed_ms, .. } => {
            // A 404 here is the actionable case: the workflow called something
            // the contract never saw, so the mock had nothing to answer with.
            let level = match source {
                ResponseSource::NotInContract => LogLevel::Warn,
                _ if status >= 400 => LogLevel::Warn,
                _ => LogLevel::Ok,
            };
            let hint = match source {
                ResponseSource::NotInContract => "  ← not in contract; add a fixture or re-scan",
                _ => "",
            };
            Some((
                format!("🎭 ← {status} ({}) {elapsed_ms}ms{hint}", describe(source)),
                level,
            ))
        }

        MockEvent::Log { level, message } => Some((
            format!("🎭 {message}"),
            match level {
                MockLevel::Info  => LogLevel::Info,
                MockLevel::Warn  => LogLevel::Warn,
                MockLevel::Error => LogLevel::Error,
            },
        )),

        MockEvent::Error { message } => Some((format!("🎭 ❌ {message}"), LogLevel::Error)),
    }
}

fn describe(source: ResponseSource) -> &'static str {
    match source {
        ResponseSource::AutoStub      => "auto-stub",
        ResponseSource::Fixture       => "fixture",
        ResponseSource::Recorded      => "recorded",
        ResponseSource::Passthrough   => "passthrough",
        ResponseSource::NotInContract => "no match",
    }
}
