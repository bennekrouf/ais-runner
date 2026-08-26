use chrono::Local;
use dioxus::prelude::{Readable, Writable};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::components::log_panel::{LogLevel, LogLine};
use crate::services::workflows;

/// Open a file path in the best available editor.
/// Tries VS Code first (works for any file type), then falls back to the
/// OS default text editor.
pub fn open_in_editor(path: &str) {
    #[cfg(target_os = "macos")]
    {
        // Prefer VS Code if installed, otherwise force text editor with -t
        if std::process::Command::new("code")
            .arg(path)
            .spawn()
            .is_err()
        {
            let _ = std::process::Command::new("open")
                .args(["-t", path])
                .spawn();
        }
    }
    #[cfg(target_os = "windows")]
    {
        // Prefer VS Code, fall back to notepad
        if std::process::Command::new("code")
            .arg(path)
            .spawn()
            .is_err()
        {
            let _ = std::process::Command::new("notepad.exe").arg(path).spawn();
        }
    }
    #[cfg(target_os = "linux")]
    {
        // Prefer VS Code, fall back to xdg-open
        if std::process::Command::new("code")
            .arg(path)
            .spawn()
            .is_err()
        {
            let _ = std::process::Command::new("xdg-open").arg(path).spawn();
        }
    }
}

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

/// Format a Logic Apps RFC3339 timestamp (always UTC, e.g.
/// `2026-06-18T15:45:36.7891234Z`) as a local-time `YYYY-MM-DD HH:MM:SS` string.
///
/// Returns the original string trimmed to its first 19 characters if parsing
/// fails — that way unexpected formats degrade to the previous behaviour
/// rather than rendering an empty cell.
pub fn fmt_utc_as_local(rfc3339: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(rfc3339) {
        Ok(dt) => dt
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
        Err(_) => rfc3339
            .chars()
            .take(19)
            .collect::<String>()
            .replace('T', " "),
    }
}

/// Build a `push_log` closure that writes into a Dioxus signal.
///
/// Caps the buffer so a long-running session can't accumulate unbounded log
/// lines and freeze the UI — the SB emulator alone can emit thousands of
/// session-idle traces per hour.
pub fn make_push(
    mut log_lines: dioxus::prelude::Signal<Vec<LogLine>>,
) -> impl FnMut(String, LogLevel) + 'static {
    const MAX_LOG_LINES: usize = 2000;
    move |msg: String, level: LogLevel| {
        let mut w = log_lines.write();
        w.push(LogLine {
            time: now(),
            msg,
            level,
        });
        let len = w.len();
        if len > MAX_LOG_LINES {
            w.drain(..len - MAX_LOG_LINES);
        }
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
    let Ok(cleared_dt) = chrono::DateTime::parse_from_rfc3339(ts) else {
        return runs;
    };
    let threshold = cleared_dt - chrono::Duration::seconds(2);
    runs.into_iter()
        .filter(|r| {
            r.properties
                .start_time
                .as_deref()
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
