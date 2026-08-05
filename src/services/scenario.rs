//! Replayable action sequences — "set up this state, trigger that workflow,
//! assert what came out", saved as data and re-runnable on demand.
//!
//! Every step here is a thin wrapper over an existing service call
//! (`azurite_client`, `sb_amqp`, `sql_runner`, `cosmos_query`, `workflows`).
//! The value this module adds is not new capability but *sequencing*: ordering,
//! `{{var}}` substitution between steps, and — most importantly — the
//! `WaitFor*` steps. Without those, replaying an async pipeline is a coin flip,
//! because a trigger returns long before the workflow it started has finished.
//!
//! Scenarios live in the project being developed, not in ais-runner:
//!
//! ```text
//! <project>/.ais-runner/scenarios/*.json
//! ```
//!
//! Same rationale as `msg_template`: a team's fixtures belong next to the
//! workflows they exercise, versioned together and reviewable in a PR.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::services::{azurite_client, cosmos_query, sb_amqp, sb_testing, sql_runner, workflows};

/// Where scenarios live, relative to the selected project root.
const SCENARIO_DIR: &str = ".ais-runner/scenarios";

/// How often a `WaitFor*` step re-checks its condition.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

// ─────────────────────────────────────────────────────────────────────────────
// 1. Model
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Scenario {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Seed values for `{{var}}` substitution. Steps with a `capture` field add
    /// more as the run progresses.
    #[serde(default)]
    pub vars: HashMap<String, String>,
    pub steps: Vec<Step>,
    /// Source file, for error messages and re-saving. Not part of the JSON.
    #[serde(skip)]
    pub source: PathBuf,
}

/// One action in a scenario.
///
/// Note there is no "create folder" step: an Azurite virtual folder is just a
/// blob-name prefix, so `upload_file`/`upload_inline` with `some/prefix/name`
/// creates it implicitly.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Step {
    // ── Blob ────────────────────────────────────────────────────────────
    CreateContainer {
        container: String,
    },
    UploadFile {
        container: String,
        file: String,
        blob_name: String,
    },
    /// Write literal content instead of copying from disk — keeps a scenario
    /// self-contained when the payload is small.
    UploadInline {
        container: String,
        blob_name: String,
        content: String,
    },
    ClearContainer {
        container: String,
    },

    // ── Service Bus ─────────────────────────────────────────────────────
    SendMessage {
        queue: String,
        body: String,
    },
    DrainQueue {
        queue: String,
    },

    // ── SQL ─────────────────────────────────────────────────────────────
    CreateSqlDatabase {
        name: String,
    },
    /// `GO`-separated scripts are split by `sql_runner::run_sql` itself.
    RunSql {
        database: String,
        sql: String,
        #[serde(default)]
        capture: Option<String>,
    },

    // ── Cosmos ──────────────────────────────────────────────────────────
    CreateCosmosDatabase {
        database: String,
    },
    CreateCosmosContainer {
        database: String,
        container: String,
        #[serde(default = "default_partition_key")]
        partition_key: String,
    },
    UpsertCosmosDocument {
        database: String,
        container: String,
        document: Value,
    },
    RunCosmosQuery {
        database: String,
        container: String,
        query: String,
        #[serde(default)]
        capture: Option<String>,
    },

    // ── Workflow ────────────────────────────────────────────────────────
    RunWorkflow {
        workflow: String,
        #[serde(default = "default_trigger")]
        trigger: String,
        #[serde(default)]
        body: String,
    },

    // ── Synchronisation ─────────────────────────────────────────────────
    Sleep {
        ms: u64,
    },
    /// Poll until `queue` holds at least `min_count` messages whose value at
    /// dot-`path` equals `expected`. An empty `path` counts every message.
    WaitForMessage {
        queue: String,
        #[serde(default)]
        path: String,
        #[serde(default)]
        expected: String,
        #[serde(default = "one")]
        min_count: usize,
        #[serde(default = "default_timeout")]
        timeout_ms: u64,
    },
    /// Poll until `workflow` has a run in a terminal state that started after
    /// the most recent `run_workflow` step in this scenario.
    ///
    /// Fails if that run finished in any state other than Succeeded — a
    /// workflow that reliably reaches "Failed" is not a passing scenario.
    WaitForRun {
        workflow: String,
        #[serde(default = "default_timeout")]
        timeout_ms: u64,
    },
    /// Poll until `sql` returns at least `min_rows` rows.
    WaitForSql {
        database: String,
        sql: String,
        #[serde(default = "one")]
        min_rows: usize,
        #[serde(default = "default_timeout")]
        timeout_ms: u64,
    },

    // ── Assertion ───────────────────────────────────────────────────────
    /// Same predicate as `WaitForMessage`, evaluated once. Fails the scenario
    /// rather than waiting.
    Expect {
        queue: String,
        #[serde(default)]
        path: String,
        expected: String,
        #[serde(default = "one")]
        min_count: usize,
    },
}

fn one() -> usize {
    1
}
fn default_partition_key() -> String {
    "/id".to_string()
}
fn default_trigger() -> String {
    "manual".to_string()
}
fn default_timeout() -> u64 {
    30_000
}

/// Everything a run needs that the scenario file deliberately doesn't hardcode,
/// so the same scenario works against whatever emulators are currently up.
#[derive(Clone, Debug)]
pub struct RunContext {
    pub sb_host: String,
    pub cosmos_endpoint: String,
    pub cosmos_key: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepStatus {
    Ok,
    Failed,
    Skipped,
}

#[derive(Clone, Debug)]
pub struct StepResult {
    pub index: usize,
    pub label: String,
    pub status: StepStatus,
    pub detail: String,
    pub elapsed_ms: u128,
}

// ─────────────────────────────────────────────────────────────────────────────
// 2. Persistence
// ─────────────────────────────────────────────────────────────────────────────

/// Read every scenario under `<project_root>/.ais-runner/scenarios`.
///
/// A malformed file is reported but doesn't hide the rest — one bad scenario
/// shouldn't make the whole panel look empty.
pub fn discover(project_root: &Path) -> (Vec<Scenario>, Vec<String>) {
    let dir = project_root.join(SCENARIO_DIR);
    let mut scenarios = Vec::new();
    let mut errors = Vec::new();

    let Ok(entries) = std::fs::read_dir(&dir) else {
        return (scenarios, errors);
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match load(&path) {
            Ok(s) => scenarios.push(s),
            Err(e) => errors.push(format!("{}: {e}", path.display())),
        }
    }

    scenarios.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    (scenarios, errors)
}

pub fn load(path: &Path) -> Result<Scenario, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut scenario: Scenario = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    scenario.source = path.to_path_buf();
    Ok(scenario)
}

// ─────────────────────────────────────────────────────────────────────────────
// 3. Runner
// ─────────────────────────────────────────────────────────────────────────────

/// Execute every step in order, stopping at the first failure.
///
/// `on_step` fires after each step so the UI can stream progress — a scenario
/// with `WaitFor*` steps can legitimately run for minutes, and showing nothing
/// until it finishes would look like a hang.
pub async fn run(
    scenario: &Scenario,
    ctx: &RunContext,
    mut on_step: impl FnMut(StepResult),
) -> Vec<StepResult> {
    let mut state = RunState {
        vars: scenario.vars.clone(),
        run_floor: chrono::Utc::now(),
        claimed_runs: std::collections::HashSet::new(),
    };
    let mut results = Vec::new();
    let mut aborted = false;

    for (index, step) in scenario.steps.iter().enumerate() {
        if aborted {
            let result = StepResult {
                index,
                label: label_of(step),
                status: StepStatus::Skipped,
                detail: "skipped after an earlier failure".to_string(),
                elapsed_ms: 0,
            };
            on_step(result.clone());
            results.push(result);
            continue;
        }

        let started = std::time::Instant::now();
        // Substitute per-step rather than up front, so a step can consume a
        // variable captured by the step before it.
        let outcome = match resolve_vars(step, &state.vars) {
            Ok(resolved) => exec(&resolved, ctx, &mut state).await,
            Err(e) => Err(e),
        };
        let elapsed_ms = started.elapsed().as_millis();

        let result = match outcome {
            Ok(detail) => StepResult {
                index,
                label: label_of(step),
                status: StepStatus::Ok,
                detail,
                elapsed_ms,
            },
            Err(detail) => {
                aborted = true;
                StepResult {
                    index,
                    label: label_of(step),
                    status: StepStatus::Failed,
                    detail,
                    elapsed_ms,
                }
            }
        };
        on_step(result.clone());
        results.push(result);
    }

    results
}

struct RunState {
    vars: HashMap<String, String>,
    /// Earliest start time a run may have and still satisfy `WaitForRun`.
    ///
    /// Set when the scenario starts, so a run left over from a previous replay
    /// can never count — the common trigger is a queue message or a blob
    /// upload, not `RunWorkflow`, and keying the floor off `RunWorkflow` alone
    /// left those scenarios with no floor at all.
    ///
    /// `RunWorkflow` tightens it. The other trigger surfaces deliberately don't:
    /// a scenario that uploads a blob and *then* sends a message would end up
    /// with a floor later than the run it is waiting for, and would time out
    /// waiting for a run that had already happened.
    run_floor: chrono::DateTime<chrono::Utc>,
    /// Runs already claimed by an earlier `WaitForRun`. Without this, two waits
    /// on the same workflow are both satisfied by the first run — the second
    /// would pass before its trigger had produced anything.
    claimed_runs: std::collections::HashSet<String>,
}

async fn exec(step: &Step, ctx: &RunContext, state: &mut RunState) -> Result<String, String> {
    use Step::*;
    match step {
        // ── Blob ────────────────────────────────────────────────────────
        // These are reqwest::blocking calls; running them directly on the async
        // runtime would stall the UI thread.
        CreateContainer { container } => {
            let name = container.clone();
            blocking(move || azurite_client::create_container(&name)).await?;
            Ok(format!("container '{container}' ready"))
        }
        UploadFile {
            container,
            file,
            blob_name,
        } => {
            let (c, f, b) = (container.clone(), file.clone(), blob_name.clone());
            blocking(move || azurite_client::upload_blob(&c, &f, &b)).await?;
            Ok(format!("uploaded '{blob_name}' to '{container}'"))
        }
        UploadInline {
            container,
            blob_name,
            content,
        } => {
            let (c, b, data) = (container.clone(), blob_name.clone(), content.clone().into_bytes());
            let size = data.len();
            blocking(move || azurite_client::upload_blob_bytes_sync(&c, &b, data)).await?;
            Ok(format!("wrote '{blob_name}' ({size} bytes)"))
        }
        ClearContainer { container } => {
            let name = container.clone();
            let removed = blocking(move || azurite_client::clear_container(&name)).await?;
            Ok(format!("cleared {removed} blob(s) from '{container}'"))
        }

        // ── Service Bus ─────────────────────────────────────────────────
        SendMessage { queue, body } => {
            sb_amqp::send_amqp_message(&ctx.sb_host, queue, body).await?;
            Ok(format!("sent {} bytes to '{queue}'", body.len()))
        }
        DrainQueue { queue } => {
            let n = sb_amqp::drain_queue(&ctx.sb_host, queue).await?;
            Ok(format!("drained {n} message(s) from '{queue}'"))
        }

        // ── SQL ─────────────────────────────────────────────────────────
        CreateSqlDatabase { name } => sql_runner::create_database(name).await,
        RunSql {
            database,
            sql,
            capture,
        } => {
            let out = sql_runner::run_sql(database, sql).await?;
            if let Some(key) = capture {
                state.vars.insert(key.clone(), out.trim().to_string());
            }
            Ok(summarise(&out))
        }

        // ── Cosmos ──────────────────────────────────────────────────────
        CreateCosmosDatabase { database } => {
            let created =
                cosmos_query::create_database(&ctx.cosmos_endpoint, &ctx.cosmos_key, database)
                    .await?;
            Ok(if created {
                format!("created database '{database}'")
            } else {
                format!("database '{database}' already existed")
            })
        }
        CreateCosmosContainer {
            database,
            container,
            partition_key,
        } => {
            let created = cosmos_query::create_container(
                &ctx.cosmos_endpoint,
                &ctx.cosmos_key,
                database,
                container,
                partition_key,
            )
            .await?;
            Ok(if created {
                format!("created container '{container}' (pk {partition_key})")
            } else {
                format!("container '{container}' already existed")
            })
        }
        UpsertCosmosDocument {
            database,
            container,
            document,
        } => {
            cosmos_query::upsert_document(
                &ctx.cosmos_endpoint,
                &ctx.cosmos_key,
                database,
                container,
                document.clone(),
            )
            .await?;
            Ok(format!("upserted document into '{database}/{container}'"))
        }
        RunCosmosQuery {
            database,
            container,
            query,
            capture,
        } => {
            let value = cosmos_query::run_query(
                &ctx.cosmos_endpoint,
                &ctx.cosmos_key,
                database,
                container,
                query,
            )
            .await?;
            let count = value["Documents"].as_array().map(|a| a.len()).unwrap_or(0);
            if let Some(key) = capture {
                state.vars.insert(key.clone(), value.to_string());
            }
            Ok(format!("query returned {count} document(s)"))
        }

        // ── Workflow ────────────────────────────────────────────────────
        RunWorkflow {
            workflow,
            trigger,
            body,
        } => {
            // Tighten the floor *before* triggering: a run that starts while the
            // request is still in flight must still count.
            state.run_floor = chrono::Utc::now();
            workflows::run_trigger_direct(workflow, trigger, body).await?;
            Ok(format!("triggered '{workflow}' via '{trigger}'"))
        }

        // ── Synchronisation ─────────────────────────────────────────────
        Sleep { ms } => {
            tokio::time::sleep(Duration::from_millis(*ms)).await;
            Ok(format!("slept {ms}ms"))
        }
        WaitForMessage {
            queue,
            path,
            expected,
            min_count,
            timeout_ms,
        } => {
            poll_until(*timeout_ms, || async {
                let r =
                    sb_testing::check_expectation(&ctx.sb_host, queue, path, expected, *min_count)
                        .await?;
                Ok((r.passed, r.detail))
            })
            .await
        }
        WaitForRun {
            workflow,
            timeout_ms,
        } => {
            let floor = state.run_floor;
            let claimed = state.claimed_runs.clone();
            // Which run satisfied the wait, so it can be claimed afterwards.
            // A cell rather than a captured `&mut`, because the closure hands
            // its future to `poll_until` and can't lend a mutable borrow across
            // the await.
            let winner: std::cell::RefCell<Option<String>> = Default::default();

            let detail = poll_until(*timeout_ms, || async {
                match latest_terminal_run(workflow, floor, &claimed).await {
                    Some((run_name, status)) if status == "Succeeded" => {
                        *winner.borrow_mut() = Some(run_name.clone());
                        Ok((true, format!("run {run_name} Succeeded")))
                    }
                    // A terminal non-success is final — no amount of waiting
                    // improves it, so surface it now instead of at timeout.
                    Some((run_name, status)) => Err(format!("run {run_name} finished {status}")),
                    None => Ok((false, "no terminal run yet".to_string())),
                }
            })
            .await?;

            if let Some(name) = winner.into_inner() {
                state.claimed_runs.insert(name);
            }
            Ok(detail)
        }
        WaitForSql {
            database,
            sql,
            min_rows,
            timeout_ms,
        } => {
            poll_until(*timeout_ms, || async {
                let out = sql_runner::run_sql(database, sql).await?;
                let rows = sql_runner::count_rows(&out);
                Ok((
                    rows >= *min_rows,
                    format!("{rows} row(s) (expected >= {min_rows})"),
                ))
            })
            .await
        }

        // ── Assertion ───────────────────────────────────────────────────
        Expect {
            queue,
            path,
            expected,
            min_count,
        } => {
            let r =
                sb_testing::check_expectation(&ctx.sb_host, queue, path, expected, *min_count)
                    .await?;
            if r.passed {
                Ok(r.detail)
            } else {
                Err(r.detail)
            }
        }
    }
}

/// Run a blocking service call off the async runtime.
async fn blocking<T, F>(f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("task panicked: {e}"))?
}

/// Poll `check` until it reports success or the deadline passes.
///
/// The most recent detail becomes the timeout message, so a failure explains
/// what the condition actually saw rather than just "timed out".
async fn poll_until<F, Fut>(timeout_ms: u64, mut check: F) -> Result<String, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(bool, String), String>>,
{
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        // `?` rather than a retry: a hard error inside the check is terminal.
        // A malformed query or a dead emulator won't fix itself, and burning
        // the whole timeout on it would bury the real message.
        let (passed, detail) = check().await?;
        if passed {
            return Ok(detail);
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("timed out after {timeout_ms}ms — {detail}"));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Most recent run of `workflow` in a terminal state that started at or after
/// `floor` and hasn't already been claimed by an earlier wait. `None` while the
/// workflow is still running or hasn't started.
///
/// A run with no parseable start time is excluded rather than accepted: the
/// whole point of the floor is to reject runs from a previous replay, and
/// letting an untimed run through would defeat it.
async fn latest_terminal_run(
    workflow: &str,
    floor: chrono::DateTime<chrono::Utc>,
    claimed: &std::collections::HashSet<String>,
) -> Option<(String, String)> {
    const TERMINAL: [&str; 4] = ["Succeeded", "Failed", "Cancelled", "Aborted"];

    let runs = workflows::list_runs(workflow).await.ok()?;
    runs.iter()
        .filter(|r| TERMINAL.contains(&r.properties.status.as_str()))
        .filter(|r| !claimed.contains(&r.name))
        .filter(|r| started_at_or_after(r.properties.start_time.as_deref(), floor))
        .max_by_key(|r| r.properties.start_time.clone())
        .map(|r| (r.name.clone(), r.properties.status.clone()))
}

fn started_at_or_after(start_time: Option<&str>, floor: chrono::DateTime<chrono::Utc>) -> bool {
    start_time
        .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
        .map(|t| t.with_timezone(&chrono::Utc) >= floor)
        .unwrap_or(false)
}

/// Collapse multi-line command output to something that fits one status line.
fn summarise(out: &str) -> String {
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    match lines.len() {
        0 => "ok".to_string(),
        1 => lines[0].trim().to_string(),
        n => format!("{} (+{} more lines)", lines[0].trim(), n - 1),
    }
}

pub fn label_of(step: &Step) -> String {
    use Step::*;
    match step {
        CreateContainer { container } => format!("create container {container}"),
        UploadFile { blob_name, .. } => format!("upload {blob_name}"),
        UploadInline { blob_name, .. } => format!("write {blob_name}"),
        ClearContainer { container } => format!("clear {container}"),
        SendMessage { queue, .. } => format!("send to {queue}"),
        DrainQueue { queue } => format!("drain {queue}"),
        CreateSqlDatabase { name } => format!("create SQL db {name}"),
        RunSql { database, .. } => format!("run SQL on {database}"),
        CreateCosmosDatabase { database } => format!("create Cosmos db {database}"),
        CreateCosmosContainer { container, .. } => format!("create Cosmos container {container}"),
        UpsertCosmosDocument { container, .. } => format!("upsert doc into {container}"),
        RunCosmosQuery { container, .. } => format!("query {container}"),
        RunWorkflow { workflow, .. } => format!("run {workflow}"),
        Sleep { ms } => format!("sleep {ms}ms"),
        WaitForMessage { queue, .. } => format!("wait for message on {queue}"),
        WaitForRun { workflow, .. } => format!("wait for {workflow} run"),
        WaitForSql { database, .. } => format!("wait for SQL on {database}"),
        Expect { queue, .. } => format!("expect on {queue}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 4. Variable substitution
// ─────────────────────────────────────────────────────────────────────────────

/// Replace `{{var}}` in every string field of `step`.
///
/// Done by round-tripping through `serde_json` and walking the value tree,
/// which substitutes into nested structures (a Cosmos `document`) without this
/// module needing to know each step's shape. Walking the tree rather than doing
/// a textual replace on the serialized JSON also means a variable whose value
/// contains a quote or backslash can't corrupt the document.
fn resolve_vars(step: &Step, vars: &HashMap<String, String>) -> Result<Step, String> {
    if vars.is_empty() {
        return Ok(step.clone());
    }
    let mut value = serde_json::to_value(step).map_err(|e| e.to_string())?;
    substitute(&mut value, vars);
    serde_json::from_value(value).map_err(|e| format!("substitution produced an invalid step: {e}"))
}

fn substitute(value: &mut Value, vars: &HashMap<String, String>) {
    match value {
        Value::String(s) => {
            if s.contains("{{") {
                *s = expand(s, vars);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| substitute(v, vars)),
        Value::Object(map) => map.values_mut().for_each(|v| substitute(v, vars)),
        _ => {}
    }
}

/// Expand `{{name}}` placeholders. An unknown name is left verbatim so the
/// failure shows up as a recognisable `{{typo}}` in the step detail rather than
/// silently becoming an empty string.
fn expand(text: &str, vars: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            // Unterminated — emit the rest as-is rather than dropping it.
            out.push_str(&rest[start..]);
            return out;
        };
        let name = after[..end].trim();
        match vars.get(name) {
            Some(v) => out.push_str(v),
            None => out.push_str(&rest[start..start + 2 + end + 2]),
        }
        rest = &after[end + 2..];
    }

    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn expand_replaces_known_names_and_keeps_unknown_ones() {
        let v = vars(&[("id", "INV-1")]);
        assert_eq!(expand("file-{{id}}.xml", &v), "file-INV-1.xml");
        assert_eq!(expand("{{id}}/{{id}}", &v), "INV-1/INV-1");
        assert_eq!(expand("{{missing}}", &v), "{{missing}}");
        assert_eq!(expand("no placeholders", &v), "no placeholders");
        assert_eq!(expand("{{ id }}", &v), "INV-1");
    }

    #[test]
    fn expand_leaves_unterminated_placeholder_intact() {
        assert_eq!(expand("a {{b", &vars(&[])), "a {{b");
    }

    #[test]
    fn substitution_reaches_nested_document_fields() {
        let step = Step::UpsertCosmosDocument {
            database: "{{db}}".into(),
            container: "events".into(),
            document: serde_json::json!({ "id": "{{id}}", "tags": ["{{id}}"] }),
        };
        let resolved = resolve_vars(&step, &vars(&[("db", "EventStore"), ("id", "42")])).unwrap();

        let Step::UpsertCosmosDocument {
            database, document, ..
        } = resolved
        else {
            panic!("wrong variant")
        };
        assert_eq!(database, "EventStore");
        assert_eq!(document["id"], "42");
        assert_eq!(document["tags"][0], "42");
    }

    #[test]
    fn substitution_does_not_corrupt_values_containing_quotes() {
        let step = Step::SendMessage {
            queue: "q".into(),
            body: "{{payload}}".into(),
        };
        let resolved = resolve_vars(&step, &vars(&[("payload", r#"{"a":"b\"c"}"#)])).unwrap();
        let Step::SendMessage { body, .. } = resolved else {
            panic!("wrong variant")
        };
        assert_eq!(body, r#"{"a":"b\"c"}"#);
    }

    #[test]
    fn steps_round_trip_through_json_with_defaults_applied() {
        let json = r#"{ "action": "run_workflow", "workflow": "Pivot-Ignite-Invoice" }"#;
        let step: Step = serde_json::from_str(json).unwrap();
        let Step::RunWorkflow {
            workflow,
            trigger,
            body,
        } = step
        else {
            panic!("wrong variant")
        };
        assert_eq!(workflow, "Pivot-Ignite-Invoice");
        assert_eq!(trigger, "manual");
        assert_eq!(body, "");
    }

    #[test]
    fn a_full_scenario_document_parses_and_round_trips() {
        // Exercises every variant so a rename or a changed serde tag fails here
        // rather than silently breaking scenario files already in projects.
        let json = r#"{
          "name": "coverage",
          "vars": { "id": "42" },
          "steps": [
            { "action": "create_container", "container": "in" },
            { "action": "upload_file", "container": "in", "file": "/tmp/a.xml", "blob_name": "a.xml" },
            { "action": "upload_inline", "container": "in", "blob_name": "b.xml", "content": "<x/>" },
            { "action": "clear_container", "container": "in" },
            { "action": "send_message", "queue": "q", "body": "{}" },
            { "action": "drain_queue", "queue": "q" },
            { "action": "create_sql_database", "name": "aisdev" },
            { "action": "run_sql", "database": "aisdev", "sql": "SELECT 1", "capture": "out" },
            { "action": "create_cosmos_database", "database": "EventStore" },
            { "action": "create_cosmos_container", "database": "EventStore", "container": "events" },
            { "action": "upsert_cosmos_document", "database": "EventStore", "container": "events", "document": { "id": "1" } },
            { "action": "run_cosmos_query", "database": "EventStore", "container": "events", "query": "SELECT * FROM c" },
            { "action": "run_workflow", "workflow": "W", "trigger": "manual", "body": "{}" },
            { "action": "sleep", "ms": 100 },
            { "action": "wait_for_message", "queue": "q", "path": "id", "expected": "42" },
            { "action": "wait_for_run", "workflow": "W" },
            { "action": "wait_for_sql", "database": "aisdev", "sql": "SELECT 1", "min_rows": 1 },
            { "action": "expect", "queue": "q", "expected": "42", "min_count": 1 }
          ]
        }"#;

        let scenario: Scenario = serde_json::from_str(json).unwrap();
        assert_eq!(scenario.steps.len(), 18);

        // Defaults applied where the document omitted them.
        let Step::CreateCosmosContainer { partition_key, .. } = &scenario.steps[9] else {
            panic!("wrong variant")
        };
        assert_eq!(partition_key, "/id");

        // Re-serializing and re-parsing must preserve every step.
        let text = serde_json::to_string(&scenario).unwrap();
        let again: Scenario = serde_json::from_str(&text).unwrap();
        assert_eq!(again.steps.len(), scenario.steps.len());
    }

    #[test]
    fn wait_for_sql_does_not_count_the_empty_result_sentinel_as_a_row() {
        // `run_sql` returns a sentinel, not "", when a SELECT matches nothing.
        // Counting lines naively made `wait_for_sql` pass on its first poll.
        assert_eq!(sql_runner::count_rows(sql_runner::NO_ROWS), 0);
        assert_eq!(sql_runner::count_rows("(0 row(s) affected)"), 0);
        assert_eq!(sql_runner::count_rows("(already exists — skipped)"), 0);
        assert_eq!(sql_runner::count_rows("1 | a\n2 | b\n"), 2);
        assert_eq!(
            sql_runner::count_rows("1 | a\n(1 row(s) affected)\n"),
            1,
            "status lines mixed with data must not inflate the count"
        );
    }

    #[test]
    fn run_floor_rejects_runs_that_started_before_the_scenario() {
        let floor = chrono::Utc::now();
        let before = (floor - chrono::Duration::seconds(30)).to_rfc3339();
        let after = (floor + chrono::Duration::seconds(1)).to_rfc3339();

        assert!(!started_at_or_after(Some(&before), floor));
        assert!(started_at_or_after(Some(&after), floor));
        assert!(started_at_or_after(Some(&floor.to_rfc3339()), floor));
        // Unparseable or absent — excluded, so a stale run can't slip past.
        assert!(!started_at_or_after(None, floor));
        assert!(!started_at_or_after(Some("not-a-timestamp"), floor));
    }

    #[test]
    fn summarise_collapses_multiline_output() {
        assert_eq!(summarise(""), "ok");
        assert_eq!(summarise("one row\n"), "one row");
        assert_eq!(summarise("a\nb\nc"), "a (+2 more lines)");
    }
}

