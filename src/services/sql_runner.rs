use tiberius::{AuthMethod, Client, Config, EncryptionLevel, Query};
use std::sync::OnceLock;

// Global channel: sql_runner pushes log lines here; the UI drains them into the SQL Dev tab.
static SQL_LOG_TX: OnceLock<tokio::sync::mpsc::UnboundedSender<String>> = OnceLock::new();

pub fn init_log_channel(tx: tokio::sync::mpsc::UnboundedSender<String>) {
    let _ = SQL_LOG_TX.set(tx);
}

fn sql_log(msg: impl Into<String>) {
    if let Some(tx) = SQL_LOG_TX.get() {
        let _ = tx.send(msg.into());
    }
}
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncWriteCompatExt;

use crate::handlers::sql_emulator::{SA_PASSWORD, SQL_PORT};

fn base_config(database: &str) -> Config {
    let mut config = Config::new();
    config.host("127.0.0.1");
    config.port(SQL_PORT);
    config.database(database);
    config.authentication(AuthMethod::sql_server("sa", SA_PASSWORD));
    // SQL Edge uses a self-signed cert — disable TLS negotiation entirely
    config.encryption(EncryptionLevel::NotSupported);
    config
}

/// Execute a SQL statement against the ais-sql-dev container.
/// Async — call with `.await` from an async context or wrap in spawn.
pub async fn run_sql(database: &str, sql: &str) -> Result<String, String> {
    let config = base_config(database);

    let tcp = TcpStream::connect(config.get_addr())
        .await
        .map_err(|e| format!("Cannot connect to SQL Dev (localhost:{SQL_PORT}): {e}"))?;
    tcp.set_nodelay(true).ok();

    let mut client = Client::connect(config, tcp.compat_write())
        .await
        .map_err(|e| format!("SQL login failed: {e}"))?;

    sql_log(format!("→ [{database}]"));

    // Split on both GO (on its own line) and ; so that DDL statements like
    // CREATE SCHEMA, CREATE TABLE, ALTER DATABASE each run as their own batch.
    // SQL Server requires certain DDL to be the only statement in a batch.
    let batches: Vec<String> = sql
        .split('\n')
        .collect::<Vec<_>>()
        // First split on GO lines
        .split(|l: &&str| l.trim().eq_ignore_ascii_case("GO"))
        .flat_map(|chunk| {
            // Then split each chunk on semicolons
            chunk.join("\n")
                .split(';')
                .map(|s| s.trim().to_string())
                .collect::<Vec<_>>()
        })
        .filter(|s| !s.is_empty())
        .collect();

    let mut output = String::new();

    for raw_batch in &batches {
        let batch = make_idempotent(raw_batch);
        sql_log(format!("  ▸ {}", batch.trim().lines().next().unwrap_or("").chars().take(80).collect::<String>()));
        let trimmed = batch.trim_start().to_ascii_uppercase();

        // ALTER DATABASE must run from master — reconnect for those batches.
        // Check contains() not starts_with() so IF NOT EXISTS ... ALTER DATABASE is caught too.
        let exec_result = if trimmed.contains("ALTER DATABASE") {
            let master_config = base_config("master");
            let tcp = TcpStream::connect(master_config.get_addr())
                .await
                .map_err(|e| format!("Cannot connect to master: {e}"))?;
            tcp.set_nodelay(true).ok();
            let mut master = Client::connect(master_config, tcp.compat_write())
                .await
                .map_err(|e| format!("master login failed: {e}"))?;
            master.execute(batch.as_str(), &[]).await
                .map(|r| r.total())
                .map_err(|e| e.to_string())
        } else if trimmed.starts_with("SELECT") || trimmed.starts_with("WITH") {
            // SELECT — use query() so rows are returned
            let query   = Query::new(batch.as_str());
            let results = query.query(&mut client)
                .await
                .map_err(|e| format!("SQL error: {e}"))?;
            let rows = results.into_results()
                .await
                .map_err(|e| format!("Result error: {e}"))?;
            for result_set in rows {
                for row in result_set {
                    let cols: Vec<String> = (0..row.len())
                        .map(|i| { let v: Option<&str> = row.get(i); v.unwrap_or("NULL").to_string() })
                        .collect();
                    output.push_str(&cols.join(" | "));
                    output.push('\n');
                }
            }
            continue;
        } else {
            client.execute(batch.as_str(), &[]).await
                .map(|r| r.total())
                .map_err(|e| e.to_string())
        };

        match exec_result {
            Ok(affected) => {
                if affected > 0 {
                    output.push_str(&format!("({affected} row(s) affected)\n"));
                }
            }
            Err(msg) => {
                if is_already_exists(&msg) {
                    sql_log(format!("  ↷ already exists — skipped"));
                    output.push_str("(already exists — skipped)\n");
                } else if msg.contains("2760") || msg.contains("does not exist or you do not have permission") {
                    // Schema not found — extract schema name, auto-create it, retry once.
                    if let Some(schema) = extract_schema_from_batch(&batch) {
                        let create = format!("IF SCHEMA_ID('{schema}') IS NULL EXEC('CREATE SCHEMA [{schema}]')");
                        let _ = client.execute(create.as_str(), &[]).await;
                        // Retry the original batch
                        match client.execute(batch.as_str(), &[]).await {
                            Ok(r) => {
                                output.push_str(&format!("(auto-created schema [{schema}], then OK)\n"));
                                let affected = r.total();
                                if affected > 0 { output.push_str(&format!("({affected} row(s) affected)\n")); }
                            }
                            Err(e2) => {
                                if is_already_exists(&e2.to_string()) {
                                    output.push_str("(already exists — skipped)\n");
                                } else {
                                    return Err(format!("SQL error: {e2}"));
                                }
                            }
                        }
                    } else {
                        return Err(format!("SQL error: {msg}"));
                    }
                } else {
                    return Err(format!("SQL error: {msg}"));
                }
            }
        }
    }

    if output.is_empty() {
        sql_log("  ✓ OK".to_string());
        output = "Command(s) completed successfully.".into();
    } else {
        sql_log(format!("  ✓ {}", output.trim().lines().next().unwrap_or("")));
    }

    Ok(output)
}

/// Returns true for SQL Server errors that mean "object already exists".
/// These are safe to ignore — running the same DDL twice is idempotent.
fn is_already_exists(msg: &str) -> bool {
    // 2714 = "There is already an object named '...' in the database."
    // 1801 = "Database '...' already exists."
    // NOTE: 2760 ("schema not found") is intentionally NOT here — it means
    // a real error (CREATE TABLE in a non-existent schema) and must surface.
    msg.contains("already exists")
        || msg.contains("2714")
        || msg.contains("1801")
}

/// Extract the schema name from a batch that references [schema].[object].
/// Used to auto-create missing schemas on error 2760.
fn extract_schema_from_batch(batch: &str) -> Option<String> {
    // Match [schema].[anything] or schema.anything
    let re = regex::Regex::new(r"\[([^\]]+)\]\.\[").ok()?;
    if let Some(cap) = re.captures(batch) {
        return Some(cap[1].to_string());
    }
    // Unbracketed: schema.table
    let re2 = regex::Regex::new(r"\b(\w+)\.\w+").ok()?;
    re2.captures(batch).map(|c| c[1].to_string())
}

/// Rewrite `CREATE SCHEMA foo` into an idempotent inline form using EXEC so it
/// can coexist with other statements in a batch (SQL Server restriction).
fn make_idempotent(batch: &str) -> String {
    let upper = batch.trim_start().to_ascii_uppercase();
    if upper.starts_with("CREATE SCHEMA ") {
        // Extract the schema name (first token after CREATE SCHEMA)
        let rest = batch.trim_start()["CREATE SCHEMA ".len()..].trim();
        let name = rest.split_whitespace().next().unwrap_or(rest);
        // Strip brackets if present
        let clean = name.trim_matches(|c| c == '[' || c == ']');
        return format!("IF SCHEMA_ID('{clean}') IS NULL EXEC('CREATE SCHEMA [{clean}]')");
    }
    batch.to_string()
}

/// List all user databases (excludes system databases).
pub async fn list_databases() -> Result<Vec<String>, String> {
    let sql = "SELECT name FROM sys.databases \
               WHERE name NOT IN ('master','tempdb','model','msdb') \
               ORDER BY name";
    let config = base_config("master");
    let tcp = TcpStream::connect(config.get_addr()).await
        .map_err(|e| format!("Cannot connect: {e}"))?;
    tcp.set_nodelay(true).ok();
    let mut client = Client::connect(config, tcp.compat_write()).await
        .map_err(|e| format!("Login failed: {e}"))?;
    let rows = Query::new(sql).query(&mut client).await
        .map_err(|e| e.to_string())?
        .into_results().await
        .map_err(|e| e.to_string())?;
    Ok(rows.into_iter().flatten()
        .filter_map(|r| r.get::<&str, _>(0).map(|s| s.to_string()))
        .collect())
}

/// List all user tables in a database as (schema, table, row_count) triples.
pub async fn list_tables(database: &str) -> Result<Vec<(String, String, u64)>, String> {
    let sql = "SELECT s.name, t.name, COALESCE(SUM(p.rows), 0) \
               FROM sys.tables t \
               JOIN sys.schemas s ON t.schema_id = s.schema_id \
               LEFT JOIN sys.partitions p ON t.object_id = p.object_id AND p.index_id < 2 \
               GROUP BY s.name, t.name \
               ORDER BY s.name, t.name";
    let config = base_config(database);
    let tcp = TcpStream::connect(config.get_addr()).await
        .map_err(|e| format!("Cannot connect: {e}"))?;
    tcp.set_nodelay(true).ok();
    let mut client = Client::connect(config, tcp.compat_write()).await
        .map_err(|e| format!("Login failed: {e}"))?;
    let rows = Query::new(sql).query(&mut client).await
        .map_err(|e| e.to_string())?
        .into_results().await
        .map_err(|e| e.to_string())?;
    Ok(rows.into_iter().flatten()
        .filter_map(|r| {
            let schema = r.get::<&str, _>(0)?.to_string();
            let table  = r.get::<&str, _>(1)?.to_string();
            let count: i64 = r.get::<i64, _>(2).unwrap_or(0);
            Some((schema, table, count.max(0) as u64))
        })
        .collect())
}

/// Drop a database entirely.
pub async fn drop_database(name: &str) -> Result<(), String> {
    // Must close existing connections first — set SINGLE_USER to force disconnect
    let sql = format!(
        "ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; \
         DROP DATABASE [{name}];"
    );
    run_sql("master", &sql).await.map(|_| ())
}

/// Truncate a table (remove all rows, keep structure).
pub async fn truncate_table(database: &str, schema: &str, table: &str) -> Result<(), String> {
    run_sql(database, &format!("TRUNCATE TABLE [{schema}].[{table}]")).await.map(|_| ())
}

/// Drop a table entirely.
pub async fn drop_table(database: &str, schema: &str, table: &str) -> Result<(), String> {
    run_sql(database, &format!("DROP TABLE [{schema}].[{table}]")).await.map(|_| ())
}

/// Create a database if it doesn't already exist.
pub async fn create_database(name: &str) -> Result<String, String> {
    // Single-line IF with no semicolons — avoids being split by the batch splitter.
    // CREATE DATABASE must be the only statement in its batch (SQL Server rule).
    let sql = format!("IF DB_ID(N'{name}') IS NULL CREATE DATABASE [{name}]");
    match run_sql("master", &sql).await {
        Ok(_)  => Ok(format!("✅ Database [{name}] ready.")),
        Err(e) => Err(e),
    }
}
