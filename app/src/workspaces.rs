//! Trusted-host fixed task workspace ownership. No queue state lives here.
use crate::host::{HostConfig, Job, Outcome, RunResult};
use relay::Task;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Continuation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub developer_stage: Option<crate::replacement::StoppedStage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement: Option<crate::replacement::Replacement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_increase: Option<QuotaIncrease>,
    pub workspace_task_id: i64,
    pub predecessor_task_id: i64,
    pub predecessor_generation: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_only: Option<crate::workflow::ReviewContinuation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_adoption: Option<crate::workflow::PinnedReviewAdoption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_approved: Option<crate::publication::PinnedPublication>,
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
    fresh_session_role: Option<crate::replacement::Role>,
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
pub(crate) fn config_binding(config: &HostConfig, job: &Job) -> io::Result<String> {
    if job.role_selections.is_some() {
        let workflow = job
            .workflow
            .as_ref()
            .and_then(|name| config.workflows.get(name))
            .map(|workflow| crate::selection::effective_workflow(job, config, workflow));
        let developer =
            crate::selection::native_profile(job, config, false).map_err(io::Error::other)?;
        let reviewer =
            crate::selection::native_profile(job, config, true).map_err(io::Error::other)?;
        return crate::sessions::fingerprint(&json!({
            "source":config.repositories.get(&job.repository),
            "agent":config.agents.get(&job.agent),"native":developer,"workflow":workflow,"reviewer":reviewer,
            "test":job.test.as_ref().or(workflow.as_ref().map(|w|&w.test)).and_then(|name|config.tests.get(name)),
            "publisher":job.draft_pr_adapter.as_ref().and_then(|name|config.draft_pr_adapters.get(name)),
            "role_selections":job.role_selections,
        }));
    }
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
pub(crate) fn lock(path: &Path) -> io::Result<Arc<File>> {
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
    let replacement_transition =
        record.task_id != task.id && crate::replacement::current(job).is_some();
    if replacement_transition {
        let proof = crate::replacement::current(job).expect("replacement");
        let continuation = job.continuation.as_ref().expect("continuation");
        if record.task_id != continuation.predecessor_task_id
            || record.generation != continuation.predecessor_generation
        {
            return Err(io::Error::other(
                "replacement predecessor ownership changed",
            ));
        }
        crate::replacement::verify_transition(
            &record.job,
            job,
            &read_stopped_result(path)?,
            &record.owner,
            config,
        )?;
        crate::replacement::verify_stage_checkpoint(&proof.stopped_stage, path)?;
        verify_candidate_checkpoint(config, &record.job, path)?;
    }
    let mut expected = if replacement_transition {
        record.job.clone()
    } else {
        normalized(job)
    };
    // Quota is an independent approved delta, even when the role also changes.
    expected.workspace_quota_bytes = job.workspace_quota_bytes;
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
        || record.config_binding
            != config_binding(
                config,
                if replacement_transition {
                    &record.job
                } else {
                    job
                },
            )?
        || serde_json::to_value(&record.job).map_err(io::Error::other)?
            != serde_json::to_value(expected).map_err(io::Error::other)?
    {
        return Err(io::Error::other(
            "workspace task, job, or selected profile binding changed",
        ));
    }
    if fs::symlink_metadata(path.join("publication-attempt.json"))
        .map(|_| true)
        .or_else(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                Ok(false)
            } else {
                Err(error)
            }
        })?
    {
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
    let fresh_session_role = crate::replacement::current(job)
        .filter(|_| read_record(&path).is_ok_and(|record| record.task_id != task.id))
        .map(|proof| proof.role);
    let record = Record {
        version: 1,
        fresh_session_role,
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
    continuation_under_lease(config, task, job)
}
pub(crate) fn continuation_under_lease(
    config: &HostConfig,
    task: &Task,
    job: &Job,
) -> io::Result<Continuation> {
    let path = root(config, task, job);
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
        developer_stage: None,
        replacement: None,
        quota_increase: None,
        workspace_task_id: record.workspace_task_id,
        predecessor_task_id: task.id,
        predecessor_generation: task.generation,
        review_only: None,
        operator_adoption: None,
        publish_approved: None,
    })
}
pub(crate) fn consume_fresh_role_epoch(path: &Path, reviewer: bool) -> io::Result<()> {
    let mut record = read_record(path)?;
    if record
        .fresh_session_role
        .is_some_and(|role| role.reviewer() == reviewer)
    {
        record.fresh_session_role = None;
        crate::sessions::atomic_write(&path.join("claim.json"), &record)?;
    }
    Ok(())
}
pub(crate) fn fresh_role_epoch(path: &Path, reviewer: bool) -> io::Result<bool> {
    Ok(read_record(path)?
        .fresh_session_role
        .is_some_and(|role| role.reviewer() == reviewer))
}
pub(crate) fn role_epoch(path: &Path, reviewer: bool) -> io::Result<Option<String>> {
    Ok(read_record(path)?
        .job
        .role_epochs
        .and_then(|epochs| epochs.role(reviewer).map(str::to_owned)))
}
pub(crate) fn attempt(path: &Path) -> io::Result<u64> {
    Ok(read_record(path)?.attempt)
}
pub(crate) fn mark_ready(path: &Path) -> io::Result<()> {
    crate::sessions::atomic_write(&path.join("workspace-ready"), &json!({"version":1}))
}
pub(crate) fn mark_publication(path: &Path, task: &Task) -> io::Result<()> {
    // Never replace this marker, including a partial/crashed write. Its presence
    // means external effects may exist and manual reconciliation is required.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path.join("publication-attempt.json"))?;
    file.write_all(
        &serde_json::to_vec(&json!({"task_id":task.id,"generation":task.generation}))
            .map_err(io::Error::other)?,
    )?;
    file.sync_all()?;
    File::open(path)?.sync_all()
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
pub(crate) fn cleanup(
    config: &HostConfig,
    store: &relay::Store,
    control: &mut Connection,
) -> io::Result<usize> {
    let Some(retention) = config.successful_workspace_retention_seconds else {
        return Ok(0);
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_secs();
    let mut removed = 0;
    // Reservations and host claims use this same database writer boundary.
    // Take it before workspace locks, matching publish-approved admission order.
    let transaction = control
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(io::Error::other)?;
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
        let Ok(finished_at) = successful_completion(&path, &path, &record, store) else {
            continue;
        };
        match crate::publication::pending_retention(&transaction, record.task_id, now) {
            Ok(Some(pending)) if pending.protected => continue,
            Err(_) => continue,
            _ => {}
        }
        if now.saturating_sub(finished_at) < retention {
            continue;
        }
        fs::remove_dir_all(&path)?;
        removed += 1;
        if removed >= 16 {
            break;
        }
    }
    transaction.commit().map_err(io::Error::other)?;
    Ok(removed)
}

/// Shared with the host cleanup path: only matching durable success and the
/// matching host completion marker establish the start of the existing TTL.
fn successful_completion(
    records: &Path,
    workspace: &Path,
    record: &Record,
    store: &relay::Store,
) -> io::Result<u64> {
    let finished: Finished =
        serde_json::from_str(&read_bounded_record(&records.join("finished.json"), 4096)?)
            .map_err(io::Error::other)?;
    if finished.task_id != record.task_id
        || finished.generation != record.generation
        || finished.owner != record.owner
    {
        return Err(io::Error::other(
            "completion marker ownership does not match the current workspace claim",
        ));
    }
    let task = store.get(record.task_id).map_err(io::Error::other)?;
    if task.state != relay::State::Finished
        || task.generation != record.generation
        || task.owner.as_deref() != Some(&record.owner)
    {
        return Err(io::Error::other(
            "durable task ownership or completion does not match the workspace claim",
        ));
    }
    let result: RunResult =
        serde_json::from_str(task.result.as_deref().unwrap_or("")).map_err(io::Error::other)?;
    if result.outcome != Outcome::Success || result.workspace.as_deref() != Some(workspace) {
        return Err(io::Error::other(
            "current durable result is not a successful result for this workspace",
        ));
    }
    Ok(finished.finished_at)
}

/// Bounded, read-only policy observations. This is deliberately not an action
/// plan: no snapshot can authorize deletion or establish external PR status.
pub(crate) fn inventory(
    config: &HostConfig,
    store: &relay::Store,
    control: &Connection,
    before: Option<i64>,
) -> serde_json::Value {
    let started = Instant::now();
    let observed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut response = json!({
        "observed_at":observed_at,
        "policy":{"successful_retention_seconds":config.successful_workspace_retention_seconds,
            "automatic_cleanup_enabled":config.successful_workspace_retention_seconds.is_some()},
        "workspaces":[],"next_before":null,"complete":false,"reason":null
    });
    let listing = (|| -> io::Result<Vec<i64>> {
        let directory = crate::resources::open_inventory_root(&config.workspace_root)?;
        let mut ids = Vec::new();
        for (count, entry) in fs::read_dir(crate::resources::fd_path(&directory))?.enumerate() {
            if count >= 10_000 || started.elapsed() >= Duration::from_millis(100) {
                return Err(io::Error::other(
                    "configured root inventory exceeds its entry or time bound; no complete page is available",
                ));
            }
            let name = entry?.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(id) = name
                .strip_prefix("task-")
                .and_then(|id| id.parse::<i64>().ok())
                .filter(|id| *id > 0 && name == format!("task-{id}"))
            else {
                continue;
            };
            if before.is_none_or(|before| id < before) {
                ids.push(id);
            }
        }
        ids.sort_unstable_by(|a, b| b.cmp(a));
        Ok(ids)
    })();
    let ids = match listing {
        Ok(ids) => ids,
        Err(error) => {
            response["reason"] = json!(error.to_string());
            return response;
        }
    };
    response["complete"] = json!(true);
    if ids.len() > 16 {
        response["next_before"] = json!(ids[15]);
    }
    response["workspaces"] = json!(
        ids.into_iter()
            .take(16)
            .map(|id| { inventory_entry(config, store, control, id, started, observed_at) })
            .collect::<Vec<_>>()
    );
    response
}

fn inventory_entry(
    config: &HostConfig,
    store: &relay::Store,
    control: &Connection,
    root_id: i64,
    started: Instant,
    observed_at: u64,
) -> serde_json::Value {
    let path = config.workspace_root.join(format!("task-{root_id}"));
    let mut entry = json!({"workspace_task_id":root_id,"path":path,
        "current_owner":null,"references":[],"references_complete":false,"successor_reserved":false,
        "allocated_usage":{"allocated_bytes":null,"complete":false,"measured_at":observed_at,"reason":"workspace ownership has not been verified"},
        "retention":{"status":"unknown","reason":"workspace identity could not be verified","eligible_at":null}});
    let inspect = (|| -> io::Result<()> {
        if started.elapsed() >= Duration::from_millis(250) {
            return Err(io::Error::other(
                "inventory request time bound reached; refresh for a new observation",
            ));
        }
        // Anchor the directory once, and read only through this descriptor.
        // A renamed/symlink-replaced path cannot redirect control-file reads.
        let directory = crate::resources::open_inventory_root(&path)?;
        let records = crate::resources::fd_path(&directory);
        let record = read_record(&records)?;
        if record.version != 1 || record.workspace_task_id != root_id {
            return Err(io::Error::other(
                "workspace claim does not match its canonical task root",
            ));
        }
        let current = store.get(record.task_id).map_err(io::Error::other)?;
        if current.generation != record.generation
            || current.owner.as_deref() != Some(&record.owner)
            || task_root(&current) != Some(root_id)
        {
            return Err(io::Error::other(
                "current durable task identity does not match the workspace claim",
            ));
        }
        entry["current_owner"] = json!({"task_id":record.task_id,"generation":record.generation,
            "owner":record.owner,"state":current.state});
        let (references, complete, reserved, active) =
            shared_references(store, control, root_id, started)?;
        let current_is_latest = references
            .last()
            .is_some_and(|reference| reference["task_id"] == current.id);
        entry["references"] = json!(references);
        entry["references_complete"] = json!(complete);
        entry["successor_reserved"] = json!(reserved);
        let publication_retention =
            crate::publication::pending_retention(control, record.task_id, observed_at);
        if started.elapsed() < Duration::from_millis(250) {
            entry["allocated_usage"] = json!(crate::resources::measure_allocated_directory(
                &directory, config
            ));
        } else {
            entry["allocated_usage"]["reason"] =
                json!("inventory request time bound reached before allocation observation");
        }
        let publication_retention = match publication_retention {
            Ok(pending) => pending,
            Err(_) if active || current.state != relay::State::Finished || reserved => {
                // A known owner/reservation is sufficient evidence of protection
                // even when optional publication metadata cannot be interpreted.
                // Do not invent an expiry or downgrade that fact to unknown.
                entry["retention"] = json!({"status":"protected","reason":"an active/queued owner or reserved successor still references this workspace; publication retention metadata cannot be verified","eligible_at":null});
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let released_publication = publication_retention
            .as_ref()
            .is_some_and(|pending| !pending.protected);
        if let Some(pending) = &publication_retention
            && pending.protected
        {
            entry["retention"] = json!({"status":"protected","reason":pending.reason,"eligible_at":null,"authorization_expires_at":pending.expires_at});
        } else if !released_publication
            && (active || current.state != relay::State::Finished || reserved)
        {
            entry["retention"] = json!({"status":"protected","reason":"an active/queued owner or reserved successor still references this workspace","eligible_at":null});
        } else if !complete || (!current_is_latest && !released_publication) {
            return Err(io::Error::other(
                "shared task history is incomplete or does not end at the current owner",
            ));
        } else if current
            .result
            .as_deref()
            .and_then(|text| serde_json::from_str::<RunResult>(text).ok())
            .is_some_and(|result| result.outcome != Outcome::Success)
        {
            entry["retention"] = json!({"status":"protected","reason":"the current owner has no successful outcome; failure, cancellation, timeout and unknown evidence are retained by the existing success-only policy","eligible_at":null});
        } else {
            // Never contend with prepare or mark_finished: active/reserved and
            // non-success tasks above need no probe, and the matching completion
            // marker must already exist before attempting any lock. Successful
            // finished tasks cannot create an ordinary continuation.
            successful_completion(&records, &path, &record, store)?;
            // A read descriptor and nonblocking flock neither create nor rewrite a
            // lock file. Failure is a protection fact, never proof of stopped work.
            let lease = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(records.join("owner.lock"));
            let lease = lease
                .ok()
                .filter(|file| file.metadata().is_ok_and(|metadata| metadata.is_file()))
                .filter(|file| {
                    // SAFETY: live private descriptor, nonblocking advisory lock.
                    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
                });
            if lease.is_none() {
                entry["retention"] = json!({"status":"protected","reason":"host ownership lock is held or unavailable; stopped execution is not established","eligible_at":null});
                return Ok(());
            }
            let current_record = read_record(&records)?;
            if current_record.task_id != record.task_id
                || current_record.generation != record.generation
                || current_record.owner != record.owner
                || current_record.workspace_task_id != root_id
            {
                return Err(io::Error::other(
                    "workspace ownership changed during the observation",
                ));
            }
            let finished_at = successful_completion(&records, &path, &current_record, store)?;
            if let Some(ttl) = config.successful_workspace_retention_seconds {
                let eligible_at = finished_at.checked_add(ttl).ok_or_else(|| {
                    io::Error::other("retention timestamp exceeds its supported range")
                })?;
                entry["retention"] = json!({"status":if observed_at >= eligible_at {"eligible"} else {"waiting"},
                    "reason":"current durable success and matching host completion satisfy the existing success-only TTL evidence; this snapshot is not deletion authorization or a reclaimable-space guarantee",
                    "eligible_at":eligible_at});
            } else {
                entry["retention"] = json!({"status":"disabled","reason":"automatic successful-workspace retention is not configured; no cleanup is enabled by this preview","eligible_at":null});
            }
            if let Some(pending) = &publication_retention {
                entry["retention"]["reason"] = json!(pending.reason);
                entry["retention"]["authorization_expires_at"] = json!(pending.expires_at);
            }
        }
        Ok(())
    })();
    if let Err(error) = inspect {
        entry["retention"] =
            json!({"status":"unknown","reason":error.to_string(),"eligible_at":null});
    }
    entry
}

fn task_root(task: &Task) -> Option<i64> {
    let job: Job = serde_json::from_str(&task.payload).ok()?;
    Some(
        job.continuation
            .as_ref()
            .map_or(task.id, |continuation| continuation.workspace_task_id),
    )
}

fn shared_references(
    store: &relay::Store,
    control: &Connection,
    root_id: i64,
    started: Instant,
) -> io::Result<(Vec<serde_json::Value>, bool, bool, bool)> {
    let mut references = Vec::new();
    let mut task_id = root_id;
    let mut active = false;
    let mut reserved = false;
    for _ in 0..100 {
        if started.elapsed() >= Duration::from_millis(250) {
            return Ok((references, false, reserved, active));
        }
        let task = store.get(task_id).map_err(io::Error::other)?;
        if task_root(&task) != Some(root_id)
            || references
                .iter()
                .any(|reference: &serde_json::Value| reference["task_id"] == task_id)
        {
            return Ok((references, false, reserved, active));
        }
        active |= task.state != relay::State::Finished;
        reserved |= task.id != root_id && task.state != relay::State::Finished;
        let outcome = task
            .result
            .as_deref()
            .and_then(|text| serde_json::from_str::<RunResult>(text).ok())
            .map(|result| result.outcome);
        references.push(json!({"task_id":task.id,"generation":task.generation,"state":task.state,"outcome":outcome}));
        // The unique predecessor index and task key index bound each lookup;
        // no full task-table or host-filesystem scan is required.
        let successor: Option<Option<i64>> = control.query_row(
            "SELECT COALESCE(c.task_id,t.id) FROM app_continuations c LEFT JOIN tasks t ON t.key=c.key AND t.payload=c.payload WHERE c.predecessor_id=?1",
            [task_id], |row| row.get(0)).optional().map_err(io::Error::other)?;
        match successor {
            None => return Ok((references, true, reserved, active)),
            Some(None) => return Ok((references, true, true, active)),
            Some(Some(id)) => task_id = id,
        }
    }
    Ok((references, false, reserved, active))
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
pub(crate) fn read_bounded_record(path: &Path, limit: u64) -> io::Result<String> {
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
    verify_review_candidate_checkpoint(config, job, result, path)?;
    verify_review_session(config, job, path)
}
pub(crate) fn verify_review_candidate_checkpoint(
    config: &HostConfig,
    job: &Job,
    result: &RunResult,
    path: &Path,
) -> io::Result<()> {
    let review = crate::workflow::review_continuation(result, None).map_err(io::Error::other)?;
    let profile = crate::selection::native_profile(job, config, true)
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other("review continuation has no configured reviewer"))?;
    let profile = &profile;
    if crate::sessions::reviewer_checkout(profile) {
        let repository = path.join("reviewer-repository");
        if read_marker(&path.join("reviewer-candidate.txt"))? != review.candidate_sha
            || git_head(&repository)? != review.candidate_sha
        {
            return Err(io::Error::other(
                "reviewer candidate checkpoint does not match the stopped candidate",
            ));
        }
    }
    Ok(())
}
fn verify_review_session(config: &HostConfig, job: &Job, path: &Path) -> io::Result<()> {
    let profile = crate::selection::native_profile(job, config, true)
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other("reviewer missing"))?;
    if crate::sessions::enabled(&profile) {
        let next_attempt = attempt(path)?
            .checked_add(1)
            .ok_or_else(|| io::Error::other("workspace attempt limit reached"))?;
        crate::sessions::Session::verify_reviewer_resume(
            path,
            &path.join("reviewer-repository"),
            &profile,
            next_attempt,
            job.role_epochs
                .as_ref()
                .and_then(|epochs| epochs.role(true)),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod selection_binding_tests {
    use super::*;
    #[test]
    fn legacy_workspace_fingerprint_is_stable_and_selected_settings_are_bound() {
        let config: HostConfig = serde_json::from_value(json!({
            "workspace_root":"/fixture/runs","repositories":{"repo":"/fixture/source"},
            "native_agents":{"dev":{"provider":"codex_cli","program":"/bin/true"}}
        }))
        .unwrap();
        let mut job: Job = serde_json::from_value(
            json!({"repository":"repo","requirements":"Do work","agent":"dev"}),
        )
        .unwrap();
        assert_eq!(
            config_binding(&config, &job).unwrap(),
            "fnv1a-v1-6de094035fdf55a3"
        );
        job.role_selections = Some(
            serde_json::from_value(
                json!({"developer":{"profile":"dev","model":{"value":"first","source":"manual"}}}),
            )
            .unwrap(),
        );
        let selected = config_binding(&config, &job).unwrap();
        job.role_selections
            .as_mut()
            .unwrap()
            .developer
            .as_mut()
            .unwrap()
            .model
            .as_mut()
            .unwrap()
            .value = "second".into();
        assert_ne!(config_binding(&config, &job).unwrap(), selected);
        let selected = config_binding(&config, &job).unwrap();
        let mut changed = config.clone();
        changed
            .native_agents
            .get_mut("dev")
            .unwrap()
            .env
            .insert("POLICY".into(), "different".into());
        assert_ne!(config_binding(&changed, &job).unwrap(), selected);
    }
}
