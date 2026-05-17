use std::collections::HashMap;
use dioxus::prelude::*;
use crate::services::{
    config,
    devops_cli::{self, EnvInfo, Pipeline, PipelineRun, ReleaseArtifact, ReleaseDefinition, ReleaseEnvStatus, ReleaseInfo},
    azure_cli::AzError,
};

// ── helpers ───────────────────────────────────────────────────────────────────

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
        AzError::NotLoggedIn => "Not logged in — use ☁ Azure to sign in first.".into(),
        AzError::Other(msg)  => format!("Error: {}", msg),
    }
}

// ── props ─────────────────────────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
pub struct DevOpsPanelProps {
    pub logic_apps_dir: String,
}

// ── component ─────────────────────────────────────────────────────────────────

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
    // "builds" | "releases"
    let mut mode    = use_signal(|| "releases".to_string());

    // inject column-resize JS once on mount
    use_effect(move || {
        document::eval(r#"
            (function() {
                function initGridResize() {
                    document.querySelectorAll('.dv-grid').forEach(function(grid) {
                        if (grid._resizeInit) return;
                        grid._resizeInit = true;
                        grid.querySelectorAll('.dv-header-row > div').forEach(function(th, colIdx) {
                            var handle = document.createElement('div');
                            handle.className = 'dv-col-resize-handle';
                            th.appendChild(handle);
                            handle.addEventListener('mousedown', function(e) {
                                e.preventDefault();
                                handle.classList.add('dragging');
                                var startX = e.clientX;
                                var startW = th.offsetWidth;
                                function onMove(e) {
                                    var newW = Math.max(60, startW + e.clientX - startX);
                                    grid.querySelectorAll('.dv-header-row > div, .dv-row > div').forEach(function(row) {
                                        // find all cells at this column index
                                    });
                                    // update every row's nth cell
                                    [].forEach.call(grid.querySelectorAll('.dv-header-row, .dv-row'), function(row) {
                                        var cell = row.children[colIdx];
                                        if (cell) cell.style.width = newW + 'px';
                                    });
                                }
                                function onUp() {
                                    handle.classList.remove('dragging');
                                    document.removeEventListener('mousemove', onMove);
                                    document.removeEventListener('mouseup', onUp);
                                }
                                document.addEventListener('mousemove', onMove);
                                document.addEventListener('mouseup', onUp);
                            });
                        });
                    });
                }
                initGridResize();
                // re-init when grid is added to the DOM (tab switches)
                new MutationObserver(function() { initGridResize(); })
                    .observe(document.body, { childList: true, subtree: true });
            })();
        "#);
    });

    // ── build pipeline state ──────────────────────────────────────────────
    let mut pipelines:     Signal<Vec<Pipeline>>    = use_signal(Vec::new);
    let mut sel_pipeline:  Signal<Option<u64>>      = use_signal(|| None);
    let mut runs:          Signal<Vec<PipelineRun>> = use_signal(Vec::new);
    let mut loading_pipes: Signal<bool>             = use_signal(|| false);
    let mut loading_runs:  Signal<bool>             = use_signal(|| false);
    // (pipeline_id, pipeline_name, recent_branches)
    let mut build_dialog: Signal<Option<(u64, String, Vec<String>)>> = use_signal(|| None);

    // ── release pipeline state ────────────────────────────────────────────
    let mut rel_defs:         Signal<Vec<ReleaseDefinition>>       = use_signal(Vec::new);
    let mut sel_rel_def:      Signal<Option<u64>>                  = use_signal(|| None);
    let mut releases:         Signal<Vec<ReleaseInfo>>             = use_signal(Vec::new);
    // release_id → artifact details (fetched in background per release)
    let mut rel_artifacts:    Signal<HashMap<u64, ReleaseArtifact>> = use_signal(HashMap::new);
    // environments from the definition (rank-ordered, each with currentRelease.id)
    let mut def_envs:         Signal<Vec<EnvInfo>>                 = use_signal(Vec::new);
    // env_name → (release_name, artifact) for the current deployment
    let mut current_rel_art:  Signal<HashMap<String, (String, ReleaseArtifact)>> = use_signal(HashMap::new);
    let mut loading_defs:     Signal<bool>                         = use_signal(|| false);
    let mut loading_rels:     Signal<bool>                         = use_signal(|| false);
    let mut deployed_only:    Signal<bool>                         = use_signal(|| false);
    // (release_name, build_number, build_id, artifact_alias, env_name)
    let mut deploy_dialog:    Signal<Option<(String, String, String, String, String)>> = use_signal(|| None);

    // ── shared: fetch build pipelines ─────────────────────────────────────
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

    // ── shared: fetch release definitions ─────────────────────────────────
    let mut fetch_rel_defs = move |o: String, p: String| {
        loading_defs.set(true);
        status.set(String::new()); is_err.set(false);
        spawn(async move {
            let res = tokio::task::spawn_blocking(move || devops_cli::list_release_definitions(&o, &p))
                .await.unwrap_or_else(|e| Err(AzError::Other(e.to_string())));
            loading_defs.set(false);
            match res {
                Ok(list) => {
                    rel_defs.set(list);
                }
                Err(e) => { status.set(fmt_error(&e)); is_err.set(true); }
            }
        });
    };

    // ── auto-load on mount ────────────────────────────────────────────────
    use_effect(move || {
        let o = org.read().trim().to_string();
        let p = project.read().trim().to_string();
        if o.is_empty() || p.is_empty() { return; }
        if mode.read().as_str() == "releases" && rel_defs.read().is_empty() {
            fetch_rel_defs(o.clone(), p.clone());
        }
        if pipelines.read().is_empty() {
            fetch_pipelines(o, p);
        }
    });

    // ── save config + full reload ─────────────────────────────────────────
    let on_load = {
        let dir = dir.clone();
        move |_: MouseEvent| {
            let o = org.read().trim().to_string();
            let p = project.read().trim().to_string();
            if o.is_empty() || p.is_empty() {
                status.set("Enter org URL and project name first.".into());
                is_err.set(true);
                return;
            }
            let dir2 = dir.clone();
            let o2 = o.clone(); let p2 = p.clone();
            spawn(async move {
                tokio::task::spawn_blocking(move || {
                    let mut cfg = config::load();
                    let link = cfg.workspace_links.entry(dir2).or_default();
                    link.devops_org     = Some(o2);
                    link.devops_project = Some(p2);
                    config::save(&cfg);
                }).await.ok();
            });
            pipelines.write().clear(); sel_pipeline.set(None); runs.write().clear();
            rel_defs.write().clear();  sel_rel_def.set(None);  releases.write().clear();
            def_envs.write().clear(); rel_artifacts.write().clear(); current_rel_art.write().clear();
            let o3 = o.clone(); let p3 = p.clone();
            let o4 = o.clone(); let p4 = p.clone();
            fetch_pipelines(o3, p3);
            fetch_rel_defs(o4, p4);
        }
    };

    let on_refresh = move |_: MouseEvent| {
        let o = org.read().trim().to_string();
        let p = project.read().trim().to_string();
        if o.is_empty() || p.is_empty() { return; }
        runs.write().clear();
        releases.write().clear();
        def_envs.write().clear();
        rel_artifacts.write().clear();
        current_rel_art.write().clear();
        let o2 = o.clone(); let p2 = p.clone();
        fetch_pipelines(o, p);
        fetch_rel_defs(o2, p2);
    };

    // ── select build pipeline → fetch runs ───────────────────────────────
    let mut select_pipeline = move |id: u64| {
        sel_pipeline.set(Some(id));
        runs.write().clear();
        loading_runs.set(true);
        let o = org.read().trim().to_string();
        let p = project.read().trim().to_string();
        spawn(async move {
            let res = tokio::task::spawn_blocking(move || devops_cli::list_runs(&o, &p, id))
                .await.unwrap_or_else(|e| Err(AzError::Other(e.to_string())));
            loading_runs.set(false);
            match res {
                Ok(list) => runs.set(list),
                Err(e)   => { status.set(fmt_error(&e)); is_err.set(true); }
            }
        });
    };

    // ── select release definition → fetch envs + releases in parallel ──────
    let mut select_rel_def = move |id: u64| {
        sel_rel_def.set(Some(id));
        releases.write().clear();
        def_envs.write().clear();
        rel_artifacts.write().clear();
        current_rel_art.write().clear();
        loading_rels.set(true);
        let o = org.read().trim().to_string();
        let p = project.read().trim().to_string();
        spawn(async move {
            let o1 = o.clone(); let p1 = p.clone();
            let o2 = o.clone(); let p2 = p.clone();

            let (envs_res, rels_res) = tokio::join!(
                tokio::task::spawn_blocking(move || devops_cli::get_release_definition_envs(&o1, &p1, id)),
                tokio::task::spawn_blocking(move || devops_cli::list_releases(&o2, &p2, id)),
            );

            loading_rels.set(false);

            // fetch the current release for each env (single show call gives name + artifact)
            if let Ok(Ok(env_list)) = envs_res {
                for env_info in &env_list {
                    if env_info.current_release_id == 0 { continue; }
                    let o3 = o.clone(); let p3 = p.clone();
                    let env_name   = env_info.name.clone();
                    let cur_rel_id = env_info.current_release_id;
                    spawn(async move {
                        if let Ok(art) = tokio::task::spawn_blocking(move || {
                            devops_cli::get_release_artifact(&o3, &p3, cur_rel_id)
                        }).await.unwrap_or(Err(AzError::Other(String::new()))) {
                            let rel_name = art.release_name.clone();
                            current_rel_art.write().insert(env_name, (rel_name, art));
                        }
                    });
                }
                def_envs.set(env_list);
            }

            match rels_res.unwrap_or_else(|e| Err(AzError::Other(e.to_string()))) {
                Err(e) => { status.set(fmt_error(&e)); is_err.set(true); }
                Ok(list) => {
                    let release_ids: Vec<u64> = list.iter().map(|r| r.id).collect();
                    releases.set(list);
                    // fetch artifact details per release in background (for History view)
                    for rid in release_ids {
                        let o3 = o.clone(); let p3 = p.clone();
                        spawn(async move {
                            if let Ok(art) = tokio::task::spawn_blocking(move || {
                                devops_cli::get_release_artifact(&o3, &p3, rid)
                            }).await.unwrap_or(Err(AzError::Other(String::new()))) {
                                rel_artifacts.write().insert(rid, art);
                            }
                        });
                    }
                }
            }
        });
    };

    // env column names in rank order (from definition)
    let env_columns: Vec<String> = def_envs.read().iter().map(|e| e.name.clone()).collect();

    let all_releases  = releases.read().clone();
    let arts_snap     = rel_artifacts.read().clone();
    let cur_art_snap  = current_rel_art.read().clone();

    // env_name → current_release_id (authoritative from definition)
    let current_ids: HashMap<String, u64> = def_envs.read().iter()
        .map(|e| (e.name.clone(), e.current_release_id))
        .collect();

    let list_ids: std::collections::HashSet<u64> = all_releases.iter().map(|r| r.id).collect();

    // Synthetic ReleaseInfo rows for current releases that aren't in the top-20 list.
    // These are pinned at the top of the grid so they always appear.
    let pinned_releases: Vec<ReleaseInfo> = {
        let mut seen_ids = std::collections::HashSet::<u64>::new();
        let mut pinned = Vec::new();
        for (_, &cid) in &current_ids {
            if cid == 0 || list_ids.contains(&cid) || !seen_ids.insert(cid) { continue; }
            if let Some((rel_name, art)) = cur_art_snap.get(
                current_ids.iter().find(|(_, &v)| v == cid).map(|(k, _)| k).unwrap_or(&String::new())
            ) {
                pinned.push(ReleaseInfo {
                    id:           cid,
                    name:         rel_name.clone(),
                    build_number: art.build_number.clone(),
                    branch:       art.branch.clone(),
                    commit:       art.commit.clone(),
                    created_on:   art.created_on.clone(),
                    environments: art.environments.clone(),
                });
            }
        }
        pinned
    };

    let visible_releases: Vec<&ReleaseInfo> = all_releases.iter().filter(|r| {
        if !*deployed_only.read() { return true; }
        if current_ids.values().any(|&cid| cid == r.id) { return true; }
        arts_snap.get(&r.id)
            .map(|a| a.environments.iter().any(|e| e.status == "succeeded" || e.status == "partiallySucceeded"))
            .unwrap_or(false)
    }).collect();

    let sel_rel_name = sel_rel_def.read()
        .and_then(|id| rel_defs.read().iter().find(|d| d.id == id).map(|d| d.name.clone()));
    let sel_pipe_name = sel_pipeline.read()
        .and_then(|id| pipelines.read().iter().find(|p| p.id == id).map(|p| p.name.clone()));

    // group pipelines by folder
    let grouped_pipes: Vec<(String, Vec<Pipeline>)> = {
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

    let is_releases = mode.read().as_str() == "releases";
    let loading_left  = if is_releases { *loading_defs.read() } else { *loading_pipes.read() };
    let loading_right = if is_releases { *loading_rels.read() } else { *loading_runs.read() };

    rsx! {
        div { id: "settings-panel",

            // ── header ────────────────────────────────────────────────────
            div { class: "settings-header",
                // mode tabs
                div { class: "settings-tabs", style: "border:none; padding:0; margin-right:12px",
                    button {
                        class: if is_releases { "settings-tab" } else { "settings-tab active" },
                        onclick: move |_| mode.set("builds".into()),
                        "Builds"
                    }
                    button {
                        class: if is_releases { "settings-tab active" } else { "settings-tab" },
                        onclick: move |_| mode.set("releases".into()),
                        "Releases"
                    }
                }
                div { style: "display:flex; gap:8px; align-items:center; flex:1",
                    input {
                        class: "settings-cfg-val",
                        style: "flex:2; min-width:160px",
                        placeholder: "https://dev.azure.com/ORG",
                        value: "{org}",
                        oninput: move |e| org.set(e.value()),
                    }
                    input {
                        class: "settings-cfg-val",
                        style: "flex:1; min-width:120px",
                        placeholder: "Project name",
                        value: "{project}",
                        oninput: move |e| project.set(e.value()),
                    }
                    button {
                        class: "btn btn-run btn-small",
                        disabled: loading_left,
                        onclick: on_load,
                        if loading_left { "Loading…" } else { "Load" }
                    }
                    button {
                        class: "btn btn-small",
                        title: "Refresh",
                        disabled: loading_left || org.read().trim().is_empty(),
                        onclick: on_refresh,
                        "↻"
                    }
                }
            }

            if !status.read().is_empty() {
                div {
                    class: if *is_err.read() { "settings-status error" } else { "settings-status ok" },
                    "{status}"
                }
            }

            // ── body ──────────────────────────────────────────────────────
            div { class: "devops-body",

                // ── left list ─────────────────────────────────────────────
                div { class: "devops-left",
                    if loading_left {
                        div { class: "devops-empty", "Loading…" }
                    } else if is_releases {
                        // release definitions list
                        if rel_defs.read().is_empty() {
                            div { class: "devops-empty", "No release definitions found." }
                        }
                        div { class: "settings-group",
                            div { class: "settings-group-label", "Release Pipelines" }
                            for def in rel_defs.read().iter() {
                                {
                                    let def_id = def.id;
                                    let is_sel = *sel_rel_def.read() == Some(def_id);
                                    rsx! {
                                        div {
                                            class: if is_sel { "devops-pipeline-row selected" } else { "devops-pipeline-row" },
                                            onclick: move |_| select_rel_def(def_id),
                                            "{def.name}"
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        // build pipeline list
                        if pipelines.read().is_empty() {
                            div { class: "devops-empty", "No pipelines found." }
                        }
                        for (folder, pipes) in grouped_pipes.iter() {
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
                }

                // ── right grid ────────────────────────────────────────────
                div { class: "devops-right",
                    if is_releases {
                        // ── RELEASES MODE ─────────────────────────────────
                        if sel_rel_def.read().is_none() {
                            div { class: "devops-empty", "← Select a release pipeline" }
                        } else {
                            div { class: "devops-runs-toolbar",
                                span { class: "settings-group-label", style: "border:none; padding:0; flex:1",
                                    if let Some(n) = &sel_rel_name { "Releases — {n}" }
                                }
                                label { class: "devops-filter-label",
                                    input {
                                        r#type: "checkbox",
                                        checked: *deployed_only.read(),
                                        onchange: move |e| deployed_only.set(e.checked()),
                                    }
                                    " Deployed only"
                                }
                            }

                            if loading_right {
                                div { class: "devops-empty", "Loading releases…" }
                            } else {
                                div { class: "dv-grid",
                                    div { class: "dv-header-row",
                                        div { class: "dv-col-build", "Release" }
                                        div { class: "dv-col-build", "Build" }
                                        div { class: "dv-col-branch", "Branch" }
                                        div { class: "dv-col-commit", "Commit" }
                                        div { class: "dv-col-date", "Date" }
                                        for col in env_columns.iter() {
                                            div { class: "dv-col-env", "{col}" }
                                        }
                                    }
                                    if visible_releases.is_empty() && pinned_releases.is_empty() {
                                        div { class: "devops-empty",
                                            if *deployed_only.read() { "No deployed releases." } else { "No releases found." }
                                        }
                                    }
                                    // pinned rows first (current releases not in top-20)
                                    for rel in pinned_releases.iter().chain(visible_releases.iter().copied()) {
                                        {
                                            let rel_id  = rel.id;
                                            let date    = short_date(&rel.created_on);
                                            let release = rel.name.clone();
                                            // prefer history cache; fall back to current-release cache
                                            let art = arts_snap.get(&rel_id).cloned()
                                                .or_else(|| cur_art_snap.values()
                                                    .find(|(_, a)| a.release_id == rel_id)
                                                    .map(|(_, a)| a.clone()));
                                            let build   = art.as_ref().map(|a| a.build_number.clone())
                                                .filter(|s| !s.is_empty() && s != "—")
                                                .unwrap_or_else(|| "…".into());
                                            let branch         = art.as_ref().map(|a| a.branch.clone()).unwrap_or_default();
                                            let commit         = art.as_ref().map(|a| a.commit.clone()).unwrap_or_default();
                                            let build_id       = art.as_ref().map(|a| a.build_id.clone()).unwrap_or_default();
                                            let artifact_alias = art.as_ref().map(|a| a.artifact_alias.clone()).unwrap_or_default();
                                            let env_map: HashMap<String, ReleaseEnvStatus> = art.as_ref()
                                                .map(|a| a.environments.iter().map(|e| (e.name.clone(), e.clone())).collect())
                                                .unwrap_or_default();
                                            rsx! {
                                                div { class: "dv-row",
                                                    div { class: "dv-col-build",
                                                        span { class: "dv-build-num", "{release}" }
                                                    }
                                                    div { class: "dv-col-build",
                                                        span { class: "dv-build-num", "#{build}" }
                                                    }
                                                    div { class: "dv-col-branch", title: "{branch}", "{branch}" }
                                                    div { class: "dv-col-commit",
                                                        span { class: "dv-build-num", "{commit}" }
                                                    }
                                                    div { class: "dv-col-date", "{date}" }
                                                    for col in env_columns.iter() {
                                                        {
                                                            let col_name   = col.clone();
                                                            let build_ref  = build.clone();
                                                            let art_loaded = art.is_some();
                                                            // is this release the currently deployed one for this env?
                                                            let is_current = current_ids.get(col)
                                                                .map(|&cid| cid == rel_id)
                                                                .unwrap_or(false);

                                                            if let Some(env) = env_map.get(col) {
                                                                let status = env.status.clone();
                                                                let cls = if is_current {
                                                                    match status.as_str() {
                                                                        "succeeded"          => "dv-cell dv-env-cell dv-env-current-ok",
                                                                        "partiallySucceeded" => "dv-cell dv-env-cell dv-env-current-warn",
                                                                        "failed"             => "dv-cell dv-env-cell dv-env-current-fail",
                                                                        "inProgress"         => "dv-cell dv-env-cell dv-env-current-running",
                                                                        _                    => "dv-cell dv-env-cell dv-env-current-ok",
                                                                    }
                                                                } else {
                                                                    "dv-cell dv-env-cell dv-env-past"
                                                                };
                                                                let label = if art_loaded { build_ref.clone() } else { "…".into() };
                                                                rsx! {
                                                                    div { class: cls, title: "{status}",
                                                                        span { class: "dv-env-build", "#{label}" }
                                                                    }
                                                                }
                                                            } else if !art_loaded {
                                                                rsx! { div { class: "dv-cell dv-env-cell dv-env-pending", "·" } }
                                                            } else {
                                                                let rel_name_d  = release.clone();
                                                                let build_num_d = build_ref.clone();
                                                                let build_id_d  = build_id.clone();
                                                                let alias_d     = artifact_alias.clone();
                                                                let env_d       = col_name.clone();
                                                                rsx! {
                                                                    div {
                                                                        class: "dv-cell dv-env-cell dv-env-none",
                                                                        title: "Click to deploy {build_ref} to {col_name}",
                                                                        onclick: move |_| {
                                                                            deploy_dialog.set(Some((
                                                                                rel_name_d.clone(),
                                                                                build_num_d.clone(),
                                                                                build_id_d.clone(),
                                                                                alias_d.clone(),
                                                                                env_d.clone(),
                                                                            )));
                                                                        },
                                                                        "＋"
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
                    } else {
                        // ── BUILDS MODE ───────────────────────────────────
                        if sel_pipeline.read().is_none() {
                            div { class: "devops-empty", "← Select a build pipeline" }
                        } else {
                            div { class: "devops-runs-toolbar",
                                span { class: "settings-group-label", style: "border:none; padding:0; flex:1",
                                    if let Some(n) = &sel_pipe_name { "Runs — {n}" }
                                }
                                {
                                    let pipe_id   = *sel_pipeline.read();
                                    let pipe_name = sel_pipe_name.clone().unwrap_or_default();
                                    let branches: Vec<String> = {
                                        let mut seen = Vec::<String>::new();
                                        for r in runs.read().iter() {
                                            let b = r.source_branch.trim_start_matches("refs/heads/").to_string();
                                            if !b.is_empty() && !seen.contains(&b) { seen.push(b); }
                                        }
                                        seen
                                    };
                                    rsx! {
                                        button {
                                            class: "btn btn-run btn-small",
                                            title: "Trigger a new build run",
                                            onclick: move |_| {
                                                if let Some(id) = pipe_id {
                                                    build_dialog.set(Some((id, pipe_name.clone(), branches.clone())));
                                                }
                                            },
                                            "▶ Run"
                                        }
                                    }
                                }
                            }
                            if loading_right {
                                div { class: "devops-empty", "Loading runs…" }
                            } else {
                                div { class: "dv-grid",
                                    div { class: "dv-header-row",
                                        div { class: "dv-col-build", "Build" }
                                        div { class: "dv-col-status", "" }
                                        div { class: "dv-col-branch", "Branch" }
                                        div { class: "dv-col-commit", "Commit" }
                                        div { class: "dv-col-date", "Date" }
                                    }
                                    if runs.read().is_empty() {
                                        div { class: "devops-empty", "No runs found." }
                                    }
                                    for run in runs.read().iter() {
                                        {
                                            let icon   = run_icon(run);
                                            let build  = run.name.clone();
                                            let date   = short_date(&run.created_date);
                                            let branch = run.source_branch
                                                .trim_start_matches("refs/heads/").to_string();
                                            let commit = run.source_version.get(..8)
                                                .unwrap_or(&run.source_version).to_string();
                                            rsx! {
                                                div { class: "dv-row",
                                                    div { class: "dv-col-build",
                                                        span { class: "dv-build-num", "#{build}" }
                                                    }
                                                    div { class: "dv-col-status", "{icon}" }
                                                    div { class: "dv-col-branch", title: "{branch}", "{branch}" }
                                                    div { class: "dv-col-commit",
                                                        span { class: "dv-build-num", "{commit}" }
                                                    }
                                                    div { class: "dv-col-date", "{date}" }
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

        // ── Deploy Release Dialog ─────────────────────────────────────────
        if let Some((rel_name, build_num, build_id_d, alias_d, env_name)) = deploy_dialog.read().clone() {
            {
                let sel_def_id = *sel_rel_def.read();
                let mut deploying   = use_signal(|| false);
                let mut dep_status  = use_signal(|| String::new());
                let mut dep_err     = use_signal(|| false);

                let o = org.read().trim().to_string();
                let p = project.read().trim().to_string();

                rsx! {
                    div {
                        id: "dialog-backdrop",
                        onclick: move |_| deploy_dialog.set(None),
                    }
                    div { id: "run-dialog",
                        div { id: "run-dialog-header",
                            div {
                                h3 { "Deploy to {env_name}" }
                                span { class: "dialog-hint",
                                    "Create a new release from {rel_name} (build #{build_num}) targeting {env_name}."
                                }
                            }
                            button {
                                class: "btn-icon",
                                onclick: move |_| deploy_dialog.set(None),
                                "×"
                            }
                        }

                        div { id: "run-dialog-body",
                            div { class: "dialog-blob-info",
                                span { class: "dialog-label", "Release" }
                                span { class: "dialog-blob-container", "{rel_name}" }
                            }
                            div { class: "dialog-blob-info",
                                span { class: "dialog-label", "Build" }
                                span { class: "dialog-blob-container", "#{build_num}" }
                            }
                            div { class: "dialog-blob-info",
                                span { class: "dialog-label", "Target" }
                                span { class: "dialog-blob-container", "{env_name}" }
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
                            button {
                                class: "btn btn-small",
                                onclick: move |_| deploy_dialog.set(None),
                                "Cancel"
                            }
                            button {
                                class: "btn btn-run btn-small",
                                disabled: *deploying.read() || build_id_d.is_empty(),
                                onclick: {
                                    let o = o.clone(); let p = p.clone();
                                    let alias = alias_d.clone();
                                    let bid   = build_id_d.clone();
                                    move |_| {
                                        if let Some(def_id) = sel_def_id {
                                            deploying.set(true);
                                            dep_status.set(String::new());
                                            let o2 = o.clone(); let p2 = p.clone();
                                            let alias2 = alias.clone(); let bid2 = bid.clone();
                                            spawn(async move {
                                                let res = tokio::task::spawn_blocking(move || {
                                                    devops_cli::create_release(&o2, &p2, def_id, &alias2, &bid2)
                                                }).await.unwrap_or_else(|e| Err(AzError::Other(e.to_string())));
                                                deploying.set(false);
                                                match res {
                                                    Ok(name) => {
                                                        dep_status.set(format!("✅ {} created — deployment started", name));
                                                        dep_err.set(false);
                                                        // close after 2s and refresh
                                                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                                        deploy_dialog.set(None);
                                                    }
                                                    Err(e) => {
                                                        dep_status.set(fmt_error(&e));
                                                        dep_err.set(true);
                                                    }
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
                let default_branch = branches.first().cloned().unwrap_or_else(|| "main".into());
                let mut branch_input = use_signal(|| default_branch.clone());
                let mut triggering   = use_signal(|| false);
                let mut trig_status  = use_signal(|| String::new());
                let mut trig_err     = use_signal(|| false);

                let o = org.read().trim().to_string();
                let p = project.read().trim().to_string();

                rsx! {
                    div {
                        id: "dialog-backdrop",
                        onclick: move |_| build_dialog.set(None),
                    }
                    div { id: "run-dialog",
                        div { id: "run-dialog-header",
                            div {
                                h3 { "▶  {pipe_name}" }
                                span { class: "dialog-hint",
                                    "Queue a new build run on Azure DevOps."
                                }
                            }
                            button {
                                class: "btn-icon",
                                onclick: move |_| build_dialog.set(None),
                                "×"
                            }
                        }

                        div { id: "run-dialog-body",
                            // branch selector
                            label { class: "dialog-label", "Branch" }
                            input {
                                id: "run-dialog-blobname",
                                value: "{branch_input}",
                                placeholder: "e.g. main",
                                list: "build-branch-list",
                                oninput: move |e| branch_input.set(e.value()),
                            }
                            datalist { id: "build-branch-list",
                                for b in branches.iter() {
                                    option { value: "{b}" }
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
                            button {
                                class: "btn btn-small",
                                onclick: move |_| build_dialog.set(None),
                                "Cancel"
                            }
                            button {
                                class: "btn btn-run btn-small",
                                disabled: *triggering.read(),
                                onclick: {
                                    let o = o.clone(); let p = p.clone();
                                    move |_| {
                                        let branch = branch_input.read().trim().to_string();
                                        if branch.is_empty() { return; }
                                        triggering.set(true);
                                        trig_status.set(String::new());
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
                                                    // refresh runs after a short delay
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
                                                Err(e) => {
                                                    trig_status.set(fmt_error(&e));
                                                    trig_err.set(true);
                                                }
                                            }
                                        });
                                    }
                                },
                                if *triggering.read() { "Queuing…" } else { "▶  Run" }
                            }
                        }
                    }
                }
            }
        }
    }
}
