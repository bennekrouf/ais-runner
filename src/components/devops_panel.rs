use dioxus::prelude::*;
use crate::services::{
    config::{self, WorkspaceLink},
    devops_cli::{self, Pipeline, PipelineRun},
    azure_cli::AzError,
};

// ── helpers ──────────────────────────────────────────────────────────────────

fn result_icon(run: &PipelineRun) -> &'static str {
    match run.state.as_str() {
        "inProgress" | "canceling" => "⏳",
        "completed" => match run.result.as_deref() {
            Some("succeeded") => "✅",
            Some("partiallySucceeded") => "⚠️",
            Some("canceled") => "🚫",
            _ => "❌",
        },
        _ => "⬜",
    }
}

fn result_label(run: &PipelineRun) -> String {
    match run.state.as_str() {
        "inProgress" => "running".into(),
        "canceling"  => "canceling".into(),
        "completed"  => run.result.clone().unwrap_or_else(|| "unknown".into()),
        s            => s.into(),
    }
}

/// Trim an ISO-8601 timestamp to a human-readable short form.
fn short_date(s: &str) -> String {
    // "2025-05-15T14:32:00.000Z" → "2025-05-15 14:32"
    s.get(..16)
        .map(|s| s.replace('T', " "))
        .unwrap_or_else(|| s.to_string())
}

fn fmt_az_error(e: &AzError) -> String {
    match e {
        AzError::NotLoggedIn => "Not logged in — use ☁ Azure to sign in first.".into(),
        AzError::Other(msg)  => format!("Error: {}", msg),
    }
}

// ── props ─────────────────────────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
pub struct DevOpsPanelProps {
    pub workspace_link: Option<WorkspaceLink>,
    pub logic_apps_dir: String,
    pub is_open: Signal<bool>,
}

// ── component ─────────────────────────────────────────────────────────────────

#[component]
pub fn DevOpsPanel(mut props: DevOpsPanelProps) -> Element {
    // ── config inputs ─────────────────────────────────────────────────────
    let initial_org = props.workspace_link.as_ref()
        .and_then(|l| l.devops_org.clone())
        .unwrap_or_default();
    let initial_proj = props.workspace_link.as_ref()
        .and_then(|l| l.devops_project.clone())
        .unwrap_or_default();

    let mut org     = use_signal(|| initial_org);
    let mut project = use_signal(|| initial_proj);

    // ── data ──────────────────────────────────────────────────────────────
    let mut pipelines:       Signal<Vec<Pipeline>>    = use_signal(Vec::new);
    let mut selected_id:     Signal<Option<u64>>      = use_signal(|| None);
    let mut runs:            Signal<Vec<PipelineRun>> = use_signal(Vec::new);
    let mut loading_pipes:   Signal<bool>             = use_signal(|| false);
    let mut loading_runs:    Signal<bool>             = use_signal(|| false);
    let mut error_msg:       Signal<Option<String>>   = use_signal(|| None);

    let dir = props.logic_apps_dir.clone();

    // ── fetch pipelines ───────────────────────────────────────────────────
    let fetch_pipelines = {
        move |_: MouseEvent| {
            let o = org.read().trim().to_string();
            let p = project.read().trim().to_string();
            if o.is_empty() || p.is_empty() {
                error_msg.set(Some("Set org and project first.".into()));
                return;
            }
            // persist to config
            let dir2 = dir.clone();
            spawn(async move {
                let o2 = o.clone(); let p2 = p.clone();
                tokio::task::spawn_blocking(move || {
                    let mut cfg = config::load();
                    let link = cfg.workspace_links.entry(dir2.clone()).or_default();
                    link.devops_org     = Some(o2);
                    link.devops_project = Some(p2);
                    config::save(&cfg);
                }).await.ok();
            });

            loading_pipes.set(true);
            error_msg.set(None);
            pipelines.write().clear();
            selected_id.set(None);
            runs.write().clear();

            let o3 = org.read().trim().to_string();
            let p3 = project.read().trim().to_string();
            spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    devops_cli::list_pipelines(&o3, &p3)
                }).await.unwrap_or_else(|e| Err(AzError::Other(e.to_string())));

                loading_pipes.set(false);
                match result {
                    Ok(mut list) => {
                        list.sort_by(|a, b| {
                            a.folder.cmp(&b.folder).then(a.name.cmp(&b.name))
                        });
                        pipelines.set(list);
                    }
                    Err(e) => error_msg.set(Some(fmt_az_error(&e))),
                }
            });
        }
    };

    // ── select pipeline → fetch runs ──────────────────────────────────────
    let mut select_pipeline = {
        move |id: u64| {
            selected_id.set(Some(id));
            runs.write().clear();
            loading_runs.set(true);
            error_msg.set(None);

            let o = org.read().trim().to_string();
            let p = project.read().trim().to_string();
            spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    devops_cli::list_runs(&o, &p, id)
                }).await.unwrap_or_else(|e| Err(AzError::Other(e.to_string())));

                loading_runs.set(false);
                match result {
                    Ok(list) => runs.set(list),
                    Err(e)   => error_msg.set(Some(fmt_az_error(&e))),
                }
            });
        }
    };

    // ── group pipelines by folder ─────────────────────────────────────────
    let grouped: Vec<(String, Vec<Pipeline>)> = {
        let list = pipelines.read();
        let mut map: Vec<(String, Vec<Pipeline>)> = Vec::new();
        for pipe in list.iter() {
            let folder = if pipe.folder.is_empty() || pipe.folder == "\\" {
                "—".to_string()
            } else {
                pipe.folder.trim_matches('\\').to_string()
            };
            if let Some(entry) = map.iter_mut().find(|(f, _)| f == &folder) {
                entry.1.push(pipe.clone());
            } else {
                map.push((folder, vec![pipe.clone()]));
            }
        }
        map
    };

    let selected_pipeline_name = {
        let id = *selected_id.read();
        id.and_then(|id| pipelines.read().iter().find(|p| p.id == id).map(|p| p.name.clone()))
    };

    rsx! {
        div { id: "devops-panel",

            // ── header ────────────────────────────────────────────────────
            div { class: "az-panel-header",
                span { style: "font-weight:600; font-size:0.95rem", "🚀 DevOps Pipelines" }
                button {
                    class: "btn-icon",
                    title: "Close",
                    onclick: move |_| props.is_open.set(false),
                    "×"
                }
            }

            // ── config row ────────────────────────────────────────────────
            div { class: "devops-config-row",
                input {
                    class: "devops-input",
                    placeholder: "https://dev.azure.com/ORG",
                    value: "{org}",
                    oninput: move |e| org.set(e.value()),
                }
                input {
                    class: "devops-input",
                    placeholder: "Project name",
                    value: "{project}",
                    oninput: move |e| project.set(e.value()),
                }
                button {
                    class: "btn btn-small btn-fetch",
                    disabled: *loading_pipes.read(),
                    onclick: fetch_pipelines,
                    if *loading_pipes.read() { "Loading…" } else { "Load" }
                }
            }

            // ── error banner ──────────────────────────────────────────────
            if let Some(msg) = error_msg.read().as_ref() {
                div { class: "az-panel-status", "{msg}" }
            }

            // ── body: two-column split ────────────────────────────────────
            div { class: "devops-body",

                // ── left: pipeline list ───────────────────────────────────
                div { class: "devops-left",
                    if pipelines.read().is_empty() && !*loading_pipes.read() {
                        div { class: "devops-empty", "Enter org & project, then click Load." }
                    }
                    for (folder, pipes) in grouped.iter() {
                        div { class: "devops-folder-group",
                            div { class: "devops-folder-label", "{folder}" }
                            for pipe in pipes.iter() {
                                {
                                    let pipe_id = pipe.id;
                                    let is_sel  = *selected_id.read() == Some(pipe_id);
                                    rsx! {
                                        div {
                                            class: if is_sel { "devops-pipeline-row selected" } else { "devops-pipeline-row" },
                                            onclick: move |_| select_pipeline(pipe_id),
                                            span { class: "devops-pipe-name", "{pipe.name}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // ── right: run list ───────────────────────────────────────
                div { class: "devops-right",
                    if let Some(name) = &selected_pipeline_name {
                        div { class: "devops-runs-header", "Runs — {name}" }
                    }
                    if *loading_runs.read() {
                        div { class: "az-panel-loading", "Loading runs…" }
                    } else if runs.read().is_empty() && selected_id.read().is_some() {
                        div { class: "devops-empty", "No runs found." }
                    } else if selected_id.read().is_none() {
                        div { class: "devops-empty", "← Select a pipeline." }
                    }
                    for run in runs.read().iter() {
                        {
                            let icon  = result_icon(run);
                            let label = result_label(run);
                            let build = run.name.clone();
                            let date  = short_date(&run.created_date);
                            rsx! {
                                div { class: "devops-run-row",
                                    span { class: "devops-run-icon", "{icon}" }
                                    span { class: "devops-run-build", "#{build}" }
                                    span { class: "devops-run-status", "{label}" }
                                    span { class: "devops-run-date", "{date}" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
