use crate::cli::{az_command, AzError};
use serde::Deserialize;

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
    pub id: u64,
    pub name: String,
    pub last_release_on: Option<String>,
}

/// Full release detail from `az pipelines release show`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReleaseArtifact {
    pub release_id: u64,
    pub release_name: String,
    pub created_on: String,
    pub build_number: String,
    pub build_id: String, // internal numeric ID used for az pipelines release create
    pub artifact_alias: String, // artifact alias in the definition, e.g. "_Build ERP Connector"
    pub branch: String,
    pub commit: String,
    pub environments: Vec<ReleaseEnvStatus>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReleaseEnvStatus {
    pub name: String,
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
        return Err(AzError::Other(if stderr.is_empty() {
            stdout
        } else {
            stderr
        }));
    }
    Ok(stdout)
}

// ── finding the org and project ───────────────────────────────────────────────

/// Where a detected org/project came from, for the status line.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectedTarget {
    pub org: String,
    pub project: Option<String>,
    pub source: &'static str,
}

/// Split any Azure DevOps URL into the org URL `az` wants and the project name.
///
/// Accepts what people actually paste: a pipelines page
/// (`https://dev.azure.com/org/My%20Project/_build?definitionId=3`), a repo
/// clone URL with a user in it, the SSH remote form, the old
/// `org.visualstudio.com` host, or a bare org name.
pub fn split_devops_url(input: &str) -> Option<(String, Option<String>)> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    let s = s.split(['?', '#']).next().unwrap_or(s);

    // SSH: git@ssh.dev.azure.com:v3/{org}/{project}/{repo}
    //      {org}@vs-ssh.visualstudio.com:v3/{org}/{project}/{repo}
    if let Some(idx) = s.find(":v3/") {
        let mut parts = s[idx + 4..].split('/');
        let org = parts.next().filter(|o| !o.is_empty())?;
        let project = parts.next().filter(|p| !p.is_empty()).map(percent_decode);
        return Some((org_url(&percent_decode(org)), project));
    }

    let rest = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    // drop "user@" in clone URLs
    let rest = match (rest.find('@'), rest.find('/')) {
        (Some(at), Some(slash)) if at < slash => &rest[at + 1..],
        (Some(at), None) => &rest[at + 1..],
        _ => rest,
    };
    let mut segs = rest.split('/').filter(|p| !p.is_empty());
    let host = segs.next()?;

    let (org, tail): (String, Vec<&str>) = if host.eq_ignore_ascii_case("dev.azure.com") {
        let org = segs.next()?;
        (percent_decode(org), segs.collect())
    } else if let Some(org) = host.strip_suffix(".visualstudio.com") {
        let mut tail: Vec<&str> = segs.collect();
        // collection segment in old-style URLs
        if tail.first() == Some(&"DefaultCollection") {
            tail.remove(0);
        }
        (org.to_string(), tail)
    } else if !host.contains('.') && !host.contains(' ') {
        // a bare org name, optionally followed by the project
        (percent_decode(host), segs.collect())
    } else {
        return None;
    };

    // the project is the first segment, unless it's an area like "_build"
    let project = tail
        .first()
        .filter(|p| !p.starts_with('_'))
        .map(|p| percent_decode(p));
    Some((org_url(&org), project))
}

fn org_url(org: &str) -> String {
    format!("https://dev.azure.com/{}", org)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Work out the org and project without asking: the folder's git remote if it
/// is hosted on Azure DevOps, otherwise the defaults set with
/// `az devops configure`.
pub fn detect_target(project_dir: &str) -> Option<DetectedTarget> {
    if let Some((org, project)) = git_remote(project_dir).and_then(|r| split_devops_url(&r)) {
        return Some(DetectedTarget {
            org,
            project,
            source: "the folder's git remote",
        });
    }
    let (org, project) = configured_defaults();
    org.map(|org| DetectedTarget {
        org,
        project,
        source: "your az devops defaults",
    })
}

fn git_remote(dir: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let is_ado = url.contains("dev.azure.com") || url.contains("visualstudio.com");
    is_ado.then_some(url)
}

/// `az devops configure --list` prints `organization = …` / `project = …`.
fn configured_defaults() -> (Option<String>, Option<String>) {
    let Ok(out) = run_cmd(&["devops", "configure", "--list"]) else {
        return (None, None);
    };
    let value = |key: &str| {
        out.lines()
            .filter_map(|l| l.split_once('='))
            .find(|(k, _)| k.trim() == key)
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let org = value("organization").and_then(|o| split_devops_url(&o).map(|(o, _)| o));
    (org, value("project"))
}

/// Resource ID `az rest` needs to get a token for Azure DevOps.
const DEVOPS_RESOURCE: &str = "499b84ac-1321-427f-aa17-267ca6975798";

fn devops_rest(url: &str) -> Result<serde_json::Value, AzError> {
    let out = run_cmd(&[
        "rest",
        "--method",
        "get",
        "--resource",
        DEVOPS_RESOURCE,
        "--url",
        url,
        "-o",
        "json",
    ])?;
    serde_json::from_str(&out).map_err(|e| AzError::Other(format!("parse {}: {}", url, e)))
}

/// Org URLs of every Azure DevOps organization the signed-in user belongs to.
/// `az devops` has no command for this, so it asks the profile and accounts
/// APIs directly.
pub fn list_organizations() -> Result<Vec<String>, AzError> {
    let me = devops_rest(
        "https://app.vssps.visualstudio.com/_apis/profile/profiles/me?api-version=7.1",
    )?;
    let member_id = me["publicAlias"]
        .as_str()
        .or_else(|| me["id"].as_str())
        .ok_or_else(|| AzError::Other("no member id in Azure DevOps profile".into()))?;
    let accounts = devops_rest(&format!(
        "https://app.vssps.visualstudio.com/_apis/accounts?memberId={}&api-version=7.1",
        member_id
    ))?;
    Ok(parse_organizations(&accounts))
}

fn parse_organizations(accounts: &serde_json::Value) -> Vec<String> {
    let items = accounts
        .as_array()
        .or_else(|| accounts["value"].as_array())
        .cloned()
        .unwrap_or_default();
    let mut names: Vec<&str> = items
        .iter()
        .filter_map(|a| a["accountName"].as_str())
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names.into_iter().map(org_url).collect()
}

/// Names of the projects the signed-in user can see in `org`.
pub fn list_projects(org: &str) -> Result<Vec<String>, AzError> {
    let out = run_cmd(&[
        "devops", "project", "list", "--org", org, "--top", "500", "-o", "json",
    ])?;
    let v: serde_json::Value =
        serde_json::from_str(&out).map_err(|e| AzError::Other(format!("parse projects: {}", e)))?;
    let items = v
        .as_array()
        .or_else(|| v["value"].as_array())
        .ok_or_else(|| AzError::Other("unexpected project list shape".into()))?;
    let mut names: Vec<String> = items
        .iter()
        .filter_map(|p| p["name"].as_str().map(str::to_string))
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    Ok(names)
}

// ── build pipeline APIs ───────────────────────────────────────────────────────

pub fn list_pipelines(org: &str, project: &str) -> Result<Vec<Pipeline>, AzError> {
    let out = run_cmd(&[
        "pipelines",
        "list",
        "--org",
        org,
        "--project",
        project,
        "-o",
        "json",
    ])?;
    serde_json::from_str::<Vec<Pipeline>>(&out)
        .map_err(|e| AzError::Other(format!("parse pipelines: {}", e)))
}

pub fn list_runs(org: &str, project: &str, pipeline_id: u64) -> Result<Vec<PipelineRun>, AzError> {
    let id_str = pipeline_id.to_string();
    let out = run_cmd(&[
        "pipelines",
        "runs",
        "list",
        "--org",
        org,
        "--project",
        project,
        "--pipeline-ids",
        &id_str,
        "--top",
        "20",
        "-o",
        "json",
    ])?;
    serde_json::from_str::<Vec<PipelineRun>>(&out)
        .map_err(|e| AzError::Other(format!("parse runs: {}", e)))
}

// ── build logs ────────────────────────────────────────────────────────────────

/// An error or warning a build step reported.
#[derive(Debug, Clone, PartialEq)]
pub struct StepIssue {
    /// "error" | "warning"
    pub kind: String,
    pub message: String,
}

/// One record of a build's timeline: a stage, phase, job or task (step).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TimelineRecord {
    pub id: String,
    pub parent_id: Option<String>,
    /// "Stage" | "Phase" | "Job" | "Task" | "Checkpoint" …
    pub kind: String,
    pub name: String,
    pub order: u64,
    pub state: String,
    pub result: Option<String>,
    pub log_id: Option<u64>,
    pub start_time: Option<String>,
    pub finish_time: Option<String>,
    pub issues: Vec<StepIssue>,
}

/// A job and its steps, in the order they ran.
#[derive(Debug, Clone, PartialEq)]
pub struct BuildJob {
    pub stage: Option<String>,
    pub job: TimelineRecord,
    pub steps: Vec<TimelineRecord>,
}

/// The steps of a build, grouped by job. Stages, phases and jobs with no
/// steps (skipped ones) are kept so the reader sees what didn't run.
pub fn build_timeline(org: &str, project: &str, build_id: u64) -> Result<Vec<BuildJob>, AzError> {
    let v = devops_rest(&format!(
        "{}/{}/_apis/build/builds/{}/timeline?api-version=7.1",
        org.trim_end_matches('/'),
        percent_encode(project),
        build_id
    ))?;
    Ok(group_timeline(parse_timeline(&v)))
}

/// The full text of one step's log.
pub fn build_log(org: &str, project: &str, build_id: u64, log_id: u64) -> Result<String, AzError> {
    // not JSON, so no `-o json`: `az rest` prints the text as-is
    run_cmd(&[
        "rest",
        "--method",
        "get",
        "--resource",
        DEVOPS_RESOURCE,
        "--url",
        &format!(
            "{}/{}/_apis/build/builds/{}/logs/{}?api-version=7.1",
            org.trim_end_matches('/'),
            percent_encode(project),
            build_id,
            log_id
        ),
    ])
}

/// Web page of a build, for opening it in the browser.
pub fn build_web_url(org: &str, project: &str, build_id: u64) -> String {
    format!(
        "{}/{}/_build/results?buildId={}&view=logs",
        org.trim_end_matches('/'),
        percent_encode(project),
        build_id
    )
}

fn parse_timeline(v: &serde_json::Value) -> Vec<TimelineRecord> {
    let Some(records) = v["records"].as_array() else {
        return Vec::new();
    };
    let text = |r: &serde_json::Value, k: &str| r[k].as_str().map(str::to_string);
    records
        .iter()
        .map(|r| TimelineRecord {
            id: text(r, "id").unwrap_or_default(),
            parent_id: text(r, "parentId"),
            kind: text(r, "type").unwrap_or_default(),
            name: text(r, "name").unwrap_or_default(),
            order: r["order"].as_u64().unwrap_or(u64::MAX),
            state: text(r, "state").unwrap_or_default(),
            result: text(r, "result"),
            log_id: r["log"]["id"].as_u64(),
            start_time: text(r, "startTime"),
            finish_time: text(r, "finishTime"),
            issues: r["issues"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .map(|i| StepIssue {
                            kind: text(i, "type").unwrap_or_default(),
                            message: text(i, "message").unwrap_or_default(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect()
}

fn group_timeline(records: Vec<TimelineRecord>) -> Vec<BuildJob> {
    use std::collections::HashMap;
    let by_id: HashMap<&str, &TimelineRecord> =
        records.iter().map(|r| (r.id.as_str(), r)).collect();
    let ancestor = |r: &TimelineRecord, kind: &str| -> Option<&TimelineRecord> {
        let mut cur = r.parent_id.as_deref().and_then(|id| by_id.get(id).copied());
        while let Some(p) = cur {
            if p.kind == kind {
                return Some(p);
            }
            cur = p.parent_id.as_deref().and_then(|id| by_id.get(id).copied());
        }
        None
    };

    let mut jobs: Vec<(u64, u64, Option<String>, BuildJob)> = records
        .iter()
        .filter(|r| r.kind == "Job")
        .map(|job| {
            let stage = ancestor(job, "Stage");
            let phase_order = ancestor(job, "Phase").map_or(job.order, |p| p.order);
            let mut steps: Vec<TimelineRecord> = records
                .iter()
                .filter(|t| t.kind == "Task" && t.parent_id.as_deref() == Some(job.id.as_str()))
                .cloned()
                .collect();
            steps.sort_by_key(|t| t.order);
            (
                stage.map_or(0, |s| s.order),
                phase_order,
                job.start_time.clone(),
                BuildJob {
                    stage: stage.map(|s| s.name.clone()),
                    job: job.clone(),
                    steps,
                },
            )
        })
        .collect();

    // phases that never got a job (skipped stages) still show up, as empty jobs
    for phase in records.iter().filter(|r| r.kind == "Phase") {
        let has_job = records
            .iter()
            .any(|r| r.kind == "Job" && r.parent_id.as_deref() == Some(phase.id.as_str()));
        if !has_job {
            let stage = ancestor(phase, "Stage");
            jobs.push((
                stage.map_or(0, |s| s.order),
                phase.order,
                None,
                BuildJob {
                    stage: stage.map(|s| s.name.clone()),
                    job: phase.clone(),
                    steps: Vec::new(),
                },
            ));
        }
    }

    // jobs that never started sort after the ones that did
    jobs.sort_by(|a, b| (a.0, a.1, a.2.is_none(), &a.2).cmp(&(b.0, b.1, b.2.is_none(), &b.2)));
    jobs.into_iter().map(|(_, _, _, j)| j).collect()
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{:02X}", b),
        })
        .collect()
}

// ── classic release pipeline APIs (az pipelines release …) ───────────────────

pub fn list_release_definitions(
    org: &str,
    project: &str,
) -> Result<Vec<ReleaseDefinition>, AzError> {
    let out = run_cmd(&[
        "pipelines",
        "release",
        "definition",
        "list",
        "--org",
        org,
        "--project",
        project,
        "-o",
        "json",
    ])?;
    let v: serde_json::Value = serde_json::from_str(&out)
        .map_err(|e| AzError::Other(format!("parse release defs: {}", e)))?;
    let items = v
        .as_array()
        .or_else(|| v["value"].as_array())
        .ok_or_else(|| AzError::Other("unexpected release definitions shape".into()))?;
    let mut defs: Vec<ReleaseDefinition> = items
        .iter()
        .filter_map(|e| {
            Some(ReleaseDefinition {
                id: e["id"].as_u64()?,
                name: e["name"].as_str()?.to_string(),
                last_release_on: e["lastRelease"]["createdOn"]
                    .as_str()
                    .map(|s| s.to_string()),
            })
        })
        .collect();
    // most recently released first
    defs.sort_by(|a, b| b.last_release_on.cmp(&a.last_release_on));
    Ok(defs)
}

#[derive(Debug, Clone)]
pub struct EnvInfo {
    pub name: String,
    /// The release ID currently deployed to this environment (`currentRelease.id`).
    pub current_release_id: u64,
}

/// Returns the artifact alias of the first Build-type artifact in a release definition.
/// This is the value that must be passed as `artifact_alias` when calling `create_release`.
pub fn get_release_def_artifact_alias(
    org: &str,
    project: &str,
    def_id: u64,
) -> Result<String, AzError> {
    let id_str = def_id.to_string();
    let out = run_cmd(&[
        "pipelines",
        "release",
        "definition",
        "show",
        "--org",
        org,
        "--project",
        project,
        "--id",
        &id_str,
        "-o",
        "json",
    ])?;
    let v: serde_json::Value = serde_json::from_str(&out)
        .map_err(|e| AzError::Other(format!("parse definition: {}", e)))?;
    v["artifacts"]
        .as_array()
        .and_then(|arts| arts.iter().find(|a| a["type"].as_str() == Some("Build")))
        .and_then(|a| a["alias"].as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| AzError::Other("No Build artifact found in release definition".into()))
}

/// Returns environments sorted by rank, each with its authoritative current release ID.
pub fn get_release_definition_envs(
    org: &str,
    project: &str,
    def_id: u64,
) -> Result<Vec<EnvInfo>, AzError> {
    let id_str = def_id.to_string();
    let out = run_cmd(&[
        "pipelines",
        "release",
        "definition",
        "show",
        "--org",
        org,
        "--project",
        project,
        "--id",
        &id_str,
        "-o",
        "json",
    ])?;
    let v: serde_json::Value = serde_json::from_str(&out)
        .map_err(|e| AzError::Other(format!("parse definition: {}", e)))?;

    let mut envs: Vec<(u64, EnvInfo)> = v["environments"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|e| {
            let rank = e["rank"].as_u64().unwrap_or(999);
            let name = e["name"].as_str()?.to_string();
            let current_release_id = e["currentRelease"]["id"].as_u64().unwrap_or(0);
            Some((
                rank,
                EnvInfo {
                    name,
                    current_release_id,
                },
            ))
        })
        .collect();

    envs.sort_by_key(|(rank, _)| *rank);
    Ok(envs.into_iter().map(|(_, info)| info).collect())
}

/// Fetches artifact details for a single release (build number, branch, commit).
/// Uses `az pipelines release show` which returns full artifact data.
pub fn get_release_artifact(
    org: &str,
    project: &str,
    release_id: u64,
) -> Result<ReleaseArtifact, AzError> {
    let id_str = release_id.to_string();
    let out = run_cmd(&[
        "pipelines",
        "release",
        "show",
        "--org",
        org,
        "--project",
        project,
        "--id",
        &id_str,
        "-o",
        "json",
    ])?;
    let v: serde_json::Value = serde_json::from_str(&out)
        .map_err(|e| AzError::Other(format!("parse release show: {}", e)))?;

    let artifact = v["artifacts"]
        .as_array()
        .and_then(|arts| arts.iter().find(|a| a["type"].as_str() == Some("Build")));

    let artifact_alias = artifact
        .and_then(|a| a["alias"].as_str())
        .unwrap_or("")
        .to_string();

    let build_number = artifact
        .and_then(|a| a["definitionReference"]["version"]["name"].as_str())
        .unwrap_or("—")
        .to_string();

    let build_id = artifact
        .and_then(|a| a["definitionReference"]["version"]["id"].as_str())
        .unwrap_or("")
        .to_string();

    let raw_branch = artifact
        .and_then(|a| a["definitionReference"]["branch"]["name"].as_str())
        .unwrap_or("")
        .to_string();
    let branch = raw_branch.trim_start_matches("refs/heads/").to_string();

    let commit = artifact
        .and_then(|a| {
            a["definitionReference"]["sourceVersion"]["id"]
                .as_str()
                .or_else(|| a["definitionReference"]["sourceVersion"]["name"].as_str())
        })
        .map(|s| s.get(..8).unwrap_or(s).to_string())
        .unwrap_or_default();

    let release_name = v["name"].as_str().unwrap_or("").to_string();
    let created_on = v["createdOn"].as_str().unwrap_or("").to_string();

    let environments = v["environments"]
        .as_array()
        .map(|envs| {
            envs.iter()
                .filter_map(|e| {
                    Some(ReleaseEnvStatus {
                        name: e["name"].as_str()?.to_string(),
                        status: e["status"].as_str().unwrap_or("notDeployed").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(ReleaseArtifact {
        release_id,
        release_name,
        created_on,
        build_number,
        build_id,
        artifact_alias,
        branch,
        commit,
        environments,
    })
}

/// Returns all branch names for the repository linked to a pipeline.
/// Calls `az pipelines show` to get the repo ID, then `az repos ref list`
/// to enumerate every refs/heads/* ref.
pub fn list_pipeline_branches(
    org: &str,
    project: &str,
    pipeline_id: u64,
) -> Result<Vec<String>, AzError> {
    let id_str = pipeline_id.to_string();

    // 1. Get the pipeline definition to find the repository
    let pipe_json = run_cmd(&[
        "pipelines",
        "show",
        "--org",
        org,
        "--project",
        project,
        "--id",
        &id_str,
        "-o",
        "json",
    ])?;
    let pipe: serde_json::Value = serde_json::from_str(&pipe_json)
        .map_err(|e| AzError::Other(format!("parse pipeline show: {}", e)))?;

    // The repository can be identified by name or id
    let repo_name = pipe["repository"]["name"]
        .as_str()
        .or_else(|| pipe["repository"]["id"].as_str())
        .ok_or_else(|| AzError::Other("pipeline has no repository field".into()))?
        .to_string();

    // 2. List all refs/heads/* in that repo
    let refs_json = run_cmd(&[
        "repos",
        "ref",
        "list",
        "--org",
        org,
        "--project",
        project,
        "--repository",
        &repo_name,
        "--filter",
        "heads",
        "-o",
        "json",
    ])?;
    let refs: serde_json::Value = serde_json::from_str(&refs_json)
        .map_err(|e| AzError::Other(format!("parse refs: {}", e)))?;

    let mut branches: Vec<String> = refs
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|r| {
            r["name"]
                .as_str()
                .map(|n| n.trim_start_matches("refs/heads/").to_string())
        })
        .filter(|b| !b.is_empty())
        .collect();

    branches.sort();
    Ok(branches)
}

/// Queues a new build run on the given pipeline and branch.
/// Returns the new run's build number on success.
pub fn trigger_build(
    org: &str,
    project: &str,
    pipeline_id: u64,
    branch: &str,
) -> Result<String, AzError> {
    let id_str = pipeline_id.to_string();
    let out = run_cmd(&[
        "pipelines",
        "run",
        "--org",
        org,
        "--project",
        project,
        "--id",
        &id_str,
        "--branch",
        branch,
        "-o",
        "json",
    ])?;
    let v: serde_json::Value = serde_json::from_str(&out)
        .map_err(|e| AzError::Other(format!("parse run response: {}", e)))?;
    let build_number = v["buildNumber"].as_str().unwrap_or("queued").to_string();
    Ok(build_number)
}

/// Creates a new release targeting a specific build artifact version.
/// `artifact_alias` is the alias from the release definition (e.g. "_Build ERP Connector").
/// `build_id` is the internal numeric build ID from the artifact's `version.id`.
/// Returns the new release name (e.g. "Release-44").
pub fn create_release(
    org: &str,
    project: &str,
    def_id: u64,
    artifact_alias: &str,
    build_id: &str,
) -> Result<String, AzError> {
    let def_id_str = def_id.to_string();
    // Format: "<alias>=<versionId>" — the artifact alias is the KEY, build ID is the VALUE.
    // Do NOT use "alias=<name>,versionId=<id>": that makes the CLI treat the literal
    // word "alias" as the artifact name, producing VS403230.
    let metadata = format!("{}={}", artifact_alias, build_id);
    let out = run_cmd(&[
        "pipelines",
        "release",
        "create",
        "--org",
        org,
        "--project",
        project,
        "--definition-id",
        &def_id_str,
        "--artifact-metadata-list",
        &metadata,
        "-o",
        "json",
    ])?;
    let v: serde_json::Value = serde_json::from_str(&out)
        .map_err(|e| AzError::Other(format!("parse release create: {}", e)))?;
    Ok(v["name"].as_str().unwrap_or("Release created").to_string())
}

/// Find release definitions that consume a specific build pipeline as an artifact.
/// Checks each definition's artifact reference against the given build pipeline ID.
/// Runs definition shows in parallel — typically fast since there aren't many defs.
pub fn find_release_defs_for_pipeline(
    org: &str,
    project: &str,
    build_pipeline_id: u64,
) -> Result<Vec<ReleaseDefinition>, AzError> {
    let defs = list_release_definitions(org, project)?;
    let mut matched = Vec::new();
    for def in defs {
        let id_str = def.id.to_string();
        if let Ok(out) = run_cmd(&[
            "pipelines",
            "release",
            "definition",
            "show",
            "--org",
            org,
            "--project",
            project,
            "--id",
            &id_str,
            "-o",
            "json",
        ]) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&out) {
                let references_pipeline = v["artifacts"]
                    .as_array()
                    .map(|arts| {
                        arts.iter().any(|a| {
                            a["type"].as_str() == Some("Build")
                                && a["definitionReference"]["definition"]["id"]
                                    .as_str()
                                    .and_then(|s| s.parse::<u64>().ok())
                                    == Some(build_pipeline_id)
                        })
                    })
                    .unwrap_or(false);
                if references_pipeline {
                    matched.push(def);
                }
            }
        }
    }
    Ok(matched)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(s: &str) -> Option<(String, Option<String>)> {
        split_devops_url(s)
    }

    fn both(org: &str, project: &str) -> Option<(String, Option<String>)> {
        Some((org.to_string(), Some(project.to_string())))
    }

    #[test]
    fn splits_a_pipelines_page_url() {
        assert_eq!(
            split("https://dev.azure.com/oryxenergies/Trading%20and%20corporate%20applications/_build"),
            both("https://dev.azure.com/oryxenergies", "Trading and corporate applications")
        );
        assert_eq!(
            split("https://dev.azure.com/acme/Ops/_build/results?buildId=42&view=logs"),
            both("https://dev.azure.com/acme", "Ops")
        );
    }

    #[test]
    fn org_only_urls_have_no_project() {
        assert_eq!(
            split("https://dev.azure.com/acme/"),
            Some(("https://dev.azure.com/acme".into(), None))
        );
        assert_eq!(
            split("https://dev.azure.com/acme/_settings"),
            Some(("https://dev.azure.com/acme".into(), None))
        );
        assert_eq!(
            split("acme"),
            Some(("https://dev.azure.com/acme".into(), None))
        );
    }

    #[test]
    fn splits_git_remotes() {
        assert_eq!(
            split("https://acme@dev.azure.com/acme/My%20Proj/_git/repo"),
            both("https://dev.azure.com/acme", "My Proj")
        );
        assert_eq!(
            split("git@ssh.dev.azure.com:v3/acme/My%20Proj/repo"),
            both("https://dev.azure.com/acme", "My Proj")
        );
        assert_eq!(
            split("acme@vs-ssh.visualstudio.com:v3/acme/Proj/repo"),
            both("https://dev.azure.com/acme", "Proj")
        );
    }

    #[test]
    fn splits_old_visualstudio_urls() {
        assert_eq!(
            split("https://acme.visualstudio.com/DefaultCollection/Proj/_git/repo"),
            both("https://dev.azure.com/acme", "Proj")
        );
        assert_eq!(
            split("https://acme.visualstudio.com/Proj/_build"),
            both("https://dev.azure.com/acme", "Proj")
        );
    }

    #[test]
    fn rejects_other_hosts() {
        assert_eq!(split("https://github.com/acme/repo"), None);
        assert_eq!(split(""), None);
    }

    #[test]
    fn parses_the_accounts_response() {
        let v = serde_json::json!({
            "count": 2,
            "value": [
                { "accountName": "zeta", "accountUri": "https://vssps.dev.azure.com:443/zeta/" },
                { "accountName": "Acme", "accountUri": "https://vssps.dev.azure.com:443/Acme/" }
            ]
        });
        assert_eq!(
            parse_organizations(&v),
            vec!["https://dev.azure.com/Acme", "https://dev.azure.com/zeta"]
        );
        assert!(parse_organizations(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn groups_a_staged_timeline_into_jobs() {
        let v = serde_json::json!({ "records": [
            { "id": "s2", "type": "Stage", "name": "Deploy prd", "order": 2, "result": "skipped" },
            { "id": "p2", "parentId": "s2", "type": "Phase", "name": "Deploy prd", "order": 1, "result": "skipped" },
            { "id": "s1", "type": "Stage", "name": "Deploy dev", "order": 1, "result": "failed" },
            { "id": "p1", "parentId": "s1", "type": "Phase", "name": "Deploy dev", "order": 1 },
            { "id": "j1", "parentId": "p1", "type": "Job", "name": "Deploy dev",
              "startTime": "2026-10-06T13:16:55Z", "result": "failed", "log": { "id": 15 } },
            { "id": "t2", "parentId": "j1", "type": "Task", "name": "Build defs", "order": 6,
              "result": "failed", "log": { "id": 8 },
              "issues": [{ "type": "error", "message": "Bash exited with code '1'." }] },
            { "id": "t1", "parentId": "j1", "type": "Task", "name": "Initialize job", "order": 1,
              "result": "succeeded", "log": { "id": 3 } },
            { "id": "c1", "parentId": "s1", "type": "Checkpoint", "name": "Checkpoint" }
        ]});
        let jobs = group_timeline(parse_timeline(&v));
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].stage.as_deref(), Some("Deploy dev"));
        let names: Vec<&str> = jobs[0].steps.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["Initialize job", "Build defs"]);
        assert_eq!(jobs[0].steps[1].log_id, Some(8));
        assert_eq!(
            jobs[0].steps[1].issues[0].message,
            "Bash exited with code '1'."
        );
        // the skipped stage is still listed, with no steps
        assert_eq!(jobs[1].stage.as_deref(), Some("Deploy prd"));
        assert!(jobs[1].steps.is_empty());
    }

    #[test]
    fn encodes_project_names_for_urls() {
        assert_eq!(
            percent_encode("Trading and corporate applications"),
            "Trading%20and%20corporate%20applications"
        );
        assert_eq!(percent_encode("R&D/Ops"), "R%26D%2FOps");
    }

    #[test]
    fn percent_decode_handles_odd_input() {
        assert_eq!(percent_decode("a%20b%C3%A9"), "a bé");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%é1"), "%é1");
    }
}
