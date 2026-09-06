//! Data model for the mock contract — the artifact produced by the scanner.
//!
//! The contract is a serializable snapshot of every external HTTP dependency
//! that the Logic Apps workspace touches. It is **environment-agnostic** — no
//! ERP/SAP/Komgo-specific fields, just generic HTTP descriptors.
//!
//! Cached under `<workspace>/.ais-cache/mock-contract.json` — not committed.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Top-level mock contract. One per Logic Apps workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MockContract {
    /// Semver of the contract format itself, not the workspace.
    pub version: String,
    /// ISO-8601 UTC.
    pub generated_at: String,
    /// Absolute path that was scanned.
    pub workspace: String,
    /// Every entry from `local.settings.json`, classified and cross-referenced.
    pub app_settings: BTreeMap<String, AppSetting>,
    /// Every outbound HTTP call found across all workflow.json files.
    pub endpoints: Vec<Endpoint>,
    /// Warnings (hardcoded URLs, unresolved parameters, etc).
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSetting {
    /// Raw value as found in local.settings.json.
    pub raw_value: String,
    /// After parameter chain resolution (currently same as raw — settings ARE leaves).
    pub resolved_value: Option<String>,
    /// Workflow folders that reference this setting.
    pub references: Vec<String>,
    pub kind: SettingKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SettingKind {
    Url,
    ConnectionString,
    Other,
    Secret,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    /// Stable hash of method + url_template — survives across scans.
    pub id: String,
    pub method: String,
    /// As written in the workflow JSON (may contain `@{parameters('X')}` placeholders).
    pub url_template: String,
    /// After substituting parameter → app setting values. `None` if unresolvable.
    pub url_resolved: Option<String>,
    pub auth: AuthKind,
    /// Where this endpoint was discovered (one entry per call site).
    pub discovered_in: Vec<DiscoveryRef>,
    pub request: RequestSpec,
    pub response: ResponseSpec,
    /// Reserved for Phase 5 (declarative async callbacks). Empty in Phase 1.
    pub side_effects: Vec<SideEffect>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryRef {
    /// Folder name (workflow id).
    pub workflow: String,
    /// Action name inside that workflow.
    pub action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AuthKind {
    None,
    ActiveDirectoryOAuth { audience: String, tenant: String },
    BearerToken,
    ApiKeyHeader { header_name: String },
    Basic,
    Unknown(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestSpec {
    /// Inferred from the inline body if present.
    pub body_schema: Option<serde_json::Value>,
    /// Literal headers declared in the workflow (with their template values).
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseSpec {
    /// JSON schema from the adjacent `Parse_JSON` action, if any.
    pub schema: Option<serde_json::Value>,
    /// Auto-generated example body matching the schema.
    pub example: serde_json::Value,
    /// 200 default; 202 if the URL hints at `/executeasync` or similar async pattern.
    pub status_code: u16,
    /// Reserved — populated by branch detector when `Check_success`-style patterns are seen.
    pub branches: Vec<ResponseBranch>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseBranch {
    pub name: String,
    pub condition: String,
    pub example: serde_json::Value,
}

/// Declarative async effect attached to an endpoint — Phase 5.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SideEffect {
    pub kind: SideEffectKind,
    pub delay_seconds: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SideEffectKind {
    SendServiceBus {
        queue: String,
        body: serde_json::Value,
    },
    HttpCallback {
        url: String,
        method: String,
        body: serde_json::Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Warning {
    pub level: WarningLevel,
    pub workflow: Option<String>,
    pub action: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WarningLevel {
    Info,
    Warn,
    Error,
}
