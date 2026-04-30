use dioxus::prelude::*;
use std::collections::HashSet;
use crate::services::workflows::WorkflowItem;

#[derive(Props, Clone, PartialEq)]
pub struct WorkflowListProps {
    pub workflows: Vec<WorkflowItem>,
    pub selected:  Option<String>,
    pub traced:    HashSet<String>,
    pub on_select: EventHandler<String>,
    pub on_run:    EventHandler<(String, String, String)>, // (name, trigger_name, trigger_type)
}

fn trigger_icon(trigger: &str) -> &'static str {
    match trigger.to_lowercase().as_str() {
        "recurrence"         => "⏱",
        "request" | "http"   => "🌐",
        "manual"             => "▶",
        "eventgridtrigger"   => "⚡",
        "blob"               => "📦",
        "servicebustrigger"  => "📨",
        _                    => "◆",
    }
}

#[component]
pub fn WorkflowList(props: WorkflowListProps) -> Element {
    let mut filter = use_signal(|| String::new());

    let query = filter.read().to_lowercase();
    let visible: Vec<_> = props.workflows.iter()
        .filter(|wf| query.is_empty() || wf.name.to_lowercase().contains(&query))
        .collect();

    rsx! {
        div { id: "workflows",
            div { id: "wf-header",
                h2 { "Workflows ({props.workflows.len()})" }
                input {
                    id: "wf-filter",
                    placeholder: "Filter…",
                    value: "{filter}",
                    oninput: move |e| filter.set(e.value()),
                }
            }
            div { id: "workflow-list",
                if props.workflows.is_empty() {
                    div { class: "empty-state", "No workflows found.\nIs func start running?" }
                } else if visible.is_empty() {
                    div { class: "empty-state", "No match for "{query}"" }
                }
                for wf in visible.iter() {
                    {
                        let name      = wf.name.clone();
                        let name_run  = name.clone();
                        let name_sel  = name.clone();
                        let trigger   = wf.trigger_name.clone();
                        let ttype     = wf.trigger_type.clone();
                        let is_sel    = props.selected.as_deref() == Some(&name);
                        let has_trace = props.traced.contains(&name);
                        let health_cls = if wf.healthy { "wf-dot healthy" } else { "wf-dot unhealthy" };
                        let icon      = trigger_icon(&wf.trigger_type);
                        let disabled  = wf.disabled;
                        let runnable  = wf.healthy && !wf.disabled;
                        let run_title = if !wf.healthy { "Unhealthy — connection reference broken" }
                                        else if wf.disabled { "Workflow is disabled" }
                                        else { "Run workflow" };
                        rsx! {
                            div {
                                class: if is_sel { "workflow-item selected" } else { "workflow-item" },
                                onclick: move |_| props.on_select.call(name_sel.clone()),

                                span { class: health_cls }
                                span { class: "wf-trigger-icon", title: "{wf.trigger_type}", "{icon}" }
                                span {
                                    class: if disabled { "workflow-name disabled" } else { "workflow-name" },
                                    "{name}"
                                }
                                if has_trace {
                                    span { class: "wf-trace-dot", title: "Has run history — click to view" }
                                }
                                button {
                                    class: "btn btn-run btn-small",
                                    disabled: !runnable,
                                    title: "{run_title}",
                                    onclick: move |e| {
                                        e.stop_propagation();
                                        props.on_run.call((name_run.clone(), trigger.clone(), ttype.clone()));
                                    },
                                    "▶"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
