use dioxus::prelude::*;
use crate::services::workflows::{self, ActionItem, RunItem, duration_ms};
use crate::services::workflow_analysis::{WorkflowAnalysis, TriggerKind};
use crate::services::config::WorkspaceLink;
use crate::components::log_panel::LogLine;
use crate::components::tooltip::Tooltip;

use crate::utils::open_in_editor;

#[derive(Props, Clone, PartialEq)]
pub struct RunDetailProps {
    pub workflow:           Option<String>,
    pub source_text:        String,
    pub runs:               Vec<RunItem>,
    pub actions:            Vec<ActionItem>,
    pub is_live:            bool,
    pub active_tab:         Signal<String>,
    pub health_error:       Option<String>,
    pub logs:               Vec<LogLine>,
    pub analysis:           WorkflowAnalysis,
    pub source_path:        Option<String>,
    pub workspace_link:     Option<WorkspaceLink>,
    pub suggested_payload:  String,
    pub on_run:             EventHandler<()>,
    pub on_refresh:         EventHandler<()>,
    pub on_clear_runs:      EventHandler<()>,
    pub on_select_run:      EventHandler<String>,
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

    // ── Publish dialog state ───────────────────────────────────────────────
    let mut publish_open   = use_signal(|| false);
    let mut publish_busy   = use_signal(|| false);
    let mut publish_result = use_signal(|| Option::<Result<String, String>>::None);

    // ── Payload popover ────────────────────────────────────────────────────
    let mut payload_open   = use_signal(|| false);
    let mut copied         = use_signal(|| false);
    let suggested          = props.suggested_payload.clone();

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
                        button {
                            class: if *active_tab.read() == "Logs" { "detail-tab active" } else { "detail-tab" },
                            onclick: move |_| active_tab.set("Logs".to_string()),
                            "📜 Logs"
                        }
                    }
                }

                // right: action buttons + live badge (fixed positions)
                div { class: "detail-header-right",
                    Tooltip { text: "Trigger workflow", direction: "bottom",
                        button {
                            class: "btn btn-run btn-small",
                            disabled: props.workflow.is_none() || props.is_live,
                            onclick: move |_| props.on_run.call(()),
                            "▶ Trigger"
                        }
                    }
                    Tooltip { text: "Refresh run history", direction: "bottom",
                        button {
                            class: "btn btn-run btn-small",
                            disabled: props.is_live,
                            onclick: move |_| props.on_refresh.call(()),
                            "⟳ Refresh"
                        }
                    }
                    Tooltip { text: "Flush history — hides all existing runs. Only runs triggered after this point will appear.", direction: "bottom",
                        button {
                            class: "btn btn-small btn-clear",
                            onclick: move |_| props.on_clear_runs.call(()),
                            "⊘ Flush"
                        }
                    }
                    if props.is_live {
                        span { class: "live-badge", "● LIVE" }
                    }
                }
            }

            // ── Workflow analysis bar ─────────────────────────────────
            {
                let a = &props.analysis;
                if props.workflow.is_some() && !a.is_empty() {
                    rsx! {
                        div { class: "wf-analysis-bar",
                            // trigger chip
                            match &a.trigger {
                                TriggerKind::Http  => rsx! {
                                    span { class: "wf-chip wf-chip-http", title: "HTTP trigger",
                                        span { class: "wf-dir wf-dir-in", "▼" }
                                        span { class: "wf-type", "HTTP" }
                                        if !suggested.is_empty() && suggested != "{}" {
                                            button {
                                                class: "wf-chip-payload-btn",
                                                title: "Show sample payload",
                                                onclick: move |e| { e.stop_propagation(); payload_open.set(!payload_open()); },
                                                "📋"
                                            }
                                        }
                                    }
                                },
                                TriggerKind::Timer { schedule } => rsx! {
                                    span { class: "wf-chip wf-chip-timer", title: "Scheduled trigger",
                                        span { class: "wf-dir wf-dir-in", "▼" }
                                        span { class: "wf-type", "Timer" }
                                        span { class: "wf-name", "{schedule}" }
                                    }
                                },
                                TriggerKind::ServiceBus { queue } => rsx! {
                                    span { class: "wf-chip wf-chip-sb", title: "Service Bus trigger: reads from {queue}",
                                        span { class: "wf-dir wf-dir-in", "▼" }
                                        span { class: "wf-type", "SB" }
                                        span { class: "wf-name", "{queue}" }
                                        if !suggested.is_empty() && suggested != "{}" {
                                            button {
                                                class: "wf-chip-payload-btn",
                                                title: "Show sample message body",
                                                onclick: move |e| { e.stop_propagation(); payload_open.set(!payload_open()); },
                                                "📋"
                                            }
                                        }
                                    }
                                },
                                TriggerKind::Blob { container } => rsx! {
                                    span { class: "wf-chip wf-chip-blob", title: "Blob trigger: listens on {container}",
                                        span { class: "wf-dir wf-dir-in", "▼" }
                                        span { class: "wf-type", "Blob" }
                                        span { class: "wf-name", "{container}" }
                                    }
                                },
                                TriggerKind::Unknown => rsx! { span {} },
                            }

                            // payload popover
                            if payload_open() {
                                div { class: "wf-payload-popover",
                                    div { class: "wf-payload-popover-header",
                                        span { "Sample payload" }
                                        div { style: "display:flex;gap:6px;align-items:center",
                                            button {
                                                class: "wf-payload-copy-btn",
                                                onclick: {
                                                    let s = suggested.clone();
                                                    move |_| {
                                                        let _ = document::eval(&format!(
                                                            "navigator.clipboard.writeText({})",
                                                            serde_json::to_string(&s).unwrap_or_default()
                                                        ));
                                                        copied.set(true);
                                                        let mut c = copied;
                                                        spawn(async move {
                                                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                                            c.set(false);
                                                        });
                                                    }
                                                },
                                                if copied() { "✅ Copied" } else { "📋 Copy" }
                                            }
                                            button {
                                                class: "btn-icon",
                                                onclick: move |_| payload_open.set(false),
                                                "×"
                                            }
                                        }
                                    }
                                    pre { class: "wf-payload-code", "{suggested}" }
                                }
                            }

                            // input queues (excluding trigger — already shown)
                            for q in a.input_queues.iter().filter(|q| {
                                !matches!(&a.trigger, TriggerKind::ServiceBus { queue } if queue == *q)
                            }) {
                                span { class: "wf-chip wf-chip-sb", title: "Reads from queue: {q}",
                                    span { class: "wf-dir wf-dir-in", "▼" }
                                    span { class: "wf-type", "SB" }
                                    span { class: "wf-name", "{q}" }
                                }
                            }
                            // output queues
                            for q in &a.output_queues {
                                span { class: "wf-chip wf-chip-sb", title: "Sends to queue: {q}",
                                    span { class: "wf-dir wf-dir-out", "▲" }
                                    span { class: "wf-type", "SB" }
                                    span { class: "wf-name", "{q}" }
                                }
                            }
                            // input blobs (excluding trigger)
                            for c in a.input_blobs.iter().filter(|c| {
                                !matches!(&a.trigger, TriggerKind::Blob { container } if container == *c)
                            }) {
                                span { class: "wf-chip wf-chip-blob", title: "Reads from container: {c}",
                                    span { class: "wf-dir wf-dir-in", "▼" }
                                    span { class: "wf-type", "Blob" }
                                    span { class: "wf-name", "{c}" }
                                }
                            }
                            // output blobs
                            for c in &a.output_blobs {
                                span { class: "wf-chip wf-chip-blob", title: "Writes to container: {c}",
                                    span { class: "wf-dir wf-dir-out", "▲" }
                                    span { class: "wf-type", "Blob" }
                                    span { class: "wf-name", "{c}" }
                                }
                            }
                            // http calls
                            for h in &a.http_calls {
                                span { class: "wf-chip wf-chip-http", title: "Calls: {h}",
                                    span { class: "wf-dir wf-dir-out", "▲" }
                                    span { class: "wf-type", "HTTP" }
                                    span { class: "wf-name", "{h}" }
                                }
                            }
                            // liquid transforms
                            for m in &a.liquid_maps {
                                span { class: "wf-chip wf-chip-liquid", title: "Liquid transform: {m}",
                                    span { class: "wf-type", "🔄" }
                                    span { class: "wf-name", "{m}" }
                                }
                            }
                        }
                    }
                } else { rsx! {} }
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
            } else if *active_tab.read() == "Logs" {
                div { id: "status-view",
                    if props.workflow.is_none() {
                        div { class: "empty-state", "Select a workflow to view its logs." }
                    } else {
                        if let Some(err) = props.health_error.clone() {
                            div { class: "status-container",
                                h3 { "Workflow Status" }
                                div { class: "status-card unhealthy",
                                    span { class: "status-icon", "🔴" }
                                    div { class: "status-info",
                                        span { class: "status-label", "Unhealthy" }
                                        p { class: "status-message", "{err}" }
                                    }
                                }
                            }
                        }

                        div { class: "status-logs-full",
                            div { class: "status-logs-scroll",
                                if props.logs.is_empty() {
                                    div { class: "empty-state", "No related logs found in current session." }
                                } else {
                                    for line in props.logs.iter() {
                                        div { class: "log-line",
                                            span { class: "log-time", "{line.time}" }
                                            span { class: line.level.css_class(), "{line.msg}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            } else {
                div { id: "source-view",
                    if props.workflow.is_none() {
                        div { class: "empty-state", "Select a workflow to view its source." }
                    } else {
                        div { class: "source-wrap",
                            button {
                                class: "source-copy-btn",
                                title: "Copy to clipboard",
                                onclick: {
                                    let text = props.source_text.clone();
                                    move |_| {
                                        let text = text.clone();
                                        spawn(async move {
                                            tokio::task::spawn_blocking(move || {
                                                if let Ok(mut cb) = arboard::Clipboard::new() {
                                                    let _ = cb.set_text(text);
                                                }
                                            }).await.ok();
                                        });
                                    }
                                },
                                "⎘"
                            }
                            if let Some(path) = props.source_path.clone() {
                                button {
                                    class: "source-copy-btn",
                                    title: "Open in editor",
                                    onclick: move |_| {
                                        let p = path.clone();
                                        std::thread::spawn(move || { open_in_editor(&p); });
                                    },
                                    "✎"
                                }
                            }
                            // ── Publish button — only when Logic App is configured ──
                            if props.workspace_link.as_ref()
                                .and_then(|l| l.logic_app_name.as_deref())
                                .is_some()
                                && props.source_path.is_some()
                                && props.workflow.is_some()
                            {
                                button {
                                    class: "source-copy-btn",
                                    title: "Publish this workflow to Azure",
                                    onclick: move |_| {
                                        publish_result.set(None);
                                        publish_open.set(true);
                                    },
                                    "⬆"
                                }
                            }
                            pre { id: "source-pre", "{props.source_text}" }
                        }
                    }
                }
            }

            // ── Publish confirmation modal ─────────────────────────────────
            if *publish_open.read() {
                if let (Some(link), Some(path), Some(wf)) = (
                    props.workspace_link.clone(),
                    props.source_path.clone(),
                    props.workflow.clone(),
                ) {
                    if let Some(app_name) = link.logic_app_name.clone() {
                        div { class: "az-confirm-backdrop",
                            onclick: move |_| { if !*publish_busy.read() { publish_open.set(false); } },
                            div { class: "az-confirm-modal",
                                onclick: move |e| e.stop_propagation(),

                                div { class: "az-confirm-title", "⬆ Publish workflow to Azure" }

                                p { style: "margin: 0 0 0.25rem;",
                                    span { style: "opacity:.7", "Workflow: " }
                                    strong { "{wf}" }
                                }
                                p { style: "margin: 0 0 0.25rem;",
                                    span { style: "opacity:.7", "Logic App: " }
                                    strong { "{app_name}" }
                                }
                                p { class: "az-confirm-body",
                                    "This replaces the live workflow definition in Azure directly, \
                                     without a commit or CI/CD pipeline."
                                }

                                // ── result / error feedback ──────────────
                                match publish_result.read().as_ref() {
                                    Some(Ok(msg)) => rsx! {
                                        p { style: "color: var(--green); margin-bottom:.75rem;", "{msg}" }
                                    },
                                    Some(Err(err)) => rsx! {
                                        p { style: "color: var(--red); margin-bottom:.75rem; white-space:pre-wrap; font-size:.82rem;",
                                            "{err}"
                                        }
                                    },
                                    None => rsx! { span {} },
                                }

                                div { class: "az-confirm-actions",
                                    button {
                                        class: "btn btn-small",
                                        disabled: *publish_busy.read(),
                                        onclick: move |_| { publish_open.set(false); },
                                        "Cancel"
                                    }
                                    button {
                                        class: "btn btn-run btn-small",
                                        disabled: *publish_busy.read(),
                                        onclick: {
                                            let sub  = link.subscription_id.clone();
                                            let rg   = link.resource_group.clone();
                                            let app  = app_name.clone();
                                            let name = wf.clone();
                                            let p    = path.clone();
                                            move |_| {
                                                let sub  = sub.clone();
                                                let rg   = rg.clone();
                                                let app  = app.clone();
                                                let name = name.clone();
                                                let p    = p.clone();
                                                publish_busy.set(true);
                                                publish_result.set(None);
                                                spawn(async move {
                                                    let res = tokio::task::spawn_blocking(move || {
                                                        crate::services::azure_cli::publish_workflow(
                                                            &sub, &rg, &app, &name, &p,
                                                        )
                                                    }).await;
                                                    let outcome = match res {
                                                        Ok(Ok(msg)) => Ok(msg),
                                                        Ok(Err(e))  => Err(format!("{:?}", e)),
                                                        Err(e)      => Err(format!("task error: {}", e)),
                                                    };
                                                    publish_result.set(Some(outcome));
                                                    publish_busy.set(false);
                                                });
                                            }
                                        },
                                        if *publish_busy.read() { "Publishing…" } else { "⬆ Publish" }
                                    }
                                }
                            }
                        }
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

    let mut expanded       = use_signal(|| false);
    let mut loading        = use_signal(|| false);
    let mut children       = use_signal(|| Vec::<ActionItem>::new());
    let mut detail_open    = use_signal(|| false);
    let mut detail_loading = use_signal(|| false);
    let mut detail_json    = use_signal(|| String::new());

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

    let error_msg = props.action.properties.error.as_ref().and_then(|e| {
        // Prefer message; fall back to code so skipped-reason is always visible
        e.message.clone().or_else(|| e.code.clone())
    });

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
            // ── expand toggle: child actions (Foreach/Scope/Until/If) ──
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
                span { class: "action-expand-placeholder" }
            }
            span { class: "{icon_class}", "{icon}" }
            span { class: "action-name", "{props.action.name}" }
            span { class: "action-duration", "{dur_label}" }
            div { class: "timing-bar-bg",
                div { class: "{bar_class}", style: "width:{pct:.0}%" }
            }
            // ── detail toggle: raw input/output JSON ──────────────────
            button {
                class: "btn-icon action-detail-btn",
                title: if *detail_open.read() { "Hide detail" } else { "Show input / output" },
                onclick: {
                    let wf   = props.workflow.clone();
                    let rid  = props.run_id.clone();
                    let name = props.action.name.clone();
                    move |_| {
                        if *detail_open.read() {
                            detail_open.set(false);
                        } else if detail_json.read().is_empty() {
                            detail_loading.set(true);
                            let wf2   = wf.clone();
                            let rid2  = rid.clone();
                            let name2 = name.clone();
                            spawn(async move {
                                let text = match workflows::get_action_detail(&wf2, &rid2, &name2).await {
                                    Ok(v)  => serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string()),
                                    Err(e) => format!("Error fetching detail: {}", e),
                                };
                                detail_json.set(text);
                                detail_loading.set(false);
                                detail_open.set(true);
                            });
                        } else {
                            detail_open.set(true);
                        }
                    }
                },
                if *detail_loading.read() { "…" }
                else if *detail_open.read() { "▲" }
                else { "⋯" }
            }
        }
        if let Some(msg) = error_msg {
            div { class: "{err_class}", style: "padding-left:{indent_px}px", "{msg}" }
        }
        if *detail_open.read() {
            pre {
                class: "action-detail-pre",
                style: "padding-left:{indent_px}px",
                "{detail_json.read()}"
            }
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
