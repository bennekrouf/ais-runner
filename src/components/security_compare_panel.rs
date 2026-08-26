use dioxus::prelude::*;
use std::collections::HashMap;

use crate::services::env_compare::VarGroup;
use crate::services::security_compare::{
    self, AccessPolicy, CosmosSecurity, EnvTarget, KeyVaultSecurity, RoleAssignment,
    SecuritySnapshot,
};

// ── Fetch state ───────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, Debug)]
enum FetchState {
    Loading,
    Done(SecuritySnapshot),
    Err(String),
}

impl FetchState {
    fn snap(&self) -> Option<&SecuritySnapshot> {
        if let FetchState::Done(s) = self {
            Some(s)
        } else {
            None
        }
    }
}

// ── Props ─────────────────────────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
pub struct SecurityComparePanelProps {
    pub groups: Signal<Vec<VarGroup>>,
    pub col_order: Signal<Vec<String>>,
}

// ── Component ─────────────────────────────────────────────────────────────────

#[component]
pub fn SecurityComparePanel(props: SecurityComparePanelProps) -> Element {
    let mut states: Signal<HashMap<String, FetchState>> = use_signal(HashMap::new);
    let mut only_diff: Signal<bool> = use_signal(|| false);
    let mut overrides: Signal<HashMap<String, EnvTarget>> = use_signal(HashMap::new);
    let mut editing: Signal<Option<String>> = use_signal(|| None);
    let mut edit_buf: Signal<EnvTarget> = use_signal(EnvTarget::default);
    // Cell detail popup: (key, env_name, full_value)
    let mut detail: Signal<Option<(String, String, String)>> = use_signal(|| None);

    let groups = props.groups.read().clone();
    let order = props.col_order.read().clone();
    let envs: Vec<VarGroup> = order
        .iter()
        .filter_map(|n| groups.iter().find(|g| &g.name == n).cloned())
        .collect();

    if envs.is_empty() {
        return rsx! {
            div { class: "env-compare-empty",
                "Add at least one environment from the chip row above, then come back here to compare its security posture."
            }
        };
    }

    // Effective targets: override (if set) wins over inference.
    let overrides_read = overrides.read().clone();
    let targets: Vec<(String, EnvTarget)> = envs
        .iter()
        .map(|g| {
            let name = g.name.clone();
            let target = overrides_read
                .get(&name)
                .cloned()
                .unwrap_or_else(|| security_compare::infer_env_from_group(g));
            (name, target)
        })
        .collect();

    let mut fetch_one = move |name: String, target: EnvTarget| {
        states.write().insert(name.clone(), FetchState::Loading);
        spawn(async move {
            let n = name.clone();
            let res = tokio::task::spawn_blocking(move || {
                security_compare::fetch_security_snapshot(&n, &target)
            })
            .await;
            match res {
                Ok(snap) => {
                    states.write().insert(name, FetchState::Done(snap));
                }
                Err(_) => {
                    states
                        .write()
                        .insert(name, FetchState::Err("task failed".into()));
                }
            }
        });
    };

    // Auto-fetch any env with an actionable target that hasn't been fetched yet.
    {
        let pending: Vec<(String, EnvTarget)> = targets
            .iter()
            .filter(|(name, target)| target.is_actionable() && !states.read().contains_key(name))
            .cloned()
            .collect();
        for (name, target) in pending {
            fetch_one(name, target);
        }
    }

    let states_read = states.read().clone();
    let diff_on = *only_diff.read();

    let snapshots: Vec<Option<&SecuritySnapshot>> = targets
        .iter()
        .map(|(name, _)| states_read.get(name).and_then(FetchState::snap))
        .collect();

    let rows = build_rows(&snapshots);
    let visible_rows: Vec<&Row> = rows.iter().filter(|r| !diff_on || r.has_diff()).collect();

    // Accurate empty-state messaging — distinguishes "loading", "nothing to do",
    // "fetch finished but empty", and "fetch failed".
    let any_loading = targets
        .iter()
        .any(|(n, _)| matches!(states_read.get(n), Some(FetchState::Loading)));
    let any_actionable = targets.iter().any(|(_, t)| t.is_actionable());
    let any_done = targets
        .iter()
        .any(|(n, _)| matches!(states_read.get(n), Some(FetchState::Done(_))));
    let any_err: Vec<(String, String)> = targets
        .iter()
        .filter_map(|(n, _)| match states_read.get(n) {
            Some(FetchState::Err(e)) => Some((n.clone(), e.clone())),
            Some(FetchState::Done(s)) => {
                let mut parts = Vec::new();
                if let Some(e) = &s.cosmos_err {
                    parts.push(format!("cosmos: {}", e));
                }
                if let Some(e) = &s.key_vault_err {
                    parts.push(format!("kv: {}", e));
                }
                if parts.is_empty() {
                    None
                } else {
                    Some((n.clone(), parts.join("; ")))
                }
            }
            _ => None,
        })
        .collect();

    let empty_msg: String = if !any_actionable {
        "No environment has a complete Azure target. Click ⚙ Configure in a column header to set subscription / resource group / Cosmos / Key Vault for that env.".into()
    } else if any_loading {
        "Fetching security posture…".into()
    } else if !any_done {
        "Auto-fetch didn't start (all envs unactionable). Configure a target above.".into()
    } else if diff_on {
        "No differences found across environments.".into()
    } else if rows.is_empty() {
        "Fetch returned no Cosmos or Key Vault data. See errors above the table if any.".into()
    } else {
        "No data.".into()
    };

    rsx! {
        // ── Top bar: just the differences toggle (env chips are above tabs) ──
        div { class: "env-compare-topbar",
            div { class: "env-col-chips",
                div { style: "flex:1" }
                label { class: "env-diff-toggle",
                    input {
                        r#type: "checkbox",
                        checked: diff_on,
                        onchange: move |_| { let v = *only_diff.read(); only_diff.set(!v); }
                    }
                    " Differences only"
                }
            }
        }

        // ── Per-env fetch errors (surfaced so the user isn't stuck on "…") ──
        if !any_err.is_empty() {
            div { style: "padding:8px 14px;background:color-mix(in srgb, var(--red) 12%, transparent);border-bottom:1px solid var(--border);font-size:11px;color:var(--text2)",
                strong { style: "color:var(--red)", "⚠ Fetch errors " }
                for (env_name, err) in any_err.iter() {
                    div { style: "margin-top:3px;font-family:var(--font-mono);font-size:11px",
                        strong { "{env_name}: " }
                        "{err}"
                    }
                }
            }
        }

        // ── Comparison table (mirrors Settings layout exactly) ───────────
        div { class: "env-compare-scroll",
            table { class: "env-compare-table",
                thead {
                    tr {
                        th { class: "env-th-key", "Security parameter" }
                        for (name, target) in targets.iter() {
                            {
                                let n_for_edit = name.clone();
                                let t_for_edit = target.clone();
                                let configurable = !target.is_actionable();
                                let subtitle = if configurable {
                                    "couldn't infer target".to_string()
                                } else {
                                    describe_target(target)
                                };
                                let edit_btn = move |_| {
                                    edit_buf.set(t_for_edit.clone());
                                    editing.set(Some(n_for_edit.clone()));
                                };
                                let state_marker = match states_read.get(name) {
                                    Some(FetchState::Loading) => " · loading…",
                                    Some(FetchState::Err(_))  => " · fetch failed",
                                    _ => "",
                                };
                                rsx! {
                                    th { class: "env-th-val",
                                        "{name}"
                                        br {}
                                        if configurable {
                                            span { style: "font-weight:normal;font-size:10px;text-transform:none;color:var(--miss-val)",
                                                "⚠ {subtitle} "
                                                button {
                                                    class: "btn-icon",
                                                    style: "font-size:11px",
                                                    title: "Set subscription / resource group / cosmos / key-vault manually",
                                                    onclick: edit_btn,
                                                    "⚙ Configure"
                                                }
                                            }
                                        } else {
                                            span { style: "font-weight:normal;font-size:10px;text-transform:none;color:var(--text3)",
                                                title: "{subtitle}",
                                                "{subtitle}{state_marker} "
                                                button {
                                                    class: "btn-icon",
                                                    style: "font-size:10px;opacity:.7",
                                                    title: "Edit Azure target for this env",
                                                    onclick: edit_btn,
                                                    "✎"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                tbody {
                    if visible_rows.is_empty() {
                        tr {
                            td { colspan: "{targets.len() + 1}",
                                style: "padding:24px;text-align:center;color:var(--text3);line-height:1.7",
                                div { "{empty_msg}" }
                                // Prominent Configure buttons whenever any env is unactionable
                                if !any_actionable {
                                    div { style: "margin-top:14px;display:flex;gap:8px;justify-content:center;flex-wrap:wrap",
                                        for (name, target) in targets.iter() {
                                            {
                                                let n  = name.clone();
                                                let t  = target.clone();
                                                let unconfigured = !t.is_actionable();
                                                rsx! {
                                                    button {
                                                        class: "btn btn-small btn-fetch",
                                                        disabled: !unconfigured,
                                                        onclick: move |_| {
                                                            edit_buf.set(t.clone());
                                                            editing.set(Some(n.clone()));
                                                        },
                                                        if unconfigured { "⚙ Configure {name}" } else { "✓ {name} configured" }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        for row in visible_rows.iter() {
                            {
                                let row_class = if row.has_diff() { "env-compare-row has-diff" } else { "env-compare-row" };
                                rsx! {
                                    tr { class: "{row_class}",
                                        td { class: "env-col-key", title: "{row.label}", "{row.label}" }
                                        for (i, val) in row.values.iter().enumerate() {
                                            {
                                                let key  = row.label.clone();
                                                let env  = targets[i].0.clone();
                                                let raw  = val.clone();
                                                let copyable = !raw.is_empty() && raw != "—" && raw != "…";
                                                let td_class = if copyable { "env-col-val env-cell-copyable" } else { "env-col-val" };
                                                let display = trunc(&raw, 60);
                                                let cell_class = pick_class(&raw);
                                                rsx! {
                                                    td {
                                                        class: "{td_class}",
                                                        title: "{raw}",
                                                        onclick: move |_| {
                                                            if copyable {
                                                                let r = raw.clone();
                                                                let k = key.clone();
                                                                let e = env.clone();
                                                                std::thread::spawn(move || {
                                                                    if let Ok(mut cb) = arboard::Clipboard::new() {
                                                                        let _ = cb.set_text(r);
                                                                    }
                                                                });
                                                                detail.set(Some((k, e, raw.clone())));
                                                            }
                                                        },
                                                        span { class: "{cell_class}", "{display}" }
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

        // ── Cell detail popup (mirrors Settings popup exactly) ───────────
        if let Some((ref dk, ref de, ref dv)) = *detail.read() {
            div { class: "env-detail-overlay",
                onclick: move |_| detail.set(None),
                onkeydown: move |e: KeyboardEvent| { if e.key() == Key::Escape { detail.set(None); } },
                div { class: "env-detail-box",
                    onclick: move |e: Event<MouseData>| e.stop_propagation(),
                    div { class: "env-detail-header",
                        span { class: "env-detail-key", "{dk}" }
                        span { class: "env-detail-col", "{de}" }
                        button { class: "btn-icon", onclick: move |_| detail.set(None), "×" }
                    }
                    pre { class: "env-detail-value", "{dv}" }
                    div { class: "env-detail-hint", "📋 Copied to clipboard" }
                }
            }
        }

        // ── Override-target modal ────────────────────────────────────────
        if let Some(edit_name) = editing.read().clone() {
            {
                let group = envs.iter().find(|g| g.name == edit_name).cloned();
                let keys: Vec<String> = group.as_ref()
                    .map(security_compare::group_variable_keys)
                    .unwrap_or_default();
                let buf = edit_buf.read().clone();
                let close_name = edit_name.clone();
                let save_name  = edit_name.clone();
                rsx! {
                    div { class: "env-detail-overlay",
                        onclick: move |_| editing.set(None),
                        div { class: "env-detail-box",
                            onclick: move |e: Event<MouseData>| e.stop_propagation(),
                            div { class: "env-detail-header",
                                span { class: "env-detail-key", "Configure Azure target" }
                                span { class: "env-detail-col", "{edit_name}" }
                                button { class: "btn-icon", onclick: move |_| editing.set(None), "×" }
                            }
                            div { class: "env-detail-form",
                                label { "Subscription ID" }
                                input {
                                    placeholder: "00000000-0000-0000-0000-000000000000",
                                    value: "{buf.subscription}",
                                    oninput: move |e| { edit_buf.write().subscription = e.value(); }
                                }
                                label { "Resource group" }
                                input {
                                    placeholder: "rg-tom-dev-chn-001",
                                    value: "{buf.resource_group}",
                                    oninput: move |e| { edit_buf.write().resource_group = e.value(); }
                                }
                                label { "Cosmos account" }
                                input {
                                    placeholder: "cosmos-tom-dev-chn-001 (optional)",
                                    value: "{buf.cosmos_account.clone().unwrap_or_default()}",
                                    oninput: move |e| {
                                        let v = e.value();
                                        edit_buf.write().cosmos_account = if v.is_empty() { None } else { Some(v) };
                                    }
                                }
                                label { "Key vault" }
                                input {
                                    placeholder: "kv-tom-dev-chn-001 (optional)",
                                    value: "{buf.key_vault.clone().unwrap_or_default()}",
                                    oninput: move |e| {
                                        let v = e.value();
                                        edit_buf.write().key_vault = if v.is_empty() { None } else { Some(v) };
                                    }
                                }
                            }
                            div { class: "env-detail-actions",
                                button {
                                    class: "btn btn-small btn-fetch",
                                    onclick: move |_| {
                                        let t = edit_buf.read().clone();
                                        overrides.write().insert(save_name.clone(), t);
                                        editing.set(None);
                                    },
                                    "Save"
                                }
                                button {
                                    class: "btn btn-small",
                                    onclick: move |_| editing.set(None),
                                    "Cancel"
                                }
                                {
                                    let n_clear = close_name.clone();
                                    rsx! {
                                        button {
                                            class: "btn btn-small",
                                            title: "Clear override and fall back to auto-inference",
                                            onclick: move |_| {
                                                overrides.write().remove(&n_clear);
                                                editing.set(None);
                                            },
                                            "Clear override"
                                        }
                                    }
                                }
                            }
                            if !keys.is_empty() {
                                div { class: "env-detail-keys",
                                    div { class: "env-detail-keys-title",
                                        "Variable keys present in this DevOps group (used for auto-inference):"
                                    }
                                    div { class: "env-detail-keys-list",
                                        for k in keys.iter() { span { "{k}" } }
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

// ── Row model (just strings) ──────────────────────────────────────────────────

struct Row {
    label: String,
    values: Vec<String>,
}

impl Row {
    fn has_diff(&self) -> bool {
        let known: Vec<&String> = self
            .values
            .iter()
            .filter(|v| !v.is_empty() && v.as_str() != "…")
            .collect();
        known.windows(2).any(|w| w[0] != w[1])
    }
}

// ── Row construction ──────────────────────────────────────────────────────────

fn build_rows(snaps: &[Option<&SecuritySnapshot>]) -> Vec<Row> {
    let n = snaps.len();
    let mut rows: Vec<Row> = Vec::new();

    let cosmos = |i: usize| -> Option<&CosmosSecurity> { snaps[i].and_then(|s| s.cosmos.as_ref()) };
    let vault =
        |i: usize| -> Option<&KeyVaultSecurity> { snaps[i].and_then(|s| s.key_vault.as_ref()) };
    let pending = |i: usize| -> bool { snaps[i].is_none() };

    let any_cosmos = (0..n).any(|i| {
        cosmos(i).is_some()
            || snaps[i]
                .map(|s| s.target.cosmos_account.is_some())
                .unwrap_or(false)
    });
    let any_vault = (0..n).any(|i| {
        vault(i).is_some()
            || snaps[i]
                .map(|s| s.target.key_vault.is_some())
                .unwrap_or(false)
    });

    let bool_str = |b: Option<bool>| -> String {
        match b {
            Some(true) => "true".into(),
            Some(false) => "false".into(),
            None => "—".into(),
        }
    };
    let str_or_dash = |s: &Option<String>| -> String { s.clone().unwrap_or_else(|| "—".into()) };
    let list_str = |items: &[String]| -> String {
        if items.is_empty() {
            "(none)".into()
        } else {
            items.join(", ")
        }
    };

    // ── Cosmos DB ────────────────────────────────────────────────────────
    if any_cosmos {
        let cosmos_row = |label: &str, f: &dyn Fn(&CosmosSecurity) -> String| -> Row {
            Row {
                label: label.to_string(),
                values: (0..n)
                    .map(|i| {
                        if pending(i) {
                            "…".into()
                        } else {
                            cosmos(i).map(f).unwrap_or_else(|| "—".into())
                        }
                    })
                    .collect(),
            }
        };

        rows.push(cosmos_row("Cosmos / account name", &|c| {
            c.account_name.clone()
        }));
        rows.push(cosmos_row("Cosmos / disableLocalAuth", &|c| {
            bool_str(c.disable_local_auth)
        }));
        rows.push(cosmos_row("Cosmos / publicNetworkAccess", &|c| {
            str_or_dash(&c.public_network_access)
        }));
        rows.push(cosmos_row("Cosmos / networkAclBypass", &|c| {
            str_or_dash(&c.network_acl_bypass)
        }));
        rows.push(cosmos_row("Cosmos / key metadata write", &|c| {
            bool_str(c.key_metadata_write_enabled)
        }));
        rows.push(cosmos_row("Cosmos / firewall IP rules", &|c| {
            list_str(&c.ip_rules)
        }));
        rows.push(cosmos_row("Cosmos / VNet rules", &|c| {
            list_str(&c.vnet_rules)
        }));
        rows.push(cosmos_row("Cosmos / SQL role assignments", &|c| {
            roles_str(&c.sql_role_assignments)
        }));
        rows.push(cosmos_row("Cosmos / ARM role assignments", &|c| {
            roles_str(&c.arm_role_assignments)
        }));
    }

    // ── Key Vault ────────────────────────────────────────────────────────
    if any_vault {
        let kv_row = |label: &str, f: &dyn Fn(&KeyVaultSecurity) -> String| -> Row {
            Row {
                label: label.to_string(),
                values: (0..n)
                    .map(|i| {
                        if pending(i) {
                            "…".into()
                        } else {
                            vault(i).map(f).unwrap_or_else(|| "—".into())
                        }
                    })
                    .collect(),
            }
        };

        rows.push(kv_row("KeyVault / vault name", &|v| v.vault_name.clone()));
        rows.push(kv_row("KeyVault / enableRbacAuthorization", &|v| {
            bool_str(v.enable_rbac_authorization)
        }));
        rows.push(kv_row("KeyVault / publicNetworkAccess", &|v| {
            str_or_dash(&v.public_network_access)
        }));
        rows.push(kv_row("KeyVault / purge protection", &|v| {
            bool_str(v.purge_protection)
        }));
        rows.push(kv_row("KeyVault / soft-delete retention days", &|v| {
            v.soft_delete_retention_days
                .map(|d| d.to_string())
                .unwrap_or_else(|| "—".into())
        }));
        rows.push(kv_row("KeyVault / firewall IP rules", &|v| {
            list_str(&v.ip_rules)
        }));
        rows.push(kv_row("KeyVault / VNet rules", &|v| {
            list_str(&v.vnet_rules)
        }));
        rows.push(kv_row("KeyVault / role assignments", &|v| {
            roles_str(&v.role_assignments)
        }));
        rows.push(kv_row("KeyVault / access policies", &|v| {
            policies_str(&v.access_policies)
        }));
    }

    rows
}

fn roles_str(roles: &[RoleAssignment]) -> String {
    if roles.is_empty() {
        return "(none)".into();
    }
    let mut out: Vec<String> = roles
        .iter()
        .map(|ra| {
            let role = ra
                .role_name
                .clone()
                .unwrap_or_else(|| short_guid(&ra.role_definition_id));
            format!("{} → {}", role, short_guid(&ra.principal_id))
        })
        .collect();
    out.sort();
    out.join(" · ")
}

fn policies_str(policies: &[AccessPolicy]) -> String {
    if policies.is_empty() {
        return "(none)".into();
    }
    let mut out: Vec<String> = policies
        .iter()
        .map(|ap| {
            let mut parts = Vec::new();
            if !ap.permissions_keys.is_empty() {
                parts.push(format!("keys:{}", ap.permissions_keys.join(",")));
            }
            if !ap.permissions_secrets.is_empty() {
                parts.push(format!("secrets:{}", ap.permissions_secrets.join(",")));
            }
            if !ap.permissions_certs.is_empty() {
                parts.push(format!("certs:{}", ap.permissions_certs.join(",")));
            }
            format!("{} [{}]", short_guid(&ap.object_id), parts.join(" "))
        })
        .collect();
    out.sort();
    out.join(" · ")
}

// ── Cell helpers ──────────────────────────────────────────────────────────────

fn pick_class(value: &str) -> &'static str {
    match value {
        "" | "—" => "env-val-missing",
        "…" => "env-val-empty",
        "(none)" => "env-val-empty",
        "true" | "Enabled" => "env-val-local",
        "false" | "Disabled" => "env-val-differs",
        _ => "env-val-local",
    }
}

fn short_guid(s: &str) -> String {
    let tail = s.rsplit('/').next().unwrap_or(s);
    if tail.len() >= 8 {
        tail.chars().take(8).collect::<String>() + "…"
    } else {
        tail.to_string()
    }
}

fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

fn describe_target(t: &EnvTarget) -> String {
    let mut bits = Vec::new();
    if let Some(c) = &t.cosmos_account {
        bits.push(format!("cosmos: {}", c));
    }
    if let Some(v) = &t.key_vault {
        bits.push(format!("kv: {}", v));
    }
    if bits.is_empty() {
        "—".into()
    } else {
        bits.join(" · ")
    }
}
