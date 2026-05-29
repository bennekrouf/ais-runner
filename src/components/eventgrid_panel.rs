use dioxus::prelude::*;
use crate::services::{config, azure_cli, eventgrid_check::{self, EgData, EgTopic, EgSubscription}};

// ── Props ────────────────────────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
pub struct EventGridPanelProps {
    pub logic_apps_dir: String,
}

// ── Component ────────────────────────────────────────────────────────────────

#[component]
pub fn EventGridPanel(props: EventGridPanelProps) -> Element {
    let app_cfg = config::load();
    let link = app_cfg.get_link(&props.logic_apps_dir).cloned();
    let sub_id = link.as_ref().map(|l| l.subscription_id.clone()).unwrap_or_default();
    let rg = link.as_ref().map(|l| l.resource_group.clone()).unwrap_or_default();

    let mut loading = use_signal(|| false);
    let mut error_msg: Signal<Option<String>> = use_signal(|| None);
    let mut data: Signal<Option<EgData>> = use_signal(|| None);
    let mut filter = use_signal(String::new);
    let mut detail: Signal<Option<(String, String)>> = use_signal(|| None);
    let mut revision = use_signal(|| 0u32);

    // Compare: indices into data.topics
    let mut cmp_left: Signal<Option<usize>> = use_signal(|| None);
    let mut cmp_right: Signal<Option<usize>> = use_signal(|| None);

    // Auto-fetch on mount (and on revision bump)
    use_effect({
        let sub_id = sub_id.clone();
        let rg = rg.clone();
        move || {
            let _ = revision();
            if sub_id.is_empty() || rg.is_empty() { return; }
            loading.set(true);
            error_msg.set(None);
            let sub_id = sub_id.clone();
            let rg = rg.clone();
            spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    eventgrid_check::fetch_all(&sub_id, &rg)
                }).await.unwrap_or_else(|_| Err(azure_cli::AzError::Other("Task failed".into())));
                match result {
                    Ok(d) => { data.set(Some(d)); error_msg.set(None); }
                    Err(azure_cli::AzError::NotLoggedIn) => error_msg.set(Some("Session expired — run az login".into())),
                    Err(azure_cli::AzError::Other(e)) => error_msg.set(Some(e)),
                }
                loading.set(false);
            });
        }
    });

    let is_loading = *loading.read();
    let query = filter.read().to_lowercase();

    // Build topic options for the compare combos
    let topic_options: Vec<(usize, String)> = data.read().as_ref()
        .map(|d| d.topics.iter().enumerate()
            .map(|(i, (t, _))| (i, format!("{} — {}", t.name, t.resource_group)))
            .collect())
        .unwrap_or_default();

    rsx! {
        div { class: "eg-panel",

            // ── Header ────────────────────────────────────────────────────
            div { class: "eg-header",
                div { style: "display:flex;gap:8px;align-items:center;flex-wrap:wrap",
                    h3 { style: "margin:0", "⚡ Event Grid" }
                    button {
                        class: "btn btn-small",
                        disabled: is_loading,
                        title: "Refresh",
                        onclick: move |_| revision.set(revision() + 1),
                        if is_loading { "↻ Loading…" } else { "↻ Refresh" }
                    }
                    input {
                        class: "eg-filter",
                        placeholder: "Filter…",
                        value: "{filter}",
                        oninput: move |e| filter.set(e.value()),
                    }
                }
                if sub_id.is_empty() {
                    div { class: "eg-hint", "Configure Subscription ID and Resource Group in Local Settings tab first." }
                }
            }

            // ── Compare combos ───────────────────────────────────────────
            if topic_options.len() >= 2 {
                div { class: "eg-compare-bar",
                    span { class: "eg-compare-label", "Compare" }
                    {
                        let left_val = (*cmp_left.read()).map(|i| i.to_string()).unwrap_or_default();
                        let right_val = (*cmp_right.read()).map(|i| i.to_string()).unwrap_or_default();
                        rsx! {
                            select {
                                class: "eg-select",
                                value: "{left_val}",
                                onchange: move |e: FormEvent| {
                                    cmp_left.set(e.value().parse::<usize>().ok());
                                },
                                option { value: "", "— select left —" }
                                for (idx, label) in topic_options.iter() {
                                    option { value: "{idx}", "{label}" }
                                }
                            }
                            span { class: "eg-compare-vs", "↔" }
                            select {
                                class: "eg-select",
                                value: "{right_val}",
                                onchange: move |e: FormEvent| {
                                    cmp_right.set(e.value().parse::<usize>().ok());
                                },
                                option { value: "", "— select right —" }
                                for (idx, label) in topic_options.iter() {
                                    option { value: "{idx}", "{label}" }
                                }
                            }
                        }
                    }
                }
            }

            // ── Compare side-by-side ─────────────────────────────────────
            {
                let left_idx = *cmp_left.read();
                let right_idx = *cmp_right.read();
                if let (Some(li), Some(ri), Some(ref eg)) = (left_idx, right_idx, &*data.read()) {
                    if li != ri && li < eg.topics.len() && ri < eg.topics.len() {
                        let (lt, ls) = &eg.topics[li];
                        let (rt, rs) = &eg.topics[ri];
                        rsx! { { render_compare(lt, ls, rt, rs, &mut detail) } }
                    } else { rsx! {} }
                } else { rsx! {} }
            }

            // ── Error ─────────────────────────────────────────────────────
            { if let Some(ref e) = *error_msg.read() {
                rsx! { div { class: "settings-status error", "{e}" } }
            } else { rsx! {} }}

            // ── Loading placeholder ───────────────────────────────────────
            if is_loading && data.read().is_none() {
                div { class: "eg-empty", "Fetching Event Grid topics…" }
            }

            // ── All topics list ──────────────────────────────────────────
            { if let Some(ref eg) = *data.read() {
                rsx! { { render_eg_data(eg, &query, &mut detail) } }
            } else { rsx! {} }}

            // ── Detail overlay ────────────────────────────────────────────
            if let Some((ref title, ref content)) = &*detail.read() {
                div {
                    class: "env-detail-overlay",
                    onclick: move |_| detail.set(None),
                    onkeydown: move |e: KeyboardEvent| {
                        if e.key() == Key::Escape { detail.set(None); }
                    },
                    div {
                        class: "env-detail-box",
                        onclick: move |e| e.stop_propagation(),
                        div { class: "env-detail-header", "{title}" }
                        pre { class: "env-detail-value", "{content}" }
                        div { class: "env-detail-hint", "Esc or click outside to close" }
                    }
                }
            }
        }
    }
}

// ── Compare view ─────────────────────────────────────────────────────────────

fn render_compare(
    lt: &EgTopic, ls: &[EgSubscription],
    rt: &EgTopic, rs: &[EgSubscription],
    detail: &mut Signal<Option<(String, String)>>,
) -> Element {
    // Build a union of subscription names for row alignment
    let mut all_names: Vec<String> = Vec::new();
    for s in ls.iter() {
        if !all_names.contains(&s.name) { all_names.push(s.name.clone()); }
    }
    for s in rs.iter() {
        if !all_names.contains(&s.name) { all_names.push(s.name.clone()); }
    }

    rsx! {
        div { class: "eg-compare-panel",
            // Column headers
            div { class: "eg-compare-grid",
                div { class: "eg-compare-col-header", "" }
                div { class: "eg-compare-col-header eg-compare-left",
                    "📂 {lt.name}"
                    div { class: "eg-topic-rg", "{lt.resource_group}" }
                }
                div { class: "eg-compare-col-header eg-compare-right",
                    "📂 {rt.name}"
                    div { class: "eg-topic-rg", "{rt.resource_group}" }
                }

                // Rows
                for sub_name in all_names.iter() {
                    {
                        let left = ls.iter().find(|s| &s.name == sub_name);
                        let right = rs.iter().find(|s| &s.name == sub_name);
                        let diff = match (left, right) {
                            (Some(l), Some(r)) => {
                                l.included_event_types != r.included_event_types
                                || l.filters != r.filters
                                || l.endpoint != r.endpoint
                            }
                            _ => true, // missing on one side
                        };
                        let row_class = if diff { "eg-compare-row eg-compare-diff" } else { "eg-compare-row" };
                        rsx! {
                            div { class: "{row_class} eg-compare-name", "{sub_name}" }
                            div { class: row_class,
                                { render_compare_cell(left, detail) }
                            }
                            div { class: row_class,
                                { render_compare_cell(right, detail) }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn render_compare_cell(
    sub: Option<&EgSubscription>,
    detail: &mut Signal<Option<(String, String)>>,
) -> Element {
    match sub {
        None => rsx! { span { class: "eg-compare-missing", "—" } },
        Some(s) => {
            let events = s.included_event_types.join(", ");
            let filters: String = s.filters.iter()
                .map(|f| format!("{}: {}", f.label, f.value))
                .collect::<Vec<_>>()
                .join(" | ");
            let endpoint_short = if s.endpoint.len() > 50 {
                format!("{}…", &s.endpoint[..50])
            } else {
                s.endpoint.clone()
            };
            let full_endpoint = s.endpoint.clone();
            let sub_name = s.name.clone();
            let mut detail = detail.clone();
            rsx! {
                div { class: "eg-compare-cell",
                    if !events.is_empty() {
                        div { class: "eg-compare-events", "{events}" }
                    }
                    if !filters.is_empty() {
                        div { class: "eg-compare-filters", "{filters}" }
                    }
                    div {
                        class: "eg-endpoint",
                        title: "{full_endpoint}",
                        onclick: move |_| detail.set(Some((sub_name.clone(), full_endpoint.clone()))),
                        "{endpoint_short}"
                    }
                }
            }
        }
    }
}

// ── Full list render ─────────────────────────────────────────────────────────

fn render_eg_data(
    data: &EgData,
    query: &str,
    detail: &mut Signal<Option<(String, String)>>,
) -> Element {
    let n_topics = data.topics.len();
    let n_sys = data.system_topics.len();
    rsx! {
        div { class: "eg-section-label",
            "Topics"
            span { class: "eg-count", " ({n_topics})" }
        }
        if data.topics.is_empty() {
            div { class: "eg-empty", "No custom topics in this subscription" }
        }
        for (topic, subs) in data.topics.iter() {
            {
                let name_lc = topic.name.to_lowercase();
                let rg_lc = topic.resource_group.to_lowercase();
                let topic_matches = query.is_empty() || name_lc.contains(query) || rg_lc.contains(query);
                if topic_matches {
                    rsx! {
                        div { class: "eg-topic-block",
                            div { class: "eg-topic-name",
                                "📂 {topic.name}"
                                span { class: "eg-topic-type", " {topic.input_schema} · {topic.location}" }
                            }
                            div { class: "eg-topic-rg", "{topic.resource_group}" }
                            div { class: "eg-topic-source",
                                "Endpoint: "
                                span { class: "eg-endpoint-inline", "{topic.endpoint}" }
                            }
                            { render_subs_table(subs, query, detail) }
                        }
                    }
                } else { rsx! {} }
            }
        }

        div { class: "eg-section-label", style: "margin-top: 16px",
            "System Topics"
            span { class: "eg-count", " ({n_sys})" }
        }
        if data.system_topics.is_empty() {
            div { class: "eg-empty", "No system topics in this resource group" }
        }
        for (topic, subs) in data.system_topics.iter() {
            div { class: "eg-topic-block",
                div { class: "eg-topic-name",
                    "📂 {topic.name}"
                    span { class: "eg-topic-type", " ({topic.topic_type})" }
                }
                div { class: "eg-topic-source", "Source: {topic.source}" }
                { render_subs_table(subs, query, detail) }
            }
        }
    }
}

fn render_subs_table(
    subs: &[EgSubscription],
    query: &str,
    detail: &mut Signal<Option<(String, String)>>,
) -> Element {
    if subs.is_empty() {
        return rsx! { div { class: "eg-empty", "No event subscriptions" } };
    }
    rsx! {
        table { class: "eg-table",
            thead {
                tr {
                    th { "Subscription" }
                    th { "Event Types" }
                    th { "Filters" }
                    th { "Endpoint" }
                    th { "State" }
                }
            }
            tbody {
                for sub in subs.iter() {
                    {
                        let name_lc = sub.name.to_lowercase();
                        let endpoint_lc = sub.endpoint.to_lowercase();
                        let matches = query.is_empty()
                            || name_lc.contains(query)
                            || endpoint_lc.contains(query)
                            || sub.included_event_types.iter().any(|t| t.to_lowercase().contains(query));
                        if matches {
                            let events_str = sub.included_event_types.join(", ");
                            let filters_str: String = sub.filters.iter()
                                .map(|f| format!("{}: {}", f.label, f.value))
                                .collect::<Vec<_>>()
                                .join("\n");
                            let endpoint_display = if sub.endpoint.len() > 60 {
                                format!("{}…", &sub.endpoint[..60])
                            } else {
                                sub.endpoint.clone()
                            };
                            let full_endpoint = sub.endpoint.clone();
                            let sub_name = sub.name.clone();
                            let prov = sub.provisioning_state.clone();
                            let filters_display = if filters_str.is_empty() { "—".to_string() } else { filters_str };
                            rsx! {
                                tr {
                                    td { "{sub_name}" }
                                    td { class: "eg-events", "{events_str}" }
                                    td {
                                        class: "eg-filters",
                                        title: "{filters_display}",
                                        pre { style: "margin:0;white-space:pre-wrap;font-size:11px", "{filters_display}" }
                                    }
                                    td {
                                        class: "eg-endpoint",
                                        title: "{full_endpoint}",
                                        onclick: {
                                            let title = sub_name.clone();
                                            let content = full_endpoint.clone();
                                            let mut detail = detail.clone();
                                            move |_| detail.set(Some((title.clone(), content.clone())))
                                        },
                                        "{endpoint_display}"
                                    }
                                    td {
                                        class: if prov == "Succeeded" { "eg-state-ok" } else { "eg-state-warn" },
                                        "{prov}"
                                    }
                                }
                            }
                        } else {
                            rsx! {}
                        }
                    }
                }
            }
        }
    }
}
