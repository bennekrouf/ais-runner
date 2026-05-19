use std::collections::HashMap;
use dioxus::prelude::*;
use crate::services::{
    config,
    devops_cli::{self, EnvInfo, Pipeline, PipelineRun, ReleaseArtifact},
    azure_cli::AzError,
};

fn run_icon(run: &PipelineRun) -> &'static str {
    match run.state.as_str() {
        "inProgress" | "canceling" => "⏳",
        "completed" => match run.result.as_deref() {
            Some("succeeded")          => "✅",
            Some("partiallySucceeded") => "⚠️",
            Some("canceled")           => "🚫",
            _                          => "❌",
        },
        _ => "⬜",
    }
}

fn short_date(s: &str) -> String {
    s.get(..16).map(|s| s.replace('T', " ")).unwrap_or_else(|| s.to_string())
}

fn fmt_error(e: &AzError) -> String {
    match e {
        AzError::NotLoggedIn => "Azure session expired — click ⟳ in the header to re-authenticate, then Load again.".into(),
        AzError::Other(msg)  => format!("Error: {}", msg),
    }
}

#[derive(Props, Clone, PartialEq)]
pub struct DevOpsPanelProps {
    pub logic_apps_dir: String,
}

#[component]
pub fn DevOpsPanel(props: DevOpsPanelProps) -> Element {
    let dir = props.logic_apps_dir.clone();

    let cfg        = config::load();
    let link       = cfg.get_link(&dir);
    let saved_org  = link.and_then(|l| l.devops_org.clone()).unwrap_or_default();
    let saved_proj = link.and_then(|l| l.devops_project.clone()).unwrap_or_default();

    let mut org     = use_signal(|| saved_org);
    let mut project = use_signal(|| saved_proj);
    let mut status  = use_signal(|| String::new());
    let mut is_err  = use_signal(|| false);

    // ── pipeline list ─────────────────────────────────────────────────────
    let mut pipelines:    Signal<Vec<Pipeline>>    = use_signal(Vec::new);
    let mut sel_pipeline: Signal<Option<u64>>      = use_signal(|| None);
    let mut loading_pipes: Signal<bool>            = use_signal(|| false);

    // ── runs + linked release info ────────────────────────────────────────
    let mut runs:          Signal<Vec<PipelineRun>>                         = use_signal(Vec::new);
    let mut loading_runs:  Signal<bool>                                     = use_signal(|| false);
    // env columns from the auto-detected release definition
    let mut def_envs:      Signal<Vec<EnvInfo>>                             = use_signal(Vec::new);
    // env_name → (release_name, artifact) for the currently-deployed build
    let mut cur_art:       Signal<HashMap<String, (String, ReleaseArtifact)>> = use_signal(HashMap::new);
    // name of the auto-detected release pipeline
    let mut linked_rel_name: Signal<String>       = use_signal(String::new);
    // id of the auto-detected release definition (needed for create_release)
    let mut linked_rel_def_id: Signal<Option<u64>> = use_signal(|| None);
    // artifact alias from the release definition (authoritative source for create_release)
    let mut linked_artifact_alias: Signal<String>  = use_signal(String::new);

    // ── dialogs ───────────────────────────────────────────────────────────
    // (pipeline_id, pipeline_name, recent_branches)
    let mut build_dialog: Signal<Option<(u64, String, Vec<String>)>> = use_signal(|| None);
    // (build_num, build_id, artifact_alias, env_name, branch)
    let mut deploy_dialog: Signal<Option<(String, String, String, String, String)>> = use_signal(|| None);

    // ── filter ────────────────────────────────────────────────────────────
    let mut show_deployed_only: Signal<bool> = use_signal(|| false);

    // ── all branches for the selected pipeline's repo ─────────────────────
    let mut repo_branches: Signal<Vec<String>> = use_signal(Vec::new);

    // ── dialog-local state (hoisted — hooks must be unconditional) ─────────
    let mut deploying    = use_signal(|| false);
    let mut dep_status   = use_signal(|| String::new());
    let mut dep_err      = use_signal(|| false);
    let mut branch_input = use_signal(|| String::new());
    let mut triggering   = use_signal(|| false);
    let mut trig_status  = use_signal(|| String::new());
    let mut trig_err     = use_signal(|| false);

    // Reset dialog state whenever a dialog opens.
    use_effect(move || {
        if deploy_dialog.read().is_some() {
            deploying.set(false);
            dep_status.set(String::new());
            dep_err.set(false);
        }
    });
    use_effect(move || {
        if let Some((_, _, ref branches)) = *build_dialog.read() {
            branch_input.set(branches.first().cloned().unwrap_or_else(|| "main".into()));
            triggering.set(false);
            trig_status.set(String::new());
            trig_err.set(false);
        }
    });

    // ── fetch build pipelines ─────────────────────────────────────────────
    let mut fetch_pipelines = move |o: String, p: String| {
        loading_pipes.set(true);
        status.set(String::new()); is_err.set(false);
        spawn(async move {
            let res = tokio::task::spawn_blocking(move || devops_cli::list_pipelines(&o, &p))
                .await.unwrap_or_else(|e| Err(AzError::Other(e.to_string())));
            loading_pipes.set(false);
            match res {
                Ok(mut list) => {
                    list.sort_by(|a, b| a.folder.cmp(&b.folder).then(a.name.cmp(&b.name)));
                    pipelines.set(list);
                }
                Err(e) => { status.set(fmt_error(&e)); is_err.set(true); }
            }
        });
    };

    // ── auto-load on mount ────────────────────────────────────────────────
    use_effect(move || {
        let o = org.read().trim().to_string();
        let p = project.read().trim().to_string();
        if o.is_empty() || p.is_empty() || !pipelines.read().is_empty() { return; }
        loading_pipes.set(true);
        spawn(async move {
            let res = tokio::task::spawn_blocking(move || devops_cli::list_pipelines(&o, &p))
                .await.unwrap_or_else(|e| Err(AzError::Other(e.to_string())));
            loading_pipes.set(false);
            if let Ok(mut list) = res {
                list.sort_by(|a, b| a.folder.cmp(&b.folder).then(a.name.cmp(&b.name)));
                pipelines.set(list);
            }
        });
    });

    // ── Load button ───────────────────────────────────────────────────────
    let on_load = {
        let dir = dir.clone();
        move |_: MouseEvent| {
            let o = org.read().trim().to_string();
            let p = project.read().trim().to_string();
            if o.is_empty() || p.is_empty() {
                status.set("Enter org URL and project name.".into()); is_err.set(true); return;
            }
            let dir2 = dir.clone(); let o2 = o.clone(); let p2 = p.clone();
            spawn(async move {
                tokio::task::spawn_blocking(move || {
                    let mut cfg = config::load();
                    let link = cfg.workspace_links.entry(dir2).or_default();
                    link.devops_org = Some(o2); link.devops_project = Some(p2);
                    config::save(&cfg);
                }).await.ok();
            });
            pipelines.write().clear(); sel_pipeline.set(None);
            runs.write().clear(); def_envs.write().clear(); cur_art.write().clear();
            linked_rel_name.set(String::new()); linked_rel_def_id.set(None);
            fetch_pipelines(o, p);
        }
    };

    // ── select build pipeline → runs + auto-detect release def ───────────
    let mut select_pipeline = move |id: u64| {
        sel_pipeline.set(Some(id));
        runs.write().clear();
        def_envs.write().clear();
        cur_art.write().clear();
        repo_branches.write().clear();
        linked_rel_name.set(String::new());
        linked_rel_def_id.set(None);
        linked_artifact_alias.set(String::new());
        loading_runs.set(true);
        status.set(String::new()); is_err.set(false);

        let o = org.read().trim().to_string();
        let p = project.read().trim().to_string();

        spawn(async move {
            // fetch runs + find linked release definition + repo branches in parallel
            let o1 = o.clone(); let p1 = p.clone();
            let o2 = o.clone(); let p2 = p.clone();
            let o3 = o.clone(); let p3 = p.clone();

            let (runs_res, rel_res, branches_res) = tokio::join!(
                tokio::task::spawn_blocking(move || devops_cli::list_runs(&o1, &p1, id)),
                tokio::task::spawn_blocking(move || devops_cli::find_release_defs_for_pipeline(&o2, &p2, id)),
                tokio::task::spawn_blocking(move || devops_cli::list_pipeline_branches(&o3, &p3, id)),
            );

            loading_runs.set(false);

            match runs_res.unwrap_or_else(|e| Err(AzError::Other(e.to_string()))) {
                Ok(list) => runs.set(list),
                Err(e)   => { status.set(fmt_error(&e)); is_err.set(true); }
            }

            if let Ok(Ok(branches)) = branches_res {
                repo_branches.set(branches);
            }

            // use the first matched release definition
            if let Ok(Ok(mut defs)) = rel_res {
                if let Some(def) = defs.drain(..).next() {
                    let def_id   = def.id;
                    let def_name = def.name.clone();
                    linked_rel_name.set(def_name);
                    linked_rel_def_id.set(Some(def_id));

                    // fetch artifact alias + env columns in parallel
                    let o_alias = o.clone(); let p_alias = p.clone();
                    let alias_res = tokio::task::spawn_blocking(move || {
                        devops_cli::get_release_def_artifact_alias(&o_alias, &p_alias, def_id)
                    });

                    if let Ok(env_list) = tokio::task::spawn_blocking({
                        let o3 = o.clone(); let p3 = p.clone();
                        move || devops_cli::get_release_definition_envs(&o3, &p3, def_id)
                    }).await.unwrap_or(Err(AzError::Other(String::new()))) {
                        for env_info in &env_list {
                            if env_info.current_release_id == 0 { continue; }
                            let o4 = o.clone(); let p4 = p.clone();
                            let env_name   = env_info.name.clone();
                            let cur_rel_id = env_info.current_release_id;
                            spawn(async move {
                                if let Ok(art) = tokio::task::spawn_blocking(move || {
                                    devops_cli::get_release_artifact(&o4, &p4, cur_rel_id)
                                }).await.unwrap_or(Err(AzError::Other(String::new()))) {
                                    let rel_name = art.release_name.clone();
                                    cur_art.write().insert(env_name, (rel_name, art));
                                }
                            });
                        }
                        def_envs.set(env_list);
                    }
                    // Store the artifact alias from the definition (authoritative)
                    if let Ok(Ok(alias)) = alias_res.await {
                        linked_artifact_alias.set(alias);
                    }
                }
            }
        });
    };

    // ── derived ───────────────────────────────────────────────────────────
    let all_runs     = runs.read().clone();
    let env_names: Vec<String> = def_envs.read().iter().map(|e| e.name.clone()).collect();
    let cur_snap     = cur_art.read().clone();

    // env_name → build_number currently deployed
    let deployed: HashMap<String, String> = cur_snap.iter()
        .map(|(env, (_, art))| (env.clone(), art.build_number.clone()))
        .collect();

    // build numbers that are currently deployed in any env
    let deployed_builds: std::collections::HashSet<String> =
        deployed.values().cloned().collect();

    // apply deployed-only filter
    let displayed_runs: Vec<&PipelineRun> = all_runs.iter()
        .filter(|r| !*show_deployed_only.read() || deployed_builds.contains(&r.name))
        .collect();

    // group pipelines by folder
    let grouped: Vec<(String, Vec<Pipeline>)> = {
        let list = pipelines.read();
        let mut map: Vec<(String, Vec<Pipeline>)> = Vec::new();
        for pipe in list.iter() {
            let folder = if pipe.folder.is_empty() || pipe.folder == "\\" { "General".into() }
                         else { pipe.folder.trim_matches('\\').to_string() };
            if let Some(e) = map.iter_mut().find(|(f, _)| f == &folder) { e.1.push(pipe.clone()); }
            else { map.push((folder, vec![pipe.clone()])); }
        }
        map
    };

    let sel_pipe_name = sel_pipeline.read()
        .and_then(|id| pipelines.read().iter().find(|p| p.id == id).map(|p| p.name.clone()));

    rsx! {
        div { id: "settings-panel",

            // ── header ────────────────────────────────────────────────────
            div { class: "settings-header",
                h2 { "DevOps" }
                div { style: "display:flex; gap:8px; align-items:center; flex:1; margin-left:16px",
                    input {
                        class: "settings-cfg-val", style: "flex:2; min-width:160px",
                        placeholder: "https://dev.azure.com/ORG",
                        value: "{org}", oninput: move |e| org.set(e.value()),
                    }
                    input {
                        class: "settings-cfg-val", style: "flex:1; min-width:120px",
                        placeholder: "Project name",
                        value: "{project}", oninput: move |e| project.set(e.value()),
                    }
                    button {
                        class: "btn btn-run btn-small",
                        disabled: *loading_pipes.read(),
                        onclick: on_load,
                        if *loading_pipes.read() { "Loading…" } else { "Load" }
                    }
                }
            }

            if !status.read().is_empty() {
                div {
                    class: if *is_err.read() { "settings-status error" } else { "settings-status ok" },
                    "{status}"
                }
            }

            div { class: "devops-body",

                // ── left: build pipelines only ────────────────────────────
                div { class: "devops-left",
                    if *loading_pipes.read() {
                        div { class: "devops-empty", "Loading…" }
                    } else if pipelines.read().is_empty() {
                        div { class: "devops-empty", "Enter org & project, then Load." }
                    }
                    for (folder, pipes) in grouped.iter() {
                        div { class: "settings-group",
                            div { class: "settings-group-label", "{folder}" }
                            for pipe in pipes.iter() {
                                {
                                    let pipe_id = pipe.id;
                                    let is_sel  = *sel_pipeline.read() == Some(pipe_id);
                                    rsx! {
                                        div {
                                            class: if is_sel { "devops-pipeline-row selected" } else { "devops-pipeline-row" },
                                            onclick: move |_| select_pipeline(pipe_id),
                                            "{pipe.name}"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // ── right: unified grid ───────────────────────────────────
                div { class: "devops-right",
                    if sel_pipeline.read().is_none() {
                        div { class: "devops-empty", "← Select a build pipeline" }
                    } else {
                        // toolbar
                        div { class: "devops-runs-toolbar",
                            div { style: "flex:1; min-width:0; display:flex; align-items:center; gap:8px; flex-wrap:wrap",
                                span { class: "settings-group-label", style: "border:none; padding:0",
                                    if let Some(n) = &sel_pipe_name { "{n}" }
                                }
                                if !linked_rel_name.read().is_empty() {
                                    span { style: "font-size:10px; color:var(--text3)",
                                        "→ {linked_rel_name}"
                                    }
                                } else if !*loading_runs.read() && !all_runs.is_empty() {
                                    span { style: "font-size:10px; color:var(--text3); font-style:italic",
                                        "no linked release pipeline found"
                                    }
                                }
                                // ── Deployed-only filter — inline with the pipeline name ──
                                label { class: "dv-filter-label",
                                    input {
                                        r#type: "checkbox",
                                        checked: *show_deployed_only.read(),
                                        onchange: move |e| show_deployed_only.set(e.checked()),
                                    }
                                    "Deployed only"
                                }
                            }
                            // ▶ Build trigger
                            {
                                let pipe_id   = *sel_pipeline.read();
                                let pipe_name = sel_pipe_name.clone().unwrap_or_default();
                                let branches  = repo_branches.read().clone();
                                rsx! {
                                    button {
                                        class: "btn btn-run btn-small",
                                        title: "Trigger a new build",
                                        onclick: move |_| {
                                            if let Some(id) = pipe_id {
                                                build_dialog.set(Some((id, pipe_name.clone(), branches.clone())));
                                            }
                                        },
                                        "▶ Build"
                                    }
                                }
                            }
                        }

                        if *loading_runs.read() {
                            div { class: "devops-empty", "Loading runs…" }
                        } else if displayed_runs.is_empty() {
                            div { class: "devops-empty",
                                if all_runs.is_empty() { "No runs found." }
                                else { "No deployed builds. Uncheck 'Deployed only' to see all." }
                            }
                        } else {
                            div { class: "dv-scroll-wrap",
                                table { class: "dv-table",
                                    thead {
                                        tr {
                                            th { class: "dv-th dv-th-build", "Build" }
                                            th { class: "dv-th dv-th-status", "" }
                                            th { class: "dv-th dv-th-branch", "Branch" }
                                            th { class: "dv-th dv-th-commit", "Commit" }
                                            th { class: "dv-th dv-th-date", "Date" }
                                            for env in env_names.iter() {
                                                th { class: "dv-th dv-th-env", "{env}" }
                                            }
                                        }
                                    }
                                    tbody {
                                        for run in displayed_runs.iter() {
                                            {
                                                let icon   = run_icon(run);
                                                let build  = run.name.clone();
                                                let date   = short_date(&run.created_date);
                                                let branch = run.source_branch.trim_start_matches("refs/heads/").to_string();
                                                let commit = run.source_version.get(..8).unwrap_or(&run.source_version).to_string();
                                                let envs_ready = !env_names.is_empty();
                                                // Alias comes from the release definition, not a deployed release
                                                let alias = linked_artifact_alias.read().clone();
                                                // Use this run's own pipeline run ID as the build_id
                                                let run_build_id = run.id.to_string();
                                                let def_id = *linked_rel_def_id.read();

                                                rsx! {
                                                    tr { class: "dv-row",
                                                        td { class: "dv-td dv-td-build",
                                                            span { class: "dv-build-num", "#{build}" }
                                                        }
                                                        td { class: "dv-td dv-td-status", "{icon}" }
                                                        td { class: "dv-td dv-td-branch",
                                                            span { title: "{branch}", "{branch}" }
                                                        }
                                                        td { class: "dv-td dv-td-commit",
                                                            span { class: "dv-build-num", "{commit}" }
                                                        }
                                                        td { class: "dv-td dv-td-date", "{date}" }

                                                        for env_name in env_names.iter() {
                                                            {
                                                                let col     = env_name.clone();
                                                                let build2  = build.clone();
                                                                let branch2 = branch.clone();
                                                                let bid2    = run_build_id.clone();
                                                                let alias2  = alias.clone();

                                                                if let Some(dep) = deployed.get(env_name) {
                                                                    if dep == &build {
                                                                        // ── currently deployed — green, not clickable ──
                                                                        rsx! {
                                                                            td { class: "dv-td dv-td-env",
                                                                                div { class: "dv-env-cell dv-env-current-ok",
                                                                                    title: "Currently deployed to {col}",
                                                                                    span { class: "dv-env-build", "#{build2}" }
                                                                                }
                                                                            }
                                                                        }
                                                                    } else {
                                                                        // ── superseded — clickable to re-deploy ──
                                                                        rsx! {
                                                                            td { class: "dv-td dv-td-env",
                                                                                div {
                                                                                    class: "dv-env-cell dv-env-past dv-env-redeploy",
                                                                                    title: "Re-deploy #{build2} to {col}",
                                                                                    onclick: move |_| {
                                                                                        deploy_dialog.set(Some((
                                                                                            build2.clone(),
                                                                                            bid2.clone(),
                                                                                            alias2.clone(),
                                                                                            col.clone(),
                                                                                            branch2.clone(),
                                                                                        )));
                                                                                    },
                                                                                    "—"
                                                                                }
                                                                            }
                                                                        }
                                                                    }
                                                                } else if !envs_ready {
                                                                    rsx! { td { class: "dv-td dv-td-env dv-skip", "·" } }
                                                                } else if def_id.is_some() {
                                                                    // ── never deployed to this env — clickable ＋ ──
                                                                    rsx! {
                                                                        td { class: "dv-td dv-td-env",
                                                                            div {
                                                                                class: "dv-env-cell dv-env-none",
                                                                                title: "Deploy #{build2} to {col}",
                                                                                onclick: move |_| {
                                                                                    deploy_dialog.set(Some((
                                                                                        build2.clone(),
                                                                                        bid2.clone(),
                                                                                        alias2.clone(),
                                                                                        col.clone(),
                                                                                        branch2.clone(),
                                                                                    )));
                                                                                },
                                                                                "＋"
                                                                            }
                                                                        }
                                                                    }
                                                                } else {
                                                                    rsx! { td { class: "dv-td dv-td-env dv-skip", "—" } }
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
                    }
                }
            }
        }

        // ── Deploy Confirmation Dialog ────────────────────────────────────
        if let Some((build_num, build_id_d, alias_d, env_name, branch_d)) = deploy_dialog.read().clone() {
            {
                let sel_def_id  = *linked_rel_def_id.read();
                let o = org.read().trim().to_string();
                let p = project.read().trim().to_string();
                let is_rollback = deployed_builds.contains(&build_num);

                rsx! {
                    div { id: "dialog-backdrop", onclick: move |_| deploy_dialog.set(None) }
                    div { id: "run-dialog",
                        div { id: "run-dialog-header",
                            div {
                                h3 {
                                    if is_rollback { "↩ Re-deploy to {env_name}" }
                                    else { "🚀 Deploy to {env_name}" }
                                }
                                span { class: "dialog-hint",
                                    if is_rollback {
                                        "Roll back {env_name} to build #{build_num} by creating a new release."
                                    } else {
                                        "Create a new release from build #{build_num} targeting {env_name}."
                                    }
                                }
                            }
                            button { class: "btn-icon", onclick: move |_| deploy_dialog.set(None), "×" }
                        }
                        div { id: "run-dialog-body",
                            div { class: "dialog-blob-info",
                                span { class: "dialog-label", "Build" }
                                span { class: "dialog-blob-container", "#{build_num}" }
                            }
                            div { class: "dialog-blob-info",
                                span { class: "dialog-label", "Target" }
                                span { class: "dialog-blob-container", "{env_name}" }
                            }
                            if !branch_d.is_empty() {
                                div { class: "dialog-blob-info",
                                    span { class: "dialog-label", "Branch" }
                                    code {
                                        style: "flex:1; font-family:monospace; font-size:13px; font-weight:600; color:var(--blue); background:var(--bg2); padding:3px 8px; border-radius:4px",
                                        "{branch_d}"
                                    }
                                    button {
                                        class: "btn btn-small",
                                        title: "Copy branch name",
                                        onclick: {
                                            let b = branch_d.clone();
                                            move |_| {
                                                let b2 = b.clone();
                                                spawn(async move {
                                                    tokio::task::spawn_blocking(move || {
                                                        if let Ok(mut cb) = arboard::Clipboard::new() {
                                                            let _ = cb.set_text(b2);
                                                        }
                                                    }).await.ok();
                                                });
                                            }
                                        },
                                        "⎘"
                                    }
                                }
                            }
                            if !dep_status.read().is_empty() {
                                div {
                                    class: if *dep_err.read() { "settings-status error" } else { "settings-status ok" },
                                    style: "margin-top:6px",
                                    "{dep_status}"
                                }
                            }
                        }
                        div { id: "run-dialog-footer",
                            button { class: "btn btn-small", onclick: move |_| deploy_dialog.set(None), "Cancel" }
                            button {
                                class: "btn btn-run btn-small",
                                disabled: *deploying.read() || sel_def_id.is_none(),
                                onclick: {
                                    let o = o.clone(); let p = p.clone();
                                    let alias = alias_d.clone(); let bid = build_id_d.clone();
                                    move |_| {
                                        if let Some(def_id) = sel_def_id {
                                            deploying.set(true); dep_status.set(String::new());
                                            let o2 = o.clone(); let p2 = p.clone();
                                            let a2 = alias.clone(); let b2 = bid.clone();
                                            spawn(async move {
                                                let res = tokio::task::spawn_blocking(move || {
                                                    devops_cli::create_release(&o2, &p2, def_id, &a2, &b2)
                                                }).await.unwrap_or_else(|e| Err(AzError::Other(e.to_string())));
                                                deploying.set(false);
                                                match res {
                                                    Ok(name) => {
                                                        dep_status.set(format!("✅ {} created", name));
                                                        dep_err.set(false);
                                                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                                        deploy_dialog.set(None);
                                                    }
                                                    Err(e) => { dep_status.set(fmt_error(&e)); dep_err.set(true); }
                                                }
                                            });
                                        }
                                    }
                                },
                                if *deploying.read() { "Creating release…" } else { "🚀 Deploy" }
                            }
                        }
                    }
                }
            }
        }

        // ── Build Run Dialog ──────────────────────────────────────────────
        if let Some((pipe_id, pipe_name, branches)) = build_dialog.read().clone() {
            {
                let o = org.read().trim().to_string();
                let p = project.read().trim().to_string();

                rsx! {
                    div { id: "dialog-backdrop", onclick: move |_| build_dialog.set(None) }
                    div { id: "run-dialog",
                        div { id: "run-dialog-header",
                            div {
                                h3 { "▶ Build — {pipe_name}" }
                                span { class: "dialog-hint", "Queue a new build on Azure DevOps." }
                            }
                            button { class: "btn-icon", onclick: move |_| build_dialog.set(None), "×" }
                        }
                        div { id: "run-dialog-body",
                            label { class: "dialog-label", "Branch" }
                            if branches.is_empty() {
                                // Branches still loading or unavailable — free-text fallback
                                input {
                                    style: "width:100%; box-sizing:border-box",
                                    value: "{branch_input}",
                                    placeholder: "e.g. main",
                                    oninput: move |e| branch_input.set(e.value()),
                                }
                            } else {
                                // Full branch list from the repo — show as scrollable select
                                select {
                                    style: "width:100%; box-sizing:border-box; max-height:220px",
                                    size: "6",   // show 6 rows, scroll for the rest
                                    onchange: move |e| branch_input.set(e.value()),
                                    for b in branches.iter() {
                                        option {
                                            value: "{b}",
                                            selected: *branch_input.read() == *b,
                                            "{b}"
                                        }
                                    }
                                }
                            }
                            if !trig_status.read().is_empty() {
                                div {
                                    class: if *trig_err.read() { "settings-status error" } else { "settings-status ok" },
                                    style: "margin-top:6px",
                                    "{trig_status}"
                                }
                            }
                        }
                        div { id: "run-dialog-footer",
                            button { class: "btn btn-small", onclick: move |_| build_dialog.set(None), "Cancel" }
                            button {
                                class: "btn btn-run btn-small",
                                disabled: *triggering.read(),
                                onclick: {
                                    let o = o.clone(); let p = p.clone();
                                    move |_| {
                                        let branch = branch_input.read().trim().to_string();
                                        if branch.is_empty() { return; }
                                        triggering.set(true); trig_status.set(String::new());
                                        let o2 = o.clone(); let p2 = p.clone(); let b = branch.clone();
                                        spawn(async move {
                                            let res = tokio::task::spawn_blocking(move || {
                                                devops_cli::trigger_build(&o2, &p2, pipe_id, &b)
                                            }).await.unwrap_or_else(|e| Err(AzError::Other(e.to_string())));
                                            triggering.set(false);
                                            match res {
                                                Ok(num) => {
                                                    trig_status.set(format!("✅ Queued #{}", num));
                                                    trig_err.set(false);
                                                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                                    build_dialog.set(None);
                                                    let o3 = org.read().trim().to_string();
                                                    let p3 = project.read().trim().to_string();
                                                    loading_runs.set(true);
                                                    let res2 = tokio::task::spawn_blocking(move || {
                                                        devops_cli::list_runs(&o3, &p3, pipe_id)
                                                    }).await.unwrap_or_else(|e| Err(AzError::Other(e.to_string())));
                                                    loading_runs.set(false);
                                                    if let Ok(list) = res2 { runs.set(list); }
                                                }
                                                Err(e) => { trig_status.set(fmt_error(&e)); trig_err.set(true); }
                                            }
                                        });
                                    }
                                },
                                if *triggering.read() { "Queuing…" } else { "▶ Build" }
                            }
                        }
                    }
                }
            }
        }
    }
}
