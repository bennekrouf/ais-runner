use chrono::Local;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use dioxus::prelude::{Readable, Writable};

use crate::components::log_panel::{LogLevel, LogLine};
use crate::services::workflows;

/// Cross-platform temp directory for Azurite data.
/// - macOS/Linux: `/tmp/azurite`   (Docker-accessible, consistent across sessions)
/// - Windows:     `%TEMP%\azurite`
pub fn azurite_dir() -> PathBuf {
    if cfg!(target_os = "windows") {
        std::env::temp_dir().join("azurite")
    } else {
        PathBuf::from("/tmp/azurite")
    }
}

/// Path to Azurite's debug log file.
pub fn azurite_log() -> PathBuf {
    azurite_dir().join("debug.log")
}

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
///
/// Uses proper datetime parsing with a 2-second tolerance to handle the
/// timestamp format mismatch between Rust's rfc3339 (`+00:00` suffix, nanoseconds)
/// and the Logic Apps runtime's format (`Z` suffix, milliseconds). Without the
/// tolerance the run can appear to precede cleared_at and get filtered forever.
pub fn filter_cleared(
    runs: Vec<workflows::RunItem>,
    cleared_at: Option<&str>,
) -> Vec<workflows::RunItem> {
    let Some(ts) = cleared_at else { return runs };
    let Ok(cleared_dt) = chrono::DateTime::parse_from_rfc3339(ts) else { return runs };
    let threshold = cleared_dt - chrono::Duration::seconds(2);
    runs.into_iter()
        .filter(|r| {
            r.properties.start_time.as_deref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt > threshold)
                .unwrap_or(false)
        })
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
