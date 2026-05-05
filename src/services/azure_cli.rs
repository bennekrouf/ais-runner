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

/// Opens a new Terminal window on macOS and runs `az login`, so the interactive
/// login flow is visible without blocking the runner's terminal.
pub fn launch_az_login(subscription_id: Option<String>) {
    let sub_cmd = match subscription_id {
        Some(id) if !id.is_empty() =>
            format!(" && az account set --subscription {}", id),
        _ => String::new(),
    };
    let inner = format!(
        "az login --tenant 68fac18b-9e76-4cef-b2b7-2c51b521cb94{}; echo ''; echo '✅ Done — close this window and retry in the runner.'",
        sub_cmd
    );
    let script = format!(
        "tell application \"Terminal\"\n    activate\n    do script \"{}\"\nend tell",
        inner.replace('"', "\\\"")
    );
    let _ = std::process::Command::new("osascript")
        .args(["-e", &script])
        .spawn();
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

/// Lists all Resource Groups in a subscription.
pub fn list_resource_groups(subscription: &str) -> Result<Vec<String>, AzError> {
    let out = run(&[
        "group", "list",
        "--subscription", subscription,
        "--query", "[].name",
        "-o", "tsv",
    ])?;
    Ok(out.lines().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
}


/// Fetches the Subscription ID that a given Resource Group belongs to.
/// Useful for "self-healing" when a user enters a broken sub ID but has the right RG.
pub fn get_subscription_id_by_group(resource_group: &str) -> Result<String, AzError> {
    let out = run(&[
        "group", "show",
        "--name", resource_group,
        "--query", "id",
        "-o", "tsv",
    ])?;
    // The ID looks like: /subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/rg-name
    let parts: Vec<&str> = out.split('/').collect();
    if parts.len() >= 3 && parts[1] == "subscriptions" {
        Ok(parts[2].to_string())
    } else {
        Err(AzError::Other(format!("Could not parse subscription ID from group metadata: {}", out)))
    }
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
/// Pass `subscription` to target the correct subscription directly.
pub fn sb_find_rg(namespace_fqdn: &str, subscription: Option<&str>) -> Result<String, AzError> {
    let short_name = namespace_fqdn.split('.').next().unwrap_or(namespace_fqdn);
    // Use a more global search that doesn't require --resource-group itself to find the group
    let mut args = vec![
        "resource", "list",
        "--name", short_name,
        "--resource-type", "Microsoft.ServiceBus/namespaces",
        "--query", "[0].resourceGroup",
        "-o", "tsv",
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

/// Sets the active subscription for subsequent az commands.
pub fn set_subscription(subscription_id: &str) -> Result<(), AzError> {
    run(&["account", "set", "--subscription", subscription_id]).map(|_| ())
}

/// List all queue names in a Service Bus namespace.
pub fn sb_list_queues(rg: &str, namespace_fqdn: &str) -> Result<Vec<String>, AzError> {
    let short_name = namespace_fqdn.split('.').next().unwrap_or(namespace_fqdn);
    let out = run(&[
        "servicebus", "queue", "list",
        "--resource-group", rg,
        "--namespace-name", short_name,
        "--query", "[].name",
        "-o", "tsv",
    ])?;
    Ok(out.lines().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
}

/// Create a queue in a Service Bus namespace.
pub fn sb_create_queue(rg: &str, namespace_fqdn: &str, queue: &str) -> Result<(), AzError> {
    let short_name = namespace_fqdn.split('.').next().unwrap_or(namespace_fqdn);
    run(&[
        "servicebus", "queue", "create",
        "--resource-group", rg,
        "--namespace-name", short_name,
        "--name", queue,
    ]).map(|_| ())
}

/// List all storage accounts in the subscription with their blob endpoint URLs.
/// Returns (account_name, blob_endpoint) pairs.
pub fn list_storage_accounts(subscription: Option<&str>) -> Result<Vec<(String, String)>, AzError> {
    let mut args = vec![
        "storage", "account", "list",
        "--query", "[].{name:name,ep:primaryEndpoints.blob}",
        "-o", "tsv",
    ];
    let sub_owned;
    if let Some(sub) = subscription {
        sub_owned = sub.to_string();
        args.push("--subscription");
        args.push(&sub_owned);
    }
    let out = run(&args)?;
    Ok(out.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, '\t');
            let name = parts.next()?.trim().to_string();
            let ep   = parts.next().unwrap_or("").trim().trim_end_matches('/').to_string();
            if name.is_empty() { None } else { Some((name, ep)) }
        })
        .collect())
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

// ── Azurite / local blob storage helpers ──────────────────────────────────

#[allow(dead_code)]
pub const AZURITE_CONN_STR: &str = "UseDevelopmentStorage=true";

#[derive(Debug, Clone)]
pub struct BlobInfo {
    pub name: String,
    pub size: u64,
}

#[allow(dead_code)]
pub fn storage_create_container(conn_str: &str, container: &str) -> Result<(), AzError> {
    run(&["storage", "container", "create", "--connection-string", conn_str, "--name", container]).map(|_| ())
}

#[allow(dead_code)]
pub fn storage_list_containers(conn_str: &str) -> Result<Vec<String>, AzError> {
    let out = run(&[
        "storage", "container", "list",
        "--connection-string", conn_str,
        "--query", "[].name",
        "-o", "tsv",
    ])?;
    Ok(out.lines().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
}

#[allow(dead_code)]
pub fn storage_list_blobs(conn_str: &str, container: &str) -> Result<Vec<BlobInfo>, AzError> {
    let out = run(&[
        "storage", "blob", "list",
        "--container-name", container,
        "--connection-string", conn_str,
        "--query", "[].{name:name,size:properties.contentLength}",
        "-o", "json",
    ])?;
    let arr: serde_json::Value =
        serde_json::from_str(&out).map_err(|e| AzError::Other(e.to_string()))?;
    let mut result = Vec::new();
    if let Some(items) = arr.as_array() {
        for item in items {
            let name = item["name"].as_str().unwrap_or("").to_string();
            let size = item["size"].as_u64().unwrap_or(0);
            if !name.is_empty() {
                result.push(BlobInfo { name, size });
            }
        }
    }
    Ok(result)
}

#[allow(dead_code)]
pub fn storage_clear_container(conn_str: &str, container: &str) -> Result<u64, AzError> {
    let out = run(&[
        "storage", "blob", "delete-batch",
        "--source", container,
        "--connection-string", conn_str,
    ])?;
    Ok(out.trim().parse().unwrap_or(0))
}

#[allow(dead_code)]
pub fn storage_upload_blob(conn_str: &str, container: &str, file_path: &str, blob_name: &str) -> Result<(), AzError> {
    run(&[
        "storage", "blob", "upload",
        "--file", file_path,
        "--container-name", container,
        "--name", blob_name,
        "--connection-string", conn_str,
        "--overwrite", "true",
    ]).map(|_| ())
}

