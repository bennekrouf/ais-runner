use dioxus::prelude::*;

use crate::services::{azure_sync, azure_cli, config};

#[derive(Props, Clone, PartialEq)]
pub struct WelcomeScreenProps {
    pub app_cfg: Signal<config::AppConfig>,
    pub on_open: EventHandler<String>,
}

#[component]
pub fn WelcomeScreen(props: WelcomeScreenProps) -> Element {
    let app_cfg = props.app_cfg;
    let on_open = props.on_open.clone();
    let recent  = app_cfg.read().recent_dirs.clone();

    let mut picked_dir: Signal<Option<String>> = use_signal(|| None);
    let mut rg_list: Signal<Vec<azure_sync::LogicAppSite>> = use_signal(Vec::new);
    let mut fetching: Signal<bool>        = use_signal(|| false);
    let mut error: Signal<Option<String>> = use_signal(|| None);

    rsx! {
        div { id: "welcome",
            div { id: "welcome-header",
                h1 { "AIS Local Runner" }
                p { "Select your AIS platform folder to get started." }
            }

            div { id: "welcome-box",
                if let Some(dir) = picked_dir.read().clone() {
                    div { id: "onboarding",
                        h3 { "🔗 Link Project to Azure" }
                        p { class: "onboarding-path", "{dir}" }
                        p { class: "onboarding-hint", "Select the Logic App site (Resource Group) for this customer repo:" }

                        if *fetching.read() {
                            div { class: "onboarding-loading", "Fetching sites from Azure..." }
                        } else if let Some(err) = error.read().clone() {
                            div { class: "onboarding-error",
                                "{err}"
                                button {
                                    class: "btn btn-small",
                                    style: "margin-left: 10px",
                                    onclick: move |_| {
                                        error.set(None);
                                        fetching.set(true);
                                        spawn(async move {
                                            azure_cli::launch_az_login(None, None);
                                        });
                                    },
                                    "🔐 Login"
                                }
                            }
                        } else {
                            div { class: "onboarding-list",
                                for site in rg_list.read().clone() {
                                    div {
                                        class: "onboarding-item",
                                        onclick: {
                                            let site   = site.clone();
                                            let dir    = dir.clone();
                                            let mut app_cfg = app_cfg.clone();
                                            let on_open     = on_open.clone();
                                            move |_| {
                                                let mut cfg = app_cfg.read().clone();
                                                cfg.workspace_links.insert(dir.clone(), config::WorkspaceLink {
                                                    subscription_id: site.subscription.clone(),
                                                    resource_group:  site.resource_group.clone(),
                                                    tenant_id:       None,
                                                    logic_app_name:  Some(site.name.clone()),
                                                    sb_namespace:    None,
                                                    devops_org:      None,
                                                    devops_project:  None,
                                                });
                                                config::save(&cfg);
                                                app_cfg.set(cfg);
                                                on_open.call(dir.clone());
                                            }
                                        },
                                        span { class: "onboarding-item-name", "{site.name}" }
                                        span { class: "onboarding-item-rg",   "{site.resource_group}" }
                                    }
                                }
                            }
                        }

                        div { class: "onboarding-actions",
                            button {
                                class: "btn-back",
                                onclick: move |_| picked_dir.set(None),
                                "← Back"
                            }
                        }
                    }
                } else {
                    div { id: "welcome-pick",
                        p { "Choose the root folder of your ais_platform repo" }
                        button {
                            class: "btn-pick-folder",
                            onclick: move |_| {
                                spawn(async move {
                                    let picked = tokio::task::spawn_blocking(|| {
                                        config::pick_folder(None)
                                    }).await.ok().flatten();

                                    if let Some(dir) = picked {
                                        let cfg = config::load();
                                        if cfg.workspace_links.contains_key(&dir) {
                                            on_open.call(dir);
                                        } else {
                                            picked_dir.set(Some(dir));
                                            fetching.set(true);
                                            let res = tokio::task::spawn_blocking(|| {
                                                azure_sync::list_logic_app_sites(None)
                                            }).await.unwrap_or(Err(azure_cli::AzError::Other("Task failed".into())));
                                            fetching.set(false);
                                            match res {
                                                Ok(list) => rg_list.set(list),
                                                Err(e)   => error.set(Some(format!("Azure error: {:?}", e))),
                                            }
                                        }
                                    }
                                });
                            },
                            "📁  Browse…"
                        }
                    }

                    if !recent.is_empty() {
                        div { id: "recent-list",
                            h3 { "Recent Projects" }
                            for dir in recent {
                                {
                                    let dir2    = dir.clone();
                                    let on_open = on_open.clone();
                                    let link    = app_cfg.read().workspace_links.get(&dir).cloned();
                                    let rg_name = link.as_ref().and_then(|l| l.logic_app_name.clone())
                                        .or_else(|| link.as_ref().map(|l| l.resource_group.clone()))
                                        .unwrap_or_else(|| "Not linked".to_string());
                                    rsx! {
                                        div {
                                            class: "recent-item",
                                            onclick: move |_| on_open.call(dir2.clone()),
                                            div { style: "display:flex; flex-direction:column",
                                                span { class: "recent-path", title: "{dir}", "{dir}" }
                                                span { class: "recent-rg",   "{rg_name}" }
                                            }
                                            span { class: "recent-arrow", "›" }
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
