//! JUnit XML output for scenario runs.
//!
//! Azure DevOps, GitHub Actions, and Jenkins all render JUnit XML natively, so
//! emitting it is what turns a CI scenario run from "red X, go read the log"
//! into a per-step failure list in the pipeline UI.
//!
//! The mapping is one `<testsuite>` per scenario and one `<testcase>` per step.
//! Steps rather than scenarios are the unit because a scenario is a sequence:
//! knowing *which* step failed is the whole diagnostic value, and a scenario-
//! level pass/fail throws that away.

use crate::services::scenario::{StepResult, StepStatus};

/// One scenario's results.
pub struct SuiteReport {
    pub scenario: String,
    pub steps: Vec<StepResult>,
}

impl SuiteReport {
    pub fn failures(&self) -> usize {
        self.steps
            .iter()
            .filter(|s| s.status == StepStatus::Failed)
            .count()
    }
    pub fn skipped(&self) -> usize {
        self.steps
            .iter()
            .filter(|s| s.status == StepStatus::Skipped)
            .count()
    }
    pub fn elapsed_ms(&self) -> u128 {
        self.steps.iter().map(|s| s.elapsed_ms).sum()
    }
    pub fn passed(&self) -> bool {
        self.failures() == 0
    }
}

/// Render a complete JUnit XML document.
pub fn render(suites: &[SuiteReport]) -> String {
    let total: usize = suites.iter().map(|s| s.steps.len()).sum();
    let failures: usize = suites.iter().map(|s| s.failures()).sum();
    let skipped: usize = suites.iter().map(|s| s.skipped()).sum();
    let time: u128 = suites.iter().map(|s| s.elapsed_ms()).sum();

    let mut out = String::from(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    out.push('\n');
    out.push_str(&format!(
        r#"<testsuites name="ais-runner scenarios" tests="{total}" failures="{failures}" skipped="{skipped}" time="{}">"#,
        secs(time)
    ));
    out.push('\n');

    for suite in suites {
        out.push_str(&format!(
            r#"  <testsuite name="{}" tests="{}" failures="{}" skipped="{}" time="{}">"#,
            escape(&suite.scenario),
            suite.steps.len(),
            suite.failures(),
            suite.skipped(),
            secs(suite.elapsed_ms()),
        ));
        out.push('\n');

        for step in &suite.steps {
            // The step number is part of the name so ordering survives in UIs
            // that sort test cases alphabetically.
            let name = format!("{:02}. {}", step.index + 1, step.label);
            out.push_str(&format!(
                r#"    <testcase name="{}" classname="{}" time="{}""#,
                escape(&name),
                escape(&suite.scenario),
                secs(step.elapsed_ms),
            ));

            match step.status {
                StepStatus::Ok => out.push_str("/>\n"),
                StepStatus::Skipped => {
                    out.push_str(">\n");
                    out.push_str(&format!(
                        "      <skipped message=\"{}\"/>\n",
                        escape(&step.detail)
                    ));
                    out.push_str("    </testcase>\n");
                }
                StepStatus::Failed => {
                    out.push_str(">\n");
                    // `message` is the one-line summary CI shows in the list;
                    // the element body carries the same text for the detail view.
                    out.push_str(&format!(
                        "      <failure message=\"{}\" type=\"StepFailed\">{}</failure>\n",
                        escape(&truncate(&step.detail, 300)),
                        escape(&step.detail),
                    ));
                    out.push_str("    </testcase>\n");
                }
            }
        }

        out.push_str("  </testsuite>\n");
    }

    out.push_str("</testsuites>\n");
    out
}

fn secs(ms: u128) -> String {
    format!("{:.3}", ms as f64 / 1000.0)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

/// Escape text for use in XML attributes and character data.
///
/// Also drops control characters that are illegal in XML 1.0 regardless of
/// escaping. This matters in practice: AMQP and SQL driver errors arrive with
/// embedded NULs and other control bytes, and a single one makes the whole
/// report unparseable — so the CI run reports "no tests" instead of the failure
/// that actually happened.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            // Legal XML 1.0: #x9 | #xA | #xD | [#x20-#xD7FF] | ...
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(index: usize, label: &str, status: StepStatus, detail: &str) -> StepResult {
        StepResult {
            index,
            label: label.to_string(),
            status,
            detail: detail.to_string(),
            elapsed_ms: 500,
        }
    }

    #[test]
    fn escapes_every_xml_metacharacter() {
        assert_eq!(
            escape(r#"a & b < c > d " e ' f"#),
            "a &amp; b &lt; c &gt; d &quot; e &apos; f"
        );
    }

    #[test]
    fn strips_control_characters_that_would_break_the_document() {
        // A raw NUL or bell in a driver error must not reach the XML.
        let escaped = escape("before\u{0}after\u{7}end");
        assert_eq!(escaped, "before after end");
        assert!(!escaped.contains('\u{0}'));
    }

    #[test]
    fn keeps_whitespace_that_is_legal_in_xml() {
        assert_eq!(escape("a\tb\nc\rd"), "a\tb\nc\rd");
    }

    #[test]
    fn renders_counts_and_marks_each_status() {
        let suites = vec![SuiteReport {
            scenario: "Ignite invoice".into(),
            steps: vec![
                step(0, "drain q", StepStatus::Ok, "drained 0"),
                step(1, "send to q", StepStatus::Failed, "entity not found"),
                step(
                    2,
                    "wait for run",
                    StepStatus::Skipped,
                    "skipped after an earlier failure",
                ),
            ],
        }];

        let xml = render(&suites);
        assert!(xml.contains(r#"tests="3""#));
        assert!(xml.contains(r#"failures="1""#));
        assert!(xml.contains(r#"skipped="1""#));
        assert!(xml.contains(r#"name="01. drain q""#));
        assert!(xml.contains(r#"<failure message="entity not found""#));
        assert!(xml.contains("<skipped"));
        // A passing step is a self-closing testcase with no child element.
        assert!(xml.contains(r#"name="01. drain q" classname="Ignite invoice" time="0.500"/>"#));
    }

    #[test]
    fn failure_message_attribute_is_truncated_but_body_is_not() {
        let long = "x".repeat(500);
        let suites = vec![SuiteReport {
            scenario: "s".into(),
            steps: vec![step(0, "l", StepStatus::Failed, &long)],
        }];
        let xml = render(&suites);
        assert!(xml.contains(&format!("{}…", "x".repeat(300))));
        assert!(xml.contains(&long));
    }

    #[test]
    fn suite_passed_reflects_failures_only_not_skips() {
        let s = SuiteReport {
            scenario: "s".into(),
            steps: vec![step(0, "l", StepStatus::Skipped, "d")],
        };
        assert!(s.passed());
        assert_eq!(s.skipped(), 1);
    }
}
