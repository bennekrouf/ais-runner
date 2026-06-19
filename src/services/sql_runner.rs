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

    // Split into batches the way sqlcmd does — respecting string literals,
    // bracketed identifiers, and `--` / `/* */` comments. Each entry carries
    // the source-line on which the batch started so we can include it in
    // error messages (item #3 in the bug report).
    let batches = split_sql_batches(sql);

    let mut output = String::new();

    for SqlBatch { text: raw_batch, start_line } in &batches {
        let batch = make_idempotent(raw_batch);
        sql_log(format!("  ▸ (L{}) {}", start_line,
            batch.trim().lines().next().unwrap_or("").chars().take(80).collect::<String>()));
        let trimmed = batch.trim_start().to_ascii_uppercase();

        // Use `simple_query` for every batch — it sends the SQL as a real
        // TDS *batch* (the same wire shape `sqlcmd` uses), NOT through
        // `sp_executesql` like `execute()` does. This matters for three
        // distinct cases that the previous RPC-based path silently broke:
        //
        // 1. **CREATE PROCEDURE / VIEW / TRIGGER / FUNCTION** — these
        //    cannot run inside `sp_executesql`'s dynamic batch on some SQL
        //    Server versions; the parser surfaces a nonsensical
        //    "Incorrect syntax near 'PROCEDURE'" instead of the real
        //    "cannot execute in dynamic SQL" error.
        // 2. **`USE <db>`** — when wrapped in `sp_executesql`, the database
        //    change only lasts for the wrapper call's lifetime; the
        //    connection's default DB never actually moves, so all
        //    subsequent batches in our loop run against the wrong DB.
        // 3. **Multi-statement batches** — the RPC path expects a single
        //    statement; multi-statement procedure bodies got rewritten.
        //
        // For SELECT we still collect rows the same way — `simple_query`
        // returns a `QueryStream` with the same `into_results()` shape.
        //
        // ALTER DATABASE keeps its master-reconnect path because we need to
        // switch logins, not just batches.
        let exec_result: Result<u64, String> = if trimmed.contains("ALTER DATABASE") {
            let master_config = base_config("master");
            let tcp = TcpStream::connect(master_config.get_addr())
                .await
                .map_err(|e| format!("Cannot connect to master: {e}"))?;
            tcp.set_nodelay(true).ok();
            let mut master = Client::connect(master_config, tcp.compat_write())
                .await
                .map_err(|e| format!("master login failed: {e}"))?;
            run_batch_via_simple_query(&mut master, &batch, &mut output).await
        } else if trimmed.starts_with("SELECT") || trimmed.starts_with("WITH") {
            // SELECT — same path as everything else (simple_query also returns rows),
            // but we report 0 affected rows since SELECT doesn't update.
            match client.simple_query(batch.as_str()).await {
                Ok(stream) => match stream.into_results().await {
                    Ok(rows) => {
                        for result_set in rows {
                            for row in result_set {
                                let cols: Vec<String> = (0..row.len())
                                    .map(|i| cell_to_string(&row, i))
                                    .collect();
                                output.push_str(&cols.join(" | "));
                                output.push('\n');
                            }
                        }
                        continue;
                    }
                    Err(e) => Err(format!("Result error in batch starting at line {start_line}: {e}")),
                },
                Err(e) => Err(format!("SQL error in batch starting at line {start_line}: {e}")),
            }
        } else {
            run_batch_via_simple_query(&mut client, &batch, &mut output).await
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
                    // Both calls go through `simple_query` to stay consistent with
                    // the main exec path (CREATE PROCEDURE / USE / multi-statement
                    // all need real SQL batches, not sp_executesql wrapping).
                    if let Some(schema) = extract_schema_from_batch(&batch) {
                        let create = format!("IF SCHEMA_ID('{schema}') IS NULL EXEC('CREATE SCHEMA [{schema}]')");
                        if let Ok(s) = client.simple_query(create.as_str()).await {
                            let _ = s.into_results().await;
                        }
                        // Retry the original batch
                        match run_batch_via_simple_query(&mut client, &batch, &mut output).await {
                            Ok(affected) => {
                                output.push_str(&format!("(auto-created schema [{schema}], then OK)\n"));
                                if affected > 0 { output.push_str(&format!("({affected} row(s) affected)\n")); }
                            }
                            Err(e2) => {
                                if is_already_exists(&e2) {
                                    output.push_str("(already exists — skipped)\n");
                                } else {
                                    return Err(format!("SQL error in batch starting at line {start_line}: {e2}"));
                                }
                            }
                        }
                    } else {
                        return Err(format!("SQL error in batch starting at line {start_line}: {msg}"));
                    }
                } else {
                    return Err(format!("SQL error in batch starting at line {start_line}: {msg}"));
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

/// Send a single batch via `simple_query` and accumulate any rows it
/// emitted (some non-SELECT batches still return result sets — `MERGE …
/// OUTPUT`, `INSERT … OUTPUT`, etc.) into `output`. Returns the total row
/// count "affected" by the batch — for batches with no result sets this is
/// 0 by definition, which is consistent with the caller's "(N row(s)
/// affected)" formatting only firing for `> 0`.
///
/// Why `simple_query` and not `execute`: see the long comment in `run_sql`
/// above the dispatch — `execute` wraps the SQL in `sp_executesql`, which
/// silently breaks `CREATE PROCEDURE`, `USE <db>`, and multi-statement
/// batches.
// Concrete type alias — both call sites build a tiberius client over a
// `tokio::TcpStream` wrapped with `tokio_util::compat::Compat`. Using the
// concrete type sidesteps the generic `futures::io::AsyncRead/Write`
// bounds that `Client::simple_query` requires (we don't depend on the
// `futures` crate directly).
type TdsClient = tiberius::Client<tokio_util::compat::Compat<TcpStream>>;

async fn run_batch_via_simple_query(
    client: &mut TdsClient,
    batch:  &str,
    output: &mut String,
) -> Result<u64, String>
{
    let stream = client.simple_query(batch).await.map_err(|e| e.to_string())?;
    let result_sets = stream.into_results().await.map_err(|e| e.to_string())?;
    let mut total: u64 = 0;
    for result_set in result_sets {
        for row in result_set {
            total += 1;
            let cols: Vec<String> = (0..row.len())
                .map(|i| cell_to_string(&row, i))
                .collect();
            output.push_str(&cols.join(" | "));
            output.push('\n');
        }
    }
    Ok(total)
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

/// Stringify a single cell of a `tiberius` row without ever panicking.
///
/// `Row::get::<T, _>(i)` PANICS when the column's runtime type doesn't match
/// `T` — so the previous code, which assumed every column was `&str`, took
/// the whole app down the moment the user ran `SELECT 1` (an `INT`).
///
/// This helper tries each common T-SQL type via the non-panicking
/// `try_get` in priority order: text first (so `VARCHAR`-y columns stay
/// readable), then integers, floats, bool, and finally raw byte buffers.
/// SQL NULL is reported once we've confirmed it's actually null vs. a type
/// we don't yet decode (which is reported as `<unsupported type>` so the
/// user knows the row arrived but we couldn't render that cell).
fn cell_to_string(row: &tiberius::Row, i: usize) -> String {
    // Strings — covers VARCHAR, NVARCHAR, CHAR, NCHAR, TEXT, NTEXT.
    if let Ok(Some(s)) = row.try_get::<&str, _>(i) { return s.to_string(); }

    // Integer family. Order matters only for "compact display" — the type
    // tag is exact so we'll only match the right arm.
    if let Ok(Some(v)) = row.try_get::<i64, _>(i) { return v.to_string(); }
    if let Ok(Some(v)) = row.try_get::<i32, _>(i) { return v.to_string(); }
    if let Ok(Some(v)) = row.try_get::<i16, _>(i) { return v.to_string(); }
    if let Ok(Some(v)) = row.try_get::<u8,  _>(i) { return v.to_string(); }

    // Floats / decimals — tiberius decodes DECIMAL/NUMERIC as f64 by default.
    if let Ok(Some(v)) = row.try_get::<f64, _>(i) { return v.to_string(); }
    if let Ok(Some(v)) = row.try_get::<f32, _>(i) { return v.to_string(); }

    // BIT.
    if let Ok(Some(v)) = row.try_get::<bool, _>(i) { return (if v { "1" } else { "0" }).to_string(); }

    // VARBINARY / BINARY / IMAGE — render as lowercase hex so the user can
    // inspect short blobs in the result panel.
    if let Ok(Some(v)) = row.try_get::<&[u8], _>(i) {
        return v.iter().map(|b| format!("{b:02x}")).collect();
    }

    // We've ruled out every type we know how to format. The only remaining
    // explanations are SQL NULL or a runtime type we don't decode yet. Use
    // a string-typed try_get one more time and inspect its Ok(None) state
    // to differentiate.
    match row.try_get::<&str, _>(i) {
        Ok(None) => "NULL".to_string(),
        _        => "<unsupported type>".to_string(),
    }
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

/// Check whether a stored procedure exists in `database`.
/// `sproc` may be unqualified (`"Foo"`) or qualified (`"dbo.Foo"`).
pub async fn sproc_exists(database: &str, sproc: &str) -> Result<bool, String> {
    let cleaned = sproc.replace(['[', ']'], "");
    let qualified = if cleaned.contains('.') {
        cleaned
    } else {
        format!("dbo.{cleaned}")
    };
    let escaped = qualified.replace('\'', "''");
    let sql = format!("SELECT CAST(CASE WHEN OBJECT_ID(N'{escaped}', N'P') IS NULL THEN 0 ELSE 1 END AS INT)");

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
    let exists = rows.into_iter().flatten().next()
        .and_then(|r| r.get::<i32, _>(0))
        .map(|n| n != 0)
        .unwrap_or(false);
    Ok(exists)
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

/// Detect the path to `sqlcmd` inside the running `ais-sql-dev` container.
///
/// The Microsoft `mcr.microsoft.com/azure-sql-edge:latest` image ships
/// `sqlcmd` at one of two paths depending on which version of mssql-tools
/// was baked in:
///
/// * **Legacy**: `/opt/mssql-tools/bin/sqlcmd` — accepts insecure TLS by
///   default, no `-C` flag needed.
/// * **Modern (tools18)**: `/opt/mssql-tools18/bin/sqlcmd` — requires `-C`
///   to skip self-signed cert validation against SQL Edge.
///
/// Returns `(path, needs_dash_c)`. `needs_dash_c` lets the caller build a
/// command line that works against either flavour without the user having to
/// know which they got.
///
/// `None` when neither tool is present (container not running, or image
/// changed shape).
pub async fn detect_sqlcmd_path() -> Option<(String, bool)> {
    use tokio::process::Command;
    // tools18 is the newer image — probe it first so we prefer it when both
    // exist (unlikely but harmless).
    for (path, needs_c) in &[
        ("/opt/mssql-tools18/bin/sqlcmd", true),
        ("/opt/mssql-tools/bin/sqlcmd",   false),
    ] {
        let docker = crate::services::runtime_manager::resolve_tool("docker");
        let out = Command::new(&docker)
            .args(["exec", crate::handlers::sql_emulator::CONTAINER_NAME,
                   "test", "-x", path])
            .output()
            .await;
        if let Ok(out) = out {
            if out.status.success() {
                return Some(((*path).to_string(), *needs_c));
            }
        }
    }
    None
}

/// Build the docker-exec command line that opens an interactive `sqlcmd`
/// session against ais-sql-dev. The user copies this to their own terminal
/// — spawning an interactive PTY from inside the GUI app would be a
/// cross-platform mess (TTY allocation, signal forwarding, theme handling).
/// Returns the literal string to display + copy.
pub fn shell_command_line(sqlcmd_path: &str, needs_dash_c: bool) -> String {
    let dash_c = if needs_dash_c { " -C" } else { "" };
    format!(
        "docker exec -it {} {} -S localhost -U sa -P '{}'{} -d master",
        crate::handlers::sql_emulator::CONTAINER_NAME,
        sqlcmd_path,
        crate::handlers::sql_emulator::SA_PASSWORD,
        dash_c,
    )
}

/// One executable T-SQL batch with the source line it started on.
/// The `start_line` is 1-based and points at the first non-empty line of the
/// batch in the *original* script, so error messages can guide the user back
/// to the offending statement (vs. a line inside an opaque split chunk).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlBatch {
    pub text:       String,
    pub start_line: u32,
}

/// Split a T-SQL script into batches the way `sqlcmd` does:
///
/// * Lines whose trimmed content is `GO` (optionally followed by an integer
///   count, case-insensitive) terminate the current batch.
/// * **`;` is NOT a batch separator** — it's a statement separator *within*
///   a batch. SQL Server happily executes multi-statement batches, and
///   procedure bodies (`CREATE PROCEDURE … AS BEGIN SET NOCOUNT ON; MERGE …;
///   END`) routinely use `;` between internal statements. Treating `;` as a
///   batch terminator would slice procedure bodies in half and produce
///   "Incorrect syntax near 'PROCEDURE'" because what remains after the cut
///   is no longer a valid CREATE PROCEDURE invocation.
/// * String literals (`'…'`, `N'…'`), bracketed identifiers (`[…]`), ANSI
///   quoted identifiers (`"…"`), line comments (`-- …\n`) and block comments
///   (`/* … */`) are still tracked so a `GO` *inside* any of them isn't
///   accidentally treated as a separator (rare but possible in dynamic SQL).
/// * CRLF line endings are tolerated — a `GO\r\n` is still recognised.
///
/// Empty batches are dropped.
pub fn split_sql_batches(sql: &str) -> Vec<SqlBatch> {
    #[derive(Copy, Clone, PartialEq, Eq)]
    enum State {
        Code,
        // Inside a '…' string literal (or N'…' — handled by entering with `'`).
        SingleString,
        // Inside a "…" ANSI-quoted identifier.
        DoubleString,
        // Inside a [..] bracketed identifier.
        Bracket,
        // Inside a -- line comment, until newline.
        LineComment,
        // Inside a /* … */ block comment, may span multiple lines.
        BlockComment,
    }

    let bytes = sql.as_bytes();
    let mut out: Vec<SqlBatch> = Vec::new();
    let mut buf = String::new();
    let mut state = State::Code;
    let mut current_line: u32 = 1;
    let mut batch_start_line: u32 = 1;
    let mut buf_started = false;

    let push_batch = |buf: &mut String, start: u32, out: &mut Vec<SqlBatch>| {
        let text = buf.trim().to_string();
        if !text.is_empty() {
            out.push(SqlBatch { text, start_line: start });
        }
        buf.clear();
    };

    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;

        // Track source line numbers and lazily anchor batch-start to the first
        // non-whitespace char of the next batch.
        if !buf_started && !c.is_whitespace() && state == State::Code {
            batch_start_line = current_line;
            buf_started = true;
        }

        match state {
            State::Code => {
                // -- line comment
                if c == '-' && bytes.get(i + 1).copied() == Some(b'-') {
                    state = State::LineComment;
                    buf.push_str("--");
                    i += 2;
                    continue;
                }
                // /* block comment
                if c == '/' && bytes.get(i + 1).copied() == Some(b'*') {
                    state = State::BlockComment;
                    buf.push_str("/*");
                    i += 2;
                    continue;
                }
                // ' string literal — also handles N'…' (the N is just code).
                if c == '\'' {
                    state = State::SingleString;
                    buf.push(c);
                    i += 1;
                    continue;
                }
                // " ANSI identifier
                if c == '"' {
                    state = State::DoubleString;
                    buf.push(c);
                    i += 1;
                    continue;
                }
                // [ bracketed identifier
                if c == '[' {
                    state = State::Bracket;
                    buf.push(c);
                    i += 1;
                    continue;
                }
                // Note: `;` is NOT a batch terminator — see the module
                // doc-comment on `split_sql_batches` for why. It's a
                // statement separator within a batch and is left in the
                // buffer untouched.
                // Detect a GO line: peek backwards from `i` to the previous newline,
                // then forward from there to see if the trimmed line is `GO` (optional N).
                // We only need to run this check at newline boundaries — at the start
                // of a fresh line — so do it when the previous char was `\n` (LF or the
                // trailing `\n` of a CRLF sequence; we track only `\n` so CR is
                // implicitly accepted).
                if i == 0 || bytes[i - 1] == b'\n' {
                    if let Some(advanced) = match_go_line(bytes, i) {
                        // End the current batch, skip the GO line entirely.
                        push_batch(&mut buf, batch_start_line, &mut out);
                        buf_started = false;
                        // Advance past the GO line including its newline.
                        for j in i..advanced {
                            if bytes[j] == b'\n' { current_line += 1; }
                        }
                        i = advanced;
                        continue;
                    }
                }
                buf.push(c);
            }
            State::SingleString => {
                buf.push(c);
                if c == '\'' {
                    // '' escapes inside a single-quoted string — stay in string.
                    if bytes.get(i + 1).copied() == Some(b'\'') {
                        buf.push('\'');
                        i += 2;
                        continue;
                    }
                    state = State::Code;
                }
            }
            State::DoubleString => {
                buf.push(c);
                if c == '"' {
                    if bytes.get(i + 1).copied() == Some(b'"') {
                        buf.push('"');
                        i += 2;
                        continue;
                    }
                    state = State::Code;
                }
            }
            State::Bracket => {
                buf.push(c);
                if c == ']' {
                    if bytes.get(i + 1).copied() == Some(b']') {
                        buf.push(']');
                        i += 2;
                        continue;
                    }
                    state = State::Code;
                }
            }
            State::LineComment => {
                buf.push(c);
                if c == '\n' { state = State::Code; }
            }
            State::BlockComment => {
                buf.push(c);
                if c == '*' && bytes.get(i + 1).copied() == Some(b'/') {
                    buf.push('/');
                    state = State::Code;
                    i += 2;
                    continue;
                }
            }
        }

        if c == '\n' { current_line += 1; }
        i += 1;
    }

    // Flush whatever's left as the final batch (GO is optional at EOF).
    push_batch(&mut buf, batch_start_line, &mut out);
    out
}

/// If the line that starts at `i` (`i` must be either 0 or the byte right after
/// a `\n`) is a `GO` batch separator (optionally followed by an integer
/// repetition count, case-insensitive, with arbitrary whitespace), return the
/// byte index just past the trailing newline. Otherwise return `None`.
fn match_go_line(bytes: &[u8], i: usize) -> Option<usize> {
    // Skip leading whitespace (but not newline — we're testing this single line).
    let mut j = i;
    while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') { j += 1; }
    // Must start with G/g then O/o, not followed by an identifier-continuing char.
    if j + 1 >= bytes.len() { return None; }
    if !(bytes[j] == b'G' || bytes[j] == b'g') { return None; }
    if !(bytes[j + 1] == b'O' || bytes[j + 1] == b'o') { return None; }
    let after = j + 2;
    // The next char must be a word boundary (whitespace, newline, EOF, or comment).
    let next_is_word = after < bytes.len()
        && (bytes[after] as char).is_ascii_alphanumeric();
    if next_is_word { return None; }
    // Optional whitespace then optional digits (repetition count).
    let mut k = after;
    while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t') { k += 1; }
    while k < bytes.len() && (bytes[k] as char).is_ascii_digit() { k += 1; }
    // Optional trailing whitespace.
    while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t') { k += 1; }
    // Optional line comment to end of line.
    if k + 1 < bytes.len() && bytes[k] == b'-' && bytes[k + 1] == b'-' {
        while k < bytes.len() && bytes[k] != b'\n' { k += 1; }
    }
    // Tolerate Windows CRLF line endings — `\r` between the trailing
    // whitespace and the `\n` is part of the line terminator and must be
    // consumed alongside the newline. Without this, a script saved with
    // CRLF would slip every GO past the matcher and end up shipped to the
    // server as part of the previous batch, producing nonsense errors.
    if k < bytes.len() && bytes[k] == b'\r' { k += 1; }
    // Must end at newline or EOF.
    if k >= bytes.len() { return Some(bytes.len()); }
    if bytes[k] == b'\n' { return Some(k + 1); }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_statement_batches_are_kept_together_without_go() {
        // Sqlcmd-faithful semantics: `;` is a statement separator within a
        // batch, not a batch terminator. A script without GO is a single
        // batch — SQL Server happily handles multiple statements per batch.
        let sql = "SELECT 1; SELECT 2; SELECT 3";
        let batches = split_sql_batches(sql);
        assert_eq!(batches.len(), 1);
        assert!(batches[0].text.contains("SELECT 1"));
        assert!(batches[0].text.contains("SELECT 2"));
        assert!(batches[0].text.contains("SELECT 3"));
    }

    #[test]
    fn procedure_body_semicolons_dont_split_the_batch() {
        // This was the real-world regression: my splitter cut CREATE
        // PROCEDURE bodies on every internal `SET NOCOUNT ON;`, leaving an
        // orphan MERGE in the next batch. SQL Server then complained about
        // "Incorrect syntax near 'PROCEDURE'" once the END drifted into the
        // wrong batch.
        let sql = "CREATE PROCEDURE p AS\nBEGIN\n  SET NOCOUNT ON;\n  MERGE t AS x USING s ON x.id = s.id WHEN MATCHED THEN UPDATE SET v=1;\nEND\nGO\nSELECT 'after'";
        let batches = split_sql_batches(sql);
        assert_eq!(batches.len(), 2, "got: {batches:#?}");
        assert!(batches[0].text.starts_with("CREATE PROCEDURE p"));
        assert!(batches[0].text.contains("SET NOCOUNT ON"));
        assert!(batches[0].text.contains("MERGE"));
        assert!(batches[0].text.contains("END"));
        assert!(batches[1].text.starts_with("SELECT 'after'"));
    }

    #[test]
    fn comments_and_strings_remain_neutral_to_splitting() {
        // `;` inside strings or comments never mattered for our purposes
        // (it was never a separator) — keep a regression test that string-
        // and comment-aware tracking still works for `GO` detection inside
        // dynamic SQL, where it could theoretically appear inside a string.
        let sql = "EXEC('PRINT ''hi''; GO ABORTED')\nGO\nSELECT 1";
        let batches = split_sql_batches(sql);
        assert_eq!(batches.len(), 2);
        assert!(batches[0].text.contains("EXEC('PRINT ''hi''; GO ABORTED')"));
        assert_eq!(batches[1].text, "SELECT 1");
    }

    #[test]
    fn go_on_its_own_line_terminates_a_batch() {
        let sql = "CREATE PROCEDURE p AS BEGIN SELECT 1 END\nGO\nCREATE PROCEDURE q AS BEGIN SELECT 2 END";
        let batches = split_sql_batches(sql);
        assert_eq!(batches.len(), 2);
        assert!(batches[0].text.starts_with("CREATE PROCEDURE p"));
        assert!(batches[1].text.starts_with("CREATE PROCEDURE q"));
    }

    #[test]
    fn go_is_case_insensitive_and_tolerates_repeat_count_and_trailing_comment() {
        let sql = "SELECT 1\n  Go 3  -- repeat\nSELECT 2";
        let batches = split_sql_batches(sql);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].text, "SELECT 1");
        assert_eq!(batches[1].text, "SELECT 2");
    }

    #[test]
    fn go_inside_identifier_is_not_a_separator() {
        // "GOOD" begins with GO but is an identifier, not a batch terminator.
        let sql = "SELECT GOOD FROM t";
        let batches = split_sql_batches(sql);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].text, "SELECT GOOD FROM t");
    }

    #[test]
    fn create_procedure_body_is_one_batch_until_the_next_go() {
        let sql = "CREATE PROCEDURE p\nAS\nBEGIN\n  SET NOCOUNT ON;\n  SELECT 1;\nEND\nGO\nSELECT 'after'";
        let batches = split_sql_batches(sql);
        // sqlcmd-faithful: GO is the only batch separator. The procedure
        // body — including every internal `;` — becomes one batch.
        assert_eq!(batches.len(), 2, "got: {batches:#?}");
        assert!(batches[0].text.starts_with("CREATE PROCEDURE p"));
        assert!(batches[0].text.contains("SET NOCOUNT ON"));
        assert!(batches[0].text.contains("END"));
        assert_eq!(batches[1].text, "SELECT 'after'");
        assert_eq!(batches[0].start_line, 1);
    }

    #[test]
    fn each_batch_records_its_source_line() {
        // Without `;` as a separator the source-line check uses GO boundaries.
        let sql = "SELECT 1\nGO\n\nSELECT 2\nGO\nSELECT 3";
        let batches = split_sql_batches(sql);
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].start_line, 1, "first batch starts at line 1");
        assert_eq!(batches[1].start_line, 4, "second batch starts after the blank line at line 4");
        assert_eq!(batches[2].start_line, 6, "third batch starts after the GO at line 6");
    }

    #[test]
    fn crlf_line_endings_are_tolerated_for_go() {
        // Scripts saved on Windows arrive with `\r\n` line endings — the
        // GO matcher needs to consume the trailing `\r` so the batch break
        // actually happens. Without this, a CRLF file slips every GO past
        // the matcher and ships it to the server as part of the previous
        // batch.
        let sql = "SELECT 1\r\nGO\r\nSELECT 2";
        let batches = split_sql_batches(sql);
        assert_eq!(batches.len(), 2);
        assert!(batches[0].text.contains("SELECT 1"));
        assert!(batches[1].text.contains("SELECT 2"));
    }

    #[test]
    fn empty_input_yields_no_batches() {
        assert!(split_sql_batches("").is_empty());
        assert!(split_sql_batches("   \n   \n").is_empty());
        assert!(split_sql_batches("GO\nGO\n").is_empty());
    }

    #[test]
    fn unicode_box_drawing_chars_in_comments_dont_break_splitting() {
        // The user's bootstrap script has comment lines like
        // `-- ── Procedures ──` where `─` is U+2500 — a 3-byte UTF-8
        // sequence. The tokenizer walks bytes, so we want to be sure
        // multi-byte chars don't shift line counts or hide a GO.
        let sql = "-- ══════════════════\n-- ── Tables ──────\nGO\nCREATE PROCEDURE p AS BEGIN SELECT 1 END\nGO";
        let batches = split_sql_batches(sql);
        // The leading comments before the first GO produce a batch whose
        // trimmed text starts with `--` (comment-only batches send fine to
        // SQL Server — they're no-ops). After that, the CREATE PROCEDURE
        // batch must start clean.
        let proc_batch = batches.iter().find(|b| b.text.contains("CREATE PROCEDURE")).unwrap();
        assert!(
            proc_batch.text.trim_start().starts_with("CREATE PROCEDURE"),
            "CREATE PROCEDURE batch was: {:?}", proc_batch.text
        );
        assert_eq!(proc_batch.start_line, 4, "expected line 4, got {}", proc_batch.start_line);
    }

    #[test]
    fn bootstrap_staging_repro_keeps_each_create_procedure_intact() {
        // Verbatim repro of the user's bootstrap-staging.sql header + first
        // two CREATE PROCEDURE blocks. Every CREATE PROCEDURE must come out
        // as the first non-whitespace token of its own batch — otherwise
        // SQL Server fires "Incorrect syntax near 'PROCEDURE'" (the actual
        // failure the user reported).
        let sql = r#"-- ============================================================================
-- bootstrap-staging.sql
-- ============================================================================

IF DB_ID('AIS') IS NULL CREATE DATABASE AIS;
GO
USE AIS;
GO
IF SCHEMA_ID('ais') IS NULL EXEC('CREATE SCHEMA ais');
GO

IF OBJECT_ID('ais.usp_PreAnalysisStaging_InsertFinding', 'P') IS NOT NULL
  DROP PROCEDURE ais.usp_PreAnalysisStaging_InsertFinding;
GO
CREATE PROCEDURE ais.usp_PreAnalysisStaging_InsertFinding
  @CorrelationId        varchar(64),
  @RowKey               varchar(256),
  @TtlUtc               datetime2
AS
BEGIN
  SET NOCOUNT ON;
  MERGE ais.PreAnalysisStaging AS t
  USING (SELECT @CorrelationId AS CorrelationId, @RowKey AS RowKey) AS s
    ON t.CorrelationId = s.CorrelationId AND t.RowKey = s.RowKey
  WHEN MATCHED THEN UPDATE SET
       TtlUtc = @TtlUtc
  WHEN NOT MATCHED THEN INSERT (CorrelationId, RowKey, TtlUtc)
  VALUES (@CorrelationId, @RowKey, @TtlUtc);
END
GO

IF OBJECT_ID('ais.usp_PreAnalysisStaging_InsertSummary', 'P') IS NOT NULL
  DROP PROCEDURE ais.usp_PreAnalysisStaging_InsertSummary;
GO
CREATE PROCEDURE ais.usp_PreAnalysisStaging_InsertSummary
  @CorrelationId        varchar(64)
AS
BEGIN
  SET NOCOUNT ON;
  SELECT 1;
END
GO"#;
        let batches = split_sql_batches(sql);
        // For every batch that DOES contain "CREATE PROCEDURE", that token
        // must be the very first non-whitespace word of the batch.
        for b in &batches {
            if b.text.contains("CREATE PROCEDURE") {
                let trimmed = b.text.trim_start();
                assert!(
                    trimmed.starts_with("CREATE PROCEDURE"),
                    "batch at line {} contains CREATE PROCEDURE not at the start — full text:\n{}",
                    b.start_line, b.text,
                );
            }
        }
        // And we should have at least the two procedure batches plus the
        // header batches.
        let create_proc_batches = batches.iter().filter(|b| b.text.trim_start().starts_with("CREATE PROCEDURE")).count();
        assert_eq!(create_proc_batches, 2, "expected exactly two CREATE PROCEDURE batches; batches:\n{:#?}", batches);
    }

    #[test]
    fn n_prefixed_strings_preserve_content_inside_a_batch() {
        // No GO present → one batch. The point of this test is that the
        // tokenizer's string-tracking still works around N'…' literals so a
        // hypothetical GO inside one wouldn't terminate prematurely.
        let sql = "INSERT INTO t VALUES (N'message; with; GO inside'); SELECT 1";
        let batches = split_sql_batches(sql);
        assert_eq!(batches.len(), 1);
        assert!(batches[0].text.contains("N'message; with; GO inside'"));
        assert!(batches[0].text.contains("SELECT 1"));
    }
}
