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

#[derive(Props, Clone, PartialEq)]
pub struct LogPanelProps {
    pub lines: Vec<LogLine>,
    pub on_clear: EventHandler<()>,
}

#[component]
pub fn LogPanel(props: LogPanelProps) -> Element {
    let mut active_tab = use_signal(|| "console");
    let mut az_lines: Signal<Vec<String>> = use_signal(Vec::new);

    // MutationObserver: auto-scroll both containers on any DOM change.
    use_effect(move || {
        document::eval(
            "(function(){\
                ['log-scroll','az-log-scroll'].forEach(function(id){\
                    var el = document.getElementById(id);\
                    if (!el || el._aisObs) return;\
                    el._aisObs = new MutationObserver(function(){\
                        if (el.style.display !== 'none') el.scrollTop = el.scrollHeight;\
                    });\
                    el._aisObs.observe(el, { childList: true, subtree: true });\
                });\
            })()"
        );
    });

    // tail -f /tmp/azurite/debug.log  (polls every 500 ms)
    use_coroutine(move |_rx: UnboundedReceiver<()>| async move {
        let path = "/tmp/azurite/debug.log";
        let mut offset: u64 = 0;
        loop {
            match tokio::fs::metadata(path).await {
                Ok(meta) => {
                    let len = meta.len();
                    if len < offset {
                        // Azurite restarted — file was truncated/replaced
                        offset = 0;
                        az_lines.write().clear();
                    }
                    if len > offset {
                        if let Ok(mut f) = tokio::fs::File::open(path).await {
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
                                        // keep last 500 lines to avoid unbounded growth
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
                    // File doesn't exist (Azurite not started or location changed)
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

    rsx! {
        div { id: "log-panel",
            div { id: "log-header",
                // Tab buttons
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
                // Spacer + Clear (console only)
                div { style: "flex:1" }
                if tab == "console" {
                    button {
                        class: "btn btn-small",
                        style: "background:#21262d;color:#8b949e",
                        onclick: move |_| props.on_clear.call(()),
                        "Clear"
                    }
                }
            }

            // ── Console tab ──────────────────────────────────────────────
            div {
                id: "log-scroll",
                style: if tab == "console" { "" } else { "display:none" },
                for line in props.lines.iter() {
                    div { class: "log-line",
                        span { class: "log-time", "{line.time}" }
                        span { class: line.level.css_class(), "{line.msg}" }
                    }
                }
            }

            // ── Azurite tab ──────────────────────────────────────────────
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
                                    "Waiting for /tmp/azurite/debug.log…"
                                }
                            }
                        }
                    } else {
                        rsx! {
                            for line in lines.iter() {
                                div { class: "log-line",
                                    span { class: az_line_class(line), "{line}" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
