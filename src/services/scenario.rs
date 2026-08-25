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

use std::net::ToSocketAddrs;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::services::{
    azurite_client, cosmos_query, sb_amqp, sb_testing, settings_file, sql_runner, workflows,
};

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
    /// Non-destructive assertion — unlike `clear_container`/`upload_file`, this
    /// never changes emulator state.
    CheckBlobExists {
        container: String,
        blob_name: String,
        #[serde(default = "default_true")]
        exists: bool,
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
        /// When set, invoke the workflow's own HTTP endpoint and capture its
        /// synchronous response body as `{{name}}` instead of firing it via
        /// the management API's fire-and-forget trigger-run endpoint. For an
        /// HTTP-triggered workflow whose `Response` action is the thing under
        /// test — e.g. a routing resolver — rather than one whose side
        /// effects are checked by a later step.
        #[serde(default)]
        capture: Option<String>,
        /// The trigger call itself is expected to come back as an HTTP error —
        /// not because firing it failed, but because the workflow's own logic
        /// terminates the run (e.g. `runStatus: "Cancelled"` on invalid input),
        /// which kills any pending `Response` action before it can answer the
        /// caller. That's normal when the workflow is invoked directly over
        /// HTTP for a case designed to be reached via the internal
        /// `Workflow`-invoke mechanism instead, where a terminated child run
        /// is a status, not a broken connection. Set this so the step still
        /// records the run for a later `wait_for_run` to check, rather than
        /// failing here on a response that was never coming.
        #[serde(default)]
        expect_trigger_error: bool,
    },

    // ── External process ──────────────────────────────────────────────────
    /// Start a helper process for the duration of the scenario — typically a
    /// stub server standing in for an API the workflows call.
    ///
    /// Needed because not every external dependency can be intercepted by the
    /// built-in mock: that one rewrites URL-shaped *app settings*, so a
    /// workflow whose base URL arrives in the message payload (or is built from
    /// `variables(...)`) never routes through it. A stub the scenario starts
    /// itself has no such constraint.
    ///
    /// Runs the command as-is, with no approval prompt — deliberately, because
    /// the toolbar's own `func start` and `mvn package` already execute whatever
    /// code the opened workspace contains. Gating only this step would add
    /// friction to the one path that states its command in plain sight (in a
    /// reviewable JSON file, echoed into the run log) while leaving the broader
    /// ones open. Visibility is the control here, not a permission.
    RunProcess {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        /// Relative paths resolve against the project root, same rule as
        /// `UploadFile`, so a scenario replays in someone else's checkout.
        #[serde(default)]
        workdir: Option<String>,
        #[serde(default)]
        env: Vars,
        /// Wait until something is listening here before the step succeeds.
        /// Without it the next step races the process's startup — the usual
        /// symptom being a first request that connection-refuses while the
        /// stub is still binding.
        #[serde(default)]
        wait_for_port: Option<u16>,
        #[serde(default = "default_port_wait")]
        wait_timeout_ms: u64,
        /// Kill when the scenario ends. Left on by default: a stub that
        /// outlives the run holds its port, and the next run fails with a
        /// bind error that points nowhere near the real cause.
        #[serde(default = "default_true")]
        stop_at_end: bool,
    },

    // ── Local settings ────────────────────────────────────────────────────
    /// Write key/value pairs into `local.settings.json`, snapshotting whatever
    /// each key held before (or its absence) so `restore_settings` — or an
    /// automatic restore after a later step fails — can put it back exactly.
    ///
    /// Logic Apps Standard reads this file only at host startup, so a setting
    /// changed here has no effect until a `restart_func` step follows it.
    SetSettings {
        values: Vars,
    },
    /// Put back every key touched by `set_settings` since the last restore (or
    /// since the scenario started). A no-op, not an error, when nothing was
    /// snapshotted — safe to include defensively at the end of a scenario even
    /// if an earlier step already triggered the automatic restore.
    RestoreSettings,
    /// Stop and restart the func host, then wait for its workflows to
    /// re-register. Only meaningful after `set_settings`, which otherwise sits
    /// in `local.settings.json` unread by an already-running host.
    RestartFunc {
        #[serde(default = "default_func_restart_timeout")]
        timeout_ms: u64,
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
    /// Fails if that run finishes in any terminal state other than
    /// `expect_status` — a workflow whose failure path is itself the thing
    /// under test (e.g. "no record found" should reach `Failed`) sets that
    /// explicitly; everything else defaults to `Succeeded`.
    WaitForRun {
        workflow: String,
        #[serde(default = "default_timeout")]
        timeout_ms: u64,
        #[serde(default = "default_expect_status")]
        expect_status: String,
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
    /// Assert on one action inside a workflow run: its status, and optionally
    /// that its inputs/outputs contain `contains`.
    ///
    /// The way to verify a message a live consumer would otherwise eat. A queue
    /// with its own triggered workflow (`ais.teams.notif` → `AIS-GenericNotif`)
    /// can't be observed with `wait_for_message`: the consumer peek-locks each
    /// message within a second and holds it for minutes, so a scenario polling
    /// the queue sees nothing whether the consumer succeeded or failed. The
    /// producing action's recorded inputs are not a race — they're durable run
    /// history, and they carry the payload that was actually sent.
    ///
    /// Targets the run a preceding `wait_for_run` on the same workflow matched;
    /// otherwise the most recent run started since the scenario began.
    ExpectAction {
        workflow: String,
        /// Not `action`: that key is the enum's own serde tag.
        action_name: String,
        #[serde(default = "default_expect_status")]
        status: String,
        #[serde(default)]
        contains: Option<String>,
        #[serde(default = "default_timeout")]
        timeout_ms: u64,
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
fn default_true() -> bool {
    true
}
fn default_expect_status() -> String {
    "Succeeded".to_string()
}
fn default_port_wait() -> u64 {
    // A local stub binds in well under a second; this is a "something is
    // wrong" ceiling, not an expected wait.
    15_000
}
fn default_func_restart_timeout() -> u64 {
    // Cold func start plus workflow re-registration routinely takes over a
    // minute; the shared default_timeout() (30s) is tuned for emulator polls,
    // not a process restart.
    180_000
}

/// Future returned by [`RestartFn`] — boxed because the concrete future
/// depends on Dioxus `Signal`s the closure captures, which `scenario.rs`
/// itself knows nothing about.
///
/// Not `Send`: a `Signal` is backed by a thread-local `RefCell` and can't
/// cross threads. That's fine — `scenario::run` is driven by Dioxus's own
/// (single-threaded) `spawn` from the Tests view, never `tokio::spawn`.
pub type BoxFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>>>>;

/// Stops and restarts the func host, waits for its workflows to re-register,
/// and reports a short summary — or an error.
///
/// A plain callback rather than baking Dioxus signals into `RunContext`:
/// restarting func needs half a dozen of `MainContext`'s signals
/// (`azurite_state`, `func_state`, `func_proc`, `workflows`, `traced_wfs`,
/// `cleared_wfs`, `log_lines`), and this module has no Dioxus dependency
/// otherwise. The Tests view, which does have them, supplies the closure.
pub type RestartFn = std::sync::Arc<dyn Fn() -> BoxFuture>;

/// Everything a run needs that the scenario file deliberately doesn't hardcode,
/// so the same scenario works against whatever emulators are currently up.
#[derive(Clone)]
pub struct RunContext {
    pub sb_host: String,
    pub cosmos_endpoint: String,
    pub cosmos_key: String,
    /// Base for relative `UploadFile`/`DownloadBlob` paths. Deliberately not in
    /// the JSON: the same scenario has to work from whatever checkout it's in.
    pub project_root: PathBuf,
    /// `None` when the caller can't restart func (only the Tests view can) —
    /// a `restart_func` step then fails with a clear message instead of
    /// panicking on a missing callback.
    pub restart_func: Option<RestartFn>,
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

/// Read every scenario under `<project_root>/.ais-runner/scenarios`, including
/// subfolders.
///
/// A subfolder is how a scenario gets grouped in the UI — see [`group_of`] —
/// so a user organizes a suite by moving files into one with a file manager
/// or their editor; there's no separate "group" concept to keep in sync.
///
/// A malformed file is reported but doesn't hide the rest — one bad scenario
/// shouldn't make the whole panel look empty.
pub fn discover(project_root: &Path) -> (Vec<Scenario>, Vec<String>) {
    let dir = project_root.join(SCENARIO_DIR);
    let mut scenarios = Vec::new();
    let mut errors = Vec::new();
    walk(&dir, &mut scenarios, &mut errors);

    // Group first (root scenarios — no group — sort first within that), then
    // name, so the UI can render in this order directly without re-sorting.
    scenarios.sort_by(|a, b| {
        let ga = group_of(project_root, a);
        let gb = group_of(project_root, b);
        ga.cmp(&gb).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    (scenarios, errors)
}

fn walk(dir: &Path, scenarios: &mut Vec<Scenario>, errors: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, scenarios, errors);
        } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
            match load(&path) {
                Ok(s) => scenarios.push(s),
                Err(e) => errors.push(format!("{}: {e}", path.display())),
            }
        }
    }
}

/// The scenario's group — its path relative to `.ais-runner/scenarios`, minus
/// the filename — or `None` for one saved directly at the top level.
///
/// Purely a filesystem read: there's no group field in the scenario file
/// itself, so renaming a folder or moving a file regroups it with no other
/// bookkeeping to update.
pub fn group_of(project_root: &Path, scenario: &Scenario) -> Option<String> {
    let rel = scenario.source.strip_prefix(scenario_dir(project_root)).ok()?;
    let parent = rel.parent()?;
    if parent.as_os_str().is_empty() {
        None
    } else {
        Some(parent.to_string_lossy().replace(std::path::MAIN_SEPARATOR, " / "))
    }
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

/// Which local services a scenario actually needs, derived from its steps.
///
/// Probed before the run so a stopped emulator reports itself, instead of the
/// first step that needs it failing with a raw transport error.
fn required_services(scenario: &Scenario, ctx: &RunContext) -> Vec<(&'static str, String, String)> {
    let (mut blob, mut queue, mut sql, mut func, mut cosmos) = (false, false, false, false, false);
    for step in &scenario.steps {
        match step {
            Step::CreateContainer { .. } | Step::ClearContainer { .. }
            | Step::UploadFile { .. } | Step::UploadInline { .. }
            | Step::DownloadBlob { .. } | Step::CheckBlobExists { .. }
            | Step::RenameFolder { .. } => blob = true,
            Step::CreateQueue { .. } | Step::SendMessage { .. } | Step::DrainQueue { .. }
            | Step::Expect { .. } | Step::WaitForMessage { .. } => queue = true,
            Step::CreateSqlDatabase { .. } | Step::DropSqlDatabase { .. } | Step::RunSql { .. }
            | Step::WaitForSql { .. } | Step::DropTable { .. } | Step::TruncateTable { .. } => sql = true,
            Step::RunWorkflow { .. } | Step::WaitForRun { .. } | Step::ExpectAction { .. } => func = true,
            Step::CreateCosmosDatabase { .. } | Step::CreateCosmosContainer { .. }
            | Step::UpsertCosmosDocument { .. } | Step::RunCosmosQuery { .. } => cosmos = true,
            _ => {}
        }
    }
    let mut needed = Vec::new();
    if blob {
        needed.push(("Azurite (blob)", "127.0.0.1:10000".to_string(),
            workflows::AZURITE_RESET_HINT.to_string()));
    }
    if func {
        // func keeps run history in Storage Tables; without 10002 it answers 503
        // for 30s and then dies, which looks like every later step failing.
        needed.push(("Azurite (table)", "127.0.0.1:10002".to_string(),
            workflows::AZURITE_RESET_HINT.to_string()));
    }
    if queue {
        needed.push(("Service Bus emulator", format!("{}:5672", host_only(&ctx.sb_host)),
            "Start the Service Bus emulator from the toolbar.".to_string()));
    }
    if func {
        needed.push(("Logic Apps runtime (func)", "127.0.0.1:7071".to_string(),
            "Start func from the toolbar — no workflow can run without it.".to_string()));
    }
    if sql {
        needed.push(("SQL Server", "127.0.0.1:1433".to_string(),
            "Start the SQL Server container from the toolbar.".to_string()));
    }
    if cosmos {
        needed.push(("Cosmos emulator", host_port(&ctx.cosmos_endpoint, 8081),
            "Start the Cosmos emulator.".to_string()));
    }
    needed
}

fn host_only(host: &str) -> String {
    host.trim_start_matches("http://").trim_start_matches("https://")
        .split('/').next().unwrap_or(host)
        .split(':').next().unwrap_or(host).to_string()
}

fn host_port(endpoint: &str, default_port: u16) -> String {
    let bare = endpoint.trim_start_matches("http://").trim_start_matches("https://");
    let bare = bare.split('/').next().unwrap_or(bare);
    if bare.contains(':') { bare.to_string() } else { format!("{bare}:{default_port}") }
}

/// Services the scenario needs that are not accepting connections.
pub fn unavailable_services(scenario: &Scenario, ctx: &RunContext) -> Vec<String> {
    let mut down = Vec::new();
    for (label, addr, hint) in required_services(scenario, ctx) {
        let reachable = addr
            .to_socket_addrs()
            .ok()
            .and_then(|mut a| a.next())
            .is_some_and(|a| {
                std::net::TcpStream::connect_timeout(&a, std::time::Duration::from_millis(700))
                    .is_ok()
            });
        if !reachable {
            down.push(format!("{label} is not reachable on {addr} — {hint}"));
        }
    }
    down
}

/// Assertions that can never pass, found before any setup runs.
///
/// An `expect_action` naming an action the workflow does not define fails only
/// after its timeout, and blames "has not run yet" — so a scenario left stale by
/// a refactor burns the whole setup first and then points at the wrong thing.
/// Reported together so one pass fixes them all.
pub fn stale_assertions(scenario: &Scenario, project_root: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for step in &scenario.steps {
        let Step::ExpectAction { workflow, action_name, .. } = step else { continue };
        let workflow = expand(workflow, &scenario.vars);
        let action = expand(action_name, &scenario.vars);
        let Some(defined) = workflows::definition_action_names(project_root, &workflow) else {
            continue; // no readable definition — cannot tell, so do not guess
        };
        if !defined.contains(action.as_str()) {
            let elsewhere = workflows::workflows_containing_action(project_root, &action);
            let hint = match elsewhere.as_slice() {
                [] => String::new(),
                others => format!(" — it exists in {}", others.join(", ")),
            };
            problems.push(format!("workflow '{workflow}' has no action named '{action}'{hint}"));
        }
    }
    problems.sort();
    problems.dedup();
    problems
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
        settings_snapshot: std::collections::HashMap::new(),
        processes: Vec::new(),
        last_run: std::collections::HashMap::new(),
    };
    let mut results = Vec::new();
    let mut aborted = false;

    // Fail before any container, queue or database is touched.
    let down = unavailable_services(scenario, ctx);
    if !down.is_empty() {
        let result = StepResult {
            index: 0,
            label: "check local services".to_string(),
            status: StepStatus::Failed,
            detail: format!("{} service(s) down:\n  - {}", down.len(), down.join("\n  - ")),
            elapsed_ms: 0,
        };
        on_step(result.clone());
        return vec![result];
    }

    let stale = stale_assertions(scenario, &ctx.project_root.to_string_lossy());
    if !stale.is_empty() {
        let result = StepResult {
            index: 0,
            label: "validate scenario assertions".to_string(),
            status: StepStatus::Failed,
            detail: format!(
                "{} assertion(s) can never pass:\n  - {}",
                stale.len(),
                stale.join("\n  - ")
            ),
            elapsed_ms: 0,
        };
        on_step(result.clone());
        return vec![result];
    }

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
        // Built-ins are recomputed fresh every step (so `{{NOW_UTC}}` etc. are
        // never stale) and layered under the scenario/captured vars, which win
        // on a name clash. Substituted per-step rather than up front, so a
        // step can consume a variable captured by the step before it.
        let mut lookup = builtin_vars();
        lookup.extend(state.vars.clone());
        let outcome = match resolve_vars(step, &lookup) {
            Ok(resolved) => exec(&resolved, ctx, &mut state).await,
            Err(e) => Err(e),
        };
        let elapsed_ms = started.elapsed().as_millis();

        let result = match outcome {
            Ok(detail) => {
                // Available to the next step as `{{PREV_STEP_RESULT}}` without
                // that step needing an explicit `capture` field.
                state.vars.insert("PREV_STEP_RESULT".to_string(), detail.clone());
                StepResult {
                    index,
                    label: label_of(step),
                    status: StepStatus::Ok,
                    detail,
                    elapsed_ms,
                }
            }
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

        // A failure mid-scenario must not leave local.settings.json holding
        // values a set_settings step put there for this run only — restore
        // even though the scenario's own restore_settings step, if any, is
        // about to be skipped like every other remaining step.
        if aborted && !state.settings_snapshot.is_empty() {
            let restore_started = std::time::Instant::now();
            let outcome = restore_settings_now(ctx, &mut state).await;
            let elapsed_ms = restore_started.elapsed().as_millis();
            let auto_result = match outcome {
                Ok(detail) => StepResult {
                    index: results.len(),
                    label: "auto-restore settings after failure".to_string(),
                    status: StepStatus::Ok,
                    detail,
                    elapsed_ms,
                },
                Err(detail) => StepResult {
                    index: results.len(),
                    label: "auto-restore settings after failure".to_string(),
                    status: StepStatus::Failed,
                    detail,
                    elapsed_ms,
                },
            };
            on_step(auto_result.clone());
            results.push(auto_result);
        }
    }

    // Helper processes are torn down however the scenario ended — success,
    // failure, or abort. A stub left holding its port makes the *next* run fail
    // with a bind error that points nowhere near the real cause.
    let stop_started = std::time::Instant::now();
    let stopped = stop_processes(&mut state);
    if !stopped.is_empty() {
        let teardown = StepResult {
            index: results.len(),
            label: format!("stop {} helper process(es)", stopped.len()),
            status: StepStatus::Ok,
            detail: stopped.join(", "),
            elapsed_ms: stop_started.elapsed().as_millis(),
        };
        on_step(teardown.clone());
        results.push(teardown);
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
    /// Original value of every `local.settings.json` key touched by
    /// `set_settings` since the last restore (or since the scenario started).
    /// `None` means the key didn't exist before and should be removed, not
    /// blanked, on restore. First write per key wins, so two `set_settings`
    /// steps in a row still roll back to the true original rather than the
    /// intermediate value.
    settings_snapshot: std::collections::HashMap<String, Option<String>>,
    /// Helper processes started by `run_process`, in start order. Torn down by
    /// `run()` when the scenario ends — however it ends.
    processes: Vec<SpawnedProcess>,
    /// workflow → the run name the most recent `WaitForRun` matched.
    ///
    /// `ExpectAction` inspects *that* run rather than re-resolving "the latest
    /// one". A workflow whose trigger keeps redelivering (an uncompleted
    /// peek-lock message, say) can start another run between the wait and the
    /// assertion, and asserting against a different run than the one just
    /// verified is a race that only shows up intermittently.
    last_run: std::collections::HashMap<String, String>,
}

/// A process started by `run_process`, held so the run can reap it at the end.
struct SpawnedProcess {
    label: String,
    child: std::process::Child,
    stop_at_end: bool,
}

/// Kill every `stop_at_end` process, most recent first.
///
/// Best-effort by design: teardown runs after both success and failure, and a
/// process that already exited on its own is the normal case, not an error
/// worth failing an otherwise-green scenario over.
fn stop_processes(state: &mut RunState) -> Vec<String> {
    let mut stopped = Vec::new();
    // Reverse order so a stub that depends on another outlives it, mirroring
    // how the scenario started them.
    for mut proc in state.processes.drain(..).rev() {
        if !proc.stop_at_end {
            continue;
        }
        match proc.child.try_wait() {
            // Already gone — nothing to kill, but say so: a stub that exited
            // early is usually why the steps after it failed.
            Ok(Some(status)) => stopped.push(format!("{} (already exited: {status})", proc.label)),
            _ => {
                let _ = proc.child.kill();
                let _ = proc.child.wait(); // reap, so it can't zombie
                stopped.push(proc.label);
            }
        }
    }
    stopped
}

/// Poll until something accepts a TCP connection on `port`.
async fn wait_for_port(port: u16, timeout_ms: u64, child: &mut std::process::Child) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return Ok(());
        }
        // A process that died is never going to bind — fail now with its exit
        // status rather than burning the whole timeout on a corpse.
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("process exited before binding port {port} ({status})"));
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("nothing listening on port {port} after {timeout_ms}ms"));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
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
        CheckBlobExists {
            container,
            blob_name,
            exists,
        } => {
            let (c, b) = (container.clone(), blob_name.clone());
            let found = blocking(move || {
                Ok(azurite_client::list_blobs(&c)?.iter().any(|i| i.name == b))
            })
            .await?;
            match (found, exists) {
                (true, true) => Ok(format!("'{blob_name}' exists in '{container}'")),
                (false, false) => Ok(format!("'{blob_name}' is absent from '{container}', as expected")),
                (false, true) => Err(format!("expected '{blob_name}' to exist in '{container}', but it does not")),
                (true, false) => Err(format!("expected '{blob_name}' to be absent from '{container}', but it exists")),
            }
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
            capture,
            expect_trigger_error,
        } => {
            // Tighten the floor *before* triggering: a run that starts while the
            // request is still in flight must still count.
            state.run_floor = chrono::Utc::now();
            if let Some(key) = capture {
                let url = workflows::get_callback_url(workflow, trigger).await?;
                let resp = workflows::trigger_workflow_capture_body(&url, body).await?;
                state.vars.insert(key.clone(), resp.clone());
                Ok(summarise(&resp))
            } else {
                match workflows::run_trigger_direct(workflow, trigger, body).await {
                    Ok(()) => Ok(format!("triggered '{workflow}' via '{trigger}'")),
                    Err(e) if *expect_trigger_error => Ok(format!(
                        "triggered '{workflow}' via '{trigger}' — call errored as expected ({e}); checking the run itself next"
                    )),
                    Err(e) => Err(e),
                }
            }
        }

        // ── External process ──────────────────────────────────────────────
        RunProcess {
            command, args, workdir, env, wait_for_port: port, wait_timeout_ms, stop_at_end,
        } => {
            let resolved_dir = workdir
                .as_deref()
                .map(|d| resolve_path(&ctx.project_root, d))
                .unwrap_or_else(|| ctx.project_root.display().to_string());

            let program = crate::services::process::resolve_bin(command);
            let mut cmd = std::process::Command::new(&program);
            cmd.args(args)
                .current_dir(&resolved_dir)
                // A desktop-launched app inherits a minimal PATH that often
                // lacks python3/node; rich_path() is what the service handlers
                // already use to find their binaries.
                .env("PATH", crate::services::process::rich_path())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            for (k, v) in env {
                cmd.env(k, v);
            }

            let mut child = cmd
                .spawn()
                .map_err(|e| format!("could not start '{program}': {e}"))?;

            let label = if args.is_empty() {
                command.clone()
            } else {
                format!("{command} {}", args.join(" "))
            };

            let detail = match port {
                Some(p) => {
                    // Register before awaiting, so a bind that never happens
                    // still leaves the process tracked for teardown.
                    let outcome = wait_for_port(*p, *wait_timeout_ms, &mut child).await;
                    state.processes.push(SpawnedProcess {
                        label: label.clone(), child, stop_at_end: *stop_at_end,
                    });
                    outcome?;
                    format!("started '{label}' — listening on {p}")
                }
                None => {
                    state.processes.push(SpawnedProcess {
                        label: label.clone(), child, stop_at_end: *stop_at_end,
                    });
                    format!("started '{label}'")
                }
            };
            Ok(detail)
        }

        // ── Local settings ────────────────────────────────────────────────
        SetSettings { values } => set_settings_now(ctx, state, values).await,
        RestoreSettings => restore_settings_now(ctx, state).await,
        RestartFunc { timeout_ms } => restart_func_now(ctx, *timeout_ms).await,

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
            expect_status,
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
                    Some((run_name, status)) if status == *expect_status => {
                        *winner.borrow_mut() = Some(run_name.clone());
                        Ok((true, format!("run {run_name} {status}")))
                    }
                    // Any other terminal status is final — no amount of waiting
                    // improves it, so surface it now instead of at timeout. A
                    // workflow expected to fail that instead succeeds (or
                    // vice versa) is exactly as wrong as one that errors.
                    Some((run_name, status)) => {
                        Err(format!("run {run_name} finished {status}, expected {expect_status}"))
                    }
                    None => Ok((false, "no terminal run yet".to_string())),
                }
            })
            .await?;

            if let Some(name) = winner.into_inner() {
                // Remembered so a following `expect_action` inspects this exact
                // run rather than whatever is newest by then.
                state.last_run.insert(workflow.clone(), name.clone());
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
        ExpectAction {
            workflow,
            action_name: action,
            status,
            contains,
            timeout_ms,
        } => {
            let run_id = match state.last_run.get(workflow) {
                Some(id) => id.clone(),
                None => latest_run_since(workflow, state.run_floor)
                    .await
                    .ok_or_else(|| format!("no run of '{workflow}' found since the scenario started"))?,
            };

            // An action absent from the definition will never run, so polling for
            // it just burns the timeout and blames "has not run yet". Check the
            // definition first and say what is actually wrong.
            let root = ctx.project_root.to_string_lossy().into_owned();
            if let Some(defined) = workflows::definition_action_names(&root, workflow) {
                if !defined.contains(action.as_str()) {
                    let elsewhere = workflows::workflows_containing_action(&root, action);
                    let hint = match elsewhere.as_slice() {
                        [] => String::new(),
                        others => format!(" — it exists in {}", others.join(", ")),
                    };
                    return Err(format!(
                        "workflow '{workflow}' has no action named '{action}'{hint}"
                    ));
                }
            }

            // Poll: the action may not have executed yet while the run is still
            // in flight.
            let found: std::cell::RefCell<Option<workflows::ActionPayload>> = Default::default();
            poll_until(*timeout_ms, || async {
                match workflows::action_payload(workflow, &run_id, action).await {
                    Ok(p) if p.status == "Unknown" => {
                        // "not yet" is only true while the run is in flight. Once
                        // it has finished, the action was skipped or never reached,
                        // and saying "not yet" sends people to the wrong place.
                        let detail = match workflows::run_status(workflow, &run_id).await {
                            Some(s) if s == "Running" || s == "Waiting" => {
                                format!("action '{action}' has not run yet (run {run_id} is {s})")
                            }
                            Some(s) => format!(
                                "run {run_id} finished as {s} without reaching action '{action}' \
                                 — it was skipped, or its branch was not taken"
                            ),
                            None => format!("action '{action}' has not run yet"),
                        };
                        Ok((false, detail))
                    }
                    Ok(p) => {
                        let detail = format!("action '{action}' is {}", p.status);
                        *found.borrow_mut() = Some(p);
                        Ok((true, detail))
                    }
                    Err(e) => Ok((false, format!("action '{action}' not readable yet: {e}"))),
                }
            })
            .await?;

            let payload = found
                .into_inner()
                .ok_or_else(|| format!("action '{action}' never reported a status"))?;

            if payload.status != *status {
                return Err(format!(
                    "action '{action}' in run {run_id} is {}, expected {status}",
                    payload.status
                ));
            }
            if let Some(needle) = contains {
                if !payload.text.contains(needle.as_str()) {
                    return Err(format!(
                        "action '{action}' is {status} but its inputs/outputs do not contain '{needle}'"
                    ));
                }
                return Ok(format!("action '{action}' {status}, and contains '{needle}'"));
            }
            Ok(format!("action '{action}' {status}"))
        }
    }
}

/// Most recent run of `workflow` started at or after `floor`, in any state.
///
/// Unlike `latest_terminal_run` this doesn't require a terminal status: an
/// `expect_action` may legitimately inspect an action that has already
/// completed inside a run that is still going.
async fn latest_run_since(
    workflow: &str,
    floor: chrono::DateTime<chrono::Utc>,
) -> Option<String> {
    let runs = workflows::list_runs(workflow).await.ok()?;
    runs.iter()
        .filter(|r| started_at_or_after(r.properties.start_time.as_deref(), floor))
        .max_by_key(|r| r.properties.start_time.clone())
        .map(|r| r.name.clone())
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

/// Write `values` into `local.settings.json`, snapshotting whatever each key
/// held before into `state.settings_snapshot`.
///
/// First write per key wins: if a key is already snapshotted from an earlier
/// `set_settings` in this run (no restore in between), that original is kept
/// rather than overwritten with the intermediate value.
async fn set_settings_now(ctx: &RunContext, state: &mut RunState, values: &Vars) -> Result<String, String> {
    let dir = ctx.project_root.to_string_lossy().into_owned();
    let values = values.clone();
    let count = values.len();
    let originals: std::collections::HashMap<String, Option<String>> = blocking(move || {
        let mut json = read_settings_json(&dir)?;
        let obj = values_object(&mut json)?;
        let mut originals = std::collections::HashMap::new();
        for (k, v) in &values {
            originals.insert(k.clone(), obj.get(k).and_then(|x| x.as_str()).map(str::to_string));
            obj.insert(k.clone(), Value::String(v.clone()));
        }
        write_settings_json(&dir, &json)?;
        Ok(originals)
    })
    .await?;

    for (k, orig) in originals {
        state.settings_snapshot.entry(k).or_insert(orig);
    }
    Ok(format!("set {count} setting(s) — will restore on scenario end or failure"))
}

/// Put back every key in `state.settings_snapshot`, then clear it.
///
/// Shared by the `restore_settings` step and the automatic restore `run()`
/// triggers after any step failure — a `set_settings` value must never
/// outlive the run that set it, whether or not the scenario remembered its
/// own cleanup step.
async fn restore_settings_now(ctx: &RunContext, state: &mut RunState) -> Result<String, String> {
    if state.settings_snapshot.is_empty() {
        return Ok("no settings were changed — nothing to restore".to_string());
    }
    let dir = ctx.project_root.to_string_lossy().into_owned();
    let snapshot = std::mem::take(&mut state.settings_snapshot);
    let count = snapshot.len();
    blocking(move || {
        let mut json = read_settings_json(&dir)?;
        let obj = values_object(&mut json)?;
        for (k, orig) in snapshot {
            match orig {
                Some(v) => {
                    obj.insert(k, Value::String(v));
                }
                None => {
                    obj.remove(&k);
                }
            }
        }
        write_settings_json(&dir, &json)
    })
    .await?;
    Ok(format!("restored {count} setting(s)"))
}

fn read_settings_json(dir: &str) -> Result<Value, String> {
    let text = settings_file::read_local_settings(dir)?;
    if text.trim().is_empty() {
        Ok(serde_json::json!({ "IsEncrypted": false, "Values": {} }))
    } else {
        serde_json::from_str(&text).map_err(|e| format!("local.settings.json is invalid JSON: {e}"))
    }
}

fn write_settings_json(dir: &str, json: &Value) -> Result<(), String> {
    let pretty = serde_json::to_string_pretty(json).map_err(|e| e.to_string())?;
    settings_file::write_local_settings(dir, &pretty)
}

fn values_object(json: &mut Value) -> Result<&mut serde_json::Map<String, Value>, String> {
    if json["Values"].is_null() {
        json["Values"] = Value::Object(serde_json::Map::new());
    }
    json["Values"]
        .as_object_mut()
        .ok_or_else(|| "local.settings.json has a non-object \"Values\"".to_string())
}

/// Stop and restart func via the caller-supplied [`RestartFn`], bounded by
/// `timeout_ms` on top of whatever timeout the callback applies internally.
async fn restart_func_now(ctx: &RunContext, timeout_ms: u64) -> Result<String, String> {
    let restart = ctx.restart_func.clone().ok_or_else(|| {
        "this environment can't restart func — restart_func requires running from the Tests view".to_string()
    })?;
    match tokio::time::timeout(Duration::from_millis(timeout_ms), restart()).await {
        Ok(inner) => inner,
        Err(_) => Err(format!("func did not finish restarting within {timeout_ms}ms")),
    }
}

/// Dynamic values available to every step as `{{name}}`, alongside the
/// scenario's own `vars` and any `capture`d/`PREV_STEP_RESULT` values (which
/// take precedence on a name clash — see `run()`).
fn builtin_vars() -> Vars {
    let mut out = Vars::new();
    let now_utc = chrono::Utc::now();
    let cet = now_cet(now_utc);
    out.insert("NOW_UTC".to_string(), now_utc.to_rfc3339());
    out.insert("NOW_CET_HH:mm".to_string(), cet.format("%H:%M").to_string());
    out.insert("NOW_CET_HHmm".to_string(), cet.format("%H%M").to_string());
    out.insert("TODAY_YYYYMMDD".to_string(), cet.format("%Y%m%d").to_string());
    out.insert("GUID".to_string(), uuid::Uuid::new_v4().to_string());
    out
}

/// Central European local time (CET/CEST) for `utc`, computed without a
/// timezone database.
///
/// The EU's one DST rule — clocks forward the last Sunday of March at 01:00
/// UTC, back the last Sunday of October at 01:00 UTC — is simple enough not
/// to justify adding `chrono-tz` as a dependency just for this.
fn now_cet(utc: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::FixedOffset> {
    use chrono::Datelike;
    let year = utc.year();
    let dst_start = last_sunday_1am_utc(year, 3);
    let dst_end = last_sunday_1am_utc(year, 10);
    let offset_hours = if utc >= dst_start && utc < dst_end { 2 } else { 1 };
    utc.with_timezone(&chrono::FixedOffset::east_opt(offset_hours * 3600).unwrap())
}

/// The most recent Sunday on or before the last day of `month` in `year`, at
/// 01:00 UTC — the instant EU daylight-saving transitions take effect.
fn last_sunday_1am_utc(year: i32, month: u32) -> chrono::DateTime<chrono::Utc> {
    use chrono::{Datelike, NaiveDate, TimeZone, Utc};
    let (next_year, next_month) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    let first_of_next = NaiveDate::from_ymd_opt(next_year, next_month, 1)
        .expect("month is always 1..=12");
    let last_day = first_of_next - chrono::Duration::days(1);
    let back_to_sunday = last_day.weekday().num_days_from_sunday() as i64;
    let last_sunday = last_day - chrono::Duration::days(back_to_sunday);
    Utc.from_utc_datetime(&last_sunday.and_hms_opt(1, 0, 0).expect("1:00:00 is always valid"))
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
        CheckBlobExists { blob_name, exists: true, .. } => format!("check {blob_name} exists"),
        CheckBlobExists { blob_name, exists: false, .. } => format!("check {blob_name} absent"),
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
        RunWorkflow { workflow, capture: Some(key), .. } => format!("run {workflow} → {{{{{key}}}}}"),
        RunWorkflow { workflow, .. } => format!("run {workflow}"),
        RunProcess { command, .. } => format!("run process {command}"),
        SetSettings { values } => format!("set {} setting(s)", values.len()),
        RestoreSettings => "restore settings".to_string(),
        RestartFunc { .. } => "restart func".to_string(),
        Sleep { ms } => format!("sleep {ms}ms"),
        WaitForMessage { queue, .. } => format!("wait for message on {queue}"),
        WaitForRun { workflow, expect_status, .. } => format!("wait for {workflow} run ({expect_status})"),
        WaitForSql { database, .. } => format!("wait for SQL on {database}"),
        Expect { queue, .. } => format!("expect on {queue}"),
        ExpectAction { workflow, action_name, .. } => format!("expect {action_name} in {workflow}"),
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
            capture,
            expect_trigger_error,
        } = step
        else {
            panic!("wrong variant")
        };
        assert_eq!(workflow, "Pivot-Ignite-Invoice");
        assert_eq!(trigger, "manual");
        assert_eq!(body, "");
        assert_eq!(capture, None);
        assert!(!expect_trigger_error);
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
            { "action": "check_blob_exists", "container": "in", "blob_name": "b.xml" },
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
            { "action": "set_settings", "values": { "kyriba:sla:checkTime": "{{NOW_CET_HH:mm}}" } },
            { "action": "restart_func" },
            { "action": "restore_settings" },
            { "action": "sleep", "ms": 100 },
            { "action": "wait_for_message", "queue": "q", "path": "id", "expected": "42" },
            { "action": "wait_for_run", "workflow": "W" },
            { "action": "wait_for_run", "workflow": "W", "expect_status": "Failed" },
            { "action": "wait_for_sql", "database": "aisdev", "sql": "SELECT 1", "min_rows": 1 },
            { "action": "expect", "queue": "q", "expected": "42", "min_count": 1 },
            { "action": "expect_action", "workflow": "W", "action_name": "Send_notif", "contains": "42" },
            { "action": "run_process", "command": "python3", "args": ["stub.py", "8899"], "wait_for_port": 8899 }
          ]
        }"#;

        let scenario: Scenario = serde_json::from_str(json).unwrap();
        assert_eq!(scenario.steps.len(), 31);

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
    fn discover_recurses_into_subfolders_and_groups_by_them() {
        let root = std::env::temp_dir().join(format!("ais-grp-{}", std::process::id()));
        let scenarios = scenario_dir(&root);
        std::fs::create_dir_all(scenarios.join("smoke")).unwrap();
        std::fs::create_dir_all(scenarios.join("regression").join("kyriba")).unwrap();

        let mk = |dir: &Path, name: &str| Scenario {
            name: name.to_string(),
            description: String::new(),
            vars: Default::default(),
            steps: vec![],
            source: unique_in(dir, name),
        };
        save(&mk(&scenarios, "Root Level")).unwrap();
        save(&mk(&scenarios.join("smoke"), "Smoke One")).unwrap();
        save(&mk(&scenarios.join("regression").join("kyriba"), "Nested Two")).unwrap();

        let (found, errors) = discover(&root);
        assert!(errors.is_empty());
        assert_eq!(found.len(), 3);

        let groups: Vec<Option<String>> = found.iter().map(|s| group_of(&root, s)).collect();
        assert_eq!(groups[0], None, "root-level scenario sorts first, ungrouped");
        assert_eq!(groups.iter().flatten().find(|g| g.as_str() == "smoke"), Some(&"smoke".to_string()));
        assert!(groups.iter().flatten().any(|g| g == "regression / kyriba"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn group_of_is_none_for_a_scenario_outside_the_project_entirely() {
        // A defensive case: source paths always come from discover() or
        // unique_path() in practice, so this can't happen organically, but
        // group_of must not panic on a source that doesn't share a prefix.
        let scenario = Scenario {
            name: "Stray".into(),
            description: String::new(),
            vars: Default::default(),
            steps: vec![],
            source: PathBuf::from("/somewhere/else/stray.json"),
        };
        assert_eq!(group_of(Path::new("/project"), &scenario), None);
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

    // ── run_process ─────────────────────────────────────────────────────

    fn sleep_step(secs: &str) -> Step {
        Step::RunProcess {
            command: "sleep".to_string(),
            args: vec![secs.to_string()],
            workdir: None,
            env: Vars::new(),
            wait_for_port: None,
            wait_timeout_ms: default_port_wait(),
            stop_at_end: true,
        }
    }

    #[test]
    fn run_process_defaults_are_safe() {
        // stop_at_end must default ON — a stub left holding its port breaks the
        // *next* run with a bind error that points nowhere near the cause.
        let json = r#"{ "action": "run_process", "command": "python3" }"#;
        let Step::RunProcess { args, stop_at_end, wait_for_port, workdir, .. } =
            serde_json::from_str(json).unwrap()
        else {
            panic!("wrong variant")
        };
        assert!(stop_at_end, "processes must be cleaned up unless opted out");
        assert!(args.is_empty());
        assert!(wait_for_port.is_none());
        assert!(workdir.is_none());
    }

    #[tokio::test]
    async fn processes_are_killed_when_the_scenario_ends() {
        let ctx = test_ctx(Path::new("/tmp"));
        let mut state = test_state();

        // Long enough that it can only be gone because we killed it.
        exec(&sleep_step("120"), &ctx, &mut state).await.unwrap();
        assert_eq!(state.processes.len(), 1);
        let pid = state.processes[0].child.id();

        let stopped = stop_processes(&mut state);
        assert_eq!(stopped.len(), 1);
        assert!(state.processes.is_empty());

        // The pid must be reaped, not merely signalled: `kill -0` succeeds on a
        // zombie, so ask the OS whether any live process still owns it.
        let alive = std::process::Command::new("ps")
            .args(["-p", &pid.to_string()])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().count() > 1)
            .unwrap_or(false);
        assert!(!alive, "pid {pid} survived teardown");
    }

    #[tokio::test]
    async fn stop_at_end_false_leaves_the_process_running() {
        let ctx = test_ctx(Path::new("/tmp"));
        let mut state = test_state();

        let Step::RunProcess { command, args, workdir, env, wait_for_port, wait_timeout_ms, .. } =
            sleep_step("120")
        else {
            panic!("wrong variant")
        };
        let step = Step::RunProcess {
            command, args, workdir, env, wait_for_port, wait_timeout_ms,
            stop_at_end: false,
        };

        exec(&step, &ctx, &mut state).await.unwrap();
        let pid = state.processes[0].child.id();
        assert!(stop_processes(&mut state).is_empty(), "opted-out process must not be killed");

        // Clean up ourselves — the runner deliberately didn't.
        let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
    }

    #[tokio::test]
    async fn waiting_on_a_port_fails_fast_when_the_process_dies() {
        let ctx = test_ctx(Path::new("/tmp"));
        let mut state = test_state();

        // `false` exits immediately without binding anything. The step must
        // notice the exit rather than burn the whole timeout on a corpse.
        let step = Step::RunProcess {
            command: "false".to_string(),
            args: vec![],
            workdir: None,
            env: Vars::new(),
            wait_for_port: Some(59_999),
            wait_timeout_ms: 30_000,
            stop_at_end: true,
        };

        let started = std::time::Instant::now();
        let err = exec(&step, &ctx, &mut state).await.unwrap_err();
        assert!(err.contains("exited before binding"), "unexpected message: {err}");
        assert!(started.elapsed() < Duration::from_secs(10), "should not wait out the timeout");
        // Still tracked, so teardown reaps it even though the step failed.
        assert_eq!(state.processes.len(), 1);
        stop_processes(&mut state);
    }

    #[tokio::test]
    async fn a_failing_scenario_still_tears_its_processes_down() {
        let dir = std::env::temp_dir().join(format!("ais-proc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let scenario = Scenario {
            name: "teardown".into(),
            description: String::new(),
            vars: Default::default(),
            steps: vec![
                sleep_step("120"),
                // Fails deterministically: test_ctx has no restart_func. Chosen
                // over RunWorkflow so the failure is the step's own, not the
                // pre-run service probe short-circuiting the whole scenario.
                Step::RestartFunc { timeout_ms: 1 },
            ],
            source: unique_in(&dir, "teardown"),
        };

        let results = run(&scenario, &test_ctx(&dir), |_| {}).await;

        assert_eq!(results[0].status, StepStatus::Ok, "the process should start");
        assert_eq!(results[1].status, StepStatus::Failed, "the workflow step should fail");
        let teardown = results.last().unwrap();
        assert!(
            teardown.label.contains("helper process"),
            "teardown must run after a failure, got: {}", teardown.label,
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    fn test_ctx(project_root: &Path) -> RunContext {
        RunContext {
            sb_host: "127.0.0.1".to_string(),
            cosmos_endpoint: String::new(),
            cosmos_key: String::new(),
            project_root: project_root.to_path_buf(),
            restart_func: None,
        }
    }

    fn test_state() -> RunState {
        RunState {
            vars: Vars::new(),
            run_floor: chrono::Utc::now(),
            claimed_runs: std::collections::HashSet::new(),
            settings_snapshot: std::collections::HashMap::new(),
            processes: Vec::new(),
            last_run: std::collections::HashMap::new(),
        }
    }

    #[tokio::test]
    async fn set_settings_then_restore_settings_round_trips_exactly() {
        let dir = std::env::temp_dir().join(format!("ais-settings-{}-a", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("local.settings.json"),
            r#"{ "IsEncrypted": false, "Values": { "kept": "already-here", "changed": "before" } }"#,
        )
        .unwrap();

        let ctx = test_ctx(&dir);
        let mut state = test_state();

        let mut values = Vars::new();
        values.insert("changed".to_string(), "after".to_string());
        values.insert("new_key".to_string(), "brand-new".to_string());
        set_settings_now(&ctx, &mut state, &values).await.unwrap();

        let after_set: Value =
            serde_json::from_str(&settings_file::read_local_settings(dir.to_str().unwrap()).unwrap()).unwrap();
        assert_eq!(after_set["Values"]["changed"], "after");
        assert_eq!(after_set["Values"]["new_key"], "brand-new");
        assert_eq!(after_set["Values"]["kept"], "already-here");

        restore_settings_now(&ctx, &mut state).await.unwrap();
        assert!(state.settings_snapshot.is_empty());

        let restored: Value =
            serde_json::from_str(&settings_file::read_local_settings(dir.to_str().unwrap()).unwrap()).unwrap();
        assert_eq!(restored["Values"]["changed"], "before");
        assert_eq!(restored["Values"]["kept"], "already-here");
        // A key that didn't exist before set_settings must be removed, not
        // left behind blank — leaving it would be a subtler leak than the
        // value it replaced.
        assert!(restored["Values"].get("new_key").is_none());
    }

    #[tokio::test]
    async fn a_second_set_settings_does_not_overwrite_the_first_snapshot() {
        let dir = std::env::temp_dir().join(format!("ais-settings-{}-b", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("local.settings.json"),
            r#"{ "IsEncrypted": false, "Values": { "k": "original" } }"#,
        )
        .unwrap();

        let ctx = test_ctx(&dir);
        let mut state = test_state();

        let mut first = Vars::new();
        first.insert("k".to_string(), "intermediate".to_string());
        set_settings_now(&ctx, &mut state, &first).await.unwrap();

        let mut second = Vars::new();
        second.insert("k".to_string(), "final".to_string());
        set_settings_now(&ctx, &mut state, &second).await.unwrap();

        // Two writes, one restore — the snapshot must still be the true
        // original, not the intermediate value the first write produced.
        assert_eq!(state.settings_snapshot.get("k"), Some(&Some("original".to_string())));

        restore_settings_now(&ctx, &mut state).await.unwrap();
        let restored: Value =
            serde_json::from_str(&settings_file::read_local_settings(dir.to_str().unwrap()).unwrap()).unwrap();
        assert_eq!(restored["Values"]["k"], "original");
    }

    #[tokio::test]
    async fn restoring_with_nothing_snapshotted_is_a_harmless_no_op() {
        let dir = std::env::temp_dir().join(format!("ais-settings-{}-c", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = test_ctx(&dir);
        let mut state = test_state();
        let detail = restore_settings_now(&ctx, &mut state).await.unwrap();
        assert!(detail.contains("nothing to restore"));
    }

    #[tokio::test]
    async fn restart_func_without_a_callback_fails_with_a_clear_message() {
        // The Tests view always supplies one; anything constructing RunContext
        // without it (or a future caller that forgets to) should get a message
        // that explains why, not a panic on a missing closure.
        let ctx = test_ctx(Path::new("/tmp"));
        let err = restart_func_now(&ctx, 1_000).await.unwrap_err();
        assert!(err.contains("Tests view"), "got: {err}");
    }

    #[test]
    fn check_blob_exists_defaults_to_asserting_presence() {
        let json = r#"{ "action": "check_blob_exists", "container": "c", "blob_name": "b.xml" }"#;
        let Step::CheckBlobExists { exists, .. } = serde_json::from_str(json).unwrap() else {
            panic!("wrong variant")
        };
        assert!(exists);
    }

    #[test]
    fn expect_action_defaults_to_succeeded_and_no_substring_check() {
        let json = r#"{ "action": "expect_action", "workflow": "W", "action_name": "Send_notif" }"#;
        let Step::ExpectAction {
            workflow,
            action_name,
            status,
            contains,
            timeout_ms,
        } = serde_json::from_str(json).unwrap()
        else {
            panic!("wrong variant")
        };
        assert_eq!(workflow, "W");
        assert_eq!(action_name, "Send_notif");
        assert_eq!(status, "Succeeded");
        assert_eq!(contains, None);
        assert_eq!(timeout_ms, 30_000);
    }

    #[test]
    fn expect_action_uses_action_name_because_action_is_the_serde_tag() {
        // The enum is `#[serde(tag = "action")]`, so a field literally called
        // `action` would collide with the discriminator. Guards against someone
        // "tidying" the field name back to `action` later.
        let step = Step::ExpectAction {
            workflow: "W".into(),
            action_name: "A".into(),
            status: "Succeeded".into(),
            contains: None,
            timeout_ms: 1,
        };
        let text = serde_json::to_string(&step).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["action"], "expect_action");
        assert_eq!(v["action_name"], "A");
    }

    #[test]
    fn expect_action_label_names_both_the_action_and_its_workflow() {
        assert_eq!(
            label_of(&Step::ExpectAction {
                workflow: "Check-Ignite-Payment-File".into(),
                action_name: "Send_success_notification".into(),
                status: "Succeeded".into(),
                contains: None,
                timeout_ms: 1,
            }),
            "expect Send_success_notification in Check-Ignite-Payment-File"
        );
    }

    #[test]
    fn wait_for_run_defaults_to_expecting_success() {
        let json = r#"{ "action": "wait_for_run", "workflow": "W" }"#;
        let Step::WaitForRun { expect_status, .. } = serde_json::from_str(json).unwrap() else {
            panic!("wrong variant")
        };
        assert_eq!(expect_status, "Succeeded");
    }

    #[test]
    fn eu_dst_transitions_land_on_a_sunday_at_1am_utc_near_month_end() {
        use chrono::{Datelike, Timelike};
        for year in [2024, 2025, 2026, 2027] {
            for month in [3, 10] {
                let t = last_sunday_1am_utc(year, month);
                assert_eq!(t.weekday(), chrono::Weekday::Sun);
                assert_eq!(t.hour(), 1);
                assert_eq!(t.month(), month);
                // Always within the last 7 days of the month.
                assert!(t.day() > 24, "{t} is not near the end of the month");
            }
        }
    }

    #[test]
    fn now_cet_is_one_hour_ahead_in_january_and_two_in_july() {
        use chrono::TimeZone;
        let jan = chrono::Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();
        let jul = chrono::Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
        assert_eq!(now_cet(jan).offset().local_minus_utc(), 3600);
        assert_eq!(now_cet(jul).offset().local_minus_utc(), 7200);
    }

    #[test]
    fn builtin_vars_cover_every_documented_placeholder() {
        let vars = builtin_vars();
        for key in ["NOW_UTC", "NOW_CET_HH:mm", "NOW_CET_HHmm", "TODAY_YYYYMMDD", "GUID"] {
            assert!(vars.contains_key(key), "missing {key}");
        }
        assert!(chrono::DateTime::parse_from_rfc3339(&vars["NOW_UTC"]).is_ok());
        assert_eq!(vars["TODAY_YYYYMMDD"].len(), 8);
    }

    #[tokio::test]
    async fn prev_step_result_is_available_to_the_step_that_follows() {
        // Exercises the mechanism through the public run() loop rather than
        // poking state directly, since that's the actual contract: a step's
        // detail becomes {{PREV_STEP_RESULT}} for the very next step, with no
        // explicit `capture` field required.
        let dir = std::env::temp_dir().join(format!("ais-prevstep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("local.settings.json"), r#"{ "Values": {} }"#).unwrap();

        let scenario = Scenario {
            name: "prev-step".to_string(),
            description: String::new(),
            vars: Vars::new(),
            steps: vec![
                Step::Sleep { ms: 0 },
                Step::SetSettings {
                    values: {
                        let mut v = Vars::new();
                        v.insert("marker".to_string(), "{{PREV_STEP_RESULT}}".to_string());
                        v
                    },
                },
            ],
            source: dir.join("prev-step.json"),
        };
        let ctx = test_ctx(&dir);
        let results = run(&scenario, &ctx, |_| {}).await;
        assert_eq!(results[1].status, StepStatus::Ok);

        let after: Value =
            serde_json::from_str(&settings_file::read_local_settings(dir.to_str().unwrap()).unwrap()).unwrap();
        assert_eq!(after["Values"]["marker"], "slept 0ms");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_failed_step_auto_restores_settings_even_without_a_restore_step() {
        let dir = std::env::temp_dir().join(format!("ais-autorestore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("local.settings.json"),
            r#"{ "Values": { "k": "original" } }"#,
        )
        .unwrap();

        let scenario = Scenario {
            name: "auto-restore".to_string(),
            description: String::new(),
            vars: Vars::new(),
            steps: vec![
                Step::SetSettings {
                    values: {
                        let mut v = Vars::new();
                        v.insert("k".to_string(), "changed".to_string());
                        v
                    },
                },
                // No restart_func callback in this test's RunContext, so this
                // step fails — the scenario never reaches an explicit
                // restore_settings step, if it even had one.
                Step::RestartFunc { timeout_ms: 100 },
            ],
            source: dir.join("auto-restore.json"),
        };
        let ctx = test_ctx(&dir);
        let results = run(&scenario, &ctx, |_| {}).await;

        assert_eq!(results[1].status, StepStatus::Failed);
        // The synthetic auto-restore step, appended after the failure.
        let auto = results.last().unwrap();
        assert!(auto.label.contains("auto-restore"));
        assert_eq!(auto.status, StepStatus::Ok);

        let after: Value =
            serde_json::from_str(&settings_file::read_local_settings(dir.to_str().unwrap()).unwrap()).unwrap();
        assert_eq!(after["Values"]["k"], "original");

        std::fs::remove_dir_all(&dir).ok();
    }
}


#[cfg(test)]
mod stale_assertion_tests {
    use super::*;

    fn workspace() -> std::path::PathBuf {
        let tmp = std::env::temp_dir().join(format!("ais-runner-stale-{}", std::process::id()));
        let la = tmp.join("logic_apps");
        for (wf, body) in [
            ("Check-Ignite-Payment-File", r#"{"definition":{"actions":{
                "Scope_Processing":{"type":"Scope","actions":{
                    "Send_message_to_queue":{"type":"ServiceProvider"}}}}}}"#),
            ("Send-Kyriba-files", r#"{"definition":{"actions":{
                "Send_success_notification":{"type":"ServiceProvider"}}}}"#),
        ] {
            std::fs::create_dir_all(la.join(wf)).unwrap();
            std::fs::write(la.join(wf).join("workflow.json"), body).unwrap();
        }
        tmp
    }

    fn scenario_with(steps: Vec<Step>) -> Scenario {
        Scenario {
            name: "test".to_string(),
            description: String::new(),
            vars: Vars::new(),
            steps,
            source: PathBuf::new(),
        }
    }

    fn expect_action(workflow: &str, action: &str) -> Step {
        Step::ExpectAction {
            workflow: workflow.to_string(),
            action_name: action.to_string(),
            status: "Succeeded".to_string(),
            contains: None,
            timeout_ms: 15000,
        }
    }

    #[test]
    fn moved_action_is_reported_with_its_new_home() {
        let tmp = workspace();
        let scenario = scenario_with(vec![expect_action(
            "Check-Ignite-Payment-File",
            "Send_success_notification",
        )]);
        let problems = stale_assertions(&scenario, &tmp.to_string_lossy());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("has no action named 'Send_success_notification'"));
        assert!(problems[0].contains("it exists in Send-Kyriba-files"), "{}", problems[0]);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn valid_assertions_and_unknown_workflows_are_left_alone() {
        let tmp = workspace();
        let scenario = scenario_with(vec![
            expect_action("Send-Kyriba-files", "Send_success_notification"),
            expect_action("Check-Ignite-Payment-File", "Send_message_to_queue"),
            // no workflow.json on disk: cannot tell, so must not be flagged
            expect_action("Deployed-Only-Workflow", "Whatever"),
        ]);
        assert!(stale_assertions(&scenario, &tmp.to_string_lossy()).is_empty());
        std::fs::remove_dir_all(&tmp).ok();
    }
}

#[cfg(test)]
mod service_probe_tests {
    use super::*;

    fn ctx() -> RunContext {
        RunContext {
            sb_host: "127.0.0.1".to_string(),
            cosmos_endpoint: "https://127.0.0.1:8081".to_string(),
            cosmos_key: String::new(),
            project_root: PathBuf::from("/nonexistent"),
            restart_func: None,
        }
    }

    fn scenario_of(steps: Vec<Step>) -> Scenario {
        Scenario { name: "t".into(), description: String::new(), vars: Vars::new(), steps, source: PathBuf::new() }
    }

    fn labels(steps: Vec<Step>) -> Vec<&'static str> {
        required_services(&scenario_of(steps), &ctx()).into_iter().map(|(l, _, _)| l).collect()
    }

    /// Pure: what a scenario declares it needs, with no probing, so the result
    /// does not change when a developer starts or stops an emulator.
    #[test]
    fn only_requires_what_the_scenario_uses() {
        assert!(labels(vec![Step::Sleep { ms: 1 }]).is_empty());

        assert_eq!(
            labels(vec![Step::CreateContainer { container: "c".into() }]),
            vec!["Azurite (blob)"]
        );

        // func needs the table service too, which is what actually crashed it
        let with_func = labels(vec![Step::WaitForRun {
            workflow: "W".into(), timeout_ms: 1, expect_status: "Succeeded".into(),
        }]);
        assert!(with_func.contains(&"Logic Apps runtime (func)"), "{with_func:?}");
        assert!(with_func.contains(&"Azurite (table)"), "{with_func:?}");
    }

    #[test]
    fn the_azurite_hint_is_actionable() {
        let needed = required_services(
            &scenario_of(vec![Step::CreateContainer { container: "c".into() }]), &ctx());
        let (_, addr, hint) = &needed[0];
        assert_eq!(addr, "127.0.0.1:10000");
        assert!(hint.contains("⟳ Reset"), "{hint}");
    }

    #[test]
    fn host_parsing_handles_urls_and_bare_hosts() {
        assert_eq!(host_only("http://127.0.0.1:5672/x"), "127.0.0.1");
        assert_eq!(host_only("localhost"), "localhost");
        assert_eq!(host_port("https://127.0.0.1:8081/", 8081), "127.0.0.1:8081");
        assert_eq!(host_port("cosmos.local", 8081), "cosmos.local:8081");
    }
}

#[cfg(test)]
mod service_gate_tests {
    use super::*;

    /// The gate must stop the run before any step executes — creating
    /// containers against a dead emulator is what produced raw transport
    /// errors ten steps in. Port 9 (discard) is never listening.
    #[tokio::test]
    async fn a_down_service_aborts_before_the_first_step() {
        let dir = std::env::temp_dir().join(format!("ais-gate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let scenario = Scenario {
            name: "gate".into(),
            description: String::new(),
            vars: Default::default(),
            steps: vec![Step::CreateCosmosDatabase { database: "d".into() }],
            source: unique_in(&dir, "gate"),
        };
        let ctx = RunContext {
            sb_host: "127.0.0.1".to_string(),
            cosmos_endpoint: "https://127.0.0.1:9".to_string(),
            cosmos_key: String::new(),
            project_root: dir.clone(),
            restart_func: None,
        };
        let results = run(&scenario, &ctx, |_| {}).await;
        assert_eq!(results.len(), 1, "must not run any step: {results:?}");
        assert_eq!(results[0].status, StepStatus::Failed);
        assert!(results[0].label.contains("check local services"));
        assert!(results[0].detail.contains("Cosmos"), "{}", results[0].detail);
        std::fs::remove_dir_all(&dir).ok();
    }
}
