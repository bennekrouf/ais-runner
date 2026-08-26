//! Top-level mock runtime — start everything, stop everything, with one call.
//!
//! Used by the Console UI:
//!
//! ```ignore
//! let bus     = EventBus::new();
//! let runtime = MockRuntime::start(&workspace, bus.clone()).await?;
//! // ... user does work, run func, see traffic in Console ...
//! runtime.stop().await;
//! ```
//!
//! The runtime owns:
//!   - the scanned contract (used as the response source)
//!   - the rewritten `local.settings.json` (restored on `stop()`)
//!   - the embedded HTTP server (gracefully shut down on `stop()`)
//!
//! Failures during `stop()` are best-effort — we always try to restore the
//! settings file even if the server stops cleanly, and vice-versa, so the
//! workspace never ends up in a half-patched state.

use std::path::{Path, PathBuf};

use crate::services::mock::events::{EventBus, LogLevel, MockEvent};
use crate::services::mock::scanner::ScanError;
use crate::services::mock::server::MockServer;
use crate::services::mock::{rewrite, scan_workspace};

pub struct MockRuntime {
    workspace: PathBuf,
    server: Option<MockServer>,
    bus: EventBus,
}

impl MockRuntime {
    /// One-shot bootstrap:
    ///   1. Scan workspace (re-uses Phase 1 scanner).
    ///   2. Start the HTTP server on an ephemeral port.
    ///   3. Rewrite `local.settings.json` to point URL settings at the server.
    pub async fn start(workspace: &Path, bus: EventBus) -> Result<Self, ScanError> {
        bus.log(
            LogLevel::Info,
            format!("scanning workspace: {}", workspace.display()),
        );

        let (contract, cache_path) = scan_workspace(workspace)?;
        bus.log(
            LogLevel::Info,
            format!(
                "scan complete — {} endpoints, {} warnings (cached → {})",
                contract.endpoints.len(),
                contract.warnings.len(),
                cache_path.display(),
            ),
        );
        for w in &contract.warnings {
            let level = match w.level {
                crate::services::mock::contract::WarningLevel::Error => LogLevel::Error,
                crate::services::mock::contract::WarningLevel::Warn => LogLevel::Warn,
                crate::services::mock::contract::WarningLevel::Info => LogLevel::Info,
            };
            bus.log(
                level,
                format!(
                    "{}{}: {}",
                    w.workflow.as_deref().unwrap_or("?"),
                    w.action
                        .as_ref()
                        .map(|a| format!("/{}", a))
                        .unwrap_or_default(),
                    w.message,
                ),
            );
        }

        // Server first — we need its port for the settings rewrite.
        let server = MockServer::start(contract.clone(), bus.clone())
            .await
            .map_err(ScanError::Io)?;
        bus.log(
            LogLevel::Info,
            format!("mock server listening on :{}", server.port),
        );

        let outcome = rewrite::rewrite(workspace, &contract, server.port)?;
        bus.publish(MockEvent::SettingsRewritten {
            rewritten_count: outcome.rewritten_count,
            backup_path: outcome.backup_path.display().to_string(),
        });
        bus.log(
            LogLevel::Info,
            format!(
                "rewrote {} URL settings → backup: {}",
                outcome.rewritten_count,
                outcome.backup_path.display(),
            ),
        );

        Ok(Self {
            workspace: workspace.to_path_buf(),
            server: Some(server),
            bus,
        })
    }

    /// Graceful teardown. Always restores `local.settings.json` even if the
    /// server fails to stop, so the workspace is left clean.
    pub async fn stop(mut self) {
        // 1. Server first so no more inbound requests are processed.
        if let Some(server) = self.server.take() {
            server.stop().await;
            self.bus.publish(MockEvent::ServerStopped);
        }

        // 2. Restore settings — best-effort.
        match rewrite::restore(&self.workspace) {
            Ok(true) => self.bus.publish(MockEvent::SettingsRestored),
            Ok(false) => self
                .bus
                .log(LogLevel::Warn, "no settings backup to restore"),
            Err(e) => self.bus.log(
                LogLevel::Error,
                format!("failed to restore settings: {}", e),
            ),
        }
    }

    /// For tests / advanced UI: get the server port if the runtime is alive.
    pub fn port(&self) -> Option<u16> {
        self.server.as_ref().map(|s| s.port)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::mock::events::ResponseSource;

    /// End-to-end smoke test: scan the real workspace if present, start the
    /// server, fire a probe HTTP request through the mock prefix, assert the
    /// canned response comes back, then shut down and verify settings are
    /// restored.
    ///
    /// **Ignored by default, and must stay that way.** `MockRuntime::start`
    /// rewrites the workspace's `local.settings.json`, and `stop()` restores
    /// it from `.ais-cache/local.settings.json.original` — a backup that may
    /// be months old. Running this against a real, in-use workspace silently
    /// reverts the developer's current settings to whatever that snapshot
    /// held, which is indistinguishable from "my settings keep disappearing".
    /// Run it deliberately with `cargo test -- --ignored` on a workspace you
    /// don't mind rewriting.
    #[ignore = "rewrites and restores a real workspace's local.settings.json — see doc comment"]
    #[tokio::test(flavor = "current_thread")]
    async fn e2e_runtime_against_real_workspace() {
        let path = std::path::Path::new("/Users/mb/code/oryx/ais_tom_platform/logic_apps");
        if !path.exists() {
            eprintln!("skipping e2e — workspace not present");
            return;
        }

        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let runtime = MockRuntime::start(path, bus.clone())
            .await
            .expect("runtime should start");
        let port = runtime.port().expect("port");

        // Probe: hit any URL-kind setting that resolves to a contract endpoint.
        // We use the eventGrid_Uri endpoint because every workspace has it and
        // the contract had a `POST eventGrid_Uri` entry in the smoke output.
        let url = format!("http://127.0.0.1:{}/__mock__/eventGrid_Uri", port);
        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .body("{}")
            .header("Content-Type", "application/json")
            .send()
            .await
            .expect("post should succeed");

        // We don't assert 200 — the contract may say 200 or it may be unknown
        // (no Parse_JSON downstream of POST_to_EventGrid). What matters is the
        // server responded at all, and the bus saw events for the round-trip.
        let status = resp.status();
        eprintln!("e2e: probe responded with {}", status);

        // Drain everything currently buffered + a short window for stragglers.
        // (Startup emits ~30 log events for warnings, so we can't bound by count.)
        let mut saw_req = false;
        let mut saw_resp = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline && !(saw_req && saw_resp) {
            match tokio::time::timeout(std::time::Duration::from_millis(20), rx.recv()).await {
                Ok(Ok(MockEvent::Request { .. })) => saw_req = true,
                Ok(Ok(MockEvent::Response { source, .. })) => {
                    saw_resp = true;
                    eprintln!("e2e: response source = {:?}", source);
                    assert!(matches!(
                        source,
                        ResponseSource::AutoStub | ResponseSource::NotInContract
                    ));
                }
                _ => {}
            }
        }
        assert!(saw_req, "no Request event observed on the bus");
        assert!(saw_resp, "no Response event observed on the bus");

        // Shut down — must not panic.
        runtime.stop().await;

        // Verify the settings backup file exists (means we did rewrite).
        let backup = path.join(".ais-cache").join("local.settings.json.original");
        assert!(
            backup.is_file(),
            "backup should exist at {}",
            backup.display()
        );
    }
}
