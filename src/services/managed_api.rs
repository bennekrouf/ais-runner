//! Tells apart "this connection is misconfigured" from "this connection can
//! never work here".
//!
//! Managed API connections (Teams, SharePoint, Log Analytics …) are fronted by
//! Azure's own connector infrastructure: their `connectionRuntimeUrl` points at
//! an APIM endpoint, and there is no emulator to stand in for it. A workflow
//! that references one is therefore reported unhealthy on every local start,
//! for good.
//!
//! Without this distinction the health warning tells the user to "Open
//! Connections and check endpoints" — an instruction that cannot succeed, on
//! workflows that are behaving exactly as expected offline. That noise sits
//! permanently next to the warnings that *are* actionable.

use std::collections::BTreeSet;

/// Names under `managedApiConnections` in connections.json.
///
/// Read from the file rather than hardcoded: the set differs per workspace,
/// and a hardcoded list silently stops matching the day someone adds a
/// connector.
pub fn managed_api_names(connections_json: &str) -> BTreeSet<String> {
    let parsed: serde_json::Value = match serde_json::from_str(connections_json) {
        Ok(v) => v,
        Err(_) => return BTreeSet::new(),
    };
    parsed
        .get("managedApiConnections")
        .and_then(|m| m.as_object())
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

/// Which managed API connections a workflow references, via the
/// `"referenceName"` used by `ApiConnection` actions.
pub fn referenced_managed_apis(
    workflow_json: &str,
    managed: &BTreeSet<String>,
) -> Vec<String> {
    if managed.is_empty() {
        return Vec::new();
    }
    let parsed: serde_json::Value = match serde_json::from_str(workflow_json) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut found = BTreeSet::new();
    collect_reference_names(&parsed, managed, &mut found);
    found.into_iter().collect()
}

fn collect_reference_names(
    node: &serde_json::Value,
    managed: &BTreeSet<String>,
    out: &mut BTreeSet<String>,
) {
    match node {
        serde_json::Value::Object(map) => {
            if let Some(name) = map.get("referenceName").and_then(|v| v.as_str()) {
                if managed.contains(name) {
                    out.insert(name.to_string());
                }
            }
            for v in map.values() {
                collect_reference_names(v, managed, out);
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                collect_reference_names(v, managed, out);
            }
        }
        _ => {}
    }
}

/// Load the managed API names for a workspace. Empty when connections.json is
/// absent or unreadable — callers then fall back to the generic warning.
pub fn managed_api_names_in(func_cwd: &str) -> BTreeSet<String> {
    std::fs::read_to_string(std::path::Path::new(func_cwd).join("connections.json"))
        .map(|s| managed_api_names(&s))
        .unwrap_or_default()
}

/// The managed APIs a named workflow references, read from its workflow.json.
pub fn workflow_managed_apis(func_cwd: &str, workflow: &str, managed: &BTreeSet<String>) -> Vec<String> {
    let path = std::path::Path::new(func_cwd).join(workflow).join("workflow.json");
    std::fs::read_to_string(path)
        .map(|s| referenced_managed_apis(&s, managed))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONNECTIONS: &str = r#"{
        "functionConnections": { "IgniteInvoiceProcessing": {} },
        "managedApiConnections": {
            "azureLogAnalyticsDataCollector": {},
            "sharepointonline": {},
            "teams": {}
        },
        "serviceProviderConnections": { "serviceBus": {}, "sql-server-ais": {} }
    }"#;

    fn managed() -> BTreeSet<String> {
        managed_api_names(CONNECTIONS)
    }

    #[test]
    fn reads_only_the_managed_api_block() {
        let m = managed();
        assert!(m.contains("teams"));
        assert!(m.contains("sharepointonline"));
        assert!(m.contains("azureLogAnalyticsDataCollector"));
        // Service providers and function connections all have local equivalents.
        assert!(!m.contains("serviceBus"));
        assert!(!m.contains("sql-server-ais"));
        assert!(!m.contains("IgniteInvoiceProcessing"));
    }

    #[test]
    fn malformed_connections_json_yields_nothing() {
        assert!(managed_api_names("{not json").is_empty());
        assert!(managed_api_names("{}").is_empty());
    }

    /// Shape taken from AIS-GenericNotif: the reference is nested several
    /// levels down inside an action's `inputs.host.connection`.
    #[test]
    fn finds_a_reference_nested_in_an_action() {
        let wf = r#"{"definition":{"actions":{"Post":{"type":"ApiConnection",
            "inputs":{"host":{"connection":{"referenceName":"teams"}}}}}}}"#;
        assert_eq!(referenced_managed_apis(wf, &managed()), vec!["teams"]);
    }

    #[test]
    fn finds_references_inside_arrays_and_branches() {
        let wf = r#"{"definition":{"actions":{"If":{"else":{"actions":[
            {"inputs":{"host":{"connection":{"referenceName":"sharepointonline"}}}}
        ]}}}}}"#;
        assert_eq!(referenced_managed_apis(wf, &managed()), vec!["sharepointonline"]);
    }

    #[test]
    fn service_provider_connections_are_not_reported() {
        // connectionName (service provider) must not be confused with
        // referenceName (managed API) — these do have local emulators.
        let wf = r#"{"definition":{"actions":{"Send":{"inputs":{
            "serviceProviderConfiguration":{"connectionName":"serviceBus"}}}}}}"#;
        assert!(referenced_managed_apis(wf, &managed()).is_empty());
    }

    #[test]
    fn deduplicates_and_sorts_repeated_references() {
        let wf = r#"{"definition":{"actions":{
            "A":{"inputs":{"host":{"connection":{"referenceName":"teams"}}}},
            "B":{"inputs":{"host":{"connection":{"referenceName":"teams"}}}},
            "C":{"inputs":{"host":{"connection":{"referenceName":"sharepointonline"}}}}
        }}}"#;
        assert_eq!(referenced_managed_apis(wf, &managed()), vec!["sharepointonline", "teams"]);
    }

    #[test]
    fn workflow_with_no_managed_api_is_empty() {
        let wf = r#"{"definition":{"actions":{"X":{"type":"Compose","inputs":"hi"}}}}"#;
        assert!(referenced_managed_apis(wf, &managed()).is_empty());
    }

    #[test]
    fn unknown_reference_name_is_ignored() {
        // A referenceName that is not declared as a managed API (e.g. a stale
        // leftover) must not be excused as "expected offline".
        let wf = r#"{"definition":{"actions":{"X":{"inputs":{"host":{"connection":
            {"referenceName":"someRemovedConnector"}}}}}}}"#;
        assert!(referenced_managed_apis(wf, &managed()).is_empty());
    }
}
