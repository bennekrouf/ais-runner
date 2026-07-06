use dioxus::prelude::*;
use crate::services::{setup_manager, system_check, env_mode};
use crate::handlers::debug_mode;

#[derive(Props, Clone, PartialEq)]
pub struct LoadingScreenProps {
    pub logic_apps_dir: String,
    pub is_light: Signal<bool>,
    pub theme_overridden: Signal<bool>,
    pub on_done: EventHandler<String>,
    pub on_back: EventHandler<()>,
}

#[component]
pub fn LoadingScreen(props: LoadingScreenProps) -> Element {
    let dir = props.logic_apps_dir.clone();
    let log_lines: Signal<Vec<(String, LogLevel)>> = use_signal(Vec::new);
    let mut checks_done: Signal<bool> = use_signal(|| false);

    // Run all startup checks on mount
    use_effect({
        let dir = dir.clone();
        let log_lines = log_lines;
        let mut checks_done_inner = checks_done;
        move || {
            let dir = dir.clone();
            let mut log_lines = log_lines;
            let mut checks_done_inner = checks_done_inner;
            spawn(async move {
                let mut push_log = |msg: String, level: LogLevel| {
                    let ts = chrono::Local::now().format("%H:%M:%S").to_string();
                    let entry = format!("[{}] {}", ts, msg);
                    let mut w = log_lines.write();
                    w.push((entry, level));
                };

                push_log("Starting initialization...".to_string(), LogLevel::Info);

                // 1. Check and cleanup debug mode
                push_log("Checking debug mode...".to_string(), LogLevel::Info);
                let d = dir.clone();
                let cleanup_result = tokio::task::spawn_blocking(move || {
                    debug_mode::cleanup_orphans(&d)
                })
                .await
                .unwrap_or_else(|_| debug_mode::RevertOutcome {
                    reverted: vec![],
                    skipped: vec![],
                });

                if !cleanup_result.reverted.is_empty() {
                    push_log(
                        format!(
                            "🐞 Cleaned up {} orphan patch(es): {}",
                            cleanup_result.reverted.len(),
                            cleanup_result.reverted.join(", ")
                        ),
                        LogLevel::Warn,
                    );
                }
                for (name, why) in cleanup_result.skipped {
                    push_log(
                        format!("🐞 Orphan patch '{}' skipped: {}", name, why),
                        LogLevel::Warn,
                    );
                }

                // 2. Check tools
                push_log("Checking system tools...".to_string(), LogLevel::Info);
                push_log("  → Probing func...".to_string(), LogLevel::Info);
                let tool_results = tokio::task::spawn_blocking(system_check::check_tools)
                    .await
                    .unwrap_or_default();
                push_log("  ✓ Tool check complete".to_string(), LogLevel::Info);

                for tool in tool_results {
                    if tool.available {
                        let version = tool
                            .version
                            .clone()
                            .unwrap_or_else(|| "unknown".to_string());
                        push_log(
                            format!("✓ {} ({})", tool.name, version),
                            LogLevel::Success,
                        );
                    } else {
                        push_log(format!("✗ {} — {}", tool.name, tool.install_hint), LogLevel::Error);
                    }
                }

                // 3. Check setup status
                push_log("Checking project setup...".to_string(), LogLevel::Info);
                push_log("  → Reading local.settings.json...".to_string(), LogLevel::Info);
                let d = dir.clone();
                let setup_status = tokio::task::spawn_blocking(move || setup_manager::check_setup(&d))
                    .await
                    .unwrap_or(setup_manager::SetupStatus::MissingSettings);
                push_log("  ✓ Setup check complete".to_string(), LogLevel::Info);

                match setup_status {
                    setup_manager::SetupStatus::Ready => {
                        push_log("✓ Project setup is complete".to_string(), LogLevel::Success);
                    }
                    setup_manager::SetupStatus::MissingSettings => {
                        push_log("⚠ local.settings.json not found — some features unavailable".to_string(), LogLevel::Warn);
                    }
                    setup_manager::SetupStatus::NeedsInitialization => {
                        push_log("⚠ Setup requires initialization".to_string(), LogLevel::Warn);
                    }
                    setup_manager::SetupStatus::RemoteStorage => {
                        push_log("⚠ AzureWebJobsStorage points to remote Azure — local func may fail".to_string(), LogLevel::Warn);
                    }
                    setup_manager::SetupStatus::NeedsConfiguration(_) => {
                        push_log("⚠ Project needs configuration".to_string(), LogLevel::Warn);
                    }
                    setup_manager::SetupStatus::MissingKeys(_) => {
                        push_log("⚠ Some required keys are missing from configuration".to_string(), LogLevel::Warn);
                    }
                }

                // 4. Detect environment mode
                push_log("Detecting environment...".to_string(), LogLevel::Info);
                push_log("  → Scanning storage configuration...".to_string(), LogLevel::Info);
                let d = dir.clone();
                let env_mode = tokio::task::spawn_blocking(move || env_mode::detect_mode(&d))
                    .await
                    .unwrap_or(env_mode::EnvMode::Local);
                push_log("  ✓ Environment detection complete".to_string(), LogLevel::Info);

                match env_mode {
                    env_mode::EnvMode::Local => {
                        push_log("✓ Environment: Local (all services → Azurite)".to_string(), LogLevel::Success);
                    }
                    env_mode::EnvMode::Azure => {
                        push_log("✓ Environment: Azure (all services → real Azure)".to_string(), LogLevel::Success);
                    }
                    env_mode::EnvMode::Mixed => {
                        push_log("ℹ Environment: Mixed (some local, some Azure)".to_string(), LogLevel::Info);
                    }
                    env_mode::EnvMode::Unknown => {
                        push_log("ℹ Environment: Unknown (no blob endpoints configured)".to_string(), LogLevel::Info);
                    }
                }

                push_log("Initialization complete!".to_string(), LogLevel::Success);
                push_log("✓ Ready to proceed — check button below".to_string(), LogLevel::Success);
                checks_done_inner.set(true);
            });
        }
    });

    // Auto-scroll log to bottom when new messages arrive
    use_effect(move || {
        let _ = dioxus::document::eval(r#"
            const logEl = document.getElementById('loading-log');
            if (logEl) {
                logEl.scrollTop = logEl.scrollHeight;
            }
        "#);
    });

    let has_errors = log_lines.read().iter().any(|(_, level)| *level == LogLevel::Error);

    rsx! {
        div { id: "loading-screen",
            div { id: "loading-header",
                h2 { "Initializing..." }
                p { "Please wait while we check your environment." }
            }

            div { id: "loading-content",
                div { id: "loading-log",
                    if log_lines.read().is_empty() {
                        div { class: "log-empty", "Starting initialization..." }
                    } else {
                        for (line, level) in log_lines.read().iter() {
                            div {
                                class: format!("log-line log-{}", level_to_class(*level)),
                                "{line}"
                            }
                        }
                    }
                }

                if *checks_done.read() {
                    if has_errors {
                        div { class: "loading-footer loading-footer-warning",
                            p { "⚠ Some checks failed. See details above. You can still proceed." }
                        }
                    } else {
                        div { class: "loading-footer",
                            p { "✓ All checks passed! Ready to open your project." }
                        }
                    }
                } else {
                    div { class: "loading-spinner",
                        div { class: "spinner" }
                    }
                }
            }

            div { class: "loading-actions",
                match checks_done() {
                    true => rsx! {
                        button {
                            class: "btn btn-run",
                            onclick: {
                                let on_done = props.on_done.clone();
                                let dir = dir.clone();
                                move |_| on_done.call(dir.clone())
                            },
                            "▶ Open Project"
                        }
                    },
                    false => rsx! { }
                }
                button {
                    class: "btn-back",
                    onclick: {
                        let on_back = props.on_back.clone();
                        move |_| on_back.call(())
                    },
                    "← Back"
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum LogLevel {
    Info,
    Success,
    Warn,
    Error,
}

fn level_to_class(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Info => "info",
        LogLevel::Success => "success",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
    }
}
