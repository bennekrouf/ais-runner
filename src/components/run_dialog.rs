use dioxus::prelude::*;

#[derive(Props, Clone, PartialEq)]
pub struct RunDialogProps {
    pub workflow: String,
    pub trigger_type: String,
    pub payload: String,
    pub blob_container: Option<String>, // Some("kyriba-input") for blob triggers
    pub queue_name: Option<String>,     // Some("ais.pivot.trading.je-local") for SB triggers
    pub on_run: EventHandler<(String, String)>, // (blob_name_or_queue, body)
    pub on_cancel: EventHandler<()>,
}

#[component]
pub fn RunDialog(props: RunDialogProps) -> Element {
    let mut body = use_signal(|| props.payload.clone());

    let trigger_lower = props.trigger_type.to_lowercase();
    let is_http = matches!(trigger_lower.as_str(), "request" | "http");
    let is_schedule = matches!(trigger_lower.as_str(), "recurrence" | "schedule");
    let is_blob = props.blob_container.is_some();
    let is_service_bus = props.queue_name.is_some() && !is_blob;

    let hint = if is_blob {
        "Blob trigger — upload a file to the container, then click Watch to poll for the run."
    } else if is_service_bus {
        "Service Bus trigger — body will be sent as a message to the input queue."
    } else if is_http {
        "HTTP Request trigger — body will be POSTed to the callback URL."
    } else if is_schedule {
        "Recurrence trigger — runs on a schedule, no body is consumed by this workflow."
    } else {
        "Trigger will be fired via /run. Body is passed as query input."
    };

    let run_label = if is_blob {
        "👁 Watch"
    } else if is_service_bus {
        "📨 Send"
    } else {
        "▶  Run"
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
                // ── Blob trigger ──────────────────────────────────────
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
            } else if is_service_bus {
                // ── Service Bus trigger ───────────────────────────────
                div { id: "run-dialog-body",
                    if let Some(queue) = &props.queue_name {
                        div { class: "dialog-blob-info",
                            span { class: "dialog-label", "Input queue" }
                            span { class: "dialog-blob-container", "📨 {queue}" }
                        }
                    }
                    div { class: "dialog-label", "Message body (JSON)" }
                    textarea {
                        id: "run-dialog-textarea",
                        spellcheck: false,
                        value: "{body}",
                        oninput: move |e| body.set(e.value()),
                    }
                }
            } else if is_schedule {
                // ── Recurrence trigger ────────────────────────────────
                div { id: "run-dialog-body",
                    div { class: "dialog-no-body",
                        span { "⏱ This workflow is schedule-triggered." }
                        span { "No request body is required — click Run to fire it." }
                    }
                }
            } else {
                // ── HTTP / generic trigger ────────────────────────────
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
                    onclick: {
                        let queue = props.queue_name.clone().unwrap_or_default();
                        move |_| props.on_run.call((queue.clone(), body.read().clone()))
                    },
                    "{run_label}"
                }
            }
        }
    }
}
