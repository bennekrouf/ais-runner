use serde::Deserialize;
use crate::services::azure_cli::{az_command, AzError};

// ── data types ────────────────────────────────────────────────────────────────

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
    #[serde(rename = "buildNumber", default)]
    pub name: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(rename = "queueTime", default)]
    pub created_date: String,
    #[serde(rename = "finishTime", default)]
    pub finished_date: Option<String>,
    #[serde(rename = "sourceBranch", default)]
    pub source_branch: String,
    #[serde(rename = "sourceVersion", default)]
    pub source_version: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReleaseDefinition {
    pub id:              u64,
    pub name:            String,
    pub last_release_on: Option<String>,
}

/// Full release detail from `az pipelines release show`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReleaseArtifact {
    pub release_id:     u64,
    pub release_name:   String,
    pub created_on:     String,
    pub build_number:   String,
    pub build_id:       String,   // internal numeric ID used for az pipelines release create
    pub artifact_alias: String,   // artifact alias in the definition, e.g. "_Build JDE Connector"
    pub branch:         String,
    pub commit:         String,
    pub environments:   Vec<ReleaseEnvStatus>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReleaseEnvStatus {
    pub name:   String,
    /// "notDeployed" | "inProgress" | "succeeded" | "partiallySucceeded" | "failed" | "canceled"
    pub status: String,
}


// ── internal helpers ──────────────────────────────────────────────────────────

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
        return Err(AzError::Other(if stderr.is_empty() { stdout } else { stderr }));
    }
    Ok(stdout)
}


// ── build pipeline APIs ───────────────────────────────────────────────────────

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

// ── classic release pipeline APIs (az pipelines release …) ───────────────────

pub fn list_release_definitions(org: &str, project: &str) -> Result<Vec<ReleaseDefinition>, AzError> {
    let out = run_cmd(&[
        "pipelines", "release", "definition", "list",
        "--org", org,
        "--project", project,
        "-o", "json",
    ])?;
    let v: serde_json::Value = serde_json::from_str(&out)
        .map_err(|e| AzError::Other(format!("parse release defs: {}", e)))?;
    let items = v.as_array()
        .or_else(|| v["value"].as_array())
        .ok_or_else(|| AzError::Other("unexpected release definitions shape".into()))?;
    let mut defs: Vec<ReleaseDefinition> = items.iter().filter_map(|e| {
        Some(ReleaseDefinition {
            id:              e["id"].as_u64()?,
            name:            e["name"].as_str()?.to_string(),
            last_release_on: e["lastRelease"]["createdOn"].as_str().map(|s| s.to_string()),
        })
    }).collect();
    // most recently released first
    defs.sort_by(|a, b| b.last_release_on.cmp(&a.last_release_on));
    Ok(defs)
}

#[derive(Debug, Clone)]
pub struct EnvInfo {
    pub name:               String,
    /// The release ID currently deployed to this environment (`currentRelease.id`).
    pub current_release_id: u64,
}

/// Returns the artifact alias of the first Build-type artifact in a release definition.
/// This is the value that must be passed as `artifact_alias` when calling `create_release`.
pub fn get_release_def_artifact_alias(org: &str, project: &str, def_id: u64) -> Result<String, AzError> {
    let id_str = def_id.to_string();
    let out = run_cmd(&[
        "pipelines", "release", "definition", "show",
        "--org", org, "--project", project,
        "--id", &id_str,
        "-o", "json",
    ])?;
    let v: serde_json::Value = serde_json::from_str(&out)
        .map_err(|e| AzError::Other(format!("parse definition: {}", e)))?;
    v["artifacts"].as_array()
        .and_then(|arts| arts.iter().find(|a| a["type"].as_str() == Some("Build")))
        .and_then(|a| a["alias"].as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| AzError::Other("No Build artifact found in release definition".into()))
}

/// Returns environments sorted by rank, each with its authoritative current release ID.
pub fn get_release_definition_envs(org: &str, project: &str, def_id: u64) -> Result<Vec<EnvInfo>, AzError> {
    let id_str = def_id.to_string();
    let out = run_cmd(&[
        "pipelines", "release", "definition", "show",
        "--org", org,
        "--project", project,
        "--id", &id_str,
        "-o", "json",
    ])?;
    let v: serde_json::Value = serde_json::from_str(&out)
        .map_err(|e| AzError::Other(format!("parse definition: {}", e)))?;

    let mut envs: Vec<(u64, EnvInfo)> = v["environments"].as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|e| {
            let rank               = e["rank"].as_u64().unwrap_or(999);
            let name               = e["name"].as_str()?.to_string();
            let current_release_id = e["currentRelease"]["id"].as_u64().unwrap_or(0);
            Some((rank, EnvInfo { name, current_release_id }))
        })
        .collect();

    envs.sort_by_key(|(rank, _)| *rank);
    Ok(envs.into_iter().map(|(_, info)| info).collect())
}

/// Fetches artifact details for a single release (build number, branch, commit).
/// Uses `az pipelines release show` which returns full artifact data.
pub fn get_release_artifact(org: &str, project: &str, release_id: u64) -> Result<ReleaseArtifact, AzError> {
    let id_str = release_id.to_string();
    let out = run_cmd(&[
        "pipelines", "release", "show",
        "--org", org,
        "--project", project,
        "--id", &id_str,
        "-o", "json",
    ])?;
    let v: serde_json::Value = serde_json::from_str(&out)
        .map_err(|e| AzError::Other(format!("parse release show: {}", e)))?;

    let artifact = v["artifacts"].as_array()
        .and_then(|arts| arts.iter().find(|a| a["type"].as_str() == Some("Build")));

    let artifact_alias = artifact
        .and_then(|a| a["alias"].as_str())
        .unwrap_or("").to_string();

    let build_number = artifact
        .and_then(|a| a["definitionReference"]["version"]["name"].as_str())
        .unwrap_or("—").to_string();

    let build_id = artifact
        .and_then(|a| a["definitionReference"]["version"]["id"].as_str())
        .unwrap_or("").to_string();

    let raw_branch = artifact
        .and_then(|a| a["definitionReference"]["branch"]["name"].as_str())
        .unwrap_or("").to_string();
    let branch = raw_branch.trim_start_matches("refs/heads/").to_string();

    let commit = artifact
        .and_then(|a| {
            a["definitionReference"]["sourceVersion"]["id"].as_str()
                .or_else(|| a["definitionReference"]["sourceVersion"]["name"].as_str())
        })
        .map(|s| s.get(..8).unwrap_or(s).to_string())
        .unwrap_or_default();

    let release_name = v["name"].as_str().unwrap_or("").to_string();
    let created_on   = v["createdOn"].as_str().unwrap_or("").to_string();

    let environments = v["environments"].as_array()
        .map(|envs| envs.iter().filter_map(|e| {
            Some(ReleaseEnvStatus {
                name:   e["name"].as_str()?.to_string(),
                status: e["status"].as_str().unwrap_or("notDeployed").to_string(),
            })
        }).collect())
        .unwrap_or_default();

    Ok(ReleaseArtifact { release_id, release_name, created_on, build_number, build_id, artifact_alias, branch, commit, environments })
}

/// Returns all branch names for the repository linked to a pipeline.
/// Calls `az pipelines show` to get the repo ID, then `az repos ref list`
/// to enumerate every refs/heads/* ref.
pub fn list_pipeline_branches(org: &str, project: &str, pipeline_id: u64) -> Result<Vec<String>, AzError> {
    let id_str = pipeline_id.to_string();

    // 1. Get the pipeline definition to find the repository
    let pipe_json = run_cmd(&[
        "pipelines", "show",
        "--org", org, "--project", project,
        "--id", &id_str,
        "-o", "json",
    ])?;
    let pipe: serde_json::Value = serde_json::from_str(&pipe_json)
        .map_err(|e| AzError::Other(format!("parse pipeline show: {}", e)))?;

    // The repository can be identified by name or id
    let repo_name = pipe["repository"]["name"].as_str()
        .or_else(|| pipe["repository"]["id"].as_str())
        .ok_or_else(|| AzError::Other("pipeline has no repository field".into()))?
        .to_string();

    // 2. List all refs/heads/* in that repo
    let refs_json = run_cmd(&[
        "repos", "ref", "list",
        "--org", org, "--project", project,
        "--repository", &repo_name,
        "--filter", "heads",
        "-o", "json",
    ])?;
    let refs: serde_json::Value = serde_json::from_str(&refs_json)
        .map_err(|e| AzError::Other(format!("parse refs: {}", e)))?;

    let mut branches: Vec<String> = refs.as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|r| {
            r["name"].as_str().map(|n| {
                n.trim_start_matches("refs/heads/").to_string()
            })
        })
        .filter(|b| !b.is_empty())
        .collect();

    branches.sort();
    Ok(branches)
}

/// Queues a new build run on the given pipeline and branch.
/// Returns the new run's build number on success.
pub fn trigger_build(org: &str, project: &str, pipeline_id: u64, branch: &str) -> Result<String, AzError> {
    let id_str = pipeline_id.to_string();
    let out = run_cmd(&[
        "pipelines", "run",
        "--org", org,
        "--project", project,
        "--id", &id_str,
        "--branch", branch,
        "-o", "json",
    ])?;
    let v: serde_json::Value = serde_json::from_str(&out)
        .map_err(|e| AzError::Other(format!("parse run response: {}", e)))?;
    let build_number = v["buildNumber"].as_str().unwrap_or("queued").to_string();
    Ok(build_number)
}


/// Creates a new release targeting a specific build artifact version.
/// `artifact_alias` is the alias from the release definition (e.g. "_Build JDE Connector").
/// `build_id` is the internal numeric build ID from the artifact's `version.id`.
/// Returns the new release name (e.g. "Release-44").
pub fn create_release(
    org: &str, project: &str,
    def_id: u64, artifact_alias: &str, build_id: &str,
) -> Result<String, AzError> {
    let def_id_str = def_id.to_string();
    // Format: "<alias>=<versionId>" — the artifact alias is the KEY, build ID is the VALUE.
    // Do NOT use "alias=<name>,versionId=<id>": that makes the CLI treat the literal
    // word "alias" as the artifact name, producing VS403230.
    let metadata   = format!("{}={}", artifact_alias, build_id);
    let out = run_cmd(&[
        "pipelines", "release", "create",
        "--org", org,
        "--project", project,
        "--definition-id", &def_id_str,
        "--artifact-metadata-list", &metadata,
        "-o", "json",
    ])?;
    let v: serde_json::Value = serde_json::from_str(&out)
        .map_err(|e| AzError::Other(format!("parse release create: {}", e)))?;
    Ok(v["name"].as_str().unwrap_or("Release created").to_string())
}

/// Find release definitions that consume a specific build pipeline as an artifact.
/// Checks each definition's artifact reference against the given build pipeline ID.
/// Runs definition shows in parallel — typically fast since there aren't many defs.
pub fn find_release_defs_for_pipeline(
    org: &str, project: &str,
    build_pipeline_id: u64,
) -> Result<Vec<ReleaseDefinition>, AzError> {
    let defs = list_release_definitions(org, project)?;
    let mut matched = Vec::new();
    for def in defs {
        let id_str = def.id.to_string();
        if let Ok(out) = run_cmd(&[
            "pipelines", "release", "definition", "show",
            "--org", org, "--project", project,
            "--id", &id_str, "-o", "json",
        ]) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&out) {
                let references_pipeline = v["artifacts"].as_array()
                    .map(|arts| arts.iter().any(|a| {
                        a["type"].as_str() == Some("Build")
                            && a["definitionReference"]["definition"]["id"]
                                .as_str()
                                .and_then(|s| s.parse::<u64>().ok())
                                == Some(build_pipeline_id)
                    }))
                    .unwrap_or(false);
                if references_pipeline {
                    matched.push(def);
                }
            }
        }
    }
    Ok(matched)
}
