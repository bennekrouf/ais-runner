use dioxus::prelude::*;

use crate::components::log_panel::{LogLevel, LogLine};
use crate::services::{config, setup_manager};
use crate::utils::make_push;

pub fn handle_initialize(
    dir: &str,
    mut setup_status: Signal<setup_manager::SetupStatus>,
    log_lines: Signal<Vec<LogLine>>,
) {
    let d  = dir.to_string();
    let d2 = dir.to_string();
    let mut push = make_push(log_lines);
    spawn(async move {
        match tokio::task::spawn_blocking(move || setup_manager::initialize_from_template(&d)).await.unwrap_or(Err("task panicked".into())) {
            Ok(_)  => { push("Settings initialized from template.".into(), LogLevel::Ok); setup_status.set(setup_manager::check_setup(&d2)); }
            Err(e) => push(format!("Failed to initialize: {}", e), LogLevel::Error),
        }
    });
}

pub fn handle_initialize_default(
    dir: &str,
    mut setup_status: Signal<setup_manager::SetupStatus>,
    log_lines: Signal<Vec<LogLine>>,
    workspace_link: Option<config::WorkspaceLink>,
) {
    let d    = dir.to_string();
    let d2   = dir.to_string();
    let d3   = dir.to_string();
    let mut push = make_push(log_lines);
    spawn(async move {
        match tokio::task::spawn_blocking(move || setup_manager::initialize_default(&d)).await.unwrap_or(Err("task panicked".into())) {
            Ok(_) => {
                push("Default local.settings.json created.".into(), LogLevel::Ok);
                if let Some(l) = workspace_link {
                    push(format!("Auto-detecting resources in {}...", l.resource_group), LogLevel::Info);
                    match tokio::task::spawn_blocking(move || {
                        setup_manager::auto_detect_resources(
                            &d3,
                            Some(&l.subscription_id),
                            &l.resource_group,
                            l.logic_app_name.as_deref(),
                        )
                    }).await.unwrap_or(Err("task panicked".into())) {
                        Ok(msg) => push(msg, LogLevel::Ok),
                        Err(e)  => push(format!("Auto-detect failed: {}", e), LogLevel::Error),
                    }
                }
                setup_status.set(setup_manager::check_setup(&d2));
            }
            Err(e) => push(format!("Failed to create default settings: {}", e), LogLevel::Error),
        }
    });
}

pub fn handle_auto_detect(
    dir: &str,
    mut setup_status: Signal<setup_manager::SetupStatus>,
    log_lines: Signal<Vec<LogLine>>,
    workspace_link: Option<config::WorkspaceLink>,
) {
    let Some(l) = workspace_link else { return };
    let d      = dir.to_string();
    let d2     = dir.to_string();
    let rg     = l.resource_group.clone();
    let sub_id = l.subscription_id.clone();
    let mut push = make_push(log_lines);
    spawn(async move {
        push(format!("Auto-detecting resources in {}...", rg), LogLevel::Info);
        let app_name = l.logic_app_name.clone();
        match tokio::task::spawn_blocking(move || {
            setup_manager::auto_detect_resources(&d, Some(&sub_id), &rg, app_name.as_deref())
        }).await.unwrap_or(Err("task panicked".into())) {
            Ok(msg) => push(msg, LogLevel::Ok),
            Err(e)  => push(format!("Auto-detect failed: {}", e), LogLevel::Error),
        }
        setup_status.set(setup_manager::check_setup(&d2));
    });
}
