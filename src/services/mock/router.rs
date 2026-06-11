//! Match an incoming HTTP request against the endpoints declared in a
//! `MockContract`.
//!
//! The contract's URL templates contain two kinds of placeholders we need to
//! handle:
//!
//! 1. `@{parameters('X')}` / `@{appsetting('X')}` — resolved by `Resolver`
//!    at scan time, but the resolved value still embeds variables like
//!    `{methodName}` for path parameters.
//!
//! 2. `{X}` (curly-brace placeholders, openapi style) — these match any
//!    non-slash segment in the actual request path.
//!
//! Strategy: each contract endpoint is compiled once into a `CompiledRoute`
//! holding a regex; routing is just a linear scan + regex match. The
//! contract typically has < 30 endpoints so linear is fine.

use std::sync::Arc;

use regex::Regex;

use crate::services::mock::contract::{Endpoint, MockContract};

/// A request slice that the router can match against.
pub struct Incoming<'a> {
    pub method: &'a str,
    pub path:   &'a str,        // path + query of the incoming request
}

#[derive(Clone)]
pub struct CompiledRoute {
    pub endpoint: Endpoint,
    method:       String,
    /// Regex applied to the request path. `None` if compilation failed (the
    /// endpoint is then unreachable but we keep it in the table for diagnostics).
    matcher:      Option<Regex>,
    /// Resolved URL path component used to build `matcher` — kept for debugging.
    #[allow(dead_code)]
    pattern:      String,
}

pub struct Router {
    routes: Vec<CompiledRoute>,
}

impl Router {
    /// Compile every endpoint into a route.
    pub fn from_contract(contract: &MockContract) -> Self {
        let routes = contract.endpoints.iter()
            .map(|ep| CompiledRoute::compile(ep.clone()))
            .collect();
        Self { routes }
    }

    /// Find the first endpoint whose method + URL pattern match the incoming
    /// request. Linear scan; returns the cloned endpoint or `None`.
    pub fn route(&self, incoming: &Incoming<'_>) -> Option<Arc<Endpoint>> {
        for r in &self.routes {
            if !r.method.eq_ignore_ascii_case(incoming.method) { continue; }
            let Some(re) = &r.matcher else { continue };
            if re.is_match(incoming.path) {
                return Some(Arc::new(r.endpoint.clone()));
            }
        }
        None
    }

    pub fn len(&self) -> usize { self.routes.len() }
}

impl CompiledRoute {
    fn compile(endpoint: Endpoint) -> Self {
        let method  = endpoint.method.clone();
        // We match against the path-and-query of the request. We strip the
        // scheme://host:port prefix from the contract URL because the mock
        // server doesn't see those — they're consumed by the proxy/rewrite layer.
        let pattern = path_of(endpoint.url_resolved.as_deref().unwrap_or(&endpoint.url_template));
        let regex   = pattern_to_regex(&pattern);
        let matcher = Regex::new(&regex).ok();
        Self { endpoint, method, matcher, pattern }
    }
}

/// Extract the path+query portion of a URL string, leaving placeholders intact.
fn path_of(url: &str) -> String {
    // Strip the prefix up to and including the first `/` after the scheme,
    // OR everything before the first `/` if the URL starts with one.
    if let Some(rest) = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://")) {
        match rest.find('/') {
            Some(i) => rest[i..].to_string(),
            None    => "/".to_string(),
        }
    } else if url.starts_with('/') {
        url.to_string()
    } else {
        // Template starts with a placeholder like `@{parameters('Jde_Url')}` —
        // strip up to the first `/`.
        match url.find('/') {
            Some(i) => url[i..].to_string(),
            None    => "/".to_string(),
        }
    }
}

/// Convert a path pattern with `{X}` placeholders into a regex anchored at start/end.
/// Trailing slash is optional.
fn pattern_to_regex(pattern: &str) -> String {
    let mut out = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                // consume until matching '}'
                let mut name = String::new();
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if n == '}' { break; }
                    name.push(n);
                }
                let _ = name;                       // we don't capture, just match
                out.push_str("[^/?]+");
            }
            // Regex metachars in URL paths
            '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '|' | '\\' | '^' | '$' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    // optional trailing slash, optional query string
    out.push_str("/?(\\?.*)?$");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::mock::contract::{AuthKind, DiscoveryRef, Endpoint, RequestSpec, ResponseSpec};
    use std::collections::BTreeMap;

    fn make_endpoint(method: &str, url: &str) -> Endpoint {
        Endpoint {
            id:            "x".into(),
            method:        method.into(),
            url_template:  url.into(),
            url_resolved:  Some(url.into()),
            auth:          AuthKind::None,
            discovered_in: vec![DiscoveryRef { workflow: "wf".into(), action: "a".into() }],
            request:       RequestSpec { body_schema: None, headers: BTreeMap::new() },
            response:      ResponseSpec {
                schema:      None,
                example:     serde_json::json!({}),
                status_code: 200,
                branches:    vec![],
            },
            side_effects:  vec![],
        }
    }

    fn router_of(endpoints: Vec<Endpoint>) -> Router {
        Router {
            routes: endpoints.into_iter().map(CompiledRoute::compile).collect(),
        }
    }

    #[test]
    fn match_exact_path() {
        let r = router_of(vec![
            make_endpoint("GET", "https://api.example.com/health"),
        ]);
        assert!(r.route(&Incoming { method: "GET", path: "/health" }).is_some());
        assert!(r.route(&Incoming { method: "GET", path: "/other" }).is_none());
    }

    #[test]
    fn match_placeholder_segment() {
        let r = router_of(vec![
            make_endpoint("POST", "https://x/api/bsfn/{methodName}/executeasync"),
        ]);
        assert!(r.route(&Incoming { method: "POST", path: "/api/bsfn/Foo/executeasync" }).is_some());
        assert!(r.route(&Incoming { method: "POST", path: "/api/bsfn/Foo/Bar/executeasync" }).is_none());
    }

    #[test]
    fn method_mismatch() {
        let r = router_of(vec![ make_endpoint("POST", "https://x/api/health") ]);
        assert!(r.route(&Incoming { method: "GET", path: "/api/health" }).is_none());
    }

    #[test]
    fn trailing_slash_and_query_tolerated() {
        let r = router_of(vec![ make_endpoint("GET", "https://x/api/items") ]);
        assert!(r.route(&Incoming { method: "GET", path: "/api/items" }).is_some());
        assert!(r.route(&Incoming { method: "GET", path: "/api/items/" }).is_some());
        assert!(r.route(&Incoming { method: "GET", path: "/api/items?limit=10" }).is_some());
    }

    #[test]
    fn template_without_scheme_still_matches() {
        // URL template can begin with @{parameters('...')} — path_of must still extract /api/...
        let r = router_of(vec![
            make_endpoint("POST", "@{parameters('Jde_Url')}/api/query/execute"),
        ]);
        assert!(r.route(&Incoming { method: "POST", path: "/api/query/execute" }).is_some());
    }
}
