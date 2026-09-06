//! Walks a Logic Apps workspace and extracts every outbound HTTP dependency.
//!
//! Recursive over nested actions (Scope, If, Foreach, Until, Switch) so all
//! HTTP calls are found regardless of nesting depth.
//!
//! Output: a `MockContract` capturing endpoints, app settings, and warnings.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::services::mock::contract::*;
use crate::services::mock::resolver::Resolver;
use crate::services::mock::schema_infer;

// ────────────────────────────────────────────────────────────────────────────
// Error type
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum ScanError {
    Io(std::io::Error),
    Json(serde_json::Error),
    NoSettings,
    NoWorkflows,
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {}", e),
            Self::Json(e) => write!(f, "JSON parse error: {}", e),
            Self::NoSettings => write!(
                f,
                "local.settings.json missing — is this a Logic Apps workspace?"
            ),
            Self::NoWorkflows => write!(f, "no workflow.json files found under the workspace"),
        }
    }
}

impl std::error::Error for ScanError {}
impl From<std::io::Error> for ScanError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<serde_json::Error> for ScanError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Scanner
// ────────────────────────────────────────────────────────────────────────────

pub struct Scanner<'a> {
    workspace: &'a Path,
    resolver: Resolver,
    warnings: Vec<Warning>,
}

impl<'a> Scanner<'a> {
    pub fn new(workspace: &'a Path) -> Result<Self, ScanError> {
        let resolver = Resolver::load(workspace)?;
        Ok(Self {
            workspace,
            resolver,
            warnings: vec![],
        })
    }

    /// Walk every workflow.json under the workspace, build the contract.
    pub fn scan(mut self) -> Result<MockContract, ScanError> {
        let workflows = walk_workflows(self.workspace)?;
        if workflows.is_empty() {
            return Err(ScanError::NoWorkflows);
        }

        let mut endpoints: Vec<Endpoint> = vec![];

        for (folder_name, path) in workflows {
            let json = std::fs::read_to_string(&path)?;
            // Skip non-parseable workflow files with a warning, don't abort the scan.
            let v: serde_json::Value = match serde_json::from_str(&json) {
                Ok(v) => v,
                Err(e) => {
                    self.warnings.push(Warning {
                        level: WarningLevel::Error,
                        workflow: Some(folder_name.clone()),
                        action: None,
                        message: format!("Failed to parse workflow.json: {}", e),
                    });
                    continue;
                }
            };

            let actions = &v["definition"]["actions"];
            self.walk_actions(&folder_name, actions, &mut endpoints);

            // Side pass: scan headers and inputs for @{appsetting('X')} so we
            // can populate `references` even for settings not used in URLs.
            self.collect_setting_references(&folder_name, &json);
        }

        let endpoints = dedup_endpoints(endpoints);

        Ok(MockContract {
            version: "1.0.0".into(),
            generated_at: chrono::Utc::now().to_rfc3339(),
            workspace: self.workspace.display().to_string(),
            app_settings: self.resolver.into_settings(),
            endpoints,
            warnings: self.warnings,
        })
    }

    /// Recursive walk: handles Scope / If / Foreach / Until / Switch.
    fn walk_actions(
        &mut self,
        workflow: &str,
        actions: &serde_json::Value,
        endpoints: &mut Vec<Endpoint>,
    ) {
        let Some(obj) = actions.as_object() else {
            return;
        };
        for (action_name, action) in obj {
            let ty = action.get("type").and_then(|v| v.as_str()).unwrap_or("");

            if ty.eq_ignore_ascii_case("Http") {
                if let Some(ep) = self.extract_http(workflow, action_name, action, obj) {
                    endpoints.push(ep);
                }
            }

            // Recurse into containers
            if let Some(inner) = action.get("actions") {
                self.walk_actions(workflow, inner, endpoints);
            }
            // If/else branches
            if let Some(else_branch) = action.get("else").and_then(|v| v.get("actions")) {
                self.walk_actions(workflow, else_branch, endpoints);
            }
            // Switch cases
            if let Some(cases) = action.get("cases").and_then(|v| v.as_object()) {
                for (_, case) in cases {
                    if let Some(case_actions) = case.get("actions") {
                        self.walk_actions(workflow, case_actions, endpoints);
                    }
                }
            }
            // Switch default
            if let Some(default_actions) = action.get("default").and_then(|v| v.get("actions")) {
                self.walk_actions(workflow, default_actions, endpoints);
            }
        }
    }

    fn extract_http(
        &mut self,
        workflow: &str,
        action_name: &str,
        action: &serde_json::Value,
        siblings: &serde_json::Map<String, serde_json::Value>,
    ) -> Option<Endpoint> {
        let inputs = action.get("inputs")?;
        let url_template = inputs.get("uri").and_then(|v| v.as_str())?.to_string();
        let method = inputs
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("GET")
            .to_uppercase();

        let url_resolved = self.resolver.resolve(&url_template);
        let auth = parse_auth(inputs.get("authentication"));
        let body_schema = infer_body_schema(inputs.get("body"));
        let headers = parse_headers(inputs.get("headers"));

        // Adjacent Parse_JSON action that consumes `body('<action_name>')`
        let response_schema = find_response_schema(action_name, siblings);
        let example = response_schema
            .as_ref()
            .map(schema_infer::example_from)
            .unwrap_or(serde_json::json!({}));

        let status_code = guess_status_code(&url_template, &method);

        // Warn on hardcoded URLs — these bypass parameter rewriting.
        if is_hardcoded(&url_template) {
            self.warnings.push(Warning {
                level: WarningLevel::Warn,
                workflow: Some(workflow.into()),
                action: Some(action_name.into()),
                message: format!("Hardcoded URL: {}", url_template),
            });
        }

        // Warn if a parameterized template couldn't be resolved.
        if !is_hardcoded(&url_template) && url_resolved.is_none() {
            self.warnings.push(Warning {
                level: WarningLevel::Info,
                workflow: Some(workflow.into()),
                action: Some(action_name.into()),
                message: format!(
                    "Could not resolve URL template (missing parameter or setting?): {}",
                    url_template
                ),
            });
        }

        Some(Endpoint {
            id: stable_id(&method, &url_template),
            method,
            url_template,
            url_resolved,
            auth,
            discovered_in: vec![DiscoveryRef {
                workflow: workflow.into(),
                action: action_name.into(),
            }],
            request: RequestSpec {
                body_schema,
                headers,
            },
            response: ResponseSpec {
                schema: response_schema,
                example,
                status_code,
                branches: vec![],
            },
            side_effects: vec![],
        })
    }

    /// Scan the raw workflow text for `@{appsetting('X')}` references and record
    /// each one against the workflow folder, so the contract's app_settings entries
    /// show "used by" cross-references in the UI.
    fn collect_setting_references(&mut self, workflow: &str, raw: &str) {
        let re = match regex::Regex::new(r"@\{?appsetting\('([^']+)'\)\}?") {
            Ok(re) => re,
            Err(_) => return,
        };
        for caps in re.captures_iter(raw) {
            self.resolver.record_reference(&caps[1], workflow);
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────

/// Walks the workspace directory and returns `(folder_name, workflow.json path)`
/// pairs. A "workflow folder" is any direct child of the workspace containing a
/// `workflow.json`.
fn walk_workflows(workspace: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    let mut out = vec![];
    for entry in std::fs::read_dir(workspace)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let wf = path.join("workflow.json");
        if wf.is_file() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Skip hidden / cache folders just in case
            if !name.starts_with('.') {
                out.push((name, wf));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Deduplicate endpoints by `id`. When duplicates collide we union their
/// `discovered_in` lists so the contract reflects all call sites.
fn dedup_endpoints(eps: Vec<Endpoint>) -> Vec<Endpoint> {
    let mut by_id: BTreeMap<String, Endpoint> = BTreeMap::new();
    for ep in eps {
        match by_id.get_mut(&ep.id) {
            Some(existing) => {
                for r in ep.discovered_in {
                    if !existing
                        .discovered_in
                        .iter()
                        .any(|d| d.workflow == r.workflow && d.action == r.action)
                    {
                        existing.discovered_in.push(r);
                    }
                }
            }
            None => {
                by_id.insert(ep.id.clone(), ep);
            }
        }
    }
    let mut out: Vec<Endpoint> = by_id.into_values().collect();
    out.sort_by(|a, b| a.url_template.cmp(&b.url_template));
    out
}

fn parse_auth(v: Option<&serde_json::Value>) -> AuthKind {
    let Some(v) = v else { return AuthKind::None };
    let ty = v.get("type").and_then(|s| s.as_str()).unwrap_or("");
    match ty {
        "" => AuthKind::None,
        "ActiveDirectoryOAuth" => AuthKind::ActiveDirectoryOAuth {
            audience: v
                .get("audience")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .into(),
            tenant: v
                .get("tenant")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .into(),
        },
        "Basic" => AuthKind::Basic,
        "Bearer" | "Raw" => AuthKind::BearerToken,
        other => AuthKind::Unknown(other.into()),
    }
}

fn parse_headers(v: Option<&serde_json::Value>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(obj) = v.and_then(|v| v.as_object()) else {
        return out;
    };
    for (k, val) in obj {
        let s = match val {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        out.insert(k.clone(), s);
    }
    out
}

/// Infer a coarse JSON schema from an inline body example by walking the literal.
/// This is intentionally lightweight — we only care about top-level shape so the
/// mock server can validate "did the caller send something that looks right".
fn infer_body_schema(v: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    let v = v?;
    Some(infer_schema(v))
}

fn infer_schema(v: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value as V;
    match v {
        V::Null => serde_json::json!({ "type": "null" }),
        V::Bool(_) => serde_json::json!({ "type": "boolean" }),
        V::Number(_) => serde_json::json!({ "type": "number" }),
        V::String(_) => serde_json::json!({ "type": "string" }),
        V::Array(a) => {
            let items = a.first().map(infer_schema).unwrap_or(serde_json::json!({}));
            serde_json::json!({ "type": "array", "items": items })
        }
        V::Object(o) => {
            let mut props = serde_json::Map::new();
            for (k, val) in o {
                props.insert(k.clone(), infer_schema(val));
            }
            serde_json::json!({ "type": "object", "properties": props })
        }
    }
}

/// Find the response schema by scanning sibling actions for a `Parse_JSON`
/// whose `content` references `body('<http_action_name>')`.
fn find_response_schema(
    http_action_name: &str,
    siblings: &serde_json::Map<String, serde_json::Value>,
) -> Option<serde_json::Value> {
    let needle = format!("body('{}')", http_action_name);
    for (_name, action) in siblings {
        if action.get("type").and_then(|v| v.as_str()) != Some("ParseJson") {
            continue;
        }
        let content = action
            .get("inputs")
            .and_then(|v| v.get("content"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if content.contains(&needle) {
            return action.get("inputs").and_then(|v| v.get("schema")).cloned();
        }
    }
    None
}

/// Default 200; bump to 202 if the URL screams "async submission".
fn guess_status_code(url: &str, _method: &str) -> u16 {
    let u = url.to_lowercase();
    if u.contains("/executeasync") || u.contains("/submit") || u.ends_with("/queue") {
        202
    } else {
        200
    }
}

fn is_hardcoded(url: &str) -> bool {
    !url.contains("@{parameters(")
        && !url.contains("@parameters(")
        && !url.contains("@{appsetting(")
        && !url.contains("@appsetting(")
}

/// Stable id used to dedupe endpoints across workflows and across scan runs.
fn stable_id(method: &str, url_template: &str) -> String {
    let mut h = Sha256::new();
    h.update(method.as_bytes());
    h.update(b" ");
    h.update(url_template.as_bytes());
    let bytes = h.finalize();
    // 16 hex chars = 64 bits of namespace — collision-safe for any realistic platform.
    let mut s = String::with_capacity(16);
    for b in &bytes[..8] {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::sample_workspace;

    #[test]
    fn stable_id_is_deterministic() {
        let a = stable_id("POST", "@{parameters('Erp_Url')}/api/x");
        let b = stable_id("POST", "@{parameters('Erp_Url')}/api/x");
        assert_eq!(a, b);
        assert_ne!(a, stable_id("GET", "@{parameters('Erp_Url')}/api/x"));
    }

    #[test]
    fn hardcoded_detector() {
        assert!(is_hardcoded("https://example.com/api"));
        assert!(!is_hardcoded("@{parameters('Erp_Url')}/api"));
        assert!(!is_hardcoded("@{appsetting('Erp_Url')}/api"));
    }

    #[test]
    fn status_code_async_pattern() {
        assert_eq!(
            guess_status_code("https://x/api/bsfn/Foo/executeasync", "POST"),
            202
        );
        assert_eq!(guess_status_code("https://x/api/query/select", "POST"), 200);
        assert_eq!(
            guess_status_code("https://x/api/bsfn/Foo/status/123", "GET"),
            200
        );
    }

    #[test]
    fn auth_kinds_parse() {
        let v = serde_json::json!({
            "type": "ActiveDirectoryOAuth",
            "audience": "https://api.example",
            "tenant":   "tid-123"
        });
        match parse_auth(Some(&v)) {
            AuthKind::ActiveDirectoryOAuth { audience, tenant } => {
                assert_eq!(audience, "https://api.example");
                assert_eq!(tenant, "tid-123");
            }
            other => panic!("unexpected auth: {:?}", other),
        }
        assert!(matches!(parse_auth(None), AuthKind::None));
    }

    #[test]
    fn dedup_merges_discovered_in() {
        let mk = |wf: &str, action: &str| Endpoint {
            id: "abc".into(),
            method: "POST".into(),
            url_template: "https://x".into(),
            url_resolved: None,
            auth: AuthKind::None,
            discovered_in: vec![DiscoveryRef {
                workflow: wf.into(),
                action: action.into(),
            }],
            request: RequestSpec {
                body_schema: None,
                headers: BTreeMap::new(),
            },
            response: ResponseSpec {
                schema: None,
                example: serde_json::json!({}),
                status_code: 200,
                branches: vec![],
            },
            side_effects: vec![],
        };
        let out = dedup_endpoints(vec![mk("A", "a1"), mk("B", "b1"), mk("A", "a1")]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].discovered_in.len(), 2);
    }

    #[test]
    fn response_schema_found_via_body_reference() {
        // Build a fake siblings map with one HTTP action and one ParseJson that consumes it.
        let mut siblings: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
        siblings.insert("Call_Foo".into(), serde_json::json!({ "type": "Http" }));
        siblings.insert(
            "Parse_Response".into(),
            serde_json::json!({
                "type": "ParseJson",
                "inputs": {
                    "content": "@body('Call_Foo')",
                    "schema":  { "type": "object", "properties": { "ok": { "type": "boolean" } } }
                }
            }),
        );
        let schema = find_response_schema("Call_Foo", &siblings).expect("schema not found");
        assert_eq!(schema["properties"]["ok"]["type"], "boolean");
    }

    /// Smoke test against the real workspace, if it's present on this machine.
    /// Skips silently otherwise so CI on a clean checkout doesn't fail.
    /// Opt-in: point `AIS_SAMPLE_WORKSPACE` at a real Logic Apps project to
    /// run this against one. Skipped otherwise, so a clean checkout and CI
    /// both pass. It used to hardcode one developer's home directory, which
    /// meant it ran on exactly one machine and named the client's repo.
    #[test]
    fn smoke_real_workspace() {
        let Some(path) = sample_workspace() else {
            eprintln!("skipping smoke test — set AIS_SAMPLE_WORKSPACE to run it");
            return;
        };
        let path = path.join("logic_apps");
        let (contract, cache_path) = super::super::scan_workspace(&path)
            .expect("scan should succeed against real workspace");
        eprintln!(
            "smoke: {} endpoints, {} settings, {} warnings → {}",
            contract.endpoints.len(),
            contract.app_settings.len(),
            contract.warnings.len(),
            cache_path.display(),
        );
        // Sanity: we know there are hardcoded URLs in 4 workflows — expect at least 1 warning.
        let hardcoded_warns = contract
            .warnings
            .iter()
            .filter(|w| w.message.starts_with("Hardcoded URL"))
            .count();
        eprintln!("smoke: {} hardcoded-URL warnings", hardcoded_warns);
    }
}
