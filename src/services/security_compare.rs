use serde_json::Value;

use crate::services::azure_cli::{az_command, AzError};
use crate::services::env_compare::VarGroup;

// ── Models ────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq)]
pub struct EnvTarget {
    pub subscription: String,
    pub resource_group: String,
    pub cosmos_account: Option<String>,
    pub key_vault: Option<String>,
}

impl EnvTarget {
    pub fn is_actionable(&self) -> bool {
        !self.subscription.is_empty()
            && !self.resource_group.is_empty()
            && (self.cosmos_account.is_some() || self.key_vault.is_some())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RoleAssignment {
    pub principal_id: String,
    pub principal_type: Option<String>,
    pub role_definition_id: String,
    pub role_name: Option<String>,
    pub scope: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CosmosSecurity {
    pub account_name: String,
    pub disable_local_auth: Option<bool>,
    pub public_network_access: Option<String>,
    pub network_acl_bypass: Option<String>,
    pub key_metadata_write_enabled: Option<bool>,
    pub ip_rules: Vec<String>,
    pub vnet_rules: Vec<String>,
    pub sql_role_assignments: Vec<RoleAssignment>,
    pub arm_role_assignments: Vec<RoleAssignment>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AccessPolicy {
    pub object_id: String,
    pub permissions_keys: Vec<String>,
    pub permissions_secrets: Vec<String>,
    pub permissions_certs: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct KeyVaultSecurity {
    pub vault_name: String,
    pub enable_rbac_authorization: Option<bool>,
    pub public_network_access: Option<String>,
    pub purge_protection: Option<bool>,
    pub soft_delete_retention_days: Option<i64>,
    pub ip_rules: Vec<String>,
    pub vnet_rules: Vec<String>,
    pub role_assignments: Vec<RoleAssignment>,
    pub access_policies: Vec<AccessPolicy>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SecuritySnapshot {
    pub env_name: String,
    pub target: EnvTarget,
    pub cosmos: Option<CosmosSecurity>,
    pub cosmos_err: Option<String>,
    pub key_vault: Option<KeyVaultSecurity>,
    pub key_vault_err: Option<String>,
}

// ── Env discovery from variable groups ────────────────────────────────────────

/// Normalize a variable key for fuzzy matching: lowercase, strip non-alphanumerics.
/// "COSMOS_ACCOUNT_NAME", "cosmos-account-name", "cosmosAccountName" all
/// collapse to "cosmosaccountname".
fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

pub fn infer_env_from_group(group: &VarGroup) -> EnvTarget {
    let lookup = |canonicals: &[&str]| -> Option<String> {
        let needles: Vec<String> = canonicals.iter().map(|s| norm(s)).collect();
        for (k, v) in group.variables.iter() {
            if v.is_empty() {
                continue;
            }
            let nk = norm(k);
            if needles.iter().any(|n| *n == nk) {
                return Some(v.clone());
            }
        }
        None
    };
    EnvTarget {
        subscription: lookup(&[
            "subscriptionid",
            "azuresubscriptionid",
            "subscription",
            "armsubscriptionid",
            "subid",
        ])
        .unwrap_or_default(),
        resource_group: lookup(&[
            "resourcegroup",
            "azureresourcegroup",
            "resourcegroupname",
            "rg",
            "rgname",
        ])
        .unwrap_or_default(),
        cosmos_account: lookup(&[
            "cosmosaccountname",
            "cosmosdbaccountname",
            "cosmosdbaccount",
            "cosmosaccount",
            "cosmosname",
            "cosmosdbname",
            "cosmosdbsqlaccountname",
            "cosmosdbsqlaccount",
        ]),
        key_vault: lookup(&[
            "keyvaultname",
            "kvname",
            "keyvault",
            "azurekeyvaultname",
            "keyvaultaccountname",
        ]),
    }
}

/// Return the list of variable keys present in a group, for showing the user
/// what's available when inference fails.
pub fn group_variable_keys(group: &VarGroup) -> Vec<String> {
    group.variables.keys().cloned().collect()
}

// ── Az shell helpers ──────────────────────────────────────────────────────────

fn run_az_json(args: &[&str]) -> Result<Value, AzError> {
    let out = az_command(args)
        .output()
        .map_err(|e| AzError::Other(format!("az not found: {}", e)))?;
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() {
        if stderr.contains("AADSTS")
            || stderr.contains("az login")
            || stderr.contains("refresh token")
        {
            return Err(AzError::NotLoggedIn);
        }
        return Err(AzError::Other(stderr.trim().to_string()));
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    if raw.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&raw).map_err(|e| AzError::Other(format!("parse error: {}", e)))
}

fn cosmos_sql_role_name(id: &str) -> Option<&'static str> {
    let tail = id.rsplit('/').next().unwrap_or(id);
    match tail {
        "00000000-0000-0000-0000-000000000001" => Some("Cosmos DB Built-in Data Reader"),
        "00000000-0000-0000-0000-000000000002" => Some("Cosmos DB Built-in Data Contributor"),
        _ => None,
    }
}

fn collect_role_assignments(arr: &Value) -> Vec<RoleAssignment> {
    let items = match arr.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    items
        .iter()
        .map(|item| RoleAssignment {
            principal_id: item["principalId"].as_str().unwrap_or("").to_string(),
            principal_type: item["principalType"].as_str().map(str::to_string),
            role_definition_id: item["roleDefinitionId"].as_str().unwrap_or("").to_string(),
            role_name: item["roleDefinitionName"].as_str().map(str::to_string),
            scope: item["scope"].as_str().unwrap_or("").to_string(),
        })
        .collect()
}

// ── Cosmos ────────────────────────────────────────────────────────────────────

pub fn fetch_cosmos_security(env: &EnvTarget) -> Result<CosmosSecurity, AzError> {
    let acct = env
        .cosmos_account
        .as_deref()
        .ok_or_else(|| AzError::Other("no cosmos account configured".into()))?;
    if env.subscription.is_empty() || env.resource_group.is_empty() {
        return Err(AzError::Other(
            "missing subscription or resource group".into(),
        ));
    }

    let mut sec = CosmosSecurity {
        account_name: acct.to_string(),
        ..Default::default()
    };

    let show = run_az_json(&[
        "cosmosdb",
        "show",
        "--name",
        acct,
        "--resource-group",
        &env.resource_group,
        "--subscription",
        &env.subscription,
        "-o",
        "json",
    ])?;

    sec.disable_local_auth = show["disableLocalAuth"].as_bool();
    sec.public_network_access = show["publicNetworkAccess"].as_str().map(str::to_string);
    sec.network_acl_bypass = show["networkAclBypass"].as_str().map(str::to_string);
    sec.key_metadata_write_enabled = show["disableKeyBasedMetadataWriteAccess"]
        .as_bool()
        .map(|b| !b);
    if let Some(arr) = show["ipRules"].as_array() {
        sec.ip_rules = arr
            .iter()
            .filter_map(|v| v["ipAddressOrRange"].as_str())
            .map(str::to_string)
            .collect();
    }
    if let Some(arr) = show["virtualNetworkRules"].as_array() {
        sec.vnet_rules = arr
            .iter()
            .filter_map(|v| v["id"].as_str())
            .map(str::to_string)
            .collect();
    }

    if let Ok(arr) = run_az_json(&[
        "cosmosdb",
        "sql",
        "role",
        "assignment",
        "list",
        "--account-name",
        acct,
        "--resource-group",
        &env.resource_group,
        "--subscription",
        &env.subscription,
        "-o",
        "json",
    ]) {
        if let Some(items) = arr.as_array() {
            for item in items {
                let role_id = item["roleDefinitionId"].as_str().unwrap_or("").to_string();
                let role_name = cosmos_sql_role_name(&role_id).map(str::to_string);
                sec.sql_role_assignments.push(RoleAssignment {
                    principal_id: item["principalId"].as_str().unwrap_or("").to_string(),
                    principal_type: None,
                    role_definition_id: role_id,
                    role_name,
                    scope: item["scope"].as_str().unwrap_or("").to_string(),
                });
            }
        }
    }

    let scope = format!(
        "/subscriptions/{}/resourceGroups/{}/providers/Microsoft.DocumentDB/databaseAccounts/{}",
        env.subscription, env.resource_group, acct
    );
    if let Ok(arr) = run_az_json(&[
        "role",
        "assignment",
        "list",
        "--scope",
        &scope,
        "--subscription",
        &env.subscription,
        "-o",
        "json",
    ]) {
        sec.arm_role_assignments = collect_role_assignments(&arr);
    }

    Ok(sec)
}

// ── Key Vault ─────────────────────────────────────────────────────────────────

pub fn fetch_keyvault_security(env: &EnvTarget) -> Result<KeyVaultSecurity, AzError> {
    let vault = env
        .key_vault
        .as_deref()
        .ok_or_else(|| AzError::Other("no key vault configured".into()))?;
    if env.subscription.is_empty() || env.resource_group.is_empty() {
        return Err(AzError::Other(
            "missing subscription or resource group".into(),
        ));
    }

    let mut kv = KeyVaultSecurity {
        vault_name: vault.to_string(),
        ..Default::default()
    };

    let show = run_az_json(&[
        "keyvault",
        "show",
        "--name",
        vault,
        "--resource-group",
        &env.resource_group,
        "--subscription",
        &env.subscription,
        "-o",
        "json",
    ])?;
    let p = &show["properties"];
    kv.enable_rbac_authorization = p["enableRbacAuthorization"].as_bool();
    kv.public_network_access = p["publicNetworkAccess"].as_str().map(str::to_string);
    kv.purge_protection = p["enablePurgeProtection"].as_bool();
    kv.soft_delete_retention_days = p["softDeleteRetentionInDays"].as_i64();
    if let Some(acls) = p["networkAcls"].as_object() {
        if let Some(arr) = acls.get("ipRules").and_then(|v| v.as_array()) {
            kv.ip_rules = arr
                .iter()
                .filter_map(|v| v["value"].as_str())
                .map(str::to_string)
                .collect();
        }
        if let Some(arr) = acls.get("virtualNetworkRules").and_then(|v| v.as_array()) {
            kv.vnet_rules = arr
                .iter()
                .filter_map(|v| v["id"].as_str())
                .map(str::to_string)
                .collect();
        }
    }
    if let Some(arr) = p["accessPolicies"].as_array() {
        for item in arr {
            let perms = &item["permissions"];
            let pull = |k: &str| -> Vec<String> {
                perms[k]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str())
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default()
            };
            kv.access_policies.push(AccessPolicy {
                object_id: item["objectId"].as_str().unwrap_or("").to_string(),
                permissions_keys: pull("keys"),
                permissions_secrets: pull("secrets"),
                permissions_certs: pull("certificates"),
            });
        }
    }

    let scope = format!(
        "/subscriptions/{}/resourceGroups/{}/providers/Microsoft.KeyVault/vaults/{}",
        env.subscription, env.resource_group, vault
    );
    if let Ok(arr) = run_az_json(&[
        "role",
        "assignment",
        "list",
        "--scope",
        &scope,
        "--subscription",
        &env.subscription,
        "-o",
        "json",
    ]) {
        kv.role_assignments = collect_role_assignments(&arr);
    }

    Ok(kv)
}

// ── Combined snapshot ─────────────────────────────────────────────────────────

pub fn fetch_security_snapshot(env_name: &str, env: &EnvTarget) -> SecuritySnapshot {
    let mut snap = SecuritySnapshot {
        env_name: env_name.to_string(),
        target: env.clone(),
        ..Default::default()
    };
    if env.cosmos_account.is_some() {
        match fetch_cosmos_security(env) {
            Ok(c) => snap.cosmos = Some(c),
            Err(e) => snap.cosmos_err = Some(format!("{:?}", e)),
        }
    }
    if env.key_vault.is_some() {
        match fetch_keyvault_security(env) {
            Ok(v) => snap.key_vault = Some(v),
            Err(e) => snap.key_vault_err = Some(format!("{:?}", e)),
        }
    }
    snap
}

// ── Principal resolution (lazy) ───────────────────────────────────────────────

/// Resolve a principal GUID to a display name by trying sp / user / group in turn.
/// Returns `Some(name)` on first hit. Quiet failure (returns None) is expected
/// when the caller lacks Directory.Read on the token.
#[allow(dead_code)] // kept for a future "resolve principal name" affordance
pub fn resolve_principal(principal_id: &str) -> Option<String> {
    let attempts: &[&[&str]] = &[
        &[
            "ad",
            "sp",
            "show",
            "--id",
            principal_id,
            "--query",
            "displayName",
            "-o",
            "tsv",
        ],
        &[
            "ad",
            "user",
            "show",
            "--id",
            principal_id,
            "--query",
            "displayName",
            "-o",
            "tsv",
        ],
        &[
            "ad",
            "group",
            "show",
            "--group",
            principal_id,
            "--query",
            "displayName",
            "-o",
            "tsv",
        ],
    ];
    for args in attempts {
        if let Ok(out) = az_command(args).output() {
            if out.status.success() {
                let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !name.is_empty() {
                    return Some(name);
                }
            }
        }
    }
    None
}
