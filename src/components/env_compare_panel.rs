use crate::components::app_config_panel::AppConfigPanel;
use crate::components::security_compare_panel::SecurityComparePanel;
use crate::services::config;
use ais_core::cli as azure_cli;
use ais_core::env_compare::{self, EnvValues, VarGroup};
use dioxus::prelude::*;
use indexmap::IndexMap;
use std::collections::HashSet;

// ── Fetch state ───────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, Debug)]
enum FetchState {
    Idle,
    Loading,
    Done(EnvValues),
    Err(String),
}

impl FetchState {
    fn values(&self) -> Option<&EnvValues> {
        if let FetchState::Done(v) = self {
            Some(v)
        } else {
            None
        }
    }
    fn is_loading(&self) -> bool {
        matches!(self, FetchState::Loading)
    }
    fn err(&self) -> Option<&str> {
        if let FetchState::Err(e) = self {
            Some(e)
        } else {
            None
        }
    }
}

// ── Props ─────────────────────────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
pub struct EnvComparePanelProps {
    pub logic_apps_dir: String,
    pub local_pairs: Signal<IndexMap<String, String>>,
}

// ── Component ─────────────────────────────────────────────────────────────────

#[component]
pub fn EnvComparePanel(props: EnvComparePanelProps) -> Element {
    let dir = props.logic_apps_dir.clone();

    let app_cfg = config::load();
    let link = app_cfg.get_link(&props.logic_apps_dir).cloned();
    let sub_def = link
        .as_ref()
        .map(|l| l.subscription_id.clone())
        .unwrap_or_default();
    let rg_def = link
        .as_ref()
        .map(|l| l.resource_group.clone())
        .unwrap_or_default();
    let site_def = link
        .as_ref()
        .and_then(|l| l.logic_app_name.clone())
        .unwrap_or_default();

    // DevOps project URL (e.g. https://dev.azure.com/org/project)
    let mut ado_url: Signal<String> = use_signal(String::new);
    // All groups fetched from DevOps — used for the browse dropdown
    let mut vg_all: Signal<Vec<VarGroup>> = use_signal(Vec::new);
    let mut vg_loading: Signal<bool> = use_signal(|| false);
    let mut vg_error: Signal<String> = use_signal(String::new);
    let mut browse_open: Signal<bool> = use_signal(|| false);

    // Active extra columns: ordered list of group names
    let mut col_order: Signal<Vec<String>> = use_signal(Vec::new);
    // Azure Cloud column
    let mut az_state: Signal<FetchState> = use_signal(|| FetchState::Idle);
    // Filter + diff toggle
    let mut filter: Signal<String> = use_signal(String::new);
    let mut only_diff: Signal<bool> = use_signal(|| false);
    // Per-column diff toggle: columns active for "Differences only" comparison.
    // "azure" = Azure cloud column; group names for DevOps columns.
    // All columns are active by default (added when the column is created).
    let mut diff_cols: Signal<HashSet<String>> = use_signal(|| {
        let mut s = HashSet::new();
        s.insert("local".into());
        s
    });
    // Secret key visibility
    let mut show_keys: Signal<HashSet<String>> = use_signal(HashSet::new);
    // Clipboard flash: stores "key::col_index" of the last copied cell
    let mut copied_cell: Signal<Option<String>> = use_signal(|| None);
    // Cell detail popup: (key, column_label, full_value)
    let mut detail_cell: Signal<Option<(String, String, String)>> = use_signal(|| None);

    // Sub-view: "settings" (default) shows the key/value compare; "security" shows RBAC + network.
    let mut view_mode: Signal<&'static str> = use_signal(|| "settings");

    // Auto-detect DevOps URL from git remote on mount
    use_effect({
        let dir2 = dir.clone();
        move || {
            let dir2 = dir2.clone();
            spawn(async move {
                if let Some(url) =
                    tokio::task::spawn_blocking(move || env_compare::detect_ado_url(&dir2))
                        .await
                        .ok()
                        .and_then(|u| u)
                {
                    ado_url.set(url);
                }
            });
        }
    });

    // Fetch Azure App Settings
    let fetch_azure = {
        let sub = sub_def.clone();
        let rg = rg_def.clone();
        let site = site_def.clone();
        move |_| {
            if site.is_empty() {
                return;
            }
            az_state.set(FetchState::Loading);
            let (s, r, n) = (sub.clone(), rg.clone(), site.clone());
            spawn(async move {
                let res = tokio::task::spawn_blocking(move || {
                    env_compare::fetch_azure_app_settings(&s, &r, &n)
                })
                .await
                .unwrap_or_else(|_| Err(azure_cli::AzError::Other("task failed".into())));
                match res {
                    Ok(v) => {
                        az_state.set(FetchState::Done(v));
                        diff_cols.write().insert("azure".into());
                    }
                    Err(e) => {
                        az_state.set(FetchState::Err(format!("{:?}", e)));
                    }
                };
            });
        }
    };

    // Browse DevOps variable groups
    let do_browse = {
        move |_| {
            let url = ado_url.read().clone();
            if url.is_empty() {
                return;
            }
            vg_loading.set(true);
            vg_error.set(String::new());
            browse_open.set(false);
            spawn(async move {
                let res = tokio::task::spawn_blocking(move || {
                    env_compare::list_ado_variable_groups(&url)
                })
                .await
                .unwrap_or_else(|_| Err("task failed".into()));
                vg_loading.set(false);
                match res {
                    Ok(groups) => {
                        vg_all.set(groups);
                        browse_open.set(true);
                    }
                    Err(e) => vg_error.set(e),
                }
            });
        }
    };

    // Add a group as a column (data is already in vg_all)
    let mut add_column = move |name: String| {
        let mut order = col_order.write();
        if !order.contains(&name) {
            order.push(name.clone());
        }
        diff_cols.write().insert(name);
        browse_open.set(false);
    };

    let mut remove_column = move |name: String| {
        col_order.write().retain(|n| *n != name);
    };

    // ── Derived table data ────────────────────────────────────────────────────

    let local = props.local_pairs.read().clone();
    let az_vals = az_state.read().values().cloned();
    let order = col_order.read().clone();
    let all_groups = vg_all.read();
    let query = filter.read().to_lowercase();
    let active_diff = diff_cols.read().clone();
    let diff_on = *only_diff.read() && (az_vals.is_some() || !order.is_empty());

    // Build column value maps in order
    let col_maps: Vec<(String, Option<EnvValues>)> = order
        .iter()
        .map(|name| {
            let vals = all_groups
                .iter()
                .find(|g| &g.name == name)
                .map(|g| g.variables.clone());
            (name.clone(), vals)
        })
        .collect();

    // Collect all keys from all sources
    let mut all_keys: Vec<String> = {
        let mut seen: IndexMap<String, ()> = local.keys().map(|k| (k.clone(), ())).collect();
        if let Some(ref az) = az_vals {
            for k in az.keys() {
                seen.entry(k.clone()).or_default();
            }
        }
        for (_, vals) in &col_maps {
            if let Some(v) = vals {
                for k in v.keys() {
                    seen.entry(k.clone()).or_default();
                }
            }
        }
        seen.into_keys().collect()
    };
    all_keys.sort();

    let visible_keys: Vec<String> = all_keys
        .into_iter()
        .filter(|k| query.is_empty() || k.to_lowercase().contains(&query))
        .filter(|k| {
            if !diff_on {
                return true;
            }
            let local_active = active_diff.contains("local");
            if local_active {
                // Compare each active column against local
                let lv = local.get(k).map(|s| s.as_str()).unwrap_or("");
                let az_diff = active_diff.contains("azure")
                    && az_vals
                        .as_ref()
                        .map(|m| m.get(k).map(|v| v.as_str()).unwrap_or("") != lv)
                        .unwrap_or(false);
                let col_diff = col_maps.iter().any(|(name, vals)| {
                    active_diff.contains(name)
                        && vals
                            .as_ref()
                            .map(|m| m.get(k).map(|v| v.as_str()).unwrap_or("") != lv)
                            .unwrap_or(false)
                });
                az_diff || col_diff
            } else {
                // Local unchecked: compare active columns against each other
                let mut vals: Vec<&str> = Vec::new();
                if active_diff.contains("azure") {
                    vals.push(
                        az_vals
                            .as_ref()
                            .and_then(|az| az.get(k))
                            .map(|v| v.as_str())
                            .unwrap_or(""),
                    );
                }
                for (name, col_vals) in &col_maps {
                    if active_diff.contains(name) {
                        vals.push(
                            col_vals
                                .as_ref()
                                .and_then(|v| v.get(k))
                                .map(|v| v.as_str())
                                .unwrap_or(""),
                        );
                    }
                }
                vals.len() >= 2 && vals.iter().any(|v| *v != vals[0])
            }
        })
        .collect();

    let has_any = az_vals.is_some() || !col_maps.iter().all(|(_, v)| v.is_none());

    let mode = *view_mode.read();

    rsx! {
        div { class: "env-compare-panel",

            // ── Shared env-selection header (above tabs) ──────────────
            div { class: "env-compare-topbar",

                // DevOps URL input + Browse button
                div { class: "env-ado-row",
                    span { class: "env-ado-label", "Azure DevOps" }
                    input {
                        class: "env-ado-input",
                        placeholder: "https://dev.azure.com/org/project",
                        value: "{ado_url}",
                        oninput: move |e| {
                            ado_url.set(e.value());
                            browse_open.set(false);
                            vg_all.set(vec![]);
                        },
                    }
                    button {
                        class: "btn btn-small btn-fetch",
                        disabled: *vg_loading.read() || ado_url.read().is_empty(),
                        title: "List variable groups in this DevOps project",
                        onclick: do_browse,
                        if *vg_loading.read() { "…" } else { "⊕ Browse Groups" }
                    }
                    if !vg_error.read().is_empty() {
                        span { class: "env-source-err", title: "{vg_error}", "⚠ {vg_error}" }
                    }
                }

                // Variable group picker dropdown
                if *browse_open.read() && !vg_all.read().is_empty() {
                    div { class: "env-vg-dropdown",
                        div { class: "env-vg-dropdown-header",
                            "Click a group to add it as a column"
                            button {
                                class: "btn-icon",
                                style: "float:right",
                                onclick: move |_| browse_open.set(false),
                                "×"
                            }
                        }
                        for group in vg_all.read().iter() {
                            {
                                let name      = group.name.clone();
                                let name2     = name.clone();
                                let already   = col_order.read().contains(&name);
                                let var_count = group.variables.len();
                                rsx! {
                                    div {
                                        class: if already { "env-vg-option env-vg-added" } else { "env-vg-option" },
                                        onclick: move |_| { if !already { add_column(name.clone()); } },
                                        span { class: "env-vg-name", "{name2}" }
                                        span { class: "env-vg-count", "{var_count} vars" }
                                        if already { span { class: "env-vg-check", "✓" } }
                                    }
                                }
                            }
                        }
                    }
                }

                // Selected env chips (shared between Settings & Security)
                div { class: "env-col-chips",
                    if col_order.read().is_empty() {
                        span { style: "color:#9ca3af;font-size:12px;font-style:italic",
                            "No environment selected — click ⊕ Browse Groups to add one."
                        }
                    }
                    for col_name in col_order.read().iter() {
                        {
                            let name = col_name.clone();
                            let rm   = name.clone();
                            let count = all_groups.iter()
                                .find(|g| g.name == name)
                                .map(|g| g.variables.len());
                            rsx! {
                                div { class: "env-chip",
                                    if let Some(n) = count { span { "{n} keys" } }
                                    span { class: "env-chip-label", "{name}" }
                                    button {
                                        class: "btn-icon",
                                        style: "font-size:9px;opacity:.7",
                                        title: "Remove column",
                                        onclick: move |_| remove_column(rm.clone()),
                                        "×"
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // ── Sub-view tabs ────────────────────────────────────────
            // Use the true Azure product names — "Application Settings" and
            // "App Configuration" are distinct services with different scoping
            // rules. Keeping them on separate tabs preserves that semantic so
            // customers can reason about each correctly.
            div { class: "settings-tabs", style: "border-bottom:1px solid #2a2f3a;margin-bottom:6px",
                button {
                    class: if mode == "settings" { "settings-tab active" } else { "settings-tab" },
                    onclick: move |_| view_mode.set("settings"),
                    title: "Per-site key/value pairs on the Function App (az webapp config appsettings list)",
                    "⚙ Application Settings"
                }
                button {
                    class: if mode == "appconfig" { "settings-tab active" } else { "settings-tab" },
                    onclick: move |_| view_mode.set("appconfig"),
                    title: "Azure App Configuration stores (Microsoft.AppConfiguration/configurationStores)",
                    "🗂 App Configuration"
                }
                button {
                    class: if mode == "security" { "settings-tab active" } else { "settings-tab" },
                    onclick: move |_| view_mode.set("security"),
                    title: "Identity, RBAC and network rules",
                    "🔐 Security"
                }
            }

            if mode == "security" {
                SecurityComparePanel {
                    groups:    vg_all,
                    col_order: col_order,
                }
            } else if mode == "appconfig" {
                AppConfigPanel {
                    local_pairs: props.local_pairs,
                }
            } else {

            // ── Settings-only sub-toolbar: Local + Azure + filter + diff ──
            div { class: "env-compare-topbar",
                div { class: "env-col-chips",
                    // Fixed: Local
                    div { class: "env-chip env-chip-fixed",
                        span { "{local.len()} keys" }
                        span { class: "env-chip-label", "Local" }
                    }

                    // Fixed: Azure cloud
                    if !site_def.is_empty() {
                        div { class: "env-chip env-chip-fixed",
                            {
                                let st = az_state.read();
                                rsx! {
                                    button {
                                        class: "btn btn-small btn-fetch",
                                        disabled: st.is_loading(),
                                        onclick: fetch_azure,
                                        if st.is_loading() { "…" }
                                        else if st.values().is_some() { "⟳" }
                                        else { "↓" }
                                    }
                                    if let Some(v) = st.values() {
                                        span { "{v.len()} keys" }
                                    } else if let Some(e) = st.err() {
                                        span { class: "env-source-err", title: "{e}", "⚠" }
                                    }
                                    span { class: "env-chip-label", "☁ {site_def}" }
                                }
                            }
                        }
                    }

                    div { style: "flex:1" }

                    // Filter + diff toggle
                    input {
                        class: "env-filter-input",
                        placeholder: "Filter keys…",
                        value: "{filter}",
                        oninput: move |e| filter.set(e.value()),
                    }
                    label { class: "env-diff-toggle",
                        input {
                            r#type: "checkbox",
                            checked: *only_diff.read(),
                            onchange: move |_| { let v = *only_diff.read(); only_diff.set(!v); }
                        }
                        " Differences only"
                    }
                }
            }

            // ── Comparison table ───────────────────────────────────────
            div { class: "env-compare-scroll",
                if visible_keys.is_empty() {
                    div { class: "env-compare-empty",
                        if !has_any {
                            "Browse a variable group and click ↓ on Azure to start comparing."
                        } else {
                            "No differences found — all values match local settings."
                        }
                    }
                } else {
                    table { class: "env-compare-table",
                        thead {
                            tr {
                                th { class: "env-th-key", "Key" }
                                th { class: "env-th-val",
                                    label { class: "env-th-check",
                                        input {
                                            r#type: "checkbox",
                                            checked: diff_cols.read().contains("local"),
                                            onchange: move |_| {
                                                let mut dc = diff_cols.write();
                                                if dc.contains("local") { dc.remove("local"); } else { dc.insert("local".into()); }
                                            },
                                        }
                                    }
                                    "Local"
                                }
                                if az_vals.is_some() {
                                    th { class: "env-th-val",
                                        label { class: "env-th-check",
                                            input {
                                                r#type: "checkbox",
                                                checked: diff_cols.read().contains("azure"),
                                                onchange: move |_| {
                                                    let mut dc = diff_cols.write();
                                                    if dc.contains("azure") { dc.remove("azure"); } else { dc.insert("azure".into()); }
                                                },
                                            }
                                        }
                                        "☁ {site_def}"
                                    }
                                }
                                for (name, _) in &col_maps {
                                    if all_groups.iter().any(|g| &g.name == name) {
                                        {
                                            let n = name.clone();
                                            let n2 = name.clone();
                                            rsx! {
                                                th { class: "env-th-val",
                                                    label { class: "env-th-check",
                                                        input {
                                                            r#type: "checkbox",
                                                            checked: diff_cols.read().contains(&n),
                                                            onchange: move |_| {
                                                                let mut dc = diff_cols.write();
                                                                if dc.contains(&n2) { dc.remove(&n2); } else { dc.insert(n2.clone()); }
                                                            },
                                                        }
                                                    }
                                                    "{name}"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        tbody {
                            for key in &visible_keys {
                                {
                                    let key     = key.clone();
                                    let lv      = local.get(&key).cloned().unwrap_or_default();
                                    let secret  = is_secret(&key);
                                    let visible = show_keys.read().contains(&key);

                                    // Mark the whole row when any remote cell differs from local
                                    let row_has_diff = az_vals.as_ref().map(|m| {
                                        m.get(&key).map(|v| v.as_str()).unwrap_or("") != lv.as_str()
                                    }).unwrap_or(false)
                                    || col_maps.iter().any(|(_, vals)| {
                                        vals.as_ref().map(|m| {
                                            m.get(&key).map(|v| v.as_str()).unwrap_or("") != lv.as_str()
                                        }).unwrap_or(false)
                                    });
                                    let row_class = if row_has_diff { "env-compare-row has-diff" } else { "env-compare-row" };

                                    // Helper: show detail popup, copy to clipboard, flash the cell
                                    let mut copy_val = move |cell_id: String, raw: String, key_name: String, col_label: String| {
                                        if raw.is_empty() { return; }
                                        detail_cell.set(Some((key_name, col_label, raw.clone())));
                                        std::thread::spawn(move || {
                                            if let Ok(mut cb) = arboard::Clipboard::new() {
                                                let _ = cb.set_text(raw);
                                            }
                                        });
                                        copied_cell.set(Some(cell_id.clone()));
                                        spawn(async move {
                                            tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
                                            if copied_cell.read().as_deref() == Some(&cell_id) {
                                                copied_cell.set(None);
                                            }
                                        });
                                    };

                                    // Build cell IDs: "key::0" = local, "key::1" = azure, "key::2+" = groups
                                    let cell_local_id = format!("{}::0", key);
                                    let flashed_local = copied_cell.read().as_deref() == Some(&cell_local_id);

                                    rsx! {
                                        tr { class: "{row_class}",
                                            td { class: "env-col-key",
                                                span { title: "{key}", "{key}" }
                                                if secret {
                                                    button {
                                                        class: "btn-icon env-secret-tog",
                                                        title: if visible { "Hide" } else { "Reveal" },
                                                        onclick: {
                                                            let k = key.clone();
                                                            move |_| {
                                                                let mut s = show_keys.write();
                                                                if s.contains(&k) { s.remove(&k); } else { s.insert(k.clone()); }
                                                            }
                                                        },
                                                        if visible { "🙈" } else { "👁" }
                                                    }
                                                }
                                            }
                                            // Local value cell
                                            td {
                                                class: if flashed_local { "env-col-val env-cell-copied" } else { "env-col-val env-cell-copyable" },
                                                title: if secret && !visible { "Reveal to copy" } else { "Click to copy" },
                                                onclick: {
                                                    let id  = cell_local_id.clone();
                                                    let raw = if secret && !visible { String::new() } else { lv.clone() };
                                                    let kn = key.clone();
                                                    move |_| copy_val(id.clone(), raw.clone(), kn.clone(), "Local".into())
                                                },
                                                { render_local(&lv, secret && !visible) }
                                            }
                                            // Azure cloud column
                                            if az_vals.is_some() {
                                                {
                                                    let cell_id  = format!("{}::1", key);
                                                    let raw      = az_vals.as_ref().and_then(|m| m.get(&key)).cloned().unwrap_or_default();
                                                    let flashed  = copied_cell.read().as_deref() == Some(&cell_id);
                                                    let copyable = !raw.is_empty() && !(secret && !visible);
                                                    let td_class: &str = if flashed { "env-col-val env-cell-copied" } else if copyable { "env-col-val env-cell-copyable" } else { "env-col-val" };
                                                    let td_title: &str = if copyable { "Click to copy" } else { "" };
                                                    let az_label = format!("☁ {}", site_def);
                                                    rsx! {
                                                        td {
                                                            class: "{td_class}",
                                                            title: "{td_title}",
                                                            onclick: {
                                                                let id  = cell_id.clone();
                                                                let r   = if secret && !visible { String::new() } else { raw.clone() };
                                                                let kn = key.clone(); let cl = az_label.clone();
                                                                move |_| copy_val(id.clone(), r.clone(), kn.clone(), cl.clone())
                                                            },
                                                            { render_diff(&lv, az_vals.as_ref().and_then(|m| m.get(&key)).map(|s| s.as_str()), secret && !visible) }
                                                        }
                                                    }
                                                }
                                            }
                                            // DevOps group columns
                                            for (col_idx, (col_name, vals)) in col_maps.iter().enumerate() {
                                                if vals.is_some() || all_groups.iter().any(|g| order.contains(&g.name)) {
                                                    {
                                                        let cell_id  = format!("{}::{}", key, col_idx + 2);
                                                        let raw      = vals.as_ref().and_then(|m| m.get(&key)).cloned().unwrap_or_default();
                                                        let flashed  = copied_cell.read().as_deref() == Some(&cell_id);
                                                        let copyable = !raw.is_empty() && !(secret && !visible);
                                                        let td_class: &str = if flashed { "env-col-val env-cell-copied" } else if copyable { "env-col-val env-cell-copyable" } else { "env-col-val" };
                                                        let td_title: &str = if copyable { "Click to copy" } else { "" };
                                                        rsx! {
                                                            td {
                                                                class: "{td_class}",
                                                                title: "{td_title}",
                                                                onclick: {
                                                                    let id = cell_id.clone();
                                                                    let r  = if secret && !visible { String::new() } else { raw.clone() };
                                                                    let kn = key.clone(); let cl = col_name.clone();
                                                                    move |_| copy_val(id.clone(), r.clone(), kn.clone(), cl.clone())
                                                                },
                                                                { render_diff(&lv, vals.as_ref().and_then(|m| m.get(&key)).map(|s| s.as_str()), secret && !visible) }
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

            // ── Cell detail popup ─────────────────────────────────────
            if let Some((ref dk, ref dl, ref dv)) = *detail_cell.read() {
                div {
                    class: "env-detail-overlay",
                    onclick: move |_| detail_cell.set(None),
                    onkeydown: move |e: KeyboardEvent| { if e.key() == Key::Escape { detail_cell.set(None); } },
                    div {
                        class: "env-detail-box",
                        onclick: move |e: Event<MouseData>| e.stop_propagation(),
                        div { class: "env-detail-header",
                            span { class: "env-detail-key", "{dk}" }
                            span { class: "env-detail-col", "{dl}" }
                            button {
                                class: "btn-icon",
                                onclick: move |_| detail_cell.set(None),
                                "×"
                            }
                        }
                        pre { class: "env-detail-value", "{dv}" }
                        div { class: "env-detail-hint", "📋 Copied to clipboard" }
                    }
                }
            }
            } // end of mode == "settings" branch
        }
    }
}

// ── Cell renderers ────────────────────────────────────────────────────────────

fn render_local(val: &str, masked: bool) -> Element {
    if masked {
        rsx! { span { class: "env-secret", "•••" } }
    } else if val.is_empty() {
        rsx! { span { class: "env-val-empty", "—" } }
    } else {
        let d = trunc(val, 45);
        rsx! { span { class: "env-val-local", title: "{val}", "{d}" } }
    }
}

fn render_diff(local_val: &str, cell: Option<&str>, masked: bool) -> Element {
    match cell {
        // Key absent from this environment entirely — muted dash
        None => {
            rsx! { span { class: "env-val-missing", title: "Key not present in this environment", "—" } }
        }
        // Values match — quiet confirmation
        Some(v) if v == local_val => rsx! { span { class: "env-val-same", "≡" } },
        // Value differs but masked (secret)
        Some(_) if masked => {
            rsx! { span { class: "env-val-differs", title: "Value differs", "≠ •••" } }
        }
        // Value differs — amber, show the remote value
        Some(v) => {
            let d = trunc(v, 38);
            rsx! { span { class: "env-val-differs", title: "{v}", "≠ {d}" } }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn is_secret(key: &str) -> bool {
    let k = key.to_lowercase();
    k.contains("secret")
        || k.contains("password")
        || k.contains("_key")
        || k.contains("connectionstring")
        || k.contains("sharedaccesskey")
        || k.contains("apikey")
}

fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}
