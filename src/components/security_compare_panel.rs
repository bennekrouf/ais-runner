use dioxus::prelude::*;
use std::collections::{BTreeSet, HashMap};

use crate::services::env_compare::VarGroup;
use crate::services::security_compare::{
    self, AccessPolicy, CosmosSecurity, EnvTarget, KeyVaultSecurity, SecuritySnapshot,
};

// ── Fetch state ───────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, Debug)]
enum FetchState {
    Idle,
    Loading,
    Done(SecuritySnapshot),
    Err(String),
}

impl FetchState {
    fn snap(&self) -> Option<&SecuritySnapshot> {
        if let FetchState::Done(s) = self { Some(s) } else { None }
    }
}

// ── Props ─────────────────────────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
pub struct SecurityComparePanelProps {
    pub groups:    Signal<Vec<VarGroup>>,
    pub col_order: Signal<Vec<String>>,
}

// ── Component ─────────────────────────────────────────────────────────────────

#[component]
pub fn SecurityComparePanel(props: SecurityComparePanelProps) -> Element {
    let mut states:     Signal<HashMap<String, FetchState>> = use_signal(HashMap::new);
    let mut principals: Signal<HashMap<String, String>>     = use_signal(HashMap::new);
    let mut resolving:  Signal<HashMap<String, ()>>         = use_signal(HashMap::new);
    let mut only_diff:  Signal<bool>                        = use_signal(|| false);
    let mut overrides:  Signal<HashMap<String, EnvTarget>>  = use_signal(HashMap::new);
    let mut editing:    Signal<Option<String>>              = use_signal(|| None);
    let mut edit_buf:   Signal<EnvTarget>                   = use_signal(EnvTarget::default);

    let groups = props.groups.read().clone();
    let order  = props.col_order.read().clone();
    let envs: Vec<VarGroup> = order.iter()
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
    let targets: Vec<(String, EnvTarget)> = envs.iter()
        .map(|g| {
            let name = g.name.clone();
            let target = overrides_read.get(&name)
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
            }).await;
            match res {
                Ok(snap) => { states.write().insert(name, FetchState::Done(snap)); }
                Err(_)   => { states.write().insert(name, FetchState::Err("task failed".into())); }
            }
        });
    };

    // Auto-fetch any newly-selected env that has an actionable target and
    // hasn't been fetched yet. Re-runs whenever the column list or overrides
    // change. Manual ⟳ on a chip still works for refreshes.
    {
        let pending: Vec<(String, EnvTarget)> = targets.iter()
            .filter(|(name, target)| {
                target.is_actionable() && !states.read().contains_key(name)
            })
            .cloned()
            .collect();
        for (name, target) in pending {
            fetch_one(name, target);
        }
    }

    let resolve_principal = move |pid: String| {
        if pid.is_empty() { return; }
        if principals.read().contains_key(&pid) { return; }
        if resolving.read().contains_key(&pid)  { return; }
        resolving.write().insert(pid.clone(), ());
        spawn(async move {
            let p = pid.clone();
            let name = tokio::task::spawn_blocking(move || {
                security_compare::resolve_principal(&p)
            }).await.ok().flatten().unwrap_or_default();
            principals.write().insert(pid.clone(), name);
            resolving.write().remove(&pid);
        });
    };

    let states_read     = states.read().clone();
    let principals_read = principals.read().clone();
    let diff_on         = *only_diff.read();

    let snapshots: Vec<Option<&SecuritySnapshot>> = targets.iter()
        .map(|(name, _)| states_read.get(name).and_then(FetchState::snap))
        .collect();

    let rows = build_rows(&snapshots);
    let visible_rows: Vec<&SecRow> = rows.iter()
        .filter(|r| !diff_on || r.has_diff())
        .collect();

    let render_principal = |pid: &str| -> String {
        if pid.is_empty() { return String::new(); }
        match principals_read.get(pid) {
            Some(name) if !name.is_empty() => name.clone(),
            _ => short_guid(pid),
        }
    };

    rsx! {
        // ── Per-env action chips (matches Settings chip styling) ─────────
        div { class: "env-compare-topbar",
            div { class: "env-col-chips",
                for (name, target) in targets.iter() {
                    {
                        let st_name = name.clone();
                        let st = states_read.get(&st_name).cloned().unwrap_or(FetchState::Idle);
                        let actionable = target.is_actionable();
                        let title = if actionable { String::new() } else {
                            "Couldn't infer Azure target from this group's variables. Click ✎ to set it manually.".into()
                        };
                        let n_fetch = name.clone();
                        let t_fetch = target.clone();
                        let fetch_btn = move |_| fetch_one(n_fetch.clone(), t_fetch.clone());
                        let n_edit = name.clone();
                        let t_edit = target.clone();
                        let edit_btn = move |_| {
                            edit_buf.set(t_edit.clone());
                            editing.set(Some(n_edit.clone()));
                        };
                        let detail = describe_target(target);
                        rsx! {
                            div { class: "env-chip",
                                button {
                                    class: "btn btn-small btn-fetch",
                                    disabled: !actionable || matches!(st, FetchState::Loading),
                                    title: "{title}",
                                    onclick: fetch_btn,
                                    match &st {
                                        FetchState::Loading => "…",
                                        FetchState::Done(_) => "⟳",
                                        _                   => "↓",
                                    }
                                }
                                match &st {
                                    FetchState::Err(e) => rsx! { span { class: "env-source-err", title: "{e}", "⚠" } },
                                    _ => rsx! {},
                                }
                                span { class: "env-chip-label", "{name}" }
                                span { class: "env-chip-sub", title: "{detail}", "{detail}" }
                                button {
                                    class: "btn-icon",
                                    title: "Configure Azure target (subscription / RG / cosmos / key-vault)",
                                    onclick: edit_btn,
                                    "✎"
                                }
                            }
                        }
                    }
                }
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

        // ── Comparison table (same structure as Settings) ────────────────
        div { class: "env-compare-scroll",
            if snapshots.iter().all(|s| s.is_none()) {
                div { class: "env-compare-empty",
                    "Click ↓ on each environment chip to fetch its security posture."
                }
            } else if visible_rows.is_empty() {
                div { class: "env-compare-empty",
                    "No differences found across fetched environments."
                }
            } else {
                table { class: "env-compare-table",
                    thead {
                        tr {
                            th { class: "env-th-key", "Security parameter" }
                            for (name, _) in targets.iter() {
                                th { class: "env-th-val", "{name}" }
                            }
                        }
                    }
                    tbody {
                        {
                            let mut last_section: Option<&str> = None;
                            let col_count = targets.len() + 1;
                            rsx! {
                                for row in visible_rows.iter() {
                                    {
                                        let show_header = last_section != Some(row.section);
                                        last_section = Some(row.section);
                                        let row_class = if row.has_diff() { "env-compare-row has-diff" } else { "env-compare-row" };
                                        rsx! {
                                            if show_header {
                                                tr { class: "env-section-row",
                                                    td { colspan: "{col_count}", "{row.section}" }
                                                }
                                            }
                                            tr { class: "{row_class}",
                                                td { class: "env-col-key", title: "{row.tooltip}", "{row.label}" }
                                                for cell in row.cells.iter() {
                                                    { render_cell(cell, &render_principal, resolve_principal) }
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

        // ── Override-target modal (uses .env-detail-overlay shell) ───────
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
                                button {
                                    class: "btn-icon",
                                    onclick: move |_| editing.set(None),
                                    "×"
                                }
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
                                        "Variable keys available in this DevOps group "
                                        "(click ✎ on the chip uses these for auto-inference):"
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

// ── Row model ─────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
enum Cell {
    Pending,
    NotConfigured,
    Value(String),
    Bool(Option<bool>),
    List(Vec<String>),
    Role(Option<String>, String),
}

struct SecRow {
    section: &'static str,
    label:   String,
    tooltip: String,
    cells:   Vec<Cell>,
}

impl SecRow {
    fn has_diff(&self) -> bool {
        let mut seen: Option<&Cell> = None;
        for c in &self.cells {
            if matches!(c, Cell::Pending) { continue; }
            match seen {
                None => seen = Some(c),
                Some(prev) if cells_equal(prev, c) => {}
                _ => return true,
            }
        }
        false
    }
}

fn cells_equal(a: &Cell, b: &Cell) -> bool {
    match (a, b) {
        (Cell::List(x), Cell::List(y)) => {
            let xs: BTreeSet<&String> = x.iter().collect();
            let ys: BTreeSet<&String> = y.iter().collect();
            xs == ys
        }
        _ => a == b,
    }
}

// ── Row construction ──────────────────────────────────────────────────────────

fn build_rows(snaps: &[Option<&SecuritySnapshot>]) -> Vec<SecRow> {
    let mut rows = Vec::new();
    let n = snaps.len();

    let mut row = |section: &'static str, label: String, tooltip: String, cells: Vec<Cell>| {
        rows.push(SecRow { section, label, tooltip, cells });
    };

    // ── Cosmos DB ────────────────────────────────────────────────────────────
    if snaps.iter().any(|s| s.map(|s| s.cosmos.is_some() || s.target.cosmos_account.is_some()).unwrap_or(false)) {
        let cosmos_or_pending = |idx: usize| -> Option<Option<&CosmosSecurity>> {
            match snaps[idx] { None => None, Some(s) => Some(s.cosmos.as_ref()) }
        };
        let cells_from = |f: &dyn Fn(&CosmosSecurity) -> Cell| -> Vec<Cell> {
            (0..n).map(|i| match cosmos_or_pending(i) {
                None => Cell::Pending,
                Some(None) => Cell::NotConfigured,
                Some(Some(c)) => f(c),
            }).collect()
        };

        row("Cosmos DB",
            "Account name".into(), "az cosmosdb show → name".into(),
            cells_from(&|c| Cell::Value(c.account_name.clone())));
        row("Cosmos DB",
            "disableLocalAuth".into(),
            "If true, key-based auth is disabled — only AAD/RBAC allowed.".into(),
            cells_from(&|c| Cell::Bool(c.disable_local_auth)));
        row("Cosmos DB",
            "publicNetworkAccess".into(), "Enabled / Disabled".into(),
            cells_from(&|c| match c.public_network_access.as_deref() {
                Some(v) => Cell::Value(v.into()),
                None    => Cell::Value("—".into()),
            }));
        row("Cosmos DB",
            "networkAclBypass".into(),
            "Which services can bypass the firewall (None / AzureServices).".into(),
            cells_from(&|c| match c.network_acl_bypass.as_deref() {
                Some(v) => Cell::Value(v.into()),
                None    => Cell::Value("—".into()),
            }));
        row("Cosmos DB",
            "Key-based metadata write".into(),
            "If true, keys can mutate metadata (DBs/containers). Best practice: disabled.".into(),
            cells_from(&|c| Cell::Bool(c.key_metadata_write_enabled)));
        row("Cosmos DB",
            "Firewall IP rules".into(), "ipRules[].ipAddressOrRange".into(),
            cells_from(&|c| Cell::List(c.ip_rules.clone())));
        row("Cosmos DB",
            "VNet rules".into(), "virtualNetworkRules[].id".into(),
            cells_from(&|c| Cell::List(c.vnet_rules.clone())));

        // SQL RBAC pivot
        let mut keys: BTreeSet<(String, String, Option<String>)> = BTreeSet::new();
        for s in snaps.iter().flatten() {
            if let Some(c) = &s.cosmos {
                for ra in &c.sql_role_assignments {
                    keys.insert((ra.principal_id.clone(), ra.role_definition_id.clone(), ra.role_name.clone()));
                }
            }
        }
        for (pid, role_id, role_name) in keys {
            let label = format!("SQL RBAC · {} → {}",
                short_guid(&pid),
                role_name.clone().unwrap_or_else(|| short_guid(&role_id)));
            let tooltip = format!("principalId={}\nroleDefinitionId={}", pid, role_id);
            let cells: Vec<Cell> = (0..n).map(|i| match cosmos_or_pending(i) {
                None => Cell::Pending,
                Some(None) => Cell::NotConfigured,
                Some(Some(c)) => {
                    let present = c.sql_role_assignments.iter()
                        .any(|ra| ra.principal_id == pid && ra.role_definition_id == role_id);
                    if present {
                        Cell::Role(role_name.clone().or_else(|| Some(short_guid(&role_id))), pid.clone())
                    } else {
                        Cell::Role(None, pid.clone())
                    }
                }
            }).collect();
            row("Cosmos DB · SQL RBAC", label, tooltip, cells);
        }

        // ARM RBAC pivot
        let mut keys: BTreeSet<(String, String, Option<String>)> = BTreeSet::new();
        for s in snaps.iter().flatten() {
            if let Some(c) = &s.cosmos {
                for ra in &c.arm_role_assignments {
                    keys.insert((ra.principal_id.clone(), ra.role_definition_id.clone(), ra.role_name.clone()));
                }
            }
        }
        for (pid, role_id, role_name) in keys {
            let label = format!("ARM RBAC · {} → {}",
                short_guid(&pid),
                role_name.clone().unwrap_or_else(|| short_guid(&role_id)));
            let tooltip = format!("principalId={}\nroleDefinitionId={}", pid, role_id);
            let cells: Vec<Cell> = (0..n).map(|i| match cosmos_or_pending(i) {
                None => Cell::Pending,
                Some(None) => Cell::NotConfigured,
                Some(Some(c)) => {
                    let present = c.arm_role_assignments.iter()
                        .any(|ra| ra.principal_id == pid && ra.role_definition_id == role_id);
                    if present {
                        Cell::Role(role_name.clone(), pid.clone())
                    } else {
                        Cell::Role(None, pid.clone())
                    }
                }
            }).collect();
            row("Cosmos DB · ARM RBAC", label, tooltip, cells);
        }
    }

    // ── Key Vault ────────────────────────────────────────────────────────────
    if snaps.iter().any(|s| s.map(|s| s.key_vault.is_some() || s.target.key_vault.is_some()).unwrap_or(false)) {
        let kv_or_pending = |idx: usize| -> Option<Option<&KeyVaultSecurity>> {
            match snaps[idx] { None => None, Some(s) => Some(s.key_vault.as_ref()) }
        };
        let cells_from = |f: &dyn Fn(&KeyVaultSecurity) -> Cell| -> Vec<Cell> {
            (0..n).map(|i| match kv_or_pending(i) {
                None => Cell::Pending,
                Some(None) => Cell::NotConfigured,
                Some(Some(c)) => f(c),
            }).collect()
        };

        row("Key Vault",
            "Vault name".into(), "az keyvault show → name".into(),
            cells_from(&|c| Cell::Value(c.vault_name.clone())));
        row("Key Vault",
            "enableRbacAuthorization".into(),
            "If true, access is governed by RBAC (not the legacy access-policy model).".into(),
            cells_from(&|c| Cell::Bool(c.enable_rbac_authorization)));
        row("Key Vault",
            "publicNetworkAccess".into(), "Enabled / Disabled".into(),
            cells_from(&|c| match c.public_network_access.as_deref() {
                Some(v) => Cell::Value(v.into()),
                None    => Cell::Value("—".into()),
            }));
        row("Key Vault",
            "Purge protection".into(),
            "If true, deleted vaults/secrets cannot be force-purged.".into(),
            cells_from(&|c| Cell::Bool(c.purge_protection)));
        row("Key Vault",
            "Soft-delete retention (days)".into(), "softDeleteRetentionInDays".into(),
            cells_from(&|c| Cell::Value(c.soft_delete_retention_days.map(|d| d.to_string()).unwrap_or_else(|| "—".into()))));
        row("Key Vault",
            "Firewall IP rules".into(), "networkAcls.ipRules[].value".into(),
            cells_from(&|c| Cell::List(c.ip_rules.clone())));
        row("Key Vault",
            "VNet rules".into(), "networkAcls.virtualNetworkRules[].id".into(),
            cells_from(&|c| Cell::List(c.vnet_rules.clone())));

        // RBAC pivot
        let mut keys: BTreeSet<(String, String, Option<String>)> = BTreeSet::new();
        for s in snaps.iter().flatten() {
            if let Some(c) = &s.key_vault {
                for ra in &c.role_assignments {
                    keys.insert((ra.principal_id.clone(), ra.role_definition_id.clone(), ra.role_name.clone()));
                }
            }
        }
        for (pid, role_id, role_name) in keys {
            let label = format!("RBAC · {} → {}",
                short_guid(&pid),
                role_name.clone().unwrap_or_else(|| short_guid(&role_id)));
            let tooltip = format!("principalId={}\nroleDefinitionId={}", pid, role_id);
            let cells: Vec<Cell> = (0..n).map(|i| match kv_or_pending(i) {
                None => Cell::Pending,
                Some(None) => Cell::NotConfigured,
                Some(Some(c)) => {
                    let present = c.role_assignments.iter()
                        .any(|ra| ra.principal_id == pid && ra.role_definition_id == role_id);
                    if present {
                        Cell::Role(role_name.clone(), pid.clone())
                    } else {
                        Cell::Role(None, pid.clone())
                    }
                }
            }).collect();
            row("Key Vault · RBAC", label, tooltip, cells);
        }

        // Access-policy pivot
        let mut object_ids: BTreeSet<String> = BTreeSet::new();
        for s in snaps.iter().flatten() {
            if let Some(c) = &s.key_vault {
                for ap in &c.access_policies { object_ids.insert(ap.object_id.clone()); }
            }
        }
        for oid in object_ids {
            let label = format!("Access policy · {}", short_guid(&oid));
            let tooltip = format!("objectId={}", oid);
            let cells: Vec<Cell> = (0..n).map(|i| match kv_or_pending(i) {
                None => Cell::Pending,
                Some(None) => Cell::NotConfigured,
                Some(Some(c)) => {
                    match c.access_policies.iter().find(|ap| ap.object_id == oid) {
                        Some(ap) => Cell::Value(format_policy(ap)),
                        None     => Cell::Value("—".into()),
                    }
                }
            }).collect();
            row("Key Vault · Access policies", label, tooltip, cells);
        }
    }

    rows
}

fn format_policy(ap: &AccessPolicy) -> String {
    let mut parts = Vec::new();
    if !ap.permissions_keys.is_empty()    { parts.push(format!("keys: {}",    ap.permissions_keys.join(","))); }
    if !ap.permissions_secrets.is_empty() { parts.push(format!("secrets: {}", ap.permissions_secrets.join(","))); }
    if !ap.permissions_certs.is_empty()   { parts.push(format!("certs: {}",   ap.permissions_certs.join(","))); }
    if parts.is_empty() { "(no permissions)".into() } else { parts.join(" · ") }
}

// ── Cell rendering ────────────────────────────────────────────────────────────

fn render_cell(
    cell: &Cell,
    render_principal: &dyn Fn(&str) -> String,
    resolve_principal: impl FnMut(String) + Copy + 'static,
) -> Element {
    match cell {
        Cell::Pending => rsx! {
            td { class: "env-col-val", span { class: "env-val-empty", "…" } }
        },
        Cell::NotConfigured => rsx! {
            td { class: "env-col-val", span { class: "env-val-missing", title: "Not configured for this env", "—" } }
        },
        Cell::Value(v) => {
            let display = if v.is_empty() { "—".to_string() } else { trunc(v, 60) };
            let full = v.clone();
            rsx! { td { class: "env-col-val", title: "{full}", span { class: "env-val-local", "{display}" } } }
        }
        Cell::Bool(b) => match b {
            Some(true)  => rsx! { td { class: "env-col-val", span { class: "env-val-local",   "✓ true"  } } },
            Some(false) => rsx! { td { class: "env-col-val", span { class: "env-val-differs", "✗ false" } } },
            None        => rsx! { td { class: "env-col-val", span { class: "env-val-empty",   "—"       } } },
        },
        Cell::List(items) => {
            if items.is_empty() {
                rsx! { td { class: "env-col-val", span { class: "env-val-empty", "(none)" } } }
            } else {
                let joined = items.join(", ");
                let display = trunc(&joined, 60);
                let count = items.len();
                rsx! {
                    td { class: "env-col-val", title: "{joined}",
                        span { class: "env-val-local", "{count} · {display}" }
                    }
                }
            }
        }
        Cell::Role(role, pid) => match role {
            None => rsx! { td { class: "env-col-val", span { class: "env-val-missing", "—" } } },
            Some(rn) => {
                let principal_display = render_principal(pid);
                let pid_owned  = pid.clone();
                let role_owned = rn.clone();
                let tooltip = format!("principalId={}\nClick to resolve display name", pid);
                let mut resolve = resolve_principal;
                rsx! {
                    td { class: "env-col-val env-cell-copyable",
                        title: "{tooltip}",
                        onclick: move |_| resolve(pid_owned.clone()),
                        span { class: "env-val-local", "✓ {role_owned}" }
                        br {}
                        span { class: "env-val-empty", "{principal_display}" }
                    }
                }
            }
        },
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn short_guid(s: &str) -> String {
    let tail = s.rsplit('/').next().unwrap_or(s);
    if tail.len() >= 8 { tail.chars().take(8).collect::<String>() + "…" } else { tail.to_string() }
}

fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() }
    else { format!("{}…", s.chars().take(max).collect::<String>()) }
}

fn describe_target(t: &EnvTarget) -> String {
    let mut bits = Vec::new();
    if let Some(c) = &t.cosmos_account { bits.push(format!("cosmos:{}", c)); }
    if let Some(v) = &t.key_vault      { bits.push(format!("kv:{}", v)); }
    if bits.is_empty() { "—".into() } else { bits.join(" · ") }
}
