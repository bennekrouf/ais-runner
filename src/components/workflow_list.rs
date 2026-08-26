use crate::components::tooltip::Tooltip;
use crate::services::workflows::{ConnectorKind, WorkflowItem};
use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};

#[derive(Props, Clone, PartialEq)]
pub struct WorkflowListProps {
    pub workflows: Vec<WorkflowItem>,
    pub selected: Option<String>,
    pub traced: HashSet<String>,
    pub running: HashSet<String>,
    pub sql_wfs: HashSet<String>,
    pub msi_wfs: HashSet<String>,
    pub connectors: HashMap<String, Vec<ConnectorKind>>,
    pub last_ran: HashMap<String, u64>,
    pub on_select: EventHandler<String>,
    pub on_run: EventHandler<(String, String, String)>,
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
            "request" | "http" => Self::Http,
            "blob" => Self::Blob,
            "servicebustrigger" => Self::ServiceBus,
            "apiconnection" => Self::ApiConnection,
            "serviceprovider" => {
                if p.contains("servicebus") {
                    Self::ServiceBus
                } else if p.contains("azureblob") || p.contains("/blob") {
                    Self::Blob
                } else if p.contains("eventgrid") {
                    Self::EventGrid
                } else if p.contains("documentdb") || p.contains("cosmos") {
                    Self::CosmosDb
                } else if p.contains("/sql") {
                    Self::Sql
                } else if p.contains("sftp") {
                    Self::Sftp
                } else {
                    Self::Unknown
                }
            }
            _ => Self::Unknown,
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Recurrence => "⏱",
            Self::Http => "🌐",
            Self::ServiceBus => "📨",
            Self::Blob => "📦",
            Self::EventGrid => "⚡",
            Self::CosmosDb => "🌌",
            Self::Sql => "🗄",
            Self::Sftp => "📂",
            Self::ApiConnection => "🔌",
            Self::Unknown => "❓",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Recurrence => "Recurrence",
            Self::Http => "HTTP / Request",
            Self::ServiceBus => "Service Bus",
            Self::Blob => "Blob Storage",
            Self::EventGrid => "Event Grid",
            Self::CosmosDb => "Cosmos DB",
            Self::Sql => "SQL",
            Self::Sftp => "SFTP",
            Self::ApiConnection => "API Connection",
            Self::Unknown => "Unknown trigger",
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

/// Does `name` match the filter box? `terms` are the already-lowercased,
/// whitespace-split search words, OR'd together — typing "check kyriba" lists
/// workflows matching either word. An empty term list matches everything, so a
/// blank or whitespace-only box never blanks the list.
fn matches_filter(name: &str, terms: &[&str]) -> bool {
    if terms.is_empty() {
        return true;
    }
    let name = name.to_lowercase();
    terms.iter().any(|t| name.contains(t))
}

#[component]
pub fn WorkflowList(props: WorkflowListProps) -> Element {
    let mut filter = use_signal(|| String::new());
    let mut filter_mode = use_signal(|| FilterMode::All);

    let query = filter.read().to_lowercase();
    // Space-separated terms are OR'd, so "check kyriba" lists every workflow
    // matching *either* word. Collected once rather than per-row, and via
    // split_whitespace so a stray/trailing space never blanks the list.
    let terms: Vec<&str> = query.split_whitespace().collect();
    let total = props.workflows.len();

    // Pre-compute category for each workflow once (now uses trigger_provider).
    let with_cat: Vec<(&WorkflowItem, TriggerCategory)> = props
        .workflows
        .iter()
        .map(|wf| {
            (
                wf,
                TriggerCategory::from(&wf.trigger_type, wf.trigger_provider.as_deref()),
            )
        })
        .collect();

    // Counts per filter bucket — only non-zero ones get a filter button.
    let cnt = |cat: TriggerCategory| {
        with_cat
            .iter()
            .filter(|(wf, c)| wf.healthy && *c == cat)
            .count()
    };
    let count_recurrence = cnt(TriggerCategory::Recurrence);
    let count_http = cnt(TriggerCategory::Http);
    let count_sb = cnt(TriggerCategory::ServiceBus);
    let count_blob = cnt(TriggerCategory::Blob);
    let count_eventgrid = cnt(TriggerCategory::EventGrid);
    let count_cosmos = cnt(TriggerCategory::CosmosDb);
    let count_sql = cnt(TriggerCategory::Sql);
    let count_sftp = cnt(TriggerCategory::Sftp);
    let count_api = cnt(TriggerCategory::ApiConnection);
    let count_unknown = cnt(TriggerCategory::Unknown);
    let count_unhealthy = props.workflows.iter().filter(|wf| !wf.healthy).count();

    let mode = *filter_mode.read();

    let mut visible: Vec<_> = with_cat
        .iter()
        .filter(|(wf, cat)| {
            if !matches_filter(&wf.name, &terms) {
                return false;
            }
            match mode {
                FilterMode::All => true,
                FilterMode::Recurrence => wf.healthy && *cat == TriggerCategory::Recurrence,
                FilterMode::Http => wf.healthy && *cat == TriggerCategory::Http,
                FilterMode::ServiceBus => wf.healthy && *cat == TriggerCategory::ServiceBus,
                FilterMode::Blob => wf.healthy && *cat == TriggerCategory::Blob,
                FilterMode::EventGrid => wf.healthy && *cat == TriggerCategory::EventGrid,
                FilterMode::CosmosDb => wf.healthy && *cat == TriggerCategory::CosmosDb,
                FilterMode::Sql => wf.healthy && *cat == TriggerCategory::Sql,
                FilterMode::Sftp => wf.healthy && *cat == TriggerCategory::Sftp,
                FilterMode::ApiConnection => wf.healthy && *cat == TriggerCategory::ApiConnection,
                FilterMode::Unknown => wf.healthy && *cat == TriggerCategory::Unknown,
                FilterMode::Unhealthy => !wf.healthy,
            }
        })
        .collect();

    // Sort: currently running first, then by last_ran descending, then alphabetically.
    visible.sort_by(|(a, _), (b, _)| {
        let a_running = props.running.contains(&a.name);
        let b_running = props.running.contains(&b.name);
        if a_running != b_running {
            return b_running.cmp(&a_running);
        }
        let a_ts = props.last_ran.get(&a.name).copied().unwrap_or(0);
        let b_ts = props.last_ran.get(&b.name).copied().unwrap_or(0);
        if a_ts != b_ts {
            return b_ts.cmp(&a_ts);
        }
        a.name.to_lowercase().cmp(&b.name.to_lowercase())
    });

    let shown = visible.len();

    // Build a flat ordered list of visible names for keyboard navigation.
    let visible_names: Vec<String> = visible.iter().map(|(wf, _)| wf.name.clone()).collect();

    // Scroll selected item into view after each render where selection changes.
    use_effect(move || {
        document::eval(
            "var el = document.querySelector('#workflow-list .workflow-item.selected');\
             if (el) el.scrollIntoView({ block: 'nearest', behavior: 'instant' });",
        );
    });

    rsx! {
        div { id: "workflows",
            tabindex: "0",
            onkeydown: {
                let names = visible_names.clone();
                move |evt: KeyboardEvent| {
                    let key = evt.key();
                    if key != Key::ArrowDown && key != Key::ArrowUp { return; }
                    let current = props.selected.as_deref().unwrap_or("");
                    let idx = names.iter().position(|n| n == current);
                    let next = match (key, idx) {
                        (Key::ArrowDown, None)       => names.first(),
                        (Key::ArrowDown, Some(i))    => names.get(i + 1).or_else(|| names.first()),
                        (Key::ArrowUp,   None)       => names.last(),
                        (Key::ArrowUp,   Some(0))    => names.last(),
                        (Key::ArrowUp,   Some(i))    => names.get(i - 1),
                        _                            => None,
                    };
                    if let Some(name) = next {
                        props.on_select.call(name.clone());
                    }
                }
            },
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
                    placeholder: "Filter… (space = or, e.g. check kyriba)",
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

#[cfg(test)]
mod filter_tests {
    use super::matches_filter;

    fn terms(q: &str) -> Vec<&str> {
        q.split_whitespace().collect()
    }

    #[test]
    fn multi_word_query_ors_the_terms() {
        let t = terms("check kyriba");
        assert!(matches_filter("Check-Ignite-Payment-File", &t));
        assert!(matches_filter("Rcv-Kyriba-files-broadcast", &t));
        assert!(!matches_filter("Verify-Ignite-Voucher", &t));
    }

    #[test]
    fn single_word_still_works() {
        assert!(matches_filter(
            "Rcv-Kyriba-files-broadcast",
            &terms("kyriba")
        ));
        assert!(!matches_filter("Verify-Ignite-Voucher", &terms("kyriba")));
    }

    #[test]
    fn matching_is_case_insensitive() {
        // terms arrive already lowercased from the caller
        assert!(matches_filter("Rcv-KYRIBA-files", &terms("kyriba")));
    }

    #[test]
    fn blank_or_whitespace_query_matches_everything() {
        assert!(matches_filter("Anything", &terms("")));
        assert!(matches_filter("Anything", &terms("   ")));
        // trailing/duplicate spaces must not blank the list
        assert!(matches_filter(
            "Check-Ignite-Payment-File",
            &terms("  check   ")
        ));
    }

    #[test]
    fn non_matching_query_hides_everything() {
        assert!(!matches_filter(
            "Check-Ignite-Payment-File",
            &terms("zzz qqq")
        ));
    }
}
