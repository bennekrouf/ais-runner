use dioxus::prelude::*;
use indexmap::IndexMap;
use crate::services::{azure_cli, config, settings_file};
use crate::components::env_compare_panel::EnvComparePanel;
use crate::components::eventgrid_panel::EventGridPanel;
use crate::components::sb_compare_panel::SbComparePanel;

const DEFAULT_SUBSCRIPTION: &str = "b4c0de7e-1fe0-4d3b-90c7-e3e9c9e4b9db";

// ── Helpers ────────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq)]
enum FetchKind { ServiceBus }

fn fetch_kind(key: &str) -> Option<FetchKind> {
    let k = key.to_lowercase();
    if k.contains("servicebus") && k.contains("connection") { Some(FetchKind::ServiceBus) }
    else { None }
}

fn is_placeholder(val: &str) -> bool {
    let v = val.trim();
    v.is_empty()
        || v.starts_with('<')
        || v.to_lowercase().contains("placeholder")
}

fn is_secret(key: &str) -> bool {
    let k = key.to_lowercase();
    k.contains("secret") || k.contains("password") || k.contains("_key")
        || k.contains("connectionstring") || k.contains("sharedaccesskey") || k.contains("apikey")
}

fn category(key: &str) -> &'static str {
    let k = key.to_lowercase();
    if k.contains("servicebus")                         { return "Service Bus"; }
    if k.contains("azurewebjobs") || k.contains("storage") { return "Storage"; }
    if k.contains("azurefunction") || k.contains("triggerurl") { return "Function URLs"; }
    if k.contains("tenant") || k.contains("clientid") || k.contains("clientsecret") { return "Auth"; }
    if k.starts_with("workflows_")                     { return "Workflows Runtime"; }
    "Other"
}

// ── Props ──────────────────────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
pub struct SettingsEditorProps {
    pub logic_apps_dir: String,
}

// ── Component ──────────────────────────────────────────────────────────────

#[component]
pub fn SettingsEditor(props: SettingsEditorProps) -> Element {
    let logic_apps_dir = props.logic_apps_dir.clone();
    let app_cfg = config::load();

    let mut active_tab = use_signal(|| "edit");
    let mut pairs      = use_signal(|| IndexMap::<String, String>::new());
    let mut status     = use_signal(|| String::new());
    let mut is_err     = use_signal(|| false);
    let mut az_expired = use_signal(|| false);
    let mut filter     = use_signal(|| String::new());
    let mut fetching   = use_signal(|| String::new());
    let mut show_keys  = use_signal(|| std::collections::HashSet::<String>::new());
    let link = app_cfg.get_link(&props.logic_apps_dir);
    let mut tenant_id          = use_signal(|| link.and_then(|l| l.tenant_id.clone()).unwrap_or_default());
    let mut tenant_detecting   = use_signal(|| false);
    let mut sb_namespace  = use_signal(|| link.and_then(|l| l.sb_namespace.clone()).unwrap_or_default());
    let mut subscription  = use_signal(|| link.map(|l| l.subscription_id.clone()).unwrap_or_else(|| DEFAULT_SUBSCRIPTION.to_string()));
    let mut ns_options    = use_signal(|| Vec::<String>::new());
    let mut ns_loading    = use_signal(|| false);
    let mut sub_options   = use_signal(|| Vec::<(String, String)>::new());
    let mut sub_loading   = use_signal(|| false);

    // load on mount
    use_effect({
        let logic_apps_dir = logic_apps_dir.clone();
        move || {
            match settings_file::read_local_settings(&logic_apps_dir) {
                Ok(text) if !text.is_empty() => {
                    if let Ok(serde_json::Value::Object(root)) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Some(serde_json::Value::Object(vals)) = root.get("Values") {
                            pairs.set(vals.iter()
                                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                                .collect());
                        }
                    }
                }
                Err(e) => { status.set(e); is_err.set(true); }
                _ => {}
            }
        }
    });

    let on_save = {
        let logic_apps_dir = logic_apps_dir.clone();
        move |_| {
            let vals: serde_json::Map<_, _> = pairs.read().iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect();
            let text = serde_json::to_string_pretty(&serde_json::json!({
                "IsEncrypted": false,
                "Values": serde_json::Value::Object(vals)
            })).unwrap_or_default();
            match settings_file::write_local_settings(&logic_apps_dir, &text) {
                Ok(_) => { status.set("Saved — restart func start to apply changes.".to_string()); is_err.set(false); }
                Err(e) => { status.set(e); is_err.set(true); }
            }
        }
    };

    // group keys by category
    let query = filter.read().to_lowercase();
    let grouped: Vec<(&'static str, Vec<String>)> = {
        let mut cats: IndexMap<&'static str, Vec<String>> = IndexMap::new();
        for key in pairs.read().keys() {
            if query.is_empty() || key.to_lowercase().contains(&query) {
                cats.entry(category(key)).or_default().push(key.clone());
            }
        }
        cats.into_iter().collect()
    };

    let tab = active_tab();

    rsx! {
        div { id: "settings-panel",

            // ── Tab bar ──────────────────────────────────────────────────
            div { class: "settings-tabs",
                button {
                    class: if tab == "edit" { "settings-tab active" } else { "settings-tab" },
                    onclick: move |_| active_tab.set("edit"),
                    "⚙ Local Settings"
                }
                button {
                    class: if tab == "compare" { "settings-tab active" } else { "settings-tab" },
                    onclick: move |_| active_tab.set("compare"),
                    "⊞ Compare Environments"
                }
                button {
                    class: if tab == "eventgrid" { "settings-tab active" } else { "settings-tab" },
                    onclick: move |_| active_tab.set("eventgrid"),
                    "⚡ Event Grid"
                }
                button {
                    class: if tab == "servicebus" { "settings-tab active" } else { "settings-tab" },
                    onclick: move |_| active_tab.set("servicebus"),
                    "🚌 Service Bus"
                }
            }

            if tab == "compare" {
                EnvComparePanel {
                    logic_apps_dir: props.logic_apps_dir.clone(),
                    local_pairs:    pairs,
                }
            } else if tab == "eventgrid" {
                EventGridPanel {
                    logic_apps_dir: props.logic_apps_dir.clone(),
                }
            } else if tab == "servicebus" {
                SbComparePanel {
                    logic_apps_dir: props.logic_apps_dir.clone(),
                }
            } else {

            // ── Header ──────────────────────────────────────────────────
            div { class: "settings-header",
                h2 { "local.settings.json" }
                div { style: "display:flex;gap:8px;align-items:center",
                    if *az_expired.read() {
                        button {
                            class: "btn btn-warn btn-small",
                            onclick: move |_| {
                                let t = tenant_id.read().clone();
                                azure_cli::open_login(if t.is_empty() { None } else { Some(t.as_str()) });
                                az_expired.set(false);
                                status.set("az login opened — re-run fetch after signing in.".to_string());
                                is_err.set(false);
                            },
                            "⚠ az login"
                        }
                    }
                    button {
                        class: "btn btn-run btn-small",
                        onclick: on_save,
                        "💾 Save"
                    }
                    input {
                        id: "settings-filter",
                        placeholder: "Filter…",
                        value: "{filter}",
                        oninput: move |e| filter.set(e.value()),
                    }
                }
            }

            // ── Status bar ──────────────────────────────────────────────
            if !status.read().is_empty() {
                div {
                    class: if *is_err.read() { "settings-status error" } else { "settings-status ok" },
                    "{status}"
                }
            }

            // ── Azure config ────────────────────────────────────────────
            div {
                class: "settings-azure-cfg",
                tabindex: -1,
                onkeydown: move |e: KeyboardEvent| {
                    if e.key() == Key::Escape {
                        sub_options.set(vec![]);
                        ns_options.set(vec![]);
                    }
                },
                label { class: "settings-cfg-label", "Tenant ID" }
                div { class: "settings-cfg-ns-wrap",
                    input {
                        class: "settings-cfg-val",
                        placeholder: "e.g. 00000000-0000-0000-0000-000000000000  (leave blank for default)",
                        value: "{tenant_id}",
                        oninput: {
                            let dir = logic_apps_dir.clone();
                            move |e: FormEvent| {
                                let v = e.value();
                                tenant_id.set(v.clone());
                                let mut cfg = config::load();
                                if let Some(l) = cfg.workspace_links.get_mut(&dir) {
                                    l.tenant_id = if v.is_empty() { None } else { Some(v) };
                                    config::save(&cfg);
                                }
                            }
                        },
                    }
                    button {
                        class: "btn btn-small",
                        disabled: *tenant_detecting.read(),
                        title: "Detect tenant from active az login",
                        onclick: {
                            let dir = logic_apps_dir.clone();
                            move |_| {
                                let dir2 = dir.clone();
                                tenant_detecting.set(true);
                                spawn(async move {
                                    let result = tokio::task::spawn_blocking(
                                        azure_cli::get_active_tenant
                                    ).await.ok().and_then(|r| r.ok());
                                    if let Some(t) = result {
                                        tenant_id.set(t.clone());
                                        let mut cfg = config::load();
                                        if let Some(l) = cfg.workspace_links.get_mut(&dir2) {
                                            l.tenant_id = Some(t);
                                            config::save(&cfg);
                                        }
                                    }
                                    tenant_detecting.set(false);
                                });
                            }
                        },
                        if *tenant_detecting.read() { "…" } else { "Detect" }
                    }
                }

                label { class: "settings-cfg-label", "Subscription ID" }
                div { class: "settings-cfg-ns-wrap",
                    input {
                        class: "settings-cfg-val",
                        value: "{subscription}",
                        disabled: config::load().get_link(&logic_apps_dir).is_some(),
                        oninput: {
                            let dir = logic_apps_dir.clone();
                            move |e: FormEvent| {
                                let v = e.value();
                                subscription.set(v.clone());
                                sub_options.set(vec![]);
                                let mut cfg = config::load();
                                if let Some(l) = cfg.workspace_links.get_mut(&dir) {
                                    l.subscription_id = v;
                                }
                                config::save(&cfg);
                            }
                        },
                    }
                    {
                        if let Some(link) = config::load().get_link(&logic_apps_dir) {
                            let rg = link.resource_group.clone();
                            rsx! {
                                button {
                                    class: "btn btn-small btn-magic",
                                    style: "margin-left: 8px",
                                    title: "Auto-detect subscription from Resource Group",
                                    onclick: {
                                        let dir = logic_apps_dir.clone();
                                        let rg = rg.clone();
                                        move |_| {
                                            let rg = rg.clone();
                                            let dir = dir.clone();
                                            status.set("Detecting subscription...".to_string());
                                            is_err.set(false);
                                        spawn(async move {
                                            // Try primary RG name
                                            let mut res = tokio::task::spawn_blocking({
                                                let rg = rg.clone();
                                                move || azure_cli::get_subscription_id_by_group(&rg)
                                            }).await.unwrap_or(Err(azure_cli::AzError::Other("Task failed".to_string())));

                                            // Fallback: if it's 'rg-' but project uses 'logic-', try that
                                            if res.is_err() && rg.starts_with("rg-") {
                                                let fallback = rg.replace("rg-", "logic-");
                                                let res2 = tokio::task::spawn_blocking({
                                                    let f = fallback.clone();
                                                    move || azure_cli::get_subscription_id_by_group(&f)
                                                }).await.unwrap_or(Err(azure_cli::AzError::Other("Task failed".to_string())));
                                                if res2.is_ok() {
                                                    res = res2;
                                                }
                                            }

                                            match res {
                                                Ok(id) => {
                                                    subscription.set(id.clone());
                                                    status.set(format!("✅ Found subscription"));
                                                    is_err.set(false);
                                                    let mut cfg = config::load();
                                                    if let Some(l) = cfg.workspace_links.get_mut(&dir) {
                                                        l.subscription_id = id.clone();
                                                    }
                                                    config::save(&cfg);
                                                }
                                                Err(azure_cli::AzError::NotLoggedIn) => {
                                                    az_expired.set(true);
                                                    status.set("Session expired — click 'az login'".to_string());
                                                    is_err.set(true);
                                                }
                                                Err(e) => {
                                                    status.set(format!("Error: {:?}", e));
                                                    is_err.set(true);
                                                }
                                            }
                                        });
                                    }
                                },
                                    "✨ Auto-detect"
                                }
                            }
                        } else {
                            rsx! {
                                button {
                                    class: "btn btn-small btn-fetch",
                                    disabled: *sub_loading.read(),
                                    title: "List subscriptions",
                                    onclick: move |_| {
                                        sub_loading.set(true);
                                        sub_options.set(vec![]);
                                        spawn(async move {
                                            let result = tokio::task::spawn_blocking(|| {
                                                azure_cli::list_subscriptions()
                                            }).await.unwrap_or(Err(azure_cli::AzError::Other("Task failed".to_string())));
                                            sub_loading.set(false);
                                            match result {
                                                Ok(list) => {
                                                    if list.is_empty() {
                                                        status.set("No subscriptions found.".to_string());
                                                        is_err.set(false);
                                                    } else {
                                                        sub_options.set(list);
                                                    }
                                                }
                                                Err(azure_cli::AzError::NotLoggedIn) => {
                                                    az_expired.set(true);
                                                    status.set("Session expired — click 'az login'".to_string());
                                                    is_err.set(true);
                                                }
                                                Err(azure_cli::AzError::Other(e)) => {
                                                    status.set(e);
                                                    is_err.set(true);
                                                }
                                            }
                                        });
                                    },
                                    if *sub_loading.read() { "…" } else { "⊕ Browse" }
                                }
                            }
                        }
                    }
                }

                // ── Resource Group (Read-Only) ───────────────────────────────
                {
                    config::load().get_link(&logic_apps_dir).map(|link| {
                        let rg_display = link.sb_namespace.as_ref().unwrap_or(&link.resource_group);
                        rsx! {
                            label { class: "settings-cfg-label", style: "margin-top: 10px", "Azure Context" }
                            div { class: "settings-cfg-ns-wrap",
                                span { class: "settings-link-badge", "🔗 {rg_display}" }
                            }
                        }
                    })
                }

                if !sub_options.read().is_empty() {
                    div { class: "settings-cfg-spacer" }
                    div { class: "ns-dropdown",
                        for (id, name) in sub_options.read().clone() {
                            {
                                let id2 = id.clone();
                                rsx! {
                                    div {
                                        class: if *subscription.read() == id { "ns-option selected" } else { "ns-option" },
                                        onclick: {
                                            let dir = logic_apps_dir.clone();
                                            let id2 = id2.clone();
                                            move |_| {
                                                subscription.set(id2.clone());
                                                sub_options.set(vec![]);
                                                let mut cfg = config::load();
                                                if let Some(l) = cfg.workspace_links.get_mut(&dir) {
                                                    l.subscription_id = id2.clone();
                                                }
                                                config::save(&cfg);
                                            }
                                        },
                                        span { style: "color:var(--text2);margin-right:8px", "{name}" }
                                        span { style: "color:var(--text3);font-size:11px", "{id}" }
                                    }
                                }
                            }
                        }
                    }
                }
                label { class: "settings-cfg-label", "Service Bus Namespace" }
                div { class: "settings-cfg-ns-wrap",
                    input {
                        class: "settings-cfg-val",
                        placeholder: "e.g. logic-tom-dev-chn-001",
                        value: "{sb_namespace}",
                        oninput: {
                            let dir = logic_apps_dir.clone();
                            move |e: FormEvent| {
                                let v = e.value();
                                sb_namespace.set(v.clone());
                                ns_options.set(vec![]);
                                let mut cfg = config::load();
                                if let Some(l) = cfg.workspace_links.get_mut(&dir) {
                                    l.sb_namespace = if v.is_empty() { None } else { Some(v) };
                                }
                                config::save(&cfg);
                            }
                        },
                    }
                    button {
                        class: "btn btn-small btn-fetch",
                        disabled: *ns_loading.read(),
                        title: "List namespaces in subscription",
                        onclick: move |_| {
                            let sub = subscription.read().clone();
                            ns_loading.set(true);
                            ns_options.set(vec![]);
                            spawn(async move {
                                let result = tokio::task::spawn_blocking(move || {
                                    azure_cli::list_all_servicebus_namespaces(&sub)
                                }).await.unwrap_or(Err(azure_cli::AzError::Other("Task failed".to_string())));
                                ns_loading.set(false);
                                match result {
                                    Ok(list) => {
                                        if list.is_empty() {
                                            status.set("No Service Bus namespaces found in subscription.".to_string());
                                            is_err.set(false);
                                        } else {
                                            ns_options.set(list);
                                        }
                                    }
                                    Err(azure_cli::AzError::NotLoggedIn) => {
                                        az_expired.set(true);
                                        status.set("Session expired — click 'az login'".to_string());
                                        is_err.set(true);
                                    }
                                    Err(azure_cli::AzError::Other(e)) => {
                                        status.set(e);
                                        is_err.set(true);
                                    }
                                }
                            });
                        },
                        if *ns_loading.read() { "…" } else { "⊕ Browse" }
                    }
                }

                // dropdown of discovered namespaces
                if !ns_options.read().is_empty() {
                    div { class: "settings-cfg-spacer" }
                    div { class: "settings-cfg-spacer" }
                    div { class: "ns-dropdown",
                        for ns in ns_options.read().clone() {
                            {
                                let ns2 = ns.clone();
                                rsx! {
                                    div {
                                        class: if *sb_namespace.read() == ns { "ns-option selected" } else { "ns-option" },
                                        onclick: {
                                            let dir = logic_apps_dir.clone();
                                            let ns2 = ns2.clone();
                                            move |_| {
                                                sb_namespace.set(ns2.clone());
                                                ns_options.set(vec![]);
                                                let mut cfg = config::load();
                                                if let Some(l) = cfg.workspace_links.get_mut(&dir) {
                                                    l.sb_namespace = Some(ns2.clone());
                                                }
                                                config::save(&cfg);
                                            }
                                        },
                                        "{ns}"
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // ── Key-value table ─────────────────────────────────────────
            div { id: "settings-scroll",
                for (cat, keys) in grouped.iter() {
                    div { class: "settings-group",
                        div { class: "settings-group-label", "{cat}" }

                        for key in keys.iter() {
                            {
                                let key       = key.clone();
                                let key_tog   = key.clone();
                                let key_del   = key.clone();
                                let value     = pairs.read().get(&key).cloned().unwrap_or_default();
                                let secret    = is_secret(&key);
                                let visible   = show_keys.read().contains(&key);
                                let fkind     = fetch_kind(&key);
                                let busy      = *fetching.read() == key;
                                let ph        = is_placeholder(&value);

                                rsx! {
                                    div { class: if ph { "settings-row settings-row-placeholder" } else { "settings-row" },
                                        span { class: "settings-key", title: "{key}", "{key}" }
                                        div { class: "settings-val-wrap",

                                            input {
                                                class: "settings-val",
                                                r#type: if secret && !visible { "password" } else { "text" },
                                                value: "{value}",
                                                oninput: {
                                                    let key = key.clone();
                                                    move |e: Event<FormData>| {
                                                        pairs.write().insert(key.clone(), e.value());
                                                    }
                                                },
                                            }

                                            if secret {
                                                button {
                                                    class: "btn-icon",
                                                    title: if visible { "Hide" } else { "Show" },
                                                    onclick: move |_| {
                                                        let mut s = show_keys.write();
                                                        if s.contains(&key_tog) { s.remove(&key_tog); }
                                                        else { s.insert(key_tog.clone()); }
                                                    },
                                                    if visible { "🙈" } else { "👁" }
                                                }
                                            }

                                            if fkind.is_some() {
                                                button {
                                                    class: "btn btn-small btn-fetch",
                                                    disabled: busy,
                                                    title: "Fetch from Azure",
                                                    onclick: {
                                                        let key = key.clone();
                                                        move |_| {
                                                            fetching.set(key.clone());
                                                            let key2   = key.clone();
                                                            // extract plain data before spawn_blocking (Signal is not Send)
                                                            let ns_val  = sb_namespace.read().clone();
                                                            let sub_val = subscription.read().clone();
                                                            spawn(async move {
                                                                let result: Result<String, azure_cli::AzError> =
                                                                    tokio::task::spawn_blocking(move || {
                                                                        if ns_val.is_empty() {
                                                                            return Err(azure_cli::AzError::Other(
                                                                                "Service Bus namespace is not set — fill in the Namespace field above".to_string()
                                                                            ));
                                                                        }
                                                                        let rg = azure_cli::find_servicebus_rg(&sub_val, &ns_val)?;
                                                                        azure_cli::fetch_servicebus_connection_string(&sub_val, &rg, &ns_val)
                                                                    }).await.unwrap_or_else(|_| Err(azure_cli::AzError::Other("Task failed".to_string())));

                                                                fetching.set(String::new());
                                                                match result {
                                                                    Ok(val) => {
                                                                        pairs.write().insert(key2.clone(), val);
                                                                        status.set(format!("{} fetched — click Save", key2));
                                                                        is_err.set(false);
                                                                        az_expired.set(false);
                                                                    }
                                                                    Err(azure_cli::AzError::NotLoggedIn) => {
                                                                        az_expired.set(true);
                                                                        status.set("Session expired — click 'az login'".to_string());
                                                                        is_err.set(true);
                                                                    }
                                                                    Err(azure_cli::AzError::Other(e)) => {
                                                                        status.set(e);
                                                                        is_err.set(true);
                                                                    }
                                                                }
                                                            });
                                                        }
                                                    },
                                                    if busy { "…" } else { "↓ Azure" }
                                                }
                                            }

                                            button {
                                                class: "btn-icon btn-icon-del",
                                                title: "Remove key",
                                                onclick: move |_| { pairs.write().shift_remove(&key_del); },
                                                "×"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                AddKeyRow { pairs }
            }
            } // end else (edit tab)
        }
    }
}

// ── Add-key row ────────────────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct AddKeyRowProps {
    pairs: Signal<IndexMap<String, String>>,
}

#[component]
fn AddKeyRow(mut props: AddKeyRowProps) -> Element {
    let mut nk = use_signal(|| String::new());
    let mut nv = use_signal(|| String::new());

    rsx! {
        div { class: "settings-add-row",
            input {
                class: "settings-add-key",
                placeholder: "New key…",
                value: "{nk}",
                oninput: move |e| nk.set(e.value()),
            }
            input {
                class: "settings-add-val",
                placeholder: "Value…",
                value: "{nv}",
                oninput: move |e| nv.set(e.value()),
            }
            button {
                class: "btn btn-small btn-start",
                disabled: nk.read().is_empty(),
                onclick: move |_| {
                    let k = nk.read().trim().to_string();
                    let v = nv.read().trim().to_string();
                    if !k.is_empty() {
                        props.pairs.write().insert(k, v);
                        nk.set(String::new());
                        nv.set(String::new());
                    }
                },
                "+ Add"
            }
        }
    }
}
