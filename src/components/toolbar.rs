use dioxus::prelude::*;
use crate::services::process::ServiceState;

#[derive(Props, Clone, PartialEq)]
pub struct ServiceBlockProps {
    pub label: String,
    pub cmd: String,
    pub state: ServiceState,
    pub on_start: EventHandler<()>,
    pub on_stop: EventHandler<()>,
}

#[component]
pub fn ServiceBlock(props: ServiceBlockProps) -> Element {
    let dot_class = match props.state {
        ServiceState::Running  => "dot running",
        ServiceState::Starting => "dot starting",
        ServiceState::Stopped  => "dot stopped",
    };

    let (btn_label, btn_class, is_start) = match props.state {
        ServiceState::Stopped  => ("Start", "btn btn-start", true),
        ServiceState::Starting => ("Starting…", "btn btn-start", false),
        ServiceState::Running  => ("Stop",  "btn btn-stop",  false),
    };

    rsx! {
        div { class: "service-block",
            div { class: dot_class }
            span { class: "service-label", "{props.label}" }
            code { class: "service-cmd", "{props.cmd}" }
            button {
                class: btn_class,
                disabled: matches!(props.state, ServiceState::Starting),
                onclick: move |_| {
                    if is_start {
                        props.on_start.call(());
                    } else {
                        props.on_stop.call(());
                    }
                },
                "{btn_label}"
            }
        }
    }
}
