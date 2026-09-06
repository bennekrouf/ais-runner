//! Filename-driven Service Bus message templates.
//!
//! Complements `payload.rs`, which derives a *sample* body from a workflow's
//! own JSON schema. That answers "what shape does this trigger expect?".
//! This module answers a different question: "I just dropped file X in a
//! container — what message announces it?"
//!
//! The distinction matters because event-driven flows are usually triggered by
//! a *queue message that references a file*, not by the file itself. The
//! message body is fully derivable from the filename, but the mapping between
//! the two is project-specific (naming conventions, event envelopes, which
//! queue). So the engine here is generic and the mapping lives as data in the
//! project being developed:
//!
//! ```text
//! <project>/.ais-runner/message-templates/*.json
//! ```
//!
//! Keeping templates in the project (not in ais-runner) means a team's file
//! conventions are versioned alongside the workflows that consume them, and
//! ais-runner stays free of any one platform's naming rules.

use regex::Regex;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Where templates live, relative to the selected project root.
const TEMPLATE_DIR: &str = ".ais-runner/message-templates";

#[derive(Clone, Debug)]
pub struct MessageTemplate {
    /// Display name shown in the UI.
    pub name: String,
    /// Destination queue/topic the rendered message is sent to.
    pub queue: String,
    /// Regex a filename must match for this template to be offered.
    /// Named captures become substitution variables.
    pub pattern: Regex,
    /// Message body, with `{{var}}` placeholders inside string values.
    pub body: Value,
}

/// Values available to every template, independent of the filename.
#[derive(Clone, Debug, Default)]
pub struct RenderContext {
    pub env: String,
    pub blob_endpoint: String,
}

/// Callers hand us whichever path they happen to hold — sometimes the platform
/// root, sometimes its `logic_apps/` subfolder. Templates live at the root, so
/// try the given path first and then walk up one level.
fn resolve_template_dir(start: &Path) -> Option<PathBuf> {
    let direct = start.join(TEMPLATE_DIR);
    if direct.is_dir() {
        return Some(direct);
    }
    let parent = start.parent()?.join(TEMPLATE_DIR);
    parent.is_dir().then_some(parent)
}

/// Read every template under `<project_root>/.ais-runner/message-templates`.
///
/// Malformed files are skipped rather than failing the whole discovery — one
/// bad template shouldn't hide the rest. Their errors come back alongside so
/// the caller can surface them without blocking.
pub fn discover(project_root: &Path) -> (Vec<MessageTemplate>, Vec<String>) {
    let mut templates = Vec::new();
    let mut errors = Vec::new();

    // No template dir is the normal case for projects that don't use this.
    let Some(dir) = resolve_template_dir(project_root) else {
        return (templates, errors);
    };

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return (templates, errors),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match parse_template(&path) {
            Ok(t) => templates.push(t),
            Err(e) => errors.push(format!("{}: {e}", path.display())),
        }
    }

    templates.sort_by(|a, b| a.name.cmp(&b.name));
    (templates, errors)
}

fn parse_template(path: &Path) -> Result<MessageTemplate, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&raw).map_err(|e| format!("invalid JSON: {e}"))?;

    let name = v["name"].as_str().ok_or("missing \"name\"")?.to_string();
    let queue = v["queue"].as_str().ok_or("missing \"queue\"")?.to_string();
    let pattern_src = v["match"].as_str().ok_or("missing \"match\"")?;
    let pattern = Regex::new(pattern_src).map_err(|e| format!("invalid \"match\" regex: {e}"))?;
    let body = v.get("body").ok_or("missing \"body\"")?.clone();

    Ok(MessageTemplate {
        name,
        queue,
        pattern,
        body,
    })
}

impl MessageTemplate {
    pub fn matches(&self, filename: &str) -> bool {
        self.pattern.is_match(filename)
    }

    /// Render the body for `filename`, substituting `{{var}}` placeholders.
    ///
    /// Substitution walks the JSON tree and only rewrites *string values*, so
    /// the result is always structurally valid JSON regardless of what a
    /// variable expands to — unlike templating the raw document text, where an
    /// unescaped value could break parsing.
    pub fn render(&self, filename: &str, ctx: &RenderContext) -> Result<String, String> {
        let caps = self
            .pattern
            .captures(filename)
            .ok_or_else(|| format!("'{filename}' does not match template '{}'", self.name))?;

        let mut vars: Vec<(String, String)> = Vec::new();
        for name in self.pattern.capture_names().flatten() {
            if let Some(m) = caps.name(name) {
                vars.push((name.to_string(), m.as_str().to_string()));
            }
        }
        vars.push(("filename".into(), filename.to_string()));
        vars.push(("env".into(), ctx.env.clone()));
        vars.push(("blobEndpoint".into(), ctx.blob_endpoint.clone()));
        vars.push(("now".into(), chrono::Utc::now().to_rfc3339()));
        vars.push(("guid".into(), pseudo_guid()));

        let rendered = substitute_value(&self.body, &vars)?;
        serde_json::to_string_pretty(&rendered).map_err(|e| e.to_string())
    }
}

fn substitute_value(v: &Value, vars: &[(String, String)]) -> Result<Value, String> {
    Ok(match v {
        Value::String(s) => Value::String(substitute_str(s, vars)?),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|i| substitute_value(i, vars))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, val) in map {
                // Keys can carry placeholders too (e.g. dynamic property names).
                out.insert(substitute_str(k, vars)?, substitute_value(val, vars)?);
            }
            Value::Object(out)
        }
        other => other.clone(),
    })
}

/// Expand `{{name}}` and `{{name|filter}}` occurrences in one string.
fn substitute_str(s: &str, vars: &[(String, String)]) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;

    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .ok_or_else(|| format!("unclosed '{{{{' in \"{s}\""))?;
        let expr = after[..end].trim();

        let (name, filter) = match expr.split_once('|') {
            Some((n, f)) => (n.trim(), Some(f.trim())),
            None => (expr, None),
        };

        let raw = vars
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .ok_or_else(|| format!("unknown variable '{{{{{name}}}}}'"))?;

        out.push_str(&apply_filter(&raw, filter)?);
        rest = &after[end + 2..];
    }

    out.push_str(rest);
    Ok(out)
}

fn apply_filter(value: &str, filter: Option<&str>) -> Result<String, String> {
    match filter {
        None => Ok(value.to_string()),
        Some("upper") => Ok(value.to_uppercase()),
        Some("lower") => Ok(value.to_lowercase()),
        // Compact stamps like 20260804101545 are common in file naming but are
        // not valid ISO-8601, which is what event envelopes usually expect.
        Some("iso8601") => to_iso8601(value),
        Some(other) => Err(format!("unknown filter '{other}'")),
    }
}

/// `yyyyMMddHHmmss` (14 digits) or `yyyyMMdd` (8) -> ISO-8601 UTC.
fn to_iso8601(v: &str) -> Result<String, String> {
    let digits: String = v.chars().filter(|c| c.is_ascii_digit()).collect();
    let (y, mo, d, h, mi, s) = match digits.len() {
        14 => (
            &digits[0..4],
            &digits[4..6],
            &digits[6..8],
            &digits[8..10],
            &digits[10..12],
            &digits[12..14],
        ),
        8 => (
            &digits[0..4],
            &digits[4..6],
            &digits[6..8],
            "00",
            "00",
            "00",
        ),
        _ => {
            return Err(format!(
                "iso8601 expects 8 or 14 digits, got '{v}' ({} digits)",
                digits.len()
            ))
        }
    };
    Ok(format!("{y}-{mo}-{d}T{h}:{mi}:{s}.000Z"))
}

/// A v4-shaped identifier for correlation fields in test payloads.
///
/// Deliberately not a crypto-grade UUID — these values only ever label local
/// test messages, and adding a `uuid`/`rand` dependency for that isn't worth
/// it. Seeded from the clock, so successive calls differ.
fn pseudo_guid() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    // xorshift64* — cheap, adequate spread for distinct labels.
    let mut x = nanos | 1;
    let mut next = || {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        x = x.wrapping_mul(0x2545F4914F6CDD1D);
        x
    };

    let (a, b) = (next(), next());
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        a >> 32,
        (a >> 16) & 0xffff,
        a & 0x0fff,
        // Variant bits: 10xx -> 0x8000..=0xbfff
        0x8000 | ((b >> 48) & 0x3fff),
        b & 0xffff_ffff_ffff
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tpl(pattern: &str, body: Value) -> MessageTemplate {
        MessageTemplate {
            name: "t".into(),
            queue: "q".into(),
            pattern: Regex::new(pattern).unwrap(),
            body,
        }
    }

    fn ctx() -> RenderContext {
        RenderContext {
            env: "DEV".into(),
            blob_endpoint: "https://example.blob.core.windows.net/".into(),
        }
    }

    #[test]
    fn named_captures_become_variables() {
        let t = tpl(
            r"^F\.(?P<ts>\d{14})\.(?P<flow>\w+)\.xlsx$",
            json!({ "when": "{{ts}}", "flow": "{{flow}}" }),
        );
        let out = t
            .render("F.20260804101545.PY_TRANSFER.xlsx", &ctx())
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["when"], "20260804101545");
        assert_eq!(v["flow"], "PY_TRANSFER");
    }

    #[test]
    fn builtins_are_available() {
        let t = tpl(
            r"^(?P<n>.+)$",
            json!({ "f": "{{filename}}", "e": "{{env}}" }),
        );
        let out = t.render("anything.xlsx", &ctx()).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["f"], "anything.xlsx");
        assert_eq!(v["e"], "DEV");
    }

    #[test]
    fn iso8601_filter_expands_compact_stamp() {
        assert_eq!(
            to_iso8601("20260804101545").unwrap(),
            "2026-08-04T10:15:45.000Z"
        );
        assert_eq!(to_iso8601("20260804").unwrap(), "2026-08-04T00:00:00.000Z");
        assert!(to_iso8601("123").is_err());
    }

    #[test]
    fn filters_apply_in_placeholders() {
        let t = tpl(
            r"^(?P<ts>\d{14})\.(?P<f>\w+)$",
            json!({ "iso": "{{ts|iso8601}}", "lower": "{{f|lower}}" }),
        );
        let out = t.render("20260804101545.PY_TRANSFER", &ctx()).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["iso"], "2026-08-04T10:15:45.000Z");
        assert_eq!(v["lower"], "py_transfer");
    }

    #[test]
    fn nested_structures_are_substituted() {
        let t = tpl(
            r"^(?P<n>\w+)\.xlsx$",
            json!({ "a": [{ "k": "{{n}}" }], "b": { "c": "x-{{n}}" } }),
        );
        let out = t.render("file.xlsx", &ctx()).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["a"][0]["k"], "file");
        assert_eq!(v["b"]["c"], "x-file");
    }

    #[test]
    fn non_string_values_are_preserved() {
        let t = tpl(
            r"^(?P<n>\w+)$",
            json!({ "num": 42, "yes": true, "nil": null }),
        );
        let out = t.render("x", &ctx()).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["num"], 42);
        assert_eq!(v["yes"], true);
        assert!(v["nil"].is_null());
    }

    #[test]
    fn unknown_variable_is_an_error() {
        let t = tpl(r"^(?P<n>\w+)$", json!({ "x": "{{nope}}" }));
        let err = t.render("x", &ctx()).unwrap_err();
        assert!(err.contains("nope"), "got: {err}");
    }

    #[test]
    fn unknown_filter_is_an_error() {
        let t = tpl(r"^(?P<n>\w+)$", json!({ "x": "{{n|bogus}}" }));
        assert!(t.render("x", &ctx()).unwrap_err().contains("bogus"));
    }

    #[test]
    fn non_matching_filename_is_rejected() {
        let t = tpl(r"^\d+$", json!({}));
        assert!(!t.matches("abc"));
        assert!(t.render("abc", &ctx()).is_err());
    }

    #[test]
    fn pseudo_guid_is_v4_shaped_and_varies() {
        let a = pseudo_guid();
        assert_eq!(a.len(), 36);
        assert_eq!(a.chars().filter(|c| *c == '-').count(), 4);
        assert_eq!(&a[14..15], "4", "version nibble");
        assert_ne!(a, pseudo_guid());
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::services::sample_workspace;

    /// Guards a real message template against the filenames it must handle.
    /// Opt-in: point `AIS_SAMPLE_WORKSPACE` at a real Logic Apps project to
    /// run this against one. Skipped otherwise, so a clean checkout and CI
    /// both pass. It used to hardcode one developer's home directory, which
    /// meant it ran on exactly one machine and named the client's repo.
    #[test]
    fn a_real_template_renders_real_filenames() {
        let Some(root) = sample_workspace() else {
            eprintln!("skipped: set AIS_SAMPLE_WORKSPACE to run it");
            return;
        };
        if !root.join(TEMPLATE_DIR).is_dir() {
            eprintln!("skipped: no message templates in AIS_SAMPLE_WORKSPACE");
            return;
        }
        let (templates, errors) = discover(&root);
        assert!(errors.is_empty(), "template errors: {errors:?}");

        let t = templates
            .iter()
            .find(|t| t.name == "Acme Globex event")
            .expect("Acme Globex event template");
        assert_eq!(t.queue, "ais.apim.event");

        let ctx = RenderContext {
            env: "DEV".into(),
            blob_endpoint: String::new(),
        };

        for (file, flow) in [
            (
                "PARTNER.NC4.IMPORT.20260804101545.PY_TRANSFER.PY_MT101.NULL.NULL.xlsx",
                "PY_TRANSFER",
            ),
            (
                "PARTNER.NC4.IMPORT.20260731124240.CA_FLOW.CF_INTEGRATION.NULL.NULL.xlsx",
                "CA_FLOW",
            ),
        ] {
            assert!(t.matches(file), "should match {file}");
            let v: Value = serde_json::from_str(&t.render(file, &ctx).unwrap()).unwrap();
            assert_eq!(v["event"]["module"], "Globex");
            assert_eq!(v["event"]["source"], "ACME");
            assert_eq!(v["event"]["environment"], "DEV");
            assert_eq!(v["object"]["resourceURI"], file);
            assert_eq!(v["object"]["composite-keys"][0]["value"], file);
            assert!(v["object"]["composite-keys"][1]["value"]
                .as_str()
                .unwrap()
                .contains(flow));
        }

        // The 2026-07-28 incident: a space inside the filename. It must still
        // match, so the operator gets the button instead of hand-writing JSON.
        let spaced = "PARTNER.NC4.IMPORT.20260728141444.CA_FLOW. CF_INTEGRATION.NULL.NULL.xlsx";
        assert!(t.matches(spaced), "spaced filename should still match");

        // Non-Globex blobs in the same container must not offer this template.
        assert!(!t.matches("cashflow_20260726.xlsx"));
        assert!(!t.matches("sla/last-received.txt"));
    }

    #[test]
    fn iso8601_matches_the_payloads_used_in_testing() {
        assert_eq!(
            to_iso8601("20260804101545").unwrap(),
            "2026-08-04T10:15:45.000Z"
        );
        assert_eq!(
            to_iso8601("20260731124240").unwrap(),
            "2026-07-31T12:42:40.000Z"
        );
    }
}
