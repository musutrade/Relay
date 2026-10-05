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
    pub workspace_task_id: i64,
    pub predecessor_task_id: i64,
    pub predecessor_generation: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_only: Option<crate::workflow::ReviewContinuation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_adoption: Option<crate::workflow::PinnedReviewAdoption>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u8,
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
fn root(config: &HostConfig, task: &Task, job: &Job) -> PathBuf {
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
        .custom_flags(libc::O_NOFOLLOW)
        .open(path.join("claim.json"))?;
    let mut bytes = Vec::new();
    file.take(128 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 128 * 1024 {
        return Err(io::Error::other("workspace claim exceeds bound"));
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}
fn lock(path: &Path) -> io::Result<Arc<File>> {
    if !fs::symlink_metadata(path)?.is_dir() || path.canonicalize()? != path {
        return Err(io::Error::other("workspace root was redirected"));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path.join("owner.lock"))?;
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
    if record.version != 1
        || record.workspace_task_id != expected_root
        || record.config_binding != config_binding(config, job)?
        || serde_json::to_value(&record.job).map_err(io::Error::other)?
            != serde_json::to_value(normalized(job)).map_err(io::Error::other)?
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
    let file = lock(&path).map_err(|e| {
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
        workspace_task_id: record.workspace_task_id,
        predecessor_task_id: task.id,
        predecessor_generation: task.generation,
        review_only: None,
        operator_adoption: None,
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
