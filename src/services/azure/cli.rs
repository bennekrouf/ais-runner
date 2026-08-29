use std::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub enum AzError {
    NotLoggedIn,
    Other(String),
}

/// Resolve the full path to the `az` CLI on Windows.
///
/// Azure CLI on Windows installs as `az.cmd` (a batch wrapper) in a
/// non-standard directory that is usually on the user PATH inside a terminal
/// but missing from the minimal PATH that GUI apps inherit when launched from
/// the desktop or Start Menu.
///
/// Common install locations (checked in order):
///   %ProgramFiles(x86)%\Microsoft SDKs\Azure\CLI2\wbin   ← MSI default
///   %ProgramFiles%\Microsoft SDKs\Azure\CLI2\wbin         ← 64-bit MSI
///   %LOCALAPPDATA%\Programs\Azure CLI\wbin                 ← per-user install
#[cfg(target_os = "windows")]
fn resolve_az_windows() -> String {
    let candidates: &[(&str, &str)] = &[
        (
            "ProgramFiles(x86)",
            r"Microsoft SDKs\Azure\CLI2\wbin\az.cmd",
        ),
        ("ProgramFiles", r"Microsoft SDKs\Azure\CLI2\wbin\az.cmd"),
        ("LOCALAPPDATA", r"Programs\Azure CLI\wbin\az.cmd"),
    ];
    for (env_var, suffix) in candidates {
        if let Ok(base) = std::env::var(env_var) {
            let full = std::path::Path::new(&base).join(suffix);
            if full.is_file() {
                return full.to_string_lossy().to_string();
            }
        }
    }
    // Last resort: hope it's on PATH
    "az".to_string()
}

pub fn az_command(args: &[&str]) -> Command {
    if cfg!(target_os = "windows") {
        #[cfg(target_os = "windows")]
        let az_path = resolve_az_windows();
        #[cfg(not(target_os = "windows"))]
        let _az_path = "az".to_string();

        let mut cmd = Command::new("cmd");
        cmd.args(["/c", "az"]).args(args);

        // If az was found outside the inherited PATH (e.g. C:\Program Files\...),
        // inject its directory into the child's PATH so cmd.exe resolves "az" by
        // name — avoids quoting a path with spaces which cmd /c misparses.
        #[cfg(target_os = "windows")]
        if az_path != "az" {
            if let Some(dir) = std::path::Path::new(&az_path).parent() {
                let current = std::env::var("PATH").unwrap_or_default();
                cmd.env("PATH", format!("{};{}", dir.display(), current));
            }
        }

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
        if combined.contains("AADSTS")
            || combined.contains("az login")
            || combined.contains("refresh token")
        {
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

/// Returns the tenant ID of the currently active az session.
pub fn get_active_tenant() -> Result<String, AzError> {
    run(&["account", "show", "--query", "tenantId", "-o", "tsv"])
}

/// Opens a new Terminal window on macOS and runs `az login`.
/// Pass `tenant_id: Some("…")` to target a specific Azure AD tenant;
/// `None` lets `az` use the user's default tenant.
pub fn launch_az_login(subscription_id: Option<String>, tenant_id: Option<String>) {
    let tenant_flag = match tenant_id.as_deref() {
        Some(t) if !t.is_empty() => format!(" --tenant {}", t),
        _ => String::new(),
    };
    let sub_cmd = match subscription_id {
        Some(id) if !id.is_empty() => format!(" && az account set --subscription {}", id),
        _ => String::new(),
    };
    let inner = format!(
        "az login{}{}; echo ''; echo '✅ Done — close this window and retry in the runner.'",
        tenant_flag, sub_cmd
    );
    let script = format!(
        "tell application \"Terminal\"\n    activate\n    do script \"{}\"\nend tell",
        inner.replace('"', "\\\"")
    );
    let _ = std::process::Command::new("osascript")
        .args(["-e", &script])
        .spawn();
}

/// Spawns `az login` which opens a browser tab for OAuth.
///
/// On Windows we open a *visible* new console window so the user sees az's
/// output (device-code prompt, success/failure message) — a GUI-launched
/// child cmd is normally invisible, which silently swallows errors when az
/// fails to launch the browser.
///
/// Returns `Ok(())` if the child process was spawned successfully, or a
/// human-readable error message that should be surfaced in the UI — silent
/// spawn failures are the #1 reason "click did nothing" is reported.
pub fn open_login(tenant: Option<&str>) -> Result<(), String> {
    let mut args: Vec<String> = vec!["login".into()];
    if let Some(t) = tenant.filter(|t| !t.is_empty()) {
        args.push("--tenant".into());
        args.push(t.to_string());
    }
    args.push("--scope".into());
    args.push("https://management.core.windows.net//.default".into());

    #[cfg(target_os = "windows")]
    {
        // Use `cmd /c start "title" cmd /k "<az> login ..."` so the user sees
        // a console window and can complete the device-code flow if needed.
        // `/k` keeps it open after az exits so the user sees errors/success.
        let az_path = resolve_az_windows();
        let mut cmdline = format!("\"{}\"", az_path);
        for a in &args {
            cmdline.push(' ');
            cmdline.push_str(a);
        }
        let result = Command::new("cmd")
            .args(["/c", "start", "Azure CLI Login", "cmd", "/k", &cmdline])
            .spawn();
        return match result {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound =>
                Err("Azure CLI not found. Install it from https://aka.ms/installazurecliwindows then restart the app.".into()),
            Err(e) => Err(format!("Failed to start 'az login': {}", e)),
        };
    }

    #[cfg(not(target_os = "windows"))]
    {
        let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        az_command(&args_ref)
            .spawn()
            .map(|_| ())
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    "Azure CLI not found. Install it from https://aka.ms/installazurecli then restart the app.".to_string()
                } else {
                    format!("Failed to start 'az login': {}", e)
                }
            })
    }
}

/// Signs out of the current az session.
pub fn logout() {
    let _ = az_command(&["logout"]).spawn();
}

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

/// Lists all subscriptions the logged-in account has access to.
/// Returns (id, name) pairs.
pub fn list_subscriptions() -> Result<Vec<(String, String)>, AzError> {
    let out = run(&[
        "account",
        "list",
        "--query",
        "[].{id:id,name:name}",
        "-o",
        "tsv",
    ])?;
    Ok(out
        .lines()
        .filter_map(|line| {
            let mut cols = line.splitn(2, '\t');
            let id = cols.next()?.trim().to_string();
            let name = cols.next().unwrap_or("").trim().to_string();
            if id.is_empty() {
                None
            } else {
                Some((id, name))
            }
        })
        .collect())
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

/// Fetches the Subscription ID that a given Resource Group belongs to.
/// Useful for "self-healing" when a user enters a broken sub ID but has the right RG.
pub fn get_subscription_id_by_group(resource_group: &str) -> Result<String, AzError> {
    let out = run(&[
        "group",
        "show",
        "--name",
        resource_group,
        "--query",
        "id",
        "-o",
        "tsv",
    ])?;
    // The ID looks like: /subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/rg-name
    let parts: Vec<&str> = out.split('/').collect();
    if parts.len() >= 3 && parts[1] == "subscriptions" {
        Ok(parts[2].to_string())
    } else {
        Err(AzError::Other(format!(
            "Could not parse subscription ID from group metadata: {}",
            out
        )))
    }
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

/// Sets the active subscription for subsequent az commands.
pub fn set_subscription(subscription_id: &str) -> Result<(), AzError> {
    run(&["account", "set", "--subscription", subscription_id]).map(|_| ())
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
