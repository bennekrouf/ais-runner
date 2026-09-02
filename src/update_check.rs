//! Lightweight update check.
//!
//! Fetches the `latest.json` published with each release and compares
//! the version field to this build's `CARGO_PKG_VERSION`. Designed to be
//! cheap and side-effect-free so it can run in the background at startup.

use serde::Deserialize;
use std::collections::HashMap;

/// Served from mayorana.ch alongside the builds it describes. It used to be
/// fetched from the GitHub release, which meant update checks broke for every
/// installed copy the moment that repo stopped being publicly readable — the
/// app should not depend on the source host to tell users a build exists.
const LATEST_URL: &str = "https://mayorana.ch/downloads/ais-runner/latest/latest.json";

/// Fallback when `latest.json` has no entry for this OS (e.g. an Intel Mac —
/// only Apple Silicon is built). Sends the user to pick a build by hand
/// instead of at a link that would 404.
const RELEASES_URL: &str = "https://mayorana.ch/en/apps";

/// Sent on the update check so the download logs can tell a new install
/// (a browser hitting the site) from an existing user updating. Also
/// carries the version, which is what makes per-version adoption
/// visible — the number that says how many people are still on a build
/// with a bug that is already fixed.
const USER_AGENT: &str = concat!("ais-runner/", env!("CARGO_PKG_VERSION"), " (updater)");

#[derive(Debug, Deserialize)]
struct LatestJson {
    version: String,
    tag: String,
    platforms: Platforms,
}

#[derive(Debug, Deserialize)]
struct Platforms {
    macos: HashMap<String, Artifact>,
    windows: HashMap<String, Artifact>,
    linux: HashMap<String, Artifact>,
}

#[derive(Debug, Deserialize)]
struct Artifact {
    url: String,
    #[allow(dead_code)]
    sha256: String,
}

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub latest_version: String,
    #[allow(dead_code)]
    pub latest_tag: String,
    pub release_url: String,
}

/// Returns `Some(UpdateInfo)` if a newer release is available, else `None`.
/// Any network / parse failure → `None`. Never panics.
/// Disabled if DISABLE_UPDATE_CHECK environment variable is set.
pub async fn check() -> Option<UpdateInfo> {
    if std::env::var("DISABLE_UPDATE_CHECK").is_ok() {
        return None;
    }

    let current = env!("CARGO_PKG_VERSION");
    let body = reqwest::Client::new()
        .get(LATEST_URL)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;
    let latest: LatestJson = serde_json::from_str(&body).ok()?;
    if is_newer(&latest.version, current) {
        Some(UpdateInfo {
            latest_version: latest.version,
            latest_tag: latest.tag,
            release_url: platform_url(&latest.platforms),
        })
    } else {
        None
    }
}

/// Picks the one artifact URL published for this OS. Empty (build missing
/// for this OS) or unparseable falls back to the landing page.
fn platform_url(platforms: &Platforms) -> String {
    let by_os = match std::env::consts::OS {
        "macos" => &platforms.macos,
        "windows" => &platforms.windows,
        "linux" => &platforms.linux,
        _ => return RELEASES_URL.to_string(),
    };
    by_os
        .values()
        .next()
        .map(|a| a.url.clone())
        .filter(|u| !u.is_empty())
        // Marks the hit as coming from an existing install. The banner opens
        // this in the user's browser, so the updater's own User-Agent is not
        // what fetches the file — without the marker the request is
        // indistinguishable from a first-time download off the website.
        // nginx serves the file regardless of the query string.
        .map(|u| format!("{u}?src=updater"))
        .unwrap_or_else(|| RELEASES_URL.to_string())
}

fn is_newer(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Option<(u32, u32, u32)> {
        let mut parts = s.trim_start_matches('v').split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts
            .next()?
            .split(|c: char| c == '-' || c == '+')
            .next()?
            .parse()
            .ok()?;
        Some((major, minor, patch))
    };
    match (parse(a), parse(b)) {
        (Some(av), Some(bv)) => av > bv,
        _ => false,
    }
}
