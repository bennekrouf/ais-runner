use dioxus::prelude::*;
use crate::services::workflows::{self, ActionItem, RunItem, duration_ms};

#[derive(Props, Clone, PartialEq)]
pub struct RunDetailProps {
    pub workflow:      Option<String>,
    pub source_text:   String,
    pub runs:          Vec<RunItem>,
    pub actions:       Vec<ActionItem>,
    pub is_live:       bool,
    pub active_tab:    Signal<String>,
    pub on_run:        EventHandler<()>,
    pub on_refresh:    EventHandler<()>,
    pub on_clear_runs: EventHandler<()>,
    pub on_select_run: EventHandler<String>,
}

#[component]
pub fn RunDetail(props: RunDetailProps) -> Element {
    let title = props.workflow.clone().unwrap_or_else(|| "Select a workflow".to_string());
    let mut active_tab = props.active_tab;

    let max_ms = props.actions.iter()
        .filter_map(|a| duration_ms(&a.properties.start_time, &a.properties.end_time))
        .max()
        .unwrap_or(1)
        .max(1);

    let workflow_name = props.workflow.clone().unwrap_or_default();

    rsx! {
        div { id: "detail",

            // ── Tab bar + header ───────────────────────────────────────
            div { id: "detail-header",

                // left: workflow name + tabs
                div { class: "detail-header-left",
                    h2 { "{title}" }
                    div { class: "detail-tabs",
                        button {
                            class: if *active_tab.read() == "Source" { "detail-tab active" } else { "detail-tab" },
                            onclick: move |_| active_tab.set("Source".to_string()),
                            "⟨/⟩ Source"
                        }
                        button {
                            class: if *active_tab.read() == "Run" { "detail-tab active" } else { "detail-tab" },
                            onclick: move |_| active_tab.set("Run".to_string()),
                            "▶ Run"
                        }
                    }
                }

                // right: action buttons + live badge (fixed positions)
                div { class: "detail-header-right",
                    button {
                        class: "btn btn-run btn-small",
                        disabled: props.workflow.is_none() || props.is_live,
                        title: "Trigger workflow",
                        onclick: move |_| props.on_run.call(()),
                        "▶ Trigger"
                    }
                    button {
                        class: "btn btn-run btn-small",
                        disabled: props.is_live,
                        title: "Refresh run history",
                        onclick: move |_| props.on_refresh.call(()),
                        "⟳ Refresh"
                    }
                    button {
                        class: "btn btn-small btn-clear",
                        title: "Clear run history",
                        onclick: move |_| props.on_clear_runs.call(()),
                        "✕ Clear"
                    }
                    if props.is_live {
                        span { class: "live-badge", "● LIVE" }
                    }
                }
            }

            // ── Tab content ────────────────────────────────────────────
            if *active_tab.read() == "Run" {
                div { id: "runs",
                    if props.runs.is_empty() {
                        div { class: "empty-state", "No runs yet. Click ▶ to trigger the workflow." }
                    }
                    for run in props.runs.clone() {
                        RunBlock {
                            run: run,
                            actions: props.actions.clone(),
                            max_ms: max_ms,
                            is_live: props.is_live,
                            workflow: workflow_name.clone(),
                            on_select_run: props.on_select_run.clone(),
                        }
                    }
                }
            } else {
                div { id: "source-view",
                    if props.workflow.is_none() {
                        div { class: "empty-state", "Select a workflow to view its source." }
                    } else {
                        pre { id: "source-pre", "{props.source_text}" }
                    }
                }
            }
        }
    }
}

// ── Sub-component per run ──────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct RunBlockProps {
    run:           RunItem,
    actions:       Vec<ActionItem>,
    max_ms:        i64,
    is_live:       bool,
    workflow:      String,
    on_select_run: EventHandler<String>,
}

#[component]
fn RunBlock(props: RunBlockProps) -> Element {
    let status_lower = props.run.properties.status.to_lowercase();
    let status_class = format!("run-status {}", status_lower);
    let run_id  = props.run.name.clone();
    let run_id2 = run_id.clone();

    rsx! {
        div { class: "run-block",
            div {
                class: "run-header",
                style: "cursor:pointer",
                onclick: move |_| props.on_select_run.call(run_id2.clone()),
                span { class: "{status_class}", "{props.run.properties.status}" }
                span { "{run_id}" }
                if let Some(start) = &props.run.properties.start_time {
                    span { style: "margin-left:auto",
                        "{&start[..19].replace('T', \" \")}"
                    }
                }
            }
            for action in props.actions.clone() {
                ActionRow {
                    action: action,
                    max_ms: props.max_ms,
                    is_live: props.is_live,
                    workflow: props.workflow.clone(),
                    run_id: run_id.clone(),
                    depth: 0,
                }
            }
        }
    }
}

// ── Fetch children for expandable actions ─────────────────────────────────

async fn fetch_children(workflow: String, run_id: String, action: String, action_type: Option<String>) -> Vec<ActionItem> {
    match action_type.as_deref() {
        Some("Foreach") => {
            let reps = match workflows::list_repetitions(&workflow, &run_id, &action).await {
                Ok(r) => r,
                Err(_) => return vec![],
            };
            let multi = reps.len() > 1;
            let mut all = Vec::new();
            for (i, rep) in reps.iter().enumerate() {
                if let Ok(acts) = workflows::list_repetition_actions(&workflow, &run_id, &action, &rep.name).await {
                    if multi {
                        for mut act in acts {
                            act.name = format!("[{}] {}", i, act.name);
                            all.push(act);
                        }
                    } else {
                        all.extend(acts);
                    }
                }
            }
            all
        }
        Some("Scope") | Some("Until") | Some("If") => {
            workflows::list_scoped_repetitions(&workflow, &run_id, &action)
                .await
                .unwrap_or_default()
        }
        _ => vec![],
    }
}

// ── Sub-component per action ───────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct ActionRowProps {
    action:   ActionItem,
    max_ms:   i64,
    is_live:  bool,
    workflow: String,
    run_id:   String,
    depth:    u8,
}

#[component]
fn ActionRow(props: ActionRowProps) -> Element {
    let atype = props.action.properties.action_type.as_deref().unwrap_or("");
    let is_expandable = matches!(atype, "Foreach" | "Scope" | "Until" | "If");

    let mut expanded = use_signal(|| false);
    let mut loading  = use_signal(|| false);
    let mut children = use_signal(|| Vec::<ActionItem>::new());

    let status_l = props.action.properties.status.to_lowercase();
    let is_running = props.is_live && !matches!(status_l.as_str(),
        "succeeded" | "failed" | "skipped" | "timedout" | "cancelled");

    let icon = if is_running { "⟳" } else {
        match status_l.as_str() {
            "succeeded" => "✅",
            "failed"    => "❌",
            "skipped"   => "⏭",
            _           => "⏳",
        }
    };

    let ms = duration_ms(
        &props.action.properties.start_time,
        &props.action.properties.end_time,
    ).unwrap_or(0);
    let pct = ((ms as f64 / props.max_ms as f64) * 100.0).clamp(1.0, 100.0);
    let bar_class = format!("timing-bar {}", status_l);
    let dur_label = if ms == 0 && is_running {
        "…".to_string()
    } else if ms < 1000 {
        format!("{}ms", ms)
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    };

    let row_class   = if is_running { "action-row action-row-live" } else { "action-row" };
    let icon_class  = if is_running { "action-icon spin" } else { "action-icon" };
    let indent_px   = props.depth as u32 * 18;

    let error_msg = props.action.properties.error
        .as_ref()
        .and_then(|e| e.message.clone());

    let child_max_ms = {
        let c = children.read();
        c.iter()
            .filter_map(|a| duration_ms(&a.properties.start_time, &a.properties.end_time))
            .max()
            .unwrap_or(1)
            .max(1)
    };

    let err_class = if status_l == "skipped" { "action-warning" } else { "action-error" };

    rsx! {
        div { class: "{row_class}", style: "padding-left:{indent_px}px",
            // expand toggle (only for ForEach / Scope / Until / If)
            if is_expandable {
                button {
                    class: "btn-icon action-expand",
                    title: if *expanded.read() { "Collapse" } else { "Expand child actions" },
                    onclick: {
                        let wf   = props.workflow.clone();
                        let rid  = props.run_id.clone();
                        let name = props.action.name.clone();
                        let at   = props.action.properties.action_type.clone();
                        move |_| {
                            if *expanded.read() {
                                expanded.set(false);
                            } else if children.read().is_empty() {
                                loading.set(true);
                                let wf2  = wf.clone();
                                let rid2 = rid.clone();
                                let n2   = name.clone();
                                let at2  = at.clone();
                                spawn(async move {
                                    let result = fetch_children(wf2, rid2, n2, at2).await;
                                    children.set(result);
                                    loading.set(false);
                                    expanded.set(true);
                                });
                            } else {
                                expanded.set(true);
                            }
                        }
                    },
                    if *loading.read() { "…" }
                    else if *expanded.read() { "▼" }
                    else { "▶" }
                }
            } else {
                // placeholder so action columns stay aligned
                span { class: "action-expand-placeholder" }
            }
            span { class: "{icon_class}", "{icon}" }
            span { class: "action-name", "{props.action.name}" }
            span { class: "action-duration", "{dur_label}" }
            div { class: "timing-bar-bg",
                div { class: "{bar_class}", style: "width:{pct:.0}%" }
            }
        }
        if let Some(msg) = error_msg {
            div { class: "{err_class}", style: "padding-left:{indent_px}px", "{msg}" }
        }
        if *expanded.read() {
            for child in children.read().clone() {
                ActionRow {
                    action: child,
                    max_ms: child_max_ms,
                    is_live: props.is_live,
                    workflow: props.workflow.clone(),
                    run_id: props.run_id.clone(),
                    depth: props.depth + 1,
                }
            }
        }
    }
}
