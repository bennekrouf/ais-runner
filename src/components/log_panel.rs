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
    pub sb_emu_lines: Signal<Vec<String>>,
    pub on_clear:     EventHandler<()>,
}

#[component]
pub fn LogPanel(props: LogPanelProps) -> Element {
    let mut active_tab = use_signal(|| "console");
    let mut az_lines: Signal<Vec<String>> = use_signal(Vec::new);
    let mut sb_emu_lines = props.sb_emu_lines;

    // ── Pause snapshots — None = live, Some(snapshot) = paused ──────────────
    let mut console_snap: Signal<Option<Vec<LogLine>>>    = use_signal(|| None);
    let mut az_snap:      Signal<Option<Vec<String>>>     = use_signal(|| None);
    let mut sb_snap:      Signal<Option<(Vec<String>, Vec<LogLine>)>> = use_signal(|| None);

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

    // tail -f azurite debug.log  (polls every 500 ms)
    use_coroutine(move |_rx: UnboundedReceiver<()>| async move {
        let path = crate::utils::azurite_log();
        let mut offset: u64 = 0;
        loop {
            match tokio::fs::metadata(&path).await {
                Ok(meta) => {
                    let len = meta.len();
                    if len < offset {
                        offset = 0;
                        az_lines.write().clear();
                    }
                    if len > offset {
                        if let Ok(mut f) = tokio::fs::File::open(&path).await {
                            use tokio::io::{AsyncReadExt, AsyncSeekExt};
                            if f.seek(std::io::SeekFrom::Start(offset)).await.is_ok() {
                                let mut buf = String::new();
                                if f.read_to_string(&mut buf).await.is_ok() {
                                    offset = len;
                                    let new: Vec<String> = buf
                                        .lines()
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
                    }
                }
                Err(_) => {
                    if offset > 0 {
                        offset = 0;
                        az_lines.write().clear();
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    });

    let tab = active_tab();

    // ── New-line counts while paused ─────────────────────────────────────────
    let console_new = console_snap.read().as_ref().map(|snap| {
        let live: Vec<_> = props.lines.read().iter()
            .filter(|l| !is_sb_noise(&l.msg) && !is_stack_frame_noise(&l.msg))
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

    // ── Rendered lines ───────────────────────────────────────────────────────
    let console_lines: Vec<LogLine> = match console_snap.read().clone() {
        Some(snap) => snap,
        None => props.lines.read().iter()
            .filter(|l| !is_sb_noise(&l.msg) && !is_stack_frame_noise(&l.msg))
            .cloned().collect(),
    };
    let az_display: Vec<String> = match az_snap.read().clone() {
        Some(snap) => snap,
        None => az_lines.read().clone(),
    };
    let (sb_emu_display, sb_noise_display): (Vec<String>, Vec<LogLine>) =
        match sb_snap.read().clone() {
            Some(snap) => snap,
            None => (
                sb_emu_lines.read().clone(),
                props.lines.read().iter().filter(|l| is_sb_noise(&l.msg)).cloned().collect(),
            ),
        };

    rsx! {
        div { id: "log-panel",
            div { id: "log-header",
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
                    if tab != "servicebus" && sb_count > 0 {
                        "Service Bus ({sb_count})"
                    } else {
                        "Service Bus"
                    }
                    if sb_snap.read().is_some() { span { class: "log-paused-badge", "⏸" } }
                }

                div { style: "flex:1" }

                // ── Pause / resume button ─────────────────────────────────
                {
                    let (paused, new_count) = match tab {
                        "azurite"    => (az_snap.read().is_some(),      az_new),
                        "servicebus" => (sb_snap.read().is_some(),      sb_new),
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
                                    _ => {
                                        if console_snap.read().is_some() {
                                            console_snap.set(None);
                                            document::eval("var e=document.getElementById('log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
                                        } else {
                                            let snap = props.lines.read().iter()
                                                .filter(|l| !is_sb_noise(&l.msg) && !is_stack_frame_noise(&l.msg))
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

                button {
                    class: "btn btn-small",
                    style: "background:#21262d;color:#8b949e",
                    onclick: move |_| {
                        // Clear also resets the pause for that tab
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
                            _ => {
                                console_snap.set(None);
                                props.on_clear.call(());
                            }
                        }
                    },
                    "Clear"
                }
            }

            // ── Console tab ───────────────────────────────────────────────
            div {
                id: "log-scroll",
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
                style: if tab == "azurite" { "" } else { "display:none" },
                if az_display.is_empty() {
                    div { class: "log-line",
                        span { class: "log-msg info", style: "opacity:0.45;font-style:italic",
                            "Waiting for Azurite debug.log…"
                        }
                    }
                } else {
                    for line in az_display.iter() {
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

            // ── Service Bus tab ───────────────────────────────────────────
            div {
                id: "sb-log-scroll",
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
        }
    }
}
