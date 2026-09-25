use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

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
    pub resource_group: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    pub logic_app_name: Option<String>,
    pub sb_namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devops_org: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devops_project: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub recent_dirs: Vec<String>,
    pub workspace_links: HashMap<String, WorkspaceLink>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub graph_prefs: HashMap<String, GraphPrefs>,
    /// Last payload body the user entered, keyed by logic_apps_dir → workflow_name.
    /// Survives restarts so users don't lose their crafted test bodies. Stored in
    /// the OS config dir (NOT in the project workspace).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub last_payloads: HashMap<String, HashMap<String, String>>,
    /// Whether to fire native OS notifications (run succeeded/failed/timed out).
    /// Defaults to on, both for fresh installs and for existing configs
    /// written before this field existed.
    #[serde(default = "default_true")]
    pub notifications_enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            recent_dirs: Vec::new(),
            workspace_links: HashMap::new(),
            graph_prefs: HashMap::new(),
            last_payloads: HashMap::new(),
            notifications_enabled: true,
        }
    }
}

impl AppConfig {
    pub fn push_dir(&mut self, dir: String) {
        self.recent_dirs.retain(|d| d != &dir); // remove duplicate
        self.recent_dirs.insert(0, dir); // most recent first
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

    pub fn get_last_payload(&self, dir: &str, workflow: &str) -> Option<String> {
        self.last_payloads
            .get(dir)
            .and_then(|m| m.get(workflow))
            .cloned()
    }

    pub fn set_last_payload(&mut self, dir: String, workflow: String, body: String) {
        self.last_payloads
            .entry(dir)
            .or_default()
            .insert(workflow, body);
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

/// The subscription to use for a workspace.
///
/// What the project declares in its own `local.settings.json` wins — that is
/// the value committed alongside the workflows, so it is right for anyone who
/// checks the repo out. Only when the project declares nothing do we fall back
/// to the subscription pinned locally when this workspace was linked.
///
/// The two halves live apart on purpose: the first reads the workspace and is
/// usable without a runner install, the second is this app's own state.
pub fn subscription_for(logic_apps_dir: &str) -> Option<String> {
    ais_core::sync::detect_subscription(logic_apps_dir).or_else(|| {
        load()
            .get_link(logic_apps_dir)
            .map(|l| l.subscription_id.clone())
    })
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
    dialog
        .pick_folder()
        .map(|p| p.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_written_before_the_flag_existed_keeps_notifications_on() {
        // `bool`'s own Default is false, so without the explicit
        // `default = "default_true"` every existing install would silently go
        // mute the first time it read its config back. This is what guards it.
        let older = r#"{ "recent_dirs": [], "workspace_links": {} }"#;
        let cfg: AppConfig = serde_json::from_str(older).unwrap();
        assert!(cfg.notifications_enabled);
    }

    #[test]
    fn an_explicit_opt_out_survives_a_round_trip() {
        let mut cfg = AppConfig::default();
        assert!(cfg.notifications_enabled, "fresh installs start enabled");

        cfg.notifications_enabled = false;
        let back: AppConfig = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert!(!back.notifications_enabled);
    }
}
