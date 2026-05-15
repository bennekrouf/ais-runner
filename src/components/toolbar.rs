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

    let (block_state_class, tooltip, is_start) = match props.state {
        ServiceState::Stopped  => ("svc-stopped",  format!("Click to start {} — {}", props.label, props.cmd), true),
        ServiceState::Starting => ("svc-starting", format!("{} is starting…", props.label), false),
        ServiceState::Running  => ("svc-running",  format!("Click to stop {}", props.label), false),
    };

    let disabled = matches!(props.state, ServiceState::Starting);

    rsx! {
        div {
            class: "service-block service-block-btn {block_state_class}",
            title: "{tooltip}",
            role: "button",
            onclick: move |_| {
                if disabled { return; }
                if is_start { props.on_start.call(()); } else { props.on_stop.call(()); }
            },
            div { class: dot_class }
            span { class: "service-label", "{props.label}" }
        }
    }
}
