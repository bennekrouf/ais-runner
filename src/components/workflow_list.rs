use dioxus::prelude::*;
use std::collections::HashSet;
use crate::services::workflows::WorkflowItem;
use crate::components::tooltip::Tooltip;

#[derive(Props, Clone, PartialEq)]
pub struct WorkflowListProps {
    pub workflows:  Vec<WorkflowItem>,
    pub selected:   Option<String>,
    pub traced:     HashSet<String>,
    pub running:    HashSet<String>,
    pub sql_wfs:    HashSet<String>,
    pub on_select:  EventHandler<String>,
    pub on_run:     EventHandler<(String, String, String)>,
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

#[derive(Clone, Copy, PartialEq)]
enum FilterMode {
    Healthy,   // 💻 (Runnable locally)
    Unhealthy, // 🔴 (Connection issues)
    Azure,     // ☁️ (Passive triggers like Event Grid)
    All,       // 🌐 (Show everything)
}

#[component]
pub fn WorkflowList(props: WorkflowListProps) -> Element {
    let mut filter      = use_signal(|| String::new());
    let mut filter_mode = use_signal(|| FilterMode::Healthy);

    let query = filter.read().to_lowercase();
    let total = props.workflows.len();

    let is_azure_type = |ttype: &str| -> bool {
        let t = ttype.to_lowercase();
        t.contains("eventgrid") || t.contains("servicebus") || t.contains("blob") || t.contains("serviceprovider") || t.contains("apiconnection")
    };

    let visible: Vec<_> = props.workflows.iter()
        .filter(|wf| {
            if !query.is_empty() && !wf.name.to_lowercase().contains(&query) {
                return false;
            }
            match *filter_mode.read() {
                FilterMode::Healthy => wf.healthy && !wf.disabled && !is_azure_type(&wf.trigger_type),
                FilterMode::Unhealthy => !wf.healthy,
                FilterMode::Azure => wf.healthy && is_azure_type(&wf.trigger_type),
                FilterMode::All => true,
            }
        })
        .collect();

    let count_healthy = props.workflows.iter().filter(|wf| wf.healthy && !wf.disabled && !is_azure_type(&wf.trigger_type)).count();
    let count_unhealthy = props.workflows.iter().filter(|wf| !wf.healthy).count();
    let count_azure = props.workflows.iter().filter(|wf| wf.healthy && is_azure_type(&wf.trigger_type)).count();

    let (current_count, show_total) = match *filter_mode.read() {
        FilterMode::Healthy   => (count_healthy, true),
        FilterMode::Unhealthy => (count_unhealthy, true),
        FilterMode::Azure     => (count_azure, true),
        FilterMode::All       => (total, false),
    };

    rsx! {
        div { id: "workflows",
            div { id: "wf-header",
                div { id: "wf-title-row",
                    h2 {
                        if show_total { "Workflows ({current_count}/{total})" }
                        else { "Workflows ({total})" }
                    }
                    div { class: "wf-filter-group",
                        Tooltip { text: "Local / Healthy ({count_healthy})", direction: "bottom",
                            button {
                                class: if *filter_mode.read() == FilterMode::Healthy { "btn-filter active" } else { "btn-filter" },
                                onclick: move |_| filter_mode.set(FilterMode::Healthy),
                                "💻"
                            }
                        }
                        Tooltip { text: "Unhealthy / Errors ({count_unhealthy})", direction: "bottom",
                            button {
                                class: if *filter_mode.read() == FilterMode::Unhealthy { "btn-filter active" } else { "btn-filter" },
                                onclick: move |_| filter_mode.set(FilterMode::Unhealthy),
                                "🔴"
                            }
                        }
                        Tooltip { text: "Azure Only / Passive ({count_azure})", direction: "bottom",
                            button {
                                class: if *filter_mode.read() == FilterMode::Azure { "btn-filter active" } else { "btn-filter" },
                                onclick: move |_| filter_mode.set(FilterMode::Azure),
                                "☁️"
                            }
                        }
                        Tooltip { text: "Show All ({total})", direction: "bottom",
                            button {
                                class: if *filter_mode.read() == FilterMode::All { "btn-filter active" } else { "btn-filter" },
                                onclick: move |_| filter_mode.set(FilterMode::All),
                                "🌐"
                            }
                        }
                    }
                }
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
                        let is_running = props.running.contains(&name);
                        let has_sql    = props.sql_wfs.contains(&name);
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
                                if has_sql {
                                    span { class: "wf-sql-icon", title: "Uses SQL connection" }
                                }
                                if is_running {
                                    span { class: "wf-spinner", title: "Running…" }
                                } else if has_trace {
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
