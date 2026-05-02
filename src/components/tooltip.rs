use dioxus::prelude::*;

#[derive(Props, Clone, PartialEq)]
pub struct TooltipProps {
    pub text: String,
    pub children: Element,
    #[props(default = "top".to_string())]
    pub direction: String,
}

#[component]
pub fn Tooltip(props: TooltipProps) -> Element {
    let class = format!("tooltip-text {}", props.direction);
    rsx! {
        div { class: "tooltip-container",
            {props.children}
            span { class: "{class}", "{props.text}" }
        }
    }
}
