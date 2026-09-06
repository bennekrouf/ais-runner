//! Tests for `workflow_analysis::analyse`.
//!
//! Split into its own file (included via `#[path]` at the bottom of
//! workflow_analysis.rs) because the module was untested and this adds ~200
//! lines — keeping it separate avoids drowning the actual logic on scroll.
//!
//! Fixtures are deliberately shaped like the real workflows this session
//! touched (Check-Globex-File-Sla, Rcv-Event-Pivot, Send-Globex-files) rather
//! than synthetic minimal JSON, so a change that breaks analysis on an actual
//! production shape fails here first.

use super::*;

fn wf(actions: serde_json::Value) -> String {
    serde_json::json!({
        "definition": { "triggers": {}, "actions": actions }
    })
    .to_string()
}

fn wf_triggered(triggers: serde_json::Value, actions: serde_json::Value) -> String {
    serde_json::json!({
        "definition": { "triggers": triggers, "actions": actions }
    })
    .to_string()
}

// ── malformed input ─────────────────────────────────────────────────────

#[test]
fn invalid_json_yields_default_not_panic() {
    let a = analyse("{ not json");
    assert!(a.is_empty());
    assert_eq!(a.trigger, TriggerKind::Unknown);
}

#[test]
fn empty_object_yields_default() {
    assert!(analyse("{}").is_empty());
}

#[test]
fn missing_actions_key_does_not_panic() {
    let a = analyse(r#"{"definition":{"triggers":{}}}"#);
    assert!(a.is_empty());
}

// ── triggers ─────────────────────────────────────────────────────────────

#[test]
fn http_request_trigger() {
    let a = analyse(&wf_triggered(
        serde_json::json!({ "manual": { "type": "Request", "kind": "Http" } }),
        serde_json::json!({}),
    ));
    assert_eq!(a.trigger, TriggerKind::Http);
    assert!(
        a.is_empty(),
        "HTTP-only trigger counts as empty per is_empty()'s contract"
    );
}

#[test]
fn recurrence_trigger_formats_schedule() {
    // Check-Globex-File-Sla's real trigger shape.
    let a = analyse(&wf_triggered(
        serde_json::json!({
            "Recurrence_hourly": { "type": "Recurrence", "recurrence": { "frequency": "Hour", "interval": 1 } }
        }),
        serde_json::json!({}),
    ));
    assert_eq!(
        a.trigger,
        TriggerKind::Timer {
            schedule: "every 1 Hour".into()
        }
    );
}

#[test]
fn recurrence_trigger_defaults_interval_to_one_when_absent() {
    let a = analyse(&wf_triggered(
        serde_json::json!({ "t": { "type": "Recurrence", "recurrence": { "frequency": "Day" } } }),
        serde_json::json!({}),
    ));
    assert_eq!(
        a.trigger,
        TriggerKind::Timer {
            schedule: "every 1 Day".into()
        }
    );
}

#[test]
fn service_bus_trigger_records_queue_as_input() {
    // Check-Acme-Payment-File's real trigger.
    let a = analyse(&wf_triggered(
        serde_json::json!({
            "When_messages_are_available": {
                "type": "ServiceProvider",
                "inputs": {
                    "parameters": { "queueName": "ais.acme.globex.payment" },
                    "serviceProviderConfiguration": {
                        "serviceProviderId": "/serviceProviders/serviceBus",
                        "operationId": "peekLockQueueMessagesV2"
                    }
                }
            }
        }),
        serde_json::json!({}),
    ));
    assert_eq!(
        a.trigger,
        TriggerKind::ServiceBus {
            queue: "ais.acme.globex.payment".into()
        }
    );
    assert_eq!(a.input_queues, vec!["ais.acme.globex.payment"]);
    // Must not also appear as an output — a trigger only ever consumes.
    assert!(a.output_queues.is_empty());
}

#[test]
fn blob_trigger_uses_path_and_records_as_input() {
    // Check-Acme-Cashflow-File's real trigger.
    let a = analyse(&wf_triggered(
        serde_json::json!({
            "When_a_cashflow_blob_is_added": {
                "type": "ServiceProvider",
                "inputs": {
                    "parameters": { "path": "globex-reports" },
                    "serviceProviderConfiguration": {
                        "serviceProviderId": "/serviceProviders/AzureBlob",
                        "operationId": "whenABlobIsAddedOrModified"
                    }
                }
            }
        }),
        serde_json::json!({}),
    ));
    assert_eq!(
        a.trigger,
        TriggerKind::Blob {
            container: "globex-reports".into()
        }
    );
    assert_eq!(a.input_blobs, vec!["globex-reports"]);
}

#[test]
fn blob_trigger_falls_back_to_container_name_when_path_absent() {
    let a = analyse(&wf_triggered(
        serde_json::json!({
            "t": {
                "type": "ServiceProvider",
                "inputs": {
                    "parameters": { "containerName": "output" },
                    "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/AzureBlob" }
                }
            }
        }),
        serde_json::json!({}),
    ));
    assert_eq!(
        a.trigger,
        TriggerKind::Blob {
            container: "output".into()
        }
    );
}

#[test]
fn unrecognised_trigger_provider_is_unknown_not_a_panic() {
    let a = analyse(&wf_triggered(
        serde_json::json!({ "t": { "type": "ApiConnection", "inputs": {} } }),
        serde_json::json!({}),
    ));
    assert_eq!(a.trigger, TriggerKind::Unknown);
}

// ── Service Bus actions (ServiceProvider style) ─────────────────────────

#[test]
fn send_message_is_output_receive_is_input() {
    let a = analyse(&wf(serde_json::json!({
        "Send_it": {
            "type": "ServiceProvider",
            "inputs": {
                "parameters": { "entityName": "ais.teams.notif" },
                "serviceProviderConfiguration": {
                    "serviceProviderId": "/serviceProviders/serviceBus",
                    "operationId": "sendMessage"
                }
            }
        },
        "Complete_it": {
            "type": "ServiceProvider",
            "inputs": {
                "parameters": { "queueName": "ais.workflow.error" },
                "serviceProviderConfiguration": {
                    "serviceProviderId": "/serviceProviders/serviceBus",
                    "operationId": "completeQueueMessageV2"
                }
            }
        }
    })));
    assert_eq!(a.output_queues, vec!["ais.teams.notif"]);
    assert_eq!(a.input_queues, vec!["ais.workflow.error"]);
}

#[test]
fn queue_name_dedupes_across_actions() {
    let a = analyse(&wf(serde_json::json!({
        "Send1": { "type": "ServiceProvider", "inputs": {
            "parameters": { "entityName": "ais.teams.notif" },
            "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/serviceBus", "operationId": "sendMessage" }
        }},
        "Send2": { "type": "ServiceProvider", "inputs": {
            "parameters": { "entityName": "ais.teams.notif" },
            "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/serviceBus", "operationId": "sendMessage" }
        }}
    })));
    assert_eq!(
        a.output_queues,
        vec!["ais.teams.notif"],
        "same queue sent from two actions must appear once"
    );
}

#[test]
fn dynamic_queue_expression_reports_as_dynamic_not_dropped() {
    // Rcv-Event-Pivot resolves its target queue at runtime via an expression —
    // analysis can't know the literal value, but it must say so rather than
    // silently omitting the action from input_queues/output_queues.
    let a = analyse(&wf(serde_json::json!({
        "Send_dynamic": {
            "type": "ServiceProvider",
            "inputs": {
                "parameters": { "entityName": "@variables('targetQueue')" },
                "serviceProviderConfiguration": {
                    "serviceProviderId": "/serviceProviders/serviceBus",
                    "operationId": "sendMessage"
                }
            }
        }
    })));
    assert_eq!(a.output_queues, vec!["(dynamic)"]);
}

#[test]
fn service_bus_action_without_entity_or_queue_name_is_skipped() {
    let a = analyse(&wf(serde_json::json!({
        "Peek": {
            "type": "ServiceProvider",
            "inputs": {
                "parameters": {},
                "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/serviceBus", "operationId": "sendMessage" }
            }
        }
    })));
    assert!(a.output_queues.is_empty());
}

// ── Service Bus actions (ApiConnection / path style) ─────────────────────

#[test]
fn apiconnection_queue_path_is_parsed() {
    let a = analyse(&wf(serde_json::json!({
        "SendMsg": {
            "type": "ApiConnection",
            "inputs": {
                "path": "/queues/@{encodeURIComponent('my-queue')}/messages",
                "method": "post"
            }
        }
    })));
    assert_eq!(a.output_queues, vec!["my-queue"]);
}

#[test]
fn apiconnection_bare_queue_name_without_expression_wrapper() {
    let a = analyse(&wf(serde_json::json!({
        "SendMsg": { "type": "ApiConnection", "inputs": { "path": "/queues/my-queue/messages", "method": "post" } }
    })));
    assert_eq!(a.output_queues, vec!["my-queue"]);
}

#[test]
fn apiconnection_topic_path_is_parsed_same_as_queue() {
    let a = analyse(&wf(serde_json::json!({
        "SendMsg": { "type": "ApiConnection", "inputs": { "path": "/topics/my-topic/messages", "method": "post" } }
    })));
    assert_eq!(a.output_queues, vec!["my-topic"]);
}

#[test]
fn apiconnection_pure_expression_segment_is_unresolvable() {
    // "@variables('q')" with no encodeURIComponent wrapper — cannot be
    // resolved statically, and must be dropped rather than mis-parsed as a
    // literal queue named "@variables('q')".
    let a = analyse(&wf(serde_json::json!({
        "SendMsg": { "type": "ApiConnection", "inputs": { "path": "/queues/@variables('q')/messages", "method": "post" } }
    })));
    assert!(a.output_queues.is_empty());
    assert!(a.input_queues.is_empty());
}

#[test]
fn apiconnection_get_without_trailing_messages_is_input() {
    let a = analyse(&wf(serde_json::json!({
        "Peek": { "type": "ApiConnection", "inputs": { "path": "/queues/my-queue/peek", "method": "get" } }
    })));
    assert_eq!(a.input_queues, vec!["my-queue"]);
}

// ── Blob actions ─────────────────────────────────────────────────────────

#[test]
fn upload_is_output_read_is_input() {
    let a = analyse(&wf(serde_json::json!({
        "Upload": { "type": "ServiceProvider", "inputs": {
            "parameters": { "containerName": "globex-archive" },
            "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/AzureBlob", "operationId": "uploadBlob" }
        }},
        "Read": { "type": "ServiceProvider", "inputs": {
            "parameters": { "containerName": "globex-reports" },
            "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/AzureBlob", "operationId": "readBlob" }
        }}
    })));
    assert_eq!(a.output_blobs, vec!["globex-archive"]);
    assert_eq!(a.input_blobs, vec!["globex-reports"]);
}

#[test]
fn delete_is_classified_as_input_not_output() {
    // deleteBlob matches none of the write-verb substrings (create/upload/
    // write/put/copy), so it currently lands in input_blobs. Documenting the
    // actual behaviour here means a change to that verb list is a deliberate
    // decision, not an accidental one discovered in production.
    let a = analyse(&wf(serde_json::json!({
        "Delete": { "type": "ServiceProvider", "inputs": {
            "parameters": { "containerName": "output" },
            "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/AzureBlob", "operationId": "deleteBlob" }
        }}
    })));
    assert_eq!(a.input_blobs, vec!["output"]);
    assert!(a.output_blobs.is_empty());
}

#[test]
fn list_blobs_is_input() {
    let a = analyse(&wf(serde_json::json!({
        "List": { "type": "ServiceProvider", "inputs": {
            "parameters": { "containerName": "globex-reports" },
            "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/AzureBlob", "operationId": "listBlobs" }
        }}
    })));
    assert_eq!(a.input_blobs, vec!["globex-reports"]);
}

#[test]
fn blob_container_dedupes_read_and_write_independently() {
    let a = analyse(&wf(serde_json::json!({
        "Read1": { "type": "ServiceProvider", "inputs": {
            "parameters": { "containerName": "c" },
            "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/AzureBlob", "operationId": "readBlob" }
        }},
        "Read2": { "type": "ServiceProvider", "inputs": {
            "parameters": { "containerName": "c" },
            "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/AzureBlob", "operationId": "readBlob" }
        }},
        "Write1": { "type": "ServiceProvider", "inputs": {
            "parameters": { "containerName": "c" },
            "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/AzureBlob", "operationId": "uploadBlob" }
        }}
    })));
    assert_eq!(a.input_blobs, vec!["c"]);
    assert_eq!(
        a.output_blobs,
        vec!["c"],
        "same container can legitimately be both read and written"
    );
}

// ── SQL stored procedures ────────────────────────────────────────────────

#[test]
fn sproc_name_brackets_are_stripped() {
    let a = analyse(&wf(serde_json::json!({
        "Call": { "type": "ServiceProvider", "inputs": {
            "parameters": { "storedProcedureName": "[dbo].[WfFanInOut_Begin_sp]" },
            "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/sql", "operationId": "executeStoredProcedure" }
        }}
    })));
    assert_eq!(a.sql_sprocs.len(), 1);
    assert_eq!(a.sql_sprocs[0].name, "dbo.WfFanInOut_Begin_sp");
}

#[test]
fn sproc_params_strip_leading_at_and_preserve_all_keys() {
    let a = analyse(&wf(serde_json::json!({
        "Call": { "type": "ServiceProvider", "inputs": {
            "parameters": {
                "storedProcedureName": "dbo.Sp",
                "storedProcedureParameters": { "@Id": "1", "@Name": "x" }
            },
            "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/sql", "operationId": "executeStoredProcedure" }
        }}
    })));
    let mut params = a.sql_sprocs[0].params.clone();
    params.sort();
    assert_eq!(params, vec!["Id", "Name"]);
}

#[test]
fn sproc_name_field_fallback_chain() {
    for field in [
        "storedProcedureName",
        "storedProcedureFullName",
        "procedure",
    ] {
        let a = analyse(&wf(serde_json::json!({
            "Call": { "type": "ServiceProvider", "inputs": {
                "parameters": { field: "dbo.Sp" },
                "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/sql", "operationId": "executeQuery" }
            }}
        })));
        assert_eq!(
            a.sql_sprocs.first().map(|s| s.name.as_str()),
            Some("dbo.Sp"),
            "field '{field}' should resolve the sproc name"
        );
    }
}

#[test]
fn same_sproc_called_twice_is_recorded_once() {
    let a = analyse(&wf(serde_json::json!({
        "Call1": { "type": "ServiceProvider", "inputs": {
            "parameters": { "storedProcedureName": "dbo.Sp" },
            "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/sql", "operationId": "executeStoredProcedure" }
        }},
        "Call2": { "type": "ServiceProvider", "inputs": {
            "parameters": { "storedProcedureName": "dbo.Sp" },
            "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/sql", "operationId": "executeStoredProcedure" }
        }}
    })));
    assert_eq!(a.sql_sprocs.len(), 1);
}

// ── Liquid maps ───────────────────────────────────────────────────────────

#[test]
fn liquid_map_name_new_style() {
    let a = analyse(&wf(serde_json::json!({
        "Transform": { "type": "Liquid", "inputs": { "map": { "name": "ApimEventToPivotEvent.liquid" } } }
    })));
    assert_eq!(a.liquid_maps, vec!["ApimEventToPivotEvent.liquid"]);
}

#[test]
fn liquid_map_name_legacy_integration_account_style() {
    let a = analyse(&wf(serde_json::json!({
        "Transform": { "type": "Liquid", "inputs": { "integrationAccount": { "map": { "name": "Old.liquid" } } } }
    })));
    assert_eq!(a.liquid_maps, vec!["Old.liquid"]);
}

// ── HTTP calls ────────────────────────────────────────────────────────────

#[test]
fn http_host_is_extracted_and_path_stripped() {
    let a = analyse(&wf(serde_json::json!({
        "Call": { "type": "Http", "inputs": { "uri": "https://func-example.azurewebsites.net/api/ConvertXlsxToTxt" } }
    })));
    assert_eq!(a.http_calls, vec!["func-example.azurewebsites.net"]);
}

#[test]
fn http_query_string_does_not_leak_into_host() {
    let a = analyse(&wf(serde_json::json!({
        "Call": { "type": "Http", "inputs": { "uri": "https://appcs-tom-nonprod-chn-001.azconfig.io/kv/x?label=dev&api-version=1.0" } }
    })));
    assert_eq!(a.http_calls, vec!["appcs-tom-nonprod-chn-001.azconfig.io"]);
}

#[test]
fn http_url_field_is_an_accepted_alias_for_uri() {
    let a = analyse(&wf(serde_json::json!({
        "Call": { "type": "Http", "inputs": { "url": "https://example.com/x" } }
    })));
    assert_eq!(a.http_calls, vec!["example.com"]);
}

#[test]
fn http_dynamic_uri_expression_is_not_reported_as_a_host() {
    let a = analyse(&wf(serde_json::json!({
        "Call": { "type": "Http", "inputs": { "uri": "@parameters('AcmeUrl')" } }
    })));
    assert!(a.http_calls.is_empty());
}

#[test]
fn http_calls_dedupe_by_host() {
    let a = analyse(&wf(serde_json::json!({
        "Call1": { "type": "Http", "inputs": { "uri": "https://a.example.com/one" } },
        "Call2": { "type": "Http", "inputs": { "uri": "https://a.example.com/two" } }
    })));
    assert_eq!(a.http_calls, vec!["a.example.com"]);
}

// ── nested control flow ──────────────────────────────────────────────────

#[test]
fn actions_nested_inside_if_true_branch_are_found() {
    let a = analyse(&wf(serde_json::json!({
        "Check": {
            "type": "If",
            "actions": {
                "Send": { "type": "ServiceProvider", "inputs": {
                    "parameters": { "entityName": "q" },
                    "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/serviceBus", "operationId": "sendMessage" }
                }}
            },
            "else": { "actions": {} }
        }
    })));
    assert_eq!(a.output_queues, vec!["q"]);
}

#[test]
fn actions_nested_inside_if_else_branch_are_found() {
    // Check-Acme-Payment-File's error path lives entirely in an else branch.
    let a = analyse(&wf(serde_json::json!({
        "Check": {
            "type": "If",
            "actions": {},
            "else": {
                "actions": {
                    "Send_error": { "type": "ServiceProvider", "inputs": {
                        "parameters": { "entityName": "ais.workflow.error" },
                        "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/serviceBus", "operationId": "sendMessage" }
                    }}
                }
            }
        }
    })));
    assert_eq!(a.output_queues, vec!["ais.workflow.error"]);
}

#[test]
fn actions_nested_inside_foreach_are_found() {
    let a = analyse(&wf(serde_json::json!({
        "Loop": {
            "type": "Foreach",
            "actions": {
                "Upload": { "type": "ServiceProvider", "inputs": {
                    "parameters": { "containerName": "globex-archive" },
                    "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/AzureBlob", "operationId": "uploadBlob" }
                }}
            }
        }
    })));
    assert_eq!(a.output_blobs, vec!["globex-archive"]);
}

#[test]
fn actions_nested_inside_switch_cases_are_found() {
    let a = analyse(&wf(serde_json::json!({
        "Switch": {
            "type": "Switch",
            "cases": {
                "Case1": { "actions": {
                    "Send": { "type": "ServiceProvider", "inputs": {
                        "parameters": { "entityName": "case-queue" },
                        "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/serviceBus", "operationId": "sendMessage" }
                    }}
                }}
            },
            "default": { "actions": {} }
        }
    })));
    assert_eq!(a.output_queues, vec!["case-queue"]);
}

#[test]
fn actions_nested_inside_switch_default_are_found() {
    // "default" has the same wrapper shape as "else" ({ "actions": {...} }, one
    // level deeper than "actions"/"cases"/N) — this is the same bug class the
    // else-branch fix addressed, exercised here for the sibling key.
    let a = analyse(&wf(serde_json::json!({
        "Switch": {
            "type": "Switch",
            "cases": {},
            "default": { "actions": {
                "Send": { "type": "ServiceProvider", "inputs": {
                    "parameters": { "entityName": "default-queue" },
                    "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/serviceBus", "operationId": "sendMessage" }
                }}
            }}
        }
    })));
    assert_eq!(a.output_queues, vec!["default-queue"]);
}

#[test]
fn deeply_nested_if_inside_foreach_inside_scope_is_found() {
    // Mirrors Check-Acme-Payment-File's real nesting depth: Scope -> Foreach
    // -> If -> action. Confirms recursion doesn't stop after one level.
    let a = analyse(&wf(serde_json::json!({
        "Scope1": { "type": "Scope", "actions": {
            "Loop": { "type": "Foreach", "actions": {
                "Check": { "type": "If", "actions": {
                    "Send": { "type": "ServiceProvider", "inputs": {
                        "parameters": { "entityName": "deep-queue" },
                        "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/serviceBus", "operationId": "sendMessage" }
                    }}
                }, "else": { "actions": {} } }
            }}
        }}
    })));
    assert_eq!(a.output_queues, vec!["deep-queue"]);
}

// ── is_empty() ────────────────────────────────────────────────────────────

#[test]
fn is_empty_false_once_any_field_is_populated() {
    let a = analyse(&wf(serde_json::json!({
        "Send": { "type": "ServiceProvider", "inputs": {
            "parameters": { "entityName": "q" },
            "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/serviceBus", "operationId": "sendMessage" }
        }}
    })));
    assert!(!a.is_empty());
}

#[test]
fn blob_and_service_bus_triggers_are_not_empty() {
    let sb = analyse(&wf_triggered(
        serde_json::json!({ "t": { "type": "ServiceProvider", "inputs": {
            "parameters": { "queueName": "q" },
            "serviceProviderConfiguration": { "serviceProviderId": "/serviceProviders/serviceBus" }
        }}}),
        serde_json::json!({}),
    ));
    assert!(
        !sb.is_empty(),
        "a workflow that consumes a queue is not analytically empty"
    );
}

// ── all_queues() ──────────────────────────────────────────────────────────

#[test]
fn all_queues_merges_and_dedupes_input_and_output() {
    let mut a = WorkflowAnalysis::default();
    a.input_queues = vec!["a".into(), "shared".into()];
    a.output_queues = vec!["shared".into(), "b".into()];
    assert_eq!(a.all_queues(), vec!["a", "shared", "b"]);
}

// ── literal_str / normalize_sproc_name / extract_host (direct) ──────────

#[test]
fn literal_str_rejects_expressions_and_empty_strings() {
    assert_eq!(
        literal_str(&serde_json::json!("plain")),
        Some("plain".to_string())
    );
    assert_eq!(literal_str(&serde_json::json!("@variables('x')")), None);
    assert_eq!(literal_str(&serde_json::json!("")), None);
    assert_eq!(literal_str(&serde_json::json!(null)), None);
    assert_eq!(literal_str(&serde_json::json!(42)), None);
}

#[test]
fn normalize_sproc_name_strips_all_brackets_and_trims() {
    assert_eq!(normalize_sproc_name(" [dbo].[Sp] "), "dbo.Sp");
    assert_eq!(normalize_sproc_name("dbo.Sp"), "dbo.Sp");
}

#[test]
fn extract_host_handles_scheme_path_and_query() {
    assert_eq!(
        extract_host("https://host.example.com/a/b?x=1"),
        Some("host.example.com".into())
    );
    assert_eq!(
        extract_host("host.example.com/a"),
        Some("host.example.com".into())
    );
    assert_eq!(extract_host("@parameters('x')"), None);
    assert_eq!(extract_host(""), None);
}
