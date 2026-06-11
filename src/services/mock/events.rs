//! Event channel for the embedded mock server.
//!
//! Phase 2 publishes scan progress, server lifecycle and live HTTP traffic
//! through a single broadcast channel. The Console panel (UI) subscribes to
//! it — see `screens/console` (to be wired by the UI layer).
//!
//! Broadcast (vs mpsc) so multiple subscribers can attach: the UI, an optional
//! log-file writer, future test harnesses, etc. Slow consumers get dropped
//! messages — that's intentional, UI must not block the server.

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Capacity of the broadcast ring buffer. Tuned for typical UI responsiveness —
/// a sustained burst above this rate will just drop oldest events for slow
/// subscribers; the server itself never blocks.
const CHANNEL_CAPACITY: usize = 1024;

/// What the mock server emits.
///
/// Each event is small and JSON-friendly so it can also be persisted to disk
/// (cache directory) or shipped to a remote diagnostics endpoint later.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum MockEvent {
    /// Server bound and accepting connections.
    ServerStarted { port: u16 },
    /// Graceful shutdown completed.
    ServerStopped,
    /// Local settings rewritten (URLs pointed at the mock).
    SettingsRewritten { rewritten_count: usize, backup_path: String },
    /// Local settings restored from the backup.
    SettingsRestored,
    /// Incoming HTTP request received from a workflow.
    Request {
        id:     String,            // correlation id between Request and Response
        method: String,
        url:    String,             // the full incoming URL the workflow asked for
        target: Option<String>,     // resolved original URL (pre-rewrite), if known
        body:   Option<serde_json::Value>,
    },
    /// Response we returned to the workflow.
    Response {
        id:          String,
        status:      u16,
        source:      ResponseSource,
        elapsed_ms:  u64,
        body_excerpt: Option<String>,
    },
    /// Free-text status line — scan progress, info, etc.
    Log { level: LogLevel, message: String },
    /// Unrecoverable error in the server pipeline.
    Error { message: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ResponseSource {
    /// Returned from the contract's auto-generated example body.
    AutoStub,
    /// Returned from a user-edited fixture file. (Phase 3+)
    Fixture,
    /// Replayed from a recording. (Phase 3+)
    Recorded,
    /// Forwarded to the real service. (Phase 4+)
    Passthrough,
    /// 404 — no matching endpoint in the contract.
    NotInContract,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum LogLevel { Info, Warn, Error }

/// Thin wrapper around `broadcast::Sender` so the rest of the codebase doesn't
/// have to import tokio types directly. Cloning is cheap (Arc inside).
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<MockEvent>,
}

impl Default for EventBus {
    fn default() -> Self { Self::new() }
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self { tx }
    }

    /// Get a subscriber. Each call returns a fresh receiver.
    pub fn subscribe(&self) -> broadcast::Receiver<MockEvent> {
        self.tx.subscribe()
    }

    /// Best-effort publish. Drops the event if there are no subscribers — that
    /// is intentional, the server must never block on diagnostics.
    pub fn publish(&self, event: MockEvent) {
        let _ = self.tx.send(event);
    }

    /// Convenience: emit a single Log event.
    pub fn log(&self, level: LogLevel, msg: impl Into<String>) {
        self.publish(MockEvent::Log { level, message: msg.into() });
    }
}
