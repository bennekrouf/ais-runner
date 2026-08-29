//! Compare-environments view for **Azure App Configuration** stores.
//!
//! Deliberately a peer of `EnvComparePanel` rather than a sub-section of it —
//! Application Settings (on the Function App) and App Configuration (the
//! standalone store) are different Azure products with different scoping
//! rules. Lumping them under a generic "environment variables" tab would
//! hide the semantics customers rely on (labels for per-env scoping, etc.).
//!
//! UI flow:
//! 1. Detect stores referenced by the project's `local.settings.json`
//!    (`@Microsoft.AppConfiguration(...)` references + bare endpoint settings).
//! 2. User picks a store.
//! 3. User picks one or more labels to fetch and compare side-by-side
//!    (`dev`, `qa`, `prod`, or `(none)`).
//! 4. Same diff-table rendering style as the App Settings panel — keys ×
//!    label columns, missing/different highlighted.

use dioxus::prelude::*;
use indexmap::IndexMap;
use std::collections::HashSet;

use crate::services::azure::app_config::{self, AppConfigRef, AppConfigValues};
use crate::services::azure::cli::AzError;

#[derive(Clone, PartialEq, Debug)]
enum FetchState {
    Idle,
    Loading,
    Done(AppConfigValues),
    Err(String),
}

impl FetchState {
    fn values(&self) -> Option<&AppConfigValues> {
        if let FetchState::Done(v) = self {
            Some(v)
        } else {
            None
        }
    }
    fn err(&self) -> Option<&str> {
        if let FetchState::Err(e) = self {
            Some(e)
        } else {
            None
        }
    }
    fn is_loading(&self) -> bool {
        matches!(self, FetchState::Loading)
    }
}

#[derive(Props, Clone, PartialEq)]
pub struct AppConfigPanelProps {
    /// `local.settings.json` Values, used to discover which App Configuration
    /// stores this project references.
    pub local_pairs: Signal<IndexMap<String, String>>,
}

#[component]
pub fn AppConfigPanel(props: AppConfigPanelProps) -> Element {
    // ── Discovered stores ──────────────────────────────────────────────────
    let local = props.local_pairs.read().clone();
    let discovered = app_config::detect_stores(&local);
    let endpoints = unique_endpoints(&discovered);

    // ── State: which store the user has selected ───────────────────────────
    let mut selected_endpoint: Signal<Option<String>> = use_signal(|| endpoints.first().cloned());

    // ── State: labels under comparison + their fetched values ──────────────
    // Order preserved so the column layout is stable across re-renders.
    let mut labels: Signal<Vec<String>> = use_signal(Vec::new);
    let mut fetches: Signal<IndexMap<String, FetchState>> = use_signal(IndexMap::new);
    let mut new_label: Signal<String> = use_signal(String::new);
    let mut filter: Signal<String> = use_signal(String::new);
    let mut only_diff: Signal<bool> = use_signal(|| false);

    // ── Actions ────────────────────────────────────────────────────────────
    let mut add_label = move |label: String| {
        let label = label.trim().to_string();
        let key = if label.is_empty() {
            "(none)".to_string()
        } else {
            label.clone()
        };
        if labels.read().contains(&key) {
            return;
        }
        labels.write().push(key.clone());
        fetches.write().insert(key.clone(), FetchState::Loading);

        let endpoint = selected_endpoint.read().clone();
        let Some(endpoint) = endpoint else {
            return;
        };
        let label_for_call = if label.is_empty() { None } else { Some(label) };
        spawn(async move {
            let endpoint2 = endpoint.clone();
            let label2 = label_for_call.clone();
            let res = tokio::task::spawn_blocking(move || {
                app_config::fetch_kv(&endpoint2, label2.as_deref())
            })
            .await
            .unwrap_or_else(|_| Err(AzError::Other("task panicked".into())));
            let state = match res {
                Ok(v) => FetchState::Done(v),
                Err(e) => FetchState::Err(format!("{:?}", e)),
            };
            fetches.write().insert(key, state);
        });
    };

    let mut remove_label = move |label: String| {
        labels.write().retain(|l| l != &label);
        fetches.write().shift_remove(&label);
    };

    // ── Empty state: no store referenced anywhere in the project ──────────
    if discovered.is_empty() {
        return rsx! {
            div { class: "appcfg-empty",
                h3 { "No App Configuration store referenced" }
                p {
                    "This project's "
                    code { "local.settings.json" }
                    " has no "
                    code { "@Microsoft.AppConfiguration(Endpoint=…)" }
                    " or "
                    code { "AzureAppConfigurationEndpoint" }
                    " entry. If the customer uses a centralized App Configuration store, \
                     add a reference to "
                    code { "local.settings.json" }
                    " and reopen this tab."
                }
                p { class: "appcfg-hint",
                    "Application Settings (the per-site key/value pairs) are compared on the "
                    strong { "Application Settings" } " tab."
                }
            }
        };
    }

    // ── Detected stores list + selector ───────────────────────────────────
    let stores_rsx = rsx! {
        div { class: "appcfg-stores",
            div { class: "appcfg-section-title", "Detected stores" }
            for ep in endpoints.iter() {
                {
                    let ep_clone = ep.clone();
                    let is_active = selected_endpoint.read().as_deref() == Some(ep.as_str());
                    let cls = if is_active { "appcfg-store active" } else { "appcfg-store" };
                    let store_name = discovered.iter()
                        .find(|r| r.endpoint == *ep)
                        .map(|r| r.store_name.clone())
                        .unwrap_or_else(|| ep.clone());
                    // List every setting/key/label triple that brought this
                    // store into scope so the user understands the chain.
                    let refs: Vec<&AppConfigRef> = discovered.iter().filter(|r| r.endpoint == *ep).collect();
                    rsx! {
                        button {
                            class: "{cls}",
                            onclick: move |_| {
                                if selected_endpoint.read().as_deref() != Some(ep_clone.as_str()) {
                                    selected_endpoint.set(Some(ep_clone.clone()));
                                    labels.write().clear();
                                    fetches.write().clear();
                                }
                            },
                            div { class: "appcfg-store-name", "🗂 {store_name}" }
                            div { class: "appcfg-store-endpoint", "{ep}" }
                            div { class: "appcfg-store-refs",
                                for r in refs.iter() {
                                    {
                                        let key_part = match (&r.key, &r.label) {
                                            (Some(k), Some(l)) => format!("{} / Key={} / Label={}", r.setting_key, k, l),
                                            (Some(k), None)    => format!("{} / Key={}", r.setting_key, k),
                                            (None, Some(l))    => format!("{} / Label={}", r.setting_key, l),
                                            (None, None)       => r.setting_key.clone(),
                                        };
                                        rsx! { div { class: "appcfg-ref", "↳ {key_part}" } }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    // ── Label picker + compare table ──────────────────────────────────────
    let snap_labels = labels.read().clone();
    let snap_fetches = fetches.read().clone();
    let q = filter.read().to_lowercase();

    // Collect every distinct key across the loaded label columns.
    let mut all_keys: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for label in snap_labels.iter() {
        if let Some(FetchState::Done(v)) = snap_fetches.get(label) {
            for k in v.keys() {
                if !q.is_empty() && !k.to_lowercase().contains(&q) {
                    continue;
                }
                if seen.insert(k.clone()) {
                    all_keys.push(k.clone());
                }
            }
        }
    }
    let diff_only = *only_diff.read();
    if diff_only {
        all_keys.retain(|k| {
            // A key counts as "different" if any pair of labels has differing
            // values. Missing-in-one-env also counts (effectively `None` vs Some).
            let mut last: Option<Option<&String>> = None;
            for label in snap_labels.iter() {
                let here = snap_fetches
                    .get(label)
                    .and_then(|s| s.values())
                    .and_then(|v| v.get(k));
                match last {
                    None => last = Some(here),
                    Some(prev) if prev == here => continue,
                    _ => return true,
                }
            }
            false
        });
    }

    let selected_endpoint_str = selected_endpoint.read().clone().unwrap_or_default();

    rsx! {
        div { class: "appcfg-panel",
            { stores_rsx }

            if !selected_endpoint_str.is_empty() {
                div { class: "appcfg-section-title appcfg-section-title-spaced",
                    "Compare labels — " span { class: "appcfg-endpoint-inline", "{selected_endpoint_str}" }
                }

                // Label add row
                div { class: "appcfg-add-row",
                    input {
                        class: "appcfg-label-input",
                        placeholder: "label (e.g. dev, qa, prod — leave blank for no-label)",
                        value: "{new_label.read()}",
                        oninput: move |e| new_label.set(e.value()),
                    }
                    button {
                        class: "btn btn-small",
                        onclick: move |_| {
                            let v = new_label.read().clone();
                            new_label.set(String::new());
                            add_label(v);
                        },
                        "+ Add label"
                    }
                    label { class: "appcfg-only-diff",
                        input {
                            r#type: "checkbox",
                            checked: diff_only,
                            onchange: move |e| only_diff.set(e.value() == "true"),
                        }
                        " Differences only"
                    }
                    input {
                        class: "appcfg-filter",
                        placeholder: "filter keys…",
                        value: "{filter.read()}",
                        oninput: move |e| filter.set(e.value()),
                    }
                }

                // Active label chips with remove + retry
                div { class: "appcfg-label-row",
                    for label in snap_labels.iter() {
                        {
                            let l = label.clone();
                            let l_remove = l.clone();
                            let state = snap_fetches.get(label).cloned()
                                .unwrap_or(FetchState::Idle);
                            let status_icon = if state.is_loading() { "⟳" }
                                              else if state.err().is_some() { "✕" }
                                              else { "•" };
                            rsx! {
                                div { class: "appcfg-label-chip",
                                    span { class: "appcfg-label-status", "{status_icon}" }
                                    span { class: "appcfg-label-name",   "{l}" }
                                    button {
                                        class: "appcfg-label-remove",
                                        title: "Remove this label column",
                                        onclick: move |_| remove_label(l_remove.clone()),
                                        "×"
                                    }
                                    if let Some(e) = state.err() {
                                        div { class: "appcfg-label-err", title: "{e}", "error" }
                                    }
                                }
                            }
                        }
                    }
                }

                // Comparison table
                if !snap_labels.is_empty() {
                    div { class: "appcfg-table-wrap",
                        table { class: "appcfg-table",
                            thead {
                                tr {
                                    th { class: "appcfg-th-key", "Key" }
                                    for label in snap_labels.iter() {
                                        th { class: "appcfg-th-col", "{label}" }
                                    }
                                }
                            }
                            tbody {
                                for key in all_keys.iter() {
                                    {
                                        let k = key.clone();
                                        let row_values: Vec<Option<String>> = snap_labels.iter().map(|l|
                                            snap_fetches.get(l)
                                                .and_then(|s| s.values())
                                                .and_then(|v| v.get(&k).cloned())
                                        ).collect();
                                        // Highlight cells whose value differs from the first column.
                                        let baseline = row_values.first().cloned().flatten();
                                        rsx! {
                                            tr {
                                                td { class: "appcfg-td-key", "{k}" }
                                                for (i, v) in row_values.iter().enumerate() {
                                                    {
                                                        let cell_cls = match v {
                                                            None => "appcfg-td appcfg-td-missing",
                                                            Some(val) if i > 0 && Some(val) != baseline.as_ref() => "appcfg-td appcfg-td-diff",
                                                            Some(_) => "appcfg-td",
                                                        };
                                                        let display = v.clone().unwrap_or_else(|| "—".to_string());
                                                        rsx! { td { class: "{cell_cls}", "{display}" } }
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
        }
    }
}

fn unique_endpoints(refs: &[AppConfigRef]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for r in refs {
        if seen.insert(r.endpoint.clone()) {
            out.push(r.endpoint.clone());
        }
    }
    out
}
