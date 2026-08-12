//! Test-suite view — record, review, and replay scenarios.
//!
//! Three modes share the panel, driven by `recorder::RecorderState`:
//!
//! * **idle** — the saved scenarios, each with run / rename / duplicate / delete.
//! * **recording** — a live list of what has been captured so far. The Stop
//!   button lives in the toolbar, not here, because the user spends a recording
//!   in the Connectors panel rather than on this view.
//! * **review** — the captured steps, reorderable and editable, before they are
//!   written to disk. Recording picks up incidental clicks, and a review screen
//!   that made them hard to drop would make the whole feature not worth using.
//!
//! Step editing is done on the step's own JSON rather than through a form per
//! variant: the file format *is* JSON, the user edits it in their editor
//! anyway, and a typed form for each of the twenty-odd variants would go stale
//! the moment a variant gains a field.

use dioxus::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::components::log_panel::LogLine;
use crate::screens::MainContext;
use crate::services::cosmos_check::{self, CosmosConnection};
use crate::services::process::{ManagedProcess, ServiceState};
use crate::services::scenario::{self, RunContext, Scenario, Step, StepResult, StepStatus};

/// The Service Bus emulator always runs on loopback; `sb_amqp` appends the port.
const SB_HOST: &str = "127.0.0.1";

/// How long to wait for the emulator's AMQP port after a queue-setup restart.
/// The stack is SQL Edge plus the emulator host, so a cold start is slow.
const EMULATOR_READY_TIMEOUT_MS: u64 = 240_000;

#[derive(Props, Clone, PartialEq)]
pub struct TestsPanelProps {
    pub logic_apps_dir: String,
}

#[component]
pub fn TestsPanel(props: TestsPanelProps) -> Element {
    let project_root = project_root_of(&props.logic_apps_dir);
    let ctx = use_context::<MainContext>();
    let mut recorder = ctx.recorder;

    // Own status line: as a top-level view this panel can't borrow the
    // Connectors panel's, and a run's verdict needs somewhere to land.
    // `Signal` is Copy, so the closures below take their own mutable handle.
    let status: Signal<Option<(String, bool)>> = use_signal(|| None);

    // Cosmos endpoint/key come from the project's own connections rather than
    // from the Connectors panel, so this view works without opening that one.
    let cosmos_connections: Signal<Vec<CosmosConnection>> = use_signal(|| {
        cosmos_check::detect_cosmos_connections(&props.logic_apps_dir)
    });

    let mut scenarios: Signal<Vec<Scenario>> = use_signal(Vec::new);
    let mut load_errors: Signal<Vec<String>> = use_signal(Vec::new);
    // Per-scenario step results, keyed by scenario name.
    let mut results: Signal<HashMap<String, Vec<StepResult>>> = use_signal(HashMap::new);
    let mut running: Signal<Option<String>> = use_signal(|| None);
    // Group names the user has collapsed. Not persisted — a fresh open of the
    // Tests view always starts with every group expanded.
    let mut collapsed_groups: Signal<std::collections::HashSet<String>> = use_signal(std::collections::HashSet::new);

    // Which scenario name is being typed into, for the record prompt and the
    // review screen's name field.
    let mut name_draft: Signal<String> = use_signal(String::new);
    let mut naming: Signal<bool> = use_signal(|| false);
    // Review-screen step editor: the step being edited, its JSON, and any parse
    // error from the last Apply.
    let mut editing: Signal<Option<usize>> = use_signal(|| None);
    let mut edit_text: Signal<String> = use_signal(String::new);
    let mut edit_error: Signal<Option<String>> = use_signal(|| None);
    // Inline rename / delete-confirm on a saved scenario, keyed by its name.
    let mut renaming: Signal<Option<String>> = use_signal(|| None);
    let mut delete_confirm: Signal<Option<String>> = use_signal(|| None);

    let reload = {
        let root = project_root.clone();
        move |_| {
            let (found, errors) = scenario::discover(&root);
            scenarios.set(found);
            load_errors.set(errors);
        }
    };

    // Populate on first render, and whenever the project directory changes.
    use_effect({
        let root = project_root.clone();
        move || {
            let (found, errors) = scenario::discover(&root);
            scenarios.set(found);
            load_errors.set(errors);
        }
    });

    // `restart_func` needs half a dozen of MainContext's signals — see the
    // doc comment on `scenario::RestartFn` for why that lives here rather
    // than in scenario.rs itself. `handle_start` kicks the process off and
    // returns immediately, so `wait_for_workflows` is what actually waits
    // for func to be ready.
    let restart_func: scenario::RestartFn = {
        let azurite_state = ctx.azurite_state;
        let func_state = ctx.func_state;
        let func_proc = ctx.func_proc;
        let workflows_sig = ctx.workflows;
        let traced_wfs = ctx.traced_wfs;
        let cleared_wfs = ctx.cleared_wfs;
        let log_lines = ctx.log_lines;
        let dir = props.logic_apps_dir.clone();
        std::sync::Arc::new(move || {
            let dir = dir.clone();
            Box::pin(async move {
                crate::handlers::func_start::handle_stop(func_state, func_proc, log_lines);
                crate::handlers::func_start::handle_start(
                    azurite_state, func_state, func_proc, workflows_sig, traced_wfs, cleared_wfs,
                    log_lines, dir,
                );
                match crate::services::workflows::wait_for_workflows(120).await {
                    Ok(list) => Ok(format!("func restarted — {} workflow(s) registered", list.len())),
                    Err(e) => Err(format!("func restarted but its workflows never came back: {e}")),
                }
            }) as scenario::BoxFuture
        })
    };

    let run_ctx = {
        let conns = cosmos_connections.read();
        RunContext {
            sb_host: SB_HOST.to_string(),
            cosmos_endpoint: cosmos_endpoint_of(&conns),
            cosmos_key: cosmos_key_of(&conns),
            project_root: project_root.clone(),
            restart_func: Some(restart_func.clone()),
        }
    };

    let scenario_dir = scenario::scenario_dir(&project_root);
    let list = scenarios.read().clone();
    // `discover()` sorts group-first, so scenarios sharing a group are always
    // adjacent — grouping by run of equal group needs no map, and preserves
    // that order rather than re-sorting it.
    let mut grouped: Vec<(Option<String>, Vec<Scenario>)> = Vec::new();
    for s in &list {
        let g = scenario::group_of(&project_root, s);
        match grouped.last_mut() {
            Some((last_g, items)) if *last_g == g => items.push(s.clone()),
            _ => grouped.push((g, vec![s.clone()])),
        }
    }
    let busy = running.read().clone();
    let is_recording = recorder.read().is_recording();
    let in_review = recorder.read().in_review();
    // Snapshot rather than reading the signal inside the markup: the step rows
    // carry closures that write back to the recorder, and holding a read borrow
    // across the render would collide with them.
    let captured = recorder.read().steps.clone();
    let captured_name = recorder.read().name.clone();
    let captured_len = captured.len();

    rsx! {
        div { id: "settings-panel",
            // ── header ──────────────────────────────────────────────────────
            div { class: "settings-header",
                h2 { "🧪 Tests" }
                div { style: "display:flex;gap:8px;align-items:center;margin-left:auto",
                    if !is_recording && !in_review {
                        button {
                            class: "btn btn-small btn-run",
                            title: "Record a scenario: every action you take in the app is captured as a step",
                            onclick: move |_| {
                                name_draft.set(String::new());
                                naming.set(true);
                            },
                            "● Create scenario"
                        }
                    }
                    if !is_recording && !in_review {
                        button {
                            class: "btn btn-small btn-run",
                            disabled: busy.is_some() || list.is_empty(),
                            title: if busy.is_some() {
                                "A scenario is already running".to_string()
                            } else {
                                "Run every scenario in order — scenarios share the local emulators, so they run one at a time, not in parallel".to_string()
                            },
                            onclick: {
                                let all_scenarios = list.clone();
                                let run_ctx = run_ctx.clone();
                                let dir = props.logic_apps_dir.clone();
                                move |_| {
                                    spawn(run_many(
                                        "Run all".to_string(), all_scenarios.clone(), run_ctx.clone(), dir.clone(),
                                        ctx.sb_emu_state, ctx.sb_emu_proc, ctx.log_lines, ctx.sb_emu_lines,
                                        results, status, running,
                                    ));
                                }
                            },
                            "▶▶ Run all"
                        }
                    }
                    button {
                        class: "btn btn-small",
                        title: "Re-read .ais-runner/scenarios from disk — picks up files added, edited, or removed outside the app",
                        onclick: reload,
                        "↻ Reload"
                    }
                    button {
                        class: "btn btn-small",
                        title: "Clear every scenario's last-run results from this view (does not touch the scenario files themselves)",
                        onclick: {
                            let mut status = status;
                            move |_| {
                                results.write().clear();
                                status.set(None);
                            }
                        },
                        "🧹 Flush results"
                    }
                }
            }

            // Pinned above the scroll area: a run's verdict has to stay readable
            // while the user scrolls a long step list looking for what failed.
            if let Some((msg, is_err)) = status.read().clone() {
                div {
                    class: if is_err { "settings-status error" } else { "settings-status ok" },
                    "{msg}"
                }
            }

            div { class: "settings-scroll",

            // ── name prompt ─────────────────────────────────────────────────
            if *naming.read() {
                div { class: "db-section",
                    div { class: "db-create-row",
                        input {
                            class: "db-field-input",
                            style: "flex:1",
                            placeholder: "Scenario name (e.g. Invoice → SAP happy path)",
                            value: "{name_draft}",
                            oninput: move |e| name_draft.set(e.value()),
                            onkeydown: {
                                let root = project_root.clone();
                                move |e: KeyboardEvent| {
                                    if e.key() == Key::Enter {
                                        let name = name_draft.read().trim().to_string();
                                        if !name.is_empty() {
                                            recorder.write().start(name, root.clone());
                                            naming.set(false);
                                        }
                                    }
                                }
                            },
                        }
                        button {
                            class: "btn btn-run btn-small",
                            disabled: name_draft.read().trim().is_empty(),
                            onclick: {
                                let root = project_root.clone();
                                move |_| {
                                    let name = name_draft.read().trim().to_string();
                                    if name.is_empty() { return; }
                                    recorder.write().start(name, root.clone());
                                    naming.set(false);
                                }
                            },
                            "● Start recording"
                        }
                        button {
                            class: "btn btn-small",
                            onclick: move |_| naming.set(false),
                            "Cancel"
                        }
                    }
                    div { class: "scenario-desc",
                        "Then just use the app normally — creating a queue, running SQL, uploading a blob or triggering a workflow is captured as a step. Stop from the toolbar when you're done."
                    }
                }
            }

            // ── live capture ────────────────────────────────────────────────
            if is_recording {
                div { class: "db-section",
                    div { class: "db-section-title-row",
                        span { class: "db-section-title", "● Recording “{captured_name}”" }
                        span { class: "scenario-count", "{captured.len()} step(s)" }
                    }
                    if captured.is_empty() {
                        div { class: "empty-state",
                            "Nothing captured yet. Actions are recorded when they succeed — read-only things like listing blobs or peeking a queue are deliberately ignored."
                        }
                    }
                    div { class: "scenario-steps",
                        for (i, step) in captured.iter().enumerate() {
                            div { class: "scenario-step ok",
                                span { class: "scenario-step-num", "{i + 1}" }
                                span { class: "scenario-step-label", {scenario::label_of(step)} }
                            }
                        }
                    }
                }
            }

            // ── review before saving ────────────────────────────────────────
            if in_review {
                div { class: "db-section",
                    div { class: "db-section-title-row",
                        span { class: "db-section-title", "Review captured steps" }
                        span { class: "scenario-count", "{captured.len()} step(s)" }
                    }
                    div { class: "scenario-desc",
                        "Drop anything incidental, reorder what's out of sequence, and turn a guessed sleep into a real wait or assertion before saving."
                    }

                    div { class: "db-create-row",
                        input {
                            class: "db-field-input",
                            style: "flex:1",
                            placeholder: "Scenario name",
                            value: "{review_name(&name_draft.read(), &captured_name)}",
                            oninput: move |e| name_draft.set(e.value()),
                        }
                        button {
                            class: "btn btn-run btn-small",
                            title: "Write the scenario to .ais-runner/scenarios",
                            onclick: {
                                let root = project_root.clone();
                                let fallback = captured_name.clone();
                                let mut status = status;
                                move |_| {
                                    let name = review_name(&name_draft.read(), &fallback);
                                    let saved = Scenario {
                                        name: name.clone(),
                                        description: String::new(),
                                        vars: Default::default(),
                                        steps: recorder.peek().steps.clone(),
                                        source: scenario::unique_path(&root, &name),
                                    };
                                    match scenario::save(&saved) {
                                        Ok(()) => {
                                            status.set(Some((format!("💾 Saved {}", saved.source.display()), false)));
                                            recorder.write().reset();
                                            name_draft.set(String::new());
                                            editing.set(None);
                                            let (found, errors) = scenario::discover(&root);
                                            scenarios.set(found);
                                            load_errors.set(errors);
                                        }
                                        Err(e) => status.set(Some((format!("Save failed: {e}"), true))),
                                    }
                                }
                            },
                            "💾 Save scenario"
                        }
                        button {
                            class: "btn btn-small btn-danger",
                            title: "Throw the recording away",
                            onclick: move |_| {
                                recorder.write().reset();
                                name_draft.set(String::new());
                                editing.set(None);
                            },
                            "Discard"
                        }
                    }

                    if let Some(err) = edit_error.read().clone() {
                        div { class: "settings-status error", "{err}" }
                    }

                    div { class: "scenario-steps",
                        for (i, step) in captured.iter().enumerate() {
                            div { class: "scenario-review-row",
                                div { class: "scenario-step ok",
                                    span { class: "scenario-step-num", "{i + 1}" }
                                    span { class: "scenario-step-label", {scenario::label_of(step)} }
                                    span { class: "scenario-step-detail", {step_hint(step)} }
                                    div { class: "scenario-step-actions",
                                        button {
                                            class: "btn btn-small",
                                            disabled: i == 0,
                                            title: "Move earlier",
                                            onclick: move |_| { recorder.write().steps.swap(i - 1, i); editing.set(None); },
                                            "▲"
                                        }
                                        button {
                                            class: "btn btn-small",
                                            disabled: i + 1 == captured_len,
                                            title: "Move later",
                                            onclick: move |_| { recorder.write().steps.swap(i, i + 1); editing.set(None); },
                                            "▼"
                                        }
                                        button {
                                            class: "btn btn-small",
                                            title: "Edit this step's fields as JSON",
                                            onclick: {
                                                let step = step.clone();
                                                move |_| {
                                                    if editing.peek().as_ref() == Some(&i) {
                                                        editing.set(None);
                                                    } else {
                                                        edit_text.set(step_to_json(&step));
                                                        edit_error.set(None);
                                                        editing.set(Some(i));
                                                    }
                                                }
                                            },
                                            "✎"
                                        }
                                        button {
                                            class: "btn btn-small btn-danger",
                                            title: "Remove this step",
                                            onclick: move |_| {
                                                recorder.write().steps.remove(i);
                                                editing.set(None);
                                            },
                                            "✕"
                                        }
                                    }
                                }
                                if editing.read().as_ref() == Some(&i) {
                                    div { class: "scenario-step-editor",
                                        textarea {
                                            class: "scenario-step-json",
                                            rows: "8",
                                            value: "{edit_text}",
                                            oninput: move |e| edit_text.set(e.value()),
                                        }
                                        div { style: "display:flex;gap:6px;margin-top:4px",
                                            button {
                                                class: "btn btn-run btn-small",
                                                onclick: move |_| {
                                                    match parse_step(&edit_text.read()) {
                                                        Ok(parsed) => {
                                                            recorder.write().steps[i] = parsed;
                                                            edit_error.set(None);
                                                            editing.set(None);
                                                        }
                                                        Err(e) => edit_error.set(Some(e)),
                                                    }
                                                },
                                                "Apply"
                                            }
                                            button {
                                                class: "btn btn-small",
                                                onclick: move |_| { editing.set(None); edit_error.set(None); },
                                                "Cancel"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // ── saved scenarios ─────────────────────────────────────────────
            div { class: "db-section",
            div { class: "db-section-title-row",
                span { class: "db-section-title", "Scenarios" }
                span { class: "scenario-count", "{scenario_dir.display()}" }
            }

            for err in load_errors.read().iter() {
                div { class: "settings-status error", "{err}" }
            }

            if list.is_empty() {
                div { class: "empty-state",
                    "No scenarios yet. Use “● Create scenario” to record one, or write "
                    code { "{scenario_dir.display()}/<name>.json" }
                    " by hand."
                }
            }

            for (group, items) in grouped {
                {
                let is_collapsed = group.as_ref().is_some_and(|g| collapsed_groups.read().contains(g));
                rsx! {
                if let Some(group_name) = group.clone() {
                    div { class: "scenario-group-header",
                        span {
                            class: "scenario-group-toggle",
                            title: if is_collapsed { "Expand" } else { "Collapse" },
                            onclick: {
                                let group_name = group_name.clone();
                                move |_| {
                                    let mut set = collapsed_groups.write();
                                    if !set.remove(&group_name) {
                                        set.insert(group_name.clone());
                                    }
                                }
                            },
                            span { class: "scenario-group-chevron", if is_collapsed { "▸" } else { "▾" } }
                            span { class: "scenario-group-name", "📁 {group_name}" }
                            span { class: "scenario-count", "{items.len()} scenario(s)" }
                        }
                        button {
                            class: "btn btn-small btn-run",
                            disabled: busy.is_some(),
                            title: if busy.is_some() {
                                "A scenario is already running".to_string()
                            } else {
                                format!("Run every scenario in '{group_name}', in order")
                            },
                            onclick: {
                                let items = items.clone();
                                let run_ctx = run_ctx.clone();
                                let dir = props.logic_apps_dir.clone();
                                let label = format!("Run group: {group_name}");
                                move |_| {
                                    spawn(run_many(
                                        label.clone(), items.clone(), run_ctx.clone(), dir.clone(),
                                        ctx.sb_emu_state, ctx.sb_emu_proc, ctx.log_lines, ctx.sb_emu_lines,
                                        results, status, running,
                                    ));
                                }
                            },
                            "▶▶ Run group"
                        }
                    }
                }
                if !is_collapsed {
                for scenario_item in items.clone() {
                {
                    let name = scenario_item.name.clone();
                    let step_count = scenario_item.steps.len();
                    let source = scenario_item.source.clone();
                    let is_running = busy.as_deref() == Some(name.as_str());
                    // Any scenario is blocked while another one runs: they share
                    // the same emulators, so overlapping runs would corrupt each
                    // other's queues and containers.
                    let blocked = busy.is_some() && !is_running;
                    let steps = results.read().get(&name).cloned().unwrap_or_default();
                    let is_renaming = renaming.read().as_deref() == Some(name.as_str());
                    let confirming = delete_confirm.read().as_deref() == Some(name.as_str());
                    let root = project_root.clone();

                    rsx! {
                        div { class: "scenario-card",
                            div { class: "scenario-header",
                                span { class: "scenario-name", "{name}" }
                                span { class: "scenario-count", "{step_count} step(s)" }
                                {summary_badge(&steps, is_running)}
                                div { style: "display:flex;gap:6px;margin-left:auto",
                                    button {
                                        class: "btn btn-small",
                                        title: "Rename this scenario",
                                        onclick: {
                                            let name = name.clone();
                                            move |_| {
                                                name_draft.set(name.clone());
                                                renaming.set(Some(name.clone()));
                                            }
                                        },
                                        "Rename"
                                    }
                                    button {
                                        class: "btn btn-small",
                                        title: "Copy this scenario to a new file",
                                        onclick: {
                                            let original = scenario_item.clone();
                                            let root = root.clone();
                                            let mut status = status;
                                            move |_| {
                                                let mut copy = original.clone();
                                                copy.name = format!("{} (copy)", original.name);
                                                copy.source = scenario::unique_path(&root, &copy.name);
                                                match scenario::save(&copy) {
                                                    Ok(()) => {
                                                        status.set(Some((format!("Duplicated to {}", copy.source.display()), false)));
                                                        let (found, errors) = scenario::discover(&root);
                                                        scenarios.set(found);
                                                        load_errors.set(errors);
                                                    }
                                                    Err(e) => status.set(Some((format!("Duplicate failed: {e}"), true))),
                                                }
                                            }
                                        },
                                        "Duplicate"
                                    }
                                    button {
                                        class: "btn btn-small",
                                        title: "Open the scenario JSON in your editor",
                                        onclick: {
                                            let path = source.clone();
                                            move |_| crate::utils::open_in_editor(&path.to_string_lossy())
                                        },
                                        "Edit"
                                    }
                                    if confirming {
                                        button {
                                            class: "btn btn-small btn-danger",
                                            title: "Confirm — delete the scenario file",
                                            onclick: {
                                                let target = scenario_item.clone();
                                                let root = root.clone();
                                                let mut status = status;
                                                move |_| {
                                                    delete_confirm.set(None);
                                                    match scenario::delete(&target) {
                                                        Ok(()) => {
                                                            status.set(Some((format!("Deleted {}", target.source.display()), false)));
                                                            let (found, errors) = scenario::discover(&root);
                                                            scenarios.set(found);
                                                            load_errors.set(errors);
                                                        }
                                                        Err(e) => status.set(Some((format!("Delete failed: {e}"), true))),
                                                    }
                                                }
                                            },
                                            "⚠ Confirm"
                                        }
                                        button {
                                            class: "btn btn-small",
                                            onclick: move |_| delete_confirm.set(None),
                                            "✕"
                                        }
                                    } else {
                                        button {
                                            class: "btn btn-small",
                                            title: "Delete this scenario",
                                            onclick: {
                                                let name = name.clone();
                                                move |_| delete_confirm.set(Some(name.clone()))
                                            },
                                            "Delete"
                                        }
                                    }
                                    button {
                                        class: "btn btn-run btn-small",
                                        disabled: is_running || blocked,
                                        title: if blocked { "Another scenario is running" } else { "Run every step in order" },
                                        onclick: {
                                            let to_run = scenario_item.clone();
                                            let run_ctx = run_ctx.clone();
                                            let key = name.clone();
                                            let dir = props.logic_apps_dir.clone();
                                            let mut status = status;
                                            move |_| {
                                                let to_run = to_run.clone();
                                                let run_ctx = run_ctx.clone();
                                                let key = key.clone();
                                                let dir = dir.clone();
                                                running.set(Some(key.clone()));
                                                spawn(async move {
                                                    if let Some(all) = run_one(
                                                        to_run, run_ctx, dir,
                                                        ctx.sb_emu_state, ctx.sb_emu_proc,
                                                        ctx.log_lines, ctx.sb_emu_lines,
                                                        results, status,
                                                    ).await {
                                                        let failed = all
                                                            .iter()
                                                            .filter(|r| r.status == StepStatus::Failed)
                                                            .count();
                                                        status.set(Some(if failed == 0 {
                                                            (format!("✅ {key}: {} step(s) passed", all.len()), false)
                                                        } else {
                                                            (format!("❌ {key}: {failed} step(s) failed"), true)
                                                        }));
                                                    }
                                                    running.set(None);
                                                });
                                            }
                                        },
                                        if is_running { "Running…" } else { "▶ Run" }
                                    }
                                }
                            }

                            if is_renaming {
                                div { class: "db-create-row",
                                    input {
                                        class: "db-field-input",
                                        style: "flex:1",
                                        value: "{name_draft}",
                                        oninput: move |e| name_draft.set(e.value()),
                                    }
                                    button {
                                        class: "btn btn-run btn-small",
                                        disabled: name_draft.read().trim().is_empty(),
                                        onclick: {
                                            let target = scenario_item.clone();
                                            let root = root.clone();
                                            let mut status = status;
                                            move |_| {
                                                let new_name = name_draft.read().trim().to_string();
                                                if new_name.is_empty() { return; }
                                                renaming.set(None);
                                                match scenario::rename(&target, &new_name) {
                                                    Ok(r) => {
                                                        status.set(Some((format!("Renamed to {}", r.source.display()), false)));
                                                        let (found, errors) = scenario::discover(&root);
                                                        scenarios.set(found);
                                                        load_errors.set(errors);
                                                    }
                                                    Err(e) => status.set(Some((format!("Rename failed: {e}"), true))),
                                                }
                                            }
                                        },
                                        "Save"
                                    }
                                    button {
                                        class: "btn btn-small",
                                        onclick: move |_| renaming.set(None),
                                        "Cancel"
                                    }
                                }
                            }

                            if !scenario_item.description.is_empty() {
                                div { class: "scenario-desc", "{scenario_item.description}" }
                            }

                            if !steps.is_empty() {
                                div { class: "scenario-steps",
                                    for step in steps.iter() {
                                        div { class: step_row_class(step.status),
                                            span { class: "scenario-step-num", "{step.index + 1}" }
                                            span { class: "scenario-step-icon", {step_icon(step.status)} }
                                            span { class: "scenario-step-label", "{step.label}" }
                                            span { class: "scenario-step-detail", "{step.detail}" }
                                            if step.status != StepStatus::Skipped {
                                                span { class: "scenario-step-time", "{step.elapsed_ms}ms" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                } // for scenario_item in items
                } // if !is_collapsed
                } // rsx! (group header + cards)
                } // let is_collapsed block
            }
            }
            } // .settings-scroll
        }
    }
}

/// Run a list of scenarios in order, one at a time — they share the local
/// emulators, so running them concurrently would corrupt each other's queues
/// and containers. `label` distinguishes "Run all" from a specific group's
/// "Run group" in the final status line.
///
/// Doesn't stop at the first failure: a sweep is more useful as a full report
/// (which of these still pass) than a bisection.
#[allow(clippy::too_many_arguments)]
async fn run_many(
    label: String,
    scenarios: Vec<Scenario>,
    run_ctx: RunContext,
    dir: String,
    sb_emu_state: Signal<ServiceState>,
    sb_emu_proc: Signal<std::sync::Arc<ManagedProcess>>,
    log_lines: Signal<Vec<LogLine>>,
    sb_emu_lines: Signal<Vec<String>>,
    results: Signal<HashMap<String, Vec<StepResult>>>,
    mut status: Signal<Option<(String, bool)>>,
    mut running: Signal<Option<String>>,
) {
    let total = scenarios.len();
    let mut passed = 0usize;
    let mut failed_names: Vec<String> = Vec::new();

    for (i, s) in scenarios.into_iter().enumerate() {
        let key = s.name.clone();
        running.set(Some(key.clone()));
        status.set(Some((format!("{label} — {}/{total}: {key}", i + 1), false)));

        match run_one(
            s, run_ctx.clone(), dir.clone(),
            sb_emu_state, sb_emu_proc, log_lines, sb_emu_lines,
            results, status,
        ).await {
            Some(steps) if steps.iter().any(|r| r.status == StepStatus::Failed) => {
                failed_names.push(key);
            }
            Some(_) => passed += 1,
            // Queue setup failed — status already explains why; still counts
            // against the scenario, not a crash of the sweep.
            None => failed_names.push(key),
        }
    }

    running.set(None);
    status.set(Some(if failed_names.is_empty() {
        (format!("✅ {label}: {passed}/{total} scenario(s) passed"), false)
    } else {
        (
            format!("❌ {label}: {passed}/{total} passed — failed: {}", failed_names.join(", ")),
            true,
        )
    }));
}

/// Run one scenario end to end: queue setup, then every step, streaming
/// results into `results` as they land.
///
/// Shared by the single-scenario Run button and "Run all" so the two paths
/// can't drift — a scenario behaves identically alone or as part of a sweep.
///
/// `None` means queue setup itself failed before any step ran — `status` is
/// already set with why. Distinct from `Some(vec![])`, an empty-but-real
/// result, so a prep failure can never be reported as "0 step(s) passed".
async fn run_one(
    scenario_item: Scenario,
    run_ctx: RunContext,
    dir: String,
    sb_emu_state: Signal<ServiceState>,
    sb_emu_proc: Signal<std::sync::Arc<ManagedProcess>>,
    log_lines: Signal<Vec<LogLine>>,
    sb_emu_lines: Signal<Vec<String>>,
    mut results: Signal<HashMap<String, Vec<StepResult>>>,
    mut status: Signal<Option<(String, bool)>>,
) -> Option<Vec<StepResult>> {
    let key = scenario_item.name.clone();
    // Clear previous results so a rerun never shows a mix of two runs.
    results.write().insert(key.clone(), Vec::new());

    let queues = scenario::queues_to_create(&scenario_item);
    if !queues.is_empty() {
        status.set(Some((format!("Preparing {} queue(s)…", queues.len()), false)));
        if let Err(e) = prepare_queues(queues, sb_emu_state, sb_emu_proc, log_lines, sb_emu_lines, dir).await {
            status.set(Some((format!("❌ {key}: queue setup failed — {e}"), true)));
            return None;
        }
    }

    let key_for_step = key.clone();
    Some(
        scenario::run(&scenario_item, &run_ctx, move |r| {
            results.write().entry(key_for_step.clone()).or_default().push(r);
        })
        .await,
    )
}

/// Apply a scenario's `create_queue` steps, restarting the emulator once if any
/// of them actually changed `Config.json`.
///
/// `add_queue_to_emulator_config` writes the config and nothing more — the
/// emulator only learns about a queue at startup. Replaying `create_queue`
/// inline would therefore leave every subsequent send failing against a queue
/// the broker has never heard of. Doing it here, once, before the run also means
/// a scenario that creates five queues costs one restart rather than five.
///
/// A scenario whose queues are all already in the config skips the restart
/// entirely, which is the common case on a re-run.
async fn prepare_queues(
    queues: Vec<String>,
    state: Signal<ServiceState>,
    proc: Signal<std::sync::Arc<ManagedProcess>>,
    log_lines: Signal<Vec<LogLine>>,
    emu_lines: Signal<Vec<String>>,
    logic_apps_dir: String,
) -> Result<(), String> {
    let to_add = queues.clone();
    let added = tokio::task::spawn_blocking(move || {
        let mut added = Vec::new();
        for q in &to_add {
            match crate::handlers::sb_emulator::add_queue_to_emulator_config(q) {
                Ok(true) => added.push(q.clone()),
                Ok(false) => {}
                Err(e) => return Err(format!("{q}: {e}")),
            }
        }
        Ok(added)
    })
    .await
    .map_err(|e| format!("queue setup task panicked: {e}"))??;

    if added.is_empty() {
        return Ok(());
    }

    crate::handlers::sb_emulator::handle_stop(state, proc, log_lines);
    crate::handlers::sb_emulator::handle_start(state, proc, log_lines, emu_lines, logic_apps_dir);
    wait_for_amqp(EMULATOR_READY_TIMEOUT_MS).await
}

/// Poll the emulator's AMQP port until it accepts a connection.
///
/// The broker needs a moment more after the port opens; `sb_amqp::send_*`
/// already retries on the "header exchange" error that produces, so this only
/// has to wait for the listener.
async fn wait_for_amqp(timeout_ms: u64) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        if tokio::net::TcpStream::connect("127.0.0.1:5672").await.is_ok() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "the Service Bus emulator did not come back up within {}s",
                timeout_ms / 1000
            ));
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// The name the review screen will save under: what the user typed, falling
/// back to the name the recording was started with.
fn review_name(draft: &str, recorded: &str) -> String {
    let draft = draft.trim();
    if draft.is_empty() {
        recorded.to_string()
    } else {
        draft.to_string()
    }
}

/// A step's fields as pretty JSON, for the review-screen editor.
fn step_to_json(step: &Step) -> String {
    serde_json::to_string_pretty(step).unwrap_or_else(|e| format!("{{ \"error\": \"{e}\" }}"))
}

fn parse_step(text: &str) -> Result<Step, String> {
    serde_json::from_str(text).map_err(|e| format!("Not a valid step: {e}"))
}

/// Short note on the steps the recorder guessed at, so the user knows which
/// rows are worth a second look before saving.
fn step_hint(step: &Step) -> &'static str {
    match step {
        Step::Sleep { .. } => "guessed from the pause while recording — consider a wait or assertion",
        Step::WaitForRun { .. } => "inserted after the trigger",
        Step::CreateQueue { .. } => "applied with one emulator restart before the run",
        _ => "",
    }
}

/// Overall verdict for a scenario, shown next to its name.
///
/// Reports "passed" only once every step has settled — a run whose steps have
/// all succeeded *so far* is still in progress, and calling it green early
/// would be misleading.
fn summary_badge(steps: &[StepResult], is_running: bool) -> Element {
    if is_running {
        return rsx! { span { class: "scenario-badge running", "running…" } };
    }
    if steps.is_empty() {
        return rsx! { span { class: "scenario-badge idle", "not run" } };
    }
    let failed = steps.iter().filter(|s| s.status == StepStatus::Failed).count();
    let skipped = steps.iter().filter(|s| s.status == StepStatus::Skipped).count();
    if failed == 0 && skipped == 0 {
        rsx! { span { class: "scenario-badge ok", "✅ passed" } }
    } else {
        rsx! { span { class: "scenario-badge fail", "❌ {failed} failed" } }
    }
}

fn step_row_class(status: StepStatus) -> &'static str {
    match status {
        StepStatus::Ok => "scenario-step ok",
        StepStatus::Failed => "scenario-step fail",
        StepStatus::Skipped => "scenario-step skipped",
    }
}

fn step_icon(status: StepStatus) -> &'static str {
    match status {
        StepStatus::Ok => "✅",
        StepStatus::Failed => "❌",
        StepStatus::Skipped => "⊘",
    }
}

/// The project root that holds `.ais-runner/`, given the resolved workflow dir.
///
/// `logic_apps_dir` points at the folder containing the workflow folders, which
/// is usually `<project>/logic_apps`. Scenarios belong beside the project, not
/// inside the deployable folder, so step up one level when that's the shape.
fn project_root_of(logic_apps_dir: &str) -> PathBuf {
    let path = Path::new(logic_apps_dir);
    let is_nested = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n == "logic_apps" || n == "logic-apps")
        .unwrap_or(false);

    if is_nested {
        path.parent().unwrap_or(path).to_path_buf()
    } else {
        path.to_path_buf()
    }
}

fn cosmos_endpoint_of(connections: &[CosmosConnection]) -> String {
    connections
        .iter()
        .map(|c| c.endpoint.trim())
        .find(|e| !e.is_empty())
        .unwrap_or(cosmos_check::EMULATOR_ENDPOINT)
        .to_string()
}

fn cosmos_key_of(connections: &[CosmosConnection]) -> String {
    connections
        .iter()
        .map(|c| c.account_key.trim())
        .find(|k| !k.is_empty())
        .unwrap_or(cosmos_check::EMULATOR_KEY)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_root_steps_out_of_the_logic_apps_folder() {
        assert_eq!(
            project_root_of("/repo/ais_tom_platform/logic_apps"),
            PathBuf::from("/repo/ais_tom_platform")
        );
        assert_eq!(
            project_root_of("/repo/ais_tom_platform/logic-apps"),
            PathBuf::from("/repo/ais_tom_platform")
        );
    }

    #[test]
    fn project_root_keeps_a_directory_that_is_already_the_root() {
        assert_eq!(
            project_root_of("/repo/workflows"),
            PathBuf::from("/repo/workflows")
        );
    }

    #[test]
    fn cosmos_falls_back_to_the_emulator_when_nothing_is_configured() {
        assert_eq!(cosmos_endpoint_of(&[]), cosmos_check::EMULATOR_ENDPOINT);
        assert_eq!(cosmos_key_of(&[]), cosmos_check::EMULATOR_KEY);
    }

    #[test]
    fn cosmos_prefers_a_configured_connection_over_the_emulator_default() {
        let conn = CosmosConnection {
            connection_name: "cosmos".into(),
            display_name: "cosmos".into(),
            endpoint_key: None,
            key_key: None,
            endpoint: "http://localhost:9999".into(),
            account_key: "abc".into(),
        };
        assert_eq!(cosmos_endpoint_of(&[conn.clone()]), "http://localhost:9999");
        assert_eq!(cosmos_key_of(&[conn]), "abc");
    }

    #[test]
    fn a_step_survives_the_review_editor_round_trip() {
        // The review screen edits a step as JSON; anything it renders must parse
        // straight back, or an untouched step would be lost on Apply.
        let step = Step::SendMessage {
            queue: "q".into(),
            body: r#"{"id":"1"}"#.into(),
            content_type: "application/octet-stream".into(),
        };
        let parsed = parse_step(&step_to_json(&step)).unwrap();
        let Step::SendMessage {
            queue,
            body,
            content_type,
        } = parsed
        else {
            panic!("wrong variant")
        };
        assert_eq!(queue, "q");
        assert_eq!(body, r#"{"id":"1"}"#);
        assert_eq!(content_type, "application/octet-stream");
    }

    #[test]
    fn a_bad_edit_is_reported_rather_than_applied() {
        assert!(parse_step("{ not json").is_err());
        assert!(parse_step(r#"{ "action": "no_such_action" }"#).is_err());
    }

    #[test]
    fn only_the_guessed_steps_carry_a_review_hint() {
        assert!(!step_hint(&Step::Sleep { ms: 2000 }).is_empty());
        assert!(!step_hint(&Step::WaitForRun {
            workflow: "W".into(),
            timeout_ms: 60_000,
            expect_status: "Succeeded".into(),
        })
        .is_empty());
        assert!(step_hint(&Step::DrainQueue { queue: "q".into() }).is_empty());
    }
}
