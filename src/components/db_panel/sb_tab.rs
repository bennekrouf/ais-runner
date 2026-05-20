use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};
use crate::services::{
    azure_cli::{self, SbQueueStats},
    sb_check::SbQueueInfo,
};

#[derive(Props, Clone, PartialEq)]
pub struct SbTabProps {
    pub sb_queues:    Vec<SbQueueInfo>,
    pub sb_namespace: String,
    pub subscription: Signal<Option<String>>,
    pub is_open:      Signal<bool>,
    pub active_tab:   Signal<&'static str>,
    pub status:       Signal<Option<(String, bool)>>,
}

#[component]
pub fn SbTab(props: SbTabProps) -> Element {
    let mut sb_rg:          Signal<Option<String>>    = use_signal(|| None);
    let mut sb_stats:       Signal<HashMap<String, SbQueueStats>> = use_signal(HashMap::new);
    let mut sb_fetching:    Signal<HashSet<String>>   = use_signal(HashSet::new);
    let mut sb_queue_err:   Signal<HashMap<String, String>> = use_signal(HashMap::new);
    let mut sb_send_open:   Signal<HashSet<String>>   = use_signal(HashSet::new);
    let mut sb_send_bodies: Signal<HashMap<String, String>> = use_signal(HashMap::new);
    let mut sb_peek_open:   Signal<HashSet<String>>   = use_signal(HashSet::new);
    let mut sb_peek_msgs:   Signal<HashMap<String, Vec<String>>> = use_signal(HashMap::new);
    let mut sb_peeking:     Signal<HashSet<String>>   = use_signal(HashSet::new);

    let mut new_queue_name:     Signal<String> = use_signal(String::new);
    let mut new_queue_creating: Signal<bool>   = use_signal(|| false);
    let mut queue_filter:       Signal<String> = use_signal(String::new);

    let mut status     = props.status;
    let subscription   = props.subscription;
    let active_tab     = props.active_tab;

    // Signal so use_effect (FnMut) can clone it on each call without moving
    let sb_namespace_sig = use_signal(|| props.sb_namespace.clone());
    let sb_namespace_rsx = props.sb_namespace.clone();

    // Auto-refresh SB queue message counts every 3 s while panel is open on the SB tab.
    use_effect(move || {
        let is_open = props.is_open;
        spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;

                if !*is_open.read() || *active_tab.peek() != "sb" {
                    continue;
                }
                let queues: Vec<String> = sb_stats.peek().keys().cloned().collect();
                if queues.is_empty() { continue; }

                let ns  = sb_namespace_sig.read().clone();
                let rg  = sb_rg.peek().clone();
                let Some(rg) = rg else { continue; };
                if ns.is_empty() { continue; }

                for q in queues {
                    let (ns2, rg2, q2) = (ns.clone(), rg.clone(), q.clone());
                    if let Ok(Ok(stats)) = tokio::task::spawn_blocking(move || {
                        azure_cli::sb_queue_stats(&rg2, &ns2, &q2)
                    }).await {
                        sb_stats.write().insert(q, stats);
                    }
                }
            }
        });
    });

    rsx! {
        div { class: "db-section-title", style: "margin-top:20px;",
            span { "📨 Service Bus" }
        }

        div { class: "db-create-row",
            input {
                class: "db-field-input",
                style: "flex: 1",
                placeholder: "Create new queue (e.g. ais.workflow.error)...",
                value: "{new_queue_name}",
                oninput: move |e| new_queue_name.set(e.value()),
            }
            button {
                class: "btn btn-run btn-small",
                disabled: *new_queue_creating.read() || new_queue_name.read().is_empty(),
                onclick: move |_| {
                    let q = new_queue_name.read().clone();
                    new_queue_creating.set(true);
                    spawn(async move {
                        let emulator_up = tokio::net::TcpStream::connect("127.0.0.1:5672").await.is_ok();
                        if emulator_up {
                            let q2 = q.clone();
                            let result = tokio::task::spawn_blocking(move || {
                                crate::handlers::sb_emulator::add_queue_to_emulator_config(&q2)
                            }).await.unwrap_or(Err("task failed".into()));
                            match result {
                                Ok(true) => {
                                    status.set(Some((
                                        format!("✅ '{}' added — restart SB Emulator to apply.", q),
                                        false,
                                    )));
                                    new_queue_name.set(String::new());
                                }
                                Ok(false) => {
                                    status.set(Some((
                                        format!("ℹ '{}' already exists.", q),
                                        false,
                                    )));
                                    new_queue_name.set(String::new());
                                }
                                Err(e) => status.set(Some((format!("Config update failed: {}", e), true))),
                            }
                        } else {
                            status.set(Some(("SB Emulator is not running — start it from the toolbar first.".into(), true)));
                        }
                        new_queue_creating.set(false);
                    });
                },
                if *new_queue_creating.read() { "Creating..." } else { "➕ Create Queue" }
            }
        }

        // Filter
        div { class: "log-filter-wrap", style: "margin: 6px 0 4px;",
            input {
                class: "log-filter-input",
                style: "width:100%;box-sizing:border-box;",
                r#type: "text",
                placeholder: "Filter queues…",
                value: "{queue_filter}",
                oninput: move |e| queue_filter.set(e.value()),
            }
            if !queue_filter.read().is_empty() {
                button {
                    class: "log-filter-clear",
                    title: "Clear filter",
                    onclick: move |_| queue_filter.set(String::new()),
                    "×"
                }
            }
        }

        // Queue cards (local workflows)
        for q in props.sb_queues.clone().into_iter().filter(|q| {
            let f = queue_filter.read().to_lowercase();
            f.is_empty() || q.queue.to_lowercase().contains(&f)
        }) {
            {
                let sb_ns = sb_namespace_rsx.clone();
                let queue_name  = q.queue.clone();
                let queue_name2 = queue_name.clone();
                let queue_name3 = queue_name.clone();
                let queue_name4 = queue_name.clone();
                let ns  = sb_ns.clone();
                let sub_stats  = subscription.read().clone();
                let is_fetching    = sb_fetching.read().contains(&queue_name);
                let stats_opt      = sb_stats.read().get(&queue_name).cloned();
                let queue_err      = sb_queue_err.read().get(&queue_name).cloned();
                let is_send_open   = sb_send_open.read().contains(&queue_name);
                let send_body      = sb_send_bodies.read().get(&queue_name).cloned().unwrap_or_default();
                let is_peek_open   = sb_peek_open.read().contains(&queue_name);
                let peek_msgs      = sb_peek_msgs.read().get(&queue_name).cloned();
                let is_peeking     = sb_peeking.read().contains(&queue_name);
                let queue_name5    = queue_name.clone();
                let queue_name6    = queue_name.clone();

                rsx! {
                    div { class: "db-card",
                        div { class: "db-card-header",
                            span { class: "db-card-name", "{queue_name}" }
                            div { style: "display:flex;gap:4px;align-items:center",
                                if !q.trigger_workflows.is_empty() {
                                    span {
                                        class: "tooltip-container db-wf-badge trigger",
                                        "T:{q.trigger_workflows.len()}"
                                        span { class: "tooltip-text", "{q.trigger_workflows.join(\", \")}" }
                                    }
                                }
                                if !q.action_workflows.is_empty() {
                                    span {
                                        class: "tooltip-container db-wf-badge action",
                                        "A:{q.action_workflows.len()}"
                                        span { class: "tooltip-text", "{q.action_workflows.join(\", \")}" }
                                    }
                                }
                            }
                            div { style: "margin-left:auto;display:flex;gap:6px;align-items:center",
                                if let Some(ref s) = stats_opt {
                                    span { class: "db-sb-stats",
                                        "📬 {s.active_message_count}"
                                        if s.dead_letter_count > 0 {
                                            span { class: "db-sb-dlq",
                                                " DLQ:{s.dead_letter_count}"
                                            }
                                        }
                                    }
                                }
                                button {
                                    class: "btn btn-small btn-fetch",
                                    disabled: is_fetching || crate::services::sb_check::is_local_emulator(&ns),
                                    title: if crate::services::sb_check::is_local_emulator(&ns) { "Stats not available for local emulator" } else { "Fetch active message count (requires az login)" },
                                    onclick: move |_| {
                                        let qn  = queue_name2.clone();
                                        let qn2 = queue_name2.clone();
                                        let ns3 = ns.clone();
                                        let rg_now = sb_rg.read().clone();
                                        let sub3 = sub_stats.clone();
                                        sb_fetching.write().insert(qn.clone());
                                        sb_queue_err.write().remove(&qn);
                                        spawn(async move {
                                            let rg = if let Some(r) = rg_now {
                                                r
                                            } else {
                                                let ns4 = ns3.clone();
                                                match tokio::task::spawn_blocking(move || {
                                                    azure_cli::sb_find_rg(&ns4, sub3.as_deref())
                                                }).await {
                                                    Ok(Ok(r)) => {
                                                        sb_rg.set(Some(r.clone()));
                                                        r
                                                    }
                                                    _ => {
                                                        sb_queue_err.write().insert(
                                                            qn.clone(),
                                                            "Could not find resource group — check az login".into(),
                                                        );
                                                        sb_fetching.write().remove(&qn);
                                                        return;
                                                    }
                                                }
                                            };
                                            match tokio::task::spawn_blocking(move || {
                                                azure_cli::sb_queue_stats(&rg, &ns3, &qn)
                                            }).await {
                                                Ok(Ok(s)) => {
                                                    sb_stats.write().insert(qn2.clone(), s);
                                                    sb_queue_err.write().remove(&qn2);
                                                }
                                                Ok(Err(e)) => {
                                                    sb_queue_err.write().insert(
                                                        qn2.clone(),
                                                        format!("{:?}", e),
                                                    );
                                                }
                                                Err(_) => {
                                                    sb_queue_err.write().insert(
                                                        qn2.clone(),
                                                        "Stats task failed".into(),
                                                    );
                                                }
                                            }
                                            sb_fetching.write().remove(&qn2);
                                        });
                                    },
                                    if is_fetching { "…" } else { "Count" }
                                }
                                button {
                                    class: "btn btn-small",
                                    title: "Send a test message to this queue",
                                    onclick: move |_| {
                                        if sb_send_open.read().contains(&queue_name3) {
                                            sb_send_open.write().remove(&queue_name3);
                                        } else {
                                            sb_send_open.write().insert(queue_name3.clone());
                                        }
                                    },
                                    if is_send_open { "▲ Close" } else { "📤 Send" }
                                }
                                button {
                                    class: "btn btn-small",
                                    title: "Peek messages in this queue (non-destructive)",
                                    disabled: is_peeking,
                                    onclick: move |_| {
                                        if sb_peek_open.read().contains(&queue_name5) {
                                            sb_peek_open.write().remove(&queue_name5);
                                        } else {
                                            sb_peek_open.write().insert(queue_name5.clone());
                                            let qn = queue_name5.clone();
                                            let qn2 = queue_name5.clone();
                                            sb_peeking.write().insert(qn.clone());
                                            spawn(async move {
                                                let result = crate::services::sb_amqp::peek_amqp_messages(
                                                    "localhost", &qn, 10
                                                ).await;
                                                match result {
                                                    Ok(msgs) => { sb_peek_msgs.write().insert(qn2.clone(), msgs); }
                                                    Err(e)   => { sb_peek_msgs.write().insert(qn2.clone(), vec![format!("⚠ {e}")]); }
                                                }
                                                sb_peeking.write().remove(&qn2);
                                            });
                                        }
                                    },
                                    if is_peeking { "…" } else if is_peek_open { "▲ Hide" } else { "👁 Peek" }
                                }
                            }
                        }

                        // Inline error
                        if let Some(err) = queue_err {
                            div { class: "db-queue-err", "⚠ {err}" }
                        }

                        // Inline send form
                        if is_send_open {
                            {
                                let qn  = queue_name4.clone();
                                let qn2 = queue_name4.clone();
                                rsx! {
                                    div { class: "db-send-form",
                                        textarea {
                                            class: "db-field-textarea",
                                            placeholder: "{{ \"key\": \"value\" }}",
                                            value: "{send_body}",
                                            oninput: move |e| {
                                                sb_send_bodies.write().insert(qn.clone(), e.value());
                                            },
                                        }
                                        div { style: "display:flex;gap:8px;margin-top:6px",
                                            button {
                                                class: "btn btn-run btn-small",
                                                onclick: move |_| {
                                                    let qn4  = qn2.clone();
                                                    let body = sb_send_bodies.read()
                                                        .get(&qn2).cloned().unwrap_or_default();
                                                    spawn(async move {
                                                        let emulator_up = tokio::net::TcpStream::connect("127.0.0.1:5672").await.is_ok();
                                                        let result = if emulator_up {
                                                            crate::services::sb_amqp::send_amqp_message(
                                                                "localhost", &qn4, &body,
                                                            ).await
                                                        } else {
                                                            Err("SB Emulator is not running — start it from the toolbar first.".into())
                                                        };
                                                        match result {
                                                            Ok(()) => status.set(Some((
                                                                format!("✅ Sent to {}", qn4), false,
                                                            ))),
                                                            Err(e) => status.set(Some((e, true))),
                                                        }
                                                    });
                                                },
                                                "Send"
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // Inline peek messages
                        if is_peek_open {
                            div { class: "db-peek-panel",
                                if is_peeking {
                                    div { class: "db-peek-loading", "Loading messages…" }
                                } else if let Some(ref msgs) = peek_msgs {
                                    if msgs.is_empty() {
                                        div { class: "db-peek-empty", "Queue is empty" }
                                    } else {
                                        for (i, msg) in msgs.iter().enumerate() {
                                            div { class: "db-peek-msg",
                                                span { class: "db-peek-idx", "#{i}" }
                                                pre { class: "db-peek-body", "{msg}" }
                                            }
                                        }
                                    }
                                    button {
                                        class: "btn btn-small",
                                        style: "margin-top:6px",
                                        onclick: move |_| {
                                            let qn = queue_name6.clone();
                                            let qn2 = queue_name6.clone();
                                            sb_peeking.write().insert(qn.clone());
                                            spawn(async move {
                                                let result = crate::services::sb_amqp::peek_amqp_messages(
                                                    "localhost", &qn, 10
                                                ).await;
                                                match result {
                                                    Ok(msgs) => { sb_peek_msgs.write().insert(qn2.clone(), msgs); }
                                                    Err(e)   => { sb_peek_msgs.write().insert(qn2.clone(), vec![format!("⚠ {e}")]); }
                                                }
                                                sb_peeking.write().remove(&qn2);
                                            });
                                        },
                                        "🔄 Refresh"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // ── Emulator-only queues (manually added, not in any workflow) ─
        {
            let extra = crate::services::sb_check::emulator_only_queues(&props.sb_queues);
            if !extra.is_empty() {
                rsx! {
                    div { class: "db-section-sub", style: "margin-top:12px;opacity:0.7",
                        "📋 Emulator-only queues"
                    }
                    for qname in extra {
                        { let qn = qname.clone();
                          let qn2 = qname.clone();
                          let is_open = sb_send_open.read().contains(&qname);
                          let send_body = sb_send_bodies.read().get(&qname).cloned().unwrap_or_default();
                          rsx! {
                            div { class: "db-card",
                                div { class: "db-card-header",
                                    span { class: "db-card-name", "{qname}" }
                                    button {
                                        class: "btn btn-small",
                                        onclick: move |_| {
                                            if sb_send_open.read().contains(&qn) {
                                                sb_send_open.write().remove(&qn);
                                            } else {
                                                sb_send_open.write().insert(qn.clone());
                                            }
                                        },
                                        if is_open { "▲ Close" } else { "📤 Send" }
                                    }
                                }
                                if is_open {
                                    div { class: "db-send-form",
                                        textarea {
                                            class: "db-field-textarea",
                                            placeholder: "{{ \"key\": \"value\" }}",
                                            value: "{send_body}",
                                            oninput: move |e| { sb_send_bodies.write().insert(qn2.clone(), e.value()); },
                                        }
                                        button {
                                            class: "btn btn-run btn-small",
                                            onclick: move |_| {
                                                let qn3 = qname.clone();
                                                let body = sb_send_bodies.read().get(&qname).cloned().unwrap_or_default();
                                                spawn(async move {
                                                    let emulator_up = tokio::net::TcpStream::connect("127.0.0.1:5672").await.is_ok();
                                                    let result = if emulator_up {
                                                        crate::services::sb_amqp::send_amqp_message("localhost", &qn3, &body).await
                                                    } else {
                                                        Err("SB Emulator is not running — start it from the toolbar first.".into())
                                                    };
                                                    match result {
                                                        Ok(()) => status.set(Some((format!("✅ Sent to {}", qn3), false))),
                                                        Err(e) => status.set(Some((e, true))),
                                                    }
                                                });
                                            },
                                            "Send"
                                        }
                                    }
                                }
                            }
                          }
                        }
                    }
                }
            } else {
                rsx! {}
            }
        }
    }
}
