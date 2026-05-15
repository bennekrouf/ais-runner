use serde::Deserialize;
use crate::services::azure_cli::{az_command, AzError};

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Pipeline {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub folder: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PipelineRun {
    pub id: u64,
    /// Build number, e.g. "20250515.3"
    pub name: String,
    /// "inProgress" | "completed" | "canceling"
    pub state: String,
    /// "succeeded" | "failed" | "canceled" | "partiallySucceeded"
    #[serde(default)]
    pub result: Option<String>,
    #[serde(rename = "createdDate", default)]
    pub created_date: String,
    #[serde(rename = "finishedDate", default)]
    pub finished_date: Option<String>,
}

fn run_cmd(args: &[&str]) -> Result<String, AzError> {
    let out = az_command(args)
        .output()
        .map_err(|e| AzError::Other(format!("az not found: {}", e)))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        let combined = format!("{} {}", stdout, stderr);
        if combined.contains("AADSTS")
            || combined.contains("az login")
            || combined.contains("refresh token")
        {
            return Err(AzError::NotLoggedIn);
        }
        return Err(AzError::Other(stderr));
    }
    Ok(stdout)
}

pub fn list_pipelines(org: &str, project: &str) -> Result<Vec<Pipeline>, AzError> {
    let out = run_cmd(&[
        "pipelines", "list",
        "--org", org,
        "--project", project,
        "-o", "json",
    ])?;
    serde_json::from_str::<Vec<Pipeline>>(&out)
        .map_err(|e| AzError::Other(format!("parse pipelines: {}", e)))
}

pub fn list_runs(org: &str, project: &str, pipeline_id: u64) -> Result<Vec<PipelineRun>, AzError> {
    let id_str = pipeline_id.to_string();
    let out = run_cmd(&[
        "pipelines", "runs", "list",
        "--org", org,
        "--project", project,
        "--pipeline-ids", &id_str,
        "--top", "20",
        "-o", "json",
    ])?;
    serde_json::from_str::<Vec<PipelineRun>>(&out)
        .map_err(|e| AzError::Other(format!("parse runs: {}", e)))
}
