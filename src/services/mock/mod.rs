//! External-dependency discovery and mocking for Logic Apps workspaces.
//!
//! Phase 1 — discovery
//! -------------------
//! `scan_workspace(path)` walks every `workflow.json`, resolves URLs via
//! `parameters.json` + `local.settings.json`, builds a `MockContract` and
//! caches it under `<workspace>/.ais-cache/mock-contract.json`. No service-
//! specific knowledge; works on any Logic Apps Standard workspace.
//!
//! Phase 2 — mock runtime
//! ----------------------
//! `MockRuntime::start(path, bus)` spins up an embedded HTTP server on a
//! random localhost port, rewrites URL-shaped app settings to point at the
//! server, and streams live request/response events to a broadcast channel
//! consumed by the Console UI. `stop()` undoes the rewrite and shuts the
//! server down.
//!
//! Driven from the toolbar's "Mock APIs" block — see `handlers::mock_server`.
//! Because the rewrite lands in `local.settings.json`, which func reads only at
//! startup, the mock has to be started *before* func for workflows to see it.
//!
//! Future phases (recording, side-effect chains, proxy mode for hardcoded
//! URLs) will plug onto the same contract + event-bus interfaces — no API
//! changes expected.

// The lifecycle (scan → serve → rewrite) is driven from the toolbar via
// `handlers::mock_server`. The contract model still carries fields reserved for
// later phases — fixtures, recording, passthrough, side effects — which nothing
// reads yet, so dead-code warnings stay silenced at the module root.
#![allow(dead_code, unused_imports)]

pub mod contract;
pub mod events;
pub mod resolver;
pub mod rewrite;
pub mod router;
pub mod runtime;
pub mod scanner;
pub mod schema_infer;
pub mod server;
pub mod writer;

pub use contract::{
    AppSetting, AuthKind, DiscoveryRef, Endpoint, MockContract,
    RequestSpec, ResponseSpec, SettingKind, Warning, WarningLevel,
};
pub use events::{EventBus, LogLevel, MockEvent, ResponseSource};
pub use runtime::MockRuntime;
pub use scanner::{ScanError, Scanner};
pub use server::MockServer;

use std::path::{Path, PathBuf};

/// Phase 1 one-shot: scan, cache, return.
pub fn scan_workspace(workspace: &Path) -> Result<(MockContract, PathBuf), ScanError> {
    let contract = Scanner::new(workspace)?.scan()?;
    let path     = writer::write(&contract, workspace)?;
    Ok((contract, path))
}
