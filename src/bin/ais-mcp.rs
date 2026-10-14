//! MCP server for AIS Runner.
//!
//! Lets Claude (Desktop, Code) or any other MCP client look at a Logic Apps
//! project the way the app does: its workflows and what they touch, whether
//! each can run locally, the runs and actions of the local runtime, the saved
//! test scenarios, and the project's Azure DevOps pipelines and build logs.
//!
//! Read-only on purpose. Nothing here starts or stops anything, triggers a
//! workflow, sends a message or edits a file — the app's own pre-start check is
//! left out for that reason, since it repairs files as it goes. The local stack
//! (Azurite, the emulators, `func start`) is started from the app; tools that
//! need the runtime say so when it isn't up.
//!
//! Speaks MCP over stdin/stdout, so nothing in this process may print to
//! stdout. Diagnostics go to stderr.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ais_core::devops_cli;
use ais_runner::services::config;
use ais_runner::services::run_explain;
use ais_runner::services::run_readiness;
use ais_runner::services::sb_check;
use ais_runner::services::scenario;
use ais_runner::services::system_check;
use ais_runner::services::workflow_analysis::{self, TriggerKind};
use ais_runner::services::workflows;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

const USAGE: &str = "\
ais-mcp — AIS Runner's MCP server (stdio)

USAGE:
    ais-mcp [--project <dir>]

OPTIONS:
    --project <dir>   Project the tools use when a call names none.
                      [default: the project last opened in AIS Runner]
    -h, --help        Show this help

Started by an MCP client, not by hand. For Claude Code:
    claude mcp add ais-runner -- /path/to/ais-mcp
";

/// How long a call to the local runtime may take. The services apply no
/// timeout of their own, and a wedged func host would otherwise hang the
/// client's tool call forever.
const RUNTIME_TIMEOUT: Duration = Duration::from_secs(20);

/// Longest text returned for an action's inputs and outputs.
const MAX_PAYLOAD_CHARS: usize = 30_000;

const FUNC_DOWN: &str = "The Logic Apps runtime isn't answering on localhost:7071. Start it \
from AIS Runner (Azurite first, then ▶ func), then try again.";

// ── project resolution ────────────────────────────────────────────────────────

struct Project {
    /// The folder the user opened (holds `.ais-runner/scenarios`).
    root: PathBuf,
    /// The folder holding the workflow folders and local.settings.json.
    logic_apps_dir: String,
    /// Keys this project may be saved under in the app's config.
    config_keys: Vec<String>,
}

fn resolve_project(arg: Option<&str>, default: Option<&str>) -> Result<Project, String> {
    let recent = config::load().recent_dirs;
    let dir = arg
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .or_else(|| default.map(str::to_string))
        .or_else(|| recent.first().cloned())
        .ok_or(
            "No project given and none opened in AIS Runner yet. Pass `project` \
                (the folder you open in the app), or open one in the app first.",
        )?;
    let path = Path::new(dir.trim());
    if !path.is_dir() {
        return Err(format!("Project folder not found: {}", path.display()));
    }
    let is_nested = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "logic_apps" || n == "logic-apps");
    let root = if is_nested {
        path.parent().unwrap_or(path).to_path_buf()
    } else {
        path.to_path_buf()
    };
    let logic_apps_dir = workflows::resolve_logic_apps_dir_at(path)
        .to_string_lossy()
        .to_string();
    let config_keys = vec![
        path.to_string_lossy().to_string(),
        logic_apps_dir.clone(),
        root.to_string_lossy().to_string(),
    ];
    Ok(Project {
        root,
        logic_apps_dir,
        config_keys,
    })
}

/// Mask the secret parts of a connection string or URL (account keys, SAS keys
/// and signatures, passwords, function keys) before it goes to the client.
/// Everything a tool returns ends up in a chat transcript.
fn redact(s: &str) -> String {
    static SECRET: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = SECRET.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\b(AccountKey|SharedAccessKey|SharedAccessSignature|Password|Pwd|sig|code|client_secret|ApiKey|AccessKey)=([^;&\s]+)",
        )
        .expect("valid regex")
    });
    re.replace_all(s, "$1=***").into_owned()
}

/// A Service Bus namespace for display: the host when it's a connection string.
fn namespace_label(ns: &str) -> String {
    sb_check::fqdn_from_conn_str(ns).unwrap_or_else(|| redact(ns))
}

fn to_text(v: Value) -> Result<String, String> {
    serde_json::to_string_pretty(&v).map_err(|e| e.to_string())
}

async fn with_runtime<T>(
    fut: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    match tokio::time::timeout(RUNTIME_TIMEOUT, fut).await {
        Ok(Ok(v)) => Ok(v),
        // the services' errors don't say "not running" in so many words
        Ok(Err(e)) if !port_open(7071).await => Err(format!("{FUNC_DOWN} ({e})")),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(FUNC_DOWN.to_string()),
    }
}

async fn port_open(port: u16) -> bool {
    tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .is_ok_and(|r| r.is_ok())
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())
}

fn trigger_json(t: &TriggerKind) -> Value {
    match t {
        TriggerKind::Unknown => json!({ "kind": "unknown" }),
        TriggerKind::Http => json!({ "kind": "http" }),
        TriggerKind::ServiceBus { queue } => json!({ "kind": "serviceBus", "queue": queue }),
        TriggerKind::Blob { container } => json!({ "kind": "blob", "container": container }),
        TriggerKind::Timer { schedule } => json!({ "kind": "timer", "schedule": schedule }),
    }
}

/// The last `max` lines of a build log, without the timestamp Azure DevOps puts
/// on every line ("2026-10-07T09:17:59.7531762Z "): it doubles the size of the
/// log and the client rarely needs it.
fn tail_lines(text: &str, max: usize) -> (usize, String) {
    let lines: Vec<&str> = text
        .lines()
        .map(|l| match l.split_once(' ') {
            Some((ts, rest))
                if ts.len() >= 20 && ts.ends_with('Z') && ts.as_bytes()[10] == b'T' =>
            {
                rest
            }
            _ => l,
        })
        .collect();
    let skipped = lines.len().saturating_sub(max);
    (skipped, lines[skipped..].join("\n"))
}

/// At most `max` characters of `s`: a third from the start and the rest from
/// the end. An action's text is its inputs then its outputs, and on a failed
/// action the error is in the outputs, at the end.
fn truncate_chars(s: &str, max: usize) -> String {
    let total = s.chars().count();
    if total <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max / 3).collect();
    let tail: String = s.chars().skip(total - (max - max / 3)).collect();
    format!(
        "{head}\n… [{} characters cut from the middle] …\n{tail}",
        total - max
    )
}

// ── parameters ────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema, Default)]
struct ProjectArg {
    /// Project folder, as opened in AIS Runner. Defaults to the server's
    /// project, else the one last opened in the app.
    #[serde(default)]
    project: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct WorkflowArg {
    /// Workflow name (its folder name).
    workflow: String,
    /// Project folder; see `list_workflows`.
    #[serde(default)]
    project: Option<String>,
    /// Also return the full workflow.json definition.
    #[serde(default)]
    include_definition: bool,
}

#[derive(Deserialize, JsonSchema)]
struct RunsArg {
    /// Workflow name.
    workflow: String,
    /// How many of the most recent runs to return (default 10).
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct RunArg {
    /// Workflow name.
    workflow: String,
    /// Run id, from `list_runs`.
    run_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct ActionArg {
    /// Workflow name.
    workflow: String,
    /// Run id, from `list_runs`.
    run_id: String,
    /// Action name, from `get_run`.
    action: String,
}

#[derive(Deserialize, JsonSchema, Default)]
struct DevOpsTarget {
    /// Project folder; its saved or detected Azure DevOps org and project are used.
    #[serde(default)]
    project: Option<String>,
    /// Azure DevOps organization URL or name, overriding the detected one.
    #[serde(default)]
    org: Option<String>,
    /// Azure DevOps project name, overriding the detected one.
    #[serde(default)]
    devops_project: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct PipelineRunsArg {
    /// Build pipeline id, from `devops_list_pipelines`.
    pipeline_id: u64,
    #[serde(flatten)]
    target: DevOpsTarget,
}

#[derive(Deserialize, JsonSchema)]
struct BuildLogArg {
    /// Build id, from `devops_list_runs`.
    build_id: u64,
    /// Return the log of the steps whose name contains this text. By default
    /// the logs of the failed steps are returned.
    #[serde(default)]
    step: Option<String>,
    /// Lines kept from the end of each log (default 200).
    #[serde(default)]
    max_lines: Option<usize>,
    #[serde(flatten)]
    target: DevOpsTarget,
}

// ── server ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AisMcp {
    default_project: Option<String>,
    tool_router: ToolRouter<Self>,
}

impl AisMcp {
    fn project(&self, arg: Option<&str>) -> Result<Project, String> {
        resolve_project(arg, self.default_project.as_deref())
    }

    /// The DevOps org URL and project for a call: explicit arguments first,
    /// then what the DevOps tab saved for the folder, then detection.
    async fn devops_target(&self, t: &DevOpsTarget) -> Result<(String, String), String> {
        let mut org = t.org.clone().unwrap_or_default();
        let mut proj = t.devops_project.clone().unwrap_or_default();
        if let Some((o, p)) = devops_cli::split_devops_url(&org) {
            org = o;
            if proj.is_empty() {
                proj = p.unwrap_or_default();
            }
        }
        if org.is_empty() || proj.is_empty() {
            if let Ok(p) = self.project(t.project.as_deref()) {
                let cfg = config::load();
                if let Some(link) = p.config_keys.iter().find_map(|k| cfg.get_link(k)) {
                    if org.is_empty() {
                        org = link.devops_org.clone().unwrap_or_default();
                    }
                    if proj.is_empty() {
                        proj = link.devops_project.clone().unwrap_or_default();
                    }
                }
                if org.is_empty() || proj.is_empty() {
                    let dir = p.logic_apps_dir.clone();
                    if let Ok(Some(found)) = blocking(move || devops_cli::detect_target(&dir)).await
                    {
                        if org.is_empty() {
                            org = found.org;
                        }
                        if proj.is_empty() {
                            proj = found.project.unwrap_or_default();
                        }
                    }
                }
            }
        }
        if let Some((o, _)) = devops_cli::split_devops_url(&org) {
            org = o;
        }
        if org.is_empty() || proj.is_empty() {
            return Err(
                "Couldn't work out the Azure DevOps organization and project. Pass `org` \
                        and `devops_project`, or set them once in AIS Runner's DevOps tab."
                    .into(),
            );
        }
        Ok((org, proj))
    }
}

fn az_error(e: ais_core::cli::AzError) -> String {
    match e {
        ais_core::cli::AzError::NotLoggedIn => {
            "The Azure CLI session has expired. Run `az login` (or re-authenticate from \
             AIS Runner), then try again."
                .into()
        }
        ais_core::cli::AzError::Other(m) => m,
    }
}

#[tool_router(router = tool_router)]
impl AisMcp {
    fn new(default_project: Option<String>) -> Self {
        Self {
            default_project,
            tool_router: Self::tool_router(),
        }
    }

    /// The Logic Apps projects recently opened in AIS Runner, most recent first.
    #[tool(annotations(
        title = "List projects",
        read_only_hint = true,
        open_world_hint = false
    ))]
    async fn list_projects(&self) -> Result<String, String> {
        let cfg = config::load();
        let projects: Vec<Value> = cfg
            .recent_dirs
            .iter()
            .map(|d| json!({ "path": d, "exists": Path::new(d).is_dir() }))
            .collect();
        to_text(json!({
            "default": self.default_project.clone().or_else(|| cfg.recent_dirs.first().cloned()),
            "projects": projects,
        }))
    }

    /// Every workflow in a project with its trigger, read from disk, plus the
    /// health the local runtime reports when it is running and any
    /// workflow.json that fails to parse.
    #[tool(annotations(
        title = "List workflows",
        read_only_hint = true,
        open_world_hint = false
    ))]
    async fn list_workflows(
        &self,
        Parameters(a): Parameters<ProjectArg>,
    ) -> Result<String, String> {
        let p = self.project(a.project.as_deref())?;
        let dir = p.logic_apps_dir.clone();
        let (local, broken) = blocking(move || {
            (
                workflows::scan_local_workflows(&dir),
                workflows::scan_broken_workflows(&dir),
            )
        })
        .await?;
        let runtime = tokio::time::timeout(Duration::from_secs(3), workflows::list_workflows())
            .await
            .ok()
            .and_then(Result::ok);
        let list: Vec<Value> = local
            .iter()
            .map(|w| {
                let live = runtime
                    .as_ref()
                    .and_then(|r| r.iter().find(|x| x.name == w.name));
                json!({
                    "name": w.name,
                    "trigger": w.trigger_name,
                    "trigger_type": w.trigger_type,
                    "trigger_provider": w.trigger_provider,
                    "runtime_healthy": live.map(|l| l.healthy),
                    "runtime_disabled": live.map(|l| l.disabled),
                    "runtime_error": live.and_then(|l| l.health_error.clone()),
                })
            })
            .collect();
        let broken: Vec<Value> = broken
            .into_iter()
            .map(|(name, error)| json!({ "name": name, "error": error }))
            .collect();
        to_text(json!({
            "project": p.root,
            "logic_apps_dir": p.logic_apps_dir,
            "runtime_running": runtime.is_some(),
            "workflows": list,
            "unparseable": broken,
        }))
    }

    /// What a workflow touches — trigger, Service Bus queues, blob containers,
    /// outbound HTTP hosts, Liquid maps, SQL procedures — and whether it can run
    /// locally: settings still pointing at Azure, empty settings and missing
    /// connections.
    #[tool(annotations(
        title = "Describe workflow",
        read_only_hint = true,
        open_world_hint = false
    ))]
    async fn describe_workflow(
        &self,
        Parameters(a): Parameters<WorkflowArg>,
    ) -> Result<String, String> {
        let p = self.project(a.project.as_deref())?;
        let file = Path::new(&p.logic_apps_dir)
            .join(&a.workflow)
            .join("workflow.json");
        let raw = std::fs::read_to_string(&file)
            .map_err(|e| format!("Can't read {}: {e}", file.display()))?;
        let (dir, wf) = (p.logic_apps_dir.clone(), a.workflow.clone());
        let readiness = blocking(move || run_readiness::check(&dir, &wf)).await?;
        let an = workflow_analysis::analyse(&raw);
        let mut out = json!({
            "workflow": a.workflow,
            "trigger": trigger_json(&an.trigger),
            "service_bus_queues_read": an.input_queues,
            "service_bus_queues_written": an.output_queues,
            "blob_containers_read": an.input_blobs,
            "blob_containers_written": an.output_blobs,
            "http_hosts_called": an.http_calls,
            "liquid_maps": an.liquid_maps,
            "sql_procedures": an.sql_sprocs.iter()
                .map(|s| json!({ "name": s.name, "params": s.params }))
                .collect::<Vec<_>>(),
            "local_readiness": {
                "ready": readiness.is_ready(),
                "needs_manual_fix": readiness.needs_manual(),
                "empty_settings_with_local_default": readiness.auto_fixable.iter()
                    .map(|(c, k, v)| json!({ "connection": c, "setting": k, "local_default": redact(v) }))
                    .collect::<Vec<_>>(),
                "settings_pointing_at_azure": readiness.cloud_pointing.iter()
                    .map(|(k, cur, local)| json!({ "setting": k, "value": redact(cur), "local_target": redact(local) }))
                    .collect::<Vec<_>>(),
                "empty_settings_without_local_default": readiness.blocking_settings.iter()
                    .map(|(c, k)| json!({ "connection": c, "setting": k }))
                    .collect::<Vec<_>>(),
                "missing_connections": readiness.missing_connections,
                "note": "AIS Runner fixes the first two lists itself when ▶ func starts; the last two need the developer.",
            },
        });
        if a.include_definition {
            out["definition"] = serde_json::from_str(&raw).unwrap_or(Value::String(raw));
        }
        to_text(out)
    }

    /// Which tools AIS Runner needs are installed (func, azurite, az, node,
    /// mvn, Docker), and which parts of the local stack are up right now.
    #[tool(annotations(
        title = "Check environment",
        read_only_hint = true,
        open_world_hint = false
    ))]
    async fn check_environment(&self) -> Result<String, String> {
        let tools = blocking(system_check::check_tools).await?;
        let runtime = tokio::time::timeout(Duration::from_secs(3), workflows::list_workflows())
            .await
            .is_ok_and(|r| r.is_ok());
        to_text(json!({
            "tools": tools.iter().map(|t| json!({
                "name": t.name,
                "installed": t.available,
                "version": t.version,
                "install_hint": if t.available { None } else { Some(t.install_hint) },
            })).collect::<Vec<_>>(),
            "running": {
                "logic_apps_runtime (func, :7071)": runtime,
                "azurite (blob :10000)": port_open(10000).await,
                "service_bus_emulator (:5672)": port_open(5672).await,
            },
        }))
    }

    /// The most recent runs of a workflow in the local runtime. Needs ▶ func
    /// running in AIS Runner.
    #[tool(annotations(title = "List runs", read_only_hint = true, open_world_hint = false))]
    async fn list_runs(&self, Parameters(a): Parameters<RunsArg>) -> Result<String, String> {
        let runs = with_runtime(workflows::list_runs(&a.workflow)).await?;
        let limit = a.limit.unwrap_or(10).max(1);
        let list: Vec<Value> = runs
            .iter()
            .take(limit)
            .map(|r| {
                json!({
                    "run_id": r.name,
                    "status": r.properties.status,
                    "start": r.properties.start_time,
                    "end": r.properties.end_time,
                    "duration_ms": workflows::duration_ms(&r.properties.start_time, &r.properties.end_time),
                })
            })
            .collect();
        to_text(json!({ "workflow": a.workflow, "total": runs.len(), "runs": list }))
    }

    /// The actions of one run with their status and errors, and — when the run
    /// failed — the action that actually broke it. Needs ▶ func running.
    #[tool(annotations(title = "Get run", read_only_hint = true, open_world_hint = false))]
    async fn get_run(&self, Parameters(a): Parameters<RunArg>) -> Result<String, String> {
        let actions = with_runtime(workflows::list_actions(&a.workflow, &a.run_id)).await?;
        let status = workflows::run_status(&a.workflow, &a.run_id).await;
        let root_cause = if status.as_deref() == Some("Failed") {
            run_explain::explain(&a.workflow, &a.run_id).await
        } else {
            None
        };
        let list: Vec<Value> = actions
            .iter()
            .map(|x| {
                let p = &x.properties;
                json!({
                    "action": x.name,
                    "status": p.status,
                    "code": p.code,
                    "error": p.error.as_ref().map(|e| json!({ "code": e.code, "message": e.message })),
                    "duration_ms": workflows::duration_ms(&p.start_time, &p.end_time),
                })
            })
            .collect();
        to_text(json!({
            "workflow": a.workflow,
            "run_id": a.run_id,
            "status": status,
            "root_cause": root_cause,
            "actions": list,
        }))
    }

    /// The inputs and outputs of one action in a run, as text (long payloads
    /// are cut). Needs ▶ func running.
    #[tool(annotations(
        title = "Get action inputs/outputs",
        read_only_hint = true,
        open_world_hint = false
    ))]
    async fn get_action_io(&self, Parameters(a): Parameters<ActionArg>) -> Result<String, String> {
        let payload =
            with_runtime(workflows::action_payload(&a.workflow, &a.run_id, &a.action)).await?;
        // the runtime answers an unknown action name with an empty record
        if payload.status == "Unknown" && payload.text.trim().is_empty() {
            return Err(format!(
                "No action named '{}' in run {} of {}. get_run lists the run's actions.",
                a.action, a.run_id, a.workflow
            ));
        }
        to_text(json!({
            "action": a.action,
            "status": payload.status,
            "inputs_and_outputs": truncate_chars(&redact(&payload.text), MAX_PAYLOAD_CHARS),
        }))
    }

    /// The recorded test scenarios saved under the project's
    /// .ais-runner/scenarios folder, with their steps. Run them with the
    /// `ais-test` command or the app's Tests view.
    #[tool(annotations(
        title = "List scenarios",
        read_only_hint = true,
        open_world_hint = false
    ))]
    async fn list_scenarios(
        &self,
        Parameters(a): Parameters<ProjectArg>,
    ) -> Result<String, String> {
        let p = self.project(a.project.as_deref())?;
        let root = p.root.clone();
        let (found, errors) = blocking(move || scenario::discover(&root)).await?;
        let list: Vec<Value> = found
            .iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "description": s.description,
                    "file": s.source,
                    "steps": serde_json::to_string(&s.steps)
                        .ok()
                        .and_then(|t| serde_json::from_str::<Value>(&redact(&t)).ok())
                        .unwrap_or(Value::Null),
                })
            })
            .collect();
        to_text(json!({ "scenarios": list, "unreadable": errors }))
    }

    /// The Service Bus queues the project's workflows read from and write to,
    /// and which workflows use each — read from the workflow files.
    #[tool(annotations(
        title = "List Service Bus queues",
        read_only_hint = true,
        open_world_hint = false
    ))]
    async fn list_service_bus_queues(
        &self,
        Parameters(a): Parameters<ProjectArg>,
    ) -> Result<String, String> {
        let p = self.project(a.project.as_deref())?;
        let dir = p.logic_apps_dir.clone();
        let (namespace, queues) = blocking(move || sb_check::detect_sb_queues(&dir)).await?;
        to_text(json!({
            "namespace": namespace_label(&namespace),
            "queues": queues.iter().map(|q| json!({
                "queue": q.queue,
                "namespace": namespace_label(&q.namespace),
                "triggers": q.trigger_workflows,
                "used_by_actions_in": q.action_workflows,
                "requires_session": q.requires_session,
            })).collect::<Vec<_>>(),
        }))
    }

    /// The build pipelines of the project's Azure DevOps project. The org and
    /// project come from AIS Runner's DevOps tab, the folder's git remote or
    /// `az devops configure`, unless given. Uses the Azure CLI login.
    #[tool(annotations(
        title = "List DevOps pipelines",
        read_only_hint = true,
        open_world_hint = true
    ))]
    async fn devops_list_pipelines(
        &self,
        Parameters(a): Parameters<DevOpsTarget>,
    ) -> Result<String, String> {
        let (org, proj) = self.devops_target(&a).await?;
        let (o, p) = (org.clone(), proj.clone());
        let list = blocking(move || devops_cli::list_pipelines(&o, &p))
            .await?
            .map_err(az_error)?;
        to_text(json!({
            "org": org,
            "devops_project": proj,
            "pipelines": list.iter().map(|x| json!({
                "id": x.id, "name": x.name, "folder": x.folder,
            })).collect::<Vec<_>>(),
        }))
    }

    /// The 20 most recent runs (builds) of a pipeline.
    #[tool(annotations(
        title = "List pipeline runs",
        read_only_hint = true,
        open_world_hint = true
    ))]
    async fn devops_list_runs(
        &self,
        Parameters(a): Parameters<PipelineRunsArg>,
    ) -> Result<String, String> {
        let (org, proj) = self.devops_target(&a.target).await?;
        let id = a.pipeline_id;
        let runs = blocking(move || devops_cli::list_runs(&org, &proj, id))
            .await?
            .map_err(az_error)?;
        to_text(json!({
            "pipeline_id": id,
            "runs": runs.iter().map(|r| json!({
                "build_id": r.id,
                "build_number": r.name,
                "state": r.state,
                "result": r.result,
                "queued": r.created_date,
                "finished": r.finished_date,
                "branch": r.source_branch.trim_start_matches("refs/heads/"),
                "commit": r.source_version,
            })).collect::<Vec<_>>(),
        }))
    }

    /// A build's jobs and steps with their results and errors, plus the end of
    /// the log of its failed steps (or of the steps named by `step`).
    #[tool(annotations(title = "Get build log", read_only_hint = true, open_world_hint = true))]
    async fn devops_build_log(
        &self,
        Parameters(a): Parameters<BuildLogArg>,
    ) -> Result<String, String> {
        let (org, proj) = self.devops_target(&a.target).await?;
        let id = a.build_id;
        let (o, p) = (org.clone(), proj.clone());
        let jobs = blocking(move || devops_cli::build_timeline(&o, &p, id))
            .await?
            .map_err(az_error)?;
        let wanted = a.step.as_deref().map(str::to_lowercase);
        let picked: Vec<_> = jobs
            .iter()
            .flat_map(|j| j.steps.iter())
            .filter(|s| s.log_id.is_some())
            .filter(|s| match &wanted {
                Some(w) => s.name.to_lowercase().contains(w),
                None => s.result.as_deref() == Some("failed"),
            })
            .take(5)
            .cloned()
            .collect();
        let max = a.max_lines.unwrap_or(200).max(1);
        let mut logs = Vec::new();
        for step in picked {
            let (o, p, log_id) = (org.clone(), proj.clone(), step.log_id.unwrap_or_default());
            let text = blocking(move || devops_cli::build_log(&o, &p, id, log_id))
                .await?
                .map_err(az_error)?;
            let (skipped, tail) = tail_lines(&text, max);
            logs.push(json!({
                "step": step.name,
                "result": step.result,
                "lines_skipped": skipped,
                "log": tail,
            }));
        }
        to_text(json!({
            "build_id": id,
            "web_url": devops_cli::build_web_url(&org, &proj, id),
            "jobs": jobs.iter().map(|j| json!({
                "stage": j.stage,
                "job": j.job.name,
                "result": j.job.result,
                "steps": j.steps.iter().map(|s| json!({
                    "name": s.name,
                    "state": s.state,
                    "result": s.result,
                    "issues": s.issues.iter()
                        .map(|i| json!({ "type": i.kind, "message": i.message }))
                        .collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "logs": logs,
        }))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for AisMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("ais-runner", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "AIS Runner develops and tests Azure Logic Apps Standard workflows locally. \
                 These tools only read: a project's workflows and what they touch, whether \
                 each can run locally, the runs of the local runtime, saved test scenarios, \
                 and the Azure DevOps pipelines and build logs. Tools take an optional \
                 `project` folder and default to the project last opened in the app. Run \
                 tools need the local runtime, which the developer starts from the app \
                 (Azurite, then ▶ func). To find why a run failed: list_runs, then get_run \
                 (its root_cause), then get_action_io on that action.",
            )
    }
}

// ── main ──────────────────────────────────────────────────────────────────────

fn parse_args() -> Result<Option<String>, String> {
    let mut args = std::env::args().skip(1);
    let mut project = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                eprint!("{USAGE}");
                std::process::exit(0);
            }
            "--project" => {
                project = Some(args.next().ok_or("--project requires a value")?);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(project)
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let project = match parse_args() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return std::process::ExitCode::from(2);
        }
    };
    let server = match AisMcp::new(project).serve(rmcp::transport::stdio()).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ais-mcp: failed to start: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    if let Err(e) = server.waiting().await {
        eprintln!("ais-mcp: {e}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secrets_in_connection_strings_and_urls() {
        assert_eq!(
            redact("DefaultEndpointsProtocol=https;AccountName=st;AccountKey=abc+/==;EndpointSuffix=core.windows.net"),
            "DefaultEndpointsProtocol=https;AccountName=st;AccountKey=***;EndpointSuffix=core.windows.net"
        );
        assert_eq!(
            redact("Endpoint=sb://sbns-foo.servicebus.windows.net/;SharedAccessKeyName=Root;SharedAccessKey=k3y="),
            "Endpoint=sb://sbns-foo.servicebus.windows.net/;SharedAccessKeyName=Root;SharedAccessKey=***"
        );
        assert_eq!(
            redact("https://func-foo.azurewebsites.net/api/x?code=s3cr3t&other=1"),
            "https://func-foo.azurewebsites.net/api/x?code=***&other=1"
        );
        assert_eq!(
            redact("https://prod.westeurope.logic.azure.com/workflows/1/triggers/manual/paths/invoke?sp=%2Ftriggers&sv=1.0&sig=AbC"),
            "https://prod.westeurope.logic.azure.com/workflows/1/triggers/manual/paths/invoke?sp=%2Ftriggers&sv=1.0&sig=***"
        );
        assert_eq!(
            redact("Server=tcp:x;User ID=u;Password=p@ss;"),
            "Server=tcp:x;User ID=u;Password=***;"
        );
        assert_eq!(redact("https://api.example.com"), "https://api.example.com");
    }

    #[test]
    fn namespace_shows_the_host_only() {
        assert_eq!(
            namespace_label(
                "Endpoint=sb://sbns-foo.servicebus.windows.net/;SharedAccessKeyName=R;SharedAccessKey=k"
            ),
            "sbns-foo.servicebus.windows.net"
        );
    }

    #[test]
    fn tail_keeps_the_end() {
        assert_eq!(tail_lines("a\nb\nc", 2), (1, "b\nc".to_string()));
        assert_eq!(tail_lines("a", 5), (0, "a".to_string()));
        assert_eq!(
            tail_lines(
                "2026-10-06T13:17:06.3318142Z ##[error]Bash exited with code '1'.",
                5
            ),
            (0, "##[error]Bash exited with code '1'.".to_string())
        );
    }

    #[test]
    fn truncation_keeps_both_ends() {
        assert_eq!(truncate_chars("héllo", 10), "héllo");
        let cut = truncate_chars("inputs-é-………………………………-outputs-error", 20);
        assert!(cut.starts_with("inputs"), "{cut}");
        assert!(cut.ends_with("outputs-error"), "{cut}");
        assert!(cut.contains("characters cut from the middle"), "{cut}");
    }

    #[test]
    fn every_tool_is_marked_read_only() {
        let server = AisMcp::new(None);
        let tools = server.tool_router.list_all();
        assert!(tools.len() >= 12);
        for t in tools {
            let ro = t.annotations.as_ref().and_then(|a| a.read_only_hint);
            assert_eq!(ro, Some(true), "{} must be read-only", t.name);
        }
    }
}
