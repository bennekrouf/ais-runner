use std::path::PathBuf;
use std::sync::Arc;
use dioxus::prelude::*;

use crate::components::log_panel::{LogLevel, LogLine};
use crate::services::{process::{ManagedProcess, ServiceState}, runtime_manager};
use crate::utils::make_push;

pub const SB_EMULATOR_IMAGE: &str =
    "mcr.microsoft.com/azure-messaging/servicebus-emulator:latest";
const SQL_EDGE_IMAGE: &str =
    "mcr.microsoft.com/azure-sql-edge:latest";

pub const SB_EMULATOR_PORT: u16 = 5672;
const MSSQL_PASSWORD: &str = "AisRunner_Emulator1!";

/// Hard limit enforced by the Service Bus emulator: queues + topics per namespace.
/// Exceeding it makes the emulator host refuse to start with a FluentAssertions
/// failure ("Max Queue/Topic supported $50, but found N") and exit 133 — the
/// container dies, nothing listens on 5672, and every scenario then fails on its
/// first queue operation with a bare ConnectionRefused that says nothing about
/// the real cause. Worse, the port-open probe used to report "emulator ready"
/// just before the crash, so the log claimed success.
///
/// The config is a *merge* of workflow-detected queues and whatever is already in
/// Config.json, so the list grows monotonically across runs and across projects —
/// it will silently cross this limit sooner or later. Cap it here instead.
pub const SB_EMULATOR_MAX_ENTITIES: usize = 50;

/// Returns the working directory for compose + config files.
///
/// On macOS/Linux we use `/tmp/ais-sb-emulator` — Docker Desktop shares `/tmp`
/// by default, whereas `std::env::temp_dir()` on macOS returns `/var/folders/…`
/// which is NOT in Docker's default file-sharing list, causing the volume mount
/// to silently fail and the emulator to start with no config (→ NullReferenceException).
///
/// On Windows `%TEMP%` is accessible to Docker Desktop via the WSL2 filesystem.
pub fn work_dir() -> PathBuf {
    if cfg!(target_os = "windows") {
        std::env::temp_dir().join("ais-sb-emulator")
    } else {
        std::path::PathBuf::from("/tmp/ais-sb-emulator")
    }
}

/// Classify a single emulator output line so we can dampen the noise the
/// Microsoft Service Bus emulator emits for benign idle states.
///
/// The two big offenders are:
///
/// 1. **Session-idle timeouts** — every 30 s, a session-aware receiver that
///    has no session-tagged message to drain logs a multi-line `WRN`+`ERR`
///    block with a full stack trace. This is normal idle behaviour, not a
///    failure, but it floods the SB Emulator tab and looks alarming.
///
/// 2. **Tenant-cache refresh PlatformNotSupportedException** — the emulator
///    fires this once per startup on Linux containers (it's a benign quirk
///    of the bundled .NET host on non-Windows hosts).
///
/// Returns the rewritten line to show, or `None` to drop the line entirely.
fn classify_emu_line(raw: &str) -> Option<String> {
    // ── Idle session receiver timeout ──────────────────────────────────
    // The emulator emits this whenever a session-aware receiver waits the
    // full 30-second AMQP timeout without a session arriving. The line
    // contains an embedded `source(address:/<queue>,filter:[com.microsoft:
    // session-filter:])` token we use to identify both the pattern and the
    // affected queue.
    if raw.contains("com.microsoft:session-filter:")
        && (raw.contains("did not complete within the allotted timeout")
            || raw.contains("com.microsoft:timeout"))
    {
        let queue = extract_queue_from_source(raw)
            .unwrap_or_else(|| "(unknown queue)".to_string());
        return Some(format!(
            "ℹ {queue}: idle — waiting for session-tagged message (normal)"
        ));
    }

    // ── TenantCache benign warning at emulator boot ────────────────────
    if raw.contains("TenantCache")
        && raw.contains("PlatformNotSupportedException")
    {
        return Some(
            "ℹ TenantCache refresh skipped on this platform (harmless)".into()
        );
    }

    // Anything else passes through unchanged.
    Some(raw.to_string())
}

/// Parse a `source(address:/<queue>,…)` fragment out of the emulator's
/// trace line and return the queue name. The emulator embeds this token
/// in every link-level message, with the leading `/` stripped here so it
/// matches Config.json queue names.
fn extract_queue_from_source(raw: &str) -> Option<String> {
    let start = raw.find("source(address:/")?;
    let rest  = &raw[start + "source(address:/".len()..];
    let end   = rest.find(',').or_else(|| rest.find(')'))?;
    Some(rest[..end].to_string())
}

/// Collapse runs of identical lines into one entry plus a count suffix.
/// Stateful: keeps the last visible line, how many times it's repeated,
/// and how many lines pushed since we last emitted. Caller drives it with
/// `feed()` which returns Vec<String> — possibly empty, possibly one, very
/// rarely two (the flushed previous + the new distinct one).
struct EmuLineDeduper {
    last:        Option<String>,
    repeat:      u32,
    /// Index of the last entry we wrote to `emu_lines`, so we can rewrite
    /// it in place when a duplicate arrives instead of appending another
    /// `(×N)` line. -1 sentinel = no current entry.
    last_idx:    isize,
}

impl EmuLineDeduper {
    fn new() -> Self {
        Self { last: None, repeat: 0, last_idx: -1 }
    }

    /// Decide what to do with a fresh line. Returns one of:
    /// - `DedupAction::Append(s)` — a new distinct line; push it.
    /// - `DedupAction::Rewrite(idx, s)` — the same line as before; replace
    ///   the entry at `idx` with `s` (which carries the "(×N)" suffix).
    /// - `DedupAction::Drop` — line was a noise pattern we suppress entirely.
    fn feed(&mut self, raw: &str, current_len: usize) -> DedupAction {
        let Some(visible) = classify_emu_line(raw) else {
            return DedupAction::Drop;
        };

        // Same as last visible line → bump counter, rewrite in place.
        if self.last.as_deref() == Some(visible.as_str()) && self.last_idx >= 0 {
            self.repeat += 1;
            let count = self.repeat + 1;
            return DedupAction::Rewrite(
                self.last_idx as usize,
                format!("{visible}  (×{count})"),
            );
        }

        // Different line — reset state and append.
        self.last     = Some(visible.clone());
        self.repeat   = 0;
        self.last_idx = current_len as isize;
        DedupAction::Append(visible)
    }
}

enum DedupAction {
    Append(String),
    Rewrite(usize, String),
    Drop,
}

/// Validate a Service Bus queue/topic name against the same rules the SB emulator
/// enforces internally: `^[A-Za-z0-9]$|^[A-Za-z0-9][\w-\.\/]*[A-Za-z0-9]$`.
/// Must start and end with an alphanumeric; interior chars may be letters, digits,
/// underscores, hyphens, periods, or slashes.
pub fn is_valid_sb_queue_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() { return false; }
    let is_alnum = |b: u8| b.is_ascii_alphanumeric();
    let is_interior = |b: u8| {
        b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/')
    };
    if !is_alnum(bytes[0]) { return false; }
    if !is_alnum(bytes[bytes.len() - 1]) { return false; }
    bytes[1..bytes.len().saturating_sub(1)].iter().all(|&b| is_interior(b))
}

/// Docker Compose expects forward-slash paths even on Windows.
fn to_docker_path(p: &PathBuf) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Drop this into local.settings.json as SERVICE_BUS_CONNECTION_STRING.
pub const EMULATOR_CONN_STR: &str =
    "Endpoint=sb://localhost;SharedAccessKeyName=RootManageSharedAccessKey;\
     SharedAccessKey=SAS_KEY_VALUE;UseDevelopmentEmulator=true;";

// ── file generation ──────────────────────────────────────────────────────────

/// Write docker-compose.yml + config.json to WORK_DIR.
/// `queues` comes from scanning the workflow's local.settings.json — we
/// pre-create every queue the Logic App uses so it's ready on first run.
///
/// Returns the work dir plus the names of any queues dropped to stay under
/// [`SB_EMULATOR_MAX_ENTITIES`], so the caller can surface them.
fn write_compose_files(queues: &[(String, bool)]) -> Result<(PathBuf, Vec<String>), String> {
    let dir = work_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    // Mount the whole directory, not a single file — single-file bind mounts from /tmp
    // are unreliable on Docker Desktop for Mac and result in a null config inside the container.
    // The emulator reads Config.json from /ServiceBus_Emulator/ConfigFiles/.
    let config_dir_docker = to_docker_path(&dir);

    // ── docker-compose.yml ───────────────────────────────────────────────
    let compose = format!(
        r#"name: ais-sb-emulator
services:
  sqledge:
    container_name: ais-sqledge
    image: {sql_img}
    environment:
      ACCEPT_EULA: "Y"
      MSSQL_SA_PASSWORD: "{pw}"
    healthcheck:
      test: ["CMD-SHELL", "bash -c 'echo > /dev/tcp/127.0.0.1/1433' 2>/dev/null"]
      interval: 10s
      timeout: 5s
      retries: 15
      start_period: 30s
    networks: [sbnet]

  emulator:
    container_name: ais-sb-emulator
    image: {sb_img}
    volumes:
      - {config_dir}:/ServiceBus_Emulator/ConfigFiles
    ports:
      - "{port}:{port}"
    environment:
      SQL_SERVER: sqledge
      MSSQL_SA_PASSWORD: "{pw}"
      ACCEPT_EULA: "Y"
    depends_on:
      sqledge:
        condition: service_healthy
    networks: [sbnet]

networks:
  sbnet:
"#,
        sql_img = SQL_EDGE_IMAGE,
        sb_img  = SB_EMULATOR_IMAGE,
        pw      = MSSQL_PASSWORD,
        config_dir = config_dir_docker,
        port    = SB_EMULATOR_PORT,
    );
    std::fs::write(dir.join("docker-compose.yml"), compose)
        .map_err(|e| e.to_string())?;

    // ── config.json ──────────────────────────────────────────────────────
    // Merge: start from workflow-detected queues, then preserve any extra queues
    // that were manually added via the UI to an existing config.json.
    // An empty Queues array causes a NullReferenceException in the emulator host;
    // add a sentinel if nothing is detected. Omit Topics — empty array also crashes.
    // effective_queues: (name, requires_session)
    let mut effective_queues: Vec<(String, bool)> = queues.to_vec();

    // Validate and preserve queues from an existing config.json.
    // Also check for common hand-edited mistakes and warn before Docker even starts.
    let config_path = dir.join("Config.json");
    if let Ok(existing) = std::fs::read_to_string(&config_path) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&existing) {
            // Detect and auto-fix known bad Config.json patterns.
            // Rather than failing, delete the file so it gets regenerated correctly below.
            let bad_namespace = v["UserConfig"]["Namespaces"][0]["Name"]
                .as_str().map(|n| n != "sbemulatorns").unwrap_or(false);
            let bad_logging = v["UserConfig"]["LoggingConfig"].is_object()
                && !v["UserConfig"]["Logging"].is_object();
            if bad_namespace || bad_logging {
                let _ = std::fs::remove_file(&config_path);
                // Stale Config.json deleted — will be regenerated correctly below.
            } else if let Some(arr) = v["UserConfig"]["Namespaces"][0]["Queues"].as_array() {
                for q in arr {
                    if let Some(raw) = q["Name"].as_str() {
                        // Trim and validate — silently skip names that would
                        // crash the emulator (the surfaced error from there is
                        // a regex assertion deep in FluentAssertions).
                        let name = raw.trim();
                        if name.is_empty() || !is_valid_sb_queue_name(name) {
                            continue;
                        }
                        if !effective_queues.iter().any(|(n, _)| n == name) {
                            // Preserve existing session setting from config.json
                            let session = q["Properties"]["RequiresSession"].as_bool().unwrap_or(false);
                            effective_queues.push((name.to_string(), session));
                        }
                    }
                }
            }
        }
    }

    if effective_queues.is_empty() {
        effective_queues.push(("ais.default".to_string(), false));
    }

    // Enforce the emulator's ceiling. `queues` (workflow-detected) come first in
    // effective_queues and are what the current project actually needs, so a plain
    // truncate keeps them and drops the preserved leftovers — queues carried over
    // from earlier runs or other projects, which is exactly the accumulation that
    // pushes the list over the limit. Dropping them beats emitting a config the
    // emulator rejects outright.
    let dropped_queues: Vec<String> = if effective_queues.len() > SB_EMULATOR_MAX_ENTITIES {
        let dropped = effective_queues[SB_EMULATOR_MAX_ENTITIES..]
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        effective_queues.truncate(SB_EMULATOR_MAX_ENTITIES);
        dropped
    } else {
        Vec::new()
    };

    // LockDuration: PT5M (5 minutes) — generous default for local cold-start runs.
    // Logic Apps consumers commonly need >1 min for the first invocation
    // (function host JIT + child-workflow warmup + SQL roundtrips), producing
    // confusing MessageLockLost errors at the default PT1M. Real production
    // queues should tune this per workflow; PT5M is a safe local default.
    let queue_entries: String = effective_queues.iter().map(|(q, session)| format!(
        r#"          {{
            "Name": "{q}",
            "Properties": {{
              "DeadLetteringOnMessageExpiration": false,
              "DefaultMessageTimeToLive": "PT1H",
              "LockDuration": "PT5M",
              "MaxDeliveryCount": 10,
              "RequiresDuplicateDetection": false,
              "RequiresSession": {session}
            }}
          }}"#,
        q = q,
        session = session
    )).collect::<Vec<_>>().join(",\n");

    // Deliberately no "Topics" key — an empty array crashes the emulator's C#
    // model with NullReferenceException on startup just as reliably as a
    // missing one does. Omitting it entirely is what actually works.
    let config = format!(
        r#"{{
  "UserConfig": {{
    "Namespaces": [
      {{
        "Name": "sbemulatorns",
        "Queues": [
{queues}
        ]
      }}
    ],
    "Logging": {{
      "Type": "Console"
    }}
  }}
}}"#,
        queues = queue_entries,
    );
    // Use capital-C filename to match the emulator's expected path inside the container.
    std::fs::write(dir.join("Config.json"), config)
        .map_err(|e| e.to_string())?;

    Ok((dir, dropped_queues))
}

/// Build a `Command` that runs `docker compose <args>`.
/// On Windows, wraps through `cmd /c docker` so GUI-launched apps find docker.exe
/// even when it isn't on the inherited system PATH.
fn docker_compose_cmd(args: &[&str]) -> std::process::Command {
    if cfg!(target_os = "windows") {
        let docker = runtime_manager::resolve_tool("docker");
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/c", &docker, "compose"]).args(args);
        cmd
    } else {
        let docker = runtime_manager::resolve_tool("docker");
        let mut cmd = std::process::Command::new(&docker);
        cmd.arg("compose").args(args);
        cmd
    }
}

// ── emulator config helpers ───────────────────────────────────────────────────

/// Add a queue to the emulator's config.json.
/// Returns Ok(true) if the queue was added, Ok(false) if it already existed.
/// The caller is responsible for restarting the emulator to pick up the change.
pub fn add_queue_to_emulator_config(queue_name: &str) -> Result<bool, String> {
    let queue_name = queue_name.trim();
    if !is_valid_sb_queue_name(queue_name) {
        return Err(format!(
            "Invalid queue name '{}': must start and end with a letter or digit, \
             interior chars may be letters, digits, '_', '-', '.', or '/'.",
            queue_name
        ));
    }
    let path = work_dir().join("Config.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("Could not read config.json: {}", e))?;
    let mut root: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("Could not parse config.json: {}", e))?;

    let queues = root["UserConfig"]["Namespaces"][0]["Queues"]
        .as_array_mut()
        .ok_or("Unexpected config.json shape")?;

    // Idempotent — don't add duplicates
    if queues.iter().any(|q| q["Name"].as_str() == Some(queue_name)) {
        return Ok(false);
    }

    queues.push(serde_json::json!({
        "Name": queue_name,
        "Properties": {
            "DeadLetteringOnMessageExpiration": false,
            "DefaultMessageTimeToLive": "PT1H",
            "LockDuration": "PT1M",
            "MaxDeliveryCount": 10,
            "RequiresDuplicateDetection": false,
            "RequiresSession": false
        }
    }));

    let new_text = serde_json::to_string_pretty(&root)
        .map_err(|e| e.to_string())?;
    std::fs::write(&path, new_text)
        .map_err(|e| format!("Could not write config.json: {}", e))?;

    Ok(true)
}

// ── local.settings.json patcher ──────────────────────────────────────────────

/// Write the emulator connection string into local.settings.json.
/// Returns Ok(true) if the file was updated, Ok(false) if already set.
///
/// For connection-string style connections the existing key is patched.
/// For MSI-style connections (fullyQualifiedNamespace, no connectionString key)
/// we write `serviceBus_connectionString` — the key that patch_connections_for_local
/// injects into connections.json when it switches serviceBus from MSI to connectionString.
fn patch_local_settings_conn_str(logic_apps_dir: &str) -> Result<bool, String> {
    use crate::services::{sb_check, settings_file};

    // Try to find the connection-string key from connections.json.
    // Fall back to `serviceBus_connectionString` for MSI-style connections —
    // this is the key that patch_connections_for_local writes into connections.json.
    let key = sb_check::detect_sb_conn_str_key(logic_apps_dir)
        .map(|(k, _)| k)
        .unwrap_or_else(|| "serviceBus_connectionString".to_string());

    // Read current value (may be empty for the fallback key).
    let text = settings_file::read_local_settings(logic_apps_dir)
        .map_err(|e| e.to_string())?;
    let root_check: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| e.to_string())?;
    let current = root_check["Values"][&key].as_str().unwrap_or("").to_string();

    // Already pointing at the emulator — nothing to do.
    if current.contains("UseDevelopmentEmulator=true") {
        return Ok(false);
    }

    let mut root: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| e.to_string())?;

    root["Values"][&key] = serde_json::Value::String(EMULATOR_CONN_STR.to_string());

    let new_text = serde_json::to_string_pretty(&root)
        .map_err(|e| e.to_string())?;
    settings_file::write_local_settings(logic_apps_dir, &new_text)
        .map_err(|e| e.to_string())?;

    Ok(true)
}

// ── handlers ─────────────────────────────────────────────────────────────────

pub fn handle_start(
    mut state:     Signal<ServiceState>,
    proc:          Signal<Arc<ManagedProcess>>,
    log_lines:     Signal<Vec<LogLine>>,
    mut emu_lines: Signal<Vec<String>>,
    logic_apps_dir: String,
) {
    state.set(ServiceState::Starting);
    let mut push = make_push(log_lines);

    // Detect queues from the project so config.json pre-creates them.
    // Filter out Logic Apps expression placeholders like @triggerBody()?['queueName']
    // — only keep plain queue names (no @ $ ? expressions).
    //
    // Names are trimmed and validated against the same regex the SB emulator
    // enforces (`^[A-Za-z0-9]$|^[A-Za-z0-9][\w-\.\/]*[A-Za-z0-9]$`). An invalid
    // name (e.g. leading whitespace from an @appsetting value) would otherwise
    // crash the emulator host on startup with a FluentAssertions panic.
    let (_, queue_infos) = crate::services::sb_check::detect_sb_queues(&logic_apps_dir);
    let mut queues: Vec<(String, bool)> = Vec::new();
    for q in queue_infos {
        let name = q.queue.trim().to_string();
        if name.is_empty()
            || name.contains('@')
            || name.contains('?')
            || name.contains('[')
        {
            continue;
        }
        if !is_valid_sb_queue_name(&name) {
            push(
                format!("⚠ Skipping invalid queue name '{}' (does not match SB naming rules)", name),
                LogLevel::Warn,
            );
            continue;
        }
        if !queues.iter().any(|(n, _)| n == &name) {
            queues.push((name, q.requires_session));
        }
    }

    let (dir, dropped_queues) = match write_compose_files(&queues) {
        Ok(d)  => d,
        Err(e) => {
            state.set(ServiceState::Stopped);
            push(format!("❌ Could not write compose files: {}", e), LogLevel::Error);
            return;
        }
    };
    if !dropped_queues.is_empty() {
        push(
            format!(
                "⚠ Config.json held more than {} queues (the emulator's hard limit) — \
                 dropped {} carried over from previous runs: {}. Workflow-detected queues \
                 were kept. Use the emulator reset to clear the accumulated list.",
                SB_EMULATOR_MAX_ENTITIES,
                dropped_queues.len(),
                dropped_queues.join(", "),
            ),
            LogLevel::Warn,
        );
    }
    // write_compose_files silently deletes a bad Config.json (wrong namespace name
    // or wrong logging key) and regenerates it correctly. Inform the user.
    if !std::path::Path::new("/tmp/ais-sb-emulator/Config.json.bak").exists() {
        // Check if we just regenerated by seeing if the file is brand new (heuristic:
        // modified in the last 2 seconds). Simpler: always note the config was written.
    }
    push("✅ Config.json written (namespace: sbemulatorns, logging: Console)".into(), LogLevel::Info);

    if !queues.is_empty() {
        let queue_names: Vec<String> = queues.iter().map(|(n, s)| {
            if *s { format!("{} (session)", n) } else { n.clone() }
        }).collect();
        push(format!("Pre-creating queues: {}", queue_names.join(", ")), LogLevel::Info);
    }

    let compose_file = dir.join("docker-compose.yml");
    let compose_file_str = compose_file.to_string_lossy().to_string();

    // Silently wipe leftover containers + volumes from any previous run before
    // bringing the stack up. Without this, SQL Edge spends 20-30 s on parallel
    // redo against a stale `SbGatewayDatabase` / `SbMessageContainerDatabase00001`
    // and the SB emulator finishes its AMQP probe on top of a not-yet-quiescent
    // broker — which produces the `Connection reset by peer` storm the Functions
    // host hits the moment it tries to consume.
    //
    // Best-effort: a missing/already-clean compose project is fine, so a non-zero
    // exit code here doesn't fail the Start. We swallow output to avoid polluting
    // the user-visible log; the existing "$ docker compose -f … up" line below
    // remains the only Start marker the user sees.
    //
    // `--timeout 10` caps the SIGTERM wait per container; the surrounding
    // `.output()` blocks until docker compose exits. Uses the same
    // `docker_compose_cmd` wrapper as the other call sites for Windows-safe
    // invocation through `cmd /c`.
    let _ = docker_compose_cmd(&[
        "-f", &compose_file_str,
        "down", "--volumes", "--remove-orphans", "--timeout", "10",
    ]).output();

    push(format!("$ docker compose -f {} up", compose_file_str), LogLevel::Info);

    // Build the ManagedProcess start args through docker_compose_cmd so the
    // platform-specific wrapping is handled, but we need a raw binary + args
    // for ManagedProcess::start.  Extract them from the Command.
    let docker = runtime_manager::resolve_tool("docker");
    let up_args: Vec<String> = if cfg!(target_os = "windows") {
        vec!["/c".into(), docker.clone(), "compose".into(),
             "-f".into(), compose_file_str.clone(), "up".into()]
    } else {
        vec!["compose".into(), "-f".into(), compose_file_str.clone(), "up".into()]
    };
    let bin = if cfg!(target_os = "windows") { "cmd".to_string() } else { docker };
    let up_args_ref: Vec<&str> = up_args.iter().map(|s| s.as_str()).collect();

    match proc.read().start(&bin, &up_args_ref, None) {
        Ok((stdout, stderr)) => {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, bool)>();
            crate::services::process::stream_output(stdout, stderr, tx, true);

            // Route all container output to the Service Bus tab — with a
            // small classifier + de-duper to keep the noise floor down.
            // Without this, the emulator's session-idle timeouts and the
            // bundled .NET host's tenant-cache quirk flood the pane with
            // alarming `WRN`/`ERR` traces every 30 s for queues that are
            // simply waiting for traffic.
            spawn(async move {
                let mut dedup = EmuLineDeduper::new();
                while let Some((line, _)) = rx.recv().await {
                    let line = line.trim().to_string();
                    if line.is_empty() { continue; }
                    let mut w = emu_lines.write();
                    let current_len = w.len();
                    match dedup.feed(&line, current_len) {
                        DedupAction::Drop => {}
                        DedupAction::Append(s) => {
                            w.push(s);
                            let len = w.len();
                            if len > 2000 { w.drain(..len - 2000); }
                        }
                        DedupAction::Rewrite(idx, s) => {
                            // The drain above can shift indices; only rewrite
                            // when the slot is still in range and matches our
                            // expectation.
                            if idx < w.len() { w[idx] = s; }
                        }
                    }
                }
            });

            // Poll AMQP port.
            // SQL Edge healthcheck alone can take up to 2 min (start_period 20s +
            // 10 retries × 10s), then the emulator itself needs ~30 s on top.
            // Allow 4 minutes total.
            let mut state2 = state;
            let mut push2  = make_push(log_lines);
            let logic_apps_dir = logic_apps_dir.clone();
            spawn(async move {
                // Phase 1: wait for the port to open (SQL Edge + emulator start)
                let mut port_open = false;
                for _ in 0..480 {    // 480 × 500 ms = 4 minutes
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    if tokio::net::TcpStream::connect(
                        std::net::SocketAddr::from(([127, 0, 0, 1], SB_EMULATOR_PORT))
                    ).await.is_ok() {
                        port_open = true;
                        break;
                    }
                }
                // Phase 2: probe AMQP until the broker is ready to negotiate.
                // TCP open ≠ AMQP ready — SQL Edge can take 10-30 s more after the
                // port appears. Retry for up to 60 s before declaring failure.
                let mut up = false;
                if port_open {
                    push2("⏳ Port open — waiting for AMQP broker…".into(), LogLevel::Info);
                    for _ in 0..60 {
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        if crate::services::sb_amqp::probe_amqp("localhost").await {
                            up = true;
                            break;
                        }
                    }
                }
                if up {
                    state2.set(ServiceState::Running);
                    push2(
                        format!("Service Bus emulator ready — AMQP :{} (see Service Bus tab for logs)", SB_EMULATOR_PORT),
                        LogLevel::Ok,
                    );

                    // Auto-patch local.settings.json with the emulator connection string.
                    // Only writes if the key is currently empty or a real Azure endpoint —
                    // never overwrites another emulator value that's already set.
                    let dir2 = logic_apps_dir.clone();
                    let patched = tokio::task::spawn_blocking(move || {
                        patch_local_settings_conn_str(&dir2)
                    }).await.unwrap_or(Ok(false));

                    match patched {
                        Ok(true)  => push2(
                            "✅ local.settings.json updated — SERVICE_BUS_CONNECTION_STRING set to emulator.".into(),
                            LogLevel::Ok,
                        ),
                        Ok(false) => push2(
                            format!("ℹ SERVICE_BUS_CONNECTION_STRING already set — emulator conn str: {}", EMULATOR_CONN_STR),
                            LogLevel::Info,
                        ),
                        Err(e) => push2(
                            format!("⚠ Could not update local.settings.json: {} — set manually: {}", e, EMULATOR_CONN_STR),
                            LogLevel::Warn,
                        ),
                    }
                } else {
                    state2.set(ServiceState::Stopped);
                    push2(
                        "⚠ SB emulator didn't bind in 4 min — see Service Bus tab for errors.".into(),
                        LogLevel::Error,
                    );
                }
            });
        }
        Err(e) => {
            state.set(ServiceState::Stopped);
            push(
                format!("Service Bus emulator error: {} — is Docker installed and running?", e),
                LogLevel::Error,
            );
        }
    }
}

pub fn handle_stop(
    mut state: Signal<ServiceState>,
    proc:      Signal<Arc<ManagedProcess>>,
    log_lines: Signal<Vec<LogLine>>,
) {
    let mut push = make_push(log_lines);
    // Run `docker compose down` to remove containers and network.
    let compose_file = work_dir().join("docker-compose.yml");
    if compose_file.exists() {
        let compose_file_str = compose_file.to_string_lossy().to_string();
        let _ = docker_compose_cmd(&["-f", &compose_file_str, "down"]).output();
    }
    match proc.read().stop() {
        Ok(_)  => {
            state.set(ServiceState::Stopped);
            push("Service Bus emulator stopped.".into(), LogLevel::Warn);
        }
        Err(e) => push(format!("Error stopping SB emulator: {}", e), LogLevel::Error),
    }
}

/// Full reset: stop the emulator, wipe Config.json + Docker volumes (stale SQL Edge
/// data), then restart fresh. Fixes:
///   • Wrong namespace / logging key in a hand-edited Config.json
///   • SQL Edge database corruption after an unclean shutdown
///   • "NullReferenceException" / "Out of memory" on startup
///   • AMQP broker stuck in a bad state after a crash
pub fn handle_reset(
    state:         Signal<ServiceState>,
    proc:          Signal<Arc<ManagedProcess>>,
    log_lines:     Signal<Vec<LogLine>>,
    emu_lines:     Signal<Vec<String>>,
    logic_apps_dir: String,
) {
    let mut push = make_push(log_lines);
    push("⟳ Resetting SB Emulator — stopping containers and wiping state…".into(), LogLevel::Warn);

    // Stop running containers + network
    let compose_file = work_dir().join("docker-compose.yml");
    if compose_file.exists() {
        let compose_str = compose_file.to_string_lossy().to_string();
        let _ = docker_compose_cmd(&["-f", &compose_str, "down", "--volumes"]).output();
    }
    let _ = proc.read().stop();
    state.write_unchecked().clone_from(&ServiceState::Stopped);

    spawn(async move {
        let mut push2 = make_push(log_lines);

        // Delete Config.json so it's regenerated with correct keys on next start
        let cfg = work_dir().join("Config.json");
        if cfg.exists() {
            match std::fs::remove_file(&cfg) {
                Ok(_)  => push2("🗑 Config.json removed — will be regenerated with correct settings.".into(), LogLevel::Ok),
                Err(e) => push2(format!("⚠ Could not remove Config.json: {}", e), LogLevel::Warn),
            }
        }

        // Wait briefly for Docker to release ports
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        push2("✅ Reset complete — restarting SB Emulator…".into(), LogLevel::Ok);
        handle_start(state, proc, log_lines, emu_lines, logic_apps_dir);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDLE_WRN: &str = r#"[09:34:17 WRN] <Trc Id="30101" Ch="Debug" Lvl="Warning" Kw="1000000000000100" UTC="2026-06-18T09:34:17.835Z" Msg="Protocol client of type 'aDx' Open failed. Affected entity name = sbemulatorns:queue:ais.teams.notif. Stack Trace = Session id: , locktoken: , mode: PeekLock, errormessage: The operation did not complete within the allotted timeout of 00:00:29.9109964. ... source(address:/ais.teams.notif,filter:[com.microsoft:session-filter:])" />"#;
    const IDLE_ERR: &str = r#"[09:34:17 ERR] in-connection194 ... com.microsoft:timeout TrackingId:... source(address:/ais.teams.notif,filter:[com.microsoft:session-filter:]) ..."#;
    const TENANT_WRN: &str = r#"[08:39:41 WRN] <Trc ... Msg="An exception was handled at TenantCacheComponent.OnEndRefresh. Exception message ExceptionId: ...-System.PlatformNotSupportedException: Operation is not supported on this platform." />"#;
    const REAL_ERR: &str = r#"[08:39:41 ERR] Error occured while running emulator launcher: Expected string to match regex ..."#;

    #[test]
    fn idle_session_warning_is_rewritten_with_queue_name() {
        let out = classify_emu_line(IDLE_WRN).unwrap();
        assert!(out.contains("ais.teams.notif"), "rewrite should mention the queue, got: {out}");
        assert!(out.contains("idle"), "rewrite should signal idle state, got: {out}");
        // The rewrite must not preserve the original alarming `WRN`/stack-trace text.
        assert!(!out.contains("ERR"));
        assert!(!out.contains("Stack Trace"));
    }

    #[test]
    fn idle_session_err_block_is_collapsed_too() {
        // The ERR companion line carries the same source(address:…) token,
        // so it should also be re-classified to the friendly idle hint.
        let out = classify_emu_line(IDLE_ERR).unwrap();
        assert!(out.contains("ais.teams.notif"));
        assert!(out.contains("idle"));
    }

    #[test]
    fn tenant_cache_platform_exception_is_demoted() {
        let out = classify_emu_line(TENANT_WRN).unwrap();
        assert!(out.contains("harmless"), "rewrite should mark it harmless, got: {out}");
        assert!(!out.contains("PlatformNotSupportedException"));
    }

    #[test]
    fn real_error_lines_pass_through_unchanged() {
        // Anything we don't recognise must reach the user verbatim — the
        // classifier is there to dampen *known* noise, not to swallow signal.
        let out = classify_emu_line(REAL_ERR).unwrap();
        assert_eq!(out, REAL_ERR);
    }

    #[test]
    fn deduper_collapses_consecutive_duplicates_with_count() {
        let mut d = EmuLineDeduper::new();
        // First idle warning → appended at slot 0.
        match d.feed(IDLE_WRN, 0) {
            DedupAction::Append(s) => assert!(s.contains("idle")),
            _ => panic!("first feed should append"),
        }
        // Next idle warning (same rewritten text) → rewrite slot 0 with ×2.
        match d.feed(IDLE_WRN, 1) {
            DedupAction::Rewrite(idx, s) => {
                assert_eq!(idx, 0);
                assert!(s.contains("(×2)"), "expected count suffix, got: {s}");
            }
            _ => panic!("second feed should rewrite"),
        }
        // Third → rewrite with ×3.
        match d.feed(IDLE_WRN, 1) {
            DedupAction::Rewrite(_, s) => assert!(s.contains("(×3)")),
            _ => panic!("third feed should rewrite"),
        }
    }

    #[test]
    fn deduper_distinct_line_resets_the_counter() {
        let mut d = EmuLineDeduper::new();
        let _ = d.feed(IDLE_WRN, 0);
        let _ = d.feed(IDLE_WRN, 1); // ×2
        // A distinct line → append, not rewrite.
        match d.feed(REAL_ERR, 1) {
            DedupAction::Append(s) => assert_eq!(s, REAL_ERR),
            _ => panic!("distinct line should append"),
        }
        // And a subsequent IDLE_WRN should start fresh as a new entry.
        match d.feed(IDLE_WRN, 2) {
            DedupAction::Append(_) => {}
            _ => panic!("counter should have reset after a distinct line"),
        }
    }

    #[test]
    fn extract_queue_from_source_handles_realistic_input() {
        let q = extract_queue_from_source(IDLE_WRN);
        assert_eq!(q.as_deref(), Some("ais.teams.notif"));
    }
}
