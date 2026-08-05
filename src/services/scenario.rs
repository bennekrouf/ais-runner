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
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::services::{azurite_client, cosmos_query, sb_amqp, sb_testing, sql_runner, workflows};

/// Where scenarios live, relative to the selected project root.
pub const SCENARIO_DIR: &str = ".ais-runner/scenarios";

/// Ordered so a re-save produces a stable diff. `vars` used to be a `HashMap`,
/// which shuffled key order on every write and made an otherwise no-op save
/// look like a change in review.
pub type Vars = BTreeMap<String, String>;

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
    pub vars: Vars,
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
    /// `file` is resolved against the project root when it is relative, so a
    /// recorded upload can point at `.ais-runner/fixtures/…` and still replay on
    /// a machine that has never seen the original file. An absolute path is
    /// honoured as-is, which is what hand-written scenarios have always used.
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
    /// Azurite has no real folders, so this copies every blob under `from/` to
    /// `to/` and deletes the originals — same as the Blobs tab's rename.
    RenameFolder {
        container: String,
        from: String,
        to: String,
    },
    /// Write a blob back out to disk. `dest` follows the same
    /// relative-to-project-root rule as `UploadFile`.
    DownloadBlob {
        container: String,
        blob_name: String,
        dest: String,
    },

    // ── Service Bus ─────────────────────────────────────────────────────
    /// Only writes `Config.json`; the emulator has to restart before the queue
    /// exists. See [`queues_to_create`] — the caller is expected to apply these
    /// and restart once *before* the run, not to rely on this step at replay
    /// time.
    CreateQueue {
        queue: String,
    },
    SendMessage {
        queue: String,
        body: String,
        /// Decides how a Logic Apps SB trigger delivers the body:
        /// `application/json` arrives raw in `contentData`, anything else
        /// arrives base64-wrapped in `contentData.$content`. Defaulted so
        /// scenarios written before this field existed keep working.
        #[serde(default = "default_content_type")]
        content_type: String,
    },
    DrainQueue {
        queue: String,
    },

    // ── SQL ─────────────────────────────────────────────────────────────
    CreateSqlDatabase {
        name: String,
    },
    DropSqlDatabase {
        name: String,
    },
    /// `GO`-separated scripts are split by `sql_runner::run_sql` itself.
    RunSql {
        database: String,
        sql: String,
        #[serde(default)]
        capture: Option<String>,
    },
    TruncateTable {
        database: String,
        schema: String,
        table: String,
    },
    DropTable {
        database: String,
        schema: String,
        table: String,
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
fn default_content_type() -> String {
    "application/json".to_string()
}

/// Everything a run needs that the scenario file deliberately doesn't hardcode,
/// so the same scenario works against whatever emulators are currently up.
#[derive(Clone, Debug)]
pub struct RunContext {
    pub sb_host: String,
    pub cosmos_endpoint: String,
    pub cosmos_key: String,
    /// Base for relative `UploadFile`/`DownloadBlob` paths. Deliberately not in
    /// the JSON: the same scenario has to work from whatever checkout it's in.
    pub project_root: PathBuf,
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

pub fn scenario_dir(project_root: &Path) -> PathBuf {
    project_root.join(SCENARIO_DIR)
}

/// Filename-safe form of a scenario name: `Invoice → SAP (happy path)` becomes
/// `invoice-sap-happy-path`.
pub fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.trim().to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "scenario".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Path for a new scenario called `name`, never one that already exists.
///
/// Two scenarios can legitimately share a slug (`Invoice v1` / `Invoice: v1`),
/// and silently overwriting one with the other would lose work.
pub fn unique_path(project_root: &Path, name: &str) -> PathBuf {
    unique_in(&scenario_dir(project_root), name)
}

fn unique_in(dir: &Path, name: &str) -> PathBuf {
    let base = slug(name);
    let mut path = dir.join(format!("{base}.json"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{base}-{n}.json"));
        n += 1;
    }
    path
}

/// Write `scenario` to its `source`, creating the directory if needed.
///
/// Pretty-printed with a trailing newline because these files are committed and
/// read in diffs, not just by this app.
pub fn save(scenario: &Scenario) -> Result<(), String> {
    if scenario.source.as_os_str().is_empty() {
        return Err("scenario has no source path".to_string());
    }
    if let Some(parent) = scenario.source.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut text = serde_json::to_string_pretty(scenario).map_err(|e| e.to_string())?;
    text.push('\n');
    std::fs::write(&scenario.source, text).map_err(|e| e.to_string())
}

pub fn delete(scenario: &Scenario) -> Result<(), String> {
    std::fs::remove_file(&scenario.source).map_err(|e| e.to_string())
}

/// Rename in place: change the name and move the file to match the new slug.
///
/// The move is skipped when the slug is unchanged, so re-saving under a cosmetic
/// edit (`invoice` → `Invoice`) doesn't churn the filename in git.
pub fn rename(scenario: &Scenario, new_name: &str) -> Result<Scenario, String> {
    let mut renamed = scenario.clone();
    renamed.name = new_name.trim().to_string();

    let old_stem = scenario
        .source
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    if slug(&renamed.name) != old_stem {
        let dir = scenario.source.parent().unwrap_or(Path::new("."));
        renamed.source = unique_in(dir, &renamed.name);
    }

    // Save first, delete second: interrupted between the two this leaves a
    // duplicate, which the user can see and remove. The other order would lose
    // the scenario outright.
    save(&renamed)?;
    if renamed.source != scenario.source {
        let _ = std::fs::remove_file(&scenario.source);
    }
    Ok(renamed)
}

/// Queue names a scenario creates, with `{{var}}` already expanded.
///
/// `sb_emulator::add_queue_to_emulator_config` only edits `Config.json`; the
/// emulator has to restart before the queue is real. Replaying `CreateQueue`
/// inline would therefore produce a scenario whose very next send fails. The
/// Tests view instead applies all of these up front and restarts once, which
/// costs one restart no matter how many queues a scenario declares.
pub fn queues_to_create(scenario: &Scenario) -> Vec<String> {
    scenario
        .steps
        .iter()
        .filter_map(|s| match s {
            Step::CreateQueue { queue } => Some(expand(queue, &scenario.vars)),
            _ => None,
        })
        .filter(|q| !q.trim().is_empty())
        .collect()
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
    vars: Vars,
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
            let (c, b) = (container.clone(), blob_name.clone());
            let f = resolve_path(&ctx.project_root, file);
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
        RenameFolder {
            container,
            from,
            to,
        } => {
            let (c, f, t) = (container.clone(), from.clone(), to.clone());
            let moved = blocking(move || azurite_client::rename_virtual_folder(&c, &f, &t)).await?;
            Ok(format!("renamed '{from}' → '{to}' ({moved} blob(s) moved)"))
        }
        DownloadBlob {
            container,
            blob_name,
            dest,
        } => {
            let (c, b) = (container.clone(), blob_name.clone());
            let path = resolve_path(&ctx.project_root, dest);
            // The destination folder may not exist in a fresh checkout.
            if let Some(parent) = Path::new(&path).parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let shown = path.clone();
            blocking(move || azurite_client::download_blob(&c, &b, &path)).await?;
            Ok(format!("downloaded '{blob_name}' to {shown}"))
        }

        // ── Service Bus ─────────────────────────────────────────────────
        CreateQueue { queue } => {
            let q = queue.clone();
            let added =
                blocking(move || crate::handlers::sb_emulator::add_queue_to_emulator_config(&q))
                    .await?;
            Ok(if added {
                format!("'{queue}' written to Config.json — needs an emulator restart to exist")
            } else {
                format!("queue '{queue}' already in Config.json")
            })
        }
        SendMessage {
            queue,
            body,
            content_type,
        } => {
            sb_amqp::send_amqp_message_with_type(&ctx.sb_host, queue, body, content_type).await?;
            Ok(format!(
                "sent {} bytes to '{queue}' as {content_type}",
                body.len()
            ))
        }
        DrainQueue { queue } => {
            let n = sb_amqp::drain_queue(&ctx.sb_host, queue).await?;
            Ok(format!("drained {n} message(s) from '{queue}'"))
        }

        // ── SQL ─────────────────────────────────────────────────────────
        CreateSqlDatabase { name } => sql_runner::create_database(name).await,
        DropSqlDatabase { name } => {
            sql_runner::drop_database(name).await?;
            Ok(format!("dropped database '{name}'"))
        }
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
        TruncateTable {
            database,
            schema,
            table,
        } => {
            sql_runner::truncate_table(database, schema, table).await?;
            Ok(format!("truncated {schema}.{table}"))
        }
        DropTable {
            database,
            schema,
            table,
        } => {
            sql_runner::drop_table(database, schema, table).await?;
            Ok(format!("dropped {schema}.{table}"))
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

/// Make a recorded file path absolute.
///
/// Recorded steps store fixtures relative to the project root so a scenario
/// stays replayable in someone else's checkout; hand-written ones have always
/// used absolute paths, and those are left alone.
fn resolve_path(project_root: &Path, path: &str) -> String {
    let p = Path::new(path);
    if p.is_absolute() {
        path.to_string()
    } else {
        project_root.join(p).to_string_lossy().into_owned()
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
        RenameFolder { from, to, .. } => format!("rename folder {from} → {to}"),
        DownloadBlob { blob_name, .. } => format!("download {blob_name}"),
        CreateQueue { queue } => format!("create queue {queue}"),
        SendMessage { queue, .. } => format!("send to {queue}"),
        DrainQueue { queue } => format!("drain {queue}"),
        CreateSqlDatabase { name } => format!("create SQL db {name}"),
        DropSqlDatabase { name } => format!("drop SQL db {name}"),
        RunSql { database, .. } => format!("run SQL on {database}"),
        TruncateTable { schema, table, .. } => format!("truncate {schema}.{table}"),
        DropTable { schema, table, .. } => format!("drop table {schema}.{table}"),
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
fn resolve_vars(step: &Step, vars: &Vars) -> Result<Step, String> {
    if vars.is_empty() {
        return Ok(step.clone());
    }
    let mut value = serde_json::to_value(step).map_err(|e| e.to_string())?;
    substitute(&mut value, vars);
    serde_json::from_value(value).map_err(|e| format!("substitution produced an invalid step: {e}"))
}

fn substitute(value: &mut Value, vars: &Vars) {
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
fn expand(text: &str, vars: &Vars) -> String {
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

    fn vars(pairs: &[(&str, &str)]) -> Vars {
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
            content_type: default_content_type(),
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
            { "action": "rename_folder", "container": "in", "from": "old", "to": "new" },
            { "action": "download_blob", "container": "in", "blob_name": "a.xml", "dest": "out/a.xml" },
            { "action": "create_queue", "queue": "q" },
            { "action": "send_message", "queue": "q", "body": "{}" },
            { "action": "drain_queue", "queue": "q" },
            { "action": "create_sql_database", "name": "aisdev" },
            { "action": "drop_sql_database", "name": "aisdev" },
            { "action": "run_sql", "database": "aisdev", "sql": "SELECT 1", "capture": "out" },
            { "action": "truncate_table", "database": "aisdev", "schema": "dbo", "table": "Invoice" },
            { "action": "drop_table", "database": "aisdev", "schema": "dbo", "table": "Invoice" },
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
        assert_eq!(scenario.steps.len(), 24);

        // Defaults applied where the document omitted them.
        let Some(Step::CreateCosmosContainer { partition_key, .. }) = scenario
            .steps
            .iter()
            .find(|s| matches!(s, Step::CreateCosmosContainer { .. }))
        else {
            panic!("missing variant")
        };
        assert_eq!(partition_key, "/id");

        // Re-serializing and re-parsing must preserve every step.
        let text = serde_json::to_string(&scenario).unwrap();
        let again: Scenario = serde_json::from_str(&text).unwrap();
        assert_eq!(again.steps.len(), scenario.steps.len());
    }

    #[test]
    fn a_send_message_written_before_content_type_existed_still_loads() {
        // The field was added after scenarios were already in projects; without
        // the serde default every one of those files would fail to parse.
        let json = r#"{ "action": "send_message", "queue": "q", "body": "{}" }"#;
        let Step::SendMessage { content_type, .. } = serde_json::from_str(json).unwrap() else {
            panic!("wrong variant")
        };
        assert_eq!(content_type, "application/json");
    }

    #[test]
    fn slug_produces_one_filename_safe_token_per_word() {
        assert_eq!(slug("Invoice → SAP (happy path)"), "invoice-sap-happy-path");
        assert_eq!(slug("  spaced  out  "), "spaced-out");
        assert_eq!(slug("already-fine"), "already-fine");
        // Nothing usable left — still needs a filename.
        assert_eq!(slug("→→→"), "scenario");
        assert_eq!(slug(""), "scenario");
    }

    #[test]
    fn relative_fixture_paths_resolve_against_the_project_root() {
        let root = Path::new("/repo/app");
        assert_eq!(
            resolve_path(root, ".ais-runner/fixtures/a.xml"),
            "/repo/app/.ais-runner/fixtures/a.xml"
        );
        // Hand-written scenarios have always used absolute paths.
        assert_eq!(resolve_path(root, "/tmp/a.xml"), "/tmp/a.xml");
    }

    #[test]
    fn queues_to_create_collects_them_with_variables_expanded() {
        let json = r#"{
          "name": "q",
          "vars": { "env": "dev" },
          "steps": [
            { "action": "create_queue", "queue": "ais.{{env}}.in" },
            { "action": "send_message", "queue": "ais.{{env}}.in", "body": "{}" },
            { "action": "create_queue", "queue": "ais.{{env}}.out" }
          ]
        }"#;
        let scenario: Scenario = serde_json::from_str(json).unwrap();
        assert_eq!(
            queues_to_create(&scenario),
            vec!["ais.dev.in".to_string(), "ais.dev.out".to_string()]
        );
    }

    #[test]
    fn renaming_moves_the_file_and_leaves_nothing_behind() {
        let dir = std::env::temp_dir().join(format!("ais-rn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let original = Scenario {
            name: "Before".into(),
            description: String::new(),
            vars: Default::default(),
            steps: vec![Step::DrainQueue { queue: "q".into() }],
            source: unique_in(&dir, "Before"),
        };
        save(&original).unwrap();

        let renamed = rename(&original, "After Rename").unwrap();
        assert_eq!(renamed.name, "After Rename");
        assert_eq!(renamed.source, dir.join("after-rename.json"));
        assert!(renamed.source.exists());
        assert!(!original.source.exists(), "the old file must not linger");

        // A cosmetic change that leaves the slug alone keeps the filename, so a
        // rename doesn't churn the path in git for nothing.
        let again = rename(&renamed, "after-rename").unwrap();
        assert_eq!(again.source, renamed.source);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn saving_and_reloading_a_scenario_preserves_every_step() {
        let dir = std::env::temp_dir().join(format!("ais-scn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let scenario = Scenario {
            name: "Round Trip".into(),
            description: "d".into(),
            vars: vars(&[("id", "1")]),
            steps: vec![
                Step::CreateQueue { queue: "q".into() },
                Step::SendMessage {
                    queue: "q".into(),
                    body: "{}".into(),
                    content_type: "application/octet-stream".into(),
                },
            ],
            source: unique_in(&dir, "Round Trip"),
        };
        save(&scenario).unwrap();

        let reloaded = load(&scenario.source).unwrap();
        assert_eq!(reloaded.name, "Round Trip");
        assert_eq!(reloaded.steps.len(), 2);
        let Step::SendMessage { content_type, .. } = &reloaded.steps[1] else {
            panic!("wrong variant")
        };
        assert_eq!(content_type, "application/octet-stream");

        std::fs::remove_dir_all(&dir).ok();
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

