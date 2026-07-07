use dioxus::prelude::*;

#[derive(Debug, Clone, PartialEq)]
pub struct LogLine {
    pub time: String,
    pub msg: String,
    pub level: LogLevel,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LogLevel {
    Info,
    Ok,
    Warn,
    Error,
}

impl LogLevel {
    pub fn css_class(&self) -> &'static str {
        match self {
            LogLevel::Info  => "log-msg info",
            LogLevel::Ok    => "log-msg ok",
            LogLevel::Warn  => "log-msg warn",
            LogLevel::Error => "log-msg error",
        }
    }
}

fn az_line_class(line: &str) -> &'static str {
    if line.contains("error") || line.contains("Error") { "log-msg error" }
    else if line.contains("warn")  || line.contains("Warn")  { "log-msg warn" }
    else { "log-msg info" }
}

// ── Azurite poll-noise filter ─────────────────────────────────────────────

/// Returns true for HTTP access log lines that are routine job-scheduler
/// heartbeats (GET/POST/PUT on jobdefinitions or histories with 2xx status).
/// These repeat every ~60 s per workflow and carry no actionable information.
pub fn is_azurite_poll_noise(line: &str) -> bool {
    if !line.contains("HTTP/1.1") { return false; }
    // Must be a 2xx response
    let ok = line.ends_with(" 200 -") || line.ends_with(" 201 -") || line.ends_with(" 204 -");
    if !ok { return false; }
    // jobdefinitions / histories — job scheduler heartbeats
    // flows — runtime polling "is workflow X still active?" (every 2 s per workflow)
    line.contains("jobdefinitions") || line.contains("histories") || line.contains("flows(")
}

/// Percent-decodes a URL-encoded string (handles `%XX`).
fn pct_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(hi), Some(lo)) = (
                char::from(b[i + 1]).to_digit(16),
                char::from(b[i + 2]).to_digit(16),
            ) {
                out.push((((hi << 4) | lo) as u8) as char);
                i += 3;
                continue;
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

/// Extracts a human-readable queue name from an Azurite HTTP poll line.
///
/// The RowKey encodes the trigger name like:
///   `FlowTriggerJob-WHEN:5FMESSAGES:5FARE:5FAVAILABLE:5FIN:5F{QUEUE}`
/// where `:XX` is a secondary percent-encoding (`:` = `%`).
pub fn extract_azurite_poll_queue(line: &str) -> Option<String> {
    // First, URL-decode the whole line so %3A → : etc.
    let decoded_line = pct_decode(line);

    let marker = "FlowTriggerJob-WHEN";
    let start  = decoded_line.find(marker)? + marker.len();
    let rest   = &decoded_line[start..];
    let end    = rest.find(|c| c == '\'' || c == ')' || c == '?').unwrap_or(rest.len());
    let raw    = &rest[..end];

    // Secondary decode: treat `:` as `%` for two-hex sequences
    let mut decoded = String::with_capacity(raw.len());
    let rb = raw.as_bytes();
    let mut i = 0;
    while i < rb.len() {
        if rb[i] == b':' && i + 2 < rb.len() {
            if let (Some(hi), Some(lo)) = (
                char::from(rb[i + 1]).to_digit(16),
                char::from(rb[i + 2]).to_digit(16),
            ) {
                decoded.push((((hi << 4) | lo) as u8) as char);
                i += 3;
                continue;
            }
        }
        decoded.push(rb[i] as char);
        i += 1;
    }

    // Strip the leading `_MESSAGES_ARE_AVAILABLE_IN_` portion
    let sep = "_MESSAGES_ARE_AVAILABLE_IN_";
    let queue = if let Some(pos) = decoded.find(sep) {
        decoded[pos + sep.len()..].to_string()
    } else {
        decoded
    };

    let queue = queue.trim().to_lowercase();
    if queue.is_empty() { None } else { Some(queue) }
}

/// Splits the raw Azurite debug-log lines into:
///   - `display`: lines to show verbatim (non-poll or errors)
///   - `polling`: set of queue names actively polled (seen in last 20 poll lines)
///   - `stale`:   queue names that appeared early but NOT in the last 20 poll lines
///   - `poll_hidden`: total number of suppressed poll lines
pub fn process_azurite_lines(
    lines: &[String],
) -> (Vec<String>, Vec<String>, Vec<String>, usize) {
    use std::collections::{HashMap, HashSet};

    let mut display: Vec<String>  = Vec::new();
    let mut poll_hidden           = 0usize;
    // queue → (first_seen_idx, last_seen_idx) among poll lines only
    let mut queue_span: HashMap<String, (usize, usize)> = HashMap::new();
    let mut poll_count = 0usize;

    for line in lines {
        let is_noise = is_azurite_poll_noise(line);
        // Always show errors even if they look like poll lines
        let is_error = {
            let s = line.trim_end();
            // HTTP status ≥ 400
            let status_4xx = s.ends_with(" 400 -") || s.ends_with(" 401 -") ||
                             s.ends_with(" 403 -") || s.ends_with(" 404 -") ||
                             s.ends_with(" 409 -") || s.ends_with(" 500 -") ||
                             s.ends_with(" 503 -");
            status_4xx || (!s.contains("HTTP/1.1") && (s.contains("error") || s.contains("Error")))
        };

        if is_noise && !is_error {
            poll_hidden += 1;
            if let Some(q) = extract_azurite_poll_queue(line) {
                let e = queue_span.entry(q).or_insert((poll_count, poll_count));
                e.1 = poll_count;
            }
            poll_count += 1;
        } else {
            display.push(line.clone());
        }
    }

    // "recent" = seen in the last 20 poll-line slots
    let recent_threshold = poll_count.saturating_sub(20);
    let mut polling: Vec<String> = Vec::new();
    let mut stale:   Vec<String> = Vec::new();

    let mut seen_queues: HashSet<String> = queue_span.keys().cloned().collect();
    let mut sorted: Vec<String> = seen_queues.drain().collect();
    sorted.sort();

    for q in sorted {
        if let Some(&(_, last)) = queue_span.get(&q) {
            if last >= recent_threshold {
                polling.push(q);
            } else {
                stale.push(q);
            }
        }
    }

    (display, polling, stale, poll_hidden)
}

/// Returns true for .NET stack-frame lines that repeat every ~60 s and carry no
/// actionable information (e.g. "at System.ArgumentNullException.Throw(String paramName)").
pub fn is_stack_frame_noise(msg: &str) -> bool {
    let s = msg.trim_start();
    let s = if s.starts_with('[') {
        s.find("] ").map(|i| s[i + 2..].trim_start()).unwrap_or(s)
    } else {
        s
    };
    s.starts_with("at ") && s.contains('.') && s.contains('(')
}

/// Filters known-harmless noise lines from the SB emulator Docker container output.
/// These come from Windows Fabric subsystems not fully implemented in the Linux image
/// and do not affect AMQP message delivery.
pub fn is_sb_emulator_noise(line: &str) -> bool {
    // Internal Service Fabric management operations not implemented on Linux
    line.contains("NotImplementedException") ||
    line.contains("ShouldUseWindowsFabricResolver") ||
    line.contains("Not using SF-based resolver") ||
    line.contains("failed to report load to Winfab") ||
    line.contains("Service 'q.qm' not found") ||
    // Transient SQL Edge init errors during startup (resolve on their own)
    line.contains("BufferQueue") ||
    line.contains("Entity 'Microsoft.ServiceBus.MessageContainer") ||
    // Performance counter support not available on Linux
    line.contains("Performance Counters are not supported") ||
    line.contains("CounterSet") ||
    // Recoverable config warnings already handled by ais-runner
    line.contains("Recoverable validation failed") ||
    // High-frequency internal management noise (repeats every 3 min)
    (line.contains("Trc Id=\"40065\"") || line.contains("Trc Id=\"40104\"") ||
     line.contains("Trc Id=\"40064\"") || line.contains("Trc Id=\"30588\"") ||
     line.contains("Trc Id=\"32004\"") || line.contains("Trc Id=\"30504\""))
}

/// Filters Maven build-progress and JVM deprecation lines that add no value
/// in the ais-runner console (they're already visible in a real terminal if needed).
pub fn is_mvn_noise(msg: &str) -> bool {
    let t = msg.trim_start();
    // Maven artifact download progress: "Progress (N): X kB | …"
    if t.starts_with("Progress (") { return true; }
    // Maven downloaded confirmation: "Downloaded from central: https://…"
    if t.starts_with("Downloaded from ") { return true; }
    // JVM sun.misc.Unsafe deprecation warnings (Java 17+)
    if t.contains("sun.misc.Unsafe") { return true; }
    if t.contains("terminally deprecated method") { return true; }
    if t.contains("Please consider reporting this to the maintainers") { return true; }
    false
}

pub fn is_sb_noise(msg: &str) -> bool {
    msg.contains("An unhandled exception occurred in the message batch receive loop") ||
    msg.contains("aka.ms/azsdk/net/servicebus/exceptions/troubleshoot") ||
    msg.contains("serviceBus.ServiceBusServiceOperationsProvider") ||
    (msg.contains("ServiceProviderRecurrenceTriggerJob") && msg.contains("serviceBus")) ||
    (msg.contains("ServiceProviderActionJob") && msg.contains("serviceBus")) ||
    msg.contains("onNewMessagesFromQueueSession") ||
    (msg.contains("Outgoing HTTP request ends with server failure") && msg.contains("hostName='serviceBus'"))
}

fn az_split(line: &str) -> (&str, &str) {
    if let Some(sp) = line.find(' ') {
        let ts = &line[..sp];
        let time = if ts.len() >= 19 { &ts[11..19] } else { ts };
        (time, line[sp..].trim_start())
    } else {
        ("", line)
    }
}

#[derive(Props, Clone, PartialEq)]
pub struct LogPanelProps {
    pub lines:        Signal<Vec<LogLine>>,
    pub az_lines:     Signal<Vec<String>>,
    pub sb_emu_lines: Signal<Vec<String>>,
    pub java_lines:    Signal<Vec<String>>,
    pub sql_dev_lines: Signal<Vec<String>>,
    pub on_clear:      EventHandler<()>,
    /// Logic Apps workspace path — used by the Mock tab to scan and run the
    /// embedded HTTP mock server.
    pub workspace_dir: String,
}

#[component]
pub fn LogPanel(props: LogPanelProps) -> Element {
    let mut active_tab = use_signal(|| "console");
    // az_lines is owned by the parent screen so RunDetail can render storage
    // events inline next to actions. The tail-debug.log coroutine below still
    // lives here — it writes into the shared Signal.
    let mut az_lines: Signal<Vec<String>> = props.az_lines;
    let mut sb_emu_lines = props.sb_emu_lines;
    let mut java_lines    = props.java_lines;
    let mut sql_dev_lines = props.sql_dev_lines;

    // ── Pause snapshots — None = live, Some(snapshot) = paused ──────────────
    let mut console_snap: Signal<Option<Vec<LogLine>>>    = use_signal(|| None);
    let mut az_snap:      Signal<Option<Vec<String>>>     = use_signal(|| None);
    let mut sb_snap:      Signal<Option<(Vec<String>, Vec<LogLine>)>> = use_signal(|| None);
    let mut java_snap:    Signal<Option<Vec<String>>>     = use_signal(|| None);
    let mut sql_snap:     Signal<Option<Vec<String>>>     = use_signal(|| None);

    // ── Per-tab text filter ───────────────────────────────────────────────────
    let console_filter: Signal<String> = use_signal(String::new);
    let az_filter:      Signal<String> = use_signal(String::new);
    let sb_filter:      Signal<String> = use_signal(String::new);
    let java_filter:    Signal<String> = use_signal(String::new);
    let sql_filter:     Signal<String> = use_signal(String::new);
    let mock_filter:    Signal<String> = use_signal(String::new);

    // ── Mock tab state ──────────────────────────────────────────────────────
    // Lines of formatted human-readable log output from MockEvents.
    let mut mock_lines:   Signal<Vec<String>>     = use_signal(Vec::new);
    let mut mock_snap:    Signal<Option<Vec<String>>> = use_signal(|| None);
    // Status line shown above the log: scan summary, server URL, etc.
    let mut mock_status:  Signal<String>          = use_signal(String::new);
    let mut mock_busy:    Signal<bool>            = use_signal(|| false);
    let workspace_dir = props.workspace_dir.clone();

    // ── Auto-scroll: only when not paused ───────────────────────────────────
    use_effect(move || {
        let _n = props.lines.read().len();
        if console_snap.read().is_none() {
            document::eval("var e=document.getElementById('log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
        }
    });
    use_effect(move || {
        let _n = az_lines.read().len();
        if az_snap.read().is_none() {
            document::eval("var e=document.getElementById('az-log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
        }
    });
    use_effect(move || {
        let _n = sb_emu_lines.read().len();
        if sb_snap.read().is_none() {
            document::eval("var e=document.getElementById('sb-log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
        }
    });
    use_effect(move || {
        let _n = java_lines.read().len();
        if java_snap.read().is_none() {
            document::eval("var e=document.getElementById('java-log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
        }
    });
    use_effect(move || {
        let _n = sql_dev_lines.read().len();
        if sql_snap.read().is_none() {
            document::eval("var e=document.getElementById('sql-log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
        }
    });
    use_effect(move || {
        let _n = mock_lines.read().len();
        if mock_snap.read().is_none() {
            document::eval("var e=document.getElementById('mock-log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
        }
    });

    // tail -f azurite debug.log  (polls every 500 ms)
    // Never read the file whole: azurite doesn't rotate debug.log, so it can
    // reach tens of GB, and this coroutine is polled on the GUI thread — an
    // unbounded read freezes the app until macOS kills it out of memory.
    // Tail from the end on first sight and cap every read at TAIL_CAP.
    use_coroutine(move |_rx: UnboundedReceiver<()>| async move {
        const TAIL_CAP: u64 = 1024 * 1024;
        let path = crate::utils::azurite_log();
        let mut offset: Option<u64> = None; // None → jump to EOF on next read
        loop {
            match tokio::fs::metadata(&path).await {
                Ok(meta) => {
                    let len = meta.len();
                    if matches!(offset, Some(o) if len < o) {
                        // Truncated/rotated — start over from the top.
                        offset = Some(0);
                        az_lines.write().clear();
                    }
                    // Jump to the last TAIL_CAP bytes on first sight of the
                    // file, or when it grew faster than we can catch up.
                    let jumped = len.saturating_sub(offset.unwrap_or(0)) > TAIL_CAP;
                    let start = if jumped { len - TAIL_CAP } else { offset.unwrap_or(0) };
                    if len > start {
                        if let Ok(mut f) = tokio::fs::File::open(&path).await {
                            use tokio::io::{AsyncReadExt, AsyncSeekExt};
                            if f.seek(std::io::SeekFrom::Start(start)).await.is_ok() {
                                let mut buf = String::new();
                                if f.take(len - start).read_to_string(&mut buf).await.is_ok() {
                                    offset = Some(len);
                                    let new: Vec<String> = buf
                                        .lines()
                                        // A jump lands mid-line — drop the partial first line.
                                        .skip(if jumped && start > 0 { 1 } else { 0 })
                                        .filter(|l| !l.is_empty())
                                        .map(|l| l.to_string())
                                        .collect();
                                    if !new.is_empty() {
                                        let mut w = az_lines.write();
                                        w.extend(new);
                                        let len = w.len();
                                        if len > 500 {
                                            let drain_to = len - 500;
                                            w.drain(..drain_to);
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        offset = Some(len);
                    }
                }
                Err(_) => {
                    if offset.is_some_and(|o| o > 0) {
                        az_lines.write().clear();
                    }
                    // Re-arm the EOF jump for when the file reappears.
                    offset = None;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    });

    let tab = active_tab();

    // ── New-line counts while paused ─────────────────────────────────────────
    let console_new = console_snap.read().as_ref().map(|snap| {
        let live: Vec<_> = props.lines.read().iter()
            .filter(|l| !is_sb_noise(&l.msg) && !is_stack_frame_noise(&l.msg) && !is_mvn_noise(&l.msg))
            .cloned().collect();
        live.len().saturating_sub(snap.len())
    });
    let az_new = az_snap.read().as_ref().map(|snap| {
        az_lines.read().len().saturating_sub(snap.len())
    });
    let sb_new = sb_snap.read().as_ref().map(|snap| {
        let live_emu = sb_emu_lines.read().len();
        let live_noise = props.lines.read().iter().filter(|l| is_sb_noise(&l.msg)).count();
        (live_emu + live_noise).saturating_sub(snap.0.len() + snap.1.len())
    });

    let sb_noise_count = props.lines.read().iter().filter(|l| is_sb_noise(&l.msg)).count();
    let sb_emu_count   = sb_emu_lines.read().len();
    let sb_count       = sb_noise_count + sb_emu_count;
    let java_new = java_snap.read().as_ref().map(|snap| {
        java_lines.read().len().saturating_sub(snap.len())
    });
    let java_count = java_lines.read().len();
    let sql_new = sql_snap.read().as_ref().map(|snap| sql_dev_lines.read().len().saturating_sub(snap.len()));
    let sql_count = sql_dev_lines.read().len();

    // ── Rendered lines (with filter applied) ─────────────────────────────────
    let cf = console_filter.read().to_lowercase();
    let af = az_filter.read().to_lowercase();
    let sf = sb_filter.read().to_lowercase();
    let jf  = java_filter.read().to_lowercase();
    let sqf = sql_filter.read().to_lowercase();
    let mf  = mock_filter.read().to_lowercase();

    let console_lines: Vec<LogLine> = {
        let base: Vec<LogLine> = match console_snap.read().clone() {
            Some(snap) => snap,
            None => props.lines.read().iter()
                .filter(|l| !is_sb_noise(&l.msg) && !is_stack_frame_noise(&l.msg) && !is_mvn_noise(&l.msg))
                .cloned().collect(),
        };
        if cf.is_empty() { base }
        else { base.into_iter().filter(|l| l.msg.to_lowercase().contains(&cf)).collect() }
    };
    let az_display: Vec<String> = {
        let base: Vec<String> = match az_snap.read().clone() {
            Some(snap) => snap,
            None => az_lines.read().clone(),
        };
        if af.is_empty() { base }
        else { base.into_iter().filter(|l| l.to_lowercase().contains(&af)).collect() }
    };
    let sql_display: Vec<String> = {
        let base = match sql_snap.read().clone() {
            Some(snap) => snap,
            None => sql_dev_lines.read().clone(),
        };
        if sqf.is_empty() { base } else { base.into_iter().filter(|l| l.to_lowercase().contains(&sqf)).collect() }
    };
    let java_display: Vec<String> = {
        let base = match java_snap.read().clone() {
            Some(snap) => snap,
            None => java_lines.read().clone(),
        };
        if jf.is_empty() { base }
        else { base.into_iter().filter(|l| l.to_lowercase().contains(&jf)).collect() }
    };
    let mock_display: Vec<String> = {
        let base = match mock_snap.read().clone() {
            Some(snap) => snap,
            None       => mock_lines.read().clone(),
        };
        if mf.is_empty() { base }
        else { base.into_iter().filter(|l| l.to_lowercase().contains(&mf)).collect() }
    };
    let (sb_emu_display, sb_noise_display): (Vec<String>, Vec<LogLine>) = {
        let (base_emu, base_noise) = match sb_snap.read().clone() {
            Some(snap) => snap,
            None => (
                sb_emu_lines.read().clone(),
                props.lines.read().iter().filter(|l| is_sb_noise(&l.msg)).cloned().collect(),
            ),
        };
        if sf.is_empty() {
            (base_emu, base_noise)
        } else {
            (
                base_emu.into_iter().filter(|l| l.to_lowercase().contains(&sf)).collect(),
                base_noise.into_iter().filter(|l| l.msg.to_lowercase().contains(&sf)).collect(),
            )
        }
    };

    rsx! {
        div { id: "log-panel",
            div { id: "log-header",

                // ── Left: tab buttons ─────────────────────────────────────
                div { class: "log-tabs-left",
                    button {
                        class: if tab == "console" { "log-tab active" } else { "log-tab" },
                        onclick: move |_| {
                            active_tab.set("console");
                            if console_snap.read().is_none() {
                                document::eval("var e=document.getElementById('log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
                            }
                        },
                        "Console"
                        if console_snap.read().is_some() { span { class: "log-paused-badge", "⏸" } }
                    }
                    button {
                        class: if tab == "azurite" { "log-tab active" } else { "log-tab" },
                        onclick: move |_| {
                            active_tab.set("azurite");
                            if az_snap.read().is_none() {
                                document::eval("var e=document.getElementById('az-log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
                            }
                        },
                        "Azurite"
                        if az_snap.read().is_some() { span { class: "log-paused-badge", "⏸" } }
                    }
                    button {
                        class: if tab == "servicebus" { "log-tab active" } else { "log-tab" },
                        onclick: move |_| {
                            active_tab.set("servicebus");
                            if sb_snap.read().is_none() {
                                document::eval("var e=document.getElementById('sb-log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
                            }
                        },
                        if tab != "servicebus" && sb_count > 0 { "Service Bus ({sb_count})" }
                        else { "Service Bus" }
                        if sb_snap.read().is_some() { span { class: "log-paused-badge", "⏸" } }
                    }
                    button {
                        class: if tab == "java" { "log-tab active" } else { "log-tab" },
                        onclick: move |_| {
                            active_tab.set("java");
                            if java_snap.read().is_none() {
                                document::eval("var e=document.getElementById('java-log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
                            }
                        },
                        if tab != "java" && java_count > 0 { "Java ({java_count})" }
                        else { "Java" }
                        if java_snap.read().is_some() { span { class: "log-paused-badge", "⏸" } }
                    }
                    button {
                        class: if tab == "sqldev" { "log-tab active" } else { "log-tab" },
                        onclick: move |_| {
                            active_tab.set("sqldev");
                            if sql_snap.read().is_none() {
                                document::eval("var e=document.getElementById('sql-log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
                            }
                        },
                        if tab != "sqldev" && sql_count > 0 { "SQL Dev ({sql_count})" }
                        else { "SQL Dev" }
                        if sql_snap.read().is_some() { span { class: "log-paused-badge", "⏸" } }
                    }
                    button {
                        class: if tab == "mock" { "log-tab active" } else { "log-tab" },
                        onclick: move |_| {
                            active_tab.set("mock");
                            if mock_snap.read().is_none() {
                                document::eval("var e=document.getElementById('mock-log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
                            }
                        },
                        {
                            let n = mock_lines.read().len();
                            if tab != "mock" && n > 0 { rsx! { "Mock ({n})" } }
                            else                       { rsx! { "Mock" } }
                        }
                        if mock_snap.read().is_some() { span { class: "log-paused-badge", "⏸" } }
                    }
                }

                // ── Right: filter + pause + clear ─────────────────────────
                div { class: "log-controls-right",

                    // Filter input
                    {
                        let (mut filter_sig, placeholder) = match tab {
                            "azurite"    => (az_filter,      "Filter Azurite…"),
                            "servicebus" => (sb_filter,      "Filter Service Bus…"),
                            "java"       => (java_filter,    "Filter Java…"),
                            "sqldev"     => (sql_filter,     "Filter SQL Dev…"),
                            "mock"       => (mock_filter,    "Filter Mock…"),
                            _            => (console_filter, "Filter console…"),
                        };
                        let has_filter = !filter_sig.read().is_empty();
                        rsx! {
                            div { class: "log-filter-wrap",
                                input {
                                    class: "log-filter-input",
                                    r#type: "text",
                                    placeholder: "{placeholder}",
                                    value: "{filter_sig}",
                                    oninput: move |e| filter_sig.set(e.value()),
                                }
                                if has_filter {
                                    button {
                                        class: "log-filter-clear",
                                        title: "Clear filter",
                                        onclick: move |_| filter_sig.set(String::new()),
                                        "×"
                                    }
                                }
                            }
                        }
                    }

                    // Pause / resume button
                    {
                        let mock_new = mock_snap.read().as_ref().map(|snap| mock_lines.read().len().saturating_sub(snap.len()));
                        let (paused, new_count) = match tab {
                            "azurite"    => (az_snap.read().is_some(),      az_new),
                            "servicebus" => (sb_snap.read().is_some(),      sb_new),
                            "java"       => (java_snap.read().is_some(),    java_new),
                            "sqldev"     => (sql_snap.read().is_some(),     sql_new),
                            "mock"       => (mock_snap.read().is_some(),    mock_new),
                            _            => (console_snap.read().is_some(), console_new),
                        };
                        rsx! {
                            button {
                                class: if paused { "btn btn-small log-pause-btn paused" } else { "btn btn-small log-pause-btn" },
                                title: if paused { "Resume — scroll to live tail" } else { "Pause — freeze display here" },
                                onclick: move |_| {
                                    match active_tab() {
                                        "azurite" => {
                                            if az_snap.read().is_some() {
                                                az_snap.set(None);
                                                document::eval("var e=document.getElementById('az-log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
                                            } else {
                                                az_snap.set(Some(az_lines.read().clone()));
                                            }
                                        }
                                        "servicebus" => {
                                            if sb_snap.read().is_some() {
                                                sb_snap.set(None);
                                                document::eval("var e=document.getElementById('sb-log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
                                            } else {
                                                let emu   = sb_emu_lines.read().clone();
                                                let noise = props.lines.read().iter().filter(|l| is_sb_noise(&l.msg)).cloned().collect();
                                                sb_snap.set(Some((emu, noise)));
                                            }
                                        }
                                        "java" => {
                                            if java_snap.read().is_some() {
                                                java_snap.set(None);
                                                document::eval("var e=document.getElementById('java-log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
                                            } else {
                                                java_snap.set(Some(java_lines.read().clone()));
                                            }
                                        }
                                        "sqldev" => {
                                            if sql_snap.read().is_some() {
                                                sql_snap.set(None);
                                                document::eval("var e=document.getElementById('sql-log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
                                            } else {
                                                sql_snap.set(Some(sql_dev_lines.read().clone()));
                                            }
                                        }
                                        "mock" => {
                                            if mock_snap.read().is_some() {
                                                mock_snap.set(None);
                                                document::eval("var e=document.getElementById('mock-log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
                                            } else {
                                                mock_snap.set(Some(mock_lines.read().clone()));
                                            }
                                        }
                                        _ => {
                                            if console_snap.read().is_some() {
                                                console_snap.set(None);
                                                document::eval("var e=document.getElementById('log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
                                            } else {
                                                let snap = props.lines.read().iter()
                                                    .filter(|l| !is_sb_noise(&l.msg) && !is_stack_frame_noise(&l.msg) && !is_mvn_noise(&l.msg))
                                                    .cloned().collect();
                                                console_snap.set(Some(snap));
                                            }
                                        }
                                    }
                                },
                                if paused {
                                    "▶"
                                    if let Some(n) = new_count { if n > 0 { span { class: "log-new-count", "+{n}" } } }
                                } else {
                                    "⏸"
                                }
                            }
                        }
                    }

                    // Clear button
                    button {
                        class: "btn btn-small",
                        style: "background:#21262d;color:#8b949e",
                        onclick: move |_| {
                            match active_tab() {
                                "azurite" => {
                                    az_snap.set(None);
                                    az_lines.write().clear();
                                }
                                "servicebus" => {
                                    sb_snap.set(None);
                                    sb_emu_lines.write().clear();
                                    props.on_clear.call(());
                                }
                                "java" => {
                                    java_snap.set(None);
                                    java_lines.write().clear();
                                }
                                "sqldev" => {
                                    sql_snap.set(None);
                                    sql_dev_lines.write().clear();
                                }
                                "mock" => {
                                    mock_snap.set(None);
                                    mock_lines.write().clear();
                                }
                                _ => {
                                    console_snap.set(None);
                                    props.on_clear.call(());
                                }
                            }
                        },
                        "Clear"
                    }
                }
            }

            // ── Console tab ───────────────────────────────────────────────
            div {
                id: "log-scroll",
                class: "log-tab-scroll",
                style: if tab == "console" { "" } else { "display:none" },
                for line in console_lines.iter() {
                    div { class: "log-line",
                        span { class: "log-time", "{line.time}" }
                        span { class: line.level.css_class(), "{line.msg}" }
                    }
                }
            }

            // ── Azurite tab ───────────────────────────────────────────────
            div {
                id: "az-log-scroll",
                class: "log-tab-scroll",
                style: if tab == "azurite" { "" } else { "display:none" },
                if az_display.is_empty() {
                    div { class: "log-line",
                        span { class: "log-msg info", style: "opacity:0.45;font-style:italic",
                            "Waiting for Azurite debug.log…"
                        }
                    }
                } else {
                    {
                        let (shown, polling, stale, hidden) = process_azurite_lines(&az_display);
                        rsx! {
                            // ── Poll summary banner ────────────────────────
                            if !polling.is_empty() || !stale.is_empty() {
                                div { class: "log-line az-poll-summary",
                                    if !polling.is_empty() {
                                        span { class: "az-poll-icon", "📡" }
                                        span { class: "az-poll-label", "Polling: " }
                                        for (i, q) in polling.iter().enumerate() {
                                            if i > 0 { span { class: "az-poll-sep", ", " } }
                                            span { class: "az-poll-queue", "{q}" }
                                        }
                                    }
                                    if !stale.is_empty() {
                                        span { class: "az-poll-icon", style: "margin-left:.75rem", "⚠" }
                                        span { class: "az-poll-label warn", "Silent: " }
                                        for (i, q) in stale.iter().enumerate() {
                                            if i > 0 { span { class: "az-poll-sep", ", " } }
                                            span { class: "az-poll-queue warn", "{q}" }
                                        }
                                    }
                                    if hidden > 0 {
                                        span { class: "az-poll-hidden", "({hidden} poll cycles hidden)" }
                                    }
                                }
                            }
                            // ── Actual log lines ───────────────────────────
                            for line in shown.iter() {
                                {
                                    let (time, msg) = az_split(line);
                                    let cls = az_line_class(line);
                                    rsx! {
                                        div { class: "log-line",
                                            span { class: "log-time", "{time}" }
                                            span { class: cls, "{msg}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // ── Service Bus tab ───────────────────────────────────────────
            div {
                id: "sb-log-scroll",
                class: "log-tab-scroll",
                style: if tab == "servicebus" { "" } else { "display:none" },
                {
                    let signal_lines: Vec<&String> = sb_emu_display.iter()
                        .filter(|l| !is_sb_emulator_noise(l))
                        .collect();
                    let noise_count = sb_emu_display.iter()
                        .filter(|l| is_sb_emulator_noise(l))
                        .count();

                    rsx! {
                        if sb_emu_display.is_empty() {
                            div { class: "log-line",
                                span { class: "log-msg info", style: "opacity:0.45;font-style:italic",
                                    "SB Emulator not running — start it from the toolbar."
                                }
                            }
                        } else {
                            for line in signal_lines.iter() {
                                {
                                    let (time, msg) = az_split(line);
                                    let cls = az_line_class(line);
                                    rsx! {
                                        div { class: "log-line",
                                            span { class: "log-time", "{time}" }
                                            span { class: cls, "{msg}" }
                                        }
                                    }
                                }
                            }
                            if noise_count > 0 {
                                div { class: "log-line",
                                    span {
                                        class: "log-msg info",
                                        style: "opacity:0.4;font-style:italic",
                                        "{noise_count} internal Windows Fabric / SQL Edge lines hidden (expected on Linux — do not affect AMQP)"
                                    }
                                }
                            }
                        }
                    }
                }
                if !sb_noise_display.is_empty() {
                    div { class: "log-line",
                        span { class: "log-msg warn", style: "opacity:0.6;font-style:italic;margin-top:8px",
                            "── func Service Bus noise ──"
                        }
                    }
                    for line in sb_noise_display.iter() {
                        div { class: "log-line",
                            span { class: "log-time", "{line.time}" }
                            span { class: line.level.css_class(), "{line.msg}" }
                        }
                    }
                }
            }

            // ── SQL Dev tab ───────────────────────────────────────────────
            div {
                id: "sql-log-scroll",
                class: "log-tab-scroll",
                style: if tab == "sqldev" { "" } else { "display:none" },
                if sql_display.is_empty() {
                    div { class: "log-line",
                        span { class: "log-msg info", style: "opacity:0.45;font-style:italic",
                            "No SQL Dev activity yet — use SQL Dev Console in Connectors."
                        }
                    }
                } else {
                    for line in sql_display.iter() {
                        {
                            let cls = if line.contains('✗') || line.contains("error") || line.contains("WARN") {
                                "log-msg error"
                            } else if line.contains('✓') {
                                "log-msg ok"
                            } else if line.contains('↷') {
                                "log-msg warn"
                            } else {
                                "log-msg info"
                            };
                            let (time, msg) = az_split(line);
                            rsx! {
                                div { class: "log-line",
                                    span { class: "log-time", "{time}" }
                                    span { class: cls, "{msg}" }
                                }
                            }
                        }
                    }
                }
            }

            // ── Java Functions tab ────────────────────────────────────────
            div {
                id: "java-log-scroll",
                class: "log-tab-scroll",
                style: if tab == "java" { "" } else { "display:none" },
                if java_display.is_empty() {
                    div { class: "log-line",
                        span { class: "log-msg info", style: "opacity:0.45;font-style:italic",
                            "Java Functions not running — start them from the toolbar."
                        }
                    }
                } else {
                    for line in java_display.iter() {
                        {
                            let cls = if line.contains("ERROR") || line.contains("SEVERE") { "log-msg error" }
                                      else if line.contains("WARN")                         { "log-msg warn"  }
                                      else if line.contains("✅") || line.contains("BUILD SUCCESS") { "log-msg ok" }
                                      else { "log-msg info" };
                            let (time, msg) = az_split(line);
                            rsx! {
                                div { class: "log-line",
                                    span { class: "log-time", "{time}" }
                                    span { class: cls, "{msg}" }
                                }
                            }
                        }
                    }
                }
            }

            // ── Mock tab ─────────────────────────────────────────────────
            // Phase 1 (scan) + Phase 2 (server lifecycle + live events).
            div {
                id: "mock-log-scroll",
                class: "log-tab-scroll",
                style: if tab == "mock" { "" } else { "display:none" },

                // Action bar
                div { class: "log-line", style: "padding-bottom:6px;border-bottom:1px solid #21262d;margin-bottom:6px",
                    button {
                        class: "btn btn-small",
                        style: "margin-right:6px",
                        disabled: mock_busy(),
                        onclick: {
                            let ws = workspace_dir.clone();
                            move |_| {
                                let ws = ws.clone();
                                if ws.trim().is_empty() {
                                    mock_lines.write().push(format!("{}  ⚠ No workspace selected — pick one in the Workspaces tab first.", now_hms()));
                                    return;
                                }
                                mock_busy.set(true);
                                // Match the rest of ais-runner: descend into a `logic_apps/`
                                // subfolder automatically when the user picked a platform root.
                                let resolved = crate::services::workflows::resolve_logic_apps_dir(&ws);
                                mock_lines.write().push(format!("{}  ▶ Scanning {}…", now_hms(), resolved.display()));
                                spawn(async move {
                                    let path = resolved;
                                    let result = tokio::task::spawn_blocking(move || {
                                        crate::services::mock::scan_workspace(&path)
                                    }).await;
                                    match result {
                                        Ok(Ok((contract, cache_path))) => {
                                            mock_status.set(format!(
                                                "Scan: {} endpoints, {} settings, {} warnings → {}",
                                                contract.endpoints.len(), contract.app_settings.len(),
                                                contract.warnings.len(), cache_path.display()
                                            ));
                                            mock_lines.write().push(format!(
                                                "{}  ✓ {} endpoints, {} settings, {} warnings",
                                                now_hms(), contract.endpoints.len(),
                                                contract.app_settings.len(), contract.warnings.len()
                                            ));
                                            for w in contract.warnings.iter().take(50) {
                                                mock_lines.write().push(format!(
                                                    "{}  [{:?}] {}{}{}: {}",
                                                    now_hms(), w.level,
                                                    w.workflow.as_deref().unwrap_or(""),
                                                    if w.action.is_some() { "/" } else { "" },
                                                    w.action.as_deref().unwrap_or(""),
                                                    w.message,
                                                ));
                                            }
                                        }
                                        Ok(Err(e)) => { mock_lines.write().push(format!("{}  ✗ Scan failed: {}", now_hms(), e)); }
                                        Err(e)     => { mock_lines.write().push(format!("{}  ✗ Scan task error: {}", now_hms(), e)); }
                                    }
                                    mock_busy.set(false);
                                });
                            }
                        },
                        "▶ Scan workspace"
                    }
                    span { style: "margin-left:8px;opacity:0.7;font-size:0.85em", "{mock_status}" }
                }

                if mock_display.is_empty() {
                    div { class: "log-line",
                        span { class: "log-msg info", style: "opacity:0.45;font-style:italic",
                            "Click \"▶ Scan workspace\" to discover external HTTP dependencies. Results cached under .ais-cache/."
                        }
                    }
                } else {
                    for line in mock_display.iter() {
                        {
                            let cls = if line.contains("✗") || line.contains("Error") || line.contains("[Error]") { "log-msg error" }
                                      else if line.contains("⚠") || line.contains("[Warn]")                       { "log-msg warn"  }
                                      else if line.contains("✓")                                                  { "log-msg ok"    }
                                      else                                                                         { "log-msg info"  };
                            let (time, msg) = az_split(line);
                            rsx! {
                                div { class: "log-line",
                                    span { class: "log-time", "{time}" }
                                    span { class: cls, "{msg}" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Current local time as `HH:MM:SS` — small helper used by the Mock tab to
/// stamp UI-originated log entries consistently with the rest of the panel.
fn now_hms() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}
