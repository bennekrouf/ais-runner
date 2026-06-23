use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};
use crate::services::workflows::{self, ActionItem, RunItem, duration_ms};
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

// ── Sub-component per run ──────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct RunBlockProps {
    run:            RunItem,
    actions:        Vec<ActionItem>,
    max_ms:         i64,
    is_live:        bool,
    workflow:       String,
    on_select_run:  EventHandler<String>,
    collapsed_runs: Signal<HashSet<String>>,
    /// When true, the action list is filtered to just the first action whose
    /// status indicates failure (failed / timedout / cancelled). Lets the
    /// user pin down the culprit in a long action list without skipping past
    /// downstream "skipped" noise.
    failures_only:  bool,
    /// Pre-computed `action_name → error message` map, scraped from the
    /// Functions host stdout that this run produced. Used as the fallback
    /// when the Logic Apps management API returns NotSpecified — the real
    /// ParseJson / Compose / expression-failure text only appears in stdout.
    action_log_errors: HashMap<String, String>,
}

#[component]
fn RunBlock(props: RunBlockProps) -> Element {
    let status_lower = props.run.properties.status.to_lowercase();
    let status_class = format!("run-status {}", status_lower);
    let run_id  = props.run.name.clone();
    let run_id2 = run_id.clone();
    let run_id3 = run_id.clone();

    let mut collapsed_runs = props.collapsed_runs;
    let is_collapsed = collapsed_runs.read().contains(&run_id);

    rsx! {
        div { class: "run-block",
            div {
                class: "run-header",
                style: "cursor:pointer",
                onclick: move |_| {
                    let now_collapsed = {
                        let mut set = collapsed_runs.write();
                        if set.contains(&run_id3) {
                            set.remove(&run_id3);
                            false
                        } else {
                            set.insert(run_id3.clone());
                            true
                        }
                    };
                    if !now_collapsed {
                        props.on_select_run.call(run_id2.clone());
                    }
                },
                span {
                    class: "run-collapse-chevron",
                    style: "display:inline-block;width:14px;text-align:center;margin-right:4px;font-size:10px;",
                    if is_collapsed { "▶" } else { "▼" }
                }
                span { class: "{status_class}", "{props.run.properties.status}" }
                span { "{run_id}" }
                if let Some(start) = &props.run.properties.start_time {
                    {
                        // Render the run's start time in the user's local zone.
                        // Logic Apps always reports UTC ("…Z") — without the
                        // conversion the user sees a wall-clock that's offset
                        // by their TZ and assumes the run hasn't happened yet.
                        let label = crate::utils::fmt_utc_as_local(start);
                        rsx! { span { style: "margin-left:auto", "{label}" } }
                    }
                }
            }
            if !is_collapsed {
                {
                    // Apply the "Failures only" filter: keep just the first
                    // action whose status indicates a real failure (not
                    // "skipped" — that's downstream noise we want to hide).
                    // The Logic Apps action listing is execution-ordered, so
                    // the first failure IS the root cause of the run failing.
                    let visible: Vec<ActionItem> = if props.failures_only {
                        props.actions.iter()
                            .find(|a| matches!(
                                a.properties.status.to_lowercase().as_str(),
                                "failed" | "timedout" | "cancelled"
                            ))
                            .cloned().into_iter().collect()
                    } else {
                        props.actions.clone()
                    };
                    if props.failures_only && visible.is_empty() {
                        rsx! {
                            div { class: "failures-only-empty",
                                "No failed action in this run."
                            }
                        }
                    } else {
                        rsx! {
                            for action in visible {
                                {
                                    let log_err = props.action_log_errors.get(&action.name).cloned();
                                    rsx! {
                                        ActionRow {
                                            action: action,
                                            max_ms: props.max_ms,
                                            is_live: props.is_live,
                                            workflow: props.workflow.clone(),
                                            run_id: run_id.clone(),
                                            depth: 0,
                                            log_error: log_err,
                                            action_log_errors: props.action_log_errors.clone(),
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

// ── Failed-action message extraction ──────────────────────────────────────
//
// The action listing endpoint omits `properties.error` for some failure
// modes — most notably ParseJson, where the runtime stuffs the actual
// schema-mismatch message into the outputs blob instead. We probe a handful
// of well-known paths in the expanded action detail and return the first
// non-empty string we find. Order is deliberate: top-level error first,
// then outputs.body.error (the ParseJson shape), then any nested message.
fn extract_error_from_detail(v: &serde_json::Value) -> Option<String> {
    // Paths checked, in priority order, against the detail object returned
    // by `get_action_detail` (which has already inlined inputs/outputs).
    const PATHS: &[&str] = &[
        "/properties/error/message",
        "/properties/outputs/body/error/message",
        "/properties/outputs/body/message",
        "/properties/outputs/error/message",
        "/properties/outputs/message",
        "/error/message",
    ];
    for p in PATHS {
        if let Some(s) = v.pointer(p).and_then(|x| x.as_str()) {
            let s = s.trim();
            if !s.is_empty() { return Some(s.to_string()); }
        }
    }
    // Fallback: if outputs.body is itself a JSON-encoded string from a
    // ParseJson failure, the message may be the body text directly.
    if let Some(s) = v.pointer("/properties/outputs/body").and_then(|x| x.as_str()) {
        let s = s.trim();
        if !s.is_empty() { return Some(s.to_string()); }
    }
    // Last resort: surface the error code so the user at least sees *what*
    // kind of failure it was rather than a silent red row. The runtime puts
    // the code at one of two paths depending on the action type — Foreach
    // and Scope put it at properties.code, leaf actions at properties.error.code.
    let code = v.pointer("/properties/error/code")
        .or_else(|| v.pointer("/properties/code"))
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    code.map(|c| {
        // "NotSpecified" by itself is unhelpful, but it means different things
        // depending on the action type — Logic Apps stamps it on scope-type
        // actions (Foreach / Scope / Until / If) when a child action failed,
        // and on expression-evaluation actions like ParseJson / Compose /
        // Set variable when schema validation or template parsing fails.
        // For the latter the runtime intentionally does NOT attach the
        // message to the action record — it goes to the Functions host
        // stdout instead (known Logic Apps Standard limitation). Tell the
        // user where to look so they don't think this is an ais-runner bug.
        let atype = v.pointer("/properties/type")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        let is_scope = matches!(atype, "Foreach" | "Scope" | "Until" | "If");
        let is_expr  = matches!(atype, "ParseJson" | "Compose" | "InitializeVariable"
                              | "SetVariable" | "AppendToStringVariable" | "AppendToArrayVariable"
                              | "IncrementVariable" | "DecrementVariable");
        if c.eq_ignore_ascii_case("NotSpecified") && is_scope {
            format!("{c} — a child action failed; expand to see which.")
        } else if c.eq_ignore_ascii_case("NotSpecified") && is_expr {
            format!(
                "{c} — Logic Apps Standard does not expose {atype} errors via API. \
                 Check the func start console (Logs → console) for the schema/expression message."
            )
        } else if c.eq_ignore_ascii_case("NotSpecified") {
            format!(
                "{c} — runtime did not attach a message. \
                 Check the func start console for action-level errors."
            )
        } else {
            c.to_string()
        }
    })
}

#[cfg(test)]
mod extract_error_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn picks_top_level_error_message_first() {
        let v = json!({
            "properties": {
                "error": { "message": "top-level", "code": "X" },
                "outputs": { "body": { "error": { "message": "deeper" } } },
            }
        });
        assert_eq!(extract_error_from_detail(&v).as_deref(), Some("top-level"));
    }

    #[test]
    fn falls_through_to_outputs_body_error_for_parsejson_shape() {
        let v = json!({
            "properties": {
                "outputs": {
                    "body": { "error": { "message": "Invalid type. Expected Integer but got String." } }
                }
            }
        });
        assert_eq!(
            extract_error_from_detail(&v).as_deref(),
            Some("Invalid type. Expected Integer but got String."),
        );
    }

    #[test]
    fn falls_back_to_code_when_no_message_found() {
        let v = json!({ "properties": { "error": { "code": "BadRequest" } } });
        assert_eq!(extract_error_from_detail(&v).as_deref(), Some("BadRequest"));
    }

    #[test]
    fn falls_back_to_top_level_properties_code() {
        // For_each_page shape: code is at properties.code, no error object.
        let v = json!({ "properties": { "status": "Failed", "code": "ActionFailed" } });
        assert_eq!(extract_error_from_detail(&v).as_deref(), Some("ActionFailed"));
    }

    #[test]
    fn notspecified_on_scope_is_annotated_with_child_hint() {
        let v = json!({
            "properties": { "status": "Failed", "code": "NotSpecified", "type": "Foreach" }
        });
        let msg = extract_error_from_detail(&v).unwrap();
        assert!(msg.contains("NotSpecified"));
        assert!(msg.contains("child"), "expected child-action hint, got: {msg}");
    }

    #[test]
    fn notspecified_on_parsejson_points_at_func_console() {
        let v = json!({
            "properties": { "status": "Failed", "code": "NotSpecified", "type": "ParseJson" }
        });
        let msg = extract_error_from_detail(&v).unwrap();
        assert!(msg.contains("ParseJson"), "expected action type in hint, got: {msg}");
        assert!(msg.contains("func start") || msg.contains("console"),
                "expected console hint, got: {msg}");
    }

    #[test]
    fn returns_none_when_no_error_information_present() {
        let v = json!({ "properties": { "status": "Succeeded" } });
        assert_eq!(extract_error_from_detail(&v), None);
    }
}

// ── Log-derived action error extraction ───────────────────────────────────
//
// Logic Apps Standard does not write ParseJson / Compose / expression-evaluation
// failures to the management API — they only appear in the Functions host
// stdout. We've already captured that stdout in `log_lines`; this builds a
// per-action lookup so the action row can show the real error inline.
//
// Heuristic: walk the workflow-filtered log lines (already pre-filtered in
// RunDetail to those mentioning the workflow name), find ones that mention
// any action name *and* an error keyword, and keep the most recent match per
// action. Tolerant by design — every Logic Apps runtime version phrases the
// failure line slightly differently ("Action 'X' failed:", "action='X' …
// Exception:", schema-validation lines that name the action somewhere in the
// middle of the message, etc.).
fn build_action_error_map(logs: &[LogLine]) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    // Walk newest first so the latest matching line wins per action. The
    // earlier lines are the "starting…" / "evaluating…" traces we don't want.
    for line in logs.iter().rev() {
        if let Some((action, msg)) = parse_action_error_line(&line.msg) {
            out.entry(action).or_insert(msg);
        }
    }
    out
}

/// Try to pull out (action_name, error_message) from a single log line.
/// Returns `None` if the line doesn't look like an action-level failure.
fn parse_action_error_line(line: &str) -> Option<(String, String)> {
    let lower = line.to_lowercase();
    // Filter: must mention an error/failure keyword. Pure "info" lines about
    // an action's success don't make it into the map.
    let has_err_keyword = lower.contains("failed")
        || lower.contains("error")
        || lower.contains("exception")
        || lower.contains("invalid")
        || lower.contains("schema validation");
    if !has_err_keyword { return None; }

    // Most Logic Apps runtime error lines name the action in single quotes,
    // e.g. "Action 'Restrictive_Parse_JSON' failed: ..." or
    // "action 'X' status 'Failed'. Exception: ...". Use the single-quoted
    // token immediately after the word "action" as a strong signal.
    let action = extract_quoted_after(line, "action");
    let action = action.or_else(|| extract_quoted_after(line, "Action"));
    let action = action?;

    // Trim known fluff to surface the bit the user actually cares about.
    let msg = clean_action_error_message(line, &action);
    if msg.trim().is_empty() { return None; }
    Some((action, msg))
}

/// Find the first single-quoted substring that follows a marker word.
/// Returns the inner text (without the quotes). Case-sensitive on the marker.
fn extract_quoted_after(line: &str, marker: &str) -> Option<String> {
    let idx = line.find(marker)?;
    let tail = &line[idx + marker.len()..];
    let q1 = tail.find('\'')?;
    let after_q1 = &tail[q1 + 1..];
    let q2 = after_q1.find('\'')?;
    let inner = &after_q1[..q2];
    if inner.is_empty() { None } else { Some(inner.to_string()) }
}

/// Cut the action-error line down to the part the user cares about: take
/// everything after "<action>'" so the leading "Action 'X'" boilerplate is
/// dropped. If no obvious cut point exists, return the raw line.
fn clean_action_error_message(line: &str, action: &str) -> String {
    let needle = format!("'{}'", action);
    if let Some(i) = line.find(&needle) {
        let after = &line[i + needle.len()..];
        // Strip leading punctuation/spaces — ":" / "." / " " / ","
        let trimmed = after.trim_start_matches(|c: char|
            c.is_whitespace() || c == ':' || c == '.' || c == ',' || c == '-'
        );
        if !trimmed.is_empty() { return trimmed.to_string(); }
    }
    line.trim().to_string()
}

#[cfg(test)]
mod log_scrape_tests {
    use super::*;
    use crate::components::log_panel::LogLevel;

    fn mk(msg: &str) -> LogLine {
        LogLine { time: "00:00:00".into(), msg: msg.into(), level: LogLevel::Error }
    }

    #[test]
    fn picks_parsejson_validation_message_from_runtime_log() {
        let logs = vec![mk(
            "[2026-06-19T20:31:02Z] Action 'Restrictive_Parse_JSON' failed: \
             Invalid type. Expected Integer but got String at #/properties/age",
        )];
        let m = build_action_error_map(&logs);
        let v = m.get("Restrictive_Parse_JSON").unwrap();
        assert!(v.contains("Invalid type"), "got: {v}");
        assert!(v.contains("#/properties/age"), "got: {v}");
    }

    #[test]
    fn ignores_success_lines_even_if_they_name_the_action() {
        let logs = vec![mk("Action 'X' completed successfully")];
        assert!(build_action_error_map(&logs).is_empty());
    }

    #[test]
    fn most_recent_line_per_action_wins() {
        let logs = vec![
            mk("Action 'X' failed: first try"),
            mk("Action 'X' failed: retry attempt"),
        ];
        // We walk newest-first; the SECOND entry is newer in the vec, so it wins.
        let m = build_action_error_map(&logs);
        assert!(m.get("X").unwrap().contains("retry attempt"));
    }

    #[test]
    fn skips_lines_that_dont_name_an_action_in_quotes() {
        let logs = vec![mk("[ERROR] something failed somewhere")];
        assert!(build_action_error_map(&logs).is_empty());
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
    /// Pre-resolved log-scraped error for this action, if any. Comes from
    /// the parent's `action_log_errors` map; this row receives just its own
    /// slot so child rendering doesn't re-do the lookup.
    #[props(default)]
    log_error: Option<String>,
    /// Full map, so recursively-rendered child actions (inside scopes,
    /// for-each iterations, etc.) can look up their own log error.
    #[props(default)]
    action_log_errors: HashMap<String, String>,
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

    // Background-fetched error message for actions whose listing doesn't
    // carry `properties.error` (notably ParseJson failures — the runtime
    // returns `code: "BadRequest"` on the action row but pushes the actual
    // "Invalid type. Expected … but got …" message into the outputs blob).
    // Triggered by the use_effect below only when the inline error is empty.
    let mut fetched_error: Signal<Option<String>> = use_signal(|| None);
    let mut error_fetched: Signal<bool>           = use_signal(|| false);
    // For failed actions where extraction came up empty, surface the raw
    // properties object so the user can copy-paste it for diagnosis — we
    // can't iterate on the extractor without seeing the real shape.
    let mut fallback_dump: Signal<Option<String>> = use_signal(|| None);

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

    let inline_error = props.action.properties.error.as_ref().and_then(|e| {
        // Prefer message; fall back to code so skipped-reason is always visible
        e.message.clone().or_else(|| e.code.clone())
    });

    // For terminal-failed actions with no inline error (ParseJson and a few
    // other expression-evaluation failures), fetch the action detail in the
    // background and pull the real message out of the outputs blob. Runs once
    // per action — gated by `error_fetched` so a re-render doesn't restart it.
    {
        let inline_empty = inline_error.is_none();
        let is_failed = matches!(status_l.as_str(), "failed" | "timedout");
        let wf  = props.workflow.clone();
        let rid = props.run_id.clone();
        let name  = props.action.name.clone();
        // The Logic Apps detail endpoint strips `properties.type` on scope
        // actions, but the listing has it — pass it through so the helper
        // can recognise Foreach/Scope/Until/If and annotate the "NotSpecified"
        // fallback with the "expand to see which child failed" hint.
        let atype = props.action.properties.action_type.clone();
        use_effect(move || {
            if !is_failed || !inline_empty { return; }
            if *error_fetched.read() { return; }
            error_fetched.set(true);
            let wf2 = wf.clone();
            let rid2 = rid.clone();
            let name2 = name.clone();
            let atype2 = atype.clone();
            spawn(async move {
                if let Ok(mut detail) = workflows::get_action_detail(&wf2, &rid2, &name2).await {
                    if let Some(t) = atype2 {
                        if let Some(p) = detail.pointer_mut("/properties").and_then(|v| v.as_object_mut()) {
                            p.entry("type".to_string())
                                .or_insert_with(|| serde_json::Value::String(t));
                        }
                    }
                    let extracted = extract_error_from_detail(&detail);
                    // Diagnostic dump only when extraction returned NOTHING
                    // at all — for the known runtime-limitation cases we
                    // already explain in the message itself, and the JSON
                    // dump just adds noise.
                    if extracted.is_none() {
                        if let Some(props) = detail.get("properties") {
                            let dump = serde_json::to_string_pretty(props)
                                .unwrap_or_else(|_| props.to_string());
                            fallback_dump.set(Some(dump));
                        }
                    }
                    if let Some(msg) = extracted {
                        fetched_error.set(Some(msg));
                    }
                }
            });
        });
    }

    // Priority: API-attached error → log-scraped error (covers the ParseJson
    // and friends case where the runtime doesn't write `properties.error`) →
    // the fetched fallback ("NotSpecified — check the func start console").
    let error_msg = inline_error
        .or_else(|| props.log_error.clone())
        .or_else(|| fetched_error.read().clone());

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
            // ── download output ──────────────────────────────────────────
            // Generic across every customer: derives format from the action's
            // own schema (Table action with inputs.format=CSV → .csv, JSON
            // bodies → .json, plain strings → .txt or .csv if shaped that way).
            // Hidden for actions that ran but were `skipped`, since the body
            // is empty in that case.
            if !matches!(status_l.as_str(), "skipped" | "notspecified") {
                {
                    let wf   = props.workflow.clone();
                    let rid  = props.run_id.clone();
                    let name = props.action.name.clone();
                    let atype_owned = props.action.properties.action_type.clone();
                    rsx! {
                        button {
                            class: "btn-icon action-detail-btn",
                            title: "Save this action's output to disk (auto-detects CSV / JSON / text)",
                            onclick: move |_| {
                                let wf2   = wf.clone();
                                let rid2  = rid.clone();
                                let name2 = name.clone();
                                let at    = atype_owned.clone();
                                spawn(async move {
                                    let detail = match workflows::get_action_detail(&wf2, &rid2, &name2).await {
                                        Ok(v)  => v,
                                        Err(_) => return,
                                    };
                                    // Logic Apps's `Table` action puts the
                                    // requested format under `properties.inputs.format`.
                                    let req_fmt = detail
                                        .pointer("/properties/inputs/format")
                                        .and_then(|v| v.as_str())
                                        .map(str::to_string);
                                    let prep = crate::services::action_io::prepare_download(
                                        &name2,
                                        at.as_deref(),
                                        req_fmt.as_deref(),
                                        &detail,
                                    );
                                    let Some(prep) = prep else { return };
                                    let filename = prep.suggested_filename.clone();
                                    let bytes    = prep.bytes.clone();
                                    let label    = prep.format.label().to_string();
                                    let ext      = prep.format.extension().to_string();
                                    tokio::task::spawn_blocking(move || {
                                        if let Some(path) = rfd::FileDialog::new()
                                            .set_title("Save action output")
                                            .set_file_name(&filename)
                                            .add_filter(&label, &[&ext])
                                            .save_file()
                                        {
                                            let _ = std::fs::write(&path, &bytes);
                                        }
                                    }).await.ok();
                                });
                            },
                            "💾"
                        }
                    }
                }
            }
        }
        if let Some(msg) = error_msg {
            div { class: "{err_class}", style: "padding-left:{indent_px}px", "{msg}" }
        }
        // Diagnostic dump for failures where extraction came up thin —
        // shows the raw `properties` object so the user can copy-paste it.
        // Auto-shown only when fetched_error is the fallback variant; the
        // user can always still hit the ⋯ button to see the full action JSON.
        if let Some(dump) = fallback_dump.read().clone() {
            pre {
                class: "action-detail-pre",
                style: "padding-left:{indent_px}px; font-size:11px; max-height:240px; overflow:auto;",
                "{dump}"
            }
        }
        if *detail_open.read() {
            {
                let raw = detail_json.read().clone();
                let hint = serde_json::from_str::<serde_json::Value>(&raw)
                    .ok()
                    .and_then(|v| crate::services::sql_hint::detect(&v));
                rsx! {
                    if let Some(h) = hint {
                        SqlMissingHint { hint: h, indent_px: indent_px }
                    }
                    pre {
                        class: "action-detail-pre",
                        style: "padding-left:{indent_px}px",
                        "{raw}"
                    }
                }
            }
        }
        if *expanded.read() {
            for child in children.read().clone() {
                {
                    let child_log_err = props.action_log_errors.get(&child.name).cloned();
                    rsx! {
                        ActionRow {
                            action: child,
                            max_ms: child_max_ms,
                            is_live: props.is_live,
                            workflow: props.workflow.clone(),
                            run_id: props.run_id.clone(),
                            depth: props.depth + 1,
                            log_error: child_log_err,
                            action_log_errors: props.action_log_errors.clone(),
                        }
                    }
                }
            }
        }
    }
}

// ── SQL stored-procedure chip in analysis bar ─────────────────────────────

#[component]
fn SqlSprocChip(name: String, params: Vec<String>, status: Option<bool>) -> Element {
    let mut open = use_signal(|| false);
    let mut copied = use_signal(|| false);
    let param_refs: Vec<&str> = params.iter().map(|s| s.as_str()).collect();
    let stub = crate::services::sql_hint::stub_sproc_with_params(&name, &param_refs);

    let (dot_color, dot_title) = match status {
        Some(true)  => ("#3fb950", format!("Found in local SQL: {}", name)),
        Some(false) => ("#f85149", format!("MISSING in local SQL: {}", name)),
        None        => ("#8b949e", format!("Probing local SQL for: {}", name)),
    };

    rsx! {
        span { class: "wf-chip wf-chip-sql", title: "Calls stored procedure: {name}",
            span {
                title: "{dot_title}",
                style: "display:inline-block;width:7px;height:7px;border-radius:50%;background:{dot_color};margin-right:5px;vertical-align:middle",
            }
            span { class: "wf-type", "SP" }
            span { class: "wf-name", "{name}" }
            button {
                class: "wf-chip-payload-btn",
                title: "Show stub DDL",
                onclick: move |e| { e.stop_propagation(); open.set(!open()); },
                "📋"
            }
        }
        if open() {
            div { class: "wf-payload-popover",
                div { class: "wf-payload-popover-header",
                    span { "Stub DDL — {name}" }
                    div { style: "display:flex;gap:6px;align-items:center",
                        button {
                            class: "wf-payload-copy-btn",
                            onclick: {
                                let s = stub.clone();
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
                            onclick: move |_| open.set(false),
                            "×"
                        }
                    }
                }
                pre { class: "wf-payload-code", "{stub}" }
            }
        }
    }
}

// ── Missing-SQL-object hint banner (under failed action detail) ────────────

#[component]
fn SqlMissingHint(hint: crate::services::sql_hint::SqlMissingObject, indent_px: u32) -> Element {
    let mut copied = use_signal(|| false);
    let kind_label = hint.kind.label();
    let name       = hint.name.clone();
    let ddl        = hint.stub_ddl.clone();
    let raw        = hint.raw_message.clone();

    rsx! {
        div {
            class: "sql-missing-hint",
            style: "margin:6px 0; padding:10px 12px; background:rgba(210,153,34,0.12); \
                    border-left:3px solid #d29922; border-radius:4px; font-size:13px; \
                    padding-left:{indent_px + 12}px;",
            div { style: "display:flex;justify-content:space-between;align-items:center;gap:10px",
                div {
                    strong { "⚠ Missing {kind_label} locally: " }
                    code { style: "background:rgba(0,0,0,0.25); padding:1px 6px; border-radius:3px", "{name}" }
                    div { style: "opacity:0.8; margin-top:4px; font-size:12px", "{raw}" }
                }
                button {
                    class: "wf-payload-copy-btn",
                    onclick: {
                        let s = ddl.clone();
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
                    if copied() { "✅ Copied DDL" } else { "📋 Copy stub DDL" }
                }
            }
            pre {
                style: "margin-top:8px; padding:8px; background:rgba(0,0,0,0.35); \
                        border-radius:4px; font-size:11px; overflow-x:auto; white-space:pre",
                "{ddl}"
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
