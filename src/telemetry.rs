//! Anonymous usage statistics, shared by every mayorana desktop app.
//!
//! The download logs on mayorana.ch can say how many people fetched a build and
//! how many copies checked for an update. They cannot say whether a download
//! became an install, or whether a person who installed last week is still
//! here. This answers those, and only those: an install's first launch, its
//! launches after that, and whether an offered update was taken.
//!
//! What is sent is deliberately narrow, and the narrowness is enforced by the
//! types rather than by care: an [`Event`] is an enum with no free-form fields
//! except an update's target version, which is checked to look like a version
//! before it is kept. There is no way to hand it a path, a file name, a
//! document, an account or an error string.
//!
//! On by default (opt-out), but never before the person has been told: nothing
//! is recorded until the notice has been shown once ([`mark_informed`]).
//! Any one of these means nothing is recorded and nothing is sent:
//!
//!   * the build has no endpoint (`MAYORANA_TELEMETRY_URL` unset at compile
//!     time — which is every `cargo run`, so development never pollutes the
//!     numbers);
//!   * the person turned it off;
//!   * `DISABLE_UPDATE_CHECK`, `DO_NOT_TRACK` or `MAYORANA_NO_TELEMETRY` is set,
//!     in the app's environment or in the login shell's (see [`start`]).
//!
//! Best-effort throughout, like the update check: recording is a synchronous
//! append that cannot fail visibly, sending has a two-second timeout, and a
//! failed send leaves the events queued for the next attempt. Never panics,
//! never blocks the interface.
//!
//! Transport is a single GET to a URL on mayorana.ch that answers 204 and
//! nothing else. The batch rides in the query string, base64url-encoded, and
//! the statistics job reads it back out of the web server's access log — the
//! same place it already reads download counts from. A URL has a ceiling, so
//! a backlog goes out as several small requests (see `EVENTS_BUDGET_BYTES`).
//!
//! Identity is one random 128-bit value per app, generated on first use. It is
//! not derived from the machine, the account or the network, so it cannot be
//! recomputed, and a reinstall is a new install. It is deliberately not linked
//! to a mayorana.ch sign-in.
//!
//! This file is the same in every app except for `APP`; gitagent has its own,
//! richer version. Fix a bug here, fix it in all of them.

use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// The product name the statistics are filed under, matching its folder
/// under mayorana.ch/downloads/.
const APP: &str = "ais-runner";
const SCHEMA: u32 = 1;

/// Where batches go. Baked in by the release workflow; absent in a development
/// build, and an empty value (an unset repository variable expands to "") is
/// treated as absent too.
const ENDPOINT: Option<&str> = option_env!("MAYORANA_TELEMETRY_URL");

/// What an unanswered question means. `true`: statistics are on by default and
/// the person turns them off (opt-out). Even so, nothing is recorded until the
/// notice in the window has been shown once — see `State::informed` — so nobody
/// is counted before they have been told.
///
/// The wording of the banner in `main.rs`, and of the changelog entry, assumes
/// this is `true`. Flipping it to `false` makes the feature opt-in and those
/// words must change with it.
const DEFAULT_CONSENT: bool = true;

const STATE_FILE: &str = "telemetry.json";
const QUEUE_FILE: &str = "events.jsonl";

/// Offline for a long time must not grow a file without bound. Past this the
/// newest events are dropped, not the oldest: the start of a session says more
/// than its tail.
const MAX_QUEUE_BYTES: u64 = 256 * 1024;
const MAX_BATCH: usize = 200;
/// The batch travels in a URL, and a URL has a ceiling: nginx refuses a request
/// line over 8 KB by default. Keeping the events' JSON near 4 KB makes about
/// 5.4 KB once base64-encoded, which fits with room for the address and the
/// envelope around it.
const EVENTS_BUDGET_BYTES: usize = 4000;
/// A flush works through a backlog in chunks, but not forever: an app that has
/// been offline for a week should catch up over a few flushes, not in one burst
/// of requests.
const MAX_CHUNKS_PER_FLUSH: usize = 10;
/// A week-old event describes a version and a habit that no longer exist.
const MAX_AGE_SECS: u64 = 7 * 24 * 60 * 60;
const SEND_TIMEOUT: Duration = Duration::from_secs(2);
const FLUSH_FIRST_AFTER: Duration = Duration::from_secs(10);
const FLUSH_EVERY: Duration = Duration::from_secs(5 * 60);

/// Env vars that mean "do not phone home". `DISABLE_UPDATE_CHECK` is here
/// because someone who has turned that off expects silence, not a different
/// channel carrying the same information.
const OPT_OUT_VARS: &[&str] = &[
    "DISABLE_UPDATE_CHECK",
    "DO_NOT_TRACK",
    "MAYORANA_NO_TELEMETRY",
];

// ── What can be said ──────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The process started. The denominator for everything else.
    AppStarted,
    /// Emitted once per install, by [`record`] itself, when the install's
    /// identity is first created — the first event recorded after the person
    /// has seen the notice. The "installed" step of the funnel: a download in
    /// the web server's log is not an install, and only the app can tell.
    AppInstalled,
    /// The update banner offered a newer version. (Unused in an app with no
    /// update check.)
    #[allow(dead_code)]
    UpdateOffered { to: String },
    /// Its Download link was clicked.
    #[allow(dead_code)]
    UpdateClicked { to: String },
}

impl Event {
    fn wire(&self) -> (&'static str, BTreeMap<String, String>) {
        let mut p = BTreeMap::new();
        let name = match self {
            Event::AppStarted => "app_started",
            Event::AppInstalled => "app_installed",
            Event::UpdateOffered { to } => {
                p.insert("to".to_string(), sanitise_version(to));
                "update_offered"
            }
            Event::UpdateClicked { to } => {
                p.insert("to".to_string(), sanitise_version(to));
                "update_clicked"
            }
        };
        (name, p)
    }
}

/// The version text comes from a file on our own server, but it is still the
/// one string here that was not written in this crate.
fn sanitise_version(raw: &str) -> String {
    let ok = !raw.is_empty()
        && raw.len() <= 20
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    if ok {
        raw.to_string()
    } else {
        "unknown".to_string()
    }
}

// ── The login shell's opt-outs ────────────────────────────────────────────

/// The opt-out variables as the person's login shell sees them.
///
/// An app opened from Finder, the Dock or a desktop launcher does not inherit
/// the shell environment, so `DO_NOT_TRACK=1` exported in `.zshrc` would only
/// be honoured when the app happened to be started from a terminal. Read once,
/// off the interface thread, by [`start`]. Until it has been read every opt-out
/// counts as set, so nothing can be recorded before the answer is known.
static LOGIN_ENV: OnceLock<BTreeMap<String, String>> = OnceLock::new();

fn login_env(name: &str) -> Option<String> {
    match LOGIN_ENV.get() {
        Some(vars) => vars.get(name).cloned(),
        None => Some("1".to_string()),
    }
}

/// `KEY=value` lines from the shell, keeping only the opt-out variables. A
/// profile that prints its own output (nvm, conda, banners) cannot smuggle
/// anything else in, because only these names are accepted.
fn parse_login_env(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .filter(|(k, v)| !v.is_empty() && OPT_OUT_VARS.contains(&k.as_str()))
        .collect()
}

#[cfg(unix)]
fn read_login_env() -> BTreeMap<String, String> {
    use std::process::{Command, Stdio};
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let script = OPT_OUT_VARS
        .iter()
        .map(|k| format!("printf '{k}=%s\\n' \"${k}\""))
        .collect::<Vec<_>>()
        .join("; ");
    Command::new(shell)
        .args(["-lc", &script])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_login_env(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// Windows applications receive the user's environment from the system, so
/// there is no separate login shell to ask.
#[cfg(not(unix))]
fn read_login_env() -> BTreeMap<String, String> {
    BTreeMap::new()
}

/// Call once from every window's startup coroutine. Reads the login shell's
/// opt-outs (once per process), records `AppStarted` (once per process), and
/// says whether the notice should be shown.
pub async fn start() -> bool {
    static STARTED: OnceLock<()> = OnceLock::new();
    if LOGIN_ENV.get().is_none() {
        let vars = tokio::task::spawn_blocking(read_login_env)
            .await
            .unwrap_or_default();
        let _ = LOGIN_ENV.set(vars);
    }
    if STARTED.set(()).is_ok() {
        record(Event::AppStarted);
    }
    should_ask()
}

// ── Gate ──────────────────────────────────────────────────────────────────

fn endpoint() -> Option<&'static str> {
    ENDPOINT.filter(|u| !u.is_empty())
}

/// Whether this build can send at all. When it cannot, nothing about
/// statistics should be shown — asking permission to do something the build
/// cannot do would be a question with no answer.
pub fn available() -> bool {
    endpoint().is_some()
}

/// `1`, `true`, `yes` — and anything else non-empty except the explicit
/// negatives. `DO_NOT_TRACK=0` must not count as opting out, and a variable
/// exported empty must not either.
fn flag_set(value: Option<&str>) -> bool {
    match value.map(str::trim) {
        None | Some("") => false,
        Some(v) => !matches!(v.to_ascii_lowercase().as_str(), "0" | "false" | "no"),
    }
}

fn env_blocks() -> bool {
    OPT_OUT_VARS.iter().any(|name| {
        let value = std::env::var(name).ok().or_else(|| login_env(name));
        // DISABLE_UPDATE_CHECK is honoured by mere presence in update_check.rs,
        // so it is here too; the others follow the usual 0/false convention.
        if *name == "DISABLE_UPDATE_CHECK" {
            value.is_some()
        } else {
            flag_set(value.as_deref())
        }
    })
}

/// An explicit answer always wins. An unanswered question means
/// `DEFAULT_CONSENT` — but only once the person has been shown the notice, so
/// that "on by default" never means "on before you were told".
fn gate(endpoint: Option<&str>, consent: Option<bool>, informed: bool, env_blocked: bool) -> bool {
    endpoint.is_some()
        && !env_blocked
        && match consent {
            Some(answer) => answer,
            None => DEFAULT_CONSENT && informed,
        }
}

/// Whether anything is being recorded right now.
pub fn active() -> bool {
    let state = load_state(&dir());
    gate(endpoint(), state.consent, state.informed, env_blocks())
}

/// What a settings switch should show: the person's answer, or the default if
/// they have not given one. Unused in an app with no settings screen.
#[allow(dead_code)]
pub fn shared() -> bool {
    consent().unwrap_or(DEFAULT_CONSENT)
}

/// Record that the notice has been put in front of the person. From here on an
/// unanswered question counts as the default. Called when the banner appears.
pub fn mark_informed() {
    mark_informed_in(&dir());
}

fn mark_informed_in(dir: &Path) {
    let _io = IO.lock().unwrap_or_else(|e| e.into_inner());
    let mut state = load_state(dir);
    if !state.informed {
        state.informed = true;
        save_state(dir, &state);
    }
}

/// Whether to put the question to the person: a build that can send, an
/// answer not yet given, and no environment setting that has already answered
/// it for them.
pub fn should_ask() -> bool {
    available() && consent().is_none() && !env_blocks()
}

pub fn consent() -> Option<bool> {
    load_state(&dir()).consent
}

/// Saying no also deletes what was waiting to be sent: an opt-out that still
/// uploaded the backlog would not be one.
pub fn set_consent(yes: bool) {
    set_consent_in(&dir(), yes);
}

fn set_consent_in(dir: &Path, yes: bool) {
    let _io = IO.lock().unwrap_or_else(|e| e.into_inner());
    let mut state = load_state(dir);
    state.consent = Some(yes);
    save_state(dir, &state);
    if !yes {
        let _ = std::fs::remove_file(dir.join(QUEUE_FILE));
    }
}

// ── State and queue on disk ───────────────────────────────────────────────

#[derive(Default, Serialize, Deserialize)]
struct State {
    /// `None` until answered.
    #[serde(default)]
    consent: Option<bool>,
    /// Whether the notice has been shown. Only matters while `consent` is
    /// `None`: it is what lets the default take effect.
    #[serde(default)]
    informed: bool,
    #[serde(default)]
    install_id: String,
    /// `YYYY-MM-DD` of the first recorded event. Kept locally; never sent.
    #[serde(default)]
    first_seen: String,
}

/// One queued event. Carries its own launch and version because the queue can
/// outlive both: events from before an update are sent after it, and stamping
/// them with the sender's version would put them in the wrong column.
#[derive(Serialize, Deserialize)]
struct Line {
    n: String,
    t: u64,
    /// Which launch of the app produced it.
    l: String,
    /// App version at the time.
    av: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    p: BTreeMap<String, String>,
}

/// Serialises every read-modify-write of the two files within this process —
/// two windows are two tasks writing the same directory.
static IO: Mutex<()> = Mutex::new(());
/// One send at a time, so a batch cannot go out twice.
static FLUSHING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// One folder per app, inside the folder every mayorana app already shares
/// for notice dismissals — so each product keeps its own identity and its own
/// answer, and none of it lives in an app's own settings or caches.
fn dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("mayorana")
        .join("telemetry")
        .join(APP)
}

/// Write to a temporary file in the same directory, then rename it into place.
/// A plain write truncates first, so being killed half-way would leave an empty
/// state file — read back as "never answered", which would quietly undo a "turn
/// it off". A rename within one filesystem is atomic. The temporary name carries
/// the process id so two windows writing at once do not share a scratch file.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "tmp".into());
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn load_state(dir: &Path) -> State {
    std::fs::read_to_string(dir.join(STATE_FILE))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(dir: &Path, state: &State) {
    let _ = std::fs::create_dir_all(dir);
    if let Ok(json) = serde_json::to_string_pretty(state) {
        let _ = write_atomic(&dir.join(STATE_FILE), json.as_bytes());
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn launch_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(random_hex)
}

/// 128 random bits, hex. Empty if the OS gives none — callers treat that as
/// "do not record" rather than fall back to something guessable.
fn random_hex() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        return String::new();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Calendar date (UTC) of a Unix time — Howard Hinnant's days-to-civil
/// algorithm. Written out rather than taken from a date crate because this is
/// the only date arithmetic here, and not every app links one.
fn civil(secs: u64) -> (i64, u32, u32, u64) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day, rem)
}

fn date_of(secs: u64) -> String {
    let (y, m, d, _) = civil(secs);
    format!("{y:04}-{m:02}-{d:02}")
}

fn timestamp_of(secs: u64) -> String {
    let (y, m, d, rem) = civil(secs);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn make_line(event: &Event, now: u64) -> Line {
    let (n, p) = event.wire();
    Line {
        n: n.to_string(),
        t: now,
        l: launch_id().to_string(),
        av: env!("CARGO_PKG_VERSION").to_string(),
        p,
    }
}

fn queue_is_full(dir: &Path) -> bool {
    std::fs::metadata(dir.join(QUEUE_FILE))
        .map(|m| m.len() >= MAX_QUEUE_BYTES)
        .unwrap_or(false)
}

fn append(dir: &Path, lines: &[Line]) {
    let _ = std::fs::create_dir_all(dir);
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(QUEUE_FILE))
    else {
        return;
    };
    for line in lines {
        if let Ok(json) = serde_json::to_string(line) {
            let _ = writeln!(file, "{json}");
        }
    }
}

// ── Recording ─────────────────────────────────────────────────────────────

/// Note that something happened. Synchronous, cheap, and silent about every
/// way it can fail; safe to call from anywhere, including a hot path.
pub fn record(event: Event) {
    record_in(&dir(), active(), &event, now_secs());
}

fn record_in(dir: &Path, on: bool, event: &Event, now: u64) {
    if !on {
        return;
    }
    let _io = IO.lock().unwrap_or_else(|e| e.into_inner());

    let mut state = load_state(dir);
    let new_install = state.install_id.is_empty();
    if new_install {
        state.install_id = random_hex();
        if state.install_id.is_empty() {
            return;
        }
        state.first_seen = date_of(now);
        save_state(dir, &state);
    }
    if queue_is_full(dir) {
        return;
    }

    // Ahead of whatever triggered it, so "installed" always precedes the first
    // thing the install did. Tied to creating the id, so it cannot repeat.
    let mut lines = Vec::new();
    if new_install {
        lines.push(make_line(&Event::AppInstalled, now));
    }
    lines.push(make_line(event, now));

    append(dir, &lines);
}

/// Distinct from `(updater)` and `(notice)` on purpose: the download statistics
/// count `(updater)` polls as live installs.
fn user_agent() -> String {
    format!("{APP}/{} (telemetry)", env!("CARGO_PKG_VERSION"))
}

// >>> http_get — the only part that depends on the app's HTTP client.
/// One GET; true only for a 2xx answer. Any error, timeout or refusal is false.
async fn http_get(url: &str) -> bool {
    reqwest::Client::new()
        .get(url)
        .header(reqwest::header::USER_AGENT, user_agent())
        .timeout(SEND_TIMEOUT)
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}
// <<< http_get

// ── Sending ───────────────────────────────────────────────────────────────

/// Sends what is queued, if anything, and forgets it only once the server has
/// said yes. Call it whenever; it does nothing when statistics are off.
pub async fn flush() {
    let Some(url) = endpoint() else { return };
    if !active() {
        return;
    }
    flush_all_to(&dir(), url, now_secs()).await;
}

/// First flush shortly after start, then on a timer. Every window runs one;
/// they serialise on `FLUSHING`, so the second finds an empty queue.
pub async fn flush_forever() {
    tokio::time::sleep(FLUSH_FIRST_AFTER).await;
    loop {
        flush().await;
        tokio::time::sleep(FLUSH_EVERY).await;
    }
}

/// Reads the queue, discarding anything too old or unreadable, and returns the
/// next batch with the identity to send it under.
fn next_batch(dir: &Path, now: u64) -> Option<(Vec<Line>, String)> {
    let _io = IO.lock().unwrap_or_else(|e| e.into_inner());
    let text = std::fs::read_to_string(dir.join(QUEUE_FILE)).ok()?;
    let total = text.lines().filter(|l| !l.trim().is_empty()).count();
    let kept: Vec<Line> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Line>(l).ok())
        .filter(|l| l.t.saturating_add(MAX_AGE_SECS) >= now)
        .collect();
    if kept.len() != total {
        rewrite(dir, &kept);
    }
    let install_id = load_state(dir).install_id;
    if kept.is_empty() || install_id.is_empty() {
        return None;
    }
    // Oldest first, as many as fit. Always at least one, so a line somehow
    // larger than the budget cannot wedge the queue behind it.
    let mut batch = Vec::new();
    let mut used = 0usize;
    for line in kept.into_iter().take(MAX_BATCH) {
        let size = serde_json::to_string(&line)
            .map(|j| j.len() + 1)
            .unwrap_or(0);
        if !batch.is_empty() && used + size > EVENTS_BUDGET_BYTES {
            break;
        }
        used += size;
        batch.push(line);
    }
    Some((batch, install_id))
}

fn rewrite(dir: &Path, lines: &[Line]) {
    let path = dir.join(QUEUE_FILE);
    if lines.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    let mut out = String::new();
    for line in lines {
        if let Ok(json) = serde_json::to_string(line) {
            out.push_str(&json);
            out.push('\n');
        }
    }
    let _ = write_atomic(&path, out.as_bytes());
}

/// Drops the first `sent` lines. Safe because the queue is append-only between
/// flushes and only a flush ever removes from the front.
fn forget_sent(dir: &Path, sent: usize) {
    let _io = IO.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(text) = std::fs::read_to_string(dir.join(QUEUE_FILE)) else {
        return;
    };
    let rest: Vec<Line> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Line>(l).ok())
        .skip(sent)
        .collect();
    rewrite(dir, &rest);
}

/// Works through the queue one chunk at a time, stopping at the first failure
/// or when it is empty. A failure leaves the rest queued for the next flush.
async fn flush_all_to(dir: &Path, url: &str, now: u64) {
    for _ in 0..MAX_CHUNKS_PER_FLUSH {
        if !flush_to(dir, url, now).await {
            break;
        }
    }
}

/// Sends one chunk. `false` means nothing was sent — an empty queue, or a
/// refusal — and either way the caller should stop.
async fn flush_to(dir: &Path, url: &str, now: u64) -> bool {
    let _one_at_a_time = FLUSHING.lock().await;
    let Some((batch, install_id)) = next_batch(dir, now) else {
        return false;
    };
    let body = serde_json::json!({
        "v": SCHEMA,
        "app": APP,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "install_id": install_id,
        "sent_at": timestamp_of(now),
        "events": batch,
    });
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&body).unwrap_or_default());
    let separator = if url.contains('?') { '&' } else { '?' };
    let sent = http_get(&format!("{url}{separator}b={encoded}")).await;
    if sent {
        forget_sent(dir, batch.len());
    }
    sent
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const NOW: u64 = 1_790_000_000;

    fn scratch() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mayorana-telemetry-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn queued(dir: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(dir.join(QUEUE_FILE))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn nothing_is_recorded_unless_every_condition_holds() {
        let url = Some("https://x");
        // gate(endpoint, consent, informed, env_blocked)
        assert!(gate(url, Some(true), true, false));
        // No endpoint: a development build, whatever the person said.
        assert!(!gate(None, Some(true), true, false));
        // Said no.
        assert!(!gate(url, Some(false), true, false));
        // An environment opt-out beats a yes.
        assert!(!gate(url, Some(true), true, true));
    }

    #[test]
    fn on_by_default_never_means_on_before_you_were_told() {
        let url = Some("https://x");
        // Unanswered and not yet shown the notice: nothing, even though the
        // default is yes.
        assert!(!gate(url, None, false, false));
        // Shown the notice and did nothing about it: the default applies.
        assert_eq!(gate(url, None, true, false), DEFAULT_CONSENT);
        // An explicit answer is never overridden by the default or by having
        // been informed.
        assert!(!gate(url, Some(false), true, false));
        assert!(gate(url, Some(true), false, false));
        // And the environment still wins over the default.
        assert!(!gate(url, None, true, true));
    }

    #[test]
    fn being_informed_is_remembered_and_does_not_touch_the_answer() {
        let dir = scratch();
        assert!(!load_state(&dir).informed);
        mark_informed_in(&dir);
        assert!(load_state(&dir).informed);
        assert_eq!(load_state(&dir).consent, None, "informing is not answering");

        set_consent_in(&dir, false);
        mark_informed_in(&dir);
        assert_eq!(
            load_state(&dir).consent,
            Some(false),
            "and never flips an answer"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opt_out_variables_follow_their_conventions() {
        assert!(flag_set(Some("1")));
        assert!(flag_set(Some("true")));
        assert!(!flag_set(Some("0")));
        assert!(!flag_set(Some("false")));
        assert!(!flag_set(Some("")));
        assert!(!flag_set(None));
    }

    #[test]
    fn an_empty_endpoint_is_no_endpoint() {
        // An unset repository variable reaches option_env! as "", not as absent.
        assert_eq!(Some("").filter(|u| !u.is_empty()), None);
    }

    #[test]
    fn off_writes_nothing_at_all() {
        let dir = scratch();
        record_in(&dir, false, &Event::AppStarted, NOW);
        assert!(!dir.exists(), "even the state file would be a trace");
    }

    #[test]
    fn identity_is_random_stable_and_local() {
        let dir = scratch();
        record_in(&dir, true, &Event::AppStarted, NOW);
        let first = load_state(&dir).install_id;
        assert_eq!(first.len(), 32);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        record_in(&dir, true, &Event::AppStarted, NOW + 5);
        assert_eq!(load_state(&dir).install_id, first, "one id per install");
        assert_ne!(random_hex(), random_hex());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn installed_is_reported_once_ahead_of_the_first_event() {
        let dir = scratch();
        record_in(&dir, true, &Event::AppStarted, NOW);
        record_in(&dir, true, &Event::AppStarted, NOW + 60);
        record_in(&dir, true, &Event::AppStarted, NOW + 120);
        let names: Vec<String> = queued(&dir)
            .iter()
            .map(|e| e["n"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names[0], "app_installed", "first, before what triggered it");
        assert_eq!(names.iter().filter(|n| *n == "app_installed").count(), 1);

        // Off means no identity, so no install is reported either.
        let off = scratch();
        record_in(&off, false, &Event::AppStarted, NOW);
        assert!(queued(&off).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_version_string_is_checked_before_it_is_kept() {
        assert_eq!(sanitise_version("0.1.72"), "0.1.72");
        assert_eq!(sanitise_version("0.2.0-beta1"), "0.2.0-beta1");
        assert_eq!(sanitise_version("1.0 <script>"), "unknown");
        assert_eq!(sanitise_version(""), "unknown");
        assert_eq!(sanitise_version(&"9".repeat(40)), "unknown");
    }

    #[test]
    fn a_full_queue_drops_new_events_rather_than_growing() {
        let dir = scratch();
        record_in(&dir, true, &Event::AppStarted, NOW);
        let filler = "x".repeat(MAX_QUEUE_BYTES as usize);
        std::fs::write(dir.join(QUEUE_FILE), filler).unwrap();
        let before = std::fs::metadata(dir.join(QUEUE_FILE)).unwrap().len();
        record_in(&dir, true, &Event::AppStarted, NOW + 1);
        assert_eq!(
            std::fs::metadata(dir.join(QUEUE_FILE)).unwrap().len(),
            before
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saying_no_deletes_the_backlog_and_keeps_the_answer() {
        let dir = scratch();
        record_in(&dir, true, &Event::AppStarted, NOW);
        assert!(dir.join(QUEUE_FILE).exists());
        set_consent_in(&dir, false);
        assert!(!dir.join(QUEUE_FILE).exists());
        assert_eq!(load_state(&dir).consent, Some(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A tiny HTTP server: answers the nth request with `statuses[n]` (204 once
    /// the list runs out) and records each request target, in order.
    async fn serve(
        statuses: Vec<&'static str>,
    ) -> (
        String,
        std::sync::Arc<Mutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/ping", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let handle = tokio::spawn(async move {
            let mut n = 0;
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    let read = socket.read(&mut chunk).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..read]);
                }
                let head = String::from_utf8_lossy(&buf).to_string();
                let first = head.lines().next().unwrap_or_default();
                assert!(first.starts_with("GET "), "telemetry is a GET: {first}");
                log.lock().unwrap().push(first.to_string());
                let status = statuses.get(n).copied().unwrap_or("204 No Content");
                n += 1;
                let reply =
                    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        (url, seen, handle)
    }

    /// Decodes the batch out of a recorded request line.
    fn decode(request_line: &str) -> serde_json::Value {
        let target = request_line.split(' ').nth(1).unwrap();
        let encoded = target.split("b=").nth(1).expect("batch in the query");
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Exactly `events` queued lines: the first record of an install also
    /// queues its one `app_installed`, so one fewer `app_started` is needed.
    fn backlog(dir: &Path, events: usize) {
        for i in 0..events - 1 {
            record_in(dir, true, &Event::AppStarted, NOW + i as u64);
        }
        assert_eq!(queued(dir).len(), events);
    }

    #[tokio::test]
    async fn a_successful_send_delivers_the_envelope_and_clears_the_queue() {
        let dir = scratch();
        record_in(&dir, true, &Event::AppStarted, NOW);
        record_in(
            &dir,
            true,
            &Event::UpdateOffered { to: "9.9.9".into() },
            NOW + 1,
        );
        assert_eq!(
            queued(&dir).len(),
            3,
            "app_installed, app_started, update_offered"
        );

        let (url, seen, server) = serve(vec!["204 No Content"]).await;
        assert!(flush_to(&dir, &url, NOW + 2).await);
        server.abort();

        let requests = seen.lock().unwrap().clone();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /ping?b="));
        let body = decode(&requests[0]);
        let mut keys: Vec<_> = body.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            ["app", "arch", "events", "install_id", "os", "sent_at", "v"]
        );
        assert_eq!(body["app"], APP);
        assert_eq!(body["v"], 1);
        assert_eq!(body["events"].as_array().unwrap().len(), 3);
        assert_eq!(body["install_id"].as_str().unwrap().len(), 32);
        assert!(!dir.join(QUEUE_FILE).exists(), "sent events are forgotten");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failed_send_keeps_the_events_for_next_time() {
        let dir = scratch();
        record_in(&dir, true, &Event::AppStarted, NOW);

        let (url, _seen, server) = serve(vec!["500 Internal Server Error"]).await;
        assert!(!flush_to(&dir, &url, NOW + 1).await);
        server.abort();
        assert_eq!(queued(&dir).len(), 2, "app_installed, app_started");

        // Nothing listening at all.
        assert!(!flush_to(&dir, "http://127.0.0.1:1/x", NOW + 1).await);
        assert_eq!(queued(&dir).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_backlog_goes_out_in_chunks_that_each_fit_in_a_url() {
        let dir = scratch();
        backlog(&dir, 300);

        let (url, seen, server) = serve(vec![]).await;
        flush_all_to(&dir, &url, NOW + 300).await;
        server.abort();

        let requests = seen.lock().unwrap().clone();
        assert!(requests.len() > 1, "a backlog needs several requests");
        assert!(requests.len() <= MAX_CHUNKS_PER_FLUSH);
        let mut sent = 0;
        for line in &requests {
            // nginx's default request-line limit is 8 KB.
            assert!(line.len() < 8000, "request line is {} bytes", line.len());
            let events = decode(line)["events"].as_array().unwrap().len();
            assert!((1..=MAX_BATCH).contains(&events));
            sent += events;
        }
        assert_eq!(sent + queued(&dir).len(), 300, "nothing lost or doubled");

        // The remainder goes out on a later flush.
        let (url, _seen, server) = serve(vec![]).await;
        for _ in 0..3 {
            flush_all_to(&dir, &url, NOW + 300).await;
        }
        server.abort();
        assert!(!dir.join(QUEUE_FILE).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failing_chunk_stops_the_flush_and_keeps_the_rest() {
        let dir = scratch();
        backlog(&dir, 300);

        let (url, seen, server) = serve(vec!["204 No Content", "500 Internal Server Error"]).await;
        flush_all_to(&dir, &url, NOW + 300).await;
        server.abort();

        let requests = seen.lock().unwrap().clone();
        assert_eq!(requests.len(), 2, "stops at the first failure");
        let first = decode(&requests[0])["events"].as_array().unwrap().len();
        let second = decode(&requests[1])["events"].as_array().unwrap().len();
        let left = queued(&dir);
        assert_eq!(
            left.len(),
            300 - first,
            "only the acknowledged chunk is gone"
        );
        assert!(left.len() >= second, "the refused chunk is still queued");
        // The queue starts app_installed@NOW, app_started@NOW, @NOW+1, … so
        // line k (k ≥ 1) is stamped NOW + k - 1.
        assert_eq!(
            left[0]["t"],
            NOW + first as u64 - 1,
            "oldest unsent comes first"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn old_and_unreadable_lines_are_dropped_not_sent() {
        let dir = scratch();
        record_in(&dir, true, &Event::AppStarted, NOW);
        let mut text = std::fs::read_to_string(dir.join(QUEUE_FILE)).unwrap();
        text.push_str("this is not json\n");
        std::fs::write(dir.join(QUEUE_FILE), text).unwrap();

        // Two weeks later the one good event is stale too.
        let later = NOW + 2 * MAX_AGE_SECS;
        assert!(!flush_to(&dir, "http://127.0.0.1:1/x", later).await);
        assert!(!dir.join(QUEUE_FILE).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_opt_out_variables_are_taken_from_the_login_shell() {
        let shell_output = "Now using node v20 (npm 10)\n\
                            DO_NOT_TRACK=1\n\
                            DISABLE_UPDATE_CHECK=\n\
                            PATH=/usr/bin\n\
                            MAYORANA_NO_TELEMETRY=yes\n";
        let vars = parse_login_env(shell_output);
        assert_eq!(vars.get("DO_NOT_TRACK").map(String::as_str), Some("1"));
        assert_eq!(
            vars.get("MAYORANA_NO_TELEMETRY").map(String::as_str),
            Some("yes")
        );
        // Exported but empty is not set; anything outside the list is ignored.
        assert!(!vars.contains_key("DISABLE_UPDATE_CHECK"));
        assert!(!vars.contains_key("PATH"));
    }

    #[test]
    fn until_the_login_shell_has_answered_everything_counts_as_opted_out() {
        // Tests never call `start`, so the login environment is unknown here.
        assert!(LOGIN_ENV.get().is_none());
        assert!(env_blocks(), "an unknown opt-out must block, not allow");
    }

    #[test]
    fn the_app_name_is_one_the_server_accepts() {
        assert!(!APP.is_empty() && APP.len() <= 40);
        assert!(APP
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.-".contains(c)));
    }

    #[test]
    fn dates_are_formatted_without_a_date_library() {
        assert_eq!(timestamp_of(0), "1970-01-01T00:00:00Z");
        assert_eq!(date_of(951_782_400), "2000-02-29"); // a leap day
        assert_eq!(timestamp_of(1_790_000_000), "2026-09-21T14:13:20Z");
        assert_eq!(date_of(4_102_444_799), "2099-12-31");
    }
}
