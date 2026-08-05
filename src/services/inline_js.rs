//! Inline-JavaScript ("Execute JavaScript Code") awareness.
//!
//! The `JavaScriptCode` action runs inside the Functions **Node language
//! worker** — a separate process the host starts on a random localhost port.
//! When its prerequisites are missing, a run fails with the cryptic
//! `No connection could be made because the target machine actively refused it
//! (localhost:xxxxx)`. We can't start that worker for the user, but we can flag
//! the prerequisites up front and translate the error when it appears.

/// Names of workflows that contain an inline-JavaScript (`JavaScriptCode`) action.
pub fn workflows_with_inline_js(logic_apps_dir: &str) -> Vec<String> {
    let dir = crate::services::workflows::resolve_logic_apps_dir(logic_apps_dir);
    let mut hits = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let wf = e.path().join("workflow.json");
            if !wf.is_file() {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&wf) {
                if text.contains("\"JavaScriptCode\"") {
                    if let Some(name) = e.file_name().to_str() {
                        hits.push(name.to_string());
                    }
                }
            }
        }
    }
    hits.sort();
    hits
}

/// True for the runtime error a missing/failed inline-JS worker produces.
pub fn is_inline_js_worker_error(line: &str) -> bool {
    let l = line.to_lowercase();
    l.contains("actively refused")
        || (l.contains("no connection could be made") && l.contains("localhost"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_worker_refusal() {
        assert!(is_inline_js_worker_error(
            "System.Net.Http: No connection could be made because the target machine actively refused it (localhost:52344)"
        ));
        assert!(is_inline_js_worker_error("connect ECONNREFUSED — actively refused"));
        assert!(!is_inline_js_worker_error("Loaded 10 workflows"));
        assert!(!is_inline_js_worker_error("Host unavailable after check"));
    }
}
