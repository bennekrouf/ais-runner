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

/// Classify a workflow into one of four Azure trigger categories.
#[derive(Clone, Copy, PartialEq, Debug)]
enum TriggerCategory {
    Recurrence,  // Recurrence — schedulable, run on-demand via /run API
    Http,        // Request / Http — triggerable via callback URL
    ServiceBus,  // ServiceProvider → /serviceProviders/serviceBus
    Blob,        // ServiceProvider → /serviceProviders/AzureBlob (or legacy Blob)
    Other,       // EventGrid, ApiConnection, unknown
}

impl TriggerCategory {
    fn from(trigger_type: &str, trigger_provider: Option<&str>) -> Self {
        let t = trigger_type.to_lowercase();
        let p = trigger_provider.unwrap_or("").to_lowercase();
        match t.as_str() {
            "recurrence"       => Self::Recurrence,
            "request" | "http" => Self::Http,
            "blob"             => Self::Blob,
            "servicebustrigger" => Self::ServiceBus,
            "serviceprovider"  => {
                if p.contains("servicebus") { Self::ServiceBus }
                else if p.contains("blob")  { Self::Blob }
                else                         { Self::Other }
            }
            _ => Self::Other,
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Recurrence => "⏱",
            Self::Http       => "🌐",
            Self::ServiceBus => "📨",
            Self::Blob       => "📦",
            Self::Other      => "◆",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Recurrence => "Recurrence",
            Self::Http       => "HTTP / Request",
            Self::ServiceBus => "Service Bus",
            Self::Blob       => "Blob Storage",
            Self::Other      => "Other",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum FilterMode {
    All,
    Recurrence,
    Http,
    ServiceBus,
    Blob,
    Unhealthy,
}

#[component]
pub fn WorkflowList(props: WorkflowListProps) -> Element {
    let mut filter      = use_signal(|| String::new());
    let mut filter_mode = use_signal(|| FilterMode::All);

    let query = filter.read().to_lowercase();
    let total = props.workflows.len();

    // Pre-compute category for each workflow once.
    let with_cat: Vec<(&WorkflowItem, TriggerCategory)> = props.workflows.iter()
        .map(|wf| (wf, TriggerCategory::from(&wf.trigger_type, None)))
        .collect();

    // Counts per filter bucket.
    let count_recurrence = with_cat.iter().filter(|(wf, cat)| wf.healthy && *cat == TriggerCategory::Recurrence).count();
    let count_http       = with_cat.iter().filter(|(wf, cat)| wf.healthy && *cat == TriggerCategory::Http).count();
    let count_sb         = with_cat.iter().filter(|(wf, cat)| wf.healthy && *cat == TriggerCategory::ServiceBus).count();
    let count_blob       = with_cat.iter().filter(|(wf, cat)| wf.healthy && *cat == TriggerCategory::Blob).count();
    let count_unhealthy  = props.workflows.iter().filter(|wf| !wf.healthy).count();

    let mode = *filter_mode.read();

    let visible: Vec<_> = with_cat.iter()
        .filter(|(wf, cat)| {
            if !query.is_empty() && !wf.name.to_lowercase().contains(&query) {
                return false;
            }
            match mode {
                FilterMode::All        => true,
                FilterMode::Recurrence => wf.healthy && *cat == TriggerCategory::Recurrence,
                FilterMode::Http       => wf.healthy && *cat == TriggerCategory::Http,
                FilterMode::ServiceBus => wf.healthy && *cat == TriggerCategory::ServiceBus,
                FilterMode::Blob       => wf.healthy && *cat == TriggerCategory::Blob,
                FilterMode::Unhealthy  => !wf.healthy,
            }
        })
        .collect();

    let shown = visible.len();

    rsx! {
        div { id: "workflows",
            div { id: "wf-header",
                div { id: "wf-title-row",
                    h2 {
                        if mode == FilterMode::All { "Workflows ({total})" }
                        else { "Workflows ({shown}/{total})" }
                    }
                    div { class: "wf-filter-group",
                        Tooltip { text: "Recurrence ({count_recurrence})", direction: "bottom",
                            button {
                                class: if mode == FilterMode::Recurrence { "btn-filter active" } else { "btn-filter" },
                                onclick: move |_| filter_mode.set(FilterMode::Recurrence),
                                "⏱"
                            }
                        }
                        Tooltip { text: "HTTP / Request ({count_http})", direction: "bottom",
                            button {
                                class: if mode == FilterMode::Http { "btn-filter active" } else { "btn-filter" },
                                onclick: move |_| filter_mode.set(FilterMode::Http),
                                "🌐"
                            }
                        }
                        Tooltip { text: "Service Bus ({count_sb})", direction: "bottom",
                            button {
                                class: if mode == FilterMode::ServiceBus { "btn-filter active" } else { "btn-filter" },
                                onclick: move |_| filter_mode.set(FilterMode::ServiceBus),
                                "📨"
                            }
                        }
                        Tooltip { text: "Blob Storage ({count_blob})", direction: "bottom",
                            button {
                                class: if mode == FilterMode::Blob { "btn-filter active" } else { "btn-filter" },
                                onclick: move |_| filter_mode.set(FilterMode::Blob),
                                "📦"
                            }
                        }
                        Tooltip { text: "Unhealthy / broken ({count_unhealthy})", direction: "bottom",
                            button {
                                class: if mode == FilterMode::Unhealthy { "btn-filter active" } else { "btn-filter" },
                                onclick: move |_| filter_mode.set(FilterMode::Unhealthy),
                                "🔴"
                            }
                        }
                        Tooltip { text: "All ({total})", direction: "bottom",
                            button {
                                class: if mode == FilterMode::All { "btn-filter active" } else { "btn-filter" },
                                onclick: move |_| filter_mode.set(FilterMode::All),
                                "·"
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
                    div { class: "empty-state", "No match" }
                }
                for (wf, cat) in visible.iter() {
                    {
                        let name       = wf.name.clone();
                        let name_run   = name.clone();
                        let name_sel   = name.clone();
                        let trigger    = wf.trigger_name.clone();
                        let ttype      = wf.trigger_type.clone();
                        let is_sel     = props.selected.as_deref() == Some(&name);
                        let has_trace  = props.traced.contains(&name);
                        let is_running = props.running.contains(&name);
                        let has_sql    = props.sql_wfs.contains(&name);
                        let health_cls = if wf.healthy { "wf-dot healthy" } else { "wf-dot unhealthy" };
                        let icon       = cat.icon();
                        let icon_title = cat.label();
                        let disabled   = wf.disabled;
                        let runnable   = wf.healthy && !wf.disabled;
                        let run_title  = if !wf.healthy { "Unhealthy — connection reference broken" }
                                         else if wf.disabled { "Workflow is disabled" }
                                         else { "Run workflow" };
                        rsx! {
                            div {
                                class: if is_sel { "workflow-item selected" } else { "workflow-item" },
                                onclick: move |_| props.on_select.call(name_sel.clone()),

                                span { class: health_cls }
                                span { class: "wf-trigger-icon", title: "{icon_title}", "{icon}" }
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
