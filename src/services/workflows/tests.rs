use super::*;

// Fixture project at test/fixtures/hello-world/ — open that folder in AIS Runner
// to manually exercise the same workflows these tests cover.
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixtures/hello-world");

// ── 1. all three workflows load from fixture ──────────────────────────────

#[test]
fn load_all_workflows_from_fixture() {
    let items = scan_local_workflows(FIXTURE);
    assert_eq!(items.len(), 3);

    let names: Vec<&str> = items.iter().map(|w| w.name.as_str()).collect();
    assert!(names.contains(&"hello-world"),    "missing hello-world");
    assert!(names.contains(&"write-to-storage"), "missing write-to-storage");
    assert!(names.contains(&"send-to-bus"),    "missing send-to-bus");

    for wf in &items {
        assert_eq!(wf.trigger_name, "manual");
        assert_eq!(wf.trigger_type, "Request");
        assert!(wf.healthy);
    }
}

// ── 2. payload skeletons match each workflow's trigger schema ─────────────

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
    assert!(v["body"].is_string());
    assert!(v["messageType"].is_string());
}

fn payload_json(workflow: &str) -> serde_json::Value {
    let raw = crate::services::payload::suggest_payload(FIXTURE, workflow);
    serde_json::from_str(&raw).expect("suggest_payload returned invalid JSON")
}

// ── 3. az trigger command — verify args without running az ────────────────
//
//  Equivalent manual commands (with func start running in the fixture dir):
//
//  az rest --method post \
//    --url "http://localhost:7071/.../workflows/hello-world/triggers/manual/run" \
//    --body '{"message":"hi","id":"T-1"}' --skip-authorization-header
//
//  az rest --method post \
//    --url "http://localhost:7071/.../workflows/write-to-storage/triggers/manual/run" \
//    --body '{"content":"hello","blobName":"test.txt"}' --skip-authorization-header
//
//  az rest --method post \
//    --url "http://localhost:7071/.../workflows/send-to-bus/triggers/manual/run" \
//    --body '{"body":"hello","messageType":"Test"}' --skip-authorization-header

#[test]
fn az_trigger_command_args() {
    for (workflow, body) in [
        ("hello-world",     r#"{"message":"hi","id":"T-1"}"#),
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

// ── 4. trigger via ais-runner code — mock HTTP server ────────────────────

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
