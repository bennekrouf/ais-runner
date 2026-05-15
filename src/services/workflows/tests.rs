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
    // Count dynamically — optional workflows (write-to-cosmos etc.) may come and go.
    assert!(items.len() >= 3, "expected at least hello-world, write-to-storage, send-to-bus");

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
        ("send-to-bus",      r#"{"body":"hello","messageType":"Test","queueName":"q"}"#),
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

// ── blob-rename-rewrite workflow ──────────────────────────────────────────

#[test]
fn blob_rename_rewrite_detected_as_blob_trigger() {
    let wf_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/test/logic-apps/blob-rename-rewrite/workflow.json"
    );
    let json = std::fs::read_to_string(wf_path).expect("workflow.json not found");
    let info = read_blob_trigger_info(&json);
    assert!(info.is_some(), "should detect blob trigger");
    let (container, conn) = info.unwrap();
    assert_eq!(container, "rename-input");
    assert_eq!(conn, "AzureBlob");
}

#[test]
fn blob_rename_rewrite_containers_extracted() {
    let wf_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/test/logic-apps/blob-rename-rewrite/workflow.json"
    );
    let json = std::fs::read_to_string(wf_path).expect("workflow.json not found");
    let containers = extract_all_blob_containers(&json);
    assert!(containers.contains(&"rename-input".to_string()),  "missing rename-input");
    assert!(containers.contains(&"rename-output".to_string()), "missing rename-output");
}

// Integration: blob-rename-rewrite against local Azurite + func start
//
// Prerequisites:
//   azurite running (⟳ Reset Azurite in ais-runner, or `azurite --silent &`)
//   func start running in test/logic-apps/
//   containers 'rename-input' and 'rename-output' created in Azurite
//
// Run with:  cargo test blob_rename_rewrite_emulator -- --ignored

#[tokio::test]
#[ignore = "requires Azurite on :10000 and func start running in test/logic-apps/"]
async fn blob_rename_rewrite_emulator() {
    use crate::services::azurite_client;

    let original_name = "hello.txt";
    let expected_name = format!("processed_{}", original_name);
    let original_content = "hello from ais-runner test";

    // Upload source file to rename-input
    azurite_client::upload_blob_bytes_sync(
        "rename-input",
        original_name,
        original_content.as_bytes().to_vec(),
    ).expect("upload to rename-input failed — is Azurite running?");

    // Poll run history for up to 60 s
    let workflow = "blob-rename-rewrite";
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        if let Ok(runs) = list_runs(workflow).await {
            if let Some(latest) = runs.first() {
                let status = latest.properties.status.to_lowercase();
                if status == "succeeded" {
                    // Verify renamed file exists in rename-output
                    let out_blobs = azurite_client::list_blobs("rename-output")
                        .expect("failed to list rename-output");
                    assert!(
                        out_blobs.iter().any(|b| b.name == expected_name),
                        "expected '{}' in rename-output, got: {:?}",
                        expected_name,
                        out_blobs.iter().map(|b| &b.name).collect::<Vec<_>>()
                    );

                    // Verify original file was deleted from rename-input
                    let in_blobs = azurite_client::list_blobs("rename-input")
                        .expect("failed to list rename-input");
                    assert!(
                        !in_blobs.iter().any(|b| b.name == original_name),
                        "original '{}' should have been deleted from rename-input",
                        original_name
                    );

                    // Verify content was rewritten
                    let tmp = std::env::temp_dir().join("ais_rename_test_out.txt");
                    azurite_client::download_blob("rename-output", &expected_name, &tmp.to_string_lossy())
                        .expect("download failed");
                    let written = std::fs::read_to_string(&tmp).expect("read failed");
                    assert!(
                        written.contains("[rewritten]"),
                        "content should start with '[rewritten]', got: {:?}", written
                    );
                    assert!(
                        written.contains(original_content),
                        "content should include original text, got: {:?}", written
                    );
                    break;
                }
                if status == "failed" {
                    if let Ok(actions) = list_actions(workflow, &latest.name).await {
                        for act in &actions {
                            if let Some(e) = &act.properties.error {
                                panic!("Action '{}' failed: {:?}", act.name, e.message);
                            }
                        }
                    }
                    panic!("Run failed — check action errors above");
                }
            }
        }
        assert!(tokio::time::Instant::now() < deadline,
            "timed out after 60 s — blob trigger did not fire. \
             Try: ⟳ Reset Azurite → restart func, then re-run.");
    }
}

// ── blob trigger parsing ──────────────────────────────────────────────────

#[test]
fn read_blob_trigger_info_when_blob_is_added_or_modified() {
    let json = r#"{
        "definition": {
            "triggers": {
                "When_a_blob_is_added_or_updated": {
                    "type": "ServiceProvider",
                    "inputs": {
                        "parameters": { "path": "kyriba-input" },
                        "serviceProviderConfiguration": {
                            "connectionName": "IgniteBlob",
                            "operationId": "whenABlobIsAddedOrModified",
                            "serviceProviderId": "/serviceProviders/AzureBlob"
                        }
                    }
                }
            },
            "actions": {}
        }
    }"#;
    let info = read_blob_trigger_info(json);
    assert!(info.is_some(), "should detect blob trigger");
    let (container, conn) = info.unwrap();
    assert_eq!(container, "kyriba-input");
    assert_eq!(conn, "IgniteBlob");
}

#[test]
fn extract_all_blob_containers_kyriba_send() {
    let json = std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"),
                "/../../oryx/ais_tom_platform/logic_apps/Send-Kyriba-files-broadcast/workflow.json")
    );
    // Skip gracefully if the oryx repo is not alongside ais-runner
    let Ok(json) = json else { return };

    let containers = extract_all_blob_containers(&json);
    for expected in &["kyriba-input", "input", "kyriba-archive"] {
        assert!(
            containers.contains(&expected.to_string()),
            "expected container '{}' in {:?}", expected, containers
        );
    }
}

// ── Integration: blob trigger against local Azurite ───────────────────────
//
// Prerequisites (run manually, skip in CI):
//   azurite --silent &   (or use the ais-runner ⟳ Reset button)
//   func start           (in the logic-apps directory for Send-Kyriba-files-broadcast)
//   Create containers: kyriba-input, input, kyriba-archive in Azurite
//
// Run with:  cargo test blob_trigger_emulator -- --ignored

#[tokio::test]
#[ignore = "requires Azurite on :10000 and func start running with Send-Kyriba-files-broadcast"]
async fn blob_trigger_emulator() {
    use crate::services::azurite_client;

    // Upload a test file to kyriba-input
    let content = b"ORYX test payload";
    azurite_client::upload_blob_bytes_sync("kyriba-input", "payments/test.txt", content.to_vec())
        .expect("upload to kyriba-input failed — is Azurite running?");

    // Poll run history for up to 60 s (blob trigger polls every ~30 s)
    let workflow = "Send-Kyriba-files-broadcast";
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        if let Ok(runs) = list_runs(workflow).await {
            if let Some(latest) = runs.first() {
                let status = latest.properties.status.to_lowercase();
                if status == "succeeded" { break; }
                if status == "failed" {
                    if let Ok(actions) = list_actions(workflow, &latest.name).await {
                        for act in &actions {
                            if let Some(e) = &act.properties.error {
                                panic!("Action '{}' failed: {:?}", act.name, e.message);
                            }
                        }
                    }
                    panic!("Run failed — check the log panel for action errors");
                }
            }
        }
        assert!(tokio::time::Instant::now() < deadline,
                "timed out after 60 s — blob trigger did not fire. \
                 Try: ⟳ Reset Azurite → restart func, then re-run.");
    }
}

// ── Integration: write-to-cosmos against local emulator ───────────────────
//
// Prerequisites (run manually, skip in CI):
//   docker run -d -p 8081:8081 -e ACCEPT_EULA=Y mcr.microsoft.com/cosmosdb/linux/azure-cosmos-emulator
//   func start (in test/logic-apps/)
//   create database "test-db" and container "test-container" (partitionKey: /partitionKey) in emulator
//
// Run with:  cargo test cosmos_emulator -- --ignored

#[tokio::test]
#[ignore = "requires Cosmos DB Emulator on :8081 and func start running in test/logic-apps/"]
async fn write_to_cosmos_against_emulator() {
    // Verify emulator is reachable
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)  // emulator uses self-signed cert
        .timeout(std::time::Duration::from_secs(5))
        .build().unwrap();
    let ping = client.get("https://localhost:8081/").send().await;
    assert!(ping.is_ok(), "Cosmos emulator not reachable on :8081 — start it first");

    // Get callback URL from the running func host
    let callback_url = get_callback_url("write-to-cosmos", "manual").await
        .expect("func start must be running in test/logic-apps/");

    // Trigger the workflow
    let payload = r#"{"id":"test-001","partitionKey":"unit-test","data":"hello from ais-runner test"}"#;
    let run_id = trigger_workflow(&callback_url, payload).await
        .expect("trigger failed");
    assert_ne!(run_id, "unknown", "run ID should be returned for a stateful workflow");

    // Poll until terminal (max 30 s)
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        if let Ok(runs) = list_runs("write-to-cosmos").await {
            if let Some(latest) = runs.first() {
                let status = latest.properties.status.to_lowercase();
                if status == "succeeded" { break; }
                if status == "failed" {
                    if let Ok(actions) = list_actions("write-to-cosmos", &latest.name).await {
                        for act in &actions {
                            if let Some(e) = &act.properties.error {
                                panic!("Action '{}' failed: {:?}", act.name, e.message);
                            }
                        }
                    }
                    panic!("Run failed — check the ↳ error in the log panel");
                }
            }
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for run to complete");
    }
}
