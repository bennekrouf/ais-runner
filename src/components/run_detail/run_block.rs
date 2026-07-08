//! One collapsible block per run: header, failure summary, storage-events
//! strip (with per-run snapshot cache), and the action rows.

use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};
use crate::services::workflows::{ActionItem, RunItem};

use super::action_row::ActionRow;
use super::storage_events::{storage_events_for_run, summarize_storage_events};

// ── Sub-component per run ──────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
pub(super) struct RunBlockProps {
    pub(super) run:            RunItem,
    pub(super) actions:        Vec<ActionItem>,
    pub(super) max_ms:         i64,
    pub(super) is_live:        bool,
    pub(super) workflow:       String,
    pub(super) on_select_run:  EventHandler<String>,
    pub(super) collapsed_runs: Signal<HashSet<String>>,
    /// Live Azurite debug.log buffer. Filtered per-run by time window to
    /// surface storage events (4xx/5xx, table conflicts) inside this block.
    pub(super) az_lines:       Signal<Vec<String>>,
    /// Session cache of each run's storage events, owned by RunDetail. The
    /// live buffer rotates/evicts — this keeps a finished run's events
    /// visible while the user is still investigating it.
    pub(super) events_cache:   Signal<HashMap<String, Vec<String>>>,
    /// When true, the action list is filtered to just the first action whose
    /// status indicates failure (failed / timedout / cancelled). Lets the
    /// user pin down the culprit in a long action list without skipping past
    /// downstream "skipped" noise.
    pub(super) failures_only:  bool,
    /// Pre-computed `action_name → error message` map, scraped from the
    /// Functions host stdout that this run produced. Used as the fallback
    /// when the Logic Apps management API returns NotSpecified — the real
    /// ParseJson / Compose / expression-failure text only appears in stdout.
    pub(super) action_log_errors: HashMap<String, String>,
}

#[component]
pub(super) fn RunBlock(props: RunBlockProps) -> Element {
    let status_lower = props.run.properties.status.to_lowercase();
    let status_class = format!("run-status {}", status_lower);
    let run_id  = props.run.name.clone();
    let run_id2 = run_id.clone();
    let run_id3 = run_id.clone();

    let mut collapsed_runs = props.collapsed_runs;
    // Force-expand while the run is still in flight so the user can watch
    // actions appear. Once it finishes, honor the user's collapse choice.
    let is_running = status_lower == "running";
    let is_collapsed = !is_running && collapsed_runs.read().contains(&run_id);

    // ── Storage events (computed unconditionally: hooks below) ────────────
    // Live view: Azurite debug.log lines within this run's time window.
    let live_events = match props.run.properties.start_time.as_deref() {
        Some(start) => storage_events_for_run(
            &props.az_lines.read(),
            start,
            props.run.properties.end_time.as_deref(),
        ),
        None => Vec::new(),
    };
    // Persist the last non-empty view per run: the live buffer is a rolling
    // tail (rotation clears it, 500-line cap evicts) and a completed run's
    // end-time window can drop late-flushed lines — without this cache the
    // strip disappears right when the user wants to read it.
    let mut events_cache = props.events_cache;
    {
        let run_key = run_id.clone();
        let live = live_events.clone();
        use_effect(use_reactive!(|live| {
            // Keep the FULLEST view seen: when the run completes, the
            // end-time window can drop late-flushed lines from the live
            // computation — don't let that shrink an earlier, richer capture.
            if !live.is_empty() {
                let mut cache = events_cache.write();
                let keep = cache.get(&run_key).map_or(true, |old| live.len() >= old.len());
                if keep {
                    cache.insert(run_key.clone(), live.clone());
                }
            }
        }));
    }
    let from_cache = live_events.is_empty();
    let events = if from_cache {
        events_cache.read().get(&run_id).cloned().unwrap_or_default()
    } else {
        live_events
    };
    // Condense ~20 middleware log lines per request into one summary row.
    // Raw lines stay in the cache; summarizing at render time keeps the
    // cache format stable and the work is trivial (<500 lines).
    let ev_summaries = summarize_storage_events(&events);
    let ev_count  = ev_summaries.len();
    // Only REAL errors flip the strip to error state — 404 TableNotFound is
    // routine Logic Apps↔Azurite chatter (run/history tables created lazily).
    let ev_has_err = ev_summaries.iter().any(|s| s.is_real_error());
    let mut ev_open = use_signal(|| false);
    // Auto-expand when any request is a real 4xx/5xx so storage failures
    // surface without the user having to flip to the Azurite log tab.
    use_effect(use_reactive!(|ev_has_err| {
        if ev_has_err { ev_open.set(true); }
    }));

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
            // ── Failure summary strip ──────────────────────────────────────
            // For failed/timedout/cancelled runs, surface the first failed
            // action's name + error message inline so the user doesn't have
            // to expand the run and the action to see what went wrong.
            // Falls back to the log-scraped error when the API omits it
            // (ParseJson / Compose / expression failures).
            {
                let is_failure = matches!(status_lower.as_str(),
                    "failed" | "timedout" | "cancelled");
                let summary = if is_failure {
                    props.actions.iter().find(|a| matches!(
                        a.properties.status.to_lowercase().as_str(),
                        "failed" | "timedout" | "cancelled"
                    )).map(|a| {
                        let msg = a.properties.error.as_ref()
                            .and_then(|e| e.message.clone())
                            .or_else(|| props.action_log_errors.get(&a.name).cloned())
                            .unwrap_or_else(|| a.properties.code.clone()
                                .unwrap_or_else(|| "No error message reported.".to_string()));
                        (a.name.clone(), msg)
                    })
                } else { None };
                match (is_failure, summary) {
                    (true, Some((act_name, msg))) => rsx! {
                        div { class: "run-failure-summary",
                            style: "padding:6px 10px;background:rgba(248,81,73,0.08);border-left:3px solid #f85149;font-size:12px;",
                            span { style: "font-weight:600;color:#f85149;margin-right:6px;", "❌ {act_name}" }
                            span { style: "opacity:0.9;", "{msg}" }
                        }
                    },
                    (true, None) => rsx! {
                        div { class: "run-failure-summary",
                            style: "padding:6px 10px;background:rgba(248,81,73,0.08);border-left:3px solid #f85149;font-size:12px;opacity:0.8;",
                            "❌ Failed — no action-level error available (runtime did not persist action history)."
                        }
                    },
                    _ => rsx! {},
                }
            }
            if !is_collapsed {
                {
                    // Storage-events strip: Azurite debug.log lines that fell
                    // within this run's time window, minus poll heartbeats
                    // (computed above so the per-run cache hook is
                    // unconditional). "kept" = the live buffer has moved on;
                    // we're showing the snapshot captured while it was there.
                    if ev_count == 0 {
                        rsx! {}
                    } else {
                        let badge = if ev_has_err { "az-events-strip error" } else { "az-events-strip" };
                        let kept  = if from_cache { " · kept" } else { "" };
                        let label = if ev_has_err {
                            format!("📦 Storage events ({ev_count}){kept} — ⚠ errors")
                        } else {
                            format!("📦 Storage events ({ev_count}){kept}")
                        };
                        let title = if from_cache {
                            "Snapshot kept from this run — the live Azurite log buffer has rotated past these lines."
                        } else {
                            "Azurite storage activity within this run's time window."
                        };
                        rsx! {
                            div { class: "{badge}",
                                div {
                                    class: "az-events-header",
                                    style: "cursor:pointer;padding:2px 8px;font-size:11px;opacity:0.8",
                                    title: "{title}",
                                    onclick: move |_| { let v = !ev_open(); ev_open.set(v); },
                                    span { style: "display:inline-block;width:12px;",
                                        if ev_open() { "▼" } else { "▶" }
                                    }
                                    "{label}"
                                }
                                if ev_open() {
                                    div { class: "az-events-body",
                                        style: "padding:4px 8px 4px 22px;font-family:monospace;font-size:11px;max-height:200px;overflow:auto;",
                                        for s in ev_summaries.iter() {
                                            {
                                                let real_err = s.is_real_error();
                                                let benign   = s.is_benign_error();
                                                let cls = if real_err { "az-event-line error" } else { "az-event-line" };
                                                let style = if real_err { "color:#f85149" }
                                                            else if benign { "color:#c19c00;opacity:0.8" }
                                                            else { "opacity:0.75" };
                                                let status = s.status.map(|c| c.to_string())
                                                    .unwrap_or_else(|| "…".into());
                                                let code = s.error_code.as_deref().unwrap_or("");
                                                let benign_note = if benign { " (expected — table created lazily)" } else { "" };
                                                rsx! {
                                                    div { class: "{cls}", style: "{style}",
                                                        "{s.time} {s.method} {s.path} → {status} {code}{benign_note}"
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

