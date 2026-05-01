use std::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub enum AzError {
    NotLoggedIn,
    Other(String),
}


pub fn az_command(args: &[&str]) -> Command {
    if cfg!(target_os = "windows") {
        let mut cmd = Command::new("cmd");
        cmd.args(["/c", "az"]).args(args);
        cmd
    } else {
        let mut cmd = Command::new("az");
        cmd.args(args);
        cmd
    }
}

fn run(args: &[&str]) -> Result<String, AzError> {
    let out = az_command(args)
        .output()
        .map_err(|e| AzError::Other(format!("az not found: {}", e)))?;

    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();

    if !out.status.success() {
        let combined = format!("{} {}", stdout, stderr);
        if combined.contains("AADSTS") || combined.contains("az login") || combined.contains("refresh token") {
            return Err(AzError::NotLoggedIn);
        }
        return Err(AzError::Other(stderr));
    }

    Ok(stdout)
}

/// Checks whether az is logged in. Returns the signed-in account name on success.
pub fn check_login() -> Result<String, AzError> {
    run(&["account", "show", "--query", "user.name", "-o", "tsv"])
}

/// Spawns `az login` which opens a browser tab for OAuth — no terminal needed.
pub fn open_login(tenant: &str) {
    let _ = az_command(&["login", "--tenant", tenant, "--scope", "https://management.core.windows.net//.default"])
        .spawn();
}

/// Signs out of the current az session.
pub fn logout() {
    let _ = az_command(&["logout"]).spawn();
}

/// Finds the resource group of a Service Bus namespace by name (searches across the subscription).
pub fn find_servicebus_rg(subscription: &str, namespace: &str) -> Result<String, AzError> {
    run(&[
        "servicebus", "namespace", "show",
        "--subscription", subscription,
        "--name", namespace,
        "--query", "resourceGroup",
        "-o", "tsv",
    ])
}

/// Lists all subscriptions the logged-in account has access to.
/// Returns (id, name) pairs.
pub fn list_subscriptions() -> Result<Vec<(String, String)>, AzError> {
    let out = run(&[
        "account", "list",
        "--query", "[].{id:id,name:name}",
        "-o", "tsv",
    ])?;
    Ok(out.lines()
        .filter_map(|line| {
            let mut cols = line.splitn(2, '\t');
            let id   = cols.next()?.trim().to_string();
            let name = cols.next().unwrap_or("").trim().to_string();
            if id.is_empty() { None } else { Some((id, name)) }
        })
        .collect())
}

/// Lists all Service Bus namespaces in a subscription (no resource group needed).
pub fn list_all_servicebus_namespaces(subscription: &str) -> Result<Vec<String>, AzError> {
    let out = run(&[
        "servicebus", "namespace", "list",
        "--subscription", subscription,
        "--query", "[].name",
        "-o", "tsv",
    ])?;
    Ok(out.lines().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
}

// ── Service Bus data-plane helpers ─────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct SbQueueStats {
    pub active_message_count: u64,
    pub dead_letter_count:    u64,
    pub size_bytes:           u64,
}

/// List all Service Bus namespaces accessible in the current subscription.
/// Returns (short_name, fqdn, resource_group) triples.
pub fn sb_list_namespaces() -> Result<Vec<(String, String, String)>, AzError> {
    let out = run(&[
        "servicebus", "namespace", "list",
        "--query", "[].{name:name,fqdn:serviceBusEndpoint,rg:resourceGroup}",
        "-o", "json",
    ])?;
    let arr: serde_json::Value =
        serde_json::from_str(&out).map_err(|e| AzError::Other(e.to_string()))?;
    let mut result = Vec::new();
    if let Some(items) = arr.as_array() {
        for item in items {
            let name = item["name"].as_str().unwrap_or("").to_string();
            let rg   = item["rg"].as_str().unwrap_or("").to_string();
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
pub fn sb_find_rg(namespace_fqdn: &str) -> Result<String, AzError> {
    let short_name = namespace_fqdn.split('.').next().unwrap_or(namespace_fqdn);
    let rg = run(&[
        "resource", "list",
        "--resource-type", "Microsoft.ServiceBus/namespaces",
        "--name", short_name,
        "--query", "[0].resourceGroup",
        "-o", "tsv",
    ])?;
    if rg.is_empty() || rg == "None" || rg == "null" {
        return Err(AzError::Other(format!(
            "Namespace '{}' not found in current subscription — run az account show to check",
            short_name
        )));
    }
    Ok(rg)
}

/// Fetch active + DLQ message counts and size for one queue.
pub fn sb_queue_stats(rg: &str, namespace_fqdn: &str, queue: &str) -> Result<SbQueueStats, AzError> {
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
        dead_letter_count:    v["dlq"].as_u64().unwrap_or(0),
        size_bytes:           v["size"].as_u64().unwrap_or(0),
    })
}

/// Fetch the primary connection string for a SB namespace (requires Contributor or higher).
pub fn sb_fetch_conn_str(rg: &str, namespace_fqdn: &str) -> Result<String, AzError> {
    let short_name = namespace_fqdn.split('.').next().unwrap_or(namespace_fqdn);
    run(&[
        "servicebus", "namespace", "authorization-rule", "keys", "list",
        "--resource-group", rg,
        "--namespace-name", short_name,
        "--name", "RootManageSharedAccessKey",
        "--query", "primaryConnectionString",
        "-o", "tsv",
    ])
}

/// Obtain an AAD Bearer token scoped to the Service Bus data-plane.
pub fn sb_get_bearer_token() -> Result<String, AzError> {
    run(&[
        "account", "get-access-token",
        "--resource", "https://servicebus.azure.net/",
        "--query", "accessToken",
        "-o", "tsv",
    ])
}

/// Fetches the primary connection string for a Service Bus namespace.
pub fn fetch_servicebus_connection_string(subscription: &str, rg: &str, namespace: &str) -> Result<String, AzError> {
    run(&[
        "servicebus", "namespace", "authorization-rule", "keys", "list",
        "--subscription", subscription,
        "--resource-group", rg,
        "--namespace-name", namespace,
        "--name", "RootManageSharedAccessKey",
        "--query", "primaryConnectionString",
        "-o", "tsv",
    ])
}

