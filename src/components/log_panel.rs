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

#[derive(Props, Clone, PartialEq)]
pub struct LogPanelProps {
    pub lines: Vec<LogLine>,
    pub on_clear: EventHandler<()>,
}

#[component]
pub fn LogPanel(props: LogPanelProps) -> Element {
    // Register a MutationObserver once on mount — it auto-scrolls on every DOM change.
    use_effect(move || {
        document::eval(
            "(function(){\
                var el = document.getElementById('log-scroll');\
                if (!el || el._aisObs) return;\
                el._aisObs = new MutationObserver(function(){ el.scrollTop = el.scrollHeight; });\
                el._aisObs.observe(el, { childList: true, subtree: true });\
                el.scrollTop = el.scrollHeight;\
            })()"
        );
    });

    rsx! {
        div { id: "log-panel", style: "height:200px",
            div { id: "log-header",
                span { "Console" }
                button {
                    class: "btn btn-small",
                    style: "background:#21262d;color:#8b949e",
                    onclick: move |_| props.on_clear.call(()),
                    "Clear"
                }
            }
            div { id: "log-scroll",
                for line in props.lines.iter() {
                    div { class: "log-line",
                        span { class: "log-time", "{line.time}" }
                        span { class: line.level.css_class(), "{line.msg}" }
                    }
                }
            }
        }
    }
}
