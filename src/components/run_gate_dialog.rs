use dioxus::prelude::*;

use crate::services::run_readiness::RunReadiness;

#[derive(Props, Clone, PartialEq)]
pub struct RunGateDialogProps {
    pub readiness: RunReadiness,
    /// Consent granted: apply the auto-fixes and scaffold local files.
    pub on_fix:    EventHandler<()>,
    pub on_cancel: EventHandler<()>,
}

/// Blocking consent modal shown when a workflow can't run locally because its
/// connections aren't set up for the emulators. It states plainly that
/// ais-runner will modify local files, that doing so is required, and lists
/// exactly what the developer still has to supply by hand.
#[component]
pub fn RunGateDialog(props: RunGateDialogProps) -> Element {
    let r = &props.readiness;
    let has_auto = !r.auto_fixable.is_empty();

    rsx! {
        div {
            id: "dialog-backdrop",
            onclick: move |_| props.on_cancel.call(()),
        }

        div { id: "run-dialog", style: "max-width: 640px",

            div { id: "run-dialog-header",
                div {
                    h3 { "⚠  '{r.workflow}' isn't ready to run locally" }
                    span { class: "dialog-hint",
                        "ais-runner runs workflows only against local emulators — never the cloud. \
                         Some of this workflow's connections aren't local yet, so the run is blocked."
                    }
                }
                button {
                    class: "btn-icon",
                    onclick: move |_| props.on_cancel.call(()),
                    "×"
                }
            }

            div { id: "run-dialog-body",

                // ── Empty settings we can fill with a local default ──────
                if has_auto {
                    div { class: "dialog-label", "ais-runner will fill these with local defaults" }
                    ul { style: "margin: 0 0 .75rem 1.1rem; padding: 0; font-size: .82rem; line-height: 1.5",
                        for (conn, key, default) in r.auto_fixable.iter() {
                            li {
                                span { style: "font-weight: 600", "{key}" }
                                " → "
                                code { "{default}" }
                                span { style: "opacity:.6", "  (connection ‘{conn}’)" }
                            }
                        }
                    }
                }

                // ── Cloud endpoints we will redirect to local ────────────
                if !r.cloud_pointing.is_empty() {
                    div { class: "dialog-label", "ais-runner will redirect these off the cloud → local" }
                    ul { style: "margin: 0 0 .75rem 1.1rem; padding: 0; font-size: .82rem; line-height: 1.5",
                        for (key, cloud, target) in r.cloud_pointing.iter() {
                            li {
                                span { style: "font-weight: 600", "{key}" }
                                ": "
                                span { style: "opacity:.6; text-decoration: line-through", "{cloud}" }
                                " → "
                                code { "{target}" }
                            }
                        }
                    }
                }

                // ── Empty settings with no known local mapping ───────────
                if !r.blocking_settings.is_empty() {
                    div { class: "dialog-label", "Point these at a local emulator or the mock server (never a cloud value)" }
                    ul { style: "margin: 0 0 .75rem 1.1rem; padding: 0; font-size: .82rem; line-height: 1.5",
                        for (conn, key) in r.blocking_settings.iter() {
                            li {
                                span { style: "font-weight: 600", "{key}" }
                                " in "
                                code { "local.settings.json" }
                                span { style: "opacity:.6", "  (connection ‘{conn}’)" }
                            }
                        }
                    }
                }

                if !r.missing_connections.is_empty() {
                    div { class: "dialog-label", "Connections used but missing from connections.json" }
                    ul { style: "margin: 0 0 .75rem 1.1rem; padding: 0; font-size: .82rem; line-height: 1.5",
                        for conn in r.missing_connections.iter() {
                            li { code { "{conn}" } }
                        }
                    }
                }

                // ── Consent notice ───────────────────────────────────────
                div {
                    style: "margin-top: .5rem; padding: .6rem .75rem; border-radius: 6px; \
                            background: rgba(220,160,40,.12); border: 1px solid rgba(220,160,40,.4); \
                            font-size: .8rem; line-height: 1.5",
                    strong { "This will modify local files in your repo:" }
                    br {}
                    "• "
                    code { "local.settings.json" }
                    " — fill local defaults and redirect cloud endpoints to the emulators/mock."
                    br {}
                    "• "
                    code { "connections.local.json" }
                    " — created (gitignored) for persistent local overrides."
                    br {}
                    span { style: "opacity:.75",
                        "Your committed "
                        code { "connections.json" }
                        " is left unchanged. These edits are required to run locally."
                    }
                }
            }

            div { id: "run-dialog-footer",
                button {
                    class: "btn btn-small",
                    onclick: move |_| props.on_cancel.call(()),
                    "Cancel"
                }
                button {
                    class: "btn btn-run btn-small",
                    onclick: move |_| props.on_fix.call(()),
                    "Set up local files"
                }
            }
        }
    }
}
