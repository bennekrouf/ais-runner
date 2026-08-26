use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};

use crate::components::log_panel::{LogLevel, LogLine};
use crate::services::workflows::{self, ActionItem, RunItem};
use crate::utils::{filter_cleared, make_push};

pub fn handle_select(
    name: String,
    mut selected_wf: Signal<Option<String>>,
    mut runs: Signal<Vec<RunItem>>,
    mut actions: Signal<Vec<ActionItem>>,
    mut source_text: Signal<String>,
    mut traced_wfs: Signal<HashSet<String>>,
    cleared_wfs: Signal<HashMap<String, String>>,
    log_lines: Signal<Vec<LogLine>>,
    dir: &str,
) {
    let wf = name.clone();
    selected_wf.set(Some(name.clone()));
    actions.set(vec![]);

    let src_path = workflows::resolve_logic_apps_dir(dir)
        .join(&name)
        .join("workflow.json");
    source_text.set(match std::fs::read_to_string(&src_path) {
        Ok(txt) => txt,
        Err(e) => format!("// could not read {}: {}", src_path.display(), e),
    });

    let cleared_at = cleared_wfs.read().get(&wf).cloned();
    let mut push = make_push(log_lines);

    spawn(async move {
        match workflows::list_runs(&wf).await {
            Ok(r) => {
                let r = filter_cleared(r, cleared_at.as_deref());
                if !r.is_empty() {
                    traced_wfs.write().insert(wf.clone());
                } else {
                    traced_wfs.write().remove(&wf);
                }
                if let Some(latest) = r.first() {
                    if let Ok(a) = workflows::list_actions(&wf, &latest.name).await {
                        actions.set(a);
                    }
                }
                runs.set(r);
            }
            Err(e) => push(format!("Runs error: {}", e), LogLevel::Error),
        }
    });
}

pub fn handle_refresh(
    selected_wf: Signal<Option<String>>,
    mut runs: Signal<Vec<RunItem>>,
    mut actions: Signal<Vec<ActionItem>>,
    cleared_wfs: Signal<std::collections::HashMap<String, String>>,
    log_lines: Signal<Vec<LogLine>>,
) {
    let Some(wf) = selected_wf.read().clone() else {
        return;
    };
    let cleared_at = cleared_wfs.read().get(&wf).cloned();
    let mut push = make_push(log_lines);
    spawn(async move {
        match workflows::list_runs(&wf).await {
            Ok(r) => {
                let r = filter_cleared(r, cleared_at.as_deref());
                if let Some(latest) = r.first() {
                    match workflows::list_actions(&wf, &latest.name).await {
                        Ok(a) => actions.set(a),
                        Err(e) => push(format!("Actions error: {}", e), LogLevel::Error),
                    }
                } else {
                    actions.write().clear();
                }
                runs.set(r);
            }
            Err(e) => push(format!("Refresh failed: {}", e), LogLevel::Error),
        }
    });
}

pub fn handle_select_run(
    run_id: String,
    selected_wf: Signal<Option<String>>,
    mut actions: Signal<Vec<ActionItem>>,
    log_lines: Signal<Vec<LogLine>>,
) {
    let Some(wf) = selected_wf.read().clone() else {
        return;
    };
    let mut push = make_push(log_lines);
    spawn(async move {
        match workflows::list_actions(&wf, &run_id).await {
            Ok(a) => actions.set(a),
            Err(e) => push(format!("Actions error: {}", e), LogLevel::Error),
        }
    });
}
