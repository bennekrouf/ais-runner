use crate::services::{azure_cli, settings_file};
use std::collections::HashMap;
use std::fs;

#[derive(Debug, Clone, PartialEq)]
pub enum SetupStatus {
    MissingSettings,
    NeedsInitialization,
    /// AzureWebJobsStorage points to a remote Azure storage account instead of
    /// UseDevelopmentStorage=true — func will fail to start locally.
    RemoteStorage,
    /// Settings that need attention before local runs behave correctly.
    ///
    /// Both categories are reported together even though they have different
    /// fixes: they were previously two sequential early returns, so a project
    /// with blank values never heard about its absent keys until the blanks
    /// were filled and the banner advanced. Finding out about the second set
    /// one round-trip later is a bad way to learn about it.
    NeedsConfiguration {
        /// Present in local.settings.json but empty, or still holding a TODO.
        blank: Vec<String>,
        /// Referenced by connections.json with no local.settings.json entry at all.
        absent: Vec<String>,
    },
    Ready,
}

/// Settings the Logic Apps runtime reads directly, whether or not
/// connections.json mentions them. WEBSITE_SITE_NAME in particular gates the
/// FLOWLOOKUP entries written into Azurite table storage on startup — leave it
/// blank and every trigger answers "WorkflowNotFound".
const RUNTIME_REQUIRED_KEYS: &[&str] = &[
    "WEBSITE_SITE_NAME",
    "WORKFLOWS_SUBSCRIPTION_ID",
    "WORKFLOWS_RESOURCE_GROUP_NAME",
];

pub fn check_setup(dir: &str) -> SetupStatus {
    let p = crate::services::workflows::resolve_logic_apps_dir(dir);
    let settings_path = p.join("local.settings.json");
    let template_path = p.join("local.settings.json.template");

    if !settings_path.exists() {
        if template_path.exists() {
            return SetupStatus::NeedsInitialization;
        } else {
            return SetupStatus::MissingSettings;
        }
    }

    let settings_text = fs::read_to_string(&settings_path).unwrap_or_default();
    let settings: serde_json::Value = serde_json::from_str(&settings_text).unwrap_or_default();
    let vals = settings["Values"].as_object();

    // Auto-fix AzureWebJobsStorage pointing to a remote account — this blocks func start locally
    if let Some(v) = vals {
        if let Some(aws) = v.get("AzureWebJobsStorage").and_then(|v| v.as_str()) {
            if !aws.is_empty()
                && aws != "UseDevelopmentStorage=true"
                && (aws.contains("core.windows.net") || aws.contains("AccountName="))
            {
                if fix_remote_storage(dir).is_ok() {
                    return check_setup(dir); // re-check with corrected file
                }
                return SetupStatus::RemoteStorage; // fix failed, show banner
            }
        }
    }

    // Which settings actually matter is a question with an answer: a key is
    // needed when connections.json interpolates it, or when the Logic Apps
    // runtime reads it directly. The rule here used to be a case-sensitive
    // substring guess ("KEY", "CONNECTION", "SUBSCRIPTION", "siteName"), which
    // reported WORKFLOWS_SUBSCRIPTION_ID while staying silent about a blank
    // keyVault_VaultUri that connections.json genuinely depends on — so the
    // banner's count bore no relation to what would actually break at runtime.
    let conn_path = p.join("connections.json");
    let referenced: Vec<String> = if conn_path.exists() {
        let conn_text = fs::read_to_string(&conn_path).unwrap_or_default();
        let conn_json: serde_json::Value = serde_json::from_str(&conn_text).unwrap_or_default();
        // Scan for both @appsetting('key') and @{appsetting('key')} forms.
        let conn_str = conn_json.to_string();
        let mut refs: Vec<String> = Vec::new();
        for cap in regex::Regex::new(r"@\{?appsetting\('([^']+)'\)\}?")
            .unwrap()
            .captures_iter(&conn_str)
        {
            let key = cap[1].to_string();
            if !refs.contains(&key) {
                refs.push(key);
            }
        }
        refs
    } else {
        Vec::new()
    };

    // Named, not counted: a bare count sends the user hunting through the whole
    // file for which settings the banner means.
    let mut blank: Vec<String> = Vec::new();
    if let Some(v) = vals {
        for (key, val) in v {
            if let Some(s) = val.as_str() {
                // A TODO placeholder is the user's own note-to-self, so it
                // counts whether or not anything references it yet.
                let is_missing = s.contains("TODO")
                    || (s.is_empty()
                        && (referenced.contains(key)
                            || RUNTIME_REQUIRED_KEYS.contains(&key.as_str())));
                if is_missing {
                    blank.push(key.clone());
                }
            }
        }
    }
    blank.sort();

    // Referenced by connections.json with no local.settings.json entry at all.
    let mut absent: Vec<String> = referenced
        .iter()
        .filter(|k| vals.is_none_or(|v| !v.contains_key(k.as_str())))
        .cloned()
        .collect();
    absent.sort();

    if !blank.is_empty() || !absent.is_empty() {
        return SetupStatus::NeedsConfiguration { blank, absent };
    }

    SetupStatus::Ready
}

/// Render a key list for a one-line banner. Shows every key while the list is
/// short, and truncates past that so one badly configured project can't push
/// the banner's buttons off the edge of the window.
pub fn summarize_keys(keys: &[String]) -> String {
    const SHOWN: usize = 4;
    if keys.len() <= SHOWN {
        keys.join(", ")
    } else {
        format!("{}, +{} more", keys[..SHOWN].join(", "), keys.len() - SHOWN)
    }
}

/// Switch AzureWebJobsStorage from a remote connection string to UseDevelopmentStorage=true.
pub fn fix_remote_storage(dir: &str) -> Result<(), String> {
    let p = crate::services::workflows::resolve_logic_apps_dir(dir);
    let settings_path = p.join("local.settings.json");
    let text = fs::read_to_string(&settings_path).map_err(|e| e.to_string())?;
    let mut json: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    if let Some(vals) = json.get_mut("Values").and_then(|v| v.as_object_mut()) {
        vals.insert(
            "AzureWebJobsStorage".into(),
            serde_json::json!("UseDevelopmentStorage=true"),
        );
    }
    let out = serde_json::to_string_pretty(&json).map_err(|e| e.to_string())?;
    fs::write(&settings_path, out).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn initialize_from_template(dir: &str) -> Result<(), String> {
    let p = crate::services::workflows::resolve_logic_apps_dir(dir);
    let settings_path = p.join("local.settings.json");
    let template_path = p.join("local.settings.json.template");

    if !template_path.exists() {
        return Err("Template not found".into());
    }

    fs::copy(template_path, settings_path).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn initialize_default(dir: &str) -> Result<(), String> {
    let p = crate::services::workflows::resolve_logic_apps_dir(dir);

    // Ensure the target directory exists
    if !p.exists() {
        fs::create_dir_all(&p).map_err(|e| format!("Cannot create {}: {}", p.display(), e))?;
    }

    // 1. local.settings.json
    let settings_path = p.join("local.settings.json");
    let settings_content = r#"{
  "IsEncrypted": false,
  "Values": {
    "AzureWebJobsStorage": "UseDevelopmentStorage=true",
    "AzureWebJobsSecretStorageType": "Files",
    "FUNCTIONS_WORKER_RUNTIME": "node",
    "WEBSITE_SITE_NAME": "",
    "WORKFLOWS_SUBSCRIPTION_ID": "",
    "WORKFLOWS_RESOURCE_GROUP_NAME": "",
    "WORKFLOWS_LOCATION_NAME": "switzerlandnorth"
  }
}"#;
    if !settings_path.exists() {
        fs::write(settings_path, settings_content).map_err(|e| e.to_string())?;
    }

    // 2. package.json (required by Node worker)
    let pkg_path = p.join("package.json");
    let pkg_content = r#"{
  "name": "logic-apps",
  "version": "1.0.0",
  "dependencies": {}
}"#;
    if !pkg_path.exists() {
        fs::write(pkg_path, pkg_content).map_err(|e| e.to_string())?;
    }

    // 3. Fix connections.json if present:
    //    a) @{appsetting('key')} → @appsetting('key')  (ARM-template syntax rejected locally)
    //    b) MSI AzureBlob connections → connectionString  (IMDS not available on dev machines)
    let conn_path = p.join("connections.json");
    if conn_path.exists() {
        if let Ok(raw) = fs::read_to_string(&conn_path) {
            let syntax_fixed = fix_connections_json(&raw);
            let fully_fixed = patch_connections_for_local(&syntax_fixed);
            if fully_fixed != raw {
                fs::write(&conn_path, &fully_fixed).map_err(|e| e.to_string())?;
            }
        }
    }

    Ok(())
}

/// Patch `connections.json` for local development:
///
/// - AzureBlob MSI → connectionString pointing at AzureWebJobsStorage (Azurite)
/// - ServiceBus MSI → connectionString pointing at `serviceBus_connectionString`
///   (populated with the emulator connection string by the SB emulator start handler)
/// - SQL MSI → connectionString pointing at `<name>_connectionString`
///   (MSI yields `Login failed for user ''` locally — no IMDS to get a token from)
///
/// MSI (`parameterSetName: "ManagedServiceIdentity"`) requires the Azure IMDS
/// endpoint which does not exist on a developer machine.
pub fn patch_connections_for_local(raw: &str) -> String {
    let Ok(mut root) = serde_json::from_str::<serde_json::Value>(raw) else {
        return raw.to_string();
    };

    let Some(svc) = root["serviceProviderConnections"].as_object_mut() else {
        return serde_json::to_string_pretty(&root).unwrap_or_else(|_| raw.to_string());
    };

    let keys: Vec<String> = svc.keys().cloned().collect();
    for name in keys {
        let conn = &svc[&name];
        let provider_id = conn["serviceProvider"]["id"].as_str().unwrap_or("");
        let is_msi = conn["parameterSetName"]
            .as_str()
            .map(|p| p == "ManagedServiceIdentity")
            .unwrap_or(false);

        if is_msi && provider_id == "/serviceProviders/AzureBlob" {
            // All local blob connections must share the SAME connection key
            // (AzureWebJobsStorage).  If each connection uses its own key
            // (IgniteBlob_connectionString, KyribaBlob_connectionString, …)
            // the Functions runtime registers a separate ListenerFactoryContext
            // per key and, because they all resolve to the same Azurite account,
            // the DI container ends up with services.Count=N, instances.Count=0
            // → "Script host in error state: Mismatch detected for type
            // ListenerFactoryContext".  One shared key = one context = no crash.
            let entry = svc.get_mut(&name).unwrap();
            *entry = serde_json::json!({
                "displayName": entry["displayName"].clone(),
                "parameterSetName": "connectionString",
                "parameterValues": {
                    "connectionString": "@appsetting('AzureWebJobsStorage')"
                },
                "serviceProvider": {
                    "id": "/serviceProviders/AzureBlob"
                }
            });
        } else if is_msi && provider_id == "/serviceProviders/sql" {
            // MSI against SQL cannot work locally: there is no Azure IMDS
            // endpoint, so the driver authenticates with no credentials and the
            // server rejects it with `Login failed for user ''`. Switch to
            // connection-string auth against the local SQL emulator.
            let entry = svc.get_mut(&name).unwrap();
            *entry = serde_json::json!({
                "displayName": entry["displayName"].clone(),
                "parameterSetName": "connectionString",
                "parameterValues": {
                    "connectionString": format!("@appsetting('{name}_connectionString')")
                },
                "serviceProvider": {
                    "id": "/serviceProviders/sql"
                }
            });
        } else if is_msi
            && (provider_id.to_lowercase().contains("cosmos")
                || provider_id.to_lowercase().contains("documentdb"))
        {
            // MSI against Cosmos can't work locally (no IMDS). Point at the
            // local Cosmos emulator via a per-connection connection-string key.
            let entry = svc.get_mut(&name).unwrap();
            let pid = entry["serviceProvider"]["id"].clone();
            *entry = serde_json::json!({
                "displayName": entry["displayName"].clone(),
                "parameterSetName": "connectionString",
                "parameterValues": {
                    "connectionString": format!("@appsetting('{name}_connectionString')")
                },
                "serviceProvider": { "id": pid }
            });
        } else if is_msi && provider_id == "/serviceProviders/serviceBus" {
            // Switch Service Bus from MSI (requires Azure IMDS) to connectionString
            // so the local emulator can be used.  The emulator start handler writes
            // the emulator connection string to `serviceBus_connectionString` in
            // local.settings.json.
            let entry = svc.get_mut(&name).unwrap();
            *entry = serde_json::json!({
                "displayName": entry["displayName"].clone(),
                "parameterSetName": "connectionString",
                "parameterValues": {
                    "connectionString": "@appsetting('serviceBus_connectionString')"
                },
                "serviceProvider": {
                    "id": "/serviceProviders/serviceBus"
                }
            });
        }
    }

    serde_json::to_string_pretty(&root).unwrap_or_else(|_| raw.to_string())
}

/// Convert `@{appsetting('key')}` → `@appsetting('key')` throughout connections.json.
///
/// Azure Portal / VS Code sometimes exports connection references using ARM-template
/// interpolation syntax (`@{...}`).  The Logic Apps Standard local runtime only
/// understands the expression form without braces.  Mixed files cause the entire
/// `functionConnections` or `managedApiConnections` section to fail to parse.
pub fn fix_connections_json(raw: &str) -> String {
    // Validate it's JSON first; if not, return unchanged.
    if serde_json::from_str::<serde_json::Value>(raw).is_err() {
        return raw.to_string();
    }
    let fixed = regex::Regex::new(r"@\{appsetting\(([^)]+)\)\}")
        .unwrap()
        .replace_all(raw, "@appsetting($1)")
        .to_string();
    fixed
}

/// Attempts to auto-discover SB namespace and SQL server in the given resource group
/// and updates local.settings.json with the values.
///
/// `logic_app_name` is written to `WEBSITE_SITE_NAME` — the Logic Apps runtime needs this
/// to be non-empty so it can write FLOWLOOKUP entries into Azurite table storage on startup.
/// Without it all workflow triggers return "WorkflowNotFound".
pub fn auto_detect_resources(
    dir: &str,
    subscription_id: Option<&str>,
    resource_group: &str,
    logic_app_name: Option<&str>,
) -> Result<String, String> {
    let mut messages = Vec::new();

    // 0. Update identity fields: subscription, resource group, and site name.
    //    WEBSITE_SITE_NAME and WORKFLOWS_RESOURCE_GROUP_NAME *must* be non-empty for the
    //    Logic Apps runtime to write FLOWLOOKUP entries into Azurite on startup.
    if let Ok(text) = settings_file::read_local_settings(dir) {
        if let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(vals) = root["Values"].as_object_mut() {
                let mut changed = false;

                if let Some(sub_id) = subscription_id {
                    let key = "WORKFLOWS_SUBSCRIPTION_ID";
                    if vals
                        .get(key)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .is_empty()
                    {
                        vals.insert(
                            key.to_string(),
                            serde_json::Value::String(sub_id.to_string()),
                        );
                        messages.push(format!("✅ Set subscription: {}", sub_id));
                        changed = true;
                    }
                }

                {
                    let key = "WORKFLOWS_RESOURCE_GROUP_NAME";
                    if vals
                        .get(key)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .is_empty()
                        && !resource_group.is_empty()
                    {
                        vals.insert(
                            key.to_string(),
                            serde_json::Value::String(resource_group.to_string()),
                        );
                        messages.push(format!("✅ Set resource group: {}", resource_group));
                        changed = true;
                    }
                }

                if let Some(name) = logic_app_name {
                    if !name.is_empty() {
                        let key = "WEBSITE_SITE_NAME";
                        if vals
                            .get(key)
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .is_empty()
                        {
                            vals.insert(
                                key.to_string(),
                                serde_json::Value::String(name.to_string()),
                            );
                            messages
                                .push(format!("✅ Set site name (WEBSITE_SITE_NAME): {}", name));
                            changed = true;
                        }
                    }
                }

                if changed {
                    let new_text = serde_json::to_string_pretty(&root).unwrap_or_default();
                    let _ = settings_file::write_local_settings(dir, &new_text);
                }
            }
        }
    }

    // 1. Service Bus
    match azure_cli::sb_list_namespaces() {
        Ok(list) => {
            let matches: Vec<_> = list
                .iter()
                .filter(|(_, _, rg)| rg.to_lowercase() == resource_group.to_lowercase())
                .collect();

            if matches.len() == 1 {
                let fqdn = &matches[0].1;
                if let Ok(text) = settings_file::read_local_settings(dir) {
                    if let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Some(vals) = root["Values"].as_object_mut() {
                            // Find the key used in connections.json for SB
                            let sb_key = "serviceBus_fullyQualifiedNamespace";
                            vals.insert(
                                sb_key.to_string(),
                                serde_json::Value::String(fqdn.clone()),
                            );

                            let new_text = serde_json::to_string_pretty(&root).unwrap_or_default();
                            let _ = settings_file::write_local_settings(dir, &new_text);
                            messages.push(format!("✅ Auto-detected SB namespace: {}", fqdn));
                        }
                    }
                }
            } else if matches.len() > 1 {
                messages.push(format!(
                    "ℹ Found {} SB namespaces in RG — please pick one manually.",
                    matches.len()
                ));
            }
        }
        Err(e) => {
            messages.push(format!("❌ Failed to list SB namespaces: {:?}", e));
        }
    }

    // Fix connections.json: syntax + MSI → connectionString for blob connections
    let conn_path =
        crate::services::workflows::resolve_logic_apps_dir(dir).join("connections.json");
    if conn_path.exists() {
        if let Ok(raw) = std::fs::read_to_string(&conn_path) {
            let fixed = fix_connections_json(&raw);
            let fixed = patch_connections_for_local(&fixed);
            if fixed != raw {
                let _ = std::fs::write(&conn_path, &fixed);
                // Report what changed
                let syntax_changed = fix_connections_json(&raw) != raw;
                let msi_changed = patch_connections_for_local(&fix_connections_json(&raw))
                    != fix_connections_json(&raw);
                if syntax_changed {
                    messages.push("✅ Fixed connections.json: @{appsetting(...)} → @appsetting(...) (ARM-template syntax not supported locally)".into());
                }
                if msi_changed {
                    messages.push("✅ Patched connections.json: AzureBlob MSI → connectionString (Azurite does not support MSI auth)".into());
                }
            }
        }
    }

    if messages.is_empty() {
        Ok("No resources found to auto-detect.".into())
    } else {
        Ok(messages.join("\n"))
    }
}

/// Writes smart defaults for every key in `missing_keys` that isn't already in local.settings.json.
/// Idempotent — never overwrites existing values.
pub fn stub_missing_keys(dir: &str, missing_keys: &[String]) -> Result<(), String> {
    // The existing settings are read so a SQL stub can pick up the database from
    // its sibling `*_databaseName` key rather than defaulting to `master`.
    let settings: serde_json::Value = crate::services::settings_file::read_local_settings(dir)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(serde_json::Value::Null);
    let stubs: HashMap<String, String> = missing_keys
        .iter()
        .map(|k| (k.clone(), smart_default_for(k, &settings)))
        .collect();
    apply_settings(dir, stubs)
}

/// [`smart_default`], but able to consult the rest of `local.settings.json`.
///
/// Only SQL connection strings differ: pointing them at `master` opens a
/// connection that then fails every table and stored-procedure lookup, and the
/// project almost always names the real database in a sibling setting.
pub fn smart_default_for(key: &str, settings: &serde_json::Value) -> String {
    let is_sql_conn =
        key.to_uppercase().contains("SQL") && key.to_uppercase().contains("CONNECTION");
    if is_sql_conn {
        if let Some(db) = crate::services::run_readiness::sibling_database(key, settings) {
            return local_sql_connection(&db);
        }
    }
    smart_default(key)
}

/// Connection string for the bundled SQL Edge emulator, aimed at `database`.
fn local_sql_connection(database: &str) -> String {
    format!(
        "Server=localhost,{};Database={};User Id=sa;Password={};\
         Encrypt=false;TrustServerCertificate=true;",
        crate::handlers::sql_emulator::SQL_PORT,
        database,
        crate::handlers::sql_emulator::SA_PASSWORD,
    )
}

/// Returns a sensible local-dev default for a known key pattern, or empty string.
///
/// Rules discovered by comparing a working project (ais_platform) against a broken one
/// (ais_tom_platform) and tracing every func-startup validation failure:
///
/// 1. `*_blobStorageEndpoint` → Azurite blob endpoint
/// 2. `azureFunction_*_appKey` → `"placeholder"` (empty string causes functionConnections parse error)
/// 3. `azureFunction_*_triggerUrl` → inferred local Java-func URL
/// 4. `*_connectionUrl` → APIM-formatted placeholder URL containing the api name so the
///    Logic Apps runtime validator can extract api-name + connection-name from the path.
///    Format: `https://placeholder.azure-apim.net/apim/{api-name}/placeholder/`
///    Empty string = "missing required property"; `https://placeholder.logic.azure.com` = "invalid
///    connection runtime url — api name and connection name should not be null or empty".
pub fn smart_default(key: &str) -> String {
    const AZURITE: &str = "http://127.0.0.1:10000/devstoreaccount1";

    match key {
        // ── Blob endpoints ────────────────────────────────────────────────────
        "AzureBlob_blobStorageEndpoint"    => AZURITE.into(),
        "IgniteBlob_blobStorageEndpoint"   => AZURITE.into(),
        "KyribaBlob_blobStorageEndpoint"   => AZURITE.into(),
        "VentriksBlob_blobStorageEndpoint" => AZURITE.into(),
        // Blob connection strings — used when parameterSetName is "connectionString".
        // MSI (parameterSetName "ManagedServiceIdentity") cannot authenticate to Azurite
        // locally because there is no Azure Instance Metadata Service endpoint.
        // Connection string auth works in both Azurite and Azure.
        "AzureBlob_connectionString"    => "UseDevelopmentStorage=true".into(),
        "IgniteBlob_connectionString"   => "UseDevelopmentStorage=true".into(),
        "KyribaBlob_connectionString"   => "UseDevelopmentStorage=true".into(),
        "VentriksBlob_connectionString" => "UseDevelopmentStorage=true".into(),

        // ── Standard func-host settings ───────────────────────────────────────
        "AzureWebJobsStorage"      => "UseDevelopmentStorage=true".into(),
        "FUNCTIONS_WORKER_RUNTIME" => "node".into(),
        "WORKFLOWS_LOCATION_NAME"  => "switzerlandnorth".into(),

        // ── Service Bus ───────────────────────────────────────────────────────
        // `serviceBus_connectionString` is the key written by patch_connections_for_local
        // when it switches the serviceBus connection from MSI to connectionString.
        // Pre-populate it with the emulator connection string so func starts cleanly
        // without needing a real Azure namespace — the SB emulator start handler will
        // overwrite it with the same value when the emulator becomes ready.
        "serviceBus_connectionString" => {
            crate::handlers::sb_emulator::EMULATOR_CONN_STR.into()
        }
        // Generic Service Bus connection string keys — same emulator default.
        k if k.to_uppercase().contains("SERVICE_BUS") && k.to_uppercase().contains("CONNECTION") => {
            crate::handlers::sb_emulator::EMULATOR_CONN_STR.into()
        }

        // ── SQL Server ────────────────────────────────────────────────────────
        // Local-only: point at the bundled SQL Edge emulator (reachable), NOT a
        // cloud-shaped placeholder. Database=master always exists so the
        // connection opens even before the workflow's own DB is created.
        // `master` is the fallback only — it always exists, so the connection
        // opens even when the project names no database anywhere. Prefer
        // `smart_default_for`, which reads the sibling `*_databaseName` and
        // aims at the database the workflow's objects actually live in.
        k if k.to_uppercase().contains("SQL") && k.to_uppercase().contains("CONNECTION") => {
            local_sql_connection("master")
        }

        // ── Cosmos DB ───────────────────────────────────────────────────────────
        // Local-only: point at the bundled Cosmos emulator (reachable).
        k if k.to_uppercase().contains("COSMOS") && k.to_uppercase().contains("CONNECTION") => {
            crate::handlers::cosmos_emulator::local_connection_string()
        }
        k if k.to_uppercase().contains("COSMOS")
            && (k.to_uppercase().contains("ENDPOINT") || k.to_uppercase().contains("URL")) =>
        {
            format!("http://localhost:{}", crate::handlers::cosmos_emulator::COSMOS_API_PORT)
        }

        // ── Java function connections ─────────────────────────────────────────
        // appKey MUST be non-empty ("placeholder") — empty string triggers a
        // functionConnections parse error that blocks ALL workflow registrations.
        k if k.starts_with("azureFunction_") && k.ends_with("_appKey") => {
            "placeholder".into()
        }

        // triggerUrl — infer from key name: "azureFunction_{Name}_triggerUrl"
        k if k.starts_with("azureFunction_") && k.ends_with("_triggerUrl") => {
            let name = k
                .strip_prefix("azureFunction_").unwrap_or(k)
                .strip_suffix("_triggerUrl").unwrap_or(k);
            format!("http://localhost:7072/api/{}", name)
        }

        // Base URL of the project's own function app, referenced straight from
        // a workflow's HTTP action rather than through connections.json —
        // e.g. `@concat(appsetting('AIS_Functions_BaseUrl'), '/api/Convert…')`.
        // Same host as the triggerUrl keys above; only the shape differs.
        k if k.ends_with("_Functions_BaseUrl") || k == "Functions_BaseUrl" => {
            "http://localhost:7072".into()
        }
        // Paired function key. Locally the functions are AuthorizationLevel
        // ANONYMOUS, so any non-empty value works — but empty would send a
        // blank `x-functions-key` header, which some hosts reject outright.
        k if k.ends_with("_Functions_Key") || k == "Functions_Key" => {
            "placeholder".into()
        }

        // ── Teams routing ─────────────────────────────────────────────────────
        // Real channel/group ids are per-tenant and meaningless locally, but a
        // blank makes every routing branch resolve to the same empty string —
        // so a routing rule that picks the wrong channel looks identical to one
        // that picks the right one. A distinct derived value per key keeps the
        // resolution visible and testable without touching the real tenant.
        "teams:groupId" => "local-teams-group".into(),
        k if k.starts_with("teams:channel:") => {
            format!("local-channel-{}", k.trim_start_matches("teams:channel:").replace(':', "-"))
        }

        // ── Managed API connection URLs ────────────────────────────────────────
        // Must be a valid APIM-style URL: https://…/apim/{api-name}/{conn-name}/
        // The Logic Apps runtime parses this URL to extract api-name and conn-name.
        // A generic placeholder like "https://placeholder.logic.azure.com" fails
        // validation with "api name and connection name should not be null or empty".
        "Teams_connectionUrl" =>
            "https://placeholder.azure-apim.net/apim/teams/teams-local/".into(),
        "LogAnalytics_connectionUrl" =>
            "https://placeholder.azure-apim.net/apim/azureloganalyticsdatacollector/loganalytics-local/".into(),
        "Sharepoint_connectionUrl" =>
            "https://placeholder.azure-apim.net/apim/sharepointonline/sharepoint-local/".into(),
        "Office365_connectionUrl" =>
            "https://placeholder.azure-apim.net/apim/office365/office365-local/".into(),
        "Salesforce_connectionUrl" =>
            "https://placeholder.azure-apim.net/apim/salesforce/salesforce-local/".into(),
        "AcsEmail_connectionUrl" =>
            "https://placeholder.azure-apim.net/apim/acsemail/acsemail-local/".into(),
        // Generic pattern: if the key ends in _connectionUrl, infer the api name
        k if k.ends_with("_connectionUrl") => {
            let api = k.strip_suffix("_connectionUrl").unwrap_or(k).to_lowercase();
            // Strip common suffixes to get the api name
            let api = api.strip_suffix("_url").unwrap_or(&api);
            format!("https://placeholder.azure-apim.net/apim/{}/{}-local/", api, api)
        }

        // ── Generic connection string catch-all ────────────────────────────────
        // Any empty connection string (regardless of type) crashes Azurite table
        // init for ALL workflows.  If we reach here no specific rule matched, so
        // return a syntactically-valid placeholder that any connection string
        // parser can tokenise without throwing.  The workflow will be unhealthy
        // at runtime but it will NOT block unrelated workflows from recording runs.
        k if k.to_lowercase().contains("connection") => {
            "Server=tcp:placeholder.local;Initial Catalog=placeholder;\
             User ID=placeholder;Password=placeholder;".into()
        }

        // ── Everything else ────────────────────────────────────────────────────
        _ => String::new(),
    }
}

pub fn apply_settings(dir: &str, updates: HashMap<String, String>) -> Result<(), String> {
    let text = settings_file::read_local_settings(dir)?;
    let mut root: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;

    if let Some(vals) = root["Values"].as_object_mut() {
        for (k, v) in updates {
            vals.insert(k, serde_json::Value::String(v));
        }
    }

    let new_text = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    settings_file::write_local_settings(dir, &new_text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a throwaway project. `settings` are the `Values` entries;
    /// `connections` is the raw connections.json, or None to omit the file.
    fn project(settings: &[(&str, &str)], connections: Option<&str>) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let vals: serde_json::Map<String, serde_json::Value> = settings
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
            .collect();
        let root = serde_json::json!({ "IsEncrypted": false, "Values": vals });
        std::fs::write(
            tmp.path().join("local.settings.json"),
            serde_json::to_string_pretty(&root).unwrap(),
        )
        .unwrap();
        if let Some(c) = connections {
            std::fs::write(tmp.path().join("connections.json"), c).unwrap();
        }
        tmp
    }

    #[test]
    fn blank_settings_are_named_not_just_counted() {
        let tmp = project(
            &[
                ("WEBSITE_SITE_NAME", ""),
                ("WORKFLOWS_SUBSCRIPTION_ID", ""),
                ("FUNCTIONS_WORKER_RUNTIME", "node"),
                ("AzureWebJobsStorage", "UseDevelopmentStorage=true"),
            ],
            None,
        );

        match check_setup(tmp.path().to_str().unwrap()) {
            SetupStatus::NeedsConfiguration { blank, absent } => {
                assert_eq!(
                    blank,
                    vec!["WEBSITE_SITE_NAME", "WORKFLOWS_SUBSCRIPTION_ID"]
                );
                assert!(absent.is_empty());
            }
            other => panic!("expected NeedsConfiguration, got {:?}", other),
        }
    }

    #[test]
    fn blank_values_and_absent_keys_are_reported_in_one_pass() {
        // The regression this guards: these used to be two sequential early
        // returns, so the absent keys stayed invisible until the blanks were
        // filled in and the check was re-run.
        let conns = r#"{
            "managedApiConnections": {
                "teams": {
                    "connection": { "id": "/subscriptions/@appsetting('WORKFLOWS_SUBSCRIPTION_ID')/x" },
                    "connectionRuntimeUrl": "@appsetting('Teams_connectionUrl')"
                }
            }
        }"#;
        let tmp = project(
            &[
                ("WORKFLOWS_SUBSCRIPTION_ID", ""),
                ("AzureWebJobsStorage", "UseDevelopmentStorage=true"),
            ],
            Some(conns),
        );

        match check_setup(tmp.path().to_str().unwrap()) {
            SetupStatus::NeedsConfiguration { blank, absent } => {
                assert_eq!(blank, vec!["WORKFLOWS_SUBSCRIPTION_ID"]);
                // Referenced by connections.json, no entry in Values at all.
                assert_eq!(absent, vec!["Teams_connectionUrl"]);
            }
            other => panic!("expected both categories, got {:?}", other),
        }
    }

    #[test]
    fn a_blank_key_that_connections_json_needs_is_reported_whatever_its_name() {
        // The old rule matched key names against uppercase substrings, so
        // keyVault_VaultUri and serviceBus_fullyQualifiedNamespace slipped
        // through while WORKFLOWS_SUBSCRIPTION_ID was flagged — the banner
        // undercounted exactly the settings that break connections at runtime.
        let conns = r#"{
            "managedApiConnections": {
                "kv":  { "connectionRuntimeUrl": "@appsetting('keyVault_VaultUri')" },
                "sb":  { "connectionRuntimeUrl": "@{appsetting('serviceBus_fullyQualifiedNamespace')}" }
            }
        }"#;
        let tmp = project(
            &[
                ("keyVault_VaultUri", ""),
                ("serviceBus_fullyQualifiedNamespace", ""),
                ("WEBSITE_SITE_NAME", "ais-tom"),
                ("AzureWebJobsStorage", "UseDevelopmentStorage=true"),
            ],
            Some(conns),
        );

        match check_setup(tmp.path().to_str().unwrap()) {
            SetupStatus::NeedsConfiguration { blank, absent } => {
                assert_eq!(
                    blank,
                    vec!["keyVault_VaultUri", "serviceBus_fullyQualifiedNamespace"]
                );
                assert!(absent.is_empty());
            }
            other => panic!("expected NeedsConfiguration, got {:?}", other),
        }
    }

    #[test]
    fn a_blank_key_nothing_references_is_left_alone() {
        // The flip side: an unused blank entry is not a problem to nag about.
        let conns = r#"{ "managedApiConnections": {} }"#;
        let tmp = project(
            &[
                ("someUnusedThing", ""),
                ("WEBSITE_SITE_NAME", "ais-tom"),
                ("WORKFLOWS_SUBSCRIPTION_ID", "sub-1"),
                ("WORKFLOWS_RESOURCE_GROUP_NAME", "rg-1"),
                ("AzureWebJobsStorage", "UseDevelopmentStorage=true"),
            ],
            Some(conns),
        );
        assert!(matches!(
            check_setup(tmp.path().to_str().unwrap()),
            SetupStatus::Ready
        ));
    }

    #[test]
    fn a_fully_configured_project_is_ready() {
        let conns = r#"{ "managedApiConnections": { "teams": {
            "connectionRuntimeUrl": "@appsetting('Teams_connectionUrl')" } } }"#;
        let tmp = project(
            &[
                ("WEBSITE_SITE_NAME", "ais-tom"),
                ("Teams_connectionUrl", "https://example.invalid/teams"),
                ("AzureWebJobsStorage", "UseDevelopmentStorage=true"),
            ],
            Some(conns),
        );
        assert!(matches!(
            check_setup(tmp.path().to_str().unwrap()),
            SetupStatus::Ready
        ));
    }

    #[test]
    fn a_todo_placeholder_counts_as_blank_whatever_the_key_is_called() {
        let tmp = project(
            &[
                ("SomeRandomSetting", "TODO: fill me in"),
                ("AzureWebJobsStorage", "UseDevelopmentStorage=true"),
            ],
            None,
        );
        match check_setup(tmp.path().to_str().unwrap()) {
            SetupStatus::NeedsConfiguration { blank, .. } => {
                assert_eq!(blank, vec!["SomeRandomSetting"]);
            }
            other => panic!("expected NeedsConfiguration, got {:?}", other),
        }
    }

    #[test]
    fn empty_values_only_count_for_keys_that_must_be_set() {
        // An empty setting whose name matches none of the patterns is a
        // deliberate blank, not a misconfiguration.
        let tmp = project(
            &[
                ("SomeOptionalFlag", ""),
                ("AzureWebJobsStorage", "UseDevelopmentStorage=true"),
            ],
            None,
        );
        assert!(matches!(
            check_setup(tmp.path().to_str().unwrap()),
            SetupStatus::Ready
        ));
    }

    #[test]
    fn key_summary_lists_short_runs_and_truncates_long_ones() {
        let three: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(summarize_keys(&three), "a, b, c");

        let six: Vec<String> = ["a", "b", "c", "d", "e", "f"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(summarize_keys(&six), "a, b, c, d, +2 more");

        assert_eq!(summarize_keys(&[]), "");
    }

    const MSI_CONNECTIONS: &str = r#"{
        "serviceProviderConnections": {
            "IgniteBlob": {
                "displayName": "IgniteBlob",
                "parameterSetName": "ManagedServiceIdentity",
                "parameterValues": {
                    "authProvider": { "Type": "ManagedServiceIdentity" },
                    "blobStorageEndpoint": "@appsetting('IgniteBlob_blobStorageEndpoint')"
                },
                "serviceProvider": { "id": "/serviceProviders/AzureBlob" }
            },
            "KyribaBlob": {
                "displayName": "KyribaBlob",
                "parameterSetName": "ManagedServiceIdentity",
                "parameterValues": {
                    "authProvider": { "Type": "ManagedServiceIdentity" },
                    "blobStorageEndpoint": "@appsetting('KyribaBlob_blobStorageEndpoint')"
                },
                "serviceProvider": { "id": "/serviceProviders/AzureBlob" }
            },
            "serviceBus": {
                "displayName": "serviceBus",
                "parameterSetName": "ManagedServiceIdentity",
                "parameterValues": {
                    "authProvider": { "Type": "ManagedServiceIdentity" },
                    "fullyQualifiedNamespace": "@appsetting('serviceBus_fullyQualifiedNamespace')"
                },
                "serviceProvider": { "id": "/serviceProviders/serviceBus" }
            }
        }
    }"#;

    #[test]
    fn patch_blob_msi_to_connection_string() {
        let patched = patch_connections_for_local(MSI_CONNECTIONS);
        let v: serde_json::Value = serde_json::from_str(&patched).unwrap();

        // IgniteBlob: switched to connectionString pointing at AzureWebJobsStorage
        let ignite = &v["serviceProviderConnections"]["IgniteBlob"];
        assert_eq!(ignite["parameterSetName"], "connectionString");
        assert_eq!(
            ignite["parameterValues"]["connectionString"],
            "@appsetting('AzureWebJobsStorage')"
        );
        assert!(
            ignite["parameterValues"]["authProvider"].is_null(),
            "authProvider should be removed"
        );
        assert!(
            ignite["parameterValues"]["blobStorageEndpoint"].is_null(),
            "blobStorageEndpoint should be removed"
        );

        // KyribaBlob: also points at AzureWebJobsStorage (same key avoids ListenerFactoryContext clash)
        let kyriba = &v["serviceProviderConnections"]["KyribaBlob"];
        assert_eq!(kyriba["parameterSetName"], "connectionString");
        assert_eq!(
            kyriba["parameterValues"]["connectionString"],
            "@appsetting('AzureWebJobsStorage')"
        );
    }

    #[test]
    fn patch_service_bus_msi_to_connection_string() {
        let patched = patch_connections_for_local(MSI_CONNECTIONS);
        let v: serde_json::Value = serde_json::from_str(&patched).unwrap();

        // serviceBus: MSI → connectionString pointing at serviceBus_connectionString
        let sb = &v["serviceProviderConnections"]["serviceBus"];
        assert_eq!(sb["parameterSetName"], "connectionString");
        assert_eq!(
            sb["parameterValues"]["connectionString"],
            "@appsetting('serviceBus_connectionString')"
        );
        assert!(
            sb["parameterValues"]["fullyQualifiedNamespace"].is_null(),
            "fullyQualifiedNamespace should be removed"
        );
        assert!(
            sb["parameterValues"]["authProvider"].is_null(),
            "authProvider should be removed"
        );
    }

    #[test]
    fn patch_idempotent_on_already_connection_string() {
        let already_cs = r#"{
            "serviceProviderConnections": {
                "IgniteBlob": {
                    "displayName": "IgniteBlob",
                    "parameterSetName": "connectionString",
                    "parameterValues": {
                        "connectionString": "@appsetting('AzureWebJobsStorage')"
                    },
                    "serviceProvider": { "id": "/serviceProviders/AzureBlob" }
                }
            }
        }"#;
        let patched = patch_connections_for_local(already_cs);
        let v: serde_json::Value = serde_json::from_str(&patched).unwrap();
        assert_eq!(
            v["serviceProviderConnections"]["IgniteBlob"]["parameterSetName"],
            "connectionString"
        );
    }
}

#[cfg(test)]
mod sql_msi_local_tests {
    use super::*;

    #[test]
    fn msi_sql_is_switched_to_connection_string() {
        // MSI against SQL yields `Login failed for user ''` locally (no IMDS).
        let raw = r#"{
            "serviceProviderConnections": {
                "sql-server-ais": {
                    "displayName": "ais",
                    "parameterSetName": "ManagedServiceIdentity",
                    "parameterValues": {
                        "authProvider": { "Type": "ManagedServiceIdentity" },
                        "serverName": "@appsetting('sqlServerAIS_serverName')",
                        "databaseName": "@appsetting('sqlServerAIS_databaseName')"
                    },
                    "serviceProvider": { "id": "/serviceProviders/sql" }
                }
            }
        }"#;
        let out: serde_json::Value =
            serde_json::from_str(&patch_connections_for_local(raw)).unwrap();
        let c = &out["serviceProviderConnections"]["sql-server-ais"];
        assert_eq!(c["parameterSetName"], "connectionString");
        assert_eq!(
            c["parameterValues"]["connectionString"],
            "@appsetting('sql-server-ais_connectionString')"
        );
        // MSI-only fields are gone, so the driver can't fall back to identity auth.
        assert!(c["parameterValues"]["authProvider"].is_null());
        assert_eq!(c["displayName"], "ais");
    }

    #[test]
    fn non_msi_sql_is_left_alone() {
        let raw = r#"{
            "serviceProviderConnections": {
                "sql": {
                    "parameterSetName": "connectionString",
                    "parameterValues": { "connectionString": "@appsetting('sql_connectionString')" },
                    "serviceProvider": { "id": "/serviceProviders/sql" }
                }
            }
        }"#;
        let out: serde_json::Value =
            serde_json::from_str(&patch_connections_for_local(raw)).unwrap();
        assert_eq!(
            out["serviceProviderConnections"]["sql"]["parameterValues"]["connectionString"],
            "@appsetting('sql_connectionString')"
        );
    }

    #[test]
    fn function_app_base_url_and_key_get_local_defaults() {
        // These are referenced straight from a workflow's HTTP action rather
        // than through connections.json, so they only surface via the
        // missing-settings scan — but "Auto-stub all" must still produce a
        // value that actually works locally, not a blank to fill in by hand.
        assert_eq!(
            smart_default("AIS_Functions_BaseUrl"),
            "http://localhost:7072"
        );
        assert_eq!(smart_default("Functions_BaseUrl"), "http://localhost:7072");
        // Non-empty: a blank x-functions-key header is rejected by some hosts
        // even when the function itself is anonymous.
        assert_eq!(smart_default("AIS_Functions_Key"), "placeholder");
        assert_eq!(smart_default("Functions_Key"), "placeholder");
        // The pre-existing per-function keys keep their own shape.
        assert_eq!(
            smart_default("azureFunction_ConvertXlsxToTxt_triggerUrl"),
            "http://localhost:7072/api/ConvertXlsxToTxt"
        );
    }

    #[test]
    fn teams_routing_keys_resolve_to_distinct_local_values() {
        // Distinctness is the point: with blanks, a rule routing to the wrong
        // channel is indistinguishable from one routing correctly.
        assert_eq!(smart_default("teams:groupId"), "local-teams-group");
        assert_eq!(
            smart_default("teams:channel:kyriba:alerts"),
            "local-channel-kyriba-alerts"
        );
        assert_eq!(
            smart_default("teams:channel:ventriks:notifications"),
            "local-channel-ventriks-notifications"
        );
        assert_ne!(
            smart_default("teams:channel:kyriba:alerts"),
            smart_default("teams:channel:kyriba:notifications")
        );
    }
}
