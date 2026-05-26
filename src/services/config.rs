use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::collections::{HashMap, HashSet};

const MAX_RECENT: usize = 5;

/// Persisted graph-panel preferences, keyed by logic_apps_dir in AppConfig.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct GraphPrefs {
    /// Last selected chain pill ("All" or a workflow name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_chain: Option<String>,
    /// Node IDs hidden via the filter panel.
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub excluded_nodes: HashSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct WorkspaceLink {
    pub subscription_id: String,
    pub resource_group:  String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id:       Option<String>,
    pub logic_app_name:  Option<String>,
    pub sb_namespace:    Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devops_org:     Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devops_project: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    pub recent_dirs: Vec<String>,
    pub workspace_links: HashMap<String, WorkspaceLink>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub graph_prefs: HashMap<String, GraphPrefs>,
}

impl AppConfig {
    pub fn push_dir(&mut self, dir: String) {
        self.recent_dirs.retain(|d| d != &dir); // remove duplicate
        self.recent_dirs.insert(0, dir);         // most recent first
        self.recent_dirs.truncate(MAX_RECENT);
    }

    pub fn get_link(&self, dir: &str) -> Option<&WorkspaceLink> {
        self.workspace_links.get(dir)
    }

    pub fn set_link(&mut self, dir: String, link: WorkspaceLink) {
        self.workspace_links.insert(dir, link);
    }

    pub fn get_graph_prefs(&self, dir: &str) -> GraphPrefs {
        self.graph_prefs.get(dir).cloned().unwrap_or_default()
    }

    pub fn set_graph_prefs(&mut self, dir: String, prefs: GraphPrefs) {
        self.graph_prefs.insert(dir, prefs);
    }
}

fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ais-runner")
        .join("config.json")
}

pub fn load() -> AppConfig {
    let path = config_path();
    if let Ok(content) = fs::read_to_string(&path) {
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        AppConfig::default()
    }
}

pub fn save(cfg: &AppConfig) {
    let path = config_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(cfg) {
        let _ = fs::write(&path, json);
    }
}

pub fn pick_folder(current: Option<&str>) -> Option<String> {
    let mut dialog = rfd::FileDialog::new().set_title("Select AIS Platform folder");
    if let Some(dir) = current {
        dialog = dialog.set_directory(dir);
    }
    dialog.pick_folder().map(|p| p.to_string_lossy().to_string())
}
