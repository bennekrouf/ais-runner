use dioxus::prelude::*;
use crate::services::workflows::WorkflowItem;

#[derive(Props, Clone, PartialEq)]
pub struct WorkflowListProps {
    pub workflows: Vec<WorkflowItem>,
    pub selected: Option<String>,
    pub on_select: EventHandler<String>,
    pub on_run: EventHandler<(String, String, String)>, // (name, trigger_name, trigger_type)
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
    rsx! {
        div { id: "workflows",
            h2 { "Workflows ({props.workflows.len()})" }
            div { id: "workflow-list",
                if props.workflows.is_empty() {
                    div { class: "empty-state", "No workflows found.\nIs func start running?" }
                }
                for wf in props.workflows.iter() {
                    {
                        let name      = wf.name.clone();
                        let name_run  = name.clone();
                        let name_sel  = name.clone();
                        let trigger   = wf.trigger_name.clone();
                        let ttype     = wf.trigger_type.clone();
                        let is_sel    = props.selected.as_deref() == Some(&name);
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
