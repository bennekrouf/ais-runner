use crate::services::{azure_cli, config, sb_check};
use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};

// ── Fetch state ──────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, Debug)]
enum FetchState {
    Idle,
    Loading,
    Done(Vec<azure_cli::SbQueueDetail>),
    Err(String),
}

impl FetchState {
    fn queues(&self) -> Option<&[azure_cli::SbQueueDetail]> {
        if let FetchState::Done(v) = self {
            Some(v)
        } else {
            None
        }
    }
    fn is_loading(&self) -> bool {
        matches!(self, FetchState::Loading)
    }
    fn err(&self) -> Option<&str> {
        if let FetchState::Err(e) = self {
            Some(e)
        } else {
            None
        }
    }
}

/// A resolved environment: subscription + RG + SB namespace short name.
#[derive(Clone, Debug, PartialEq)]
struct SbEnv {
    label: String,
    subscription: String,
    resource_group: String,
    namespace: String, // short name
}

// ── Props ────────────────────────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
pub struct SbComparePanelProps {
    pub logic_apps_dir: String,
}

// ── Component ────────────────────────────────────────────────────────────────

#[component]
pub fn SbComparePanel(props: SbComparePanelProps) -> Element {
    // Discover available environments from config links
    let envs = use_memo(move || discover_envs());
    let local_queues = use_memo({
        let dir = props.logic_apps_dir.clone();
        move || {
            let (_ns, qs) = sb_check::detect_sb_queues(&dir);
            qs
        }
    });

    // Two environment selectors
    let mut left_idx: Signal<Option<usize>> = use_signal(|| None);
    let mut right_idx: Signal<Option<usize>> = use_signal(|| None);
    let mut left_state: Signal<FetchState> = use_signal(|| FetchState::Idle);
    let mut right_state: Signal<FetchState> = use_signal(|| FetchState::Idle);
    let mut only_diff: Signal<bool> = use_signal(|| false);
    let mut filter: Signal<String> = use_signal(String::new);

    // Auto-fetch when selection changes
    let envs_left = envs.read().clone();
    let envs_right = envs.read().clone();
    use_effect(move || {
        let idx = *left_idx.read();
        if let Some(i) = idx {
            let envs = envs_left.clone();
            if let Some(env) = envs.get(i).cloned() {
                left_state.set(FetchState::Loading);
                spawn(async move {
                    let res = tokio::task::spawn_blocking(move || {
                        azure_cli::sb_list_queues(
                            &env.subscription,
                            &env.resource_group,
                            &env.namespace,
                        )
                    })
                    .await;
                    match res {
                        Ok(Ok(qs)) => left_state.set(FetchState::Done(qs)),
                        Ok(Err(e)) => left_state.set(FetchState::Err(format!("{:?}", e))),
                        Err(e) => left_state.set(FetchState::Err(format!("{e}"))),
                    }
                });
            }
        } else {
            left_state.set(FetchState::Idle);
        }
    });
    use_effect(move || {
        let idx = *right_idx.read();
        if let Some(i) = idx {
            let envs = envs_right.clone();
            if let Some(env) = envs.get(i).cloned() {
                right_state.set(FetchState::Loading);
                spawn(async move {
                    let res = tokio::task::spawn_blocking(move || {
                        azure_cli::sb_list_queues(
                            &env.subscription,
                            &env.resource_group,
                            &env.namespace,
                        )
                    })
                    .await;
                    match res {
                        Ok(Ok(qs)) => right_state.set(FetchState::Done(qs)),
                        Ok(Err(e)) => right_state.set(FetchState::Err(format!("{:?}", e))),
                        Err(e) => right_state.set(FetchState::Err(format!("{e}"))),
                    }
                });
            }
        } else {
            right_state.set(FetchState::Idle);
        }
    });

    let query = filter.read().to_lowercase();
    let diff_on = *only_diff.read();

    // Build comparison data
    let left_qs = left_state.read().queues().map(|q| q.to_vec());
    let right_qs = right_state.read().queues().map(|q| q.to_vec());
    let local_qs = local_queues.read();

    // Collect all queue names from all sources
    let mut all_names: Vec<String> = {
        let mut set: HashSet<String> = HashSet::new();
        for q in local_qs.iter() {
            set.insert(q.queue.clone());
        }
        if let Some(ref qs) = left_qs {
            for q in qs {
                set.insert(q.name.clone());
            }
        }
        if let Some(ref qs) = right_qs {
            for q in qs {
                set.insert(q.name.clone());
            }
        }
        set.into_iter().collect()
    };
    all_names.sort();

    // Index by name for quick lookup
    let left_map: HashMap<&str, &azure_cli::SbQueueDetail> = left_qs
        .as_ref()
        .map(|qs| qs.iter().map(|q| (q.name.as_str(), q)).collect())
        .unwrap_or_default();
    let right_map: HashMap<&str, &azure_cli::SbQueueDetail> = right_qs
        .as_ref()
        .map(|qs| qs.iter().map(|q| (q.name.as_str(), q)).collect())
        .unwrap_or_default();
    let local_set: HashSet<&str> = local_qs.iter().map(|q| q.queue.as_str()).collect();

    let visible: Vec<&str> = all_names
        .iter()
        .map(|n| n.as_str())
        .filter(|n| query.is_empty() || n.to_lowercase().contains(&query))
        .filter(|n| {
            if !diff_on {
                return true;
            }
            let in_left = left_map.contains_key(n);
            let in_right = right_map.contains_key(n);
            let in_local = local_set.contains(n);
            // Show if presence differs, or if properties differ
            if left_qs.is_some() && right_qs.is_some() {
                if in_left != in_right {
                    return true;
                }
                if in_left && in_right {
                    return queues_differ(left_map[n], right_map[n]);
                }
                return false;
            }
            // One side + local
            if left_qs.is_some() {
                return in_left != in_local;
            }
            if right_qs.is_some() {
                return in_right != in_local;
            }
            false
        })
        .collect();

    let has_data = left_qs.is_some() || right_qs.is_some();
    let left_label = (*left_idx.read()).and_then(|i| envs.read().get(i).map(|e| e.label.clone()));
    let right_label = (*right_idx.read()).and_then(|i| envs.read().get(i).map(|e| e.label.clone()));

    rsx! {
        div { class: "eg-panel",
            // ── Environment selectors ──────────────────────────
            div { class: "eg-compare-bar",
                span { style: "font-weight:600; margin-right:8px;", "Compare Service Bus queues" }
                select {
                    class: "eg-select",
                    value: (*left_idx.read()).map(|i| i.to_string()).unwrap_or_default(),
                    onchange: move |e: Event<FormData>| {
                        let v = e.value();
                        left_idx.set(if v.is_empty() { None } else { v.parse().ok() });
                    },
                    option { value: "", "— select environment —" }
                    for (i, env) in envs.read().iter().enumerate() {
                        option { value: "{i}", "{env.label}" }
                    }
                }
                span { style: "opacity:.5;", "vs" }
                select {
                    class: "eg-select",
                    value: (*right_idx.read()).map(|i| i.to_string()).unwrap_or_default(),
                    onchange: move |e: Event<FormData>| {
                        let v = e.value();
                        right_idx.set(if v.is_empty() { None } else { v.parse().ok() });
                    },
                    option { value: "", "— select environment —" }
                    for (i, env) in envs.read().iter().enumerate() {
                        option { value: "{i}", "{env.label}" }
                    }
                }
                div { style: "flex:1" }
                input {
                    class: "env-filter-input",
                    placeholder: "Filter queues...",
                    value: "{filter}",
                    oninput: move |e| filter.set(e.value()),
                }
                label { class: "env-diff-toggle",
                    input {
                        r#type: "checkbox",
                        checked: *only_diff.read(),
                        onchange: move |_| { let v = *only_diff.read(); only_diff.set(!v); },
                    }
                    " Differences only"
                }
            }

            // ── Loading / error states ────────────────────────
            if left_state.read().is_loading() || right_state.read().is_loading() {
                div { class: "eg-loading", "Loading queues..." }
            }
            if let Some(e) = left_state.read().err() {
                div { class: "az-error", "Left: {e}" }
            }
            if let Some(e) = right_state.read().err() {
                div { class: "az-error", "Right: {e}" }
            }

            // ── Comparison table ──────────────────────────────
            if !visible.is_empty() {
                table { class: "env-compare-table",
                    thead {
                        tr {
                            th { class: "env-th-key", "Queue" }
                            th { class: "env-th-val", "Local" }
                            if left_qs.is_some() {
                                th { class: "env-th-val",
                                    "{left_label.as_deref().unwrap_or(\"Left\")}"
                                }
                            }
                            if right_qs.is_some() {
                                th { class: "env-th-val",
                                    "{right_label.as_deref().unwrap_or(\"Right\")}"
                                }
                            }
                        }
                    }
                    tbody {
                        for name in &visible {
                            {
                                let in_local = local_set.contains(name);
                                let lq = left_map.get(name);
                                let rq = right_map.get(name);

                                let row_diff = (left_qs.is_some() && right_qs.is_some() && lq.is_some() != rq.is_some())
                                    || (lq.is_some() && rq.is_some() && queues_differ(lq.unwrap(), rq.unwrap()))
                                    || (left_qs.is_some() && lq.is_some() != in_local)
                                    || (right_qs.is_some() && rq.is_some() != in_local);

                                let row_class = if row_diff { "env-compare-row has-diff" } else { "env-compare-row" };

                                rsx! {
                                    tr { class: "{row_class}",
                                        td { class: "env-col-key", "{name}" }
                                        td { class: "env-col-val",
                                            if in_local {
                                                span { class: "env-val-local", "✓" }
                                            } else {
                                                span { class: "env-val-missing", "—" }
                                            }
                                        }
                                        if left_qs.is_some() {
                                            td { class: "env-col-val",
                                                { render_queue_cell(lq.copied()) }
                                            }
                                        }
                                        if right_qs.is_some() {
                                            td { class: "env-col-val",
                                                { render_queue_cell(rq.copied()) }
                                            }
                                        }
                                    }
                                    // Property diff sub-rows when both sides have the queue
                                    if lq.is_some() && rq.is_some() && queues_differ(lq.unwrap(), rq.unwrap()) {
                                        { render_prop_diff_rows(lq.unwrap(), rq.unwrap(), left_qs.is_some(), right_qs.is_some()) }
                                    }
                                }
                            }
                        }
                    }
                }
            } else if has_data {
                div { class: "env-compare-empty",
                    if diff_on {
                        "No differences found — all queues match."
                    } else {
                        "No queues found."
                    }
                }
            } else {
                div { class: "env-compare-empty",
                    "Select two environments above to compare Service Bus queues."
                }
            }
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn discover_envs() -> Vec<SbEnv> {
    let cfg = config::load();
    let mut envs = Vec::new();
    for (_path, link) in &cfg.workspace_links {
        if let Some(ref ns) = link.sb_namespace {
            let short = ns.split('.').next().unwrap_or(ns);
            let label = format!("{} / {}", link.resource_group, short);
            // Deduplicate by (sub, rg, ns)
            if !envs.iter().any(|e: &SbEnv| {
                e.subscription == link.subscription_id
                    && e.resource_group == link.resource_group
                    && e.namespace == short
            }) {
                envs.push(SbEnv {
                    label,
                    subscription: link.subscription_id.clone(),
                    resource_group: link.resource_group.clone(),
                    namespace: short.to_string(),
                });
            }
        }
    }
    envs.sort_by(|a, b| a.label.cmp(&b.label));
    envs
}

fn queues_differ(a: &azure_cli::SbQueueDetail, b: &azure_cli::SbQueueDetail) -> bool {
    a.requires_session != b.requires_session
        || a.max_delivery != b.max_delivery
        || a.max_size_mb != b.max_size_mb
        || a.lock_duration != b.lock_duration
        || a.default_ttl != b.default_ttl
        || a.auto_delete != b.auto_delete
        || a.status != b.status
}

fn render_queue_cell(q: Option<&azure_cli::SbQueueDetail>) -> Element {
    match q {
        None => rsx! { span { class: "env-val-missing", "—" } },
        Some(q) => {
            let session = if q.requires_session { " 🔒" } else { "" };
            let title = format!(
                "status={}, maxSize={}MB, maxDelivery={}, session={}",
                q.status, q.max_size_mb, q.max_delivery, q.requires_session
            );
            rsx! {
                span { class: "env-val-local", title: "{title}",
                    "✓{session}"
                }
                if q.active_messages > 0 || q.dead_letter > 0 {
                    span {
                        class: if q.dead_letter > 0 { "sb-msg-badge sb-dlq" } else { "sb-msg-badge" },
                        title: "active: {q.active_messages}, DLQ: {q.dead_letter}",
                        if q.dead_letter > 0 {
                            "💀{q.dead_letter}"
                        } else {
                            "📨{q.active_messages}"
                        }
                    }
                }
            }
        }
    }
}

fn render_prop_diff_rows(
    left: &azure_cli::SbQueueDetail,
    right: &azure_cli::SbQueueDetail,
    show_left: bool,
    show_right: bool,
) -> Element {
    let props: Vec<(&str, String, String)> = vec![
        (
            "maxDelivery",
            left.max_delivery.to_string(),
            right.max_delivery.to_string(),
        ),
        (
            "requiresSession",
            left.requires_session.to_string(),
            right.requires_session.to_string(),
        ),
        (
            "maxSize (MB)",
            left.max_size_mb.to_string(),
            right.max_size_mb.to_string(),
        ),
        (
            "lockDuration",
            left.lock_duration.clone(),
            right.lock_duration.clone(),
        ),
        (
            "defaultTTL",
            left.default_ttl.clone(),
            right.default_ttl.clone(),
        ),
        (
            "autoDelete",
            left.auto_delete.clone(),
            right.auto_delete.clone(),
        ),
        ("status", left.status.clone(), right.status.clone()),
    ];

    rsx! {
        for (label, lv, rv) in props.iter().filter(|(_, l, r)| l != r) {
            tr { class: "env-compare-row sb-prop-row",
                td { class: "env-col-key sb-prop-key", "  └ {label}" }
                td { class: "env-col-val", "" }
                if show_left {
                    td { class: "env-col-val",
                        span { class: "env-val-differs", "{lv}" }
                    }
                }
                if show_right {
                    td { class: "env-col-val",
                        span { class: "env-val-differs", "{rv}" }
                    }
                }
            }
        }
    }
}
