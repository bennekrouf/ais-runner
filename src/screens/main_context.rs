use crate::components::log_panel::LogLine;
use crate::services::process::ManagedProcess;
use crate::services::process::ServiceState;
use crate::services::workflows::{self, RunItem, WorkflowItem};
use crate::services::{azure_cli, config, env_mode, setup_manager, system_check};
use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Central context holding all MainScreen signals
/// This prevents re-mounts when signals change
#[derive(Clone)]
pub struct MainContext {
    // App config snapshot (loaded once per project open)
    pub cfg: Signal<config::AppConfig>,

    // Service states
    pub azurite_state: Signal<ServiceState>,
    pub func_state: Signal<ServiceState>,
    pub java_func_state: Signal<ServiceState>,
    pub sb_emu_state: Signal<ServiceState>,
    pub cosmos_emu_state: Signal<ServiceState>,
    pub sql_dev_state: Signal<ServiceState>,
    pub mock_state: Signal<ServiceState>,

    // Service processes
    pub azurite_proc: Signal<Arc<ManagedProcess>>,
    pub func_proc: Signal<Arc<ManagedProcess>>,
    pub java_func_proc: Signal<Arc<ManagedProcess>>,
    pub sb_emu_proc: Signal<Arc<ManagedProcess>>,
    pub cosmos_emu_proc: Signal<Arc<ManagedProcess>>,
    pub sql_dev_proc: Signal<Arc<ManagedProcess>>,

    /// Live mock runtime. Not a `ManagedProcess` — the mock server is an
    /// in-process axum task, not a child process.
    pub mock_handle: Signal<crate::handlers::mock_server::MockHandle>,

    // Log lines
    pub log_lines: Signal<Vec<LogLine>>,
    pub sb_emu_lines: Signal<Vec<String>>,
    pub java_func_lines: Signal<Vec<String>>,
    pub sql_dev_lines: Signal<Vec<String>>,
    pub az_lines: Signal<Vec<String>>,

    // Workflow data
    pub workflows: Signal<Vec<WorkflowItem>>,
    pub runs: Signal<Vec<RunItem>>,
    pub actions: Signal<Vec<workflows::ActionItem>>,
    pub selected_wf: Signal<Option<String>>,
    pub source_text: Signal<String>,
    pub running_wfs: Signal<HashSet<String>>,
    pub traced_wfs: Signal<HashSet<String>>,
    pub cleared_wfs: Signal<HashMap<String, String>>,
    pub last_ran: Signal<HashMap<String, u64>>,
    pub wf_connectors: Signal<HashMap<String, Vec<workflows::ConnectorKind>>>,
    // sproc qualified name → Some(exists) once checked, None while loading.
    pub sproc_status: Signal<HashMap<String, Option<bool>>>,

    // Run dialog: (wf, method, url, body, callback_url, relative_path)
    pub run_dialog: Signal<
        Option<(
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
        )>,
    >,

    // Blocking local-readiness gate shown when a workflow's connections aren't
    // set up for the local emulators. Some(readiness) → the consent modal is open.
    pub run_gate: Signal<Option<crate::services::run_readiness::RunReadiness>>,

    // View / tab state — lives here so panel caches survive re-renders.
    pub current_view: Signal<String>,
    pub active_tab: Signal<String>,
    pub visited_views: Signal<HashSet<String>>,
    pub db_panel_open: Signal<bool>,
    pub azure_panel_open: Signal<bool>,
    /// Some(()) while the "link this workspace to Azure" chooser is up.
    pub azure_link_open: Signal<bool>,
    pub auto_watch: Signal<bool>,

    // Scenario recording. Lives here rather than in the Tests view because the
    // actions being captured happen in the Connectors panel and the workflow
    // list — every panel needs to reach it without four layers of prop drilling.
    pub recorder: Signal<crate::services::recorder::RecorderState>,

    // Setup / environment
    pub setup_status: Signal<setup_manager::SetupStatus>,
    pub setup_updates: Signal<HashMap<String, String>>,
    pub current_env: Signal<env_mode::EnvMode>,

    // Connection data
    pub sql_wfs: Signal<HashSet<String>>,
    pub msi_wfs: Signal<HashSet<String>>,
    pub sql_conns: Signal<Vec<crate::services::sql_check::SqlConnection>>,
    pub sb_namespace: Signal<String>,
    pub sb_queues: Signal<Vec<crate::services::sb_check::SbQueueInfo>>,
    pub sb_namespace_key: Signal<Option<String>>,
    pub sb_conn_str: Signal<Option<(String, String)>>,
    pub sftp_conns: Signal<Vec<crate::services::sftp_check::SftpConnection>>,
    pub blob_conns: Signal<Vec<crate::services::blob_check::BlobConnection>>,
    pub cosmos_conns: Signal<Vec<crate::services::cosmos_check::CosmosConnection>>,
    pub webjobs_storage: Signal<String>,

    // Azure panel diff cache — survives panel close/reopen.
    pub az_diff_cache: Signal<HashMap<String, crate::components::azure_panel::DiffStatus>>,

    // Tool check / Azure login
    pub tool_statuses: Signal<Vec<system_check::ToolStatus>>,
    pub tools_dismissed: Signal<bool>,
    pub az_status: Signal<Option<Result<String, azure_cli::AzError>>>,
    pub active_tenant: Signal<Option<String>>,

    // Theme — these are the App-level signal handles, shared (not copies) so
    // the MainScreen toggle and the body-class effect in main.rs stay in sync.
    pub is_light: Signal<bool>,
    pub theme_overridden: Signal<bool>,
}

impl MainContext {
    pub fn new(is_light: Signal<bool>, theme_overridden: Signal<bool>) -> Self {
        Self {
            cfg: Signal::new(config::load()),

            azurite_state: Signal::new(ServiceState::Stopped),
            func_state: Signal::new(ServiceState::Stopped),
            java_func_state: Signal::new(ServiceState::Stopped),
            sb_emu_state: Signal::new(ServiceState::Stopped),
            cosmos_emu_state: Signal::new(ServiceState::Stopped),
            sql_dev_state: Signal::new(ServiceState::Stopped),
            mock_state: Signal::new(ServiceState::Stopped),

            azurite_proc: Signal::new(Arc::new(ManagedProcess::new())),
            func_proc: Signal::new(Arc::new(ManagedProcess::new())),
            java_func_proc: Signal::new(Arc::new(ManagedProcess::new())),
            sb_emu_proc: Signal::new(Arc::new(ManagedProcess::new())),
            cosmos_emu_proc: Signal::new(Arc::new(ManagedProcess::new())),
            sql_dev_proc: Signal::new(Arc::new(ManagedProcess::new())),

            mock_handle: Signal::new(crate::handlers::mock_server::new_handle()),

            log_lines: Signal::new(Vec::new()),
            sb_emu_lines: Signal::new(Vec::new()),
            java_func_lines: Signal::new(Vec::new()),
            sql_dev_lines: Signal::new(Vec::new()),
            az_lines: Signal::new(Vec::new()),

            workflows: Signal::new(Vec::new()),
            runs: Signal::new(Vec::new()),
            actions: Signal::new(Vec::new()),
            selected_wf: Signal::new(None),
            source_text: Signal::new(String::new()),
            running_wfs: Signal::new(HashSet::new()),
            traced_wfs: Signal::new(HashSet::new()),
            cleared_wfs: Signal::new(HashMap::new()),
            last_ran: Signal::new(HashMap::new()),
            wf_connectors: Signal::new(HashMap::new()),
            sproc_status: Signal::new(HashMap::new()),

            run_dialog: Signal::new(None),
            run_gate: Signal::new(None),

            current_view: Signal::new("Workflows".to_string()),
            active_tab: Signal::new("Source".to_string()),
            visited_views: Signal::new({
                let mut s = HashSet::new();
                s.insert("Workflows".to_string());
                s
            }),
            db_panel_open: Signal::new(false),
            azure_panel_open: Signal::new(false),
            azure_link_open: Signal::new(false),
            auto_watch: Signal::new(true),

            recorder: Signal::new(Default::default()),

            setup_status: Signal::new(setup_manager::SetupStatus::MissingSettings),
            setup_updates: Signal::new(HashMap::new()),
            current_env: Signal::new(env_mode::EnvMode::Local),

            sql_wfs: Signal::new(HashSet::new()),
            msi_wfs: Signal::new(HashSet::new()),
            sql_conns: Signal::new(Vec::new()),
            sb_namespace: Signal::new(String::new()),
            sb_queues: Signal::new(Vec::new()),
            sb_namespace_key: Signal::new(None),
            sb_conn_str: Signal::new(None),
            sftp_conns: Signal::new(Vec::new()),
            blob_conns: Signal::new(Vec::new()),
            cosmos_conns: Signal::new(Vec::new()),
            webjobs_storage: Signal::new(String::new()),

            az_diff_cache: Signal::new(HashMap::new()),

            tool_statuses: Signal::new(Vec::new()),
            tools_dismissed: Signal::new(false),
            az_status: Signal::new(None),
            active_tenant: Signal::new(None),

            is_light,
            theme_overridden,
        }
    }
}
