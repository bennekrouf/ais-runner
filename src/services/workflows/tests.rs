use super::*;

// Open test/logic-apps/ in AIS Runner to manually exercise all sample workflows.
const FIXTURE:        &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test/logic-apps");
const FIXTURE_NESTED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixtures/nested");

// ── helpers ───────────────────────────────────────────────────────────────

fn payload_json(workflow: &str) -> serde_json::Value {
    let raw = crate::services::payload::suggest_payload(FIXTURE, workflow);
    serde_json::from_str(&raw).expect("suggest_payload returned invalid JSON")
}

// ── scan / load ───────────────────────────────────────────────────────────

#[test]
fn load_all_workflows_from_fixture() {
    let items = scan_local_workflows(FIXTURE);
    assert_eq!(items.len(), 3, "expected hello-world, write-to-storage, send-to-bus");

    let names: Vec<&str> = items.iter().map(|w| w.name.as_str()).collect();
    assert!(names.contains(&"hello-world"));
    assert!(names.contains(&"write-to-storage"));
    assert!(names.contains(&"send-to-bus"));

    for wf in &items {
        assert_eq!(wf.trigger_name, "manual");
        assert_eq!(wf.trigger_type, "Request");
        assert!(wf.healthy);
    }
}

#[test]
fn resolve_logic_apps_dir_flat() {
    let resolved = resolve_logic_apps_dir(FIXTURE);
    assert_eq!(resolved, std::path::Path::new(FIXTURE));
}

#[test]
fn resolve_logic_apps_dir_nested() {
    let resolved = resolve_logic_apps_dir(FIXTURE_NESTED);
    assert_eq!(resolved, std::path::Path::new(FIXTURE_NESTED).join("logic_apps"));

    let items = scan_local_workflows(FIXTURE_NESTED);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name, "hello-world");
}

#[test]
fn scan_broken_workflow_json() {
    let dir = std::env::temp_dir().join("ais_test_broken");
    let wf_dir = dir.join("bad-json");
    std::fs::create_dir_all(&wf_dir).unwrap();
    std::fs::write(wf_dir.join("workflow.json"), b"{ not valid json !!!").unwrap();

    let broken = scan_broken_workflows(&dir.to_string_lossy());
    assert_eq!(broken.len(), 1);
    assert_eq!(broken[0].0, "bad-json");
    assert!(broken[0].1.contains("JSON error"), "got: {}", broken[0].1);
}

// ── payload suggestions ───────────────────────────────────────────────────

#[test]
fn suggest_payload_hello_world() {
    let v = payload_json("hello-world");
    assert!(v["message"].is_string());
    assert!(v["id"].is_string());
}

#[test]
fn suggest_payload_write_to_storage() {
    let v = payload_json("write-to-storage");
    assert!(v["content"].is_string());
    assert!(v["blobName"].is_string());
}

#[test]
fn suggest_payload_send_to_bus() {
    let v = payload_json("send-to-bus");
    assert!(v["queueName"].is_string());
    assert!(v["body"].is_string());
    assert!(v["messageType"].is_string());
}

// ── Connection diagnostics ────────────────────────────────────────────────

#[test]
fn missing_endpoint_flagged_for_send_to_bus() {
    // Build an isolated fixture so the test is not affected by the pre-flight
    // auto-stub that writes a placeholder into the real local.settings.json.
    use std::fs;
    let dir = std::env::temp_dir().join("ais_test_sb_missing");
    let wf_dir = dir.join("send-to-bus");
    fs::create_dir_all(&wf_dir).unwrap();

    fs::write(wf_dir.join("workflow.json"), br#"{
        "definition": {
            "triggers": { "manual": { "type": "Request", "kind": "Http" } },
            "actions": {
                "Send": {
                    "type": "ServiceProvider",
                    "inputs": {
                        "parameters": { "entityName": "q" },
                        "serviceProviderConfiguration": {
                            "connectionName": "sb",
                            "operationId": "sendMessage",
                            "serviceProviderId": "/serviceProviders/serviceBus"
                        }
                    }
                }
            }
        }
    }"#).unwrap();

    fs::write(dir.join("connections.json"), br#"{
        "serviceProviderConnections": {
            "sb": {
                "parameterValues": {
                    "connectionString": "@appsetting('MY_SB_CONN_STR')"
                },
                "serviceProvider": { "id": "/serviceProviders/serviceBus" }
            }
        }
    }"#).unwrap();

    fs::write(dir.join("local.settings.json"), br#"{
        "IsEncrypted": false,
        "Values": { "MY_SB_CONN_STR": "" }
    }"#).unwrap();

    let missing = crate::services::connection_diag::missing_endpoints_for_workflow(
        &dir.to_string_lossy(), "send-to-bus",
    );
    assert_eq!(missing.len(), 1, "expected one missing endpoint, got: {:?}", missing);
    assert_eq!(missing[0].0, "sb");
    assert_eq!(missing[0].1, "MY_SB_CONN_STR");
}

#[test]
fn no_missing_endpoint_for_write_to_storage() {
    let missing = crate::services::connection_diag::missing_endpoints_for_workflow(
        FIXTURE, "write-to-storage",
    );
    assert!(missing.is_empty(), "unexpected missing: {:?}", missing);
}

// ── duration_ms helper ────────────────────────────────────────────────────

#[test]
fn duration_ms_calculates_correctly() {
    let start = Some("2026-01-01T10:00:00Z".to_string());
    let end   = Some("2026-01-01T10:00:01.5Z".to_string());
    assert_eq!(duration_ms(&start, &end), Some(1500));
}

#[test]
fn duration_ms_returns_none_for_missing_timestamps() {
    assert_eq!(duration_ms(&None, &None), None);
    assert_eq!(duration_ms(&Some("2026-01-01T10:00:00Z".to_string()), &None), None);
}

// ── az trigger commands ───────────────────────────────────────────────────

#[test]
fn az_trigger_command_args() {
    for (workflow, body) in [
        ("hello-world",      r#"{"message":"hi","id":"T-1"}"#),
        ("write-to-storage", r#"{"content":"hello","blobName":"test.txt"}"#),
        ("send-to-bus",      r#"{"body":"hello","messageType":"Test"}"#),
    ] {
        let url = format!(
            "http://localhost:7071/runtime/webhooks/workflow/api/management\
             /workflows/{workflow}/triggers/manual/run\
             ?api-version=2019-10-01-preview",
        );
        let cmd = crate::services::azure_cli::az_command(&[
            "rest", "--method", "post", "--url", &url,
            "--body", body, "--skip-authorization-header",
        ]);

        #[cfg(not(target_os = "windows"))]
        {
            use std::ffi::OsStr;
            assert_eq!(cmd.get_program(), OsStr::new("az"));
            let args: Vec<&OsStr> = cmd.get_args().collect();
            assert!(args.contains(&OsStr::new("rest")));
            assert!(args.contains(&OsStr::new("--skip-authorization-header")));
            assert!(args.iter().any(|a| a.to_string_lossy().contains(workflow)));
        }
    }
}

// ── ais-runner HTTP trigger ───────────────────────────────────────────────

#[tokio::test]
async fn trigger_workflow_via_ais_runner() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let mut buf = vec![0u8; 4096];
            let _ = stream.read(&mut buf).await;
            let _ = stream
                .write_all(
                    b"HTTP/1.1 202 Accepted\r\n\
                      x-ms-workflow-run-id: run-abc123\r\n\
                      Content-Length: 0\r\n\
                      \r\n",
                )
                .await;
        }
    });

    let callback_url = format!("http://127.0.0.1:{}/api/trigger?sig=mock", port);
    let run_id = trigger_workflow(&callback_url, r#"{"message":"hello","id":"TEST-001"}"#)
        .await
        .unwrap();
    assert_eq!(run_id, "run-abc123");
}
