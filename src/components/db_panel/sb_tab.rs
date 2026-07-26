use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};
use crate::services::{
    azure_cli::{self, SbQueueStats},
    sb_check::SbQueueInfo,
};
use crate::services::sb_amqp::PeekedMessage;

/// One row in the inline peek list. Either a real peeked message (body + the
/// AMQP `delivery-count` so the user can spot poison-loop messages without
/// leaving the queue browser) or a textual error from the peek call itself.
#[derive(Clone, Debug, PartialEq)]
enum PeekRow {
    Msg(PeekedMessage),
    Err(String),
}

#[derive(Props, Clone, PartialEq)]
pub struct SbTabProps {
    pub sb_queues:      Vec<SbQueueInfo>,
    pub sb_namespace:   String,
    pub logic_apps_dir: String,
    pub subscription:   Signal<Option<String>>,
    pub is_open:        Signal<bool>,
    pub active_tab:     Signal<&'static str>,
    pub status:         Signal<Option<(String, bool)>>,
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
    // Holds either `Ok(PeekedMessage)` rows or a single `Err(error_text)` slot
    // — modelled with `PeekRow` so we can render the body + delivery-count
    // chip per row while still surfacing peek-level errors.
    let mut sb_peek_msgs:   Signal<HashMap<String, Vec<PeekRow>>> = use_signal(HashMap::new);
    let mut sb_peeking:     Signal<HashSet<String>>   = use_signal(HashSet::new);
    // Flush (purge) is destructive, so it's two-step: the button arms a confirm,
    // a second click drains. `sb_flushing` marks the in-flight drain.
    let mut sb_flush_confirm: Signal<Option<String>> = use_signal(|| None);
    let mut sb_flushing:      Signal<HashSet<String>> = use_signal(HashSet::new);

    let mut new_queue_name:     Signal<String> = use_signal(String::new);
    let mut new_queue_creating: Signal<bool>   = use_signal(|| false);
    let mut queue_filter:       Signal<String> = use_signal(String::new);
    // Create and Trace are secondary actions collapsed behind icon buttons so
    // the filter (the primary control) owns the toolbar.
    let mut create_open: Signal<bool> = use_signal(|| false);
    let mut trace_open:  Signal<bool> = use_signal(|| false);

    // Send variants: burst count and null-field path, per queue.
    let mut sb_send_counts: Signal<HashMap<String, String>> = use_signal(HashMap::new);
    let mut sb_null_fields: Signal<HashMap<String, String>> = use_signal(HashMap::new);

    // Correlation trace across all queues.
    let mut trace_input:   Signal<String> = use_signal(String::new);
    let mut trace_running: Signal<bool>   = use_signal(|| false);
    let mut trace_hits:    Signal<Option<Vec<crate::services::sb_testing::TraceHit>>> =
        use_signal(|| None);

    // Per-queue expectation inputs (path, value, min count) and last result.
    let mut sb_assert_inputs:  Signal<HashMap<String, (String, String, String)>> =
        use_signal(HashMap::new);
    let mut sb_assert_results: Signal<HashMap<String, (bool, String)>> =
        use_signal(HashMap::new);

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
        // ── Primary toolbar: filter owns the row; create (+) and trace (🔍)
        //    are collapsed toggles to its right. ──────────────────────────
        div { class: "db-create-row",
            div { class: "log-filter-wrap", style: "flex:1;",
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
            button {
                class: if *create_open.read() { "btn btn-small btn-run" } else { "btn btn-small" },
                title: "Add a new queue to the emulator",
                onclick: move |_| { let v = !*create_open.read(); create_open.set(v); },
                "+"
            }
            button {
                class: if *trace_open.read() { "btn btn-small btn-run" } else { "btn btn-small" },
                title: "Trace a message: peeks every queue (read-only) and reports which ones hold a message containing a given id — for when a message vanished and you don't know where it landed",
                onclick: move |_| { let v = !*trace_open.read(); trace_open.set(v); },
                "🔍"
            }
        }

        // ── Create queue (revealed by +) ─────────────────────────────────
        if *create_open.read() {
            div { class: "db-create-row", style: "margin: 4px 0;",
                input {
                    class: "db-field-input",
                    style: "flex: 1",
                    placeholder: "New queue name (e.g. ais.workflow.error)",
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
                    if *new_queue_creating.read() { "Creating..." } else { "Create" }
                }
            }
        }

        // ── Trace correlation id across all queues (revealed by 🔍) ───────
        if *trace_open.read() {
            div { style: "margin: 4px 0;",
                div { class: "db-create-row",
                    input {
                        class: "db-field-input",
                        style: "flex: 1",
                        placeholder: "Correlation id to trace across all queues…",
                        value: "{trace_input}",
                        oninput: move |e| trace_input.set(e.value()),
                    }
                    button {
                        class: "btn btn-small",
                        disabled: *trace_running.read() || trace_input.read().trim().is_empty(),
                        title: "Peek every queue and count messages containing this id (read-only)",
                        onclick: {
                            let all_queues: Vec<String> = props.sb_queues.iter()
                                .map(|q| q.queue.clone())
                                .chain(crate::services::sb_check::emulator_only_queues(&props.sb_queues))
                                .collect();
                            move |_| {
                                let needle = trace_input.read().trim().to_string();
                                let queues = all_queues.clone();
                                trace_running.set(true);
                                spawn(async move {
                                    let hits = crate::services::sb_testing::trace_correlation(
                                        "localhost", &queues, &needle,
                                    ).await;
                                    trace_hits.set(Some(hits));
                                    trace_running.set(false);
                                });
                            }
                        },
                        if *trace_running.read() { "…" } else { "Trace" }
                    }
                }
                div { class: "dialog-hint", style: "margin-top:3px",
                    "Read-only. Peeks every queue and reports which hold a message containing this id — handy when a message disappeared and you don't know where it went."
                }
            }
            if let Some(ref hits) = *trace_hits.read() {
                div { class: "db-peek-panel", style: "margin: 2px 0 8px;",
                    if hits.is_empty() {
                        div { class: "db-peek-empty",
                            "No messages found — already consumed, dead-lettered, or the id is wrong."
                        }
                    } else {
                        for hit in hits.iter() {
                            div { class: "db-peek-msg",
                                span { class: "db-peek-idx", "📬" }
                                "{hit.queue}: {hit.count} message(s)"
                            }
                        }
                    }
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
                let is_flushing    = sb_flushing.read().contains(&queue_name);
                let flush_pending  = sb_flush_confirm.read().as_deref() == Some(&queue_name);
                let queue_name5    = queue_name.clone();
                let queue_name6    = queue_name.clone();
                let queue_name_fl  = queue_name.clone();
                let queue_name_fl2 = queue_name.clone();
                let la_dir         = props.logic_apps_dir.clone();
                let send_count     = sb_send_counts.read().get(&queue_name).cloned().unwrap_or_default();
                let null_path      = sb_null_fields.read().get(&queue_name).cloned().unwrap_or_default();
                let assert_inputs  = sb_assert_inputs.read().get(&queue_name).cloned()
                                        .unwrap_or_default();
                let assert_result  = sb_assert_results.read().get(&queue_name).cloned();

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
                                                    Ok(msgs) => {
                                                        let rows: Vec<PeekRow> = msgs.into_iter().map(PeekRow::Msg).collect();
                                                        sb_peek_msgs.write().insert(qn2.clone(), rows);
                                                    }
                                                    Err(e)   => { sb_peek_msgs.write().insert(qn2.clone(), vec![PeekRow::Err(format!("⚠ {e}"))]); }
                                                }
                                                sb_peeking.write().remove(&qn2);
                                            });
                                        }
                                    },
                                    if is_peeking { "…" } else if is_peek_open { "▲ Hide" } else { "👁 Peek" }
                                }
                                if flush_pending {
                                    button {
                                        class: "btn btn-small btn-danger",
                                        disabled: is_flushing,
                                        title: "Confirm — permanently remove ALL messages from this queue",
                                        onclick: move |_| {
                                            let qn  = queue_name_fl.clone();
                                            let qn2 = queue_name_fl.clone();
                                            sb_flush_confirm.set(None);
                                            sb_flushing.write().insert(qn.clone());
                                            spawn(async move {
                                                let res = crate::services::sb_amqp::drain_queue("localhost", &qn).await;
                                                match res {
                                                    Ok(n)  => status.set(Some((format!("🧹 {qn}: flushed {n} message(s)"), false))),
                                                    Err(e) => status.set(Some((format!("❌ flush {qn}: {e}"), true))),
                                                }
                                                // Reflect the drain: clear any peeked rows and zero the local count.
                                                sb_peek_msgs.write().remove(&qn2);
                                                sb_flushing.write().remove(&qn2);
                                            });
                                        },
                                        if is_flushing { "🧹 …" } else { "🧹 Confirm?" }
                                    }
                                    button {
                                        class: "btn btn-small",
                                        onclick: move |_| sb_flush_confirm.set(None),
                                        "✗"
                                    }
                                } else {
                                    button {
                                        class: "btn btn-small",
                                        disabled: is_flushing || is_peeking,
                                        title: "Flush — permanently remove all messages from this queue",
                                        onclick: move |_| sb_flush_confirm.set(Some(queue_name_fl2.clone())),
                                        if is_flushing { "🧹 …" } else { "🧹 Flush" }
                                    }
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
                                let qn_count = queue_name4.clone();
                                let qn_null  = queue_name4.clone();
                                let dir  = la_dir.clone();
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
                                        // Test variants: null a field to hit validation branches,
                                        // burst-send N copies to test alert consolidation.
                                        div { style: "display:flex;gap:8px;margin-top:6px;align-items:center",
                                            input {
                                                class: "db-field-input",
                                                style: "flex:1",
                                                placeholder: "Null out field (dot path, e.g. data.msg.content.CompanyId)…",
                                                title: "Before sending, set this field to null — tests missing-field validation branches",
                                                value: "{null_path}",
                                                oninput: move |e| {
                                                    sb_null_fields.write().insert(qn_null.clone(), e.value());
                                                },
                                            }
                                            input {
                                                class: "db-field-input",
                                                style: "width:70px",
                                                r#type: "number",
                                                min: "1",
                                                max: "500",
                                                placeholder: "×1",
                                                title: "Send this many copies — tests alert consolidation / burst behavior",
                                                value: "{send_count}",
                                                oninput: move |e| {
                                                    sb_send_counts.write().insert(qn_count.clone(), e.value());
                                                },
                                            }
                                        }
                                        div { style: "display:flex;gap:8px;margin-top:6px",
                                            button {
                                                class: "btn btn-run btn-small",
                                                onclick: move |_| {
                                                    let qn4  = qn2.clone();
                                                    let dir2 = dir.clone();
                                                    let raw  = sb_send_bodies.read()
                                                        .get(&qn2).cloned().unwrap_or_default();
                                                    let count: usize = sb_send_counts.read()
                                                        .get(&qn2).and_then(|c| c.trim().parse().ok())
                                                        .unwrap_or(1).clamp(1, 500);
                                                    let null_path = sb_null_fields.read()
                                                        .get(&qn2).cloned().unwrap_or_default();
                                                    // Strip a top-level `contentData` envelope if the user
                                                    // pasted a captured-message body — see
                                                    // payload::normalise_send_body for the why.
                                                    let normalised = crate::services::payload::normalise_send_body(&raw);
                                                    // Apply the null-field variant, if requested.
                                                    let body = if null_path.trim().is_empty() {
                                                        Ok(normalised.body.clone())
                                                    } else {
                                                        crate::services::sb_testing::null_field(
                                                            &normalised.body, null_path.trim())
                                                    };
                                                    let body = match body {
                                                        Ok(b) => b,
                                                        Err(e) => { status.set(Some((format!("Null-field: {e}"), true))); return; }
                                                    };
                                                    spawn(async move {
                                                        let emulator_up = tokio::net::TcpStream::connect("127.0.0.1:5672").await.is_ok();
                                                        if !emulator_up {
                                                            status.set(Some(("SB Emulator is not running — start it from the toolbar first.".into(), true)));
                                                            return;
                                                        }
                                                        // Pick the content-type the CONSUMING workflow expects:
                                                        // decodeBase64($content) consumers need a non-JSON type
                                                        // (connector base64-wraps), json(contentData) consumers
                                                        // need application/json. Wrong pick = "decodeBase64
                                                        // expects string, got Null" in the consumer.
                                                        let dir3 = dir2.clone();
                                                        let qn5  = qn4.clone();
                                                        let encoding = tokio::task::spawn_blocking(move || {
                                                            crate::services::sb_testing::queue_encoding(&dir3, &qn5)
                                                        }).await.unwrap_or(
                                                            crate::services::sb_testing::QueueEncoding::RawJson { consumer: None }
                                                        );
                                                        let mut sent = 0usize;
                                                        let mut err: Option<String> = None;
                                                        for _ in 0..count {
                                                            match crate::services::sb_amqp::send_amqp_message_with_type(
                                                                "localhost", &qn4, &body, encoding.content_type(),
                                                            ).await {
                                                                Ok(()) => sent += 1,
                                                                Err(e) => { err = Some(e); break; }
                                                            }
                                                        }
                                                        match err {
                                                            None => {
                                                                let mut msg = if sent == 1 {
                                                                    format!("✅ Sent to {} as {}", qn4, encoding.describe())
                                                                } else {
                                                                    format!("✅ Sent {}× to {} as {}", sent, qn4, encoding.describe())
                                                                };
                                                                if normalised.stripped_envelope {
                                                                    msg.push_str(" — auto-unwrapped contentData envelope");
                                                                }
                                                                status.set(Some((msg, false)));
                                                            }
                                                            Some(e) => status.set(Some((
                                                                format!("Sent {sent}/{count} then failed: {e}"), true,
                                                            ))),
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
                                        for (i, row) in msgs.iter().enumerate() {
                                            {
                                                match row {
                                                    PeekRow::Err(e) => rsx! {
                                                        div { class: "db-peek-msg",
                                                            span { class: "db-peek-idx", "#{i}" }
                                                            pre { class: "db-peek-body", "{e}" }
                                                        }
                                                    },
                                                    PeekRow::Msg(m) => {
                                                        // Surface the AMQP delivery-count so the user
                                                        // can spot poison-loop messages (workflow keeps
                                                        // failing → broker keeps redelivering) without
                                                        // having to leave the queue browser.
                                                        let dc = m.delivery_count;
                                                        let chip_cls = if dc >= 5      { "db-dc-chip db-dc-poison" }
                                                                       else if dc >= 1 { "db-dc-chip db-dc-warn" }
                                                                       else            { "db-dc-chip" };
                                                        let label = if dc == 0 {
                                                            "first".to_string()
                                                        } else {
                                                            format!("×{}", dc + 1)
                                                        };
                                                        let title = if dc >= 5 {
                                                            "Poison-loop signature — workflow has abandoned this message many times; it will dead-letter on the next abandon."
                                                        } else if dc >= 1 {
                                                            "Message has been re-delivered — workflow either abandoned or timed out previously."
                                                        } else {
                                                            "First delivery attempt."
                                                        };
                                                        let body = &m.body;
                                                        // If the message carries an Adaptive Card (Teams
                                                        // notifications), render a readable preview so the
                                                        // user doesn't have to mentally render card JSON.
                                                        let card = crate::services::sb_testing::adaptive_card_preview(body);
                                                        rsx! {
                                                            div { class: "db-peek-msg",
                                                                span { class: "db-peek-idx", "#{i}" }
                                                                span { class: "{chip_cls}", title: "{title}", "delivery {label}" }
                                                                if let Some(ref c) = card {
                                                                    {
                                                                        let accent_color = match c.accent.as_str() {
                                                                            "Attention" => "#d13438", // red — critical
                                                                            "Warning"   => "#c19c00", // yellow — validation
                                                                            "Good"      => "#107c10", // green — success
                                                                            _            => "#8a8886",
                                                                        };
                                                                        let border = format!("border-left:4px solid {accent_color};padding:6px 10px;margin:4px 0;background:rgba(128,128,128,0.08);border-radius:3px;");
                                                                        rsx! {
                                                                            div { style: "{border}",
                                                                                div { style: "font-size:10px;opacity:0.7;margin-bottom:2px",
                                                                                    "🃏 Adaptive Card · {c.accent}"
                                                                                }
                                                                                for (li, line) in c.lines.iter().enumerate() {
                                                                                    div {
                                                                                        style: if li == 0 { "font-weight:600" } else { "" },
                                                                                        "{line}"
                                                                                    }
                                                                                }
                                                                                for fact in c.facts.iter() {
                                                                                    div { style: "font-size:11px;opacity:0.85", "• {fact}" }
                                                                                }
                                                                                if !c.actions.is_empty() {
                                                                                    div { style: "margin-top:4px;display:flex;gap:6px",
                                                                                        for a in c.actions.iter() {
                                                                                            span {
                                                                                                style: "border:1px solid {accent_color};border-radius:3px;padding:1px 8px;font-size:11px",
                                                                                                "{a}"
                                                                                            }
                                                                                        }
                                                                                    }
                                                                                }
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                                pre { class: "db-peek-body", "{body}" }
                                                            }
                                                        }
                                                    }
                                                }
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
                                                    Ok(msgs) => {
                                                        let rows: Vec<PeekRow> = msgs.into_iter().map(PeekRow::Msg).collect();
                                                        sb_peek_msgs.write().insert(qn2.clone(), rows);
                                                    }
                                                    Err(e)   => { sb_peek_msgs.write().insert(qn2.clone(), vec![PeekRow::Err(format!("⚠ {e}"))]); }
                                                }
                                                sb_peeking.write().remove(&qn2);
                                            });
                                        },
                                        "🔄 Refresh"
                                    }

                                    // ── Expectation check: assert queue contents without
                                    //    manually eyeballing peeked JSON. Empty path = count all.
                                    {
                                        let qn_a  = queue_name6.clone();
                                        let qn_b  = queue_name6.clone();
                                        let qn_c  = queue_name6.clone();
                                        let qn_d  = queue_name6.clone();
                                        let (a_path, a_value, a_count) = assert_inputs.clone();
                                        rsx! {
                                            div { style: "display:flex;gap:6px;margin-top:8px;align-items:center",
                                                input {
                                                    class: "db-field-input",
                                                    style: "flex:2",
                                                    placeholder: "JSON path (e.g. ais.workflow.error.cp) — empty = any",
                                                    value: "{a_path}",
                                                    oninput: move |e| {
                                                        let mut w = sb_assert_inputs.write();
                                                        let entry = w.entry(qn_a.clone()).or_default();
                                                        entry.0 = e.value();
                                                    },
                                                }
                                                input {
                                                    class: "db-field-input",
                                                    style: "flex:1",
                                                    placeholder: "= value",
                                                    value: "{a_value}",
                                                    oninput: move |e| {
                                                        let mut w = sb_assert_inputs.write();
                                                        let entry = w.entry(qn_b.clone()).or_default();
                                                        entry.1 = e.value();
                                                    },
                                                }
                                                input {
                                                    class: "db-field-input",
                                                    style: "width:60px",
                                                    r#type: "number",
                                                    min: "1",
                                                    placeholder: ">=1",
                                                    title: "Minimum number of matching messages expected",
                                                    value: "{a_count}",
                                                    oninput: move |e| {
                                                        let mut w = sb_assert_inputs.write();
                                                        let entry = w.entry(qn_c.clone()).or_default();
                                                        entry.2 = e.value();
                                                    },
                                                }
                                                button {
                                                    class: "btn btn-small",
                                                    title: "Peek the queue and check the expectation (read-only)",
                                                    onclick: move |_| {
                                                        let qn = qn_d.clone();
                                                        let (path, value, count) = sb_assert_inputs.read()
                                                            .get(&qn).cloned().unwrap_or_default();
                                                        let min: usize = count.trim().parse().unwrap_or(1).max(1);
                                                        spawn(async move {
                                                            let result = crate::services::sb_testing::check_expectation(
                                                                "localhost", &qn, path.trim(), value.trim(), min,
                                                            ).await;
                                                            let entry = match result {
                                                                Ok(r)  => (r.passed, r.detail),
                                                                Err(e) => (false, format!("check failed: {e}")),
                                                            };
                                                            sb_assert_results.write().insert(qn, entry);
                                                        });
                                                    },
                                                    "✓ Check"
                                                }
                                            }
                                            if let Some((passed, detail)) = assert_result.clone() {
                                                div {
                                                    style: if passed {
                                                        "color:#107c10;font-size:12px;margin-top:4px"
                                                    } else {
                                                        "color:#d13438;font-size:12px;margin-top:4px"
                                                    },
                                                    if passed { "✅ PASS — {detail}" } else { "❌ FAIL — {detail}" }
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
                          let la_dir2 = props.logic_apps_dir.clone();
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
                                                let dir = la_dir2.clone();
                                                let raw = sb_send_bodies.read().get(&qname).cloned().unwrap_or_default();
                                                let normalised = crate::services::payload::normalise_send_body(&raw);
                                                spawn(async move {
                                                    let emulator_up = tokio::net::TcpStream::connect("127.0.0.1:5672").await.is_ok();
                                                    let result = if emulator_up {
                                                        // Even emulator-only queues may have a consumer whose
                                                        // workflow was added after the queue — detect encoding.
                                                        let qn4 = qn3.clone();
                                                        let encoding = tokio::task::spawn_blocking(move || {
                                                            crate::services::sb_testing::queue_encoding(&dir, &qn4)
                                                        }).await.unwrap_or(
                                                            crate::services::sb_testing::QueueEncoding::RawJson { consumer: None }
                                                        );
                                                        crate::services::sb_amqp::send_amqp_message_with_type(
                                                            "localhost", &qn3, &normalised.body, encoding.content_type(),
                                                        ).await
                                                    } else {
                                                        Err("SB Emulator is not running — start it from the toolbar first.".into())
                                                    };
                                                    match result {
                                                        Ok(()) => status.set(Some((
                                                            if normalised.stripped_envelope {
                                                                format!("✅ Sent to {} — auto-unwrapped contentData envelope (the SB trigger adds one)", qn3)
                                                            } else {
                                                                format!("✅ Sent to {}", qn3)
                                                            }, false))),
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
