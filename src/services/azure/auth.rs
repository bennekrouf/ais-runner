//! Signing in, and choosing which subscription subsequent calls run against.
//!
//! Split from the query modules because this is the half that is *interactive*:
//! [`launch_az_login`] drives a Terminal window and [`open_login`] opens a
//! browser. A headless consumer wants the queries without linking anything that
//! can pop a window.

#[cfg(target_os = "windows")]
use std::process::Command;

#[cfg(target_os = "windows")]
use super::cli::resolve_az_windows;
use super::cli::{az_command, run, AzError};

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

/// Sets the active subscription for subsequent az commands.
pub fn set_subscription(subscription_id: &str) -> Result<(), AzError> {
    run(&["account", "set", "--subscription", subscription_id]).map(|_| ())
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
