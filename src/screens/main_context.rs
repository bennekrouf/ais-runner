use dioxus::prelude::*;
use std::collections::HashSet;
use crate::services::process::ServiceState;
use crate::components::log_panel::LogLine;
use crate::services::workflows::{WorkflowItem, RunItem};
use std::sync::Arc;
use crate::services::process::ManagedProcess;

/// Central context holding all MainScreen signals
/// This prevents re-mounts when signals change
#[derive(Clone)]
pub struct MainContext {
    // Service states
    pub azurite_state: Signal<ServiceState>,
    pub func_state: Signal<ServiceState>,
    pub java_func_state: Signal<ServiceState>,
    pub sb_emu_state: Signal<ServiceState>,
    pub cosmos_emu_state: Signal<ServiceState>,
    pub sql_dev_state: Signal<ServiceState>,

    // Service processes
    pub azurite_proc: Signal<Arc<ManagedProcess>>,
    pub func_proc: Signal<Arc<ManagedProcess>>,
    pub java_func_proc: Signal<Arc<ManagedProcess>>,
    pub sb_emu_proc: Signal<Arc<ManagedProcess>>,
    pub cosmos_emu_proc: Signal<Arc<ManagedProcess>>,
    pub sql_dev_proc: Signal<Arc<ManagedProcess>>,

    // Log lines
    pub log_lines: Signal<Vec<LogLine>>,
    pub sb_emu_lines: Signal<Vec<String>>,
    pub java_func_lines: Signal<Vec<String>>,
    pub sql_dev_lines: Signal<Vec<String>>,
    pub az_lines: Signal<Vec<String>>,

    // Workflow data
    pub workflows: Signal<Vec<WorkflowItem>>,
    pub runs: Signal<Vec<RunItem>>,
    pub selected_wf: Signal<Option<String>>,
    pub source_text: Signal<String>,

    // Connection data
    pub sql_wfs: Signal<HashSet<String>>,
    pub msi_wfs: Signal<HashSet<String>>,
    pub sql_conns: Signal<Vec<crate::services::sql_check::SqlConnection>>,
    pub sb_namespace: Signal<String>,
    pub sb_queues: Signal<Vec<crate::services::sb_check::SbQueueInfo>>,
    pub sb_namespace_key: Signal<Option<String>>,
    pub sb_conn_str: Signal<Option<(String, String)>>,
    pub blob_conns: Signal<Vec<crate::services::blob_check::BlobConnection>>,
    pub cosmos_conns: Signal<Vec<crate::services::cosmos_check::CosmosConnection>>,
    pub webjobs_storage: Signal<String>,

    // Theme (moved from App level to prevent re-mounts when accessed)
    pub is_light: Signal<bool>,
    pub theme_overridden: Signal<bool>,
}

impl MainContext {
    pub fn new() -> Self {
        Self {
            azurite_state: Signal::new(ServiceState::Stopped),
            func_state: Signal::new(ServiceState::Stopped),
            java_func_state: Signal::new(ServiceState::Stopped),
            sb_emu_state: Signal::new(ServiceState::Stopped),
            cosmos_emu_state: Signal::new(ServiceState::Stopped),
            sql_dev_state: Signal::new(ServiceState::Stopped),

            azurite_proc: Signal::new(Arc::new(ManagedProcess::new())),
            func_proc: Signal::new(Arc::new(ManagedProcess::new())),
            java_func_proc: Signal::new(Arc::new(ManagedProcess::new())),
            sb_emu_proc: Signal::new(Arc::new(ManagedProcess::new())),
            cosmos_emu_proc: Signal::new(Arc::new(ManagedProcess::new())),
            sql_dev_proc: Signal::new(Arc::new(ManagedProcess::new())),

            log_lines: Signal::new(Vec::new()),
            sb_emu_lines: Signal::new(Vec::new()),
            java_func_lines: Signal::new(Vec::new()),
            sql_dev_lines: Signal::new(Vec::new()),
            az_lines: Signal::new(Vec::new()),

            workflows: Signal::new(Vec::new()),
            runs: Signal::new(Vec::new()),
            selected_wf: Signal::new(None),
            source_text: Signal::new(String::new()),

            sql_wfs: Signal::new(HashSet::new()),
            msi_wfs: Signal::new(HashSet::new()),
            sql_conns: Signal::new(Vec::new()),
            sb_namespace: Signal::new(String::new()),
            sb_queues: Signal::new(Vec::new()),
            sb_namespace_key: Signal::new(None),
            sb_conn_str: Signal::new(None),
            blob_conns: Signal::new(Vec::new()),
            cosmos_conns: Signal::new(Vec::new()),
            webjobs_storage: Signal::new(String::new()),

            is_light: Signal::new(true),
            theme_overridden: Signal::new(false),
        }
    }
}
