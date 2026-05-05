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
            // Extract client email — between "client '" and "' with"
            let user = msg.split("client '").nth(1)
                .and_then(|s| s.split("' with").next())
                .unwrap_or("your account");
            // Extract site name — last segment of the sites/ path
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
enum DiffStatus {
    Checking,
    Same,
    Differs(usize),
    Error,
}

#[derive(Props, Clone, PartialEq)]
pub struct AzurePanelProps {
    pub logic_apps_dir: String,
    pub local_workflows: Vec<String>,
    pub on_close:    EventHandler<()>,
    pub on_pulled:   EventHandler<String>,
}

#[component]
pub fn AzurePanel(props: AzurePanelProps) -> Element {
    let mut az_workflows:   Signal<Vec<AzureWorkflow>>   = use_signal(Vec::new);
    let mut fetching_sites: Signal<bool>                 = use_signal(|| false);
    let mut fetching_wfs:   Signal<bool>                 = use_signal(|| false);
    let mut pulling:        Signal<HashSet<String>>      = use_signal(HashSet::new);
    let mut status:         Signal<Option<String>>       = use_signal(|| None);
    let mut diff_map:       Signal<HashMap<String, DiffStatus>> = use_signal(HashMap::new);
    let mut confirm_pull:   Signal<Option<String>>       = use_signal(|| None);

    let config = use_signal(config::load);
    let workspace_link = config.read().get_link(&props.logic_apps_dir).cloned();

    let mut selected_site: Signal<Option<LogicAppSite>> = use_signal(|| {
        workspace_link.as_ref().and_then(|l| l.logic_app_name.as_ref().map(|name| LogicAppSite {
            name:           name.clone(),
            resource_group: l.resource_group.clone(),
            subscription:   l.subscription_id.clone(),
        }))
    });

    let selected_sub: Signal<Option<String>> = use_signal(|| workspace_link.as_ref().map(|l| l.subscription_id.clone()));
    let local_set: HashSet<String> = props.local_workflows.iter().cloned().collect();

    let fetch_workflows = {
        let la_dir = props.logic_apps_dir.clone();
        let local_set_ref = local_set.clone();
        move |site: LogicAppSite| {
            fetching_wfs.set(true);
            az_workflows.set(vec![]);
            diff_map.write().clear();
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
                }).await.unwrap_or(Err(crate::services::azure_cli::AzError::Other("task failed".into())));
                fetching_wfs.set(false);
                match result {
                    Ok(wfs) => {
                        for (i, wf) in wfs.iter().enumerate() {
                            if lset.contains(&wf.name) {
                                diff_map.write().insert(wf.name.clone(), DiffStatus::Checking);
                                let key             = wf.name.clone();
                                let wf_nm           = wf.name.clone();
                                let (sc, rc, nc, dc) = (sub.clone(), rg.clone(), name.clone(), dir.clone());
                                spawn(async move {
                                    if i > 0 {
                                        let delay = std::cmp::min(200 * i as u64, 2_000);
                                        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                                    }
                                    let ds = match tokio::task::spawn_blocking(move || {
                                        azure_sync::diff_workflow_vs_local(&sc, &rc, &nc, &wf_nm, &dc)
                                    }).await.unwrap_or(Err(crate::services::azure_cli::AzError::Other("task failed".into()))) {
                                        Ok(0)   => DiffStatus::Same,
                                        Ok(cnt) => DiffStatus::Differs(cnt),
                                        Err(_)  => DiffStatus::Error,
                                    };
                                    diff_map.write().insert(key, ds);
                                });
                            }
                        }
                        az_workflows.set(wfs);
                    }
                    Err(e) => status.set(Some(fmt_az_error(&e))),
                }
            });
        }
    };

    let pull_workflow = {
        let la_dir = props.logic_apps_dir.clone();
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
                }).await.unwrap_or(Err(crate::services::azure_cli::AzError::Other("task failed".into())));

                pulling.write().remove(&wf_name);

                match result {
                    Err(e) => {
                        status.set(Some(format!("❌ {wf_name}: {}", fmt_az_error(&e))));
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
                        diff_map.write().insert(wf_name.clone(), DiffStatus::Same);
                        status.set(Some(format!("✅ {} pulled", wf_name)));
                        on_pulled.call(wf_name);
                    }
                }
            });
        }
    };

    use_effect({
        let link = workspace_link.clone();
        let mut fw = fetch_workflows.clone();
        move || {
            let Some(link) = link.clone() else { return };
            status.set(None);

            if let Some(site_name) = link.logic_app_name.clone() {
                let site = azure_sync::LogicAppSite {
                    name:           site_name,
                    resource_group: link.resource_group.clone(),
                    subscription:   link.subscription_id.clone(),
                };
                fw(site);
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
                    Ok(list) if !list.is_empty() => {
                        let site = list.into_iter().next().unwrap();
                        fw(site);
                    }
                    Ok(_) => status.set(Some("No Logic Apps Standard sites found in this resource group.".into())),
                    Err(e) => status.set(Some(fmt_az_error(&e))),
                }
            });
        }
    });

    rsx! {
        div {
            id: "az-panel-backdrop",
            onclick: move |_| props.on_close.call(()),
        }

        div { id: "az-panel",
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
                        onclick: {
                            let sub = selected_sub.read().clone();
                            move |_| {
                                azure_cli::launch_az_login(sub.clone());
                            }
                        },
                        "🔐 Re-login"
                    }
                    button {
                        class: "btn-icon",
                        onclick: move |_| props.on_close.call(()),
                        "×"
                    }
                }
            }

            if let Some(msg) = status.read().as_ref() {
                div { class: "az-panel-status", "{msg}" }
            }

            div { class: "az-panel-body",
                if *fetching_sites.read() || *fetching_wfs.read() {
                    div { class: "az-panel-loading", "Connecting to Azure..." }
                } else if !az_workflows.read().is_empty() {
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
                            for wf in az_workflows.read().clone() {
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
                                            Some(DiffStatus::Checking)    => ("⋯".to_string(),          "az-sync-checking"),
                                            Some(DiffStatus::Same)        => ("≡ in sync".to_string(),  "az-sync-same"),
                                            Some(DiffStatus::Differs(n))  => (format!("≠ {} lines", n), "az-sync-differs"),
                                            _                             => ("✅ local".to_string(),  "az-sync-local"),
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
                                                            let wf_name_btn = wf2.name.clone();
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
                } else if selected_site.read().is_some() {
                    div { class: "az-panel-loading", "No workflows found." }
                }
            }

            if let Some(wf_name) = confirm_pull.read().clone() {
                div { class: "az-confirm-backdrop", onclick: move |_| confirm_pull.set(None) }
                div { class: "az-confirm-modal",
                    div { class: "az-confirm-title", {"⚠ Overwrite local changes?"} }
                    div { class: "az-confirm-body",
                        "The workflow " strong { "{wf_name}" } " has local changes. Pulling will overwrite your local copy."
                    }
                    div { class: "az-confirm-actions",
                        button { class: "btn btn-small", onclick: move |_| confirm_pull.set(None), "Cancel" }
                        button {
                            class: "btn btn-small az-confirm-overwrite",
                            onclick: {
                                let mut pw = pull_workflow.clone();
                                let wf = wf_name.clone();
                                move |_| {
                                    confirm_pull.set(None);
                                    pw(wf.clone());
                                }
                            },
                            {"\u{2B07} Overwrite & Pull"}
                        }
                    }
                }
            }
        }
    }
}
