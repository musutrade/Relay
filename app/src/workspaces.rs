//! Trusted-host fixed task workspace ownership. No queue state lives here.
use crate::host::{HostConfig, Job, Outcome, RunResult};
use relay::Task;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Continuation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_increase: Option<QuotaIncrease>,
    pub workspace_task_id: i64,
    pub predecessor_task_id: i64,
    pub predecessor_generation: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_only: Option<crate::workflow::ReviewContinuation>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaIncrease {
    pub previous_bytes: u64,
    pub new_bytes: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    quota_bytes: Option<u64>,
    workspace_task_id: i64,
    task_id: i64,
    generation: i64,
    owner: String,
    attempt: u64,
    config_binding: String,
    job: Job,
}
pub(crate) struct Workspace {
    pub path: PathBuf,
    pub file: Arc<File>,
    pub reused: bool,
}
fn failure(path: &Path, outcome: Outcome, error: impl ToString) -> Box<RunResult> {
    let mut result = RunResult::new(outcome, Some(error.to_string()));
    result.workspace = Some(path.to_owned());
    Box::new(result)
}
pub(crate) fn root(config: &HostConfig, task: &Task, job: &Job) -> PathBuf {
    config.workspace_root.join(format!(
        "task-{}",
        job.continuation
            .as_ref()
            .map_or(task.id, |c| c.workspace_task_id)
    ))
}
fn normalized(job: &Job) -> Job {
    let mut job = job.clone();
    job.continuation = None;
    job
}
fn config_binding(config: &HostConfig, job: &Job) -> io::Result<String> {
    let workflow = job
        .workflow
        .as_ref()
        .and_then(|name| config.workflows.get(name));
    let reviewer = workflow.and_then(|w| config.native_agents.get(&w.reviewer));
    let value = json!({"source":config.repositories.get(&job.repository),"agent":config.agents.get(&job.agent),
        "native":config.native_agents.get(&job.agent),"workflow":workflow,"reviewer":reviewer,
        "test":job.test.as_ref().or(workflow.map(|w|&w.test)).and_then(|name|config.tests.get(name)),
        "publisher":job.draft_pr_adapter.as_ref().and_then(|name|config.draft_pr_adapters.get(name))});
    crate::sessions::fingerprint(&value)
}
fn read_record(path: &Path) -> io::Result<Record> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path.join("claim.json"))?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("workspace claim must be a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(128 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 128 * 1024 {
        return Err(io::Error::other("workspace claim exceeds bound"));
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}
fn lock(path: &Path) -> io::Result<Arc<File>> {
    lock_with_create(path, false)
}
fn lock_with_create(path: &Path, create: bool) -> io::Result<Arc<File>> {
    if !fs::symlink_metadata(path)?.is_dir() || path.canonicalize()? != path {
        return Err(io::Error::other("workspace root was redirected"));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path.join("owner.lock"))?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("workspace lock must be a regular file"));
    }
    // SAFETY: flock operates on our live private descriptor; ownership is inherited
    // by each supervisor, but never by the configured command.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Arc::new(file))
}
fn inspect(config: &HostConfig, task: &Task, job: &Job, path: &Path) -> io::Result<Record> {
    if !fs::symlink_metadata(path)?.is_dir() || path.canonicalize()? != path {
        return Err(io::Error::other("workspace root was redirected"));
    }
    let record = read_record(path)?;
    let expected_root = job
        .continuation
        .as_ref()
        .map_or(task.id, |c| c.workspace_task_id);
    let mut expected = normalized(job);
    if expected.workspace_quota_bytes != record.job.workspace_quota_bytes {
        let proof = job
            .continuation
            .as_ref()
            .and_then(|continuation| continuation.quota_increase.as_ref())
            .ok_or_else(|| {
                io::Error::other(
                    "workspace quota changed without a validated continuation increase",
                )
            })?;
        let previous_result = read_stopped_result(path)?;
        if proof.previous_bytes == 0
            || proof.new_bytes <= proof.previous_bytes
            || Some(proof.previous_bytes) != record.quota_bytes
            || previous_result
                .resources
                .as_ref()
                .and_then(|resources| resources.quota_bytes)
                != Some(proof.previous_bytes)
            || expected.workspace_quota_bytes != Some(proof.new_bytes)
            || proof.new_bytes > config.workspace_byte_limit()
        {
            return Err(io::Error::other(
                "workspace quota increase proof does not match the stopped predecessor",
            ));
        }
        expected.workspace_quota_bytes = record.job.workspace_quota_bytes;
    }
    if record.version != 1
        || record.workspace_task_id != expected_root
        || record.config_binding != config_binding(config, job)?
        || serde_json::to_value(&record.job).map_err(io::Error::other)?
            != serde_json::to_value(expected).map_err(io::Error::other)?
    {
        return Err(io::Error::other(
            "workspace task, job, or selected profile binding changed",
        ));
    }
    if path.join("publication-attempt.json").exists() {
        return Err(io::Error::other(
            "publication was attempted; reconcile external effects locally before continuing",
        ));
    }
    Ok(record)
}
pub(crate) fn prepare(
    config: &HostConfig,
    task: &Task,
    job: &Job,
) -> Result<Workspace, Box<RunResult>> {
    let path = root(config, task, job);
    let reused = fs::symlink_metadata(&path).is_ok();
    if !reused {
        if job.continuation.is_some() {
            return Err(failure(
                &path,
                Outcome::Failure,
                "preserved workspace is missing; continuation never makes a new copy",
            ));
        }
        let entries = fs::read_dir(&config.workspace_root)
            .map_err(|e| failure(&path, Outcome::Failure, e))?;
        let mut count = 0;
        for entry in entries {
            let entry = entry.map_err(|e| failure(&path, Outcome::Failure, e))?;
            count += 1;
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("task-{}-generation-", task.id))
            {
                return Err(failure(
                    &entry.path(),
                    Outcome::Unknown,
                    "legacy generation workspace retained; inspect and migrate explicitly before recovery",
                ));
            }
        }
        if count >= config.max_retained_workspaces {
            return Err(failure(
                &path,
                Outcome::Failure,
                "workspace retention limit reached; trusted operator cleanup is required",
            ));
        }
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|e| failure(&path, Outcome::Unknown, e))?;
    }
    if !fs::symlink_metadata(&path)
        .map_err(|e| failure(&path, Outcome::Unknown, e))?
        .is_dir()
        || path
            .canonicalize()
            .map_err(|e| failure(&path, Outcome::Unknown, e))?
            != path
    {
        return Err(failure(
            &path,
            Outcome::Unknown,
            "workspace path was redirected",
        ));
    }
    let file = lock_with_create(&path, !reused).map_err(|e| {
        failure(
            &path,
            Outcome::Unknown,
            format!("workspace is not exclusively stopped: {e}"),
        )
    })?;
    let attempt = if reused {
        if !path.join("claim.json").is_file() {
            return Err(failure(
                &path,
                Outcome::Unknown,
                "workspace already exists without a verified claim; inspect old execution before recovery",
            ));
        }
        let record =
            inspect(config, task, job, &path).map_err(|e| failure(&path, Outcome::Failure, e))?;
        let valid = if let Some(previous) = &job.continuation {
            (record.task_id == previous.predecessor_task_id
                && record.generation == previous.predecessor_generation
                && task.id > record.task_id
                && task.generation == 1)
                || (record.task_id == task.id
                    && record.generation.checked_add(1) == Some(task.generation))
        } else {
            record.task_id == task.id && record.generation.checked_add(1) == Some(task.generation)
        };
        if !valid {
            return Err(failure(
                &path,
                if record.task_id == task.id {
                    Outcome::Unknown
                } else {
                    Outcome::Failure
                },
                "workspace claim is stale or already executed; only explicit stopped recovery or its matching continuation can reuse it",
            ));
        }
        if !path.join("workspace-ready").is_file() {
            return Err(failure(
                &path,
                Outcome::Failure,
                "workspace initialization was interrupted; preserve files for operator inspection",
            ));
        }
        record
            .attempt
            .checked_add(1)
            .ok_or_else(|| failure(&path, Outcome::Failure, "workspace attempt limit reached"))?
    } else {
        1
    };
    let record = Record {
        version: 1,
        quota_bytes: Some(config.workspace_byte_limit()),
        workspace_task_id: job
            .continuation
            .as_ref()
            .map_or(task.id, |c| c.workspace_task_id),
        task_id: task.id,
        generation: task.generation,
        owner: task.owner.clone().expect("validated claim"),
        attempt,
        config_binding: config_binding(config, job)
            .map_err(|e| failure(&path, Outcome::Failure, e))?,
        job: normalized(job),
    };
    crate::sessions::atomic_write(&path.join("claim.json"), &record)
        .map_err(|e| failure(&path, Outcome::Unknown, e))?;
    Ok(Workspace { path, file, reused })
}
pub(crate) fn continuation(
    config: &HostConfig,
    task: &Task,
    job: &Job,
) -> io::Result<Continuation> {
    let path = root(config, task, job);
    let _lock = lock(&path)?;
    let record = inspect(config, task, job, &path)?;
    if record.task_id != task.id
        || record.generation != task.generation
        || record.owner.as_str() != task.owner.as_deref().unwrap_or("")
    {
        return Err(io::Error::other(
            "workspace no longer belongs to this predecessor",
        ));
    }
    if !path.join("workspace-ready").is_file() {
        return Err(io::Error::other("workspace was not completely initialized"));
    }
    Ok(Continuation {
        quota_increase: None,
        workspace_task_id: record.workspace_task_id,
        predecessor_task_id: task.id,
        predecessor_generation: task.generation,
        review_only: None,
    })
}
pub(crate) fn attempt(path: &Path) -> io::Result<u64> {
    Ok(read_record(path)?.attempt)
}
pub(crate) fn mark_ready(path: &Path) -> io::Result<()> {
    crate::sessions::atomic_write(&path.join("workspace-ready"), &json!({"version":1}))
}
pub(crate) fn mark_publication(path: &Path, task: &Task) -> io::Result<()> {
    crate::sessions::atomic_write(
        &path.join("publication-attempt.json"),
        &json!({"task_id":task.id,"generation":task.generation}),
    )
}

pub(crate) fn exists_for(config: &HostConfig, task: &Task, job: &Job) -> bool {
    fs::symlink_metadata(root(config, task, job)).is_ok()
        || job.continuation.is_some()
        || fs::read_dir(&config.workspace_root)
            .map(|entries| {
                entries.filter_map(Result::ok).any(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(&format!("task-{}-generation-", task.id))
                })
            })
            .unwrap_or(true)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Finished {
    task_id: i64,
    generation: i64,
    owner: String,
    finished_at: u64,
}
pub(crate) fn mark_finished(path: &Path, task: &Task) -> io::Result<()> {
    let _lock = lock(path)?;
    let record = read_record(path)?;
    if record.task_id != task.id
        || record.generation != task.generation
        || record.owner.as_str() != task.owner.as_deref().unwrap_or("")
    {
        return Err(io::Error::other("completion ownership changed"));
    }
    let finished_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_secs();
    crate::sessions::atomic_write(
        &path.join("finished.json"),
        &Finished {
            task_id: task.id,
            generation: task.generation,
            owner: record.owner,
            finished_at,
        },
    )
}
pub(crate) fn cleanup(config: &HostConfig, store: &relay::Store) -> io::Result<usize> {
    let Some(retention) = config.successful_workspace_retention_seconds else {
        return Ok(0);
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_secs();
    let mut removed = 0;
    for entry in fs::read_dir(&config.workspace_root)?.take(10_001) {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(root_id) = name
            .strip_prefix("task-")
            .and_then(|id| id.parse::<i64>().ok())
            .filter(|id| *id > 0)
        else {
            continue;
        };
        let path = entry.path();
        let Ok(_lock) = lock(&path) else { continue };
        let Ok(record) = read_record(&path) else {
            continue;
        };
        if record.workspace_task_id != root_id {
            continue;
        }
        let Ok(file) = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path.join("finished.json"))
        else {
            continue;
        };
        let mut bytes = Vec::new();
        file.take(4097).read_to_end(&mut bytes)?;
        if bytes.len() > 4096 {
            continue;
        }
        let Ok(finished) = serde_json::from_slice::<Finished>(&bytes) else {
            continue;
        };
        if finished.task_id != record.task_id
            || finished.generation != record.generation
            || finished.owner != record.owner
            || now.saturating_sub(finished.finished_at) < retention
        {
            continue;
        }
        let Ok(task) = store.get(record.task_id) else {
            continue;
        };
        if task.state != relay::State::Finished
            || task.generation != record.generation
            || task.owner.as_deref() != Some(&record.owner)
        {
            continue;
        }
        let Ok(result) = serde_json::from_str::<RunResult>(task.result.as_deref().unwrap_or(""))
        else {
            continue;
        };
        if result.outcome != Outcome::Success || result.workspace.as_deref() != Some(&path) {
            continue;
        }
        fs::remove_dir_all(&path)?;
        removed += 1;
        if removed >= 16 {
            break;
        }
    }
    Ok(removed)
}

pub(crate) fn read_marker(path: &Path) -> io::Result<String> {
    read_bounded_record(path, 1024)
}
pub(crate) fn read_stopped_result(path: &Path) -> io::Result<RunResult> {
    serde_json::from_str(&read_bounded_record(
        &path.join("last-result.json"),
        16 * 1024,
    )?)
    .map_err(io::Error::other)
}
fn read_bounded_record(path: &Path, limit: u64) -> io::Result<String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("checkpoint must be a regular file"));
    }
    let mut text = String::new();
    file.take(limit + 1).read_to_string(&mut text)?;
    if text.len() as u64 > limit {
        return Err(io::Error::other("checkpoint exceeds its bound"));
    }
    Ok(text)
}

/// Metadata-only preflight. The execution path still verifies raw candidate bytes,
/// index, source cleanliness and compatible sessions before running any agent.
pub(crate) fn verify_candidate_checkpoint(
    config: &HostConfig,
    job: &Job,
    path: &Path,
) -> io::Result<()> {
    if job.workflow.is_none() {
        return Ok(());
    }
    let base: String = serde_json::from_str(&read_marker(&path.join("workflow-base.txt"))?)
        .map_err(io::Error::other)?;
    let candidate: String = serde_json::from_str(&read_marker(&path.join("candidate-head.json"))?)
        .map_err(io::Error::other)?;
    if !valid_sha(&base)
        || !valid_sha(&candidate)
        || git_head(&path.join("repository"))? != candidate
        || git_head(&config.repositories[&job.repository])? != base
    {
        return Err(io::Error::other(
            "preserved candidate or source HEAD differs from the host checkpoint",
        ));
    }
    Ok(())
}
fn valid_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}
fn git_head(repository: &Path) -> io::Result<String> {
    if repository.canonicalize()? != repository || !fs::symlink_metadata(repository)?.is_dir() {
        return Err(io::Error::other("repository path was redirected"));
    }
    let mut git = repository.join(".git");
    if fs::symlink_metadata(&git)?.is_file() {
        let pointer = read_bounded_record(&git, 4096)?;
        let destination = pointer
            .trim()
            .strip_prefix("gitdir: ")
            .ok_or_else(|| io::Error::other("invalid Git directory pointer"))?;
        git = repository.join(destination).canonicalize()?;
    }
    if !fs::symlink_metadata(&git)?.is_dir() || git.canonicalize()? != git {
        return Err(io::Error::other("Git metadata path was redirected"));
    }
    let head = read_bounded_record(&git.join("HEAD"), 1024)?;
    let head = head.trim();
    if valid_sha(head) {
        return Ok(head.into());
    }
    let reference = head
        .strip_prefix("ref: ")
        .filter(|value| {
            value.starts_with("refs/")
                && !value
                    .split('/')
                    .any(|part| part.is_empty() || part == "." || part == "..")
        })
        .ok_or_else(|| io::Error::other("unsupported Git HEAD"))?;
    let common = match read_bounded_record(&git.join("commondir"), 4096) {
        Ok(pointer) => git.join(pointer.trim()).canonicalize()?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => git.clone(),
        Err(error) => return Err(error),
    };
    match read_bounded_record(&common.join(reference), 1024) {
        Ok(value) if valid_sha(value.trim()) => return Ok(value.trim().into()),
        Ok(_) => return Err(io::Error::other("invalid Git reference")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error),
    }
    for line in read_bounded_record(&common.join("packed-refs"), 1024 * 1024)?.lines() {
        if let Some((sha, name)) = line.split_once(' ')
            && name == reference
            && valid_sha(sha)
        {
            return Ok(sha.into());
        }
    }
    Err(io::Error::other(
        "Git HEAD could not be verified from bounded metadata",
    ))
}

pub(crate) fn verify_review_checkpoint(
    config: &HostConfig,
    job: &Job,
    result: &RunResult,
    path: &Path,
) -> io::Result<()> {
    let review = crate::workflow::review_continuation(result, None).map_err(io::Error::other)?;
    let workflow = job
        .workflow
        .as_ref()
        .and_then(|name| config.workflows.get(name))
        .ok_or_else(|| io::Error::other("review continuation has no configured workflow"))?;
    let profile = &config.native_agents[&workflow.reviewer];
    if crate::sessions::enabled(profile) {
        let repository = path.join("reviewer-repository");
        if read_marker(&path.join("reviewer-candidate.txt"))? != review.candidate_sha
            || git_head(&repository)? != review.candidate_sha
        {
            return Err(io::Error::other(
                "reviewer candidate checkpoint does not match the stopped candidate",
            ));
        }
        let next_attempt = attempt(path)?
            .checked_add(1)
            .ok_or_else(|| io::Error::other("workspace attempt limit reached"))?;
        crate::sessions::Session::verify_reviewer_resume(path, &repository, profile, next_attempt)?;
    }
    Ok(())
}
