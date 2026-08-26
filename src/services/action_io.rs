//! Helpers for shaping an action's input/output payload before the user
//! downloads it from the Run tab.
//!
//! Purely schema-driven — the format we emit comes from the Logic Apps
//! action's own `type` and `inputs.format` fields. No customer-specific
//! naming, no workflow-name heuristics: every workflow that uses a `Table`
//! action with `format: "CSV"` (the canonical pattern for error dumps,
//! batch reports, etc.) automatically downloads as `.csv`; everything else
//! falls back to JSON or plain text based on the body's actual shape.

use serde_json::Value;

/// Inferred wire format for a downloaded action body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadFormat {
    /// Comma-separated values — the `Table` action with `format: "CSV"`,
    /// or a string body that's structurally CSV (commas + newlines).
    Csv,
    /// HTML table — the `Table` action with `format: "HTML"`.
    Html,
    /// Any JSON object or array.
    Json,
    /// String body that isn't recognised as CSV or HTML.
    Text,
}

impl DownloadFormat {
    /// Filename extension for native save-dialog defaults.
    pub fn extension(&self) -> &'static str {
        match self {
            DownloadFormat::Csv => "csv",
            DownloadFormat::Html => "html",
            DownloadFormat::Json => "json",
            DownloadFormat::Text => "txt",
        }
    }

    /// Human label for the save dialog filter ("CSV files", "JSON files"…).
    pub fn label(&self) -> &'static str {
        match self {
            DownloadFormat::Csv => "CSV files",
            DownloadFormat::Html => "HTML files",
            DownloadFormat::Json => "JSON files",
            DownloadFormat::Text => "Text files",
        }
    }
}

/// Result of preparing an action's body for download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedDownload {
    /// Bytes to write to disk. UTF-8 for every format we support today.
    pub bytes: Vec<u8>,
    pub format: DownloadFormat,
    /// Suggested filename (sans path) including extension. Includes the
    /// action name so a user dumping outputs from multiple actions in the
    /// same run gets distinct filenames by default.
    pub suggested_filename: String,
}

/// Build a `PreparedDownload` from a fetched action-detail JSON. Returns
/// `None` when there's nothing meaningful to download (no outputs, or the
/// outputs blob never arrived).
///
/// `action_detail` is the JSON returned by `workflows::get_action_detail` —
/// the structure is `{ properties: { inputs, outputs, … } }`. We look at:
///
/// * `properties.outputs.body` — the body to serialise
/// * The originating workflow's action `type` + `inputs.format` — drives
///   the CSV vs. HTML vs. JSON branching for the `Table` action
///
/// The workflow's source isn't always reachable from the runtime detail
/// alone, so the action-type hint is taken from a caller-supplied
/// `(action_type, requested_format)` pair extracted from the workflow.json
/// at trigger time (see `Run` tab call site).
pub fn prepare_download(
    action_name: &str,
    action_type: Option<&str>,
    requested_format: Option<&str>,
    action_detail: &Value,
) -> Option<PreparedDownload> {
    let outputs = action_detail.pointer("/properties/outputs")?;
    let body = outputs.get("body").unwrap_or(outputs).clone();

    let format = infer_format(action_type, requested_format, &body);
    let bytes = serialise(&body, &format);
    let suggested_filename = format!("{}.{}", sanitise_filename(action_name), format.extension());

    Some(PreparedDownload {
        bytes,
        format,
        suggested_filename,
    })
}

/// Choose the right file format for the body. The decision tree:
///
/// 1. Action type `Table` + `inputs.format` = `"CSV"` / `"HTML"` — schema-
///    driven; this is what Logic Apps's `Table` action emits and it tells
///    us the body is already a formatted string ready to write verbatim.
/// 2. Body is a JSON string that looks CSV-shaped (≥1 comma per line on the
///    first 5 lines, no `{` start, no XML opener) — treat as CSV. Catches
///    Functions / custom code that compose CSV manually.
/// 3. Body is a JSON object or array → JSON.
/// 4. Anything else (plain string, number, bool, null) → Text.
fn infer_format(
    action_type: Option<&str>,
    requested_format: Option<&str>,
    body: &Value,
) -> DownloadFormat {
    if action_type == Some("Table") {
        return match requested_format.map(str::to_ascii_lowercase).as_deref() {
            Some("html") => DownloadFormat::Html,
            _ => DownloadFormat::Csv,
        };
    }
    if let Value::String(s) = body {
        if looks_like_csv(s) {
            return DownloadFormat::Csv;
        }
        if looks_like_html(s) {
            return DownloadFormat::Html;
        }
        return DownloadFormat::Text;
    }
    if matches!(body, Value::Object(_) | Value::Array(_)) {
        return DownloadFormat::Json;
    }
    DownloadFormat::Text
}

fn serialise(body: &Value, format: &DownloadFormat) -> Vec<u8> {
    match (body, format) {
        (Value::String(s), DownloadFormat::Csv | DownloadFormat::Html | DownloadFormat::Text) => {
            s.clone().into_bytes()
        }
        (v, DownloadFormat::Json) => serde_json::to_string_pretty(v)
            .unwrap_or_else(|_| v.to_string())
            .into_bytes(),
        (v, _) =>
        // Defensive: a non-string body labelled CSV/HTML/Text — fall back
        // to the JSON serialisation rather than producing garbage.
        {
            serde_json::to_string_pretty(v)
                .unwrap_or_else(|_| v.to_string())
                .into_bytes()
        }
    }
}

fn looks_like_csv(s: &str) -> bool {
    if s.starts_with('{') || s.starts_with('[') || s.starts_with('<') {
        return false;
    }
    let first_lines: Vec<&str> = s.lines().take(5).collect();
    if first_lines.is_empty() {
        return false;
    }
    first_lines.iter().all(|l| l.contains(','))
}

fn looks_like_html(s: &str) -> bool {
    let t = s.trim_start();
    t.starts_with("<!DOCTYPE")
        || t.starts_with("<html")
        || t.starts_with("<table")
        || t.starts_with("<HTML")
}

/// Strip filesystem-hostile characters from an action name so we can use it
/// as a default filename without surprising the OS save dialog.
fn sanitise_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn detail_with_body(b: Value) -> Value {
        json!({ "properties": { "outputs": { "body": b } } })
    }

    #[test]
    fn table_action_with_format_csv_is_csv() {
        let d = detail_with_body(json!("a,b,c\n1,2,3\n4,5,6"));
        let p = prepare_download("Create_CSV_table", Some("Table"), Some("CSV"), &d).unwrap();
        assert_eq!(p.format, DownloadFormat::Csv);
        assert_eq!(p.suggested_filename, "Create_CSV_table.csv");
        assert_eq!(p.bytes, b"a,b,c\n1,2,3\n4,5,6");
    }

    #[test]
    fn table_action_with_format_html_is_html() {
        let d = detail_with_body(json!("<table><tr><td>1</td></tr></table>"));
        let p = prepare_download("ReportTable", Some("Table"), Some("HTML"), &d).unwrap();
        assert_eq!(p.format, DownloadFormat::Html);
        assert!(p.suggested_filename.ends_with(".html"));
    }

    #[test]
    fn parse_json_action_with_object_body_is_json() {
        let d = detail_with_body(json!({"id": 1, "name": "abc"}));
        let p = prepare_download("Parse_JSON", Some("ParseJson"), None, &d).unwrap();
        assert_eq!(p.format, DownloadFormat::Json);
        // Pretty-printed JSON — line breaks present.
        let s = String::from_utf8(p.bytes).unwrap();
        assert!(s.contains('\n'));
        assert!(s.contains("\"id\""));
    }

    #[test]
    fn string_body_with_csv_shape_is_detected_even_without_table_action() {
        // Functions that emit CSV via custom code don't have a `Table` action
        // — they just return a string. The shape sniff should catch this.
        let d = detail_with_body(json!("line,one\nline,two\nline,three"));
        let p = prepare_download("BuildCsv", Some("JavaScriptCode"), None, &d).unwrap();
        assert_eq!(p.format, DownloadFormat::Csv);
    }

    #[test]
    fn string_body_without_commas_is_text() {
        let d = detail_with_body(json!("just a plain log line\nno commas anywhere"));
        let p = prepare_download("Compose", Some("Compose"), None, &d).unwrap();
        assert_eq!(p.format, DownloadFormat::Text);
    }

    #[test]
    fn html_string_body_is_detected() {
        let d = detail_with_body(json!("<!DOCTYPE html><html><body>hi</body></html>"));
        let p = prepare_download("Make_report", Some("Compose"), None, &d).unwrap();
        assert_eq!(p.format, DownloadFormat::Html);
    }

    #[test]
    fn outputs_without_body_falls_back_to_the_whole_outputs() {
        // Some actions write directly to `properties.outputs` without an
        // inner `body` wrapper — we still want to surface them.
        let d = json!({ "properties": { "outputs": { "value": 42 } } });
        let p = prepare_download("Var", Some("InitializeVariable"), None, &d).unwrap();
        assert_eq!(p.format, DownloadFormat::Json);
    }

    #[test]
    fn no_outputs_yields_none() {
        let d = json!({ "properties": {} });
        assert!(prepare_download("Foo", Some("Compose"), None, &d).is_none());
    }

    #[test]
    fn unsafe_characters_in_action_name_are_sanitised() {
        let d = detail_with_body(json!("a,b"));
        let p =
            prepare_download("Create CSV / dump:file?", Some("Table"), Some("CSV"), &d).unwrap();
        // Spaces, slash, colon, question-mark all become `_`.
        assert_eq!(p.suggested_filename, "Create_CSV___dump_file_.csv");
    }
}
