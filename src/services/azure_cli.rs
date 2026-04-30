use std::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub enum AzError {
    NotLoggedIn,
    Other(String),
}


fn run(args: &[&str]) -> Result<String, AzError> {
    let out = Command::new("az")
        .args(args)
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

/// Opens an interactive az login in a new terminal window (macOS).
pub fn open_login(tenant: &str) {
    let script = format!(
        "tell application \"Terminal\" to do script \"az login --tenant {} --scope 'https://management.core.windows.net//.default'\"",
        tenant
    );
    let _ = Command::new("osascript").args(["-e", &script]).spawn();
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

