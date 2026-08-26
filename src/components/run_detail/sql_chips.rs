//! SQL stored-procedure chips (analysis bar) and missing-object hints.

use dioxus::prelude::*;

// ── SQL stored-procedure chip in analysis bar ─────────────────────────────

#[component]
pub(super) fn SqlSprocChip(name: String, params: Vec<String>, status: Option<bool>) -> Element {
    let mut open = use_signal(|| false);
    let mut copied = use_signal(|| false);
    let param_refs: Vec<&str> = params.iter().map(|s| s.as_str()).collect();
    let stub = crate::services::sql_hint::stub_sproc_with_params(&name, &param_refs);

    let (dot_color, dot_title) = match status {
        Some(true) => ("#3fb950", format!("Found in local SQL: {}", name)),
        Some(false) => ("#f85149", format!("MISSING in local SQL: {}", name)),
        None => ("#8b949e", format!("Probing local SQL for: {}", name)),
    };

    rsx! {
        span { class: "wf-chip wf-chip-sql", title: "Calls stored procedure: {name}",
            span {
                title: "{dot_title}",
                style: "display:inline-block;width:7px;height:7px;border-radius:50%;background:{dot_color};margin-right:5px;vertical-align:middle",
            }
            span { class: "wf-type", "SP" }
            span { class: "wf-name", "{name}" }
            button {
                class: "wf-chip-payload-btn",
                title: "Show stub DDL",
                onclick: move |e| { e.stop_propagation(); open.set(!open()); },
                "📋"
            }
        }
        if open() {
            div { class: "wf-payload-popover",
                div { class: "wf-payload-popover-header",
                    span { "Stub DDL — {name}" }
                    div { style: "display:flex;gap:6px;align-items:center",
                        button {
                            class: "wf-payload-copy-btn",
                            onclick: {
                                let s = stub.clone();
                                move |_| {
                                    let _ = document::eval(&format!(
                                        "navigator.clipboard.writeText({})",
                                        serde_json::to_string(&s).unwrap_or_default()
                                    ));
                                    copied.set(true);
                                    let mut c = copied;
                                    spawn(async move {
                                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                        c.set(false);
                                    });
                                }
                            },
                            if copied() { "✅ Copied" } else { "📋 Copy" }
                        }
                        button {
                            class: "btn-icon",
                            onclick: move |_| open.set(false),
                            "×"
                        }
                    }
                }
                pre { class: "wf-payload-code", "{stub}" }
            }
        }
    }
}

// ── Missing-SQL-object hint banner (under failed action detail) ────────────

#[component]
pub(super) fn SqlMissingHint(
    hint: crate::services::sql_hint::SqlMissingObject,
    indent_px: u32,
) -> Element {
    let mut copied = use_signal(|| false);
    let kind_label = hint.kind.label();
    let name = hint.name.clone();
    let ddl = hint.stub_ddl.clone();
    let raw = hint.raw_message.clone();

    rsx! {
        div {
            class: "sql-missing-hint",
            style: "margin:6px 0; padding:10px 12px; background:rgba(210,153,34,0.12); \
                    border-left:3px solid #d29922; border-radius:4px; font-size:13px; \
                    padding-left:{indent_px + 12}px;",
            div { style: "display:flex;justify-content:space-between;align-items:center;gap:10px",
                div {
                    strong { "⚠ Missing {kind_label} locally: " }
                    code { style: "background:rgba(0,0,0,0.25); padding:1px 6px; border-radius:3px", "{name}" }
                    div { style: "opacity:0.8; margin-top:4px; font-size:12px", "{raw}" }
                }
                button {
                    class: "wf-payload-copy-btn",
                    onclick: {
                        let s = ddl.clone();
                        move |_| {
                            let _ = document::eval(&format!(
                                "navigator.clipboard.writeText({})",
                                serde_json::to_string(&s).unwrap_or_default()
                            ));
                            copied.set(true);
                            let mut c = copied;
                            spawn(async move {
                                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                c.set(false);
                            });
                        }
                    },
                    if copied() { "✅ Copied DDL" } else { "📋 Copy stub DDL" }
                }
            }
            pre {
                style: "margin-top:8px; padding:8px; background:rgba(0,0,0,0.35); \
                        border-radius:4px; font-size:11px; overflow-x:auto; white-space:pre",
                "{ddl}"
            }
        }
    }
}
