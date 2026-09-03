use crate::cli::{az_command, AzError};

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
    /// The same advanced filters kept structured, for overlap analysis.
    /// `filters` is this data flattened for display and cannot be reasoned about.
    pub adv: Vec<EgAdvFilter>,
    pub provisioning_state: String,
    /// Where undeliverable events go. `None` means Event Grid *discards* them
    /// once the retry policy is exhausted — no queue, no alert, no trace.
    pub dead_letter: Option<String>,
    pub max_delivery_attempts: Option<i64>,
    pub event_ttl_minutes: Option<i64>,
    pub advanced_filtering_on_arrays: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EgFilter {
    pub label: String,
    pub value: String,
}

/// One advanced filter, unflattened.
#[derive(Clone, Debug, PartialEq)]
pub struct EgAdvFilter {
    pub key: String,
    pub operator: String,
    pub values: Vec<String>,
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

    // Advanced filters — kept twice: flattened for display, structured for analysis.
    let mut adv_filters = Vec::new();
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
            adv_filters.push(EgAdvFilter {
                key: key.to_string(),
                operator: op.to_string(),
                values,
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

    // Endpoint. A subscription delivering under a managed identity carries a
    // null `destination` and puts the real target under
    // `deliveryWithResourceIdentity.destination` — reading only the former
    // rendered those rows with a blank endpoint.
    let dest = if v["destination"].is_object() {
        &v["destination"]
    } else {
        &v["deliveryWithResourceIdentity"]["destination"]
    };
    let endpoint_type = dest["endpointType"].as_str().unwrap_or("").into();
    let endpoint = dest["properties"]["endpointUrl"]
        .as_str()
        .or_else(|| dest["properties"]["resourceId"].as_str())
        .or_else(|| dest["resourceId"].as_str())
        .unwrap_or("")
        .to_string();

    // Delivery guarantees. `deadLetterDestination` null is the dangerous one:
    // events are dropped silently once retries run out.
    let dead_letter = v["deadLetterDestination"]["properties"]["resourceId"]
        .as_str()
        .or_else(|| {
            v["deadLetterWithResourceIdentity"]["deadLetterDestination"]["properties"]["resourceId"]
                .as_str()
        })
        .map(String::from);
    let retry = &v["retryPolicy"];

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
        adv: adv_filters,
        provisioning_state: v["provisioningState"].as_str().unwrap_or("").into(),
        dead_letter,
        max_delivery_attempts: retry["maxDeliveryAttempts"].as_i64(),
        event_ttl_minutes: retry["eventTimeToLiveInMinutes"].as_i64(),
        advanced_filtering_on_arrays: filter["enableAdvancedFilteringOnArrays"]
            .as_bool()
            .unwrap_or(false),
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

// ── Overlap analysis ─────────────────────────────────────────────────────────

/// Two subscriptions on one topic that can both match the same event.
#[derive(Clone, Debug, PartialEq)]
pub struct EgOverlap {
    pub topic: String,
    pub left: String,
    pub right: String,
    pub event_type: String,
    /// Why they are not disjoint, in the reader's terms.
    pub reason: String,
}

/// True when two filters on the same key provably cannot both match.
///
/// Only the cases that actually occur are decided; anything else is treated as
/// "might overlap", because reporting a possible overlap the reader can dismiss
/// costs less than staying silent about a real one.
fn disjoint_on_key(a: &EgAdvFilter, b: &EgAdvFilter) -> bool {
    let lower = |v: &[String]| -> Vec<String> { v.iter().map(|s| s.to_lowercase()).collect() };
    let (av, bv) = (lower(&a.values), lower(&b.values));
    match (a.operator.as_str(), b.operator.as_str()) {
        // Two allow-lists: disjoint when they share no value.
        ("StringIn", "StringIn") => !av.iter().any(|x| bv.contains(x)),
        // Allow-list vs deny-list: disjoint only when the deny-list covers
        // every value the allow-list admits. This is the case that broke:
        // StringIn[Companies…] against StringNotIn[SnapShot] shares Companies,
        // so both subscriptions matched and the event was delivered twice.
        ("StringIn", "StringNotIn") => av.iter().all(|x| bv.contains(x)),
        ("StringNotIn", "StringIn") => bv.iter().all(|x| av.contains(x)),
        _ => false,
    }
}

/// Find subscription pairs on the same topic and event type whose filters can
/// both match one event.
///
/// Event Grid fans out to every matching subscription, so an overlap is a
/// silent duplicate delivery: two consumers each process the message, usually
/// with only one of them designed for it.
pub fn find_overlaps(topic: &str, subs: &[EgSubscription]) -> Vec<EgOverlap> {
    let mut out = Vec::new();
    for (i, a) in subs.iter().enumerate() {
        for b in subs.iter().skip(i + 1) {
            let shared: Vec<&String> = a
                .included_event_types
                .iter()
                .filter(|t| b.included_event_types.contains(t))
                .collect();
            for et in shared {
                // Disjoint if ANY shared key separates them.
                let mut separator = None;
                for fa in &a.adv {
                    for fb in &b.adv {
                        if fa.key.eq_ignore_ascii_case(&fb.key) && disjoint_on_key(fa, fb) {
                            separator = Some(fa.key.clone());
                        }
                    }
                }
                if separator.is_some() {
                    continue;
                }
                let keys: Vec<String> = a
                    .adv
                    .iter()
                    .map(|f| f.key.clone())
                    .filter(|k| b.adv.iter().any(|f| f.key.eq_ignore_ascii_case(k)))
                    .collect();
                let reason = if keys.is_empty() {
                    "no shared filter key separates them".to_string()
                } else {
                    format!("filters on {} do not exclude each other", keys.join(", "))
                };
                out.push(EgOverlap {
                    topic: topic.to_string(),
                    left: a.name.clone(),
                    right: b.name.clone(),
                    event_type: et.clone(),
                    reason,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod overlap_tests {
    use super::*;

    fn sub(name: &str, et: &str, adv: Vec<(&str, &str, Vec<&str>)>) -> EgSubscription {
        EgSubscription {
            name: name.into(),
            topic_name: "t".into(),
            topic_type: String::new(),
            endpoint_type: "ServiceBusQueue".into(),
            endpoint: String::new(),
            included_event_types: vec![et.into()],
            filters: Vec::new(),
            adv: adv
                .into_iter()
                .map(|(k, op, vals)| EgAdvFilter {
                    key: k.into(),
                    operator: op.into(),
                    values: vals.into_iter().map(String::from).collect(),
                })
                .collect(),
            provisioning_state: "Succeeded".into(),
            dead_letter: None,
            max_delivery_attempts: Some(30),
            event_ttl_minutes: Some(1440),
            advanced_filtering_on_arrays: true,
        }
    }

    const MODULE: &str = "data.msg.content.event.module";
    const SOURCE: &str = "data.msg.content.event.source";

    /// The real defect: an allow-list beside a deny-list that fails to exclude it.
    #[test]
    fn allow_list_inside_a_deny_list_is_an_overlap() {
        let subs = vec![
            sub(
                "ais-event-ignite",
                "ais.pivot.event",
                vec![
                    (SOURCE, "StringIn", vec!["IGNITE"]),
                    (
                        MODULE,
                        "StringIn",
                        vec!["Companies", "Strategies", "SupplyChainTanks"],
                    ),
                ],
            ),
            sub(
                "ais-pivot-event-ignite",
                "ais.pivot.event",
                vec![
                    (SOURCE, "StringIn", vec!["IGNITE"]),
                    (MODULE, "StringNotIn", vec!["SnapShot"]),
                ],
            ),
        ];
        let found = find_overlaps("evgt", &subs);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].event_type, "ais.pivot.event");
        assert!(found[0].reason.contains(MODULE));
    }

    /// After the fix the deny-list covers every allowed value, so they are complements.
    #[test]
    fn deny_list_covering_the_allow_list_is_disjoint() {
        let subs = vec![
            sub(
                "ais-event-ignite",
                "ais.pivot.event",
                vec![(
                    MODULE,
                    "StringIn",
                    vec!["Companies", "Strategies", "SupplyChainTanks"],
                )],
            ),
            sub(
                "ais-pivot-event-ignite",
                "ais.pivot.event",
                vec![(
                    MODULE,
                    "StringNotIn",
                    vec!["SnapShot", "Companies", "Strategies", "SupplyChainTanks"],
                )],
            ),
        ];
        assert!(find_overlaps("evgt", &subs).is_empty());
    }

    #[test]
    fn two_allow_lists_sharing_no_value_are_disjoint() {
        let subs = vec![
            sub(
                "a",
                "ais.pivot.event",
                vec![(MODULE, "StringIn", vec!["SnapShot"])],
            ),
            sub(
                "b",
                "ais.pivot.event",
                vec![(MODULE, "StringIn", vec!["Companies"])],
            ),
        ];
        assert!(find_overlaps("evgt", &subs).is_empty());
    }

    #[test]
    fn different_event_types_never_overlap() {
        let subs = vec![
            sub(
                "a",
                "ais.pivot.event",
                vec![(MODULE, "StringIn", vec!["X"])],
            ),
            sub(
                "b",
                "ais.pivot.event.response",
                vec![(MODULE, "StringIn", vec!["X"])],
            ),
        ];
        assert!(find_overlaps("evgt", &subs).is_empty());
    }

    /// A subscription with no advanced filter at all catches everything on its
    /// event type — worth flagging beside any sibling.
    #[test]
    fn an_unfiltered_subscription_overlaps_its_siblings() {
        let subs = vec![
            sub("catch-all", "ais.pivot.event", vec![]),
            sub(
                "specific",
                "ais.pivot.event",
                vec![(MODULE, "StringIn", vec!["Companies"])],
            ),
        ];
        let found = find_overlaps("evgt", &subs);
        assert_eq!(found.len(), 1);
        assert!(found[0].reason.contains("no shared filter key"));
    }
}
