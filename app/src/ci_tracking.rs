//! App-owned, exact-publication CI observation. This lane never claims core work,
//! invokes a model, changes a PR, or establishes remote merge eligibility.
use crate::{
    Application, Error, Result, action_unavailable,
    host::{CommandProfile, Outcome, RunResult},
};
use rand_core::RngCore;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

const GUARD: &str = ".ci-observation-in-flight";
const CONTROL_DIRECTORY: &str = ".ci-tracking";
const CONTROL_MARKER: &str = ".relay-ci-control";
const CONTROL_VERSION: &[u8] = b"relay-ci-control-v1\n";
const MAX_EVIDENCE: usize = 64 * 1024;
const MAX_RECORD: usize = 128 * 1024;
const REQUEST_TIMEOUT_SECONDS: u64 = 30;
fn poll_default() -> u64 {
    60
}
fn window_default() -> u64 {
    86_400
}
fn event_default() -> String {
    "pull_request".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CiPolicy {
    pub github_repository: String,
    pub base_branch: String,
    pub observer: CommandProfile,
    pub workflow_id: u64,
    pub app_id: u64,
    #[serde(default = "event_default")]
    pub event: String,
    pub required_jobs: Vec<String>,
    #[serde(default = "poll_default")]
    pub poll_interval_seconds: u64,
    #[serde(default = "window_default")]
    pub observation_window_seconds: u64,
}
impl CiPolicy {
    pub(crate) fn validate(&self) -> std::result::Result<(), String> {
        let parts: Vec<_> = self.github_repository.split('/').collect();
        if parts.len() != 2
            || parts.iter().any(|s| {
                s.is_empty()
                    || matches!(*s, "." | "..")
                    || s.len() > 100
                    || !s
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_ .".contains(&b))
                    || s.contains(' ')
            })
            || self.base_branch.is_empty()
            || self.base_branch.len() > 128
            || !valid_branch(&self.base_branch)
            || self.workflow_id == 0
            || self.app_id == 0
            || self.event != "pull_request"
            || !(60..=3600).contains(&self.poll_interval_seconds)
            || !(60..=86_400).contains(&self.observation_window_seconds)
            || self.required_jobs.is_empty()
            || self.required_jobs.len() > 8
            || self.required_jobs.iter().any(|s| {
                s.trim().is_empty()
                    || s != s.trim()
                    || s.len() > 256
                    || s.chars().any(char::is_control)
            })
            || self.required_jobs.iter().collect::<BTreeSet<_>>().len() != self.required_jobs.len()
        {
            return Err("CI policy requires one GitHub repository/base, numeric Actions workflow/app IDs, pull_request event, 1-8 unique exact job names, poll 60-3600s and window 60-86400s".into());
        }
        Ok(())
    }
    fn digest(&self, workspace_root: &Path) -> Result<String> {
        let m = fs::metadata(&self.observer.program).map_err(invalid_io)?;
        let executable = json!({"path":self.observer.program.canonicalize().map_err(invalid_io)?,"device":m.dev(),"inode":m.ino(),"len":m.len(),"mtime":m.mtime(),"mtime_nsec":m.mtime_nsec(),"ctime":m.ctime(),"ctime_nsec":m.ctime_nsec(),"mode":m.mode()});
        // Never persist command args or environment: either may contain credentials.
        Ok(crate::publication::digest(
            &serde_json::to_vec(
                &json!({"policy":self,"executable":executable,"workspace_root":workspace_root}),
            )
            .expect("serializable policy"),
        ))
    }
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CiStartRequest {
    pub key: String,
    pub policy: String,
    pub policy_digest: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CiControlRequest {
    pub expected_revision: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CiDiagnostic {
    pub code: String,
    pub message: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CiTrack {
    pub id: i64,
    pub publication_task_id: i64,
    pub policy: String,
    pub policy_digest: String,
    pub status: String,
    pub attempt: u64,
    pub revision: u64,
    pub stop_requested: bool,
    pub repository: String,
    pub base_branch: String,
    pub head_sha: String,
    pub head_branch: String,
    pub pr_number: u64,
    pub pr_url: String,
    pub workflow_id: u64,
    pub app_id: u64,
    pub event: String,
    pub required_jobs: Vec<String>,
    pub poll_interval_seconds: u64,
    pub observation_window_seconds: u64,
    pub created_at: u64,
    pub window_generation: u64,
    pub window_started_at: u64,
    pub last_observed_at: Option<u64>,
    pub deadline: u64,
    pub next_poll_at: u64,
    pub observed_repository_id: Option<u64>,
    pub observed_pr_id: Option<u64>,
    pub observed_base_sha: Option<String>,
    pub latest_evidence: Option<Value>,
    pub diagnostic: Option<CiDiagnostic>,
    pub remote_merge_eligibility: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Record {
    view: CiTrack,
    key: String,
    publication_result_sha256: String,
    active: bool,
    transient_failures: u32,
    last_resume_revision: Option<u64>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Publication {
    repository: String,
    base_branch: String,
    head_sha: String,
    head_branch: String,
    pr_number: u64,
    pr_url: String,
}
fn invalid_io(error: io::Error) -> Error {
    Error::Invalid(format!("CI local state: {error}"))
}
fn now() -> Result<u64> {
    crate::publication::now().map_err(invalid_io)
}
fn sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64)
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn valid_branch(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./-".contains(&b))
        && !value.contains("..")
        && !value.contains("//")
        && !value.ends_with('/')
        && value
            .split('/')
            .all(|p| !p.starts_with('.') && !p.ends_with('.') && !p.ends_with(".lock"))
}
fn unavailable(code: &str, message: &str) -> Error {
    action_unavailable(code, message)
}
fn diagnostic(record: &mut Record, status: &str, code: &str, message: &str) {
    record.view.status = status.into();
    record.view.diagnostic = Some(CiDiagnostic {
        code: code.into(),
        message: message.into(),
    });
}
fn publication(task: &relay::Task) -> Result<Publication> {
    if task.state != relay::State::Finished {
        return Err(unavailable(
            "ci_publication_unavailable",
            "CI tracking requires a finished successful real draft publication",
        ));
    }
    let result: RunResult =
        serde_json::from_str(task.result.as_deref().unwrap_or("")).map_err(|_| {
            unavailable(
                "ci_publication_unavailable",
                "publication result is missing or invalid",
            )
        })?;
    let workflow = result.workflow.as_ref().ok_or_else(|| {
        unavailable(
            "ci_publication_unavailable",
            "exact-candidate publication receipt is missing",
        )
    })?;
    let receipt = workflow.publication.as_ref().ok_or_else(|| {
        unavailable(
            "ci_publication_unavailable",
            "exact-candidate publication receipt is missing",
        )
    })?;
    if result.outcome != Outcome::Success
        || workflow.reconciliation_required
        || receipt.dry_run
        || !receipt.draft
        || !sha(&receipt.candidate_sha)
        || workflow.candidate_sha.as_deref() != Some(&receipt.candidate_sha)
        || workflow.reviewed_sha.as_deref() != Some(&receipt.candidate_sha)
    {
        return Err(unavailable(
            "ci_publication_unavailable",
            "CI tracking requires a successful, reconciled, real draft publication of the reviewed exact candidate",
        ));
    }
    let job: crate::host::Job = serde_json::from_str(&task.payload)
        .map_err(|_| Error::Invalid("publication payload is invalid".into()))?;
    let base=receipt.base_branch.clone().or_else(|| crate::publication::current(&job).filter(|p|p.request.candidate_sha==receipt.candidate_sha && p.request.github_repository==receipt.repository).map(|p|p.request.base_branch.clone())).ok_or_else(||unavailable("ci_legacy_base_unknown","this historical direct publication has no immutable base branch receipt; current configuration cannot supply it"))?;
    if base.is_empty()
        || base.len() > 128
        || base.chars().any(char::is_control)
        || receipt.branch.is_empty()
        || receipt.branch.len() > 256
    {
        return Err(Error::Invalid("invalid publication branch identity".into()));
    }
    let url = receipt
        .url
        .as_ref()
        .ok_or_else(|| Error::Invalid("real publication lacks PR URL".into()))?;
    let number = url
        .strip_prefix(&format!("https://github.com/{}/pull/", receipt.repository))
        .filter(|s| !s.starts_with('0') && !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| {
            Error::Invalid("publication URL is not the exact GitHub repository PR".into())
        })?;
    Ok(Publication {
        repository: receipt.repository.clone(),
        base_branch: base,
        head_sha: receipt.candidate_sha.clone(),
        head_branch: receipt.branch.clone(),
        pr_number: number,
        pr_url: url.clone(),
    })
}
pub(crate) fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS app_ci_storage(id INTEGER PRIMARY KEY CHECK(id=1),root TEXT NOT NULL,device TEXT NOT NULL,inode TEXT NOT NULL); CREATE TABLE IF NOT EXISTS app_ci_tracks(id INTEGER PRIMARY KEY AUTOINCREMENT,key TEXT UNIQUE NOT NULL,task_id INTEGER NOT NULL,policy TEXT NOT NULL,status TEXT NOT NULL,revision INTEGER NOT NULL,attempt INTEGER NOT NULL,active INTEGER NOT NULL DEFAULT 0,next_poll_at INTEGER NOT NULL,record TEXT NOT NULL CHECK(length(CAST(record AS BLOB))<=131072),UNIQUE(task_id,policy)); CREATE UNIQUE INDEX IF NOT EXISTS app_ci_single_active ON app_ci_tracks(active) WHERE active=1; CREATE TABLE IF NOT EXISTS app_ci_audit(id INTEGER PRIMARY KEY AUTOINCREMENT,track_id INTEGER NOT NULL,revision INTEGER NOT NULL,attempt INTEGER NOT NULL,at INTEGER NOT NULL,event TEXT NOT NULL,status TEXT NOT NULL,diagnostic_code TEXT,diagnostic TEXT,evidence_sha256 TEXT,UNIQUE(track_id,revision));")?;
    Ok(())
}
fn bound_storage(db: &Connection) -> Result<Option<(PathBuf, String, String)>> {
    Ok(db
        .query_row(
            "SELECT root,device,inode FROM app_ci_storage WHERE id=1",
            [],
            |row| {
                Ok((
                    PathBuf::from(row.get::<_, String>(0)?),
                    row.get(1)?,
                    row.get(2)?,
                ))
            },
        )
        .optional()?)
}
pub(crate) fn storage_root(
    db: &Connection,
    workspace_root: &Path,
    identity: &str,
) -> Result<PathBuf> {
    Ok(bound_storage(db)?
        .map(|(root, _, _)| root)
        .unwrap_or_else(|| workspace_root.join(CONTROL_DIRECTORY).join(identity)))
}
fn read(db: &Connection, id: i64) -> Result<Record> {
    let raw: String = db
        .query_row("SELECT record FROM app_ci_tracks WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .optional()?
        .ok_or_else(|| Error::Invalid("CI track does not exist".into()))?;
    serde_json::from_str(&raw).map_err(|_| Error::Invalid("CI track record is invalid".into()))
}
fn write(db: &Connection, record: &Record, event: &str) -> Result<()> {
    let raw = serde_json::to_string(record).expect("serializable CI record");
    if raw.len() > MAX_RECORD {
        return Err(Error::Invalid("CI record exceeds 128 KiB".into()));
    }
    db.execute("UPDATE app_ci_tracks SET status=?2,revision=?3,attempt=?4,active=?5,next_poll_at=?6,record=?7 WHERE id=?1",params![record.view.id,record.view.status,record.view.revision,record.view.attempt,record.active,record.view.next_poll_at,raw])?;
    db.execute("INSERT INTO app_ci_audit(track_id,revision,attempt,at,event,status,diagnostic_code,diagnostic,evidence_sha256) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![record.view.id,record.view.revision,record.view.attempt,now()?,event,record.view.status,record.view.diagnostic.as_ref().map(|d|&d.code),record.view.diagnostic.as_ref().map(|d|serde_json::to_string(d).expect("serializable diagnostic")),record.view.latest_evidence.as_ref().map(|e|crate::publication::digest(e.to_string().as_bytes()))])?;
    Ok(())
}
fn private_directory(path: &Path) -> bool {
    fs::symlink_metadata(path).ok().is_some_and(|m| {
        m.is_dir() && m.mode() & 0o777 == 0o700 && m.uid() == unsafe { libc::geteuid() }
    })
}
/// Only this exact, versioned, private host-owned control namespace is excluded
/// from the separate task-retention count. Unknown entries retain old safeguards.
pub(crate) fn is_control_directory(path: &Path) -> bool {
    if path
        .file_name()
        .is_none_or(|name| name != CONTROL_DIRECTORY)
        || !private_directory(path)
    {
        return false;
    }
    let marker_path = path.join(CONTROL_MARKER);
    let Ok(file) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&marker_path)
    else {
        return false;
    };
    let Ok(meta) = file.metadata() else {
        return false;
    };
    if !meta.is_file()
        || meta.mode() & 0o777 != 0o600
        || meta.uid() != unsafe { libc::geteuid() }
        || !matches_file(&marker_path, &file)
    {
        return false;
    }
    let mut bytes = Vec::new();
    file.take(64).read_to_end(&mut bytes).is_ok() && bytes == CONTROL_VERSION
}
impl Application {
    pub fn ci_for_task(&self, id: i64) -> Result<Vec<CiTrack>> {
        let state = self.state.lock().map_err(|_| Error::Poisoned)?;
        ci_for_task(&state.control, id)
    }
    pub fn ci_get(&self, id: i64) -> Result<CiTrack> {
        Ok(read(&self.state.lock().map_err(|_| Error::Poisoned)?.control, id)?.view)
    }
    pub fn ci_preview(&self, id: i64) -> Result<Value> {
        let state = self.state.lock().map_err(|_| Error::Poisoned)?;
        let task = state.store.get(id)?;
        let tracks = ci_for_task(&state.control, id)?;
        let source = match publication(&task) {
            Ok(s) => s,
            Err(error) => {
                return Ok(
                    json!({"eligible":false,"reason":error.to_string(),"publication":null,"policies":[],"tracks":tracks,"remote_merge_eligibility":"not_established"}),
                );
            }
        };
        let lane_diagnostic = self.ci_lane_diagnostic(&state.control)?;
        let mut policies = Vec::new();
        let mut unavailable_policies = Vec::new();
        for (name, policy) in self.host.config().ci_policies.iter().filter(|(_, p)| {
            p.github_repository == source.repository && p.base_branch == source.base_branch
        }) {
            match policy.digest(&self.host.config().workspace_root) {
                Ok(digest) => policies.push(json!({"name":name,"policy_digest":digest,"workflow_id":policy.workflow_id,"app_id":policy.app_id,"event":policy.event,"required_jobs":policy.required_jobs,"poll_interval_seconds":policy.poll_interval_seconds,"observation_window_seconds":policy.observation_window_seconds})),
                Err(_) => unavailable_policies.push(json!({"name":name,"reason":"CI observer executable is unavailable; persisted tracking history remains readable"})),
            }
        }
        Ok(
            json!({"eligible":!policies.is_empty() && lane_diagnostic.is_none(),"reason":lane_diagnostic.as_ref().map(|d|d.message.as_str()).or_else(||policies.is_empty().then_some("no currently available configured CI policy matches this immutable publication target")),"publication":source,"policies":policies,"unavailable_policies":unavailable_policies,"lane_diagnostic":lane_diagnostic,"tracks":tracks,"remote_merge_eligibility":"not_established"}),
        )
    }

    pub fn ci_start(&self, id: i64, input: CiStartRequest) -> Result<CiTrack> {
        if input.key.is_empty()
            || input.key.len() > 128
            || input.policy.is_empty()
            || input.policy.len() > 128
            || input.policy_digest.len() != 64
        {
            return Err(Error::Invalid(
                "CI start requires bounded key, policy and exact preview digest".into(),
            ));
        }
        let mut state = self.state.lock().map_err(|_| Error::Poisoned)?;
        let task = state.store.get(id)?;
        let tx = state
            .control
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = tx
            .query_row(
                "SELECT id FROM app_ci_tracks WHERE key=?1",
                [&input.key],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
        {
            let old = read(&tx, existing)?;
            if old.view.publication_task_id != id
                || old.view.policy != input.policy
                || old.view.policy_digest != input.policy_digest
            {
                return Err(unavailable(
                    "ci_idempotency_conflict",
                    "CI key was already accepted for a different immutable scope",
                ));
            }
            return Ok(old.view);
        }
        if tx
            .query_row(
                "SELECT id FROM app_ci_tracks WHERE task_id=?1 AND policy=?2",
                params![id, input.policy],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .is_some()
        {
            return Err(unavailable(
                "ci_scope_reserved",
                "this publication/policy already has a tracker; read or explicitly resume it",
            ));
        }
        let source = publication(&task)?;
        let policy = self
            .host
            .config()
            .ci_policies
            .get(&input.policy)
            .ok_or_else(|| Error::Invalid("CI policy is not configured".into()))?;
        let digest = policy.digest(&self.host.config().workspace_root)?;
        if digest != input.policy_digest
            || policy.github_repository != source.repository
            || policy.base_branch != source.base_branch
        {
            return Err(unavailable(
                "ci_policy_changed",
                "CI preview policy changed or does not match the immutable publication target",
            ));
        }
        self.ci_prepare_storage(&tx)?;
        let created = now()?;
        let mut record = Record {
            view: CiTrack {
                id: 0,
                publication_task_id: id,
                policy: input.policy.clone(),
                policy_digest: digest,
                status: "watching".into(),
                attempt: 0,
                revision: 1,
                stop_requested: false,
                repository: source.repository,
                base_branch: source.base_branch,
                head_sha: source.head_sha,
                head_branch: source.head_branch,
                pr_number: source.pr_number,
                pr_url: source.pr_url,
                workflow_id: policy.workflow_id,
                app_id: policy.app_id,
                event: policy.event.clone(),
                required_jobs: policy.required_jobs.clone(),
                poll_interval_seconds: policy.poll_interval_seconds,
                observation_window_seconds: policy.observation_window_seconds,
                created_at: created,
                window_generation: 1,
                window_started_at: created,
                last_observed_at: None,
                deadline: created + policy.observation_window_seconds,
                next_poll_at: created,
                observed_repository_id: None,
                observed_pr_id: None,
                observed_base_sha: None,
                latest_evidence: None,
                diagnostic: None,
                remote_merge_eligibility: "not_established".into(),
            },
            key: input.key.clone(),
            publication_result_sha256: crate::publication::digest(
                task.result.as_deref().unwrap_or("").as_bytes(),
            ),
            active: false,
            transient_failures: 0,
            last_resume_revision: None,
        };
        tx.execute("INSERT INTO app_ci_tracks(key,task_id,policy,status,revision,attempt,active,next_poll_at,record) VALUES(?1,?2,?3,'watching',1,0,0,?4,'{}')",params![input.key,id,input.policy,created])?;
        record.view.id = tx.last_insert_rowid();
        write(&tx, &record, "start")?;
        tx.commit()?;
        Ok(record.view)
    }
    pub fn ci_stop(&self, id: i64, input: CiControlRequest) -> Result<CiTrack> {
        let mut state = self.state.lock().map_err(|_| Error::Poisoned)?;
        let tx = state
            .control
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut record = read(&tx, id)?;
        if record.view.revision != input.expected_revision {
            return Err(unavailable(
                "ci_stale_revision",
                "CI tracker changed; refresh before stopping",
            ));
        }
        if record.view.status == "process_unknown" {
            return Err(unavailable(
                "ci_recovery_required",
                "unknown CI process cleanup requires local operator confirmation",
            ));
        }
        if record.view.status == "stopped" || record.view.stop_requested {
            return Ok(record.view);
        }
        if matches!(
            record.view.status.as_str(),
            "configured_checks_passed" | "expired" | "pr_closed" | "pr_merged"
        ) {
            return Err(unavailable(
                "ci_terminal",
                "CI observation is already terminal",
            ));
        }
        record.view.revision += 1;
        record.view.stop_requested = true;
        if !record.active {
            diagnostic(
                &mut record,
                "stopped",
                "user_stopped",
                "CI observation explicitly stopped",
            );
        }
        write(&tx, &record, "stop_requested")?;
        tx.commit()?;
        if let Some((track, attempt, flag)) = self
            .ci_running
            .lock()
            .map_err(|_| Error::Poisoned)?
            .as_ref()
            && *track == id
            && *attempt == record.view.attempt
        {
            flag.store(true, Ordering::Release);
        }
        Ok(record.view)
    }
    pub fn ci_resume(&self, id: i64, input: CiControlRequest) -> Result<CiTrack> {
        let mut state = self.state.lock().map_err(|_| Error::Poisoned)?;
        let tx = state
            .control
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut record = read(&tx, id)?;
        if record.last_resume_revision == Some(input.expected_revision) {
            return Ok(record.view);
        }
        if record.view.revision != input.expected_revision {
            return Err(unavailable(
                "ci_stale_revision",
                "CI tracker changed; refresh before resuming",
            ));
        }
        if record.active || record.view.status == "process_unknown" || self.ci_guard_path().exists()
        {
            return Err(unavailable(
                "ci_recovery_required",
                "CI process cleanup is active or unknown; do not resume until it is reconciled",
            ));
        }
        if !matches!(
            record.view.status.as_str(),
            "checks_failed" | "blocked" | "stopped" | "expired"
        ) {
            return Err(unavailable(
                "ci_not_resumable",
                "this CI state cannot be resumed",
            ));
        }
        self.ci_policy(&record)?;
        self.ci_validate_storage(&tx)?;
        record.last_resume_revision = Some(input.expected_revision);
        record.view.window_generation += 1;
        record.view.window_started_at = now()?;
        record.view.deadline =
            record.view.window_started_at + record.view.observation_window_seconds;
        record.view.revision += 1;
        record.view.status = "watching".into();
        record.view.stop_requested = false;
        record.view.next_poll_at = now()?;
        record.view.diagnostic = None;
        record.transient_failures = 0;
        write(&tx, &record, "resume")?;
        tx.commit()?;
        Ok(record.view)
    }
    fn ci_policy(&self, record: &Record) -> Result<&CiPolicy> {
        let policy = self
            .host
            .config()
            .ci_policies
            .get(&record.view.policy)
            .ok_or_else(|| {
                unavailable(
                    "ci_policy_changed",
                    "the admitted CI policy is no longer configured",
                )
            })?;
        if policy.digest(&self.host.config().workspace_root)? != record.view.policy_digest {
            return Err(unavailable(
                "ci_policy_changed",
                "the CI policy or observer executable changed; restore the admitted policy before resuming",
            ));
        }
        Ok(policy)
    }
    fn ci_storage(&self) -> PathBuf {
        self.ci_storage_root.clone()
    }
    fn ci_prepare_storage(&self, db: &Connection) -> Result<()> {
        if bound_storage(db)?.is_some() {
            return self.ci_validate_storage(db);
        }
        let root = self.host.config().workspace_root.join(CONTROL_DIRECTORY);
        match fs::DirBuilder::new().mode(0o700).create(&root) {
            Ok(()) => {
                let mut marker = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(root.join(CONTROL_MARKER))
                    .map_err(invalid_io)?;
                marker
                    .write_all(CONTROL_VERSION)
                    .and_then(|_| marker.sync_all())
                    .and_then(|_| File::open(&root)?.sync_all())
                    .and_then(|_| sync_parent(&root))
                    .map_err(invalid_io)?;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(invalid_io(e)),
        }
        if !is_control_directory(&root) {
            return Err(Error::Invalid(
                "CI control namespace is not the validated private host-owned directory".into(),
            ));
        }
        let storage = self.ci_storage();
        match fs::DirBuilder::new().mode(0o700).create(&storage) {
            Ok(()) => sync_parent(&storage).map_err(invalid_io)?,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(invalid_io(e)),
        }
        if !private_directory(&storage) {
            return Err(Error::Invalid(
                "CI database namespace is not a private host-owned directory".into(),
            ));
        }
        let meta = fs::symlink_metadata(&storage).map_err(invalid_io)?;
        db.execute(
            "INSERT INTO app_ci_storage(id,root,device,inode) VALUES(1,?1,?2,?3)",
            params![
                storage
                    .to_str()
                    .ok_or_else(|| Error::Invalid("CI storage path must be UTF-8".into()))?,
                meta.dev().to_string(),
                meta.ino().to_string()
            ],
        )?;
        Ok(())
    }
    fn ci_validate_storage(&self, db: &Connection) -> Result<()> {
        let Some((bound, device, inode)) = bound_storage(db)? else {
            return Err(Error::Invalid(
                "CI durable control-root identity is missing; inspect local state before recovery"
                    .into(),
            ));
        };
        let expected = self
            .host
            .config()
            .workspace_root
            .join(CONTROL_DIRECTORY)
            .join(&self.ci_database_identity);
        if bound != self.ci_storage_root || expected != bound {
            return Err(unavailable(
                "ci_storage_root_changed",
                "CI control-root configuration differs from its durable binding; restore the original workspace_root before observing or confirming stopped",
            ));
        }
        let parent = bound
            .parent()
            .ok_or_else(|| Error::Invalid("invalid bound CI root".into()))?;
        let metadata = fs::symlink_metadata(&bound).map_err(invalid_io)?;
        if !is_control_directory(parent)
            || !private_directory(&bound)
            || metadata.dev().to_string() != device
            || metadata.ino().to_string() != inode
        {
            return Err(unavailable(
                "ci_storage_root_changed",
                "CI control-root identity is missing, redirected or replaced; preserve the original lease and inspect local state",
            ));
        }
        Ok(())
    }
    fn ci_lane_diagnostic(&self, db: &Connection) -> Result<Option<CiDiagnostic>> {
        if bound_storage(db)?.is_none() {
            return Ok(None);
        }
        if self.ci_validate_storage(db).is_err() {
            return Ok(Some(CiDiagnostic { code: "ci_storage_root_changed".into(), message: "CI control storage differs from its durable host binding; restore the original workspace_root and private control-directory identity before observation or local recovery".into() }));
        }
        let invalid_guard = match open_marker(&self.ci_guard_path()) {
            Ok(None) => false,
            Ok(Some(file)) => {
                marker_contents(&file).map_or(true, |m| m.database != self.ci_database_identity)
            }
            Err(_) => true,
        };
        Ok(invalid_guard.then(|| CiDiagnostic { code: "ci_guard_invalid".into(), message: "A retained CI process guard has an incomplete or mismatched identity; preserve it and inspect the host before recovery".into() }))
    }
    fn ci_guard_path(&self) -> PathBuf {
        self.ci_storage().join(GUARD)
    }
    fn ci_workspace(&self, id: i64, attempt: u64) -> PathBuf {
        self.ci_storage().join(format!("attempt-{id}-{attempt}"))
    }
}
fn ci_for_task(db: &Connection, id: i64) -> Result<Vec<CiTrack>> {
    let mut query =
        db.prepare("SELECT record FROM app_ci_tracks WHERE task_id=?1 ORDER BY id DESC LIMIT 100")?;
    query
        .query_map([id], |r| r.get::<_, String>(0))?
        .map(|row| {
            serde_json::from_str::<Record>(&row?)
                .map(|r| r.view)
                .map_err(|_| Error::Invalid("CI record is invalid".into()))
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryEvidence {
    id: u64,
    full_name: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PrEvidence {
    id: u64,
    number: u64,
    url: String,
    state: String,
    merged: bool,
    draft: bool,
    head_sha: String,
    head_ref: String,
    head_repository_id: u64,
    base_ref: String,
    base_sha: String,
    base_repository_id: u64,
    mergeable: Option<bool>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JobEvidence {
    id: u64,
    check_run_id: u64,
    name: String,
    head_sha: String,
    check_suite_id: u64,
    app_id: u64,
    status: String,
    conclusion: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunEvidence {
    id: u64,
    run_number: u64,
    run_attempt: u64,
    workflow_id: u64,
    event: String,
    head_sha: String,
    head_repository_id: u64,
    check_suite_id: u64,
    app_id: u64,
    status: String,
    conclusion: Option<String>,
    pull_request_ids: Vec<u64>,
    jobs: Vec<JobEvidence>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    version: u32,
    observation: String,
    complete: bool,
    repository: Option<RepositoryEvidence>,
    pull_request: Option<PrEvidence>,
    run: Option<RunEvidence>,
    error_code: Option<String>,
    detail: Option<String>,
    remote_merge_eligibility: String,
}
fn input(record: &Record) -> String {
    let v = &record.view;
    json!({"version":1,"repository":v.repository,"pr_number":v.pr_number,"pr_url":v.pr_url,"head_sha":v.head_sha,"head_branch":v.head_branch,"base_branch":v.base_branch,"workflow_id":v.workflow_id,"app_id":v.app_id,"event":v.event,"required_jobs":v.required_jobs,"observed_repository_id":v.observed_repository_id,"observed_pr_id":v.observed_pr_id,"observed_base_sha":v.observed_base_sha}).to_string()
}
fn state_valid(status: &str, conclusion: Option<&str>) -> bool {
    match status {
        "completed" => conclusion.is_some_and(|c| {
            matches!(
                c,
                "success"
                    | "failure"
                    | "neutral"
                    | "cancelled"
                    | "skipped"
                    | "timed_out"
                    | "action_required"
                    | "stale"
                    | "startup_failure"
            )
        }),
        "queued" | "in_progress" | "waiting" | "pending" | "requested" => conclusion.is_none(),
        _ => false,
    }
}
fn evaluate(record: &mut Record, command: &crate::host::CommandResult, at: u64) {
    if command.outcome == Outcome::Unknown {
        diagnostic(
            record,
            "process_unknown",
            "ci_cleanup_unknown",
            "Observer process-tree cleanup is unconfirmed; inspect the exact track/attempt and use local confirm-ci-stopped",
        );
        return;
    }
    if record.view.stop_requested || command.outcome == Outcome::Cancelled {
        diagnostic(
            record,
            "stopped",
            "user_stopped",
            "CI observation stopped after confirmed observer process-tree cleanup",
        );
        return;
    }
    if at >= record.view.deadline {
        diagnostic(
            record,
            "expired",
            "ci_window_ended",
            "The original fixed CI observation window has ended",
        );
        return;
    }
    if command.outcome == Outcome::TimedOut {
        retry_transient(
            record,
            at,
            "observer_timeout",
            "The bounded read-only observer timed out after confirmed process-tree cleanup",
        );
        return;
    }
    if command.outcome != Outcome::Success
        || command.stdout_truncated
        || command.stderr_truncated
        || command.stdout.len() > MAX_EVIDENCE
    {
        diagnostic(
            record,
            "blocked",
            "ci_observer_failed",
            "Observer failed or exceeded bounded output; no check result was accepted",
        );
        return;
    }
    let observation = match serde_json::from_str::<Observation>(command.stdout.trim()) {
        Ok(o)
            if o.version == 1
                && o.remote_merge_eligibility == "not_established"
                && o.error_code.as_ref().is_none_or(|s| {
                    s.len() <= 64
                        && !s.is_empty()
                        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                })
                && o.detail
                    .as_ref()
                    .is_none_or(|s| s.len() <= 512 && !s.chars().any(char::is_control)) =>
        {
            o
        }
        _ => {
            diagnostic(
                record,
                "blocked",
                "ci_protocol_invalid",
                "Observer did not return the complete bounded version-1 CI protocol",
            );
            return;
        }
    };
    if observation.observation == "transient_error"
        && !observation.complete
        && observation.error_code.as_deref().is_some_and(|code| {
            matches!(
                code,
                "network_error"
                    | "rate_limited"
                    | "service_unavailable"
                    | "request_timeout"
                    | "source_changed"
            )
        })
    {
        retry_transient(
            record,
            at,
            observation
                .error_code
                .as_deref()
                .unwrap_or("transient_error"),
            observation
                .detail
                .as_deref()
                .unwrap_or("Read-only request temporarily unavailable"),
        );
        return;
    }
    if observation.observation == "blocked" && !observation.complete {
        diagnostic(
            record,
            "blocked",
            observation
                .error_code
                .as_deref()
                .unwrap_or("ci_observer_blocked"),
            observation.detail.as_deref().unwrap_or(
                "Observer reported an actionable policy, authentication or source error",
            ),
        );
        return;
    }
    if observation.observation != "ok"
        || !observation.complete
        || observation.error_code.is_some()
        || observation.detail.is_some()
    {
        diagnostic(
            record,
            "blocked",
            "ci_protocol_invalid",
            "Observer completion/source evidence is missing or contradictory",
        );
        return;
    }
    let (Some(repository), Some(pr)) = (&observation.repository, &observation.pull_request) else {
        diagnostic(
            record,
            "blocked",
            "ci_source_invalid",
            "Repository and PR source identities are required",
        );
        return;
    };
    let v = &record.view;
    if repository.id == 0
        || pr.id == 0
        || repository.full_name != v.repository
        || pr.number != v.pr_number
        || pr.url != v.pr_url
        || pr.base_repository_id != repository.id
        || pr.head_repository_id != repository.id
        || !matches!(pr.state.as_str(), "open" | "closed")
        || (pr.merged && pr.state != "closed")
        || !sha(&pr.head_sha)
        || !sha(&pr.base_sha)
        || v.observed_repository_id
            .is_some_and(|id| id != repository.id)
        || v.observed_pr_id.is_some_and(|id| id != pr.id)
    {
        diagnostic(
            record,
            "blocked",
            "ci_source_drift",
            "The observed numeric repository/PR identity or exact published repository does not match",
        );
        return;
    }
    if pr.head_sha != v.head_sha
        || pr.head_ref != v.head_branch
        || pr.base_ref != v.base_branch
        || v.observed_base_sha
            .as_ref()
            .is_some_and(|sha| sha != &pr.base_sha)
    {
        diagnostic(
            record,
            "blocked",
            "ci_head_or_base_drift",
            "The published HEAD, head/base branch or first-observed base SHA changed; this tracker will not follow it",
        );
        return;
    }
    let mut missing = record.view.required_jobs.clone();
    let mut failed = Vec::<String>::new();
    let mut pending = false;
    let mut pending_jobs = Vec::<String>::new();
    if let Some(run) = &observation.run {
        if run.id == 0
            || run.run_number == 0
            || run.run_attempt == 0
            || run.workflow_id != v.workflow_id
            || run.event != v.event
            || run.head_sha != v.head_sha
            || run.head_repository_id != repository.id
            || run.check_suite_id == 0
            || run.app_id != v.app_id
            || !state_valid(&run.status, run.conclusion.as_deref())
            || run.pull_request_ids != [pr.id]
            || run.jobs.len() > 64
        {
            diagnostic(
                record,
                "blocked",
                "ci_source_invalid",
                "Actions run source, event, workflow, app, check suite, HEAD or PR association is invalid",
            );
            return;
        }
        pending = run.status != "completed";
        let mut ids = BTreeSet::new();
        let mut check_ids = BTreeSet::new();
        let mut names = BTreeSet::new();
        for job in &run.jobs {
            if job.id == 0
                || job.check_run_id == 0
                || !ids.insert(job.id)
                || !check_ids.insert(job.check_run_id)
                || job.name.is_empty()
                || job.name.len() > 256
                || job.name.chars().any(char::is_control)
                || job.head_sha != v.head_sha
                || job.check_suite_id != run.check_suite_id
                || job.app_id != v.app_id
                || !state_valid(&job.status, job.conclusion.as_deref())
            {
                diagnostic(
                    record,
                    "blocked",
                    "ci_source_invalid",
                    "Job/check-run IDs, name, source app, suite, HEAD or completion state is invalid",
                );
                return;
            }
            if v.required_jobs.contains(&job.name) {
                if !names.insert(job.name.clone()) {
                    diagnostic(
                        record,
                        "blocked",
                        "ci_ambiguous_job",
                        "More than one check has an exact required job name in the selected run attempt",
                    );
                    return;
                }
                missing.retain(|name| name != &job.name);
                if job.status != "completed" {
                    pending = true;
                    pending_jobs.push(job.name.clone());
                } else if job.conclusion.as_deref() != Some("success") {
                    failed.push(job.name.clone());
                }
            }
        }
    }
    record.view.observed_repository_id = Some(repository.id);
    record.view.observed_pr_id = Some(pr.id);
    record.view.observed_base_sha = Some(pr.base_sha.clone());
    let mut evidence = serde_json::to_value(&observation).expect("serializable evidence");
    evidence["observed_at"] = json!(at);
    evidence["missing_jobs"] = json!(missing);
    evidence["failed_jobs"] = json!(failed);
    evidence["pending_jobs"] = json!(pending_jobs);
    if evidence.to_string().len() > MAX_EVIDENCE {
        diagnostic(
            record,
            "blocked",
            "ci_evidence_too_large",
            "Normalized CI evidence exceeds 64 KiB",
        );
        return;
    }
    record.view.latest_evidence = Some(evidence);
    record.view.last_observed_at = Some(at);
    record.transient_failures = 0;
    record.view.diagnostic = None;
    if pr.merged {
        record.view.status = "pr_merged".into();
    } else if pr.state == "closed" {
        record.view.status = "pr_closed".into();
    } else if !failed.is_empty() {
        diagnostic(
            record,
            "checks_failed",
            "configured_checks_failed",
            "At least one configured exact-source job did not conclude success; explicit resume is required to observe a rerun",
        );
    } else if missing.is_empty() && !pending {
        record.view.status = "configured_checks_passed".into();
    } else {
        record.view.status = "watching".into();
        record.view.next_poll_at =
            (at + record.view.poll_interval_seconds).min(record.view.deadline);
    }
}
fn retry_transient(record: &mut Record, at: u64, code: &str, message: &str) {
    record.transient_failures += 1;
    if record.transient_failures > 3 {
        diagnostic(
            record,
            "blocked",
            "ci_retry_exhausted",
            "Three bounded transient read-only retries were exhausted; explicit resume is required",
        );
        return;
    }
    diagnostic(record, "watching", code, message);
    record.view.next_poll_at = (at
        + record.view.poll_interval_seconds * (1_u64 << record.transient_failures.min(3)))
    .min(record.view.deadline);
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    database: String,
    track_id: i64,
    attempt: u64,
}
fn flock(file: &File) -> io::Result<()> {
    // SAFETY: operates only on an owned fd and never signals a process.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
fn matches_file(path: &Path, file: &File) -> bool {
    fs::symlink_metadata(path)
        .ok()
        .zip(file.metadata().ok())
        .is_some_and(|(p, f)| p.is_file() && p.dev() == f.dev() && p.ino() == f.ino())
}
fn sync_parent(path: &Path) -> io::Result<()> {
    File::open(
        path.parent()
            .ok_or_else(|| io::Error::other("missing CI guard parent"))?,
    )?
    .sync_all()
}
fn create_marker(path: &Path, marker: &Marker) -> Result<Arc<File>> {
    // Stage a complete locked marker before publishing it atomically. A crash
    // after create_new but before fsync can leave only an inert staging file,
    // never an empty canonical guard whose exact attempt cannot be recovered.
    let staging = path.with_file_name(format!(
        ".ci-guard-stage-{:016x}",
        rand_core::OsRng.next_u64()
    ));
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&staging)
        .map_err(invalid_io)?;
    let prepared = (|| -> Result<()> {
        flock(&file).map_err(invalid_io)?;
        serde_json::to_writer(&mut file, marker)
            .map_err(|_| Error::Invalid("cannot encode CI guard".into()))?;
        file.write_all(b"\n")
            .and_then(|_| file.sync_all())
            .map_err(invalid_io)?;
        // hard_link is atomic no-replace; canonical guard shares the held flock.
        fs::hard_link(&staging, path)
            .and_then(|_| sync_parent(path))
            .map_err(invalid_io)?;
        if !matches_file(path, &file) {
            return Err(Error::Invalid("CI guard identity changed".into()));
        }
        Ok(())
    })();
    let _ = fs::remove_file(&staging);
    prepared?;
    sync_parent(path).map_err(invalid_io)?;
    Ok(Arc::new(file))
}
fn open_marker(path: &Path) -> Result<Option<File>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(invalid_io(e)),
    };
    if !file.metadata().map_err(invalid_io)?.is_file() || !matches_file(path, &file) {
        return Err(Error::Invalid(
            "CI guard is not the exact regular file".into(),
        ));
    }
    Ok(Some(file))
}
fn marker_contents(file: &File) -> Result<Marker> {
    let mut raw = String::new();
    file.take(2049)
        .read_to_string(&mut raw)
        .map_err(invalid_io)?;
    if raw.len() > 2048 {
        return Err(Error::Invalid("CI guard exceeds its bound".into()));
    }
    serde_json::from_str(&raw).map_err(|_| {
        Error::Invalid("CI guard identity is incomplete; inspect host state before recovery".into())
    })
}
impl Application {
    /// One CI attempt, without holding the app DB mutex or claiming the development queue.
    pub fn ci_work_once(&self) -> Result<bool> {
        let (record, guard, cancel) = {
            let mut state = self.state.lock().map_err(|_| Error::Poisoned)?;
            if self.shutdown.load(Ordering::Acquire) {
                return Ok(false);
            }
            let tx = state
                .control
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let path = self.ci_guard_path();
            if bound_storage(&tx)?.is_some() {
                self.ci_validate_storage(&tx)?;
            }
            if let Some(file) = open_marker(&path)? {
                match flock(&file) {
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                    Err(e) => return Err(invalid_io(e)),
                    Ok(()) => {
                        let marker = marker_contents(&file)?;
                        if marker.database == self.ci_database_identity {
                            let mut r = read(&tx, marker.track_id)?;
                            if r.view.status != "process_unknown" {
                                r.view.revision += 1;
                                diagnostic(
                                    &mut r,
                                    "process_unknown",
                                    "ci_retained_guard",
                                    "A retained CI process guard requires exact track/attempt stopped reconciliation; elapsed time is not cleanup proof",
                                );
                                write(&tx, &r, "retained_guard")?;
                                tx.commit()?;
                            }
                        }
                        return Ok(false);
                    }
                }
            }
            if let Some(id) = tx
                .query_row("SELECT id FROM app_ci_tracks WHERE active=1", [], |r| {
                    r.get::<_, i64>(0)
                })
                .optional()?
            {
                let mut r = read(&tx, id)?;
                if r.view.status != "process_unknown" {
                    r.view.revision += 1;
                    diagnostic(
                        &mut r,
                        "process_unknown",
                        "ci_missing_guard",
                        "An active durable CI attempt has no process guard; local stopped reconciliation is required",
                    );
                    write(&tx, &r, "missing_guard")?;
                    tx.commit()?;
                }
                return Ok(false);
            }
            let Some(id)=tx.query_row("SELECT id FROM app_ci_tracks WHERE status='watching' AND active=0 AND next_poll_at<=?1 ORDER BY next_poll_at,id LIMIT 1",[now()?],|r|r.get::<_,i64>(0)).optional()? else{return Ok(false);};
            let mut r = read(&tx, id)?;
            if now()? >= r.view.deadline {
                r.view.revision += 1;
                diagnostic(
                    &mut r,
                    "expired",
                    "ci_window_ended",
                    "The fixed CI observation window has ended",
                );
                write(&tx, &r, "expire")?;
                tx.commit()?;
                return Ok(true);
            }
            if self.ci_policy(&r).is_err() {
                r.view.revision += 1;
                diagnostic(
                    &mut r,
                    "blocked",
                    "ci_policy_changed",
                    "The admitted CI configuration or observer executable changed",
                );
                write(&tx, &r, "policy_changed")?;
                tx.commit()?;
                return Ok(true);
            }
            r.view.attempt += 1;
            r.view.revision += 1;
            r.active = true;
            let guard = create_marker(
                &path,
                &Marker {
                    database: self.ci_database_identity.clone(),
                    track_id: id,
                    attempt: r.view.attempt,
                },
            )?;
            write(&tx, &r, "attempt_started")?;
            tx.commit()?;
            let cancel = Arc::new(AtomicBool::new(false));
            *self.ci_running.lock().map_err(|_| Error::Poisoned)? =
                Some((id, r.view.attempt, Arc::clone(&cancel)));
            (r, guard, cancel)
        };
        let workspace = self.ci_workspace(record.view.id, record.view.attempt);
        let response = match fs::DirBuilder::new().mode(0o700).create(&workspace) {
            Err(_) => crate::host::CommandResult::error(
                Outcome::Unknown,
                "Cannot create the exact private CI attempt directory; retained guard requires inspection",
            ),
            Ok(()) => {
                let finished = AtomicBool::new(false);
                std::thread::scope(|scope| {
                    scope.spawn(|| {
                        while !finished.load(Ordering::Acquire) {
                            let stop = self.shutdown.load(Ordering::Acquire)
                                || self
                                    .state
                                    .lock()
                                    .ok()
                                    .and_then(|s| read(&s.control, record.view.id).ok())
                                    .is_none_or(|r| {
                                        r.view.attempt != record.view.attempt
                                            || r.view.revision != record.view.revision
                                            || r.view.stop_requested
                                            || !r.active
                                    });
                            if stop {
                                cancel.store(true, Ordering::Release);
                            }
                            std::thread::sleep(Duration::from_millis(50));
                        }
                    });
                    let remaining = record
                        .view
                        .deadline
                        .saturating_sub(now().unwrap_or(record.view.deadline));
                    let response = if remaining == 0 {
                        crate::host::CommandResult::error(
                            Outcome::TimedOut,
                            "CI window ended before observer startup; no process was started",
                        )
                    } else {
                        match self.ci_policy(&record) {
                            Ok(policy) => self.host.run_ci_observer(
                                &policy.observer,
                                &input(&record),
                                &workspace,
                                Arc::clone(&guard),
                                &cancel,
                                REQUEST_TIMEOUT_SECONDS.min(remaining),
                            ),
                            Err(_) => crate::host::CommandResult::error(
                                Outcome::Failure,
                                "CI policy changed before observer execution",
                            ),
                        }
                    };
                    finished.store(true, Ordering::Release);
                    response
                })
            }
        };
        if let Ok(mut running) = self.ci_running.lock()
            && running.as_ref().is_some_and(|(id, attempt, _)| {
                *id == record.view.id && *attempt == record.view.attempt
            })
        {
            *running = None;
        }
        let unknown = response.outcome == Outcome::Unknown;
        {
            let mut state = self.state.lock().map_err(|_| Error::Poisoned)?;
            let tx = state
                .control
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut current = read(&tx, record.view.id)?;
            if current.view.attempt != record.view.attempt
                || !current.active
                || (current.view.revision != record.view.revision
                    && !(current.view.stop_requested
                        && current.view.revision == record.view.revision + 1))
            {
                return Err(unavailable(
                    "ci_stale_attempt",
                    "Late CI result was fenced; guard retained for exact-attempt reconciliation",
                ));
            }
            evaluate(&mut current, &response, now()?);
            current.active = unknown;
            current.view.revision += 1;
            write(
                &tx,
                &current,
                if unknown {
                    "process_unknown"
                } else {
                    "attempt_finished"
                },
            )?;
            tx.commit()?;
        }
        if unknown {
            return Ok(true);
        }
        // Only a known-cleaned process tree AND fenced durable result permit cleanup.
        let path = self.ci_guard_path();
        let cleanup = (|| -> io::Result<()> {
            if !matches_file(&path, &guard) {
                return Err(io::Error::other("CI guard identity changed"));
            }
            fs::remove_dir_all(&workspace)?;
            fs::remove_file(&path)?;
            sync_parent(&path)
        })();
        if cleanup.is_err() {
            let mut state = self.state.lock().map_err(|_| Error::Poisoned)?;
            let tx = state
                .control
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut current = read(&tx, record.view.id)?;
            current.view.revision += 1;
            current.active = true;
            diagnostic(
                &mut current,
                "process_unknown",
                "ci_local_cleanup_failed",
                "Observer stopped, but durable CI guard/temp cleanup was not completed; local reconciliation is required",
            );
            write(&tx, &current, "cleanup_failed")?;
            tx.commit()?;
        }
        Ok(true)
    }
    pub fn ci_worker(&self) {
        while !self.shutdown.load(Ordering::Acquire) {
            match self.ci_work_once() {
                Ok(true) => (),
                Ok(false) => std::thread::sleep(Duration::from_millis(200)),
                Err(error) => {
                    eprintln!("CI worker: {error}");
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
    }
    /// Local-only operator attestation; a live inherited supervisor flock always wins.
    pub fn confirm_ci_stopped(&self, id: i64, attempt: u64, attest: bool) -> Result<CiTrack> {
        if !attest || id <= 0 || attempt == 0 {
            return Err(Error::Invalid(
                "exact CI track/attempt and explicit process-tree-stopped attestation are required"
                    .into(),
            ));
        }
        let mut state = self.state.lock().map_err(|_| Error::Poisoned)?;
        let tx = state
            .control
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut record = read(&tx, id)?;
        self.ci_validate_storage(&tx)?;
        let path = self.ci_guard_path();
        let marker_file = open_marker(&path)?;
        if let Some(file) = &marker_file {
            flock(file).map_err(|_|unavailable("ci_process_live","CI guard is still locked by a live observer/supervisor; stopped confirmation refused"))?;
            let marker = marker_contents(file)?;
            if marker.database != self.ci_database_identity
                || marker.track_id != id
                || marker.attempt != attempt
                || !matches_file(&path, file)
            {
                return Err(unavailable(
                    "ci_recovery_identity_mismatch",
                    "local confirmation does not match the exact retained CI database/track/attempt guard",
                ));
            }
            // A create_new+fsync guard may survive a rolled-back reservation, but
            // no subprocess was allowed to start before that reservation committed.
            if attempt != record.view.attempt
                && !(attempt == record.view.attempt + 1 && !record.active)
            {
                return Err(unavailable(
                    "ci_recovery_identity_mismatch",
                    "guard attempt does not match durable or precommit reservation identity",
                ));
            }
        } else if attempt != record.view.attempt || !record.active {
            return Err(unavailable(
                "ci_recovery_identity_mismatch",
                "no retained CI process guard or matching active attempt exists",
            ));
        }
        record.view.attempt = attempt;
        record.view.revision += 1;
        record.active = false;
        record.view.stop_requested = true;
        diagnostic(
            &mut record,
            "stopped",
            "ci_operator_confirmed_stopped",
            "Operator explicitly attested this exact observer process tree stopped; previous diagnostics remain in durable audit; resume is a separate action",
        );
        write(&tx, &record, "operator_confirmed_stopped")?;
        tx.commit()?;
        // Retain temp diagnostics for manual inspection. It is bounded per failed
        // attempt and never protects or accesses the original task workspace.
        if let Some(file) = marker_file {
            if !matches_file(&path, &file) {
                return Err(Error::Invalid(
                    "CI guard changed during stopped confirmation".into(),
                ));
            }
            fs::remove_file(&path)
                .and_then(|_| sync_parent(&path))
                .map_err(invalid_io)?;
        }
        Ok(record.view)
    }
}
