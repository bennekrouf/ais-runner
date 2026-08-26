//! Azurite storage-event extraction: filter debug.log lines to a run's time
//! window and condense verbose middleware logs into per-request summaries.

use crate::components::log_panel::is_azurite_poll_noise;
use std::collections::HashMap;

// ── Azurite storage-event extraction ──────────────────────────────────────
//
// Azurite never names the action — its debug.log only knows about HTTP calls
// to its Blob/Queue/Table endpoints. Correlation is by time window only: for
// each run we have a UTC start_time and (once finished) end_time, and the
// Azurite line prefix is an ISO8601 UTC timestamp like `2024-01-15T10:23:45.123Z`.
// We filter to that window, drop the poll heartbeats, and return what's left.

/// Parse the leading ISO8601 UTC timestamp from an azurite debug.log line.
/// Format: `2024-01-15T10:23:45.123Z 127.0.0.1 - - [...]`.
fn parse_az_timestamp(line: &str) -> Option<&str> {
    let sp = line.find(' ')?;
    let ts = &line[..sp];
    if ts.len() >= 20 && ts.as_bytes().get(10) == Some(&b'T') && ts.ends_with('Z') {
        Some(ts)
    } else {
        None
    }
}

/// Extract the HTTP status code from an azurite access-log line.
/// The line ends with `... HTTP/1.1" 409 -` (or `... 409 -` without quotes).
fn az_status_code(line: &str) -> Option<u16> {
    let s = line.trim_end();
    let s = s.strip_suffix(" -").unwrap_or(s);
    let last_sp = s.rfind(' ')?;
    s[last_sp + 1..].parse().ok()
}

/// Filter az_lines to the (run_start..=run_end) window, dropping poll noise.
/// `run_end` is `None` for in-flight runs — we include everything from start.
pub fn storage_events_for_run(
    az_lines: &[String],
    run_start: &str,
    run_end: Option<&str>,
) -> Vec<String> {
    az_lines
        .iter()
        .filter(|l| {
            let Some(ts) = parse_az_timestamp(l) else {
                return false;
            };
            if ts < run_start {
                return false;
            }
            if let Some(end) = run_end {
                if ts > end {
                    return false;
                }
            }
            !is_azurite_poll_noise(l)
        })
        .cloned()
        .collect()
}

// ── Storage-request summaries ───────────────────────────────────────────────
//
// Azurite's debug.log logs ~20 verbose middleware lines per HTTP request
// (auth validation, deserialization, dispatch…). Rendering them raw floods
// the strip with noise. Group lines by the request id (2nd token) and emit
// one summary per request: time, method, path, status, storage error code.

/// One storage request, condensed from its middleware log lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageRequestSummary {
    pub time: String,
    pub method: String,
    pub path: String,
    pub status: Option<u16>,
    pub error_code: Option<String>,
}

impl StorageRequestSummary {
    /// A 4xx/5xx that is routine Logic Apps ↔ Azurite background chatter,
    /// not a real failure: the runtime probes run/history tables before
    /// they've been lazily created and gets 404 TableNotFound until then.
    pub fn is_benign_error(&self) -> bool {
        self.status == Some(404) && self.error_code.as_deref() == Some("TableNotFound")
    }

    /// A 4xx/5xx worth alerting on.
    pub fn is_real_error(&self) -> bool {
        matches!(self.status, Some(s) if s >= 400) && !self.is_benign_error()
    }
}

/// Condense raw Azurite debug.log lines into per-request summaries,
/// preserving first-seen request order. Handles both log shapes:
/// the verbose middleware format (`<ts> <request-id> <level>: Component: …`)
/// and the legacy access-log format (`… "GET /path HTTP/1.1" 409 -`).
pub fn summarize_storage_events(lines: &[String]) -> Vec<StorageRequestSummary> {
    let mut order: Vec<String> = Vec::new();
    let mut by_id: HashMap<String, StorageRequestSummary> = HashMap::new();

    for line in lines {
        // Legacy access-log shape: keep as a standalone summary.
        if line.contains("HTTP/1.1") {
            if let Some(s) = summarize_access_log_line(line) {
                let key = format!("access-{}", order.len());
                order.push(key.clone());
                by_id.insert(key, s);
            }
            continue;
        }
        // Middleware shape: `<ts> <request-id> <level>: …`
        let mut parts = line.splitn(3, ' ');
        let (Some(ts), Some(rid), Some(rest)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if !ts.ends_with('Z') || rid.len() < 8 {
            continue;
        }

        if let Some(pos) = rest.find("RequestMethod=") {
            let method = rest[pos + "RequestMethod=".len()..]
                .split_whitespace()
                .next()
                .unwrap_or("?")
                .to_string();
            let path = rest
                .find("RequestURL=")
                .map(|p| {
                    shorten_storage_url(
                        rest[p + "RequestURL=".len()..]
                            .split_whitespace()
                            .next()
                            .unwrap_or(""),
                    )
                })
                .unwrap_or_default();
            let entry = by_id.entry(rid.to_string()).or_insert_with(|| {
                order.push(rid.to_string());
                StorageRequestSummary {
                    time: short_time(ts),
                    method: String::new(),
                    path: String::new(),
                    status: None,
                    error_code: None,
                }
            });
            entry.method = method;
            entry.path = path;
        } else if let Some(pos) = rest.find("StatusCode=") {
            if rest.contains("End response") || rest.contains("ErrorHTTPStatusCode") {
                if let Ok(code) = rest[pos + "StatusCode=".len()..]
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .parse::<u16>()
                {
                    if let Some(entry) = by_id.get_mut(rid) {
                        entry.status.get_or_insert(code);
                    }
                }
            }
        } else if let Some(pos) = rest.find("x-ms-error-code=") {
            let code = rest[pos + "x-ms-error-code=".len()..]
                .split(|c: char| c.is_whitespace() || c == '"' || c == ',')
                .next()
                .unwrap_or("")
                .to_string();
            if !code.is_empty() {
                if let Some(entry) = by_id.get_mut(rid) {
                    entry.error_code.get_or_insert(code);
                }
            }
        }
    }

    order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        // A request we only saw fragments of (no method AND no status) is noise.
        .filter(|s| !s.method.is_empty() || s.status.is_some())
        .collect()
}

/// Legacy Apache-style access-log line → summary (best effort).
fn summarize_access_log_line(line: &str) -> Option<StorageRequestSummary> {
    let status = az_status_code(line)?;
    // `… "GET /devstoreaccount1/… HTTP/1.1" 409 -`
    let q1 = line.find('"')?;
    let req = &line[q1 + 1..];
    let mut it = req.split_whitespace();
    let method = it.next()?.to_string();
    let path = shorten_storage_url(it.next().unwrap_or(""));
    let time = parse_az_timestamp(line).map(short_time).unwrap_or_default();
    Some(StorageRequestSummary {
        time,
        method,
        path,
        status: Some(status),
        error_code: None,
    })
}

/// `http://127.0.0.1:10002/devstoreaccount1/flowXXXruns?$filter=…`
/// → `flowXXXruns` — the table/container is what the user cares about.
fn shorten_storage_url(url: &str) -> String {
    let no_query = url.split('?').next().unwrap_or(url);
    let path = no_query
        .find("://")
        .and_then(|i| no_query[i + 3..].find('/').map(|j| &no_query[i + 3 + j..]))
        .unwrap_or(no_query);
    path.trim_start_matches('/')
        .trim_start_matches("devstoreaccount1")
        .trim_start_matches('/')
        .to_string()
}

/// `2026-07-08T14:45:49.683Z` → `14:45:49`
fn short_time(ts: &str) -> String {
    ts.get(11..19).unwrap_or(ts).to_string()
}

#[cfg(test)]
mod storage_summary_tests {
    use super::*;

    fn middleware_request_lines() -> Vec<String> {
        vec![
            "2026-07-08T14:45:49.683Z 6e523f6e-4797-4931-acff-a659cee5d354 info: TableStorageContextMiddleware: RequestMethod=GET RequestURL=http://127.0.0.1/devstoreaccount1/flow8261runs?$filter=x RequestHeaders:{} ClientIP=127.0.0.1".into(),
            "2026-07-08T14:45:49.683Z 6e523f6e-4797-4931-acff-a659cee5d354 verbose: DispatchMiddleware: Dispatching request...".into(),
            "2026-07-08T14:45:49.683Z 6e523f6e-4797-4931-acff-a659cee5d354 error: ErrorMiddleware: Set HTTP Header: x-ms-error-code=TableNotFound".into(),
            "2026-07-08T14:45:49.683Z 6e523f6e-4797-4931-acff-a659cee5d354 info: EndMiddleware: End response. TotalTimeInMS=0 StatusCode=404 StatusMessage=Not Found Headers={}".into(),
        ]
    }

    #[test]
    fn condenses_middleware_lines_into_one_request() {
        let s = summarize_storage_events(&middleware_request_lines());
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].method, "GET");
        assert_eq!(s[0].path, "flow8261runs");
        assert_eq!(s[0].status, Some(404));
        assert_eq!(s[0].error_code.as_deref(), Some("TableNotFound"));
        assert_eq!(s[0].time, "14:45:49");
    }

    #[test]
    fn tablenotfound_404_is_benign_not_real_error() {
        let s = summarize_storage_events(&middleware_request_lines());
        assert!(s[0].is_benign_error());
        assert!(!s[0].is_real_error());
    }

    #[test]
    fn conflict_409_is_a_real_error() {
        let lines = vec![
            "2026-07-08T14:45:49.100Z aaaa0000-dd89-4535-a5d0-4365ab3ec38d info: TableStorageContextMiddleware: RequestMethod=POST RequestURL=http://127.0.0.1/devstoreaccount1/mytable".into(),
            "2026-07-08T14:45:49.101Z aaaa0000-dd89-4535-a5d0-4365ab3ec38d info: EndMiddleware: End response. TotalTimeInMS=1 StatusCode=409 StatusMessage=Conflict".into(),
        ];
        let s = summarize_storage_events(&lines);
        assert_eq!(s.len(), 1);
        assert!(s[0].is_real_error());
    }

    #[test]
    fn success_requests_summarized_without_error() {
        let lines = vec![
            "2026-07-08T14:45:49.100Z bbbb0000-dd89-4535-a5d0-4365ab3ec38d info: TableStorageContextMiddleware: RequestMethod=GET RequestURL=http://127.0.0.1/devstoreaccount1/flows".into(),
            "2026-07-08T14:45:49.101Z bbbb0000-dd89-4535-a5d0-4365ab3ec38d info: EndMiddleware: End response. TotalTimeInMS=1 StatusCode=200 StatusMessage=OK Headers={}".into(),
        ];
        let s = summarize_storage_events(&lines);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].status, Some(200));
        assert!(!s[0].is_real_error() && !s[0].is_benign_error());
    }

    #[test]
    fn legacy_access_log_lines_still_summarized() {
        let lines = vec![
            r#"2026-07-08T14:45:49.100Z 127.0.0.1 - - [08/Jul/2026] "PUT /devstoreaccount1/table1 HTTP/1.1" 409 -"#.into(),
        ];
        let s = summarize_storage_events(&lines);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].method, "PUT");
        assert_eq!(s[0].path, "table1");
        assert_eq!(s[0].status, Some(409));
        assert!(s[0].is_real_error());
    }

    #[test]
    fn fragment_only_requests_are_dropped() {
        // Lines whose request never shows a method or status (mid-request
        // fragments caught at the buffer edge) are noise.
        let lines = vec![
            "2026-07-08T14:45:49.683Z cccc0000-4797-4931-acff-a659cee5d354 verbose: SerializerMiddleware: Start serializing...".into(),
        ];
        assert!(summarize_storage_events(&lines).is_empty());
    }
}
