//! Links a workspace to its Azure Logic App.
//!
//! The workspace link is what every Azure-powered feature reads for its
//! subscription and resource group, and it could only ever be created on the
//! welcome screen's onboarding path. A workspace reopened from `recent_dirs`
//! with no link therefore sat showing "N setting(s) need a value" whose only
//! button was "Configure Manually" — while a perfectly good `az` session, with
//! the subscription and resource group right there for the asking, went unused.
//!
//! This is the same link-then-`auto_detect_resources` sequence the welcome
//! screen runs, reachable from the banner that complains about the problem.

use crate::services::{config, setup_manager};
use ais_core::auth as azure_auth;
use ais_core::cli as azure_cli;
use ais_core::sync::{self as azure_sync, LogicAppSite};
use dioxus::prelude::*;

/// Writes the workspace link, then fills in the identity fields it just learned.
///
/// The tenant is pinned from the session that discovered the site: we know the
/// site was reachable from this tenant, which is exactly the guarantee the
/// "pin a tenant" setting is meant to encode. It also makes the toolbar's
/// mismatch badge meaningful — with nothing pinned it can never fire.
fn link_and_fill(dir: &str, site: &LogicAppSite) -> Result<String, String> {
    let link = config::WorkspaceLink {
        subscription_id: site.subscription.clone(),
        resource_group: site.resource_group.clone(),
        tenant_id: azure_auth::get_active_tenant()
            .ok()
            .filter(|t| !t.is_empty()),
        logic_app_name: Some(site.name.clone()),
        sb_namespace: None,
        devops_org: None,
        devops_project: None,
    };

    let mut cfg = config::load();
    cfg.set_link(dir.to_string(), link.clone());
    config::save(&cfg);

    setup_manager::auto_detect_resources(
        dir,
        Some(&link.subscription_id),
        &link.resource_group,
        link.logic_app_name.as_deref(),
    )
}

#[derive(Props, Clone, PartialEq)]
pub struct AzureLinkDialogProps {
    pub logic_apps_dir: String,
    /// Drives visibility; the dialog sets this to false when dismissed.
    pub is_open: Signal<bool>,
    /// Fired once the link is saved and local.settings.json has been patched,
    /// carrying the `auto_detect_resources` report for the log panel.
    pub on_linked: EventHandler<String>,
}

#[component]
pub fn AzureLinkDialog(props: AzureLinkDialogProps) -> Element {
    let mut sites: Signal<Vec<LogicAppSite>> = use_signal(Vec::new);
    let mut loading = use_signal(|| true);
    let mut busy = use_signal(|| false);
    let mut error: Signal<Option<String>> = use_signal(|| None);

    let mut is_open = props.is_open;
    let on_linked = props.on_linked;
    let dir = props.logic_apps_dir.clone();

    // Commit a chosen site. Shared by the auto-link path below and the click
    // handler on each row, so both report failures the same way.
    let choose = {
        let dir = dir.clone();
        move |site: LogicAppSite| {
            let dir = dir.clone();
            busy.set(true);
            spawn(async move {
                let result = tokio::task::spawn_blocking(move || link_and_fill(&dir, &site))
                    .await
                    .unwrap_or_else(|_| Err("task panicked".into()));
                busy.set(false);
                match result {
                    Ok(report) => {
                        is_open.set(false);
                        on_linked.call(report);
                    }
                    Err(e) => error.set(Some(e)),
                }
            });
        }
    };

    // Discover on mount. The parent only renders this component while the
    // dialog is open, so mounting and opening are the same event.
    use_effect({
        let choose = choose.clone();
        move || {
            let mut choose = choose.clone();
            spawn(async move {
                let result = tokio::task::spawn_blocking(|| azure_sync::list_logic_app_sites(None))
                    .await
                    .unwrap_or_else(|_| Err(azure_cli::AzError::Other("task panicked".into())));
                loading.set(false);
                match result {
                    // One candidate is not a choice — link it and get out of the way.
                    Ok(list) if list.len() == 1 => choose(list[0].clone()),
                    Ok(list) if list.is_empty() => error.set(Some(
                        "No Logic Apps Standard sites found in your active subscription. \
                         Switch subscription with `az account set` and try again."
                            .into(),
                    )),
                    Ok(list) => sites.set(list),
                    Err(e) => error.set(Some(format!("{:?}", e))),
                }
            });
        }
    });

    let dismiss = move |_| is_open.set(false);

    rsx! {
        div { class: "az-link-backdrop", onclick: dismiss }
        div { class: "az-link-modal",
            div { class: "az-link-title", "🔗 Link this workspace to Azure" }

            if let Some(msg) = error.read().clone() {
                div { class: "onboarding-error", "{msg}" }
            } else if *loading.read() {
                div { class: "onboarding-loading", "Looking for Logic Apps in your subscription…" }
            } else if *busy.read() {
                div { class: "onboarding-loading", "Linking and filling in settings…" }
            } else {
                div { class: "az-link-hint",
                    "Pick the Logic App this project deploys to. Its subscription, \
                     resource group and site name go straight into local.settings.json."
                }
                div { class: "onboarding-list",
                    for site in sites.read().clone() {
                        div {
                            class: "onboarding-item",
                            onclick: {
                                let mut choose = choose.clone();
                                let site = site.clone();
                                move |_| choose(site.clone())
                            },
                            span { class: "onboarding-item-name", "{site.name}" }
                            span { class: "onboarding-item-rg", "{site.resource_group}" }
                        }
                    }
                }
            }

            div { class: "az-link-actions",
                button { class: "btn btn-small", onclick: dismiss, "Cancel" }
            }
        }
    }
}
