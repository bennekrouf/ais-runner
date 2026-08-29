use crate::services::azure::cli::{az_command, AzError};

/// One Event Grid subscription (from a system topic or resource-scoped).
#[derive(Clone, Debug, PartialEq)]
pub struct EgSubscription {
    pub name: String,
    pub topic_name: String,
    pub topic_type: String,
    /// e.g. "Microsoft.EventGrid.EventSubscription"
    pub endpoint_type: String,
    pub endpoint: String,
    /// Event types the subscription is registered for
    pub included_event_types: Vec<String>,
    /// Advanced filters (subject begins/ends with, etc.)
    pub filters: Vec<EgFilter>,
    pub provisioning_state: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EgFilter {
    pub label: String,
    pub value: String,
}

/// A system topic that exists in a resource group.
#[derive(Clone, Debug)]
pub struct EgSystemTopic {
    pub name: String,
    pub source: String,
    pub topic_type: String,
}

/// A custom Event Grid topic.
#[derive(Clone, Debug)]
pub struct EgTopic {
    pub name: String,
    pub resource_group: String,
    pub endpoint: String,
    pub location: String,
    pub input_schema: String,
}

/// Everything fetched for one environment.
#[derive(Clone, Debug)]
pub struct EgData {
    pub topics: Vec<(EgTopic, Vec<EgSubscription>)>,
    pub system_topics: Vec<(EgSystemTopic, Vec<EgSubscription>)>,
}

// ── CLI wrappers ─────────────────────────────────────────────────────────────

/// List system topics in a resource group.
pub fn list_system_topics(sub: &str, rg: &str) -> Result<Vec<EgSystemTopic>, AzError> {
    let mut cmd = az_command(&[
        "eventgrid",
        "system-topic",
        "list",
        "--subscription",
        sub,
        "--resource-group",
        rg,
        "-o",
        "json",
    ]);
    let out = cmd
        .output()
        .map_err(|e| AzError::Other(format!("az not found: {e}")))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() {
        return Err(classify_error(&stdout, &stderr));
    }
    let arr: Vec<serde_json::Value> =
        serde_json::from_str(&stdout).map_err(|e| AzError::Other(format!("parse topics: {e}")))?;

    Ok(arr
        .iter()
        .map(|v| EgSystemTopic {
            name: v["name"].as_str().unwrap_or("").into(),
            source: v["source"].as_str().unwrap_or("").into(),
            topic_type: v["topicType"].as_str().unwrap_or("").into(),
        })
        .collect())
}

/// List all custom topics in a subscription (across all resource groups).
pub fn list_topics(sub: &str) -> Result<Vec<EgTopic>, AzError> {
    let mut cmd = az_command(&[
        "eventgrid",
        "topic",
        "list",
        "--subscription",
        sub,
        "-o",
        "json",
    ]);
    let out = cmd
        .output()
        .map_err(|e| AzError::Other(format!("az not found: {e}")))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() {
        return Err(classify_error(&stdout, &stderr));
    }
    let arr: Vec<serde_json::Value> =
        serde_json::from_str(&stdout).map_err(|e| AzError::Other(format!("parse topics: {e}")))?;

    Ok(arr
        .iter()
        .map(|v| {
            // Extract resource group from id: /subscriptions/.../resourceGroups/RG_NAME/...
            let rg = v["id"]
                .as_str()
                .unwrap_or("")
                .split("/resourceGroups/")
                .nth(1)
                .and_then(|s| s.split('/').next())
                .unwrap_or("")
                .to_string();
            EgTopic {
                name: v["name"].as_str().unwrap_or("").into(),
                resource_group: rg,
                endpoint: v["endpoint"].as_str().unwrap_or("").into(),
                location: v["location"].as_str().unwrap_or("").into(),
                input_schema: v["inputSchema"]
                    .as_str()
                    .unwrap_or("EventGridSchema")
                    .into(),
            }
        })
        .collect())
}

/// List event subscriptions under a custom topic.
pub fn list_custom_topic_subscriptions(
    sub: &str,
    rg: &str,
    topic: &str,
) -> Result<Vec<EgSubscription>, AzError> {
    let mut cmd = az_command(&[
        "eventgrid",
        "topic",
        "event-subscription",
        "list",
        "--subscription",
        sub,
        "--resource-group",
        rg,
        "--topic-name",
        topic,
        "-o",
        "json",
    ]);
    let out = cmd
        .output()
        .map_err(|e| AzError::Other(format!("az not found: {e}")))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() {
        return Err(classify_error(&stdout, &stderr));
    }
    let arr: Vec<serde_json::Value> = serde_json::from_str(&stdout)
        .map_err(|e| AzError::Other(format!("parse subscriptions: {e}")))?;

    Ok(arr.iter().map(|v| parse_subscription(v, topic)).collect())
}

/// List event subscriptions under a system topic.
pub fn list_topic_subscriptions(
    sub: &str,
    rg: &str,
    topic: &str,
) -> Result<Vec<EgSubscription>, AzError> {
    let mut cmd = az_command(&[
        "eventgrid",
        "system-topic",
        "event-subscription",
        "list",
        "--subscription",
        sub,
        "--resource-group",
        rg,
        "--system-topic-name",
        topic,
        "-o",
        "json",
    ]);
    let out = cmd
        .output()
        .map_err(|e| AzError::Other(format!("az not found: {e}")))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() {
        return Err(classify_error(&stdout, &stderr));
    }
    let arr: Vec<serde_json::Value> = serde_json::from_str(&stdout)
        .map_err(|e| AzError::Other(format!("parse subscriptions: {e}")))?;

    Ok(arr.iter().map(|v| parse_subscription(v, topic)).collect())
}

/// List resource-scoped event subscriptions (not under a system topic).
#[allow(dead_code)]
pub fn list_resource_subscriptions(
    sub: &str,
    rg: &str,
    provider: &str,
    resource_type: &str,
    resource_name: &str,
) -> Result<Vec<EgSubscription>, AzError> {
    let source = format!("/subscriptions/{sub}/resourceGroups/{rg}/providers/{provider}/{resource_type}/{resource_name}");
    let mut cmd = az_command(&[
        "eventgrid",
        "event-subscription",
        "list",
        "--source-resource-id",
        &source,
        "-o",
        "json",
    ]);
    let out = cmd
        .output()
        .map_err(|e| AzError::Other(format!("az not found: {e}")))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() {
        return Err(classify_error(&stdout, &stderr));
    }
    let arr: Vec<serde_json::Value> = serde_json::from_str(&stdout)
        .map_err(|e| AzError::Other(format!("parse subscriptions: {e}")))?;

    Ok(arr.iter().map(|v| parse_subscription(v, "")).collect())
}

/// Fetch all Event Grid data for a subscription:
/// 1. All custom topics (subscription-wide) + their event subscriptions
/// 2. System topics in the given resource group + their event subscriptions
pub fn fetch_all(sub: &str, rg: &str) -> Result<EgData, AzError> {
    // Custom topics — subscription-wide
    let custom = list_topics(sub).unwrap_or_default();
    let mut topics = Vec::new();
    for t in custom {
        let t_rg = if t.resource_group.is_empty() {
            rg
        } else {
            &t.resource_group
        };
        let subs = list_custom_topic_subscriptions(sub, t_rg, &t.name).unwrap_or_default();
        topics.push((t, subs));
    }

    // System topics — resource-group scoped
    let sys = list_system_topics(sub, rg).unwrap_or_default();
    let mut system_topics = Vec::new();
    for t in sys {
        let subs = list_topic_subscriptions(sub, rg, &t.name).unwrap_or_default();
        system_topics.push((t, subs));
    }

    Ok(EgData {
        topics,
        system_topics,
    })
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn parse_subscription(v: &serde_json::Value, topic_name: &str) -> EgSubscription {
    let filter = &v["filter"];

    // Subject filters
    let mut filters = Vec::new();
    if let Some(s) = filter["subjectBeginsWith"].as_str() {
        if !s.is_empty() {
            filters.push(EgFilter {
                label: "Subject begins with".into(),
                value: s.into(),
            });
        }
    }
    if let Some(s) = filter["subjectEndsWith"].as_str() {
        if !s.is_empty() {
            filters.push(EgFilter {
                label: "Subject ends with".into(),
                value: s.into(),
            });
        }
    }

    // Advanced filters
    if let Some(adv) = filter["advancedFilters"].as_array() {
        for f in adv {
            let op = f["operatorType"].as_str().unwrap_or("?");
            let key = f["key"].as_str().unwrap_or("?");
            let values: Vec<String> = f["values"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .or_else(|| f["value"].as_str().map(|s| vec![s.to_string()]))
                .unwrap_or_default();
            filters.push(EgFilter {
                label: format!("{key} {op}"),
                value: values.join(", "),
            });
        }
    }

    // Included event types
    let included_event_types = filter["includedEventTypes"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // Endpoint
    let dest = &v["destination"];
    let endpoint_type = dest["endpointType"].as_str().unwrap_or("").into();
    let endpoint = dest["properties"]["endpointUrl"]
        .as_str()
        .or_else(|| dest["properties"]["resourceId"].as_str())
        .unwrap_or("")
        .to_string();

    EgSubscription {
        name: v["name"].as_str().unwrap_or("").into(),
        topic_name: if !topic_name.is_empty() {
            topic_name.into()
        } else {
            v["topic"]
                .as_str()
                .unwrap_or("")
                .rsplit('/')
                .next()
                .unwrap_or("")
                .into()
        },
        topic_type: v["topicType"].as_str().unwrap_or("").into(),
        endpoint_type,
        endpoint,
        included_event_types,
        filters,
        provisioning_state: v["provisioningState"].as_str().unwrap_or("").into(),
    }
}

fn classify_error(stdout: &str, stderr: &str) -> AzError {
    let combined = format!("{stdout} {stderr}");
    if combined.contains("AADSTS")
        || combined.contains("az login")
        || combined.contains("refresh token")
    {
        AzError::NotLoggedIn
    } else {
        AzError::Other(if stderr.is_empty() {
            stdout.to_string()
        } else {
            stderr.to_string()
        })
    }
}
