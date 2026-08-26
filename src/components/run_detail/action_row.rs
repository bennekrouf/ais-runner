//! One row per action in a run: status icon, duration bar, expandable
//! children (scopes / for-each), and inline error detail.

use crate::services::workflows::{self, duration_ms, ActionItem};
use dioxus::prelude::*;
use std::collections::HashMap;

use super::error_extract::extract_error_from_detail;
use super::sql_chips::SqlMissingHint;

// ── Fetch children for expandable actions ─────────────────────────────────

async fn fetch_children(
    workflow: String,
    run_id: String,
    action: String,
    action_type: Option<String>,
) -> Vec<ActionItem> {
    match action_type.as_deref() {
        Some("Foreach") => {
            let reps = match workflows::list_repetitions(&workflow, &run_id, &action).await {
                Ok(r) => r,
                Err(_) => return vec![],
            };
            let multi = reps.len() > 1;
            let mut all = Vec::new();
            for (i, rep) in reps.iter().enumerate() {
                if let Ok(acts) =
                    workflows::list_repetition_actions(&workflow, &run_id, &action, &rep.name).await
                {
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
pub(super) struct ActionRowProps {
    pub(super) action: ActionItem,
    pub(super) max_ms: i64,
    pub(super) is_live: bool,
    pub(super) workflow: String,
    pub(super) run_id: String,
    pub(super) depth: u8,
    /// Pre-resolved log-scraped error for this action, if any. Comes from
    /// the parent's `action_log_errors` map; this row receives just its own
    /// slot so child rendering doesn't re-do the lookup.
    #[props(default)]
    pub(super) log_error: Option<String>,
    /// Full map, so recursively-rendered child actions (inside scopes,
    /// for-each iterations, etc.) can look up their own log error.
    #[props(default)]
    pub(super) action_log_errors: HashMap<String, String>,
}

#[component]
pub(super) fn ActionRow(props: ActionRowProps) -> Element {
    let atype = props.action.properties.action_type.as_deref().unwrap_or("");
    let is_expandable = matches!(atype, "Foreach" | "Scope" | "Until" | "If");

    let mut expanded = use_signal(|| false);
    let mut loading = use_signal(|| false);
    let mut children = use_signal(|| Vec::<ActionItem>::new());
    let mut detail_open = use_signal(|| false);
    let mut detail_loading = use_signal(|| false);
    let mut detail_json = use_signal(|| String::new());

    // Background-fetched error message for actions whose listing doesn't
    // carry `properties.error` (notably ParseJson failures — the runtime
    // returns `code: "BadRequest"` on the action row but pushes the actual
    // "Invalid type. Expected … but got …" message into the outputs blob).
    // Triggered by the use_effect below only when the inline error is empty.
    let mut fetched_error: Signal<Option<String>> = use_signal(|| None);
    let mut error_fetched: Signal<bool> = use_signal(|| false);
    // For failed actions where extraction came up empty, surface the raw
    // properties object so the user can copy-paste it for diagnosis — we
    // can't iterate on the extractor without seeing the real shape.
    let mut fallback_dump: Signal<Option<String>> = use_signal(|| None);

    let status_l = props.action.properties.status.to_lowercase();
    let is_running = props.is_live
        && !matches!(
            status_l.as_str(),
            "succeeded" | "failed" | "skipped" | "timedout" | "cancelled"
        );

    let icon = if is_running {
        "⟳"
    } else {
        match status_l.as_str() {
            "succeeded" => "✅",
            "failed" => "❌",
            "skipped" => "⏭",
            _ => "⏳",
        }
    };

    let ms = duration_ms(
        &props.action.properties.start_time,
        &props.action.properties.end_time,
    )
    .unwrap_or(0);
    let pct = ((ms as f64 / props.max_ms as f64) * 100.0).clamp(1.0, 100.0);
    let bar_class = format!("timing-bar {}", status_l);
    let dur_label = if ms == 0 && is_running {
        "…".to_string()
    } else if ms < 1000 {
        format!("{}ms", ms)
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    };

    let row_class = if is_running {
        "action-row action-row-live"
    } else {
        "action-row"
    };
    let icon_class = if is_running {
        "action-icon spin"
    } else {
        "action-icon"
    };
    let indent_px = props.depth as u32 * 18;

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
        let wf = props.workflow.clone();
        let rid = props.run_id.clone();
        let name = props.action.name.clone();
        // The Logic Apps detail endpoint strips `properties.type` on scope
        // actions, but the listing has it — pass it through so the helper
        // can recognise Foreach/Scope/Until/If and annotate the "NotSpecified"
        // fallback with the "expand to see which child failed" hint.
        let atype = props.action.properties.action_type.clone();
        use_effect(move || {
            if !is_failed || !inline_empty {
                return;
            }
            if *error_fetched.read() {
                return;
            }
            error_fetched.set(true);
            let wf2 = wf.clone();
            let rid2 = rid.clone();
            let name2 = name.clone();
            let atype2 = atype.clone();
            spawn(async move {
                if let Ok(mut detail) = workflows::get_action_detail(&wf2, &rid2, &name2).await {
                    if let Some(t) = atype2 {
                        if let Some(p) = detail
                            .pointer_mut("/properties")
                            .and_then(|v| v.as_object_mut())
                        {
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

    let err_class = if status_l == "skipped" {
        "action-warning"
    } else {
        "action-error"
    };

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
