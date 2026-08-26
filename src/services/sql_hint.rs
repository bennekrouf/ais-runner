use serde_json::Value;

/// A missing-object hint derived from a failed SQL action's outputs blob.
#[derive(Debug, Clone, PartialEq)]
pub struct SqlMissingObject {
    pub kind: SqlObjectKind,
    pub name: String, // qualified name as it appeared in the error, e.g. "dbo.WfFanInOut_Begin_sp"
    pub raw_message: String,
    pub stub_ddl: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SqlObjectKind {
    StoredProcedure,
    Table,
}

impl SqlObjectKind {
    pub fn label(&self) -> &'static str {
        match self {
            SqlObjectKind::StoredProcedure => "stored procedure",
            SqlObjectKind::Table => "table",
        }
    }
}

/// Scan an action-detail JSON (as produced by `get_action_detail`) and return a
/// missing-object hint if the outputs/error indicates one. Returns None if the
/// action is fine or if the error is unrelated.
pub fn detect(detail: &Value) -> Option<SqlMissingObject> {
    let candidates: Vec<String> = [
        "/properties/outputs",
        "/properties/error",
        "/properties/outputs/body",
    ]
    .iter()
    .filter_map(|p| detail.pointer(p))
    .map(|v| v.to_string())
    .collect();

    let haystack = candidates.join("\n");
    if haystack.is_empty() {
        return None;
    }

    // Pattern 1: "Could not find stored procedure 'foo'" (SQL error 2812)
    if let Some(idx) = haystack.find("Could not find stored procedure") {
        let tail = &haystack[idx..];
        if let Some(name) = extract_quoted(tail) {
            let stub = stub_sproc(&name, detail);
            return Some(SqlMissingObject {
                kind: SqlObjectKind::StoredProcedure,
                stub_ddl: stub,
                raw_message: first_line(tail).to_string(),
                name,
            });
        }
    }

    // Pattern 2: "Invalid object name 'schema.table'" (SQL error 208)
    if let Some(idx) = haystack.find("Invalid object name") {
        let tail = &haystack[idx..];
        if let Some(name) = extract_quoted(tail) {
            let kind = if looks_like_sproc(&name) {
                SqlObjectKind::StoredProcedure
            } else {
                SqlObjectKind::Table
            };
            let stub = match kind {
                SqlObjectKind::StoredProcedure => stub_sproc(&name, detail),
                SqlObjectKind::Table => stub_table(&name),
            };
            return Some(SqlMissingObject {
                kind,
                stub_ddl: stub,
                raw_message: first_line(tail).to_string(),
                name,
            });
        }
    }

    None
}

fn looks_like_sproc(name: &str) -> bool {
    let lc = name.to_lowercase();
    lc.ends_with("_sp") || lc.ends_with("_proc") || lc.contains("storedprocedure")
}

fn first_line(s: &str) -> &str {
    s.split('\n')
        .next()
        .unwrap_or(s)
        .trim_end_matches(['"', '\\', '.', ' '])
}

/// Pull the first single- or double-quoted substring out of `s`.
fn extract_quoted(s: &str) -> Option<String> {
    for delim in ['\'', '"'] {
        if let Some(start) = s.find(delim) {
            if let Some(rel_end) = s[start + 1..].find(delim) {
                let raw = &s[start + 1..start + 1 + rel_end];
                let cleaned = raw.replace(['[', ']'], "");
                if !cleaned.is_empty() {
                    return Some(cleaned);
                }
            }
        }
    }
    None
}

fn split_schema(name: &str) -> (String, String) {
    let cleaned = name.replace(['[', ']'], "");
    match cleaned.split_once('.') {
        Some((s, n)) => (s.to_string(), n.to_string()),
        None => ("dbo".to_string(), cleaned),
    }
}

/// Generate a CREATE PROCEDURE stub. If the action's inputs include
/// storedProcedureParameters, emit them as `@Foo NVARCHAR(MAX) = NULL` etc.
fn stub_sproc(name: &str, detail: &Value) -> String {
    let params: Vec<String> = detail
        .pointer("/properties/inputs/parameters/storedProcedureParameters")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.keys()
                .map(|k| k.trim_start_matches('@').to_string())
                .collect()
        })
        .unwrap_or_default();
    let param_refs: Vec<&str> = params.iter().map(|s| s.as_str()).collect();
    stub_sproc_with_params(name, &param_refs)
}

/// Public stub builder used by the static-analysis chip popover. Accepts the
/// parameter names parsed from `workflow.json` so the `CREATE PROCEDURE`
/// signature matches what the workflow will actually call with.
pub fn stub_sproc_with_params(name: &str, params: &[&str]) -> String {
    let (schema, sp_name) = split_schema(name);

    let param_block = if params.is_empty() {
        String::new()
    } else {
        let lines: Vec<String> = params
            .iter()
            .map(|p| format!("    @{} NVARCHAR(MAX) = NULL", p.trim_start_matches('@')))
            .collect();
        format!("\n{}\n", lines.join(",\n"))
    };

    format!(
        "-- Auto-generated stub for missing stored procedure.\n\
         -- Replace the body with your real logic before running for real.\n\
         IF OBJECT_ID(N'[{schema}].[{sp_name}]', N'P') IS NOT NULL\n    \
            DROP PROCEDURE [{schema}].[{sp_name}];\n\
         GO\n\
         CREATE PROCEDURE [{schema}].[{sp_name}]{param_block}\n\
         AS\n\
         BEGIN\n    \
             SET NOCOUNT ON;\n    \
             -- TODO: implement\n    \
             SELECT 1 AS Stub;\n\
         END\n\
         GO\n"
    )
}

fn stub_table(name: &str) -> String {
    let (schema, t_name) = split_schema(name);
    format!(
        "-- Auto-generated stub for missing table — columns are placeholders.\n\
         IF OBJECT_ID(N'[{schema}].[{t_name}]', N'U') IS NULL\n\
         CREATE TABLE [{schema}].[{t_name}] (\n    \
             Id        INT IDENTITY(1,1) PRIMARY KEY,\n    \
             Payload   NVARCHAR(MAX) NULL,\n    \
             CreatedAt DATETIME2     NOT NULL DEFAULT SYSUTCDATETIME()\n\
         );\n\
         GO\n"
    )
}
