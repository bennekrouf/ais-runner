//! Run-detail view: per-workflow tabs (Run / Logs / Source) with the run
//! timeline. Split by concern:
//! - [`run_block`] — one collapsible block per run (status, storage events, actions)
//! - [`action_row`] — one row per action (durations, expansion, error detail)
//! - [`storage_events`] — Azurite debug.log → per-request storage summaries
//! - [`error_extract`] — digging real error messages out of API details and host logs
//! - [`sql_chips`] — SQL stored-procedure chips + missing-object hints

mod action_row;
mod error_extract;
mod run_block;
mod sql_chips;
mod storage_events;

use run_block::RunBlock;
use error_extract::build_action_error_map;
use sql_chips::SqlSprocChip;

use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};
use crate::services::workflows::{ActionItem, RunItem, duration_ms};
use crate::services::workflow_analysis::{WorkflowAnalysis, TriggerKind};
use crate::services::workflow_outline::{self, SectionKind};
use crate::services::process::ServiceState;
use crate::components::log_panel::LogLine;
use crate::components::tooltip::Tooltip;

use crate::utils::open_in_editor;

#[derive(Props, Clone, PartialEq)]
pub struct RunDetailProps {
    pub workflow:           Option<String>,
    pub source_text:        Signal<String>,
    pub runs:               Vec<RunItem>,
    pub actions:            Vec<ActionItem>,
    pub is_live:            bool,
    pub active_tab:         Signal<String>,
    pub health_error:       Option<String>,
    pub logs:               Vec<LogLine>,
    /// Live Azurite debug.log buffer (owned by main_screen). Used per-run to
    /// surface storage events that happened during each run's time window.
    pub az_lines:           Signal<Vec<String>>,
    pub analysis:           WorkflowAnalysis,
    /// sproc qualified name → Some(true|false) once probed, None while loading.
    pub sproc_status:       Signal<HashMap<String, Option<bool>>>,
    pub source_path:        Option<String>,
    pub suggested_payload:  String,
    pub on_run:             EventHandler<()>,
    pub on_refresh:         EventHandler<()>,
    pub on_clear_runs:      EventHandler<()>,
    pub on_select_run:      EventHandler<String>,
    /// True when Azurite + Functions host are both running, so workflows can be listed.
    pub services_ready:     bool,
    /// True while either Azurite or the Functions host is in the process of starting.
    pub services_starting:  bool,
    /// Number of workflows currently in the discovered list. When services
    /// are running but this is zero, the list is still being scanned —
    /// surface a spinner instead of the "Select a workflow…" prompt.
    pub workflow_count:     usize,
    /// Per-service state, so the empty-state hint can embed inline action
    /// buttons that start each service directly from the message.
    pub azurite_state:      ServiceState,
    pub func_state:         ServiceState,
    pub on_start_azurite:   EventHandler<()>,
    pub on_start_func:      EventHandler<()>,
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

    // Tracks which run blocks are currently collapsed (by run id).
    let mut collapsed_runs = use_signal(HashSet::<String>::new);
    // Per-run snapshot of the storage-events strip. The live az_lines buffer
    // is a rolling 500-line tail that clears on log rotation, and a finished
    // run's end-time window can exclude late-flushed lines — both make the
    // strip vanish after the run. Once a run has shown events, keep them here
    // (keyed by run id) so the user can still see them when investigating.
    let storage_events_cache: Signal<HashMap<String, Vec<String>>> =
        use_signal(HashMap::new);
    // "Failures only" toggle on the Run tab. When on, each run block hides
    // every action except the first one with a failed/timedout/cancelled
    // status — which is the action that actually caused the run to fail.
    // Downstream-skipped actions are noise in a failure investigation, so
    // collapsing them surfaces the real culprit in a single glance.
    let mut failures_only = use_signal(|| false);

    // ── Payload popover ────────────────────────────────────────────────────
    let mut payload_open   = use_signal(|| false);
    let mut copied         = use_signal(|| false);
    let suggested          = props.suggested_payload.clone();

    // ── Source tab actions ─────────────────────────────────────────────────
    let mut source_copied  = use_signal(|| false);
    let mut opening        = use_signal(|| false);
    let mut source_hl      = use_signal(String::new);
    let source_text        = props.source_text;

    // Pre-derive per-service state booleans so the inline service buttons in
    // the empty-state hint can show the right icon (▶ idle / ⟳ starting /
    // ✓ running) without repeating the match in both call sites.
    let az_state   = props.azurite_state.clone();
    let func_state = props.func_state.clone();
    let on_start_az   = props.on_start_azurite;
    let on_start_func = props.on_start_func;

    // Normalise the source to a stable pretty-printed form before doing
    // anything else with it. Workflows on disk arrive in whatever formatting
    // the author / IDE used (tabs, 4-space, minified, single line) — and
    // the outline's line-range pass relies on every key sitting at the start
    // of its own line in a 2-space-indented document. We feed the same
    // pretty text to both the highlighter and the outline builder, so the
    // scroll-sync line numbers always match what the user sees in the pane.
    let pretty_source = use_memo(move || {
        let raw = source_text.read().clone();
        match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(v)  => serde_json::to_string_pretty(&v).unwrap_or(raw),
            Err(_) => raw, // not JSON (e.g. read error fallback) — leave as-is
        }
    });

    // ── Workflow outline (generic, schema-driven section summary) ──────────
    // Re-derived whenever the source changes; cheap (single regex-free pass).
    let outline = use_memo(move || workflow_outline::build_outline(&pretty_source.read()));
    // Which outline section is currently in view (driven by scroll position).
    let mut active_section = use_signal(|| 0usize);
    // Section indexes the user has collapsed in the rail. Persists for the
    // life of the component — re-expanding a parent is one click away and
    // the tree is small, so we don't need to persist across workflow loads.
    let mut collapsed = use_signal(std::collections::HashSet::<usize>::new);

    // Inject the IntersectionObserver once the source HTML is in place, so the
    // active section follows the user's scroll. Re-runs when the outline or the
    // highlighted HTML changes (length-of-outline is enough to invalidate).
    use_effect(move || {
        let _ = source_hl.read(); // re-run when HTML is replaced
        let sections = outline.read().clone();
        if sections.is_empty() { return; }
        let starts: Vec<u32> = sections.iter()
            .map(|s| s.start_line.unwrap_or(0))
            .collect();
        let starts_json = serde_json::to_string(&starts).unwrap_or_else(|_| "[]".to_string());
        let script = format!(r#"
(function() {{
  var pre = document.getElementById('source-pre');
  if (!pre) return;
  // The highlighted HTML is one logical text block — to map scroll → line we
  // measure the pre's scrollTop against the line height. Cheaper and more
  // robust than wrapping every line in a span.
  var starts = {starts_json};
  if (!starts.length) return;

  function lineHeight() {{
    var s = getComputedStyle(pre);
    var lh = parseFloat(s.lineHeight);
    if (isNaN(lh) || lh <= 0) lh = parseFloat(s.fontSize) * 1.4;
    return lh || 18;
  }}

  function activeIdx() {{
    var lh = lineHeight();
    // Treat the line that's ~25% from the top of the viewport as "current"
    // so the user sees the section name align with what they're reading.
    var anchorLine = Math.floor((pre.scrollTop + pre.clientHeight * 0.25) / lh) + 1;
    var idx = 0;
    for (var i = 0; i < starts.length; i++) {{
      if (starts[i] > 0 && starts[i] <= anchorLine) idx = i;
    }}
    return idx;
  }}

  var last = -1;
  function tick() {{
    var i = activeIdx();
    if (i !== last) {{
      last = i;
      dioxus.send(i);
    }}
  }}
  pre.removeEventListener('scroll', pre.__outlineHandler || (function(){{}}));
  pre.__outlineHandler = tick;
  pre.addEventListener('scroll', tick, {{ passive: true }});
  // Initial fire so the first section is highlighted before any scroll.
  setTimeout(tick, 0);
}})();
"#);
        spawn(async move {
            let mut eval = document::eval(&script);
            while let Ok(val) = eval.recv::<serde_json::Value>().await {
                if let Some(i) = val.as_u64() {
                    active_section.set(i as usize);
                }
            }
        });
    });

    // Wire the outline-rail resize handle. Runs whenever the outline is
    // (re)mounted; idempotent — re-running re-attaches handlers cleanly.
    use_effect(move || {
        let _ = outline.read(); // re-run when outline materialises
        let script = r#"
(function() {
  var handle = document.getElementById('outline-resize');
  var rail   = document.querySelector('.outline-rail');
  if (!handle || !rail) return;

  // Restore persisted width on every (re-)render so navigating between
  // workflows keeps the user's preferred layout.
  var saved = parseInt(localStorage.getItem('outline-rail-width') || '0', 10);
  if (saved >= 120 && saved <= 600) rail.style.width = saved + 'px';

  if (handle.__wired) return;
  handle.__wired = true;

  var dragging = false;
  var startX = 0, startW = 0;
  handle.addEventListener('mousedown', function(e) {
    dragging = true;
    startX = e.clientX;
    startW = rail.getBoundingClientRect().width;
    document.body.style.cursor = 'col-resize';
    document.body.style.userSelect = 'none';
    e.preventDefault();
  });
  window.addEventListener('mousemove', function(e) {
    if (!dragging) return;
    var w = Math.max(140, Math.min(600, startW + (e.clientX - startX)));
    rail.style.width = w + 'px';
  });
  window.addEventListener('mouseup', function() {
    if (!dragging) return;
    dragging = false;
    document.body.style.cursor = '';
    document.body.style.userSelect = '';
    var w = parseInt(rail.style.width, 10);
    if (!isNaN(w)) localStorage.setItem('outline-rail-width', String(w));
  });
})();
"#;
        spawn(async move {
            let _ = document::eval(script);
        });
    });

    // Keep the active rail row in view. When the user scrolls through a long
    // workflow, the section that becomes active may sit outside the rail's
    // visible window — without this the highlight is technically correct but
    // invisible, which feels broken. `scrollIntoView({block:"nearest"})` is
    // a no-op when the row is already visible, so we don't fight short rails.
    use_effect(move || {
        let _ = active_section.read(); // re-run on transition
        let script = r#"
(function() {
  var rail = document.querySelector('.outline-rail');
  if (!rail) return;
  var el = rail.querySelector('.outline-item.active');
  if (!el) return;
  // Use the row wrapper as the scroll target so the chevron column is
  // included in the visibility calculation.
  var target = el.closest('.outline-row') || el;
  // 'nearest' = only scroll if needed. 'smooth' for a gentle follow.
  target.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
})();
"#;
        spawn(async move {
            let _ = document::eval(script);
        });
    });

    // Click on a rail item → scroll the source pane to that line.
    let scroll_to_line = move |line: u32| {
        let script = format!(r#"
(function() {{
  var pre = document.getElementById('source-pre');
  if (!pre) return;
  var s = getComputedStyle(pre);
  var lh = parseFloat(s.lineHeight) || (parseFloat(s.fontSize) * 1.4) || 18;
  // Leave a few lines of headroom so the section header isn't flush against
  // the top of the viewport.
  pre.scrollTo({{ top: Math.max(0, ({line} - 2) * lh), behavior: 'smooth' }});
}})();
"#);
        spawn(async move {
            let _ = document::eval(&script);
        });
    };

    // JSON syntax highlighting — re-runs whenever source_text changes.
    // We feed the *pretty* source to the highlighter so the rendered <pre>
    // line layout matches the outline's line-range pass, keeping the
    // scroll-sync highlight aligned with the section under the cursor.
    use_effect(move || {
        let raw = pretty_source.read().clone();
        if raw.is_empty() { source_hl.set(String::new()); return; }
        let raw_json = serde_json::to_string(&raw).unwrap_or_default();
        let script = format!(r#"
(function() {{
    var raw = {raw_json};
    function doHighlight() {{
        var tmp = document.createElement('code');
        tmp.textContent = raw;
        tmp.className = 'language-json';
        hljs.highlightElement(tmp);
        dioxus.send(tmp.innerHTML);
    }}
    var isDark = !document.body.classList.contains('light');
    var theme  = isDark ? 'github-dark' : 'github';
    var wantHref = 'https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.9.0/styles/' + theme + '.min.css';
    var cssEl = document.getElementById('hljs-css');
    if (!cssEl) {{
        cssEl = document.createElement('link');
        cssEl.id = 'hljs-css'; cssEl.rel = 'stylesheet';
        document.head.appendChild(cssEl);
    }}
    if (cssEl.href !== wantHref) cssEl.href = wantHref;
    if (typeof hljs !== 'undefined') {{
        doHighlight();
    }} else {{
        var s = document.createElement('script');
        s.src = 'https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.9.0/highlight.min.js';
        s.onload = doHighlight;
        document.head.appendChild(s);
    }}
}})();
"#);
        spawn(async move {
            let mut eval = document::eval(&script);
            if let Ok(val) = eval.recv().await {
                let html = match &val {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                source_hl.set(html);
            }
        });
    });

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
                    {
                        let on = *failures_only.read();
                        let cls = if on { "btn btn-small btn-toggle on" } else { "btn btn-small btn-toggle" };
                        let tip = if on {
                            "Showing only the first failed action per run — click to show every action."
                        } else {
                            "Hide downstream-skipped noise: show only the first failed action per run."
                        };
                        rsx! {
                            Tooltip { text: "{tip}", direction: "bottom",
                                button {
                                    class: "{cls}",
                                    disabled: *active_tab.read() != "Run",
                                    onclick: move |_| failures_only.set(!on),
                                    if on { "● Failures only" } else { "○ Failures only" }
                                }
                            }
                        }
                    }
                    {
                        let run_ids: Vec<String> = props.runs.iter().map(|r| r.name.clone()).collect();
                        let all_collapsed = !run_ids.is_empty()
                            && run_ids.iter().all(|id| collapsed_runs.read().contains(id));
                        let (label, tip) = if all_collapsed {
                            ("⊞ Expand all", "Expand all run blocks")
                        } else {
                            ("⊟ Collapse all", "Collapse all run blocks")
                        };
                        rsx! {
                            Tooltip { text: "{tip}", direction: "bottom",
                                button {
                                    class: "btn btn-run btn-small",
                                    disabled: run_ids.is_empty() || *active_tab.read() != "Run",
                                    onclick: move |_| {
                                        if all_collapsed {
                                            collapsed_runs.write().clear();
                                        } else {
                                            let mut set = collapsed_runs.write();
                                            for id in &run_ids { set.insert(id.clone()); }
                                        }
                                    },
                                    "{label}"
                                }
                            }
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
                            // sql stored procedures
                            for sp in &a.sql_sprocs {
                                SqlSprocChip {
                                    name:   sp.name.clone(),
                                    params: sp.params.clone(),
                                    status: props.sproc_status.read().get(&sp.name).copied().unwrap_or(None),
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
                    {
                        // Pre-scan the workflow-filtered logs once per render
                        // to build `action_name → error message`. ActionRow
                        // reads from this when the management API returns
                        // NotSpecified — the actual ParseJson/Compose error
                        // string only appears in the Functions host stdout
                        // that we've already captured here.
                        let action_log_errors = build_action_error_map(&props.logs);
                        rsx! {
                            for run in props.runs.clone() {
                                RunBlock {
                                    run: run,
                                    actions: props.actions.clone(),
                                    max_ms: max_ms,
                                    is_live: props.is_live,
                                    workflow: workflow_name.clone(),
                                    on_select_run: props.on_select_run.clone(),
                                    collapsed_runs: collapsed_runs,
                                    az_lines:      props.az_lines,
                                    events_cache:  storage_events_cache,
                                    failures_only: *failures_only.read(),
                                    action_log_errors: action_log_errors.clone(),
                                }
                            }
                        }
                    }
                }
            } else if *active_tab.read() == "Logs" {
                div { id: "status-view",
                    if props.workflow.is_none() {
                        div { class: "empty-state",
                            {
                                let az = az_state.clone();
                                let fu = func_state.clone();
                                render_service_hint(&az, &fu, on_start_az, on_start_func, props.services_ready, props.workflow_count, "logs")
                            }
                        }
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
                        div { class: "empty-state",
                            {
                                let az = az_state.clone();
                                let fu = func_state.clone();
                                render_service_hint(&az, &fu, on_start_az, on_start_func, props.services_ready, props.workflow_count, "source")
                            }
                        }
                    } else {
                        div { class: "source-wrap",
                            // "Copied!" toast
                            if *source_copied.read() {
                                div {
                                    style: "position:absolute; top:8px; left:50%; transform:translateX(-50%); \
                                            background:#238636; color:#fff; padding:4px 14px; border-radius:6px; \
                                            font-size:12px; font-weight:600; pointer-events:none; z-index:20; \
                                            box-shadow:0 2px 8px rgba(0,0,0,0.3);",
                                    "✅ Copied!"
                                }
                            }
                            div { class: "source-toolbar",
                            button {
                                class: "source-copy-btn",
                                title: "Copy to clipboard",
                                disabled: *source_copied.read(),
                                onclick: {
                                    let text = source_text.read().clone();
                                    move |_| {
                                        let text = text.clone();
                                        source_copied.set(true);
                                        spawn(async move {
                                            tokio::task::spawn_blocking(move || {
                                                if let Ok(mut cb) = arboard::Clipboard::new() {
                                                    let _ = cb.set_text(text);
                                                }
                                            }).await.ok();
                                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                            source_copied.set(false);
                                        });
                                    }
                                },
                                if *source_copied.read() { "✅" } else { "⎘" }
                            }
                            if let Some(path) = props.source_path.clone() {
                                button {
                                    class: "source-copy-btn",
                                    title: "Open in editor",
                                    disabled: *opening.read(),
                                    onclick: move |_| {
                                        let p = path.clone();
                                        opening.set(true);
                                        std::thread::spawn(move || { open_in_editor(&p); });
                                        spawn(async move {
                                            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                                            opening.set(false);
                                        });
                                    },
                                    if *opening.read() { "⏳" } else { "✎" }
                                }
                            }
                            } // source-toolbar
                            div { class: "source-body",
                                // ── Outline rail (generic section summary, hierarchical) ──
                                {
                                    let sections = outline.read().clone();
                                    if sections.is_empty() {
                                        rsx! {}
                                    } else {
                                        let active   = *active_section.read();
                                        let collapsed_snap = collapsed.read().clone();
                                        rsx! {
                                            nav { class: "outline-rail",
                                                div { class: "outline-rail-header", "Outline" }
                                                for (i, s) in sections.iter().enumerate() {
                                                    {
                                                        // Skip rows whose ancestor chain is collapsed.
                                                        if workflow_outline::is_under_collapsed(&sections, i, &collapsed_snap) {
                                                            rsx! {}
                                                        } else {
                                                            let line = s.start_line.unwrap_or(1);
                                                            let icon = match &s.kind {
                                                                SectionKind::Trigger        => "⚡",
                                                                SectionKind::Container(t) => match t.as_str() {
                                                                    "Scope"   => "▦",
                                                                    "If"      => "◆",
                                                                    "Switch"  => "◇",
                                                                    "Foreach" => "↻",
                                                                    "Until"   => "⟲",
                                                                    "Try"     => "⌖",
                                                                    "Case"    => "▸",
                                                                    _          => "▣",
                                                                },
                                                                SectionKind::Steps          => "›",
                                                            };
                                                            let cls = if i == active { "outline-item active" } else { "outline-item" };
                                                            let label = s.label.clone();
                                                            let hint  = s.hint.clone();
                                                            let scroll = scroll_to_line;
                                                            let indent_px = 4 + (s.depth as i32) * 14;
                                                            let style = format!("padding-left: {indent_px}px");
                                                            let has_kids = workflow_outline::has_children(&sections, i);
                                                            let is_collapsed = collapsed_snap.contains(&i);
                                                            // Chevron toggle for collapsible rows. We stop
                                                            // propagation by handling the click here and not
                                                            // also calling `scroll` — clicking the chevron
                                                            // is a "manage the tree" gesture, distinct from
                                                            // "jump to this section" on the label.
                                                            let chevron_idx = i;
                                                            rsx! {
                                                                div { class: "outline-row", style: "{style}",
                                                                    if has_kids {
                                                                        button {
                                                                            class: "outline-chevron",
                                                                            title: if is_collapsed { "Expand" } else { "Collapse" },
                                                                            onclick: move |_| {
                                                                                let mut set = collapsed.write();
                                                                                if set.contains(&chevron_idx) {
                                                                                    set.remove(&chevron_idx);
                                                                                } else {
                                                                                    set.insert(chevron_idx);
                                                                                }
                                                                            },
                                                                            if is_collapsed { "▸" } else { "▾" }
                                                                        }
                                                                    } else {
                                                                        span { class: "outline-chevron outline-chevron-placeholder" }
                                                                    }
                                                                    button {
                                                                        class: "{cls}",
                                                                        title: "{hint}",
                                                                        onclick: move |_| scroll(line),
                                                                        span { class: "outline-icon", "{icon}" }
                                                                        span { class: "outline-text",
                                                                            div { class: "outline-label-row",
                                                                                span { class: "outline-label", "{label}" }
                                                                                if !s.tags.is_empty() {
                                                                                    span { class: "outline-tags",
                                                                                        for tag in s.tags.iter() {
                                                                                            {
                                                                                                let slug = tag.css_slug();
                                                                                                let cls  = format!("chip chip-{slug}");
                                                                                                let lbl  = tag.chip_label();
                                                                                                rsx! { span { class: "{cls}", "{lbl}" } }
                                                                                            }
                                                                                        }
                                                                                    }
                                                                                }
                                                                            }
                                                                            div { class: "outline-hint",  "{hint}" }
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
                                }
                                // Drag handle — the user can drag this to widen or
                                // narrow the outline rail. Initialised once by JS
                                // (see use_effect below). When the outline is empty
                                // the handle is hidden so the source spans the full width.
                                if !outline.read().is_empty() {
                                    div { id: "outline-resize", title: "Drag to resize outline" }
                                }
                                pre { id: "source-pre", dangerous_inner_html: "{source_hl.read()}" }
                            }
                        }
                    }
                }
            }

        }
    }
}



/// Empty-state hint with inline service buttons.
///
/// Renders the right sentence for the current readiness state and, when one
/// or both services are stopped, embeds them as clickable buttons exactly
/// where the words "Azurite" / "Functions host" used to sit — so the user
/// can start a service in one click without scrolling up to the toolbar.
///
/// `tab` is "logs" or "source" — only used to vary the trailing fragment
/// ("view its source." vs "view its logs.") so the message reads naturally.
fn render_service_hint(
    azurite_state:   &ServiceState,
    func_state:      &ServiceState,
    on_start_az:     EventHandler<()>,
    on_start_func:   EventHandler<()>,
    services_ready:  bool,
    workflow_count:  usize,
    tab:             &'static str,
) -> Element {
    // Services up: either the workflow list has loaded (prompt user to pick
    // one) or the scan is still running (show a spinner — far more obvious
    // than a static sentence that looks like an error).
    if services_ready {
        if workflow_count == 0 {
            return rsx! {
                span { class: "az-spinner", "⟳" }
                " Loading workflows…"
            };
        }
        let text = if tab == "logs" {
            "Select a workflow to view its logs."
        } else {
            "Select a workflow to view its source."
        };
        return rsx! { "{text}" };
    }

    // Per-service button factory — picks label + class + disabled state
    // from the live service state and lets the caller wire the click.
    fn svc_button(
        state:    &ServiceState,
        name:     &'static str,
        on_start: EventHandler<()>,
    ) -> Element {
        match state {
            ServiceState::Running => rsx! {
                span { class: "inline-svc inline-svc-running",
                    span { class: "inline-svc-icon", "✓" }
                    "{name}"
                }
            },
            ServiceState::Starting => rsx! {
                span { class: "inline-svc inline-svc-starting",
                    span { class: "az-spinner inline-svc-icon", "⟳" }
                    "{name}"
                }
            },
            ServiceState::Stopped => rsx! {
                button {
                    class: "inline-svc inline-svc-stopped",
                    title: "Start {name}",
                    onclick: move |_| on_start.call(()),
                    span { class: "inline-svc-icon", "▶" }
                    "{name}"
                }
            },
        }
    }

    rsx! {
        span { class: "service-hint",
            "Start "
            { svc_button(azurite_state, "Azurite", on_start_az) }
            " and the "
            { svc_button(func_state, "Functions", on_start_func) }
            " host to load workflows."
        }
    }
}
