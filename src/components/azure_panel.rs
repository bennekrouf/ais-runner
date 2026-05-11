use dioxus::prelude::*;
use std::collections::{HashSet, HashMap};
use crate::services::{
    azure_sync::{self, AzureWorkflow, LogicAppSite},
    azure_cli::{self, AzError},
    config,
};

fn fmt_az_error(e: &AzError) -> String {
    if let AzError::Other(msg) = e {
        if msg.contains("AuthorizationFailed") {
            let user = msg.split("client '").nth(1)
                .and_then(|s| s.split("' with").next())
                .unwrap_or("your account");
            let site = msg.split("/sites/").nth(1)
                .and_then(|s| s.split(['\'', '"', ' ', '\\']).next())
                .unwrap_or("the Logic App");
            return format!(
                "⛔ {user} is not authorized to read workflows on {site}. \
                Activate your PIM role, then click 🔐 Re-login."
            );
        }
    }
    format!("Error: {:?}", e)
}

#[derive(Clone, PartialEq, Debug)]
pub enum DiffStatus {
    Checking,
    Same,
    Differs(usize),
    Error,
}

#[derive(Props, Clone, PartialEq)]
pub struct AzurePanelProps {
    pub logic_apps_dir:  String,
    pub local_workflows: Vec<String>,
    /// Persistent diff cache owned by the parent — survives panel close/reopen.
    pub diff_cache:      Signal<HashMap<String, DiffStatus>>,
    /// Azure AD tenant to target on login. None = use az default.
    pub tenant_id:       Option<String>,
    /// Drives visibility; panel sets this to false when the × button is clicked.
    pub is_open:         Signal<bool>,
    pub on_pulled:       EventHandler<String>,
}

#[component]
pub fn AzurePanel(props: AzurePanelProps) -> Element {
    let mut az_workflows:   Signal<Vec<AzureWorkflow>>          = use_signal(Vec::new);
    let mut fetching_sites: Signal<bool>                        = use_signal(|| false);
    let mut fetching_wfs:   Signal<bool>                        = use_signal(|| false);
    let mut pulling:        Signal<HashSet<String>>             = use_signal(HashSet::new);
    let mut status:         Signal<Option<String>>              = use_signal(|| None);
    let mut diff_map:       Signal<HashMap<String, DiffStatus>> = use_signal(HashMap::new);
    let mut computing:      Signal<usize>                       = use_signal(|| 0);
    let mut confirm_pull:   Signal<Option<String>>              = use_signal(|| None);
    let mut show_all:       Signal<bool>                        = use_signal(|| false);
    let mut diff_cache = props.diff_cache;

    let config = use_signal(config::load);
    let workspace_link = config.read().get_link(&props.logic_apps_dir).cloned();

    let mut selected_site: Signal<Option<LogicAppSite>> = use_signal(|| {
        workspace_link.as_ref().and_then(|l| l.logic_app_name.as_ref().map(|name| LogicAppSite {
            name:           name.clone(),
            resource_group: l.resource_group.clone(),
            subscription:   l.subscription_id.clone(),
        }))
    });

    let selected_sub: Signal<Option<String>> = use_signal(|| {
        workspace_link.as_ref().map(|l| l.subscription_id.clone())
    });
    let local_set: HashSet<String> = props.local_workflows.iter().cloned().collect();

    let fetch_workflows = {
        let la_dir        = props.logic_apps_dir.clone();
        let local_set_ref = local_set.clone();
        move |site: LogicAppSite| {
            fetching_wfs.set(true);
            az_workflows.set(vec![]);
            diff_map.write().clear();
            computing.set(0);
            status.set(None);
            let sub  = site.subscription.clone();
            let rg   = site.resource_group.clone();
            let name = site.name.clone();
            selected_site.set(Some(site));
            let dir  = la_dir.clone();
            let lset = local_set_ref.clone();
            spawn(async move {
                let (s, r, n) = (sub.clone(), rg.clone(), name.clone());
                let result = tokio::task::spawn_blocking(move || {
                    azure_sync::list_azure_workflows(&s, &r, &n)
                }).await.unwrap_or(Err(azure_cli::AzError::Other("task failed".into())));
                fetching_wfs.set(false);
                match result {
                    Ok(wfs) => {
                        // Populate from cache; collect workflows that still need computing.
                        let mut to_compute: Vec<(usize, String)> = Vec::new();
                        for (i, wf) in wfs.iter().enumerate() {
                            if lset.contains(&wf.name) {
                                if let Some(cached) = diff_cache.read().get(&wf.name).cloned() {
                                    diff_map.write().insert(wf.name.clone(), cached);
                                } else {
                                    diff_map.write().insert(wf.name.clone(), DiffStatus::Checking);
                                    to_compute.push((i, wf.name.clone()));
                                }
                            }
                        }
                        computing.set(to_compute.len());
                        az_workflows.set(wfs);

                        for (i, wf_nm) in to_compute {
                            let (sc, rc, nc, dc) = (sub.clone(), rg.clone(), name.clone(), dir.clone());
                            let key = wf_nm.clone();
                            spawn(async move {
                                if i > 0 {
                                    let delay = std::cmp::min(200 * i as u64, 2_000);
                                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                                }
                                let ds = match tokio::task::spawn_blocking(move || {
                                    azure_sync::diff_workflow_vs_local(&sc, &rc, &nc, &wf_nm, &dc)
                                }).await.unwrap_or(Err(azure_cli::AzError::Other("task failed".into()))) {
                                    Ok(0)   => DiffStatus::Same,
                                    Ok(cnt) => DiffStatus::Differs(cnt),
                                    Err(_)  => DiffStatus::Error,
                                };
                                diff_map.write().insert(key.clone(), ds.clone());
                                diff_cache.write().insert(key, ds);
                                let cur = *computing.read();
                                computing.set(cur.saturating_sub(1));
                            });
                        }
                    }
                    Err(e) => status.set(Some(fmt_az_error(&e))),
                }
            });
        }
    };

    let pull_workflow = {
        let la_dir    = props.logic_apps_dir.clone();
        let on_pulled = props.on_pulled.clone();
        move |wf_name: String| {
            let site = match selected_site.read().clone() {
                Some(s) => s,
                None => return,
            };
            pulling.write().insert(wf_name.clone());
            status.set(None);
            let dir      = la_dir.clone();
            let sub      = site.subscription.clone();
            let rg       = site.resource_group.clone();
            let sitename = site.name.clone();
            let wf       = wf_name.clone();
            spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    azure_sync::download_workflow(&sub, &rg, &sitename, &wf)
                }).await.unwrap_or(Err(azure_cli::AzError::Other("task failed".into())));

                pulling.write().remove(&wf_name);

                match result {
                    Err(e) => status.set(Some(format!("❌ {wf_name}: {}", fmt_az_error(&e)))),
                    Ok(json) => {
                        let resolved = crate::services::workflows::resolve_logic_apps_dir(&dir);
                        let wf_dir   = resolved.join(&wf_name);
                        if let Err(e) = std::fs::create_dir_all(&wf_dir) {
                            status.set(Some(format!("❌ mkdir failed: {}", e)));
                            return;
                        }
                        if let Err(e) = std::fs::write(wf_dir.join("workflow.json"), &json) {
                            status.set(Some(format!("❌ write failed: {}", e)));
                            return;
                        }
                        diff_map.write().insert(wf_name.clone(), DiffStatus::Same);
                        diff_cache.write().insert(wf_name.clone(), DiffStatus::Same);
                        status.set(Some(format!("✅ {} pulled", wf_name)));
                        on_pulled.call(wf_name);
                    }
                }
            });
        }
    };

    let do_refresh = {
        let mut fw = fetch_workflows.clone();
        move |_| {
            diff_cache.write().clear();
            let site = selected_site.read().clone();
            if let Some(site) = site {
                fw(site);
            }
        }
    };

    use_effect({
        let link    = workspace_link.clone();
        let mut fw  = fetch_workflows.clone();
        let is_open = props.is_open;
        move || {
            if !*is_open.read() { return; } // reactive — re-runs when panel opens
            let Some(link) = link.clone() else { return };
            status.set(None);

            if let Some(site_name) = link.logic_app_name.clone() {
                fw(LogicAppSite {
                    name:           site_name,
                    resource_group: link.resource_group.clone(),
                    subscription:   link.subscription_id.clone(),
                });
                return;
            }

            fetching_sites.set(true);
            let sub_id = link.subscription_id.clone();
            let rg_id  = link.resource_group.clone();
            let mut fw = fw.clone();
            spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    azure_sync::list_logic_app_sites_in_rg(&sub_id, &rg_id)
                }).await.unwrap_or(Err(azure_cli::AzError::Other("task failed".into())));
                fetching_sites.set(false);
                match result {
                    Ok(list) => match list.into_iter().next() {
                        Some(first) => fw(first),
                        None        => status.set(Some("No Logic Apps Standard sites found in this resource group.".into())),
                    },
                    Err(e) => status.set(Some(fmt_az_error(&e))),
                }
            });
        }
    });

    // ── derived state ──────────────────────────────────────────────────────
    let is_loading  = *fetching_sites.read() || *fetching_wfs.read();
    let is_computing = *computing.read() > 0;
    let wf_count    = az_workflows.read().len();

    // stats (computed only when done, cheap to compute always)
    let in_sync   = diff_map.read().values().filter(|s| matches!(s, DiffStatus::Same)).count();
    let differ    = diff_map.read().values().filter(|s| matches!(s, DiffStatus::Differs(_))).count();
    let errors    = diff_map.read().values().filter(|s| matches!(s, DiffStatus::Error)).count();
    let not_local = az_workflows.read().iter().filter(|w| !local_set.contains(&w.name)).count();

    let synced_count = in_sync;
    let visible: Vec<AzureWorkflow> = if !is_computing {
        az_workflows.read().iter()
            .filter(|wf| {
                *show_all.read()
                    || !matches!(diff_map.read().get(&wf.name), Some(DiffStatus::Same))
            })
            .cloned()
            .collect()
    } else {
        vec![]
    };

    rsx! {
        div { id: "az-panel",
            // ── header ────────────────────────────────────────────────────
            div { class: "az-panel-header",
                div {
                    h3 { "☁  Azure Workflows" }
                    if let Some(site) = selected_site.read().as_ref() {
                        span { class: "settings-link-badge", "🔗 {site.name}" }
                    }
                }
                div { style: "display:flex;gap:8px;align-items:center",
                    button {
                        class: "btn btn-small btn-fetch",
                        disabled: is_loading || is_computing,
                        title: "Clear cache and re-compare with Azure",
                        onclick: do_refresh,
                        "⟳ Refresh"
                    }
                    button {
                        class: "btn btn-small btn-fetch",
                        onclick: {
                            let sub    = selected_sub.read().clone();
                            let tenant = props.tenant_id.clone();
                            move |_| azure_cli::launch_az_login(sub.clone(), tenant.clone())
                        },
                        "🔐 Re-login"
                    }
                    {
                        let mut is_open = props.is_open;
                        rsx! {
                            button {
                                class: "btn-icon",
                                onclick: move |_| is_open.set(false),
                                "×"
                            }
                        }
                    }
                }
            }

            // ── status bar ────────────────────────────────────────────────
            if let Some(msg) = status.read().as_ref() {
                div { class: "az-panel-status", "{msg}" }
            }

            // ── tenant notice ─────────────────────────────────────────────
            if props.tenant_id.as_ref().map(|t| t.is_empty()).unwrap_or(true) {
                div { class: "az-tenant-notice",
                    "ℹ No tenant pinned — using your active az session. \
                     Set a Tenant ID in Settings to lock this workspace to a specific Azure AD tenant."
                }
            }

            // ── body ──────────────────────────────────────────────────────
            div { class: "az-panel-body",
                if is_loading {
                    div { class: "az-panel-loading", "Connecting to Azure…" }
                } else if is_computing {
                    div { class: "az-panel-computing",
                        span { class: "az-spinner", "⟳" }
                        " Computing diffs… ({computing} remaining)"
                    }
                } else if wf_count > 0 {
                    // ── stats bar ──────────────────────────────────────────
                    div { class: "az-stats-bar",
                        if differ > 0 {
                            span { class: "az-stat az-stat-differs", "≠ {differ} differ" }
                        }
                        if not_local > 0 {
                            span { class: "az-stat az-stat-remote", "↓ {not_local} not local" }
                        }
                        if in_sync > 0 {
                            span { class: "az-stat az-stat-sync", "✅ {in_sync} in sync" }
                        }
                        if errors > 0 {
                            span { class: "az-stat az-stat-error", "⚠ {errors} error" }
                        }
                        span { class: "az-stat az-stat-total", "· {wf_count} total" }
                    }

                    // ── filter bar ─────────────────────────────────────────
                    div { class: "az-filter-bar",
                        label { class: "az-filter-label",
                            input {
                                r#type: "checkbox",
                                checked: *show_all.read(),
                                onchange: move |_| { let v = *show_all.read(); show_all.set(!v); },
                            }
                            if synced_count > 0 {
                                " Show synchronized ({synced_count})"
                            } else {
                                " Show synchronized"
                            }
                        }
                    }

                    // ── table ──────────────────────────────────────────────
                    if visible.is_empty() {
                        div { class: "az-panel-loading",
                            "✅ All {wf_count} workflows are in sync."
                        }
                    } else {
                        table { class: "az-wf-table",
                            thead {
                                tr {
                                    th { "Workflow" }
                                    th { "Health" }
                                    th { "Sync" }
                                    th { "" }
                                }
                            }
                            tbody {
                                for wf in visible {
                                    {
                                        let wf2        = wf.clone();
                                        let is_local   = local_set.contains(&wf.name);
                                        let is_pulling = pulling.read().contains(&wf.name);
                                        let diff_st    = diff_map.read().get(&wf.name).cloned();
                                        let mut pw     = pull_workflow.clone();

                                        let (sync_label, sync_cls) = if !is_local {
                                            ("—".to_string(), "az-sync-none")
                                        } else {
                                            match &diff_st {
                                                Some(DiffStatus::Same)       => ("≡ in sync".to_string(),  "az-sync-same"),
                                                Some(DiffStatus::Differs(n)) => (format!("≠ {} lines", n), "az-sync-differs"),
                                                Some(DiffStatus::Error)      => ("⚠ error".to_string(),    "az-sync-error"),
                                                _                            => ("✅ local".to_string(),    "az-sync-local"),
                                            }
                                        };

                                        rsx! {
                                            tr { class: "az-wf-row",
                                                td { class: "az-wf-name", "{wf.name}" }
                                                td { class: "az-wf-health",
                                                    if wf.healthy { "✅" } else { "⚠" }
                                                }
                                                td { class: "az-wf-sync",
                                                    span { class: sync_cls, "{sync_label}" }
                                                }
                                                td { class: "az-wf-action",
                                                    if is_pulling {
                                                        span { class: "az-pulling", "pulling…" }
                                                    } else {
                                                        button {
                                                            class: "btn btn-small az-pull-btn",
                                                            onclick: {
                                                                let wf_name_btn  = wf2.name.clone();
                                                                let needs_confirm = matches!(&diff_st, Some(DiffStatus::Differs(_)));
                                                                move |_| {
                                                                    if needs_confirm {
                                                                        confirm_pull.set(Some(wf_name_btn.clone()));
                                                                    } else {
                                                                        pw(wf_name_btn.clone());
                                                                    }
                                                                }
                                                            },
                                                            if is_local { {"⟳ Re-pull"} } else { {"\u{2B07} Pull"} }
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
                } else if selected_site.read().is_some() {
                    div { class: "az-panel-loading", "No workflows found." }
                }
            }

            // ── confirm overwrite modal ────────────────────────────────────
            if let Some(wf_name) = confirm_pull.read().clone() {
                div { class: "az-confirm-backdrop", onclick: move |_| confirm_pull.set(None) }
                div { class: "az-confirm-modal",
                    div { class: "az-confirm-title", {"⚠ Overwrite local changes?"} }
                    div { class: "az-confirm-body",
                        "The workflow " strong { "{wf_name}" }
                        " has local changes. Pulling will overwrite your local copy."
                    }
                    div { class: "az-confirm-actions",
                        button {
                            class: "btn btn-small",
                            onclick: move |_| confirm_pull.set(None),
                            "Cancel"
                        }
                        button {
                            class: "btn btn-small az-confirm-overwrite",
                            onclick: {
                                let mut pw = pull_workflow.clone();
                                let wf = wf_name.clone();
                                move |_| { confirm_pull.set(None); pw(wf.clone()); }
                            },
                            {"\u{2B07} Overwrite & Pull"}
                        }
                    }
                }
            }
        }
    }
}
