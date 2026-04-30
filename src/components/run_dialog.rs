use dioxus::prelude::*;

#[derive(Props, Clone, PartialEq)]
pub struct RunDialogProps {
    pub workflow:     String,
    pub trigger_type: String,
    pub payload:      String,          // initial suggested payload
    pub on_run:       EventHandler<String>, // called with final payload text
    pub on_cancel:    EventHandler<()>,
}

#[component]
pub fn RunDialog(props: RunDialogProps) -> Element {
    let mut body = use_signal(|| props.payload.clone());

    let is_http = matches!(
        props.trigger_type.to_lowercase().as_str(),
        "request" | "http"
    );
    let hint = if is_http {
        "HTTP Request trigger — body will be POSTed to the callback URL."
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

            div { id: "run-dialog-body",
                div { class: "dialog-label", "Request body (JSON)" }
                textarea {
                    id: "run-dialog-textarea",
                    spellcheck: false,
                    value: "{body}",
                    oninput: move |e| body.set(e.value()),
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
                    onclick: move |_| props.on_run.call(body.read().clone()),
                    "▶  Run"
                }
            }
        }
    }
}
