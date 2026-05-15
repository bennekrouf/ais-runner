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

pub fn is_sb_noise(msg: &str) -> bool {
    msg.contains("An unhandled exception occurred in the message batch receive loop") ||
    msg.contains("aka.ms/azsdk/net/servicebus/exceptions/troubleshoot") ||
    msg.contains("serviceBus.ServiceBusServiceOperationsProvider") ||
    // Only filter ServiceProvider job errors that are specifically about Service Bus —
    // the same patterns fire for blob and other service provider triggers/actions too.
    (msg.contains("ServiceProviderRecurrenceTriggerJob") && msg.contains("serviceBus")) ||
    (msg.contains("ServiceProviderActionJob") && msg.contains("serviceBus")) ||
    msg.contains("onNewMessagesFromQueueSession") ||
    (msg.contains("Outgoing HTTP request ends with server failure") && msg.contains("hostName='serviceBus'"))
}

/// Split an Azurite debug.log line into (time, rest).
/// Lines look like: `2024-01-01T12:00:00.123Z [Queue]  message…`
/// We show only the HH:MM:SS part to match the Console time style.
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

    use_effect(move || {
        let _n = props.lines.read().len();
        document::eval("var e=document.getElementById('log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
    });
    use_effect(move || {
        let _n = az_lines.read().len();
        document::eval("var e=document.getElementById('az-log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
    });
    use_effect(move || {
        let _n = sb_emu_lines.read().len();
        document::eval("var e=document.getElementById('sb-log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
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
    let sb_noise_count = props.lines.read().iter().filter(|l| is_sb_noise(&l.msg)).count();
    let sb_emu_count   = sb_emu_lines.read().len();
    let sb_count       = sb_noise_count + sb_emu_count;

    rsx! {
        div { id: "log-panel",
            div { id: "log-header",
                button {
                    class: if tab == "console" { "log-tab active" } else { "log-tab" },
                    onclick: move |_| {
                        active_tab.set("console");
                        document::eval("var e=document.getElementById('log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
                    },
                    "Console"
                }
                button {
                    class: if tab == "azurite" { "log-tab active" } else { "log-tab" },
                    onclick: move |_| {
                        active_tab.set("azurite");
                        document::eval("var e=document.getElementById('az-log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
                    },
                    "Azurite"
                }
                button {
                    class: if tab == "servicebus" { "log-tab active" } else { "log-tab" },
                    onclick: move |_| {
                        active_tab.set("servicebus");
                        document::eval("var e=document.getElementById('sb-log-scroll'); if(e) e.scrollTop=e.scrollHeight;");
                    },
                    if tab != "servicebus" && sb_count > 0 {
                        "Service Bus ({sb_count})"
                    } else {
                        "Service Bus"
                    }
                }
                div { style: "flex:1" }
                button {
                    class: "btn btn-small",
                    style: "background:#21262d;color:#8b949e",
                    onclick: move |_| {
                        if tab == "azurite" {
                            az_lines.write().clear();
                        } else if tab == "servicebus" {
                            sb_emu_lines.write().clear();
                            props.on_clear.call(());
                        } else {
                            props.on_clear.call(());
                        }
                    },
                    "Clear"
                }
            }

            // ── Console tab (SB noise excluded) ──────────────────────────
            div {
                id: "log-scroll",
                style: if tab == "console" { "" } else { "display:none" },
                for line in props.lines.read().iter().filter(|l| !is_sb_noise(&l.msg) && !is_stack_frame_noise(&l.msg)) {
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
                {
                    let lines = az_lines.read();
                    if lines.is_empty() {
                        rsx! {
                            div { class: "log-line",
                                span { class: "log-msg info",
                                    style: "opacity:0.45;font-style:italic",
                                    "Waiting for Azurite debug.log…"
                                }
                            }
                        }
                    } else {
                        rsx! {
                            for line in lines.iter() {
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

            // ── Service Bus tab — emulator docker output + func SB noise ─
            div {
                id: "sb-log-scroll",
                style: if tab == "servicebus" { "" } else { "display:none" },
                // Docker container stdout/stderr
                {
                    let emu = sb_emu_lines.read();
                    if emu.is_empty() {
                        rsx! {
                            div { class: "log-line",
                                span { class: "log-msg info",
                                    style: "opacity:0.45;font-style:italic",
                                    "SB Emulator not running — start it from the toolbar."
                                }
                            }
                        }
                    } else {
                        rsx! {
                            for line in emu.iter() {
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
                // Func SB noise (connection errors, retries etc.)
                {
                    let noise: Vec<_> = props.lines.read().iter()
                        .filter(|l| is_sb_noise(&l.msg))
                        .cloned()
                        .collect();
                    if !noise.is_empty() {
                        rsx! {
                            div { class: "log-line",
                                span { class: "log-msg warn",
                                    style: "opacity:0.6;font-style:italic;margin-top:8px",
                                    "── func Service Bus noise ──"
                                }
                            }
                            for line in noise.iter() {
                                div { class: "log-line",
                                    span { class: "log-time", "{line.time}" }
                                    span { class: line.level.css_class(), "{line.msg}" }
                                }
                            }
                        }
                    } else {
                        rsx! {}
                    }
                }
            }
        }
    }
}
