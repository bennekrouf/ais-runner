//! Embedded HTTP server that returns mock responses to workflows.
//!
//! Lifecycle
//! ---------
//! ```ignore
//! let server = MockServer::start(contract, workspace, bus.clone()).await?;
//! // workflows hit http://localhost:<server.port>/__mock__/<setting>/...
//! server.stop().await;
//! ```
//!
//! Behaviour
//! ---------
//! Every incoming request:
//!   1. Path starts with `/__mock__/<setting>/<rest>` (set by `rewrite::rewrite`)
//!   2. Router matches `<rest>` against the contract's endpoints (with the
//!      original URL host known from `<setting>`).
//!   3. If matched → return `endpoint.response.example` with `endpoint.response.status_code`.
//!   4. If not matched → 404 + log a `NotInContract` event.
//!
//! All requests and responses are published on the `EventBus` so the Console
//! panel can show live traffic.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::IntoResponse,
    Json, Router as AxumRouter,
};
use tokio::sync::oneshot;

use crate::services::mock::contract::MockContract;
use crate::services::mock::events::{EventBus, LogLevel, MockEvent, ResponseSource};
use crate::services::mock::rewrite::parse_mock_path;
use crate::services::mock::router::{Incoming, Router as RouteTable};

pub struct MockServer {
    pub port: u16,
    shutdown: Option<oneshot::Sender<()>>,
    handle: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct AppState {
    routes: Arc<RouteTable>,
    bus: EventBus,
    contract: Arc<MockContract>,
}

impl MockServer {
    /// Bind on `127.0.0.1:0` (any free port), spawn the server task, return.
    /// The chosen port is exposed via `server.port`.
    pub async fn start(contract: MockContract, bus: EventBus) -> std::io::Result<Self> {
        let routes = Arc::new(RouteTable::from_contract(&contract));
        let state = AppState {
            routes: routes.clone(),
            bus: bus.clone(),
            contract: Arc::new(contract),
        };

        // Single catch-all handler — we route ourselves rather than relying on
        // axum's matchit so the contract's `{X}` patterns work uniformly.
        let app = AxumRouter::new().fallback(handle_request).with_state(state);

        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
        let port = listener.local_addr()?.port();

        let (tx_shutdown, rx_shutdown) = oneshot::channel::<()>();

        bus.publish(MockEvent::ServerStarted { port });

        let handle = tokio::spawn(async move {
            let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
                let _ = rx_shutdown.await;
            });
            let _ = serve.await;
        });

        let _ = routes; // suppress unused warning when no routes match
        Ok(Self {
            port,
            shutdown: Some(tx_shutdown),
            handle,
        })
    }

    /// Trigger graceful shutdown and await the server task.
    pub async fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = self.handle.await;
    }
}

/// Single catch-all axum handler.
async fn handle_request(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let request_id = new_request_id();
    let start = Instant::now();
    let path = uri.path();
    let _ = headers; // unused for now; reserved for future auth/header capture

    // Try to parse the mock prefix to learn which logical setting this targets.
    let (setting_name, inner_path) = match parse_mock_path(path) {
        Some(parts) => parts,
        None => {
            // Request didn't come through the rewrite — could be a stray
            // hardcoded URL pointing at the mock port. Log + 404.
            state.bus.log(
                LogLevel::Warn,
                format!("request without /__mock__/ prefix: {} {}", method, path),
            );
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": "request not routed through rewrite layer",
                    "path":  path,
                })),
            )
                .into_response();
        }
    };

    // Publish the request event before any matching attempt.
    let body_json = serde_json::from_slice::<serde_json::Value>(&body).ok();
    state.bus.publish(MockEvent::Request {
        id: request_id.clone(),
        method: method.as_str().to_string(),
        url: uri.to_string(),
        target: Some(format!("(via setting {}){}", setting_name, inner_path)),
        body: body_json,
    });

    // Route the *inner* path (after the /__mock__/<setting> prefix is stripped).
    let routed = state.routes.route(&Incoming {
        method: method.as_str(),
        path: inner_path,
    });

    let (status, source, body): (StatusCode, ResponseSource, serde_json::Value) = match routed {
        Some(endpoint) => {
            let code =
                StatusCode::from_u16(endpoint.response.status_code).unwrap_or(StatusCode::OK);
            (
                code,
                ResponseSource::AutoStub,
                endpoint.response.example.clone(),
            )
        }
        None => (
            StatusCode::NOT_FOUND,
            ResponseSource::NotInContract,
            serde_json::json!({
                "error":  "no endpoint in contract matches this request",
                "method": method.as_str(),
                "path":   inner_path,
                "hint":   "did you rebuild the mock contract after editing the workflow?",
            }),
        ),
    };

    let elapsed_ms = start.elapsed().as_millis() as u64;
    let body_str = body.to_string();
    let body_excerpt = if body_str.len() > 240 {
        Some(format!("{}…", &body_str[..240]))
    } else {
        Some(body_str.clone())
    };

    state.bus.publish(MockEvent::Response {
        id: request_id,
        status: status.as_u16(),
        source,
        elapsed_ms,
        body_excerpt,
    });

    (status, Json(body)).into_response()
}

/// Short, sortable request id — first 8 hex chars of a wall-clock nanos value.
fn new_request_id() -> String {
    let ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{:016x}", ns)
}
