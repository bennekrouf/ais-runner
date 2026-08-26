//! Capture side of scenarios — the other half of `scenario.rs`.
//!
//! `scenario.rs` can replay a `Vec<Step>`; until now the only way to *author*
//! one was to hand-write JSON. This module turns ordinary use of the app into
//! that vector: the panels call [`record`] at the success point of each service
//! call, and the recorder appends a step.
//!
//! Two things it does beyond appending:
//!
//! * **Gaps become `Sleep` steps.** A recording captures actions, not the time
//!   between them, and a replay that fires them back-to-back races the pipeline
//!   it is meant to exercise. Anything longer than [`MIN_GAP_MS`] is preserved
//!   as an explicit, editable `Sleep`.
//! * **A `RunWorkflow` is followed by a `WaitForRun`.** The user waited for the
//!   run to finish before doing the next thing; polling for a terminal run
//!   replays that intent far more reliably than the wall-clock gap would.
//!
//! Both are guesses, deliberately visible in the review screen so they can be
//! turned into a real `WaitForMessage`/`WaitForSql` assertion.

use std::path::{Path, PathBuf};
use std::time::Instant;

use dioxus::prelude::*;

use crate::services::scenario::Step;

/// Below this, two actions were part of one burst of typing and clicking —
/// inserting a sleep for every 300ms pause would bury the real steps.
const MIN_GAP_MS: u128 = 1_500;

/// Cap on a captured gap. Someone who starts a recording and goes to lunch
/// should not end up with a 40-minute `Sleep` in a test suite.
const MAX_GAP_MS: u64 = 30_000;

/// Timeout on the `WaitForRun` inserted after a trigger. Longer than the
/// `scenario` default because a cold Functions host takes a while on the first
/// run of a session.
const RUN_WAIT_MS: u64 = 60_000;

/// Where recorded fixtures are copied, relative to the project root.
pub const FIXTURE_DIR: &str = ".ais-runner/fixtures";

/// What the recorder is doing right now.
///
/// The three states are implicit rather than an enum because the UI needs to
/// distinguish them cheaply from a `peek()`: idle (nothing captured), recording
/// (`active`), and review (stopped, with steps still in hand).
#[derive(Clone, Debug, Default)]
pub struct RecorderState {
    pub active: bool,
    pub name: String,
    pub steps: Vec<Step>,
    /// Base for fixture copies and relative paths. Captured at `start` so the
    /// panels doing the recording don't each have to work it out.
    pub project_root: PathBuf,
    /// When the previous step landed, for the gap calculation.
    last_at: Option<Instant>,
    /// Set after an auto-inserted `WaitForRun`: the user's wait for that run is
    /// already represented, so the following gap must not become a `Sleep` too.
    suppress_next_gap: bool,
}

impl RecorderState {
    pub fn is_recording(&self) -> bool {
        self.active
    }

    /// Stopped, but holding steps that haven't been saved or discarded yet.
    pub fn in_review(&self) -> bool {
        !self.active && !self.steps.is_empty()
    }

    pub fn start(&mut self, name: String, project_root: PathBuf) {
        self.active = true;
        self.name = name;
        self.steps.clear();
        self.project_root = project_root;
        self.last_at = None;
        self.suppress_next_gap = false;
    }

    /// Stop capturing but keep the steps, which moves the recorder to review.
    pub fn stop(&mut self) {
        self.active = false;
        self.last_at = None;
    }

    /// Drop everything and go back to idle.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Append `step`, plus whatever synchronisation it implies.
    fn push(&mut self, step: Step) {
        let now = Instant::now();

        if let Some(prev) = self.last_at {
            if !self.suppress_next_gap {
                if let Some(sleep) = gap_sleep(now.duration_since(prev).as_millis()) {
                    self.steps.push(sleep);
                }
            }
        }
        self.suppress_next_gap = false;

        // Grab the workflow name before the move, so the wait can follow it.
        let triggered = match &step {
            Step::RunWorkflow { workflow, .. } => Some(workflow.clone()),
            _ => None,
        };

        self.steps.push(step);

        if let Some(workflow) = triggered {
            self.steps.push(Step::WaitForRun {
                workflow,
                timeout_ms: RUN_WAIT_MS,
                expect_status: "Succeeded".to_string(),
            });
            self.suppress_next_gap = true;
        }

        self.last_at = Some(now);
    }
}

/// The `Sleep` a gap of `gap_ms` deserves, if any.
fn gap_sleep(gap_ms: u128) -> Option<Step> {
    if gap_ms < MIN_GAP_MS {
        return None;
    }
    Some(Step::Sleep {
        ms: (gap_ms as u64).min(MAX_GAP_MS),
    })
}

/// Append a step to the recording in progress.
///
/// A no-op when nothing is being recorded, which is the overwhelmingly common
/// case — so this reads through `peek()` and never subscribes the calling scope
/// to the recorder. Callers invoke it on the success path of an action that has
/// already completed; it does no I/O and cannot fail.
pub fn record(mut recorder: Signal<RecorderState>, step: Step) {
    if !recorder.peek().active {
        return;
    }
    recorder.write().push(step);
}

/// Record an upload, copying the source file into the project so the scenario
/// replays somewhere else.
///
/// The picked file is usually somewhere personal (`~/Downloads/invoice.xml`);
/// storing that path would produce a scenario only its author can run. The copy
/// lands in `.ais-runner/fixtures` next to the scenarios, and the step refers to
/// it by a project-relative path.
///
/// The copy runs on the blocking pool and is awaited before the step is
/// appended, so step order still matches the order the user acted in.
pub async fn record_upload(
    recorder: Signal<RecorderState>,
    container: String,
    source: String,
    blob_name: String,
) {
    if !recorder.peek().active {
        return;
    }
    let root = recorder.peek().project_root.clone();

    let src = source.clone();
    let file = tokio::task::spawn_blocking(move || copy_fixture(&root, &src))
        .await
        .unwrap_or(Err("fixture copy task panicked".to_string()));

    // A failed copy is not worth losing the step over — fall back to the
    // original path, which at least replays on this machine.
    let file = file.unwrap_or(source);

    record(
        recorder,
        Step::UploadFile {
            container,
            file,
            blob_name,
        },
    );
}

/// Copy `source` into `<project_root>/.ais-runner/fixtures`, returning the
/// project-relative path to the copy.
///
/// A file already there with identical contents is reused rather than
/// duplicated — re-recording the same upload shouldn't grow the repo. A
/// *different* file with the same name gets a numbered suffix.
fn copy_fixture(project_root: &Path, source: &str) -> Result<String, String> {
    let src = Path::new(source);
    let name = src
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("unusable file name in '{source}'"))?;

    let dir = project_root.join(FIXTURE_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let bytes = std::fs::read(src).map_err(|e| e.to_string())?;
    let (stem, ext) = split_name(name);

    let mut candidate = dir.join(name);
    let mut n = 2;
    loop {
        match std::fs::read(&candidate) {
            // Same name, same bytes — already have it.
            Ok(existing) if existing == bytes => break,
            Ok(_) => {
                candidate = dir.join(match ext {
                    Some(e) => format!("{stem}-{n}.{e}"),
                    None => format!("{stem}-{n}"),
                });
                n += 1;
            }
            Err(_) => {
                std::fs::write(&candidate, &bytes).map_err(|e| e.to_string())?;
                break;
            }
        }
    }

    Ok(format!(
        "{FIXTURE_DIR}/{}",
        candidate
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(name)
    ))
}

/// `invoice.xml` → `("invoice", Some("xml"))`, `README` → `("README", None)`.
fn split_name(name: &str) -> (&str, Option<&str>) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, Some(ext)),
        _ => (name, None),
    }
}

/// Make a path project-relative when it lives under the project, for the
/// destination of a recorded download. Anything outside stays absolute — the
/// user chose that location deliberately.
pub fn relative_to_project(project_root: &Path, path: &str) -> String {
    Path::new(path)
        .strip_prefix(project_root)
        .ok()
        .and_then(|p| p.to_str())
        .map(|p| p.to_string())
        .unwrap_or_else(|| path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_gaps_are_not_worth_a_sleep() {
        assert!(gap_sleep(0).is_none());
        assert!(gap_sleep(1_499).is_none());
    }

    #[test]
    fn long_gaps_become_a_sleep_of_the_observed_length() {
        assert!(matches!(gap_sleep(1_500), Some(Step::Sleep { ms: 1_500 })));
        assert!(matches!(gap_sleep(4_200), Some(Step::Sleep { ms: 4_200 })));
    }

    #[test]
    fn a_coffee_break_is_capped_rather_than_recorded_verbatim() {
        assert!(matches!(
            gap_sleep(20 * 60 * 1_000),
            Some(Step::Sleep { ms: MAX_GAP_MS })
        ));
    }

    #[test]
    fn a_triggered_workflow_gains_a_wait_for_its_run() {
        let mut state = RecorderState::default();
        state.start("s".into(), PathBuf::from("/tmp"));
        state.push(Step::RunWorkflow {
            workflow: "Invoice".into(),
            trigger: "manual".into(),
            body: String::new(),
            capture: None,
            expect_trigger_error: false,
        });

        assert_eq!(state.steps.len(), 2);
        assert!(
            matches!(&state.steps[1], Step::WaitForRun { workflow, .. } if workflow == "Invoice")
        );
        // The user's wait for that run is now represented by the WaitForRun, so
        // the next action must not also produce a Sleep.
        assert!(state.suppress_next_gap);
    }

    #[test]
    fn the_first_step_never_gets_a_leading_sleep() {
        let mut state = RecorderState::default();
        state.start("s".into(), PathBuf::from("/tmp"));
        state.push(Step::DrainQueue { queue: "q".into() });
        assert_eq!(state.steps.len(), 1);
    }

    #[test]
    fn review_is_the_state_between_stop_and_save() {
        let mut state = RecorderState::default();
        assert!(!state.is_recording() && !state.in_review());

        state.start("s".into(), PathBuf::from("/tmp"));
        state.push(Step::DrainQueue { queue: "q".into() });
        assert!(state.is_recording() && !state.in_review());

        state.stop();
        assert!(!state.is_recording() && state.in_review());

        state.reset();
        assert!(!state.is_recording() && !state.in_review());
    }

    #[test]
    fn split_name_handles_dotfiles_and_extensionless_names() {
        assert_eq!(split_name("invoice.xml"), ("invoice", Some("xml")));
        assert_eq!(split_name("a.b.csv"), ("a.b", Some("csv")));
        assert_eq!(split_name("README"), ("README", None));
        assert_eq!(split_name(".keep"), (".keep", None));
    }

    #[test]
    fn a_path_outside_the_project_is_left_absolute() {
        let root = Path::new("/repo/app");
        assert_eq!(
            relative_to_project(root, "/repo/app/out/f.xml"),
            "out/f.xml"
        );
        assert_eq!(relative_to_project(root, "/tmp/f.xml"), "/tmp/f.xml");
    }

    #[test]
    fn an_identical_fixture_is_reused_and_a_different_one_is_suffixed() {
        let root = std::env::temp_dir().join(format!("ais-rec-{}", std::process::id()));
        let src_dir = root.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();

        let a = src_dir.join("invoice.xml");
        std::fs::write(&a, b"<one/>").unwrap();
        let first = copy_fixture(&root, a.to_str().unwrap()).unwrap();
        assert_eq!(first, format!("{FIXTURE_DIR}/invoice.xml"));

        // Same bytes again — no duplicate.
        assert_eq!(copy_fixture(&root, a.to_str().unwrap()).unwrap(), first);

        // Same name, different bytes — suffixed rather than clobbered.
        std::fs::write(&a, b"<two/>").unwrap();
        assert_eq!(
            copy_fixture(&root, a.to_str().unwrap()).unwrap(),
            format!("{FIXTURE_DIR}/invoice-2.xml")
        );

        std::fs::remove_dir_all(&root).ok();
    }
}
