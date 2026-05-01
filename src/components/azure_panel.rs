use dioxus::prelude::*;
use std::collections::HashSet;
use crate::services::azure_sync::{self, AzureWorkflow, LogicAppSite};

#[derive(Props, Clone, PartialEq)]
pub struct AzurePanelProps {
    pub logic_apps_dir: String,
    pub local_workflows: Vec<String>,      // names already present locally
    pub on_close:    EventHandler<()>,
    pub on_pulled:   EventHandler<String>, // workflow name that was just pulled
}

#[component]
pub fn AzurePanel(props: AzurePanelProps) -> Element {
    // ── site picker ───────────────────────────────────────────────────────
    let mut sites:        Signal<Vec<LogicAppSite>>    = use_signal(Vec::new);
    let mut sites_open:   Signal<bool>                 = use_signal(|| false);
    let mut selected_site: Signal<Option<LogicAppSite>> = use_signal(|| None);
    let mut fetching_sites: Signal<bool>               = use_signal(|| false);

    // ── workflow list ─────────────────────────────────────────────────────
    let mut az_workflows: Signal<Vec<AzureWorkflow>>   = use_signal(Vec::new);
    let mut fetching_wfs: Signal<bool>                 = use_signal(|| false);
    let mut pulling:      Signal<HashSet<String>>       = use_signal(HashSet::new);
    let mut status:       Signal<Option<String>>        = use_signal(|| None);

    let local_set: HashSet<String> = props.local_workflows.iter().cloned().collect();

    // ── fetch site list ───────────────────────────────────────────────────
    let fetch_sites = {
        move |_| {
            fetching_sites.set(true);
            sites.set(vec![]);
            status.set(None);
            spawn(async move {
                let result = tokio::task::spawn_blocking(azure_sync::list_logic_app_sites)
                    .await
                    .unwrap_or(Err(crate::services::azure_cli::AzError::Other("task failed".into())));
                fetching_sites.set(false);
                match result {
                    Ok(list) => {
                        if list.is_empty() {
                            status.set(Some("No Logic Apps Standard sites found in current subscription.".into()));
                        } else {
                            sites.set(list);
                            sites_open.set(true);
                        }
                    }
                    Err(e) => status.set(Some(format!("Error: {:?}", e))),
                }
            });
        }
    };

    // ── fetch workflow list for selected site ──────────────────────────────
    let fetch_workflows = move |site: LogicAppSite| {
        fetching_wfs.set(true);
        az_workflows.set(vec![]);
        status.set(None);
        let sub  = site.subscription.clone();
        let rg   = site.resource_group.clone();
        let name = site.name.clone();
        selected_site.set(Some(site));
        spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                azure_sync::list_azure_workflows(&sub, &rg, &name)
            }).await.unwrap_or(Err(crate::services::azure_cli::AzError::Other("task failed".into())));
            fetching_wfs.set(false);
            match result {
                Ok(wfs) => az_workflows.set(wfs),
                Err(e)  => status.set(Some(format!("Error: {:?}", e))),
            }
        });
    };

    // ── pull one workflow ─────────────────────────────────────────────────
    let pull_workflow = {
        let la_dir = props.logic_apps_dir.clone();
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
                }).await.unwrap_or(Err(crate::services::azure_cli::AzError::Other("task failed".into())));

                pulling.write().remove(&wf_name);

                match result {
                    Err(e) => {
                        status.set(Some(format!("❌ {}: {:?}", wf_name, e)));
                    }
                    Ok(json) => {
                        let wf_dir = std::path::Path::new(&dir).join(&wf_name);
                        if let Err(e) = std::fs::create_dir_all(&wf_dir) {
                            status.set(Some(format!("❌ mkdir failed: {}", e)));
                            return;
                        }
                        let wf_path = wf_dir.join("workflow.json");
                        if let Err(e) = std::fs::write(&wf_path, &json) {
                            status.set(Some(format!("❌ write failed: {}", e)));
                            return;
                        }
                        // git diff --stat
                        let diff = std::process::Command::new("git")
                            .args(["diff", "--stat", "HEAD", &format!("{}/workflow.json", wf_name)])
                            .current_dir(&dir)
                            .output()
                            .ok()
                            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                            .unwrap_or_default();
                        let msg = if diff.is_empty() {
                            format!("✅ {} — pulled (no diff from HEAD)", wf_name)
                        } else {
                            format!("✅ {} — pulled  git diff: {}", wf_name, diff)
                        };
                        status.set(Some(msg));
                        props.on_pulled.call(wf_name);
                    }
                }
            });
        }
    };

    // ── render ─────────────────────────────────────────────────────────────
    let site_label = selected_site.read().as_ref()
        .map(|s| format!("{} / {}", s.resource_group, s.name))
        .unwrap_or_else(|| "— pick a site —".into());

    rsx! {
        // backdrop
        div {
            id: "az-panel-backdrop",
            onclick: move |_| props.on_close.call(()),
        }

        div { id: "az-panel",

            // header
            div { class: "az-panel-header",
                div {
                    h3 { "☁  Azure Workflows" }
                    if let Some(site) = selected_site.read().as_ref() {
                        span { class: "az-panel-hint",
                            "{site.subscription} › {site.resource_group} › {site.name}"
                        }
                    }
                }
                button {
                    class: "btn-icon",
                    onclick: move |_| props.on_close.call(()),
                    "×"
                }
            }

            // site picker
            div { class: "az-panel-site-row",
                div { class: "db-ns-combobox",
                    div {
                        class: if *fetching_sites.read() { "db-ns-field loading" } else { "db-ns-field" },
                        onclick: fetch_sites,
                        span { class: "db-ns-field-value", "{site_label}" }
                        span { class: "db-ns-chevron",
                            if *fetching_sites.read() { "…" } else { "▾" }
                        }
                    }
                    if *sites_open.read() && !sites.read().is_empty() {
                        div { class: "db-ns-dropdown",
                            for site in sites.read().clone() {
                                {
                                    let site2 = site.clone();
                                    let mut fw = fetch_workflows.clone();
                                    rsx! {
                                        div {
                                            class: "db-ns-item",
                                            onclick: move |_| {
                                                sites_open.set(false);
                                                fw(site2.clone());
                                            },
                                            span { class: "db-ns-item-name", "{site.name}" }
                                            span { class: "db-ns-item-hint", "{site.resource_group}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // status
            if let Some(msg) = status.read().as_ref() {
                div { class: "az-panel-status", "{msg}" }
            }

            // workflow table
            div { class: "az-panel-body",
                if *fetching_wfs.read() {
                    div { class: "az-panel-loading", "Fetching workflows from Azure…" }
                } else if !az_workflows.read().is_empty() {
                    table { class: "az-wf-table",
                        thead {
                            tr {
                                th { "Workflow" }
                                th { "Health" }
                                th { "Local" }
                                th { "" }
                            }
                        }
                        tbody {
                            for wf in az_workflows.read().clone() {
                                {
                                    let wf2      = wf.clone();
                                    let is_local = local_set.contains(&wf.name);
                                    let is_pulling = pulling.read().contains(&wf.name);
                                    let mut pw   = pull_workflow.clone();
                                    rsx! {
                                        tr { class: if is_local { "az-wf-row local" } else { "az-wf-row azure-only" },
                                            td { class: "az-wf-name", "{wf.name}" }
                                            td { class: "az-wf-health",
                                                if wf.healthy { "✅" } else { "⚠" }
                                            }
                                            td { class: "az-wf-local",
                                                if is_local { "✅" } else { "—" }
                                            }
                                            td { class: "az-wf-action",
                                                if is_pulling {
                                                    span { class: "az-pulling", "pulling…" }
                                                } else {
                                                    button {
                                                        class: "btn btn-small az-pull-btn",
                                                        title: if is_local { "Re-pull from Azure" } else { "Pull to local" },
                                                        onclick: move |_| pw(wf2.name.clone()),
                                                        if is_local { "⟳ Re-pull" } else { "⬇ Pull" }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                } else if selected_site.read().is_some() && !*fetching_wfs.read() {
                    div { class: "az-panel-loading", "No workflows found." }
                } else {
                    div { class: "az-panel-loading", "Pick a Logic Apps site above to compare." }
                }
            }
        }
    }
}
