//! Names AIS Runner to desktop-kit, the code every mayorana desktop app
//! shares, and lends it this app's HTTP client.
//!
//! Only the kit's update check and in-place install are used here: its user
//! interface needs a newer Dioxus than this app runs, and AIS Runner keeps
//! its own usage statistics, notice and MCP server (`ais-mcp`).

use std::future::Future;
use std::pin::Pin;

use desktop_kit::http::{Method, Request, Response};

/// First thing in `main`: until then, the kit checks for nothing.
pub fn init() {
    desktop_kit::init(desktop_kit::App {
        id: "ais-runner",
        name: "AIS Runner",
        version: env!("CARGO_PKG_VERSION"),
        telemetry_folder: "ais-runner",
        product: "ais-runner",
        // No licence: the kit's licence code is never called.
        licence_file: || None,
        http,
    });
}

fn http(req: Request) -> Pin<Box<dyn Future<Output = Option<Response>> + Send>> {
    Box::pin(async move {
        let client = reqwest::Client::new();
        let mut request = match req.method {
            Method::Get => client.get(&req.url),
            Method::Post => client.post(&req.url),
        }
        .header(reqwest::header::USER_AGENT, req.user_agent)
        .timeout(req.timeout);
        for (name, value) in req.headers {
            request = request.header(name, value);
        }
        if let Some(json) = req.json {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(json);
        }
        let response = request.send().await.ok()?;
        let status = response.status().as_u16();
        let body = response.text().await.ok()?;
        Some(Response { status, body })
    })
}
