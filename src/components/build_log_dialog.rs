use crate::utils::open_in_browser;
use ais_core::cli::AzError;
use ais_core::devops_cli::{self, BuildJob, TimelineRecord};
use dioxus::prelude::*;

/// Longest log shown in full; beyond this only the end is kept, which is where
/// a failing step says why.
const MAX_LOG_LINES: usize = 5000;

fn step_icon(r: &TimelineRecord) -> &'static str {
    match r.state.as_str() {
        "inProgress" => "⏳",
        "pending" => "⬜",
        _ => match r.result.as_deref() {
            Some("succeeded") => "✅",
            Some("succeededWithIssues") | Some("partiallySucceeded") => "⚠️",
            Some("failed") => "❌",
            Some("canceled") | Some("abandoned") => "🚫",
            Some("skipped") => "⏭",
            _ => "⬜",
        },
    }
}

fn duration(r: &TimelineRecord) -> String {
    let parse = |s: &Option<String>| {
        s.as_deref()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
    };
    let (Some(start), Some(end)) = (parse(&r.start_time), parse(&r.finish_time)) else {
        return String::new();
    };
    let secs = (end - start).num_seconds().max(0);
    if secs < 60 {
        format!("{}s", secs)
    } else {
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

/// The step to open first: the first failure, else whatever is still running,
/// else the last step that has a log.
fn initial_step(jobs: &[BuildJob]) -> Option<TimelineRecord> {
    let steps = || {
        jobs.iter()
            .flat_map(|j| j.steps.iter())
            .filter(|s| s.log_id.is_some())
    };
    steps()
        .find(|s| s.result.as_deref() == Some("failed"))
        .or_else(|| steps().find(|s| s.state == "inProgress"))
        .or_else(|| steps().last())
        .cloned()
}

/// A log line without its timestamp, and the CSS class for its `##[…]` marker.
fn log_line(raw: &str) -> (&'static str, &str) {
    // "2026-10-07T09:17:59.7531762Z text"
    let line = match raw.split_once(' ') {
        Some((ts, rest)) if ts.len() >= 20 && ts.ends_with('Z') && ts.as_bytes()[10] == b'T' => {
            rest
        }
        _ => raw,
    };
    for (marker, class) in [
        ("##[error]", "bl-line bl-error"),
        ("##[warning]", "bl-line bl-warning"),
        ("##[section]", "bl-line bl-section"),
        ("##[command]", "bl-line bl-command"),
        ("##[group]", "bl-line bl-section"),
        ("##[debug]", "bl-line bl-debug"),
    ] {
        if let Some(rest) = line.strip_prefix(marker) {
            return (class, rest);
        }
    }
    if line.starts_with("##[endgroup]") {
        return ("bl-line bl-debug", "");
    }
    ("bl-line", line)
}

fn fmt_error(e: &AzError) -> String {
    match e {
        AzError::NotLoggedIn => {
            "Azure session expired — click ⟳ in the header to re-authenticate.".into()
        }
        AzError::Other(msg) => format!("Error: {}", msg),
    }
}

#[derive(Props, Clone, PartialEq)]
pub struct BuildLogDialogProps {
    pub org: String,
    pub project: String,
    pub build_id: u64,
    pub build_name: String,
    pub on_close: EventHandler<()>,
}

#[component]
pub fn BuildLogDialog(props: BuildLogDialogProps) -> Element {
    let mut jobs: Signal<Option<Result<Vec<BuildJob>, String>>> = use_signal(|| None);
    let mut selected: Signal<Option<TimelineRecord>> = use_signal(|| None);
    let mut log: Signal<Option<Result<String, String>>> = use_signal(|| None);

    let org = props.org.clone();
    let project = props.project.clone();
    let build_id = props.build_id;

    let open_step = {
        let org = org.clone();
        let project = project.clone();
        move |step: TimelineRecord| {
            let Some(log_id) = step.log_id else { return };
            selected.set(Some(step));
            log.set(None);
            let (o, p) = (org.clone(), project.clone());
            spawn(async move {
                let res = tokio::task::spawn_blocking(move || {
                    devops_cli::build_log(&o, &p, build_id, log_id)
                })
                .await
                .unwrap_or_else(|e| Err(AzError::Other(e.to_string())));
                // ignore a slow answer for a step the user already moved away from
                if selected.peek().as_ref().and_then(|s| s.log_id) == Some(log_id) {
                    log.set(Some(res.map_err(|e| fmt_error(&e))));
                }
            });
        }
    };

    let load_timeline = {
        let org = org.clone();
        let project = project.clone();
        let open_step = open_step.clone();
        move || {
            jobs.set(None);
            let (o, p) = (org.clone(), project.clone());
            let mut open_step = open_step.clone();
            spawn(async move {
                let res = tokio::task::spawn_blocking(move || {
                    devops_cli::build_timeline(&o, &p, build_id)
                })
                .await
                .unwrap_or_else(|e| Err(AzError::Other(e.to_string())));
                match res {
                    Ok(list) => {
                        // keep the open step on refresh, if it still exists
                        let keep = selected.peek().as_ref().and_then(|cur| {
                            list.iter()
                                .flat_map(|j| j.steps.iter())
                                .find(|s| s.id == cur.id)
                                .cloned()
                        });
                        let first = keep.or_else(|| initial_step(&list));
                        jobs.set(Some(Ok(list)));
                        if let Some(step) = first {
                            open_step(step);
                        }
                    }
                    Err(e) => jobs.set(Some(Err(fmt_error(&e)))),
                }
            });
        }
    };

    let mut started = use_signal(|| false);
    let mut first_load = load_timeline.clone();
    use_effect(move || {
        if !*started.peek() {
            started.set(true);
            first_load();
        }
    });
    let mut refresh = load_timeline.clone();

    let web_url = devops_cli::build_web_url(&org, &project, build_id);
    let on_close = props.on_close;
    let sel = selected.read().clone();

    rsx! {
        div { id: "dialog-backdrop", onclick: move |_| on_close.call(()) }
        div { id: "build-log-dialog",
            div { id: "run-dialog-header",
                div {
                    h3 { "Build #{props.build_name}" }
                    span { class: "dialog-hint", "Steps and logs from Azure DevOps." }
                }
                div { style: "display:flex; gap:6px; align-items:center",
                    button {
                        class: "btn btn-small",
                        title: "Fetch the steps and log again",
                        onclick: move |_| refresh(),
                        "⟳ Refresh"
                    }
                    button {
                        class: "btn btn-small",
                        onclick: move |_| open_in_browser(&web_url),
                        "Open in browser"
                    }
                    button { class: "btn-icon", onclick: move |_| on_close.call(()), "×" }
                }
            }
            div { class: "bl-body",
                // ── steps ─────────────────────────────────────────────────
                div { class: "bl-steps",
                    match &*jobs.read() {
                        None => rsx! { div { class: "devops-empty", "Loading steps…" } },
                        Some(Err(e)) => rsx! { div { class: "devops-empty bl-error", "{e}" } },
                        Some(Ok(list)) if list.is_empty() => rsx! {
                            div { class: "devops-empty", "This build has no steps yet." }
                        },
                        Some(Ok(list)) => rsx! {
                            for job in list.iter() {
                                div { class: "bl-job",
                                    div { class: "bl-job-name",
                                        span { "{step_icon(&job.job)}" }
                                        span {
                                            if let Some(stage) = job.stage.as_ref().filter(|s| **s != job.job.name) {
                                                span { class: "bl-stage", "{stage} › " }
                                            }
                                            "{job.job.name}"
                                        }
                                    }
                                    for step in job.steps.iter() {
                                        {
                                            let step2 = step.clone();
                                            let mut open = open_step.clone();
                                            let has_log = step.log_id.is_some();
                                            let is_sel = sel.as_ref().map(|s| s.id == step.id).unwrap_or(false);
                                            let class = match (is_sel, has_log) {
                                                (true, _) => "bl-step selected",
                                                (false, true) => "bl-step",
                                                (false, false) => "bl-step no-log",
                                            };
                                            rsx! {
                                                div {
                                                    class,
                                                    title: if has_log { "" } else { "This step has no log" },
                                                    onclick: move |_| open(step2.clone()),
                                                    span { class: "bl-step-icon", "{step_icon(step)}" }
                                                    span { class: "bl-step-name", "{step.name}" }
                                                    span { class: "bl-step-time", "{duration(step)}" }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        },
                    }
                }
                // ── log of the selected step ──────────────────────────────
                div { class: "bl-log",
                    if let Some(step) = sel.as_ref() {
                        div { class: "bl-log-title",
                            "{step_icon(step)} {step.name}"
                            span { class: "bl-step-time", "{duration(step)}" }
                        }
                        for issue in step.issues.iter() {
                            div {
                                class: if issue.kind == "error" { "bl-issue bl-error" } else { "bl-issue bl-warning" },
                                "{issue.message}"
                            }
                        }
                        match &*log.read() {
                            None => rsx! { div { class: "devops-empty", "Loading log…" } },
                            Some(Err(e)) => rsx! { div { class: "devops-empty bl-error", "{e}" } },
                            Some(Ok(text)) => {
                                let lines: Vec<&str> = text.lines().collect();
                                let skipped = lines.len().saturating_sub(MAX_LOG_LINES);
                                rsx! {
                                    pre { class: "bl-text",
                                        if skipped > 0 {
                                            div { class: "bl-line bl-debug",
                                                "… {skipped} earlier lines not shown — open in browser for the full log"
                                            }
                                        }
                                        for raw in lines[skipped..].iter() {
                                            {
                                                let (class, line) = log_line(raw);
                                                rsx! { div { class, "{line}" } }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else if matches!(&*jobs.read(), Some(Ok(_))) {
                        div { class: "devops-empty", "← Pick a step to see its log" }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_timestamps_and_classifies_markers() {
        assert_eq!(
            log_line("2026-10-07T09:17:59.7531762Z ##[error]Bash exited with code '1'."),
            ("bl-line bl-error", "Bash exited with code '1'.")
        );
        assert_eq!(
            log_line("2026-10-07T09:17:59.7627005Z Task         : Get sources"),
            ("bl-line", "Task         : Get sources")
        );
        assert_eq!(
            log_line("no timestamp here"),
            ("bl-line", "no timestamp here")
        );
    }

    #[test]
    fn opens_the_first_failed_step() {
        let step = |id: &str, result: &str| TimelineRecord {
            id: id.into(),
            kind: "Task".into(),
            state: "completed".into(),
            result: Some(result.into()),
            log_id: Some(1),
            ..Default::default()
        };
        let jobs = vec![BuildJob {
            stage: None,
            job: TimelineRecord::default(),
            steps: vec![
                step("a", "succeeded"),
                step("b", "failed"),
                step("c", "skipped"),
            ],
        }];
        assert_eq!(initial_step(&jobs).map(|s| s.id), Some("b".into()));
    }
}
