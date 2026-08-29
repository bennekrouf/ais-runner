//! Service Bus lookups against Azure: namespaces, resource groups, queue
//! metadata and message counts.
//!
//! Read-only and non-interactive — safe to call headlessly, given a session
//! already established via [`super::auth`].

use super::cli::{run, AzError};

/// Finds the resource group of a Service Bus namespace by name (searches across the subscription).
pub fn find_servicebus_rg(subscription: &str, namespace: &str) -> Result<String, AzError> {
    run(&[
        "servicebus",
        "namespace",
        "show",
        "--subscription",
        subscription,
        "--name",
        namespace,
        "--query",
        "resourceGroup",
        "-o",
        "tsv",
    ])
}

/// Lists all Service Bus namespaces in a subscription (no resource group needed).
pub fn list_all_servicebus_namespaces(subscription: &str) -> Result<Vec<String>, AzError> {
    let out = run(&[
        "servicebus",
        "namespace",
        "list",
        "--subscription",
        subscription,
        "--query",
        "[].name",
        "-o",
        "tsv",
    ])?;
    Ok(out
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

// ── Service Bus data-plane helpers ─────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct SbQueueStats {
    pub active_message_count: u64,
    pub dead_letter_count: u64,
    pub size_bytes: u64,
}

/// List all Service Bus namespaces accessible in the current subscription.
/// Returns (short_name, fqdn, resource_group) triples.
pub fn sb_list_namespaces() -> Result<Vec<(String, String, String)>, AzError> {
    let out = run(&[
        "servicebus",
        "namespace",
        "list",
        "--query",
        "[].{name:name,fqdn:serviceBusEndpoint,rg:resourceGroup}",
        "-o",
        "json",
    ])?;
    let arr: serde_json::Value =
        serde_json::from_str(&out).map_err(|e| AzError::Other(e.to_string()))?;
    let mut result = Vec::new();
    if let Some(items) = arr.as_array() {
        for item in items {
            let name = item["name"].as_str().unwrap_or("").to_string();
            let rg = item["rg"].as_str().unwrap_or("").to_string();
            // fqdn from the API looks like "https://sbns-xxx.servicebus.windows.net:443/"
            // strip to bare hostname
            let raw_fqdn = item["fqdn"].as_str().unwrap_or("");
            let fqdn = raw_fqdn
                .trim_start_matches("https://")
                .trim_end_matches(":443/")
                .trim_end_matches('/')
                .to_string();
            if !name.is_empty() {
                result.push((name, fqdn, rg));
            }
        }
    }
    Ok(result)
}

/// Find the resource group of a SB namespace without knowing it upfront.
/// Pass `subscription` to target the correct subscription directly.
pub fn sb_find_rg(namespace_fqdn: &str, subscription: Option<&str>) -> Result<String, AzError> {
    let short_name = namespace_fqdn.split('.').next().unwrap_or(namespace_fqdn);
    // Use a more global search that doesn't require --resource-group itself to find the group
    let mut args = vec![
        "resource",
        "list",
        "--name",
        short_name,
        "--resource-type",
        "Microsoft.ServiceBus/namespaces",
        "--query",
        "[0].resourceGroup",
        "-o",
        "tsv",
    ];
    if let Some(sub) = subscription {
        args.push("--subscription");
        args.push(sub);
    }
    let rg = run(&args)?;
    let rg = rg.trim();
    if rg.is_empty() || rg == "None" || rg == "null" {
        return Err(AzError::Other(format!(
            "Namespace '{}' not found in active subscription — check az login",
            short_name
        )));
    }
    Ok(rg.to_string())
}

/// Fetch active + DLQ message counts and size for one queue.
pub fn sb_queue_stats(
    rg: &str,
    namespace_fqdn: &str,
    queue: &str,
) -> Result<SbQueueStats, AzError> {
    let short_name = namespace_fqdn.split('.').next().unwrap_or(namespace_fqdn);
    let out = run(&[
        "servicebus", "queue", "show",
        "--resource-group", rg,
        "--namespace-name", short_name,
        "--name", queue,
        "--query",
        "{active:countDetails.activeMessageCount,dlq:countDetails.deadLetterMessageCount,size:sizeInBytes}",
        "-o", "json",
    ])?;
    let v: serde_json::Value =
        serde_json::from_str(&out).map_err(|e| AzError::Other(e.to_string()))?;
    Ok(SbQueueStats {
        active_message_count: v["active"].as_u64().unwrap_or(0),
        dead_letter_count: v["dlq"].as_u64().unwrap_or(0),
        size_bytes: v["size"].as_u64().unwrap_or(0),
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct SbQueueDetail {
    pub name: String,
    pub status: String,
    pub max_size_mb: u64,
    pub active_messages: u64,
    pub dead_letter: u64,
    pub requires_session: bool,
    pub max_delivery: u32,
    pub lock_duration: String,
    pub default_ttl: String,
    pub auto_delete: String,
}

/// List all queues in a Service Bus namespace.
/// Returns queue names sorted alphabetically.
pub fn sb_list_queues(
    subscription: &str,
    rg: &str,
    namespace: &str,
) -> Result<Vec<SbQueueDetail>, AzError> {
    let short_name = namespace.split('.').next().unwrap_or(namespace);
    let out = run(&[
        "servicebus", "queue", "list",
        "--subscription", subscription,
        "--resource-group", rg,
        "--namespace-name", short_name,
        "--query", "[].{name:name,status:status,maxSize:maxSizeInMegabytes,msgCount:countDetails.activeMessageCount,dlq:countDetails.deadLetterMessageCount,requiresSession:requiresSession,maxDelivery:maxDeliveryCount,lockDuration:lockDuration,defaultTtl:defaultMessageTimeToLive,autoDelete:autoDeleteOnIdle}",
        "-o", "json",
    ])?;
    let arr: serde_json::Value =
        serde_json::from_str(&out).map_err(|e| AzError::Other(e.to_string()))?;
    let mut result = Vec::new();
    if let Some(items) = arr.as_array() {
        for item in items {
            result.push(SbQueueDetail {
                name: item["name"].as_str().unwrap_or("").to_string(),
                status: item["status"].as_str().unwrap_or("Active").to_string(),
                max_size_mb: item["maxSize"].as_u64().unwrap_or(0),
                active_messages: item["msgCount"].as_u64().unwrap_or(0),
                dead_letter: item["dlq"].as_u64().unwrap_or(0),
                requires_session: item["requiresSession"].as_bool().unwrap_or(false),
                max_delivery: item["maxDelivery"].as_u64().unwrap_or(10) as u32,
                lock_duration: item["lockDuration"].as_str().unwrap_or("").to_string(),
                default_ttl: item["defaultTtl"].as_str().unwrap_or("").to_string(),
                auto_delete: item["autoDelete"].as_str().unwrap_or("").to_string(),
            });
        }
    }
    result.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(result)
}

/// Fetches the primary connection string for a Service Bus namespace.
pub fn fetch_servicebus_connection_string(
    subscription: &str,
    rg: &str,
    namespace: &str,
) -> Result<String, AzError> {
    run(&[
        "servicebus",
        "namespace",
        "authorization-rule",
        "keys",
        "list",
        "--subscription",
        subscription,
        "--resource-group",
        rg,
        "--namespace-name",
        namespace,
        "--name",
        "RootManageSharedAccessKey",
        "--query",
        "primaryConnectionString",
        "-o",
        "tsv",
    ])
}
