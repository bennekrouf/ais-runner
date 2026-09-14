//! A message from us to the people running this build.
//!
//! The builds are free and the download logs are anonymous, so this is the one
//! channel that reaches exactly the people using the product: a small JSON
//! file next to the builds, fetched once at startup and shown as a banner
//! until dismissed. Used for the founding-users ask ("it's free — we'd love
//! your feedback"), not for release news, which the update check covers.
//!
//! Best-effort throughout: no network, no file, or a malformed file all mean
//! no banner. Never panics, never slows startup.
//!
//! Served from mayorana.ch:
//!   /downloads/ais-runner/notice.json   — this product only (wins when present)
//!   /downloads/notice.json           — every product
//!
//! ```json
//! { "id": "founding-2026-09",
//!   "apps": null,                       // or ["ais-monitor", ...] to target
//!   "text": "You're one of the first people running {app}. ...",
//!   "link_text": "Tell us how it's going",
//!   "url": "https://mayorana.ch/en/contact" }
//! ```
//!
//! `id` is what a dismissal remembers, so a new message needs a new id.

use serde::Deserialize;
use std::path::PathBuf;

const APP: &str = "ais-runner";
const APP_NAME: &str = "AIS Runner";

const APP_NOTICE_URL: &str = "https://mayorana.ch/downloads/ais-runner/notice.json";
const GLOBAL_NOTICE_URL: &str = "https://mayorana.ch/downloads/notice.json";

/// Distinct from the updater's agent on purpose: the download statistics
/// count `(updater)` polls as live installs, and this fetch is not one.
const USER_AGENT: &str = concat!("ais-runner/", env!("CARGO_PKG_VERSION"), " (notice)");

#[derive(Debug, Clone, Deserialize)]
pub struct Notice {
    pub id: String,
    #[serde(default)]
    pub apps: Option<Vec<String>>,
    pub text: String,
    #[serde(default)]
    pub link_text: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// Returns the notice to show, or `None` — not published, not for this
/// product, already dismissed, or unreachable.
pub async fn fetch() -> Option<Notice> {
    if std::env::var("DISABLE_UPDATE_CHECK").is_ok() {
        return None;
    }
    let notice = match get(APP_NOTICE_URL).await {
        Some(n) => n,
        None => get(GLOBAL_NOTICE_URL).await?,
    };
    if let Some(apps) = &notice.apps {
        if !apps.iter().any(|a| a == APP) {
            return None;
        }
    }
    if dismissed_ids().iter().any(|id| id == &notice.id) {
        return None;
    }
    Some(Notice {
        text: notice.text.replace("{app}", APP_NAME),
        ..notice
    })
}

/// Remember that this notice was closed, so it does not come back at the next
/// start. Losing the file (or failing to write it) only means asking again.
pub fn dismiss(id: &str) {
    let mut ids = dismissed_ids();
    if ids.iter().any(|d| d == id) {
        return;
    }
    ids.push(id.to_string());
    if let Some(path) = dismissed_path() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string(&ids) {
            let _ = std::fs::write(path, json);
        }
    }
}

async fn get(url: &str) -> Option<Notice> {
    let body = reqwest::Client::new()
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .text()
        .await
        .ok()?;
    serde_json::from_str(&body).ok()
}

/// One file for every mayorana product rather than one per app: the notice is
/// usually the same message, and dismissing it in one tool should not mean
/// seeing it again in the next.
fn dismissed_path() -> Option<PathBuf> {
    Some(
        dirs::data_local_dir()?
            .join("mayorana")
            .join("dismissed-notices.json"),
    )
}

fn dismissed_ids() -> Vec<String> {
    dismissed_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}
