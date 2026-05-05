use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct SftpConnection {
    pub connection_name: String,  // key in connections.json, e.g. "sftpOryx"
    pub display_name:    String,
    pub host_key:        String,  // appsetting key for sshHostAddress
    pub user_key:        String,  // appsetting key for username
    pub pass_key:        String,  // appsetting key for password
    pub host:            String,  // resolved value (empty if not set)
    pub user:            String,
    pub configured:      bool,    // host and user are non-empty
}

#[derive(Debug, Clone, PartialEq)]
pub enum SftpTestResult {
    Ok,
    AuthFailed,
    Unreachable(String),
    NotConfigured,
}

/// Scans connections.json for SFTP serviceProviderConnections and resolves
/// their appsetting keys against local.settings.json.
pub fn detect_sftp_connections(logic_apps_dir: &str) -> Vec<SftpConnection> {
    let dir = crate::services::workflows::resolve_logic_apps_dir(logic_apps_dir);

    let conn_text = match std::fs::read_to_string(dir.join("connections.json")) {
        Ok(t) => t,
        Err(_) => return vec![],
    };
    let conn_json: Value = match serde_json::from_str(&conn_text) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let settings_text = std::fs::read_to_string(dir.join("local.settings.json")).unwrap_or_default();
    let settings: Value = serde_json::from_str(&settings_text).unwrap_or_default();

    let resolve = |raw: &str| -> (String, String) {
        // raw is either "@appsetting('key')" or a literal value
        let key = raw
            .strip_prefix("@appsetting('")
            .and_then(|s| s.strip_suffix("')"))
            .unwrap_or(raw)
            .to_string();
        let val = settings["Values"][&key].as_str().unwrap_or("").to_string();
        (key, val)
    };

    let mut result = Vec::new();
    if let Some(providers) = conn_json["serviceProviderConnections"].as_object() {
        for (name, conn) in providers {
            if conn["serviceProvider"]["id"].as_str() != Some("/serviceProviders/Sftp") {
                continue;
            }
            let pv = &conn["parameterValues"];
            let display = conn["displayName"].as_str().unwrap_or(name).to_string();

            let (host_key, host) = pv["sshHostAddress"].as_str()
                .map(resolve)
                .unwrap_or_default();
            let (user_key, user) = pv["username"].as_str()
                .map(resolve)
                .unwrap_or_default();
            let (pass_key, _) = pv["password"].as_str()
                .map(resolve)
                .unwrap_or_default();

            let configured = !host.is_empty() && !user.is_empty();

            result.push(SftpConnection {
                connection_name: name.clone(),
                display_name: display,
                host_key,
                user_key,
                pass_key,
                host,
                user,
                configured,
            });
        }
    }
    result.sort_by(|a, b| a.connection_name.cmp(&b.connection_name));
    result
}

/// Attempts an SSH connection with a 4-second timeout and returns immediately on failure.
/// Uses `ssh` with BatchMode (no interactive prompts) — if auth fails we get AuthFailed,
/// if host is unreachable we get Unreachable.
pub fn test_sftp_connection(host: &str, user: &str) -> SftpTestResult {
    if host.is_empty() || user.is_empty() {
        return SftpTestResult::NotConfigured;
    }

    let output = std::process::Command::new("ssh")
        .args([
            "-o", "BatchMode=yes",
            "-o", "ConnectTimeout=4",
            "-o", "StrictHostKeyChecking=no",
            "-o", "PasswordAuthentication=no",
            "-p", "22",
            &format!("{}@{}", user, host),
            "exit",
        ])
        .output();

    match output {
        Err(e) => SftpTestResult::Unreachable(format!("ssh not found: {}", e)),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
            if out.status.success() {
                SftpTestResult::Ok
            } else if stderr.contains("permission denied")
                || stderr.contains("publickey")
                || stderr.contains("authentication failed")
            {
                // Auth failed but host was reachable — password auth will likely work at runtime
                SftpTestResult::AuthFailed
            } else if stderr.contains("connection refused")
                || stderr.contains("no route")
                || stderr.contains("timed out")
                || stderr.contains("could not resolve")
            {
                SftpTestResult::Unreachable(stderr.trim().to_string())
            } else {
                // Unknown exit — treat as reachable (sftp servers often close ssh immediately)
                SftpTestResult::Ok
            }
        }
    }
}
