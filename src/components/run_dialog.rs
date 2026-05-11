use dioxus::prelude::*;

#[derive(Props, Clone, PartialEq)]
pub struct RunDialogProps {
    pub workflow:        String,
    pub trigger_type:    String,
    pub payload:         String,
    pub blob_container:  Option<String>,   // Some("kyriba-input") for blob triggers
    pub on_run:          EventHandler<(String, String)>, // (blob_name, body)
    pub on_cancel:       EventHandler<()>,
}

#[component]
pub fn RunDialog(props: RunDialogProps) -> Element {
    let mut body = use_signal(|| props.payload.clone());

    let trigger_lower = props.trigger_type.to_lowercase();
    let is_http       = matches!(trigger_lower.as_str(), "request" | "http");
    let is_schedule   = matches!(trigger_lower.as_str(), "recurrence" | "schedule");
    let is_blob       = props.blob_container.is_some();

    let hint = if is_blob {
        "Blob trigger — upload a file to the container, then click Watch to poll for the run."
    } else if is_http {
        "HTTP Request trigger — body will be POSTed to the callback URL."
    } else if is_schedule {
        "Recurrence trigger — runs on a schedule, no body is consumed by this workflow."
    } else {
        "Trigger will be fired via /run. Body is passed as query input."
    };

    rsx! {
        // ── backdrop ──────────────────────────────────────────────────
        div {
            id: "dialog-backdrop",
            onclick: move |_| props.on_cancel.call(()),
        }

        // ── dialog box ────────────────────────────────────────────────
        div { id: "run-dialog",

            div { id: "run-dialog-header",
                div {
                    h3 { "▶  {props.workflow}" }
                    span { class: "dialog-hint", "{hint}" }
                }
                button {
                    class: "btn-icon",
                    onclick: move |_| props.on_cancel.call(()),
                    "×"
                }
            }

            if is_blob {
                // ── Blob trigger: instruct user to upload manually ────
                div { id: "run-dialog-body",
                    if let Some(container) = &props.blob_container {
                        div { class: "dialog-blob-info",
                            span { class: "dialog-label", "Trigger container" }
                            span { class: "dialog-blob-container", "📦 {container}" }
                        }
                        div { class: "dialog-blob-instructions",
                            "Upload a file to "
                            strong { "{container}" }
                            " via the DB panel → Blobs tab, then click Watch. \
                             ais-runner will poll until the workflow run appears."
                        }
                    }
                }
            } else if is_schedule {
                div { id: "run-dialog-body",
                    div { class: "dialog-no-body",
                        span { "⏱ This workflow is schedule-triggered." }
                        span { "No request body is required — click Run to fire it." }
                    }
                }
            } else {
                div { id: "run-dialog-body",
                    div { class: "dialog-label", "Request body (JSON)" }
                    textarea {
                        id: "run-dialog-textarea",
                        spellcheck: false,
                        value: "{body}",
                        oninput: move |e| body.set(e.value()),
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
                    onclick: move |_| props.on_run.call((
                        String::new(),
                        body.read().clone(),
                    )),
                    if is_blob { "👁 Watch" } else { "▶  Run" }
                }
            }
        }
    }
}
