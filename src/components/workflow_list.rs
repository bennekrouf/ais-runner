use dioxus::prelude::*;
use std::collections::{HashSet, HashMap};
use crate::services::workflows::{WorkflowItem, ConnectorKind};
use crate::components::tooltip::Tooltip;

#[derive(Props, Clone, PartialEq)]
pub struct WorkflowListProps {
    pub workflows:   Vec<WorkflowItem>,
    pub selected:    Option<String>,
    pub traced:      HashSet<String>,
    pub running:     HashSet<String>,
    pub sql_wfs:     HashSet<String>,
    pub msi_wfs:     HashSet<String>,
    pub connectors:  HashMap<String, Vec<ConnectorKind>>,
    pub on_select:   EventHandler<String>,
    pub on_run:      EventHandler<(String, String, String)>,
}

/// Classify a workflow by its trigger type.
#[derive(Clone, Copy, PartialEq, Debug)]
enum TriggerCategory {
    Recurrence,    // Recurrence / Schedule
    Http,          // Request / Http — callback URL
    ServiceBus,    // ServiceProvider → serviceBus  |  ServiceBusTrigger
    Blob,          // ServiceProvider → AzureBlob   |  legacy Blob
    EventGrid,     // ServiceProvider → eventGrid
    CosmosDb,      // ServiceProvider → documentDb
    Sql,           // ServiceProvider → sql
    Sftp,          // ServiceProvider → sftp
    ApiConnection, // Managed connector (Outlook, Teams, SharePoint …)
    Unknown,       // Truly unrecognised type
}

impl TriggerCategory {
    fn from(trigger_type: &str, trigger_provider: Option<&str>) -> Self {
        let t = trigger_type.to_lowercase();
        let p = trigger_provider.unwrap_or("").to_lowercase();
        match t.as_str() {
            "recurrence" | "schedule" => Self::Recurrence,
            "request" | "http"        => Self::Http,
            "blob"                    => Self::Blob,
            "servicebustrigger"       => Self::ServiceBus,
            "apiconnection"           => Self::ApiConnection,
            "serviceprovider" => {
                if      p.contains("servicebus")                     { Self::ServiceBus }
                else if p.contains("azureblob") || p.contains("/blob") { Self::Blob }
                else if p.contains("eventgrid")                      { Self::EventGrid }
                else if p.contains("documentdb") || p.contains("cosmos") { Self::CosmosDb }
                else if p.contains("/sql")                           { Self::Sql }
                else if p.contains("sftp")                           { Self::Sftp }
                else                                                 { Self::Unknown }
            }
            _ => Self::Unknown,
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Recurrence    => "⏱",
            Self::Http          => "🌐",
            Self::ServiceBus    => "📨",
            Self::Blob          => "📦",
            Self::EventGrid     => "⚡",
            Self::CosmosDb      => "🌌",
            Self::Sql           => "🗄",
            Self::Sftp          => "📂",
            Self::ApiConnection => "🔌",
            Self::Unknown       => "❓",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Recurrence    => "Recurrence",
            Self::Http          => "HTTP / Request",
            Self::ServiceBus    => "Service Bus",
            Self::Blob          => "Blob Storage",
            Self::EventGrid     => "Event Grid",
            Self::CosmosDb      => "Cosmos DB",
            Self::Sql           => "SQL",
            Self::Sftp          => "SFTP",
            Self::ApiConnection => "API Connection",
            Self::Unknown       => "Unknown trigger",
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
    EventGrid,
    CosmosDb,
    Sql,
    Sftp,
    ApiConnection,
    Unknown,
    Unhealthy,
}

#[component]
pub fn WorkflowList(props: WorkflowListProps) -> Element {
    let mut filter      = use_signal(|| String::new());
    let mut filter_mode = use_signal(|| FilterMode::All);

    let query = filter.read().to_lowercase();
    let total = props.workflows.len();

    // Pre-compute category for each workflow once (now uses trigger_provider).
    let with_cat: Vec<(&WorkflowItem, TriggerCategory)> = props.workflows.iter()
        .map(|wf| (wf, TriggerCategory::from(&wf.trigger_type, wf.trigger_provider.as_deref())))
        .collect();

    // Counts per filter bucket — only non-zero ones get a filter button.
    let cnt = |cat: TriggerCategory| with_cat.iter().filter(|(wf, c)| wf.healthy && *c == cat).count();
    let count_recurrence    = cnt(TriggerCategory::Recurrence);
    let count_http          = cnt(TriggerCategory::Http);
    let count_sb            = cnt(TriggerCategory::ServiceBus);
    let count_blob          = cnt(TriggerCategory::Blob);
    let count_eventgrid     = cnt(TriggerCategory::EventGrid);
    let count_cosmos        = cnt(TriggerCategory::CosmosDb);
    let count_sql           = cnt(TriggerCategory::Sql);
    let count_sftp          = cnt(TriggerCategory::Sftp);
    let count_api           = cnt(TriggerCategory::ApiConnection);
    let count_unknown       = cnt(TriggerCategory::Unknown);
    let count_unhealthy     = props.workflows.iter().filter(|wf| !wf.healthy).count();

    let mode = *filter_mode.read();

    let visible: Vec<_> = with_cat.iter()
        .filter(|(wf, cat)| {
            if !query.is_empty() && !wf.name.to_lowercase().contains(&query) {
                return false;
            }
            match mode {
                FilterMode::All           => true,
                FilterMode::Recurrence    => wf.healthy && *cat == TriggerCategory::Recurrence,
                FilterMode::Http          => wf.healthy && *cat == TriggerCategory::Http,
                FilterMode::ServiceBus    => wf.healthy && *cat == TriggerCategory::ServiceBus,
                FilterMode::Blob          => wf.healthy && *cat == TriggerCategory::Blob,
                FilterMode::EventGrid     => wf.healthy && *cat == TriggerCategory::EventGrid,
                FilterMode::CosmosDb      => wf.healthy && *cat == TriggerCategory::CosmosDb,
                FilterMode::Sql           => wf.healthy && *cat == TriggerCategory::Sql,
                FilterMode::Sftp          => wf.healthy && *cat == TriggerCategory::Sftp,
                FilterMode::ApiConnection => wf.healthy && *cat == TriggerCategory::ApiConnection,
                FilterMode::Unknown       => wf.healthy && *cat == TriggerCategory::Unknown,
                FilterMode::Unhealthy     => !wf.healthy,
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
                        // One button per category — only shown when count > 0.
                        for (fmode, icon, label, count) in [
                            (FilterMode::Recurrence,    "⏱", "Recurrence",      count_recurrence),
                            (FilterMode::Http,          "🌐", "HTTP / Request",  count_http),
                            (FilterMode::ServiceBus,    "📨", "Service Bus",     count_sb),
                            (FilterMode::Blob,          "📦", "Blob Storage",    count_blob),
                            (FilterMode::EventGrid,     "⚡", "Event Grid",      count_eventgrid),
                            (FilterMode::CosmosDb,      "🌌", "Cosmos DB",       count_cosmos),
                            (FilterMode::Sql,           "🗄", "SQL",             count_sql),
                            (FilterMode::Sftp,          "📂", "SFTP",            count_sftp),
                            (FilterMode::ApiConnection, "🔌", "API Connection",  count_api),
                            (FilterMode::Unknown,       "❓", "Unknown trigger", count_unknown),
                        ] {
                            if count > 0 {
                                Tooltip { text: "{label} ({count})", direction: "bottom",
                                    button {
                                        class: if mode == fmode { "btn-filter active" } else { "btn-filter" },
                                        onclick: move |_| filter_mode.set(fmode),
                                        "{icon}"
                                    }
                                }
                            }
                        }
                        if count_unhealthy > 0 {
                            Tooltip { text: "Unhealthy / broken ({count_unhealthy})", direction: "bottom",
                                button {
                                    class: if mode == FilterMode::Unhealthy { "btn-filter active" } else { "btn-filter" },
                                    onclick: move |_| filter_mode.set(FilterMode::Unhealthy),
                                    "🔴"
                                }
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
                        let has_msi    = props.msi_wfs.contains(&name);
                        let health_cls = if wf.healthy { "wf-dot healthy" } else { "wf-dot unhealthy" };
                        let icon       = cat.icon();
                        let icon_title = cat.label();
                        let disabled   = wf.disabled;
                        let runnable   = wf.healthy && !wf.disabled;
                        let run_title  = if !wf.healthy { "Unhealthy — connection reference broken" }
                                         else if wf.disabled { "Workflow is disabled" }
                                         else { "Run workflow" };
                        let conn_icons: Vec<ConnectorKind> = props.connectors
                            .get(&name)
                            .cloned()
                            .unwrap_or_default();
                        rsx! {
                            div {
                                class: if is_sel { "workflow-item selected" } else { "workflow-item" },
                                onclick: move |_| props.on_select.call(name_sel.clone()),

                                span { class: health_cls }
                                span { class: "wf-trigger-icon", title: "{icon_title}", "{icon}" }

                                // ── connector strip ───────────────────────
                                if !conn_icons.is_empty() {
                                    span { class: "wf-connectors",
                                        for kind in conn_icons.iter() {
                                            Tooltip {
                                                text: kind.label().to_string(),
                                                direction: "right",
                                                span { class: "wf-conn-icon", "{kind.icon()}" }
                                            }
                                        }
                                    }
                                }

                                span {
                                    class: if disabled { "workflow-name disabled" } else { "workflow-name" },
                                    "{name}"
                                }
                                if has_sql {
                                    span { class: "wf-sql-icon", title: "Uses SQL connection" }
                                }
                                if has_msi {
                                    span {
                                        class: "wf-msi-warn",
                                        title: "Trigger uses Managed Identity auth against Azurite — \
                                                trigger will never fire locally. \
                                                Fix: change connections.json to parameterSetName: connectionString \
                                                and add <Name>_connectionString = UseDevelopmentStorage=true",
                                        "⚠ MSI"
                                    }
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
