use std::collections::HashSet;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum SqlAuthType {
    ManagedIdentity,
    ConnectionString,
    Unknown,
}

impl SqlAuthType {
    pub fn label(&self) -> &'static str {
        match self {
            SqlAuthType::ManagedIdentity  => "MSI",
            SqlAuthType::ConnectionString => "ConnStr",
            SqlAuthType::Unknown          => "?",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SqlConnection {
    pub name:             String,
    pub auth_type:        SqlAuthType,
    /// Setting key for serverName (MSI)
    pub server_key:       Option<String>,
    /// Setting key for databaseName (MSI)
    pub db_key:           Option<String>,
    /// Setting key for connectionString (ConnStr)
    pub conn_str_key:     Option<String>,
    /// Resolved values from local.settings.json
    pub resolved_server:  String,
    pub resolved_db:      String,
    pub resolved_conn_str: String,
}

impl SqlConnection {
}

#[derive(Debug, Clone, PartialEq)]
pub struct TestResult {
    pub reachable:  bool,
    pub latency_ms: Option<u64>,
    pub error:      Option<String>,
}

fn extract_appsetting_key(val: &str) -> Option<String> {
    val.strip_prefix("@appsetting('")
        .and_then(|s| s.strip_suffix("')"))
        .map(|s| s.to_string())
}

/// Parse the server hostname from a SQL Server connection string.
/// Handles `Server=`, `Data Source=`, with optional port suffix `host,1433`.
pub fn parse_server_from_conn_str(conn_str: &str) -> Option<String> {
    for part in conn_str.split(';') {
        let part = part.trim();
        let val = ["Server=", "server=", "Data Source=", "data source="]
            .iter()
            .find_map(|prefix| part.strip_prefix(prefix));
        if let Some(val) = val {
            let host = val.split(',').next().unwrap_or(val).trim();
            if !host.is_empty() {
                return Some(host.to_string());
            }
        }
    }
    None
}

/// Load all SQL service-provider connections and resolve their settings.
pub fn load_sql_connections(logic_apps_dir: &str) -> Vec<SqlConnection> {
    let dir = std::path::Path::new(logic_apps_dir);

    let conn_text = match std::fs::read_to_string(dir.join("connections.json")) {
        Ok(t) => t,
        Err(_) => return vec![],
    };
    let conn_json: serde_json::Value = match serde_json::from_str(&conn_text) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let settings_map: serde_json::Map<String, serde_json::Value> =
        std::fs::read_to_string(dir.join("local.settings.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v["Values"].as_object().cloned())
            .unwrap_or_default();

    let providers = match conn_json["serviceProviderConnections"].as_object() {
        Some(m) => m,
        None => return vec![],
    };

    let mut result = Vec::new();

    for (name, conn) in providers {
        if conn["serviceProvider"]["id"].as_str() != Some("/serviceProviders/sql") {
            continue;
        }

        let param_set = conn["parameterSetName"].as_str().unwrap_or("");
        let params    = conn["parameterValues"].as_object();

        let get_key = |field: &str| -> Option<String> {
            params
                .and_then(|p| p.get(field))
                .and_then(|v| v.as_str())
                .and_then(|s| extract_appsetting_key(s))
        };

        let (auth_type, server_key, db_key, conn_str_key) = match param_set {
            "ManagedServiceIdentity" => (
                SqlAuthType::ManagedIdentity,
                get_key("serverName"),
                get_key("databaseName"),
                None,
            ),
            "connectionString" => (
                SqlAuthType::ConnectionString,
                None,
                None,
                get_key("connectionString"),
            ),
            _ => (SqlAuthType::Unknown, None, None, None),
        };

        let resolve = |key: &Option<String>| -> String {
            key.as_deref()
                .and_then(|k| settings_map.get(k))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };

        result.push(SqlConnection {
            name:             name.clone(),
            resolved_server:  resolve(&server_key),
            resolved_db:      resolve(&db_key),
            resolved_conn_str: resolve(&conn_str_key),
            auth_type,
            server_key,
            db_key,
            conn_str_key,
        });
    }

    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}

/// Returns the set of workflow names (folder names) that reference any SQL connection.
pub fn detect_sql_workflows(logic_apps_dir: &str) -> HashSet<String> {
    let dir = std::path::Path::new(logic_apps_dir);

    let conn_text = match std::fs::read_to_string(dir.join("connections.json")) {
        Ok(t) => t,
        Err(_) => return HashSet::new(),
    };
    let conn_json: serde_json::Value = match serde_json::from_str(&conn_text) {
        Ok(v) => v,
        Err(_) => return HashSet::new(),
    };

    let sql_names: Vec<String> = conn_json["serviceProviderConnections"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(_, v)| {
                    v["serviceProvider"]["id"].as_str() == Some("/serviceProviders/sql")
                })
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default();

    if sql_names.is_empty() {
        return HashSet::new();
    }

    let mut result = HashSet::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let wf_path = entry.path().join("workflow.json");
            if let Ok(text) = std::fs::read_to_string(&wf_path) {
                if sql_names.iter().any(|n| text.contains(n.as_str())) {
                    if let Some(name) = entry.file_name().to_str() {
                        result.insert(name.to_string());
                    }
                }
            }
        }
    }

    result
}

/// Attempt a TCP connection to `host:port` with a 5-second timeout.
/// Runs synchronously — call inside `spawn_blocking`.
pub fn test_tcp(host: &str, port: u16) -> TestResult {
    if host.is_empty() {
        return TestResult {
            reachable:  false,
            latency_ms: None,
            error:      Some("No host configured".into()),
        };
    }
    let addr_str = format!("{}:{}", host, port);
    let start    = Instant::now();

    let mut addrs = match addr_str.to_socket_addrs() {
        Ok(a)  => a,
        Err(e) => return TestResult {
            reachable:  false,
            latency_ms: None,
            error:      Some(format!("DNS: {}", e)),
        },
    };

    match addrs.next() {
        Some(addr) => match TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
            Ok(_) => TestResult {
                reachable:  true,
                latency_ms: Some(start.elapsed().as_millis() as u64),
                error:      None,
            },
            Err(e) => TestResult {
                reachable:  false,
                latency_ms: None,
                error:      Some(e.to_string()),
            },
        },
        None => TestResult {
            reachable:  false,
            latency_ms: None,
            error:      Some("No addresses resolved".into()),
        },
    }
}
