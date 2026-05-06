use chrono::Local;
use std::collections::{HashMap, HashSet};
use dioxus::prelude::{Readable, Writable};

use crate::components::log_panel::{LogLevel, LogLine};
use crate::services::workflows;

pub fn now() -> String {
    Local::now().format("%H:%M:%S").to_string()
}

/// Build a `push_log` closure that writes into a Dioxus signal.
pub fn make_push(
    mut log_lines: dioxus::prelude::Signal<Vec<LogLine>>,
) -> impl FnMut(String, LogLevel) + 'static {
    move |msg: String, level: LogLevel| {
        log_lines.write().push(LogLine { time: now(), msg, level });
    }
}

/// Keep only runs whose start_time is after `cleared_at`.
pub fn filter_cleared(
    runs: Vec<workflows::RunItem>,
    cleared_at: Option<&str>,
) -> Vec<workflows::RunItem> {
    let Some(ts) = cleared_at else { return runs };
    runs.into_iter()
        .filter(|r| r.properties.start_time.as_deref().map(|s| s > ts).unwrap_or(false))
        .collect()
}

/// Background sweep: populate `traced_wfs` for workflows that have run history.
pub async fn sweep_run_history(
    names: Vec<String>,
    traced: &mut dioxus::prelude::Signal<HashSet<String>>,
    cleared: &dioxus::prelude::Signal<HashMap<String, String>>,
) {
    for name in names {
        if workflows::check_has_runs(&name).await {
            let cleared_at = cleared.read().get(&name).cloned();
            if cleared_at.is_none() {
                traced.write().insert(name);
            }
        }
    }
}
