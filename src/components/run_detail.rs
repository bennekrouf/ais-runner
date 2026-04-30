use dioxus::prelude::*;
use crate::services::workflows::{ActionItem, RunItem, duration_ms};

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
    run: RunItem,
    actions: Vec<ActionItem>,
    max_ms: i64,
    is_live: bool,
    on_select_run: EventHandler<String>,
}

#[component]
fn RunBlock(props: RunBlockProps) -> Element {
    let status_lower = props.run.properties.status.to_lowercase();
    let status_class = format!("run-status {}", status_lower);
    let run_id = props.run.name.clone();
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
                ActionRow { action: action, max_ms: props.max_ms, is_live: props.is_live }
            }
        }
    }
}

// ── Sub-component per action ───────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct ActionRowProps {
    action: ActionItem,
    max_ms: i64,
    is_live: bool,
}

#[component]
fn ActionRow(props: ActionRowProps) -> Element {
    let status_l = props.action.properties.status.to_lowercase();
    let is_running = props.is_live && !matches!(status_l.as_str(),
        "succeeded" | "failed" | "skipped" | "timedout" | "cancelled");

    let icon = if is_running {
        "⟳"  // will spin via CSS
    } else {
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

    let row_class = if is_running { "action-row action-row-live" } else { "action-row" };
    let icon_class = if is_running { "action-icon spin" } else { "action-icon" };

    let error_msg = props.action.properties.error
        .as_ref()
        .and_then(|e| e.message.clone());

    rsx! {
        div { class: "{row_class}",
            span { class: "{icon_class}", "{icon}" }
            span { class: "action-name", "{props.action.name}" }
            span { class: "action-duration", "{dur_label}" }
            div { class: "timing-bar-bg",
                div { class: "{bar_class}", style: "width:{pct:.0}%" }
            }
        }
        if let Some(msg) = error_msg {
            div { class: "action-error", "{msg}" }
        }
    }
}
