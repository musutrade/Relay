//! Default-off, per-target authorized async merge. Application-owned state only:
//! waiting never claims the development queue and local reads never access GitHub.
use crate::{
    Application, Error, Result, action_unavailable,
    ci_tracking::{self, CiDiagnostic, CiTrack},
    host::{CommandProfile, CommandResult, HostConfig, Outcome},
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const GUARD: &str = ".merge-authorization-in-flight";
const MAX_EVIDENCE: usize = 64 * 1024;
const MAX_RECORD: usize = 128 * 1024;
const MAX_ATTEMPTS: u64 = 4096;
const RISK_VERSION: u32 = 1;
const RISK_TEXT: &str = "Authorize only this published PR and exact HEAD, fixed base branch, configured CI source, method and deadline. Optional draft-to-ready may run before CI passes. Target/base/stack/queue/ready guards are preflight-only, not atomic; concurrent changes can occur. GitHub enforces current protections with bypass_rules=false. The deadline is last dispatch, not completion: already-dispatched or accepted GitHub work may finish after expiry or revocation and no cancellation endpoint is documented. The exact HEAD is SHA-conditional. Observed stacks, queues and existing auto-merge are rejected. Accepted is not merged. Relay does not separately delete branches, deploy or change repository settings. Existing repository automation, including branch deletion, may run. A successful CI observation alone is not merge permission.";
fn poll_default() -> u64 {
    60
}
fn window_default() -> u64 {
    3600
}
fn guard_default() -> String {
    "preflight_only".into()
}
fn now() -> Result<u64> {
    crate::publication::now().map_err(invalid_io)
}
fn invalid_io(e: io::Error) -> Error {
    Error::Invalid(format!("merge local state: {e}"))
}
fn unavailable(code: &str, message: &str) -> Error {
    action_unavailable(code, message)
}
fn hash(value: &impl Serialize) -> String {
    crate::publication::digest(&serde_json::to_vec(value).expect("serializable merge binding"))
}
fn sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn node(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_=-".contains(&b))
}
fn method(value: &str) -> bool {
    matches!(value, "merge" | "squash" | "rebase")
}
fn risk() -> Value {
    json!({"version":RISK_VERSION,"sha256":crate::publication::digest(RISK_TEXT.as_bytes()),"text":RISK_TEXT})
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergePolicy {
    pub ci_policy: String,
    pub adapter: CommandProfile,
    pub merge_method: String,
    #[serde(default)]
    pub allow_ready: bool,
    #[serde(default = "guard_default")]
    pub target_guard: String,
    #[serde(default = "window_default")]
    pub authorization_window_seconds: u64,
    #[serde(default = "poll_default")]
    pub poll_interval_seconds: u64,
}
impl MergePolicy {
    pub(crate) fn validate(&self, config: &HostConfig) -> std::result::Result<(), String> {
        if !config.ci_policies.contains_key(&self.ci_policy)
            || !method(&self.merge_method)
            || self.target_guard != "preflight_only"
            || !(60..=86400).contains(&self.authorization_window_seconds)
            || !(60..=3600).contains(&self.poll_interval_seconds)
        {
            return Err("merge policy requires an existing CI policy, merge/squash/rebase method, preflight_only guard, window 60-86400s and poll 60-3600s".into());
        }
        Ok(())
    }
    fn digest(&self, config: &HostConfig) -> Result<String> {
        let m = fs::metadata(&self.adapter.program).map_err(invalid_io)?;
        let source = config
            .ci_policies
            .get(&self.ci_policy)
            .ok_or_else(|| Error::Invalid("merge CI policy missing".into()))?;
        // Private arguments and environment only enter this digest, never a durable record.
        Ok(hash(
            &json!({"policy":self,"ci_policy_digest":source.digest(&config.workspace_root)?,"executable":{"path":self.adapter.program.canonicalize().map_err(invalid_io)?,"device":m.dev(),"inode":m.ino(),"len":m.len(),"mtime":m.mtime(),"mtime_nsec":m.mtime_nsec(),"ctime":m.ctime(),"ctime_nsec":m.ctime_nsec(),"mode":m.mode()},"workspace_root":config.workspace_root}),
        ))
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MergeAuthorizeRequest {
    pub key: String,
    pub policy: String,
    pub policy_digest: String,
    pub ci_track_id: i64,
    pub scope_digest: String,
    pub deadline: u64,
    pub allow_ready: bool,
    pub confirm_merge: bool,
    pub accept_non_atomic_target_guard: bool,
    pub deadline_semantics: String,
    pub accept_existing_automation: bool,
    pub risk_disclosure_version: u32,
    pub risk_disclosure_sha256: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeControlRequest {
    pub expected_revision: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MergeTarget {
    pub repository: String,
    pub repository_id: u64,
    pub pr_number: u64,
    pub pr_id: u64,
    pub pr_node_id: Option<String>,
    pub pr_url: String,
    pub head_sha: String,
    pub head_branch: String,
    pub base_branch: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AsyncOptions {
    pub sha: String,
    pub merge_method: String,
    pub merge_action: String,
    pub bypass_rules: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AsyncRequest {
    pub id: String,
    pub options: AsyncOptions,
    pub provenance: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeAuthorization {
    pub id: i64,
    pub publication_task_id: i64,
    pub ci_track_id: i64,
    pub previous_authorization_id: Option<i64>,
    pub policy: String,
    pub policy_digest: String,
    pub scope_digest: String,
    pub ci_policy: String,
    pub ci_policy_digest: String,
    pub ci_source: Value,
    pub target: MergeTarget,
    pub resolved_pr_node_id: Option<String>,
    pub merge_method: String,
    pub target_guard: String,
    pub allow_ready: bool,
    pub authorization_window_seconds: u64,
    pub poll_interval_seconds: u64,
    pub created_at: u64,
    pub deadline: u64,
    pub consent: Value,
    pub status: String,
    pub revision: u64,
    pub attempt: u64,
    pub revoked: bool,
    pub ready_dispatched: bool,
    pub merge_dispatched: bool,
    pub ready_write_not_started: bool,
    pub merge_write_not_started: bool,
    pub ready_confirmed: bool,
    pub merge_failed_confirmed: bool,
    pub async_request: Option<AsyncRequest>,
    pub merge_commit_sha: Option<String>,
    pub last_observed_at: Option<u64>,
    pub next_poll_at: u64,
    pub latest_evidence: Option<Value>,
    pub diagnostic: Option<CiDiagnostic>,
    pub remote_merge_eligibility: String,
    pub observed_remote_status: Option<String>,
    pub observation_only: bool,
}
#[derive(Clone, Serialize, Deserialize)]
struct Record {
    view: MergeAuthorization,
    request: MergeAuthorizeRequest,
    publication_result_sha256: String,
    ci_binding: CiTrack,
    active: bool,
    phase: String,
    next_operation: Option<String>,
    gate_identity: Option<GateIdentity>,
    completed_response_attempt: Option<u64>,
    readonly_reconciliation: bool,
    transient_failures: u32,
    last_reconcile_revision: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetObservation {
    repository_id: u64,
    pr_id: u64,
    pr_node_id: String,
    head_sha: String,
    head_branch: String,
    base_branch: String,
    base_sha: String,
    draft: bool,
    state: String,
    merged: bool,
    stack_clear: bool,
    queue_clear: bool,
    auto_merge_disabled: bool,
    #[serde(default)]
    delete_branch_on_merge: Option<bool>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdapterResponse {
    version: u32,
    operation: String,
    status: String,
    complete: bool,
    effect: String,
    ci_observation: Option<Value>,
    target: Option<TargetObservation>,
    async_request: Option<AsyncRequest>,
    merge_commit_sha: Option<String>,
    error_code: Option<String>,
    detail: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    database: String,
    authorization_id: i64,
    attempt: u64,
    phase: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    database: String,
    authorization_id: i64,
    attempt: u64,
    phase: String,
    consent_sha256: String,
    finished_at: u64,
    outcome: Outcome,
    gate_identity: Option<GateIdentity>,
    gate_write_started: Option<bool>,
    response: Option<AdapterResponse>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct GateIdentity {
    device: u64,
    inode: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteGate {
    version: u32,
    authorization_id: i64,
    attempt: u64,
    phase: String,
    consent_sha256: String,
    deadline: u64,
    revoked: bool,
    write_started: bool,
}

pub(crate) fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS app_merge_authorizations(id INTEGER PRIMARY KEY AUTOINCREMENT,key TEXT UNIQUE NOT NULL,task_id INTEGER NOT NULL,repository TEXT NOT NULL,pr_number INTEGER NOT NULL,status TEXT NOT NULL,revision INTEGER NOT NULL,attempt INTEGER NOT NULL,active INTEGER NOT NULL DEFAULT 0,next_poll_at INTEGER NOT NULL,record TEXT NOT NULL CHECK(length(CAST(record AS BLOB))<=131072)); CREATE TABLE IF NOT EXISTS app_merge_scopes(repository TEXT NOT NULL,pr_number INTEGER NOT NULL,repository_id INTEGER NOT NULL,pr_id INTEGER NOT NULL,authorization_id INTEGER NOT NULL,PRIMARY KEY(repository_id,pr_id),UNIQUE(repository,pr_number)); CREATE UNIQUE INDEX IF NOT EXISTS app_merge_single_active ON app_merge_authorizations(active) WHERE active=1; CREATE TABLE IF NOT EXISTS app_merge_audit(id INTEGER PRIMARY KEY AUTOINCREMENT,authorization_id INTEGER NOT NULL,revision INTEGER NOT NULL,attempt INTEGER NOT NULL,at INTEGER NOT NULL,event TEXT NOT NULL,status TEXT NOT NULL,diagnostic_code TEXT,evidence_sha256 TEXT,UNIQUE(authorization_id,revision));")?;
    Ok(())
}
fn read(db: &Connection, id: i64) -> Result<Record> {
    let raw: String = db
        .query_row(
            "SELECT record FROM app_merge_authorizations WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| Error::Invalid("merge authorization does not exist".into()))?;
    serde_json::from_str(&raw)
        .map_err(|_| Error::Invalid("merge authorization record invalid".into()))
}
fn records(db: &Connection, id: i64) -> Result<Vec<MergeAuthorization>> {
    let mut q = db.prepare(
        "SELECT record FROM app_merge_authorizations WHERE task_id=?1 ORDER BY id DESC LIMIT 100",
    )?;
    q.query_map([id], |r| r.get::<_, String>(0))?
        .map(|r| {
            serde_json::from_str::<Record>(&r?)
                .map(|r| r.view)
                .map_err(|_| Error::Invalid("merge record invalid".into()))
        })
        .collect()
}
fn write(db: &Connection, r: &Record, event: &str) -> Result<()> {
    let raw = serde_json::to_string(r).expect("serializable authorization");
    if raw.len() > MAX_RECORD {
        return Err(Error::Invalid("merge authorization exceeds 128 KiB".into()));
    }
    db.execute("UPDATE app_merge_authorizations SET status=?2,revision=?3,attempt=?4,active=?5,next_poll_at=?6,record=?7 WHERE id=?1",params![r.view.id,r.view.status,r.view.revision,r.view.attempt,r.active,r.view.next_poll_at,raw])?;
    db.execute("INSERT INTO app_merge_audit(authorization_id,revision,attempt,at,event,status,diagnostic_code,evidence_sha256) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![r.view.id,r.view.revision,r.view.attempt,now()?,event,r.view.status,r.view.diagnostic.as_ref().map(|d|&d.code),r.view.latest_evidence.as_ref().map(hash)])?;
    Ok(())
}
fn diagnostic(r: &mut Record, status: &str, code: &str, message: &str) {
    r.view.status = status.into();
    r.view.diagnostic = Some(CiDiagnostic {
        code: code.into(),
        message: message.into(),
    });
    r.next_operation = None;
}
fn public_source(track: &CiTrack) -> Value {
    json!({"workflow_id":track.workflow_id,"app_id":track.app_id,"event":track.event,"required_jobs":track.required_jobs})
}
fn target(task: &relay::Task, track: &CiTrack) -> Result<MergeTarget> {
    let p = ci_tracking::publication(task)?;
    let (Some(repository_id), Some(pr_id)) = (track.observed_repository_id, track.observed_pr_id)
    else {
        return Err(unavailable(
            "merge_identity_missing",
            "A complete CI observation must first establish the exact numeric repository and PR identity; CI may still be pending",
        ));
    };
    if track.publication_task_id != task.id
        || track.repository != p.repository
        || track.pr_number != p.pr_number
        || track.pr_url != p.pr_url
        || track.head_sha != p.head_sha
        || track.head_branch != p.head_branch
        || track.base_branch != p.base_branch
        || repository_id == 0
        || pr_id == 0
        || matches!(track.status.as_str(), "pr_closed" | "pr_merged")
    {
        return Err(unavailable(
            "merge_source_mismatch",
            "The immutable real publication and observed CI source do not match an open authorized target",
        ));
    }
    Ok(MergeTarget {
        repository: p.repository,
        repository_id,
        pr_number: p.pr_number,
        pr_id,
        pr_node_id: None,
        pr_url: p.pr_url,
        head_sha: p.head_sha,
        head_branch: p.head_branch,
        base_branch: p.base_branch,
    })
}
fn scope(
    target: &MergeTarget,
    policy: &str,
    policy_digest: &str,
    track: &CiTrack,
    previous: Option<i64>,
) -> String {
    hash(
        &json!({"previous_authorization_id":previous,"target":target,"policy":policy,"policy_digest":policy_digest,"ci_track_id":track.id,"ci_policy_digest":track.policy_digest,"source":public_source(track),"risk_disclosure":risk()}),
    )
}
fn clean_ci_binding(track: &CiTrack) -> CiTrack {
    let mut c = track.clone();
    c.latest_evidence = None;
    c.diagnostic = None;
    c
}
impl Application {
    pub fn merge_for_task(&self, id: i64) -> Result<Vec<MergeAuthorization>> {
        records(&self.state.lock().map_err(|_| Error::Poisoned)?.control, id)
    }
    pub fn merge_get(&self, id: i64) -> Result<MergeAuthorization> {
        Ok(read(&self.state.lock().map_err(|_| Error::Poisoned)?.control, id)?.view)
    }
    pub fn merge_preview(&self, id: i64) -> Result<Value> {
        let state = self.state.lock().map_err(|_| Error::Poisoned)?;
        let task = state.store.get(id)?;
        let mut authorizations = records(&state.control, id)?;
        let mut policies = Vec::new();
        let tracks = ci_tracking::ci_for_task(&state.control, id)?;
        let mut reason=Some("No matching configured merge policy and trustworthy numeric CI identity; merge is disabled by default".to_owned());
        let lane_diagnostic = self.merge_lane_diagnostic(&state.control)?;
        for (name, p) in &self.host.config().merge_policies {
            for track in tracks.iter().filter(|t| t.policy == p.ci_policy) {
                let result = (|| -> Result<Value> {
                    let target = target(&task, track)?;
                    let source = self
                        .host
                        .config()
                        .ci_policies
                        .get(&p.ci_policy)
                        .ok_or_else(|| Error::Invalid("CI policy unavailable".into()))?;
                    if source.digest(&self.host.config().workspace_root)? != track.policy_digest {
                        return Err(unavailable(
                            "merge_policy_changed",
                            "The source CI policy binding changed",
                        ));
                    }
                    let digest = p.digest(self.host.config())?;
                    Ok(
                        json!({"name":name,"policy_digest":digest,"scope_digest":scope(&target,name,&digest,track,previous_scope(&state.control,&target.repository,target.pr_number,Some(target.repository_id),Some(target.pr_id))?.map(|r|r.view.id)),"ci_track_id":track.id,"ci_policy":track.policy,"ci_policy_digest":track.policy_digest,"ci_source":public_source(track),"target":target,"merge_method":p.merge_method,"target_guard":p.target_guard,"allow_ready":p.allow_ready,"authorization_window_seconds":p.authorization_window_seconds,"poll_interval_seconds":p.poll_interval_seconds,"deadline":now()?+p.authorization_window_seconds}),
                    )
                })();
                match result {
                    Ok(p) => policies.push(p),
                    Err(e) => reason = Some(e.to_string()),
                }
            }
        }
        let previous = ci_tracking::publication(&task)
            .ok()
            .map(|p| {
                previous_scope(
                    &state.control,
                    &p.repository,
                    p.pr_number,
                    tracks
                        .iter()
                        .find(|t| t.repository == p.repository && t.pr_number == p.pr_number)
                        .and_then(|t| t.observed_repository_id),
                    tracks
                        .iter()
                        .find(|t| t.repository == p.repository && t.pr_number == p.pr_number)
                        .and_then(|t| t.observed_pr_id),
                )
            })
            .transpose()?
            .flatten();
        if let Some(prior) = &previous
            && !authorizations.iter().any(|a| a.id == prior.view.id)
        {
            authorizations.push(prior.view.clone());
        }
        let reserved = previous
            .as_ref()
            .filter(|r| !safe_to_replace(r))
            .map(|r| r.view.id);
        let eligible = !policies.is_empty()
            && lane_diagnostic.is_none()
            && reserved.is_none()
            && !self.merge_guard_path().exists();
        if eligible {
            reason = None;
        }
        if reserved.is_some() {
            reason=Some("This PR already has an immutable authorization; read, revoke or reconcile it. A new key cannot replace it".into());
        }
        if let Some(d) = &lane_diagnostic {
            reason = Some(d.message.clone());
        }
        Ok(
            json!({"eligible":eligible,"reason":reason,"policies":policies,"risk_disclosure":risk(),"authorizations":authorizations,"reserved_authorization_id":reserved,"replacement_authorization_id":previous.as_ref().filter(|r|safe_to_replace(r)).map(|r|r.view.id),"lane_diagnostic":lane_diagnostic,"remote_merge_eligibility":"not_established"}),
        )
    }
    pub fn merge_authorize(
        &self,
        id: i64,
        input: MergeAuthorizeRequest,
    ) -> Result<MergeAuthorization> {
        if input.key.is_empty()
            || input.key.len() > 128
            || input.policy.is_empty()
            || input.policy.len() > 128
            || input.ci_track_id <= 0
            || !sha(&input.policy_digest)
            || !sha(&input.scope_digest)
            || !input.confirm_merge
            || !input.accept_non_atomic_target_guard
            || !input.accept_existing_automation
            || input.deadline_semantics != "last_dispatch"
            || input.risk_disclosure_version != RISK_VERSION
            || input.risk_disclosure_sha256 != crate::publication::digest(RISK_TEXT.as_bytes())
        {
            return Err(Error::Invalid("Explicit merge confirmation requires exact bounded preview scope and policy digests, risk disclosure, preflight-only guard acceptance, last-dispatch deadline and existing-automation acceptance".into()));
        }
        let mut state = self.state.lock().map_err(|_| Error::Poisoned)?;
        let task = state.store.get(id)?;
        let tx = state
            .control
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = tx
            .query_row(
                "SELECT id FROM app_merge_authorizations WHERE key=?1",
                [&input.key],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
        {
            let r = read(&tx, existing)?;
            if r.view.publication_task_id != id || r.request != input {
                return Err(unavailable(
                    "merge_idempotency_conflict",
                    "This key is already bound to different immutable merge consent",
                ));
            }
            return Ok(r.view);
        }
        let track = ci_tracking::ci_for_task(&tx, id)?
            .into_iter()
            .find(|t| t.id == input.ci_track_id)
            .ok_or_else(|| {
                Error::Invalid("Source CI track not found for this publication".into())
            })?;
        let target = target(&task, &track)?;
        let previous = previous_scope(
            &tx,
            &target.repository,
            target.pr_number,
            Some(target.repository_id),
            Some(target.pr_id),
        )?;
        if previous.as_ref().is_some_and(|r| !safe_to_replace(r)) {
            return Err(unavailable(
                "merge_scope_reserved",
                "This PR has live, unresolved or previously dispatched consent; changing the key cannot authorize another effect",
            ));
        }
        let p = self
            .host
            .config()
            .merge_policies
            .get(&input.policy)
            .ok_or_else(|| {
                unavailable(
                    "merge_disabled",
                    "This merge policy is not configured; merge is disabled by default",
                )
            })?;
        let source = self
            .host
            .config()
            .ci_policies
            .get(&p.ci_policy)
            .ok_or_else(|| Error::Invalid("CI source unavailable".into()))?;
        let digest = p.digest(self.host.config())?;
        let at = now()?;
        if p.ci_policy != track.policy
            || source.digest(&self.host.config().workspace_root)? != track.policy_digest
            || digest != input.policy_digest
            || scope(
                &target,
                &input.policy,
                &digest,
                &track,
                previous.as_ref().map(|r| r.view.id),
            ) != input.scope_digest
            || (input.allow_ready && !p.allow_ready)
        {
            return Err(unavailable(
                "merge_scope_changed",
                "The exact publication, source, policy or ready permission changed since preview",
            ));
        }
        if input.deadline <= at
            || input.deadline > at.saturating_add(p.authorization_window_seconds)
        {
            return Err(unavailable(
                "merge_window_invalid",
                "The preview deadline expired or exceeds the configured bounded authorization window",
            ));
        }
        self.ci_prepare_storage(&tx)?;
        if self.merge_guard_path().exists() {
            return Err(unavailable(
                "merge_local_process_unresolved",
                "An active or retained merge process guard must be resolved before accepting fresh consent",
            ));
        }
        if let Some(d) = self.merge_lane_diagnostic(&tx)? {
            return Err(unavailable(&d.code, &d.message));
        }
        let mut consent = serde_json::to_value(&input).expect("serializable consent");
        consent.as_object_mut().unwrap().remove("key");
        let mut record = Record {
            view: MergeAuthorization {
                id: 0,
                publication_task_id: id,
                ci_track_id: track.id,
                previous_authorization_id: previous.as_ref().map(|r| r.view.id),
                policy: input.policy.clone(),
                policy_digest: digest,
                scope_digest: input.scope_digest.clone(),
                ci_policy: track.policy.clone(),
                ci_policy_digest: track.policy_digest.clone(),
                ci_source: public_source(&track),
                target,
                resolved_pr_node_id: None,
                merge_method: p.merge_method.clone(),
                target_guard: p.target_guard.clone(),
                allow_ready: input.allow_ready,
                authorization_window_seconds: p.authorization_window_seconds,
                poll_interval_seconds: p.poll_interval_seconds,
                created_at: at,
                deadline: input.deadline,
                consent,
                status: "watching".into(),
                revision: 1,
                attempt: 0,
                revoked: false,
                ready_dispatched: false,
                merge_dispatched: false,
                ready_write_not_started: false,
                merge_write_not_started: false,
                ready_confirmed: false,
                merge_failed_confirmed: false,
                async_request: None,
                merge_commit_sha: None,
                last_observed_at: None,
                next_poll_at: at,
                latest_evidence: None,
                diagnostic: None,
                remote_merge_eligibility: "not_established".into(),
                observed_remote_status: None,
                observation_only: false,
            },
            request: input.clone(),
            publication_result_sha256: crate::publication::digest(
                task.result.as_deref().unwrap_or("").as_bytes(),
            ),
            ci_binding: clean_ci_binding(&track),
            active: false,
            phase: String::new(),
            next_operation: Some("preflight".into()),
            gate_identity: None,
            completed_response_attempt: None,
            readonly_reconciliation: false,
            transient_failures: 0,
            last_reconcile_revision: None,
        };
        tx.execute("INSERT INTO app_merge_authorizations(key,task_id,repository,pr_number,status,revision,attempt,active,next_poll_at,record) VALUES(?1,?2,?3,?4,'watching',1,0,0,?5,'{}')",params![input.key,id,record.view.target.repository,record.view.target.pr_number,at])?;
        record.view.id = tx.last_insert_rowid();
        tx.execute("INSERT INTO app_merge_scopes(repository,pr_number,repository_id,pr_id,authorization_id) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(repository_id,pr_id) DO UPDATE SET repository=excluded.repository,pr_number=excluded.pr_number,authorization_id=excluded.authorization_id",params![record.view.target.repository,record.view.target.pr_number,record.view.target.repository_id,record.view.target.pr_id,record.view.id])?;
        write(&tx, &record, "authorized")?;
        tx.commit()?;
        Ok(record.view)
    }
    pub fn merge_revoke(&self, id: i64, input: MergeControlRequest) -> Result<MergeAuthorization> {
        // Acquire the per-attempt gate outside both the app mutex and SQLite.
        // The expected revision is checked again after acquiring it. If the
        // attempt completed meanwhile, no revocation is acknowledged: refresh.
        let snapshot = {
            let state = self.state.lock().map_err(|_| Error::Poisoned)?;
            let r = read(&state.control, id)?;
            if r.view.revision != input.expected_revision {
                return Err(unavailable(
                    "merge_stale_revision",
                    "Merge authorization changed; refresh before revoking",
                ));
            }
            if r.view.revoked {
                return Ok(r.view);
            }
            self.ci_validate_storage(&state.control)?;
            r
        };
        let mut gate = if snapshot.active && matches!(snapshot.phase.as_str(), "ready" | "merge") {
            Some(self.lock_merge_gate(&snapshot)?)
        } else {
            None
        };
        let mut state = self.state.lock().map_err(|_| Error::Poisoned)?;
        let tx = state
            .control
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut r = read(&tx, id)?;
        if r.view.revision != input.expected_revision
            || r.view.attempt != snapshot.view.attempt
            || r.phase != snapshot.phase
        {
            return Err(unavailable(
                "merge_stale_revision",
                "The in-flight attempt changed while waiting for its dispatch gate; refresh before revoking",
            ));
        }
        self.ci_validate_storage(&tx)?;
        let cancel_preflight = if let Some(file) = gate.as_mut() {
            self.revoke_merge_gate(&r, file)?
        } else {
            r.active && r.phase == "preflight"
        };
        r.view.revoked = true;
        r.view.revision += 1;
        // Revocation cannot overwrite a known terminal outcome.
        if !r.active
            && r.view.async_request.is_none()
            && !matches!(
                r.view.status.as_str(),
                "merged" | "externally_merged" | "failed" | "enqueued" | "process_unknown"
            )
        {
            if !no_unresolved_effect(&r) {
                diagnostic(
                    &mut r,
                    "effect_unknown",
                    "merge_revoked_after_dispatch",
                    "New writes are forbidden; a previously dispatched effect may complete. Explicit read-only reconciliation remains available",
                );
            } else {
                diagnostic(
                    &mut r,
                    "revoked",
                    "merge_revoked",
                    "Authorization revoked; all prior readiness effects, if any, are known and no merge was dispatched",
                );
            }
        }
        write(&tx, &r, "revoked")?;
        tx.commit()?;
        if cancel_preflight
            && let Some((running_id, attempt, flag)) = self
                .merge_running
                .lock()
                .map_err(|_| Error::Poisoned)?
                .as_ref()
            && *running_id == id
            && *attempt == r.view.attempt
        {
            flag.store(true, Ordering::Release);
        }
        Ok(r.view)
    }
    pub fn merge_reconcile(
        &self,
        id: i64,
        input: MergeControlRequest,
    ) -> Result<MergeAuthorization> {
        let mut state = self.state.lock().map_err(|_| Error::Poisoned)?;
        let tx = state
            .control
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut r = read(&tx, id)?;
        if r.last_reconcile_revision == Some(input.expected_revision) {
            return Ok(r.view);
        }
        if r.view.revision != input.expected_revision {
            return Err(unavailable(
                "merge_stale_revision",
                "Merge authorization changed; refresh before reconciliation",
            ));
        }
        self.ci_validate_storage(&tx)?;
        if r.active || r.view.status == "process_unknown" || self.merge_guard_path().exists() {
            return Err(unavailable(
                "merge_local_recovery_required",
                "Active or unknown local process ownership requires exact local stopped confirmation; HTTP/MCP cannot clear it",
            ));
        }
        if safe_to_replace(&r)
            || matches!(r.view.status.as_str(), "merged" | "externally_merged")
            || r.next_operation.as_deref() == Some("reconcile")
        {
            return Ok(r.view);
        }
        if r.view.attempt >= MAX_ATTEMPTS {
            return Err(unavailable(
                "merge_attempt_limit",
                "The bounded merge observation budget is exhausted; retained evidence remains readable",
            ));
        }
        self.merge_policy(&r)?;
        r.last_reconcile_revision = Some(input.expected_revision);
        r.readonly_reconciliation = true;
        r.view.observation_only = true;
        r.next_operation = Some("reconcile".into());
        r.view.next_poll_at = now()?;
        r.view.revision += 1;
        r.view.diagnostic = None;
        r.view.status = "watching".into();
        write(&tx, &r, "readonly_reconcile_requested")?;
        tx.commit()?;
        Ok(r.view)
    }
    fn merge_policy(&self, r: &Record) -> Result<&MergePolicy> {
        let p = self
            .host
            .config()
            .merge_policies
            .get(&r.view.policy)
            .ok_or_else(|| {
                unavailable(
                    "merge_policy_changed",
                    "The admitted merge policy is no longer configured",
                )
            })?;
        if p.digest(self.host.config())? != r.view.policy_digest {
            return Err(unavailable(
                "merge_policy_changed",
                "The admitted merge/CI policy or adapter executable changed; restore its binding before execution",
            ));
        }
        Ok(p)
    }
    fn merge_gate_path(&self, id: i64, attempt: u64) -> PathBuf {
        self.ci_storage_root
            .join(format!(".merge-write-gate-{id}-{attempt}"))
    }
    fn create_merge_gate(&self, r: &Record) -> Result<GateIdentity> {
        let path = self.merge_gate_path(r.view.id, r.view.attempt);
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(invalid_io)?;
        let gate = WriteGate {
            version: 1,
            authorization_id: r.view.id,
            attempt: r.view.attempt,
            phase: r.phase.clone(),
            consent_sha256: hash(&r.request),
            deadline: r.view.deadline,
            revoked: r.view.revoked,
            write_started: false,
        };
        serde_json::to_writer(&mut f, &gate)
            .map_err(|_| Error::Invalid("Cannot encode merge gate".into()))?;
        f.sync_all()
            .and_then(|_| ci_tracking::sync_parent(&path))
            .map_err(invalid_io)?;
        let m = f.metadata().map_err(invalid_io)?;
        Ok(GateIdentity {
            device: m.dev(),
            inode: m.ino(),
        })
    }
    fn lock_merge_gate(&self, r: &Record) -> Result<File> {
        let path = self.merge_gate_path(r.view.id, r.view.attempt);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
            .map_err(invalid_io)?;
        let metadata = file.metadata().map_err(invalid_io)?;
        if r.gate_identity
            .as_ref()
            .is_none_or(|g| g.device != metadata.dev() || g.inode != metadata.ino())
            || !ci_tracking::matches_file(&path, &file)
        {
            return Err(unavailable(
                "merge_gate_invalid",
                "The write gate changed; preserve the local attempt",
            ));
        }
        let end = Instant::now() + Duration::from_secs(35);
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let e = io::Error::last_os_error();
            if !matches!(
                e.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
            ) {
                return Err(invalid_io(e));
            }
            if Instant::now() >= end {
                return Err(unavailable(
                    "merge_dispatch_inflight",
                    "Revocation is not yet acknowledged because the write gate remains locked; inspect this exact in-flight request and retry after its bounded process finishes",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(file)
    }
    fn revoke_merge_gate(&self, r: &Record, file: &mut File) -> Result<bool> {
        let path = self.merge_gate_path(r.view.id, r.view.attempt);
        let mut raw = String::new();
        (&*file)
            .take(4097)
            .read_to_string(&mut raw)
            .map_err(invalid_io)?;
        let mut gate: WriteGate = serde_json::from_str(&raw)
            .map_err(|_| Error::Invalid("The bounded write gate is invalid".into()))?;
        if raw.len() > 4096
            || gate.version != 1
            || gate.authorization_id != r.view.id
            || gate.attempt != r.view.attempt
            || gate.phase != r.phase
            || gate.consent_sha256 != hash(&r.request)
            || gate.deadline != r.view.deadline
            || !ci_tracking::matches_file(&path, file)
        {
            return Err(unavailable(
                "merge_gate_invalid",
                "The write gate does not match this exact immutable authorization and phase",
            ));
        }
        gate.revoked = true;
        file.seek(SeekFrom::Start(0))
            .and_then(|_| file.set_len(0))
            .map_err(invalid_io)?;
        serde_json::to_writer(&mut *file, &gate)
            .map_err(|_| Error::Invalid("Cannot persist write-gate revocation".into()))?;
        file.sync_all().map_err(invalid_io)?;
        Ok(!gate.write_started)
    }
    fn merge_gate_started(&self, r: &Record) -> Result<Option<bool>> {
        if !matches!(r.phase.as_str(), "ready" | "merge") {
            return Ok(None);
        }
        let path = self.merge_gate_path(r.view.id, r.view.attempt);
        let Some(file) = ci_tracking::open_marker(&path)? else {
            return Ok(None);
        };
        let m = file.metadata().map_err(invalid_io)?;
        if r.gate_identity
            .as_ref()
            .is_none_or(|g| g.device != m.dev() || g.inode != m.ino())
            || m.mode() & 0o777 != 0o600
            || m.uid() != unsafe { libc::geteuid() }
            || unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0
        {
            return Err(unavailable(
                "merge_gate_unavailable",
                "The exact gate is not available for stopped-process proof",
            ));
        }
        let mut raw = Vec::new();
        (&file)
            .take(4097)
            .read_to_end(&mut raw)
            .map_err(invalid_io)?;
        let gate: WriteGate = serde_json::from_slice(&raw)
            .map_err(|_| Error::Invalid("Invalid merge gate".into()))?;
        if raw.len() > 4096
            || gate.version != 1
            || gate.authorization_id != r.view.id
            || gate.attempt != r.view.attempt
            || gate.phase != r.phase
            || gate.consent_sha256 != hash(&r.request)
            || gate.deadline != r.request.deadline
            || !ci_tracking::matches_file(&path, &file)
        {
            return Err(unavailable(
                "merge_gate_invalid",
                "The stopped-process gate does not match immutable consent",
            ));
        }
        Ok(Some(gate.write_started))
    }
    fn merge_guard_path(&self) -> PathBuf {
        self.ci_storage_root.join(GUARD)
    }
    fn merge_workspace(&self, id: i64, attempt: u64) -> PathBuf {
        self.ci_storage_root.join(format!("merge-{id}-{attempt}"))
    }
    fn merge_lane_diagnostic(&self, db: &Connection) -> Result<Option<CiDiagnostic>> {
        if db.query_row("SELECT COUNT(*) FROM app_ci_storage", [], |r| {
            r.get::<_, i64>(0)
        })? == 0
        {
            return Ok(None);
        }
        if self.ci_validate_storage(db).is_err() {
            return Ok(Some(CiDiagnostic{code:"merge_storage_root_changed".into(),message:"The durable control root changed; restore its original path/device/inode before merge operations or local recovery".into()}));
        }
        let invalid = match ci_tracking::open_marker(&self.merge_guard_path()) {
            Ok(None) => false,
            Ok(Some(f)) => {
                marker_contents(&f).map_or(true, |m| m.database != self.ci_database_identity)
            }
            Err(_) => true,
        };
        Ok(invalid.then(|| CiDiagnostic {
            code: "merge_guard_invalid".into(),
            message:
                "A retained merge guard has invalid identity; preserve it and inspect the host"
                    .into(),
        }))
    }
}
fn uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit() && !b.is_ascii_uppercase()
            }
        })
}
fn valid_request(r: &Record, request: &AsyncRequest) -> bool {
    uuid(&request.id)
        && request.options
            == AsyncOptions {
                sha: r.view.target.head_sha.clone(),
                merge_method: r.view.merge_method.clone(),
                merge_action: "direct_merge".into(),
                bypass_rules: false,
            }
        && matches!(request.provenance.as_str(), "relay" | "external_unknown")
}
fn adapter_input(r: &Record) -> String {
    let v = &r.view;
    let t = &v.target;
    json!({"version":1,"operation":r.phase,"authorization_id":v.id,"attempt":v.attempt,"repository":t.repository,"repository_id":t.repository_id,"pr_number":t.pr_number,"pr_id":t.pr_id,"pr_node_id":v.resolved_pr_node_id,"pr_url":t.pr_url,"head_sha":t.head_sha,"head_branch":t.head_branch,"base_branch":t.base_branch,"ci_source":v.ci_source,"merge_method":v.merge_method,"target_guard":v.target_guard,"allow_ready":v.allow_ready,"accept_non_atomic_target_guard":true,"deadline_semantics":"last_dispatch","deadline":v.deadline,"async_request":v.async_request}).to_string()
}
fn parse_response(command: &CommandResult, phase: &str) -> Option<AdapterResponse> {
    if command.stdout_truncated || command.stderr_truncated || command.stdout.len() > MAX_EVIDENCE {
        return None;
    }
    let response: AdapterResponse = serde_json::from_str(command.stdout.trim()).ok()?;
    if response.version != 1
        || response.operation != phase
        || !matches!(
            response.status.as_str(),
            "preflight_ready"
                | "waiting_ci"
                | "ready_confirmed"
                | "accepted"
                | "pending"
                | "merged"
                | "externally_merged"
                | "failed"
                | "enqueued"
                | "effect_unknown"
                | "blocked"
                | "transient_error"
        )
        || !matches!(
            response.effect.as_str(),
            "none"
                | "unknown"
                | "ready_confirmed"
                | "merge_request_recorded"
                | "merge_confirmed"
                | "externally_merged"
        )
        || response.error_code.as_ref().is_some_and(|v| {
            v.is_empty()
                || v.len() > 64
                || !v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
        || response
            .detail
            .as_ref()
            .is_some_and(|v| v.len() > 512 || v.chars().any(char::is_control))
        || response.merge_commit_sha.as_ref().is_some_and(|v| !sha(v))
    {
        return None;
    }
    Some(response)
}
fn validate_target(r: &Record, t: &TargetObservation) -> bool {
    let v = &r.view;
    let fixed = &v.target;
    t.repository_id == fixed.repository_id
        && t.pr_id == fixed.pr_id
        && node(&t.pr_node_id)
        && v.resolved_pr_node_id
            .as_ref()
            .is_none_or(|n| n == &t.pr_node_id)
        && t.head_sha == fixed.head_sha
        && t.head_branch == fixed.head_branch
        && t.base_branch == fixed.base_branch
        && sha(&t.base_sha)
        && matches!(t.state.as_str(), "open" | "closed")
        && (!t.merged || t.state == "closed")
        && t.stack_clear
        && t.queue_clear
        && t.auto_merge_disabled
}
fn mark_invalid(r: &mut Record) {
    if matches!(r.phase.as_str(), "ready" | "merge") {
        diagnostic(
            r,
            "effect_unknown",
            "merge_protocol_invalid",
            "A dispatched mutation returned incomplete or mismatched evidence; do not retry it. Preserve the exact attempt and reconcile read-only",
        );
    } else {
        diagnostic(
            r,
            "blocked",
            "merge_protocol_invalid",
            "The read-only adapter returned incomplete, mismatched or contradictory evidence",
        );
    }
}
fn apply_response(r: &mut Record, outcome: Outcome, response: Option<&AdapterResponse>, at: u64) {
    r.next_operation = None;
    if outcome == Outcome::Unknown {
        if response.is_some_and(|v| {
            v.complete
                && matches!(
                    v.status.as_str(),
                    "ready_confirmed"
                        | "accepted"
                        | "pending"
                        | "merged"
                        | "externally_merged"
                        | "failed"
                        | "enqueued"
                )
        }) {
            apply_response(r, Outcome::Success, response, at);
            if matches!(
                r.view.status.as_str(),
                "watching"
                    | "accepted"
                    | "pending"
                    | "merged"
                    | "externally_merged"
                    | "failed"
                    | "enqueued"
                    | "revoked"
                    | "expired"
            ) {
                r.view.observed_remote_status = response.map(|v| v.status.clone());
            }
        }
        diagnostic(
            r,
            "process_unknown",
            "merge_cleanup_unknown",
            "Local process-tree cleanup is unconfirmed; exact authorization/attempt/phase stopped confirmation is required",
        );
        return;
    }
    let Some(response) = response else {
        mark_invalid(r);
        return;
    };
    let durable_effect = response.complete
        && matches!(
            response.status.as_str(),
            "ready_confirmed"
                | "accepted"
                | "pending"
                | "merged"
                | "externally_merged"
                | "failed"
                | "enqueued"
        );
    if outcome != Outcome::Success && !durable_effect {
        mark_invalid(r);
        return;
    }
    // A result is never discarded solely because revocation or expiry happened after dispatch.
    if let Some(request) = &response.async_request
        && (!valid_request(r, request)
            || !matches!(r.phase.as_str(), "merge" | "reconcile")
            || r.view
                .async_request
                .as_ref()
                .is_some_and(|old| old != request)
            || (r.phase == "reconcile" && r.view.async_request.is_none()))
    {
        mark_invalid(r);
        return;
    }
    if let Some(target) = &response.target
        && !validate_target(r, target)
    {
        mark_invalid(r);
        return;
    }
    if response.status == "effect_unknown" || response.effect == "unknown" {
        if response.complete {
            mark_invalid(r);
            return;
        }
        // Validated UUID from an earlier phase is retained even when its result has expired.
        r.view.latest_evidence =
            Some(serde_json::to_value(response).expect("serializable evidence"));
        diagnostic(
            r,
            "effect_unknown",
            response
                .error_code
                .as_deref()
                .unwrap_or("merge_effect_unknown"),
            "The external effect or result cannot be established; no blind mutation retry is allowed. Read-only reconciliation remains available",
        );
        return;
    }
    if matches!(response.status.as_str(), "blocked" | "transient_error") {
        if response.complete || response.effect != "none" || response.async_request.is_some() {
            mark_invalid(r);
            return;
        }
        if response.status == "transient_error"
            && r.phase == "preflight"
            && !r.view.revoked
            && at < r.view.deadline
            && r.transient_failures < 3
        {
            r.transient_failures += 1;
            diagnostic(
                r,
                "watching",
                response
                    .error_code
                    .as_deref()
                    .unwrap_or("merge_transient_error"),
                "Bounded read-only preflight temporarily unavailable; no mutation was sent",
            );
            r.next_operation = Some("preflight".into());
            r.view.next_poll_at = at + r.view.poll_interval_seconds;
        } else {
            diagnostic(r,"blocked",response.error_code.as_deref().unwrap_or("merge_preflight_blocked"),response.detail.as_deref().unwrap_or("The exact target, policy, permission or remote state could not be established; no further write is allowed"));
        }
        return;
    }
    if !response.complete {
        mark_invalid(r);
        return;
    }
    // Mutating phases must return their own fresh target and exact-source CI evidence.
    let fresh = if matches!(r.phase.as_str(), "preflight" | "ready" | "merge")
        && response.status != "externally_merged"
    {
        let (Some(target), Some(ci)) = (&response.target, &response.ci_observation) else {
            mark_invalid(r);
            return;
        };
        let Ok(track) = ci_tracking::validate_merge_observation(&r.ci_binding, ci, at) else {
            mark_invalid(r);
            return;
        };
        if track.observed_base_sha.as_deref() != Some(&target.base_sha)
            || ci["pull_request"]["draft"].as_bool() != Some(target.draft)
            || ci["pull_request"]["state"].as_str() != Some(&target.state)
            || target.merged
            || target.state != "open"
        {
            mark_invalid(r);
            return;
        }
        Some(track)
    } else {
        None
    };
    match response.status.as_str() {
        "preflight_ready" | "waiting_ci" => {
            let Some(track) = fresh else {
                mark_invalid(r);
                return;
            };
            if response.effect != "none"
                || response.async_request.is_some()
                || response.status == "preflight_ready"
                    && track.status != "configured_checks_passed"
                || response.status == "waiting_ci" && track.status == "configured_checks_passed"
            {
                mark_invalid(r);
                return;
            }
            if r.phase != "preflight" {
                diagnostic(
                    r,
                    "blocked",
                    "merge_no_effect",
                    "A final mutating-phase preflight declined the effect. No automatic mutation retry is performed",
                );
                return;
            }
            let target = response.target.as_ref().unwrap();
            r.view.resolved_pr_node_id = Some(target.pr_node_id.clone());
            if r.readonly_reconciliation {
                diagnostic(
                    r,
                    "blocked",
                    "merge_readonly_observed",
                    "Read-only reconciliation completed; it does not restart writes",
                );
            } else if r.view.revoked {
                diagnostic(
                    r,
                    "revoked",
                    "merge_revoked",
                    "Revocation forbids new writes",
                );
            } else if at >= r.view.deadline {
                diagnostic(
                    r,
                    "expired",
                    "merge_window_ended",
                    "The last-dispatch deadline ended before the next effect",
                );
            } else if target.draft {
                if r.view.allow_ready && !r.view.ready_dispatched {
                    r.view.status = "watching".into();
                    r.next_operation = Some("ready".into());
                    r.view.next_poll_at = at;
                } else {
                    diagnostic(
                        r,
                        "blocked",
                        "merge_draft_requires_consent",
                        "The PR is draft and no unused draft-to-ready grant exists; no readiness change will be repeated",
                    );
                }
            } else if track.status == "configured_checks_passed" && !r.view.merge_dispatched {
                r.view.status = "watching".into();
                r.next_operation = Some("merge".into());
                r.view.next_poll_at = at;
            } else {
                r.view.status = "watching".into();
                r.next_operation = Some("preflight".into());
                r.view.next_poll_at = at + r.view.poll_interval_seconds;
            }
        }
        "ready_confirmed" => {
            if r.phase != "ready"
                || response.effect != "ready_confirmed"
                || response.async_request.is_some()
                || !r.view.allow_ready
                || !r.view.ready_dispatched
                || fresh.is_none()
            {
                mark_invalid(r);
                return;
            }
            r.view.ready_confirmed = true;
            if r.view.revoked {
                diagnostic(
                    r,
                    "revoked",
                    "ready_completed_after_revoke",
                    "The dispatched ready change completed; revocation prevents merge dispatch",
                );
            } else if at >= r.view.deadline {
                diagnostic(
                    r,
                    "expired",
                    "ready_completed_after_expiry",
                    "The dispatched ready change completed after expiry; no merge will be dispatched",
                );
            } else {
                r.view.status = "watching".into();
                r.next_operation = Some("preflight".into());
                r.view.next_poll_at = at;
            }
        }
        "accepted" | "pending" => {
            let Some(request) = &response.async_request else {
                mark_invalid(r);
                return;
            };
            if r.phase == "merge" {
                if fresh
                    .as_ref()
                    .is_none_or(|c| c.status != "configured_checks_passed")
                    || response.target.as_ref().is_none_or(|t| t.draft)
                    || !r.view.merge_dispatched
                    || (response.status == "accepted"
                        && (response.effect != "merge_request_recorded"
                            || request.provenance != "relay"))
                    || (response.status == "pending"
                        && (response.effect != "none" || request.provenance != "external_unknown"))
                {
                    mark_invalid(r);
                    return;
                }
            } else if r.phase != "reconcile"
                || response.status != "pending"
                || response.effect != "none"
            {
                mark_invalid(r);
                return;
            }
            r.view.async_request = Some(request.clone());
            r.view.observation_only = true;
            r.readonly_reconciliation = true;
            r.view.status = response.status.clone();
            r.next_operation = Some("reconcile".into());
            r.view.next_poll_at = at + r.view.poll_interval_seconds;
        }
        "merged" => {
            if r.phase != "reconcile"
                || response.effect != "merge_confirmed"
                || r.view
                    .async_request
                    .as_ref()
                    .is_none_or(|q| q.provenance != "relay")
                || response.async_request != r.view.async_request
                || response.merge_commit_sha.is_none()
            {
                mark_invalid(r);
                return;
            }
            r.view.status = "merged".into();
            r.view.merge_commit_sha = response.merge_commit_sha.clone();
            r.view.remote_merge_eligibility = "merged".into();
        }
        "externally_merged" => {
            if response.effect != "externally_merged"
                || (r.phase == "reconcile"
                    && r.view
                        .async_request
                        .as_ref()
                        .is_some_and(|q| q.provenance == "relay"))
            {
                mark_invalid(r);
                return;
            }
            // A UUID-less PR read or PUT already-merged response cannot attribute the merge to Relay.
            r.view.status = "externally_merged".into();
            r.view.merge_commit_sha = response.merge_commit_sha.clone();
            r.view.remote_merge_eligibility = "externally_merged".into();
        }
        "failed" | "enqueued" => {
            let server_receipt = r.phase == "reconcile"
                && r.view.async_request.is_some()
                && response.async_request == r.view.async_request;
            let already_queued = r.phase == "merge"
                && response.status == "enqueued"
                && response.async_request.is_none()
                && fresh
                    .as_ref()
                    .is_some_and(|c| c.status == "configured_checks_passed")
                && response.target.as_ref().is_some_and(|t| !t.draft);
            if !(server_receipt || already_queued) || response.effect != "none" {
                mark_invalid(r);
                return;
            }
            if response.status == "failed" {
                r.view.merge_failed_confirmed = true;
            }
            diagnostic(
                r,
                &response.status,
                if response.status == "enqueued" {
                    "merge_unexpected_queue"
                } else {
                    "merge_server_failed"
                },
                if response.status == "enqueued" {
                    "GitHub reported an unexpected queued request outside the authorized direct-merge scope; no further mutation is allowed"
                } else {
                    "GitHub reported this exact async request failed; no automatic mutation retry is allowed"
                },
            );
        }
        _ => {
            mark_invalid(r);
            return;
        }
    }
    r.transient_failures = 0;
    r.view.last_observed_at = Some(at);
    if matches!(
        response.status.as_str(),
        "ready_confirmed"
            | "accepted"
            | "pending"
            | "merged"
            | "externally_merged"
            | "failed"
            | "enqueued"
    ) {
        r.view.observed_remote_status = Some(response.status.clone());
    }
    if !matches!(
        r.view.status.as_str(),
        "blocked" | "failed" | "enqueued" | "revoked" | "expired"
    ) {
        r.view.diagnostic = None;
    }
    r.view.latest_evidence = Some(serde_json::to_value(response).expect("serializable evidence"));
}
fn marker_contents(file: &File) -> Result<Marker> {
    let mut raw = Vec::new();
    file.take(2049).read_to_end(&mut raw).map_err(invalid_io)?;
    if raw.len() > 2048 {
        return Err(Error::Invalid("merge guard exceeds bound".into()));
    }
    let marker: Marker = serde_json::from_slice(&raw)
        .map_err(|_| Error::Invalid("merge guard identity invalid".into()))?;
    if marker.authorization_id <= 0
        || marker.attempt == 0
        || !matches!(
            marker.phase.as_str(),
            "preflight" | "ready" | "merge" | "reconcile"
        )
    {
        return Err(Error::Invalid(
            "merge guard phase or identity invalid".into(),
        ));
    }
    Ok(marker)
}
fn receipt_path(workspace: &Path) -> PathBuf {
    workspace.join(".merge-receipt.json")
}
fn save_receipt(workspace: &Path, receipt: &Receipt) -> Result<()> {
    let path = receipt_path(workspace);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(invalid_io)?;
    let raw = serde_json::to_vec(receipt).expect("serializable receipt");
    if raw.len() > MAX_RECORD {
        return Err(Error::Invalid("merge receipt exceeds bound".into()));
    }
    file.write_all(&raw)
        .and_then(|_| file.sync_all())
        .and_then(|_| ci_tracking::sync_parent(&path))
        .map_err(invalid_io)
}
fn load_receipt(workspace: &Path) -> Result<Option<Receipt>> {
    let Some(file) = ci_tracking::open_marker(&receipt_path(workspace))? else {
        return Ok(None);
    };
    let mut raw = Vec::new();
    file.take((MAX_RECORD + 1) as u64)
        .read_to_end(&mut raw)
        .map_err(invalid_io)?;
    if raw.len() > MAX_RECORD {
        return Err(Error::Invalid("merge receipt exceeds bound".into()));
    }
    serde_json::from_slice(&raw)
        .map(Some)
        .map_err(|_| Error::Invalid("merge receipt invalid; preserve host evidence".into()))
}
impl Application {
    /// At most one short process per merge lane, never a core queue claim. A saved
    /// server UUID waits without holding a process lock or application mutex.
    pub fn merge_work_once(&self) -> Result<bool> {
        let (record, guard, cancel) = {
            let mut state = self.state.lock().map_err(|_| Error::Poisoned)?;
            if self.shutdown.load(Ordering::Acquire) {
                return Ok(false);
            }
            let tx = state
                .control
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            if tx.query_row("SELECT COUNT(*) FROM app_merge_authorizations", [], |r| {
                r.get::<_, i64>(0)
            })? == 0
            {
                return Ok(false);
            }
            self.ci_validate_storage(&tx)?;
            let path = self.merge_guard_path();
            if let Some(file) = ci_tracking::open_marker(&path)? {
                match ci_tracking::flock(&file) {
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                    Err(e) => return Err(invalid_io(e)),
                    Ok(()) => {
                        let marker = marker_contents(&file)?;
                        if marker.database != self.ci_database_identity {
                            return Err(unavailable(
                                "merge_guard_database_mismatch",
                                "The retained merge guard belongs to another database",
                            ));
                        }
                        let mut r = read(&tx, marker.authorization_id)?;
                        if (marker.attempt != r.view.attempt
                            && !(marker.attempt == r.view.attempt + 1 && !r.active))
                            || (marker.attempt == r.view.attempt && marker.phase != r.phase)
                        {
                            return Err(unavailable(
                                "merge_guard_attempt_mismatch",
                                "The retained merge guard does not match the exact durable attempt and phase",
                            ));
                        }
                        if r.view.status != "process_unknown" {
                            r.view.revision += 1;
                            diagnostic(
                                &mut r,
                                "process_unknown",
                                "merge_retained_guard",
                                "A retained process guard requires exact local stopped confirmation; elapsed time is not cleanup proof",
                            );
                            write(&tx, &r, "retained_guard")?;
                            tx.commit()?;
                        }
                        return Ok(false);
                    }
                }
            }
            if let Some(id) = tx
                .query_row(
                    "SELECT id FROM app_merge_authorizations WHERE active=1",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
            {
                let mut r = read(&tx, id)?;
                if r.view.status != "process_unknown" {
                    r.view.revision += 1;
                    diagnostic(
                        &mut r,
                        "process_unknown",
                        "merge_missing_guard",
                        "An active durable merge attempt has no matching guard; local stopped confirmation is required",
                    );
                    write(&tx, &r, "missing_guard")?;
                    tx.commit()?;
                }
                return Ok(false);
            }
            let mut query=tx.prepare("SELECT id FROM app_merge_authorizations WHERE active=0 AND next_poll_at<=?1 AND status IN ('watching','accepted','pending') ORDER BY next_poll_at,id LIMIT 1")?;
            let id = query
                .query_row([now()?], |r| r.get::<_, i64>(0))
                .optional()?;
            drop(query);
            let Some(id) = id else {
                return Ok(false);
            };
            let mut r = read(&tx, id)?;
            let Some(phase) = r.next_operation.clone() else {
                return Ok(false);
            };
            let at = now()?;
            if r.view.attempt >= MAX_ATTEMPTS {
                r.view.revision += 1;
                diagnostic(
                    &mut r,
                    "blocked",
                    "merge_attempt_limit",
                    "The bounded observation budget ended; no additional process will start",
                );
                write(&tx, &r, "attempt_limit")?;
                tx.commit()?;
                return Ok(true);
            }
            if phase != "reconcile" && (r.view.revoked || at >= r.view.deadline) {
                r.view.revision += 1;
                let revoked = r.view.revoked;
                diagnostic(
                    &mut r,
                    if revoked { "revoked" } else { "expired" },
                    if revoked {
                        "merge_revoked"
                    } else {
                        "merge_window_ended"
                    },
                    "No new effect may be dispatched after revocation or the fixed last-dispatch deadline",
                );
                write(&tx, &r, "dispatch_forbidden")?;
                tx.commit()?;
                return Ok(true);
            }
            if self.merge_policy(&r).is_err() {
                r.view.revision += 1;
                diagnostic(
                    &mut r,
                    "blocked",
                    "merge_policy_changed",
                    "The admitted policy/source/executable binding changed; restore it before read-only reconciliation",
                );
                write(&tx, &r, "policy_changed")?;
                tx.commit()?;
                return Ok(true);
            }
            // Re-read immutable publication evidence before every phase, including retries.
            let raw: Option<String> = tx
                .query_row(
                    "SELECT result FROM tasks WHERE id=?1",
                    [r.view.publication_task_id],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            if raw
                .as_ref()
                .map(|v| crate::publication::digest(v.as_bytes()))
                .as_deref()
                != Some(&r.publication_result_sha256)
            {
                r.view.revision += 1;
                diagnostic(
                    &mut r,
                    "blocked",
                    "merge_publication_changed",
                    "The original successful publication receipt changed or disappeared",
                );
                write(&tx, &r, "publication_changed")?;
                tx.commit()?;
                return Ok(true);
            }
            if matches!(phase.as_str(), "ready" | "merge") {
                if r.readonly_reconciliation
                    || r.view.resolved_pr_node_id.is_none()
                    || (phase == "ready" && (!r.view.allow_ready || r.view.ready_dispatched))
                    || (phase == "merge" && r.view.merge_dispatched)
                {
                    return Err(unavailable(
                        "merge_write_forbidden",
                        "No unused explicit grant exists for this effect phase",
                    ));
                }
                if phase == "ready" {
                    r.view.ready_dispatched = true;
                    r.view.status = "ready_dispatching".into();
                } else {
                    r.view.merge_dispatched = true;
                    r.view.status = "merge_dispatching".into();
                }
            }
            r.phase = phase;
            r.gate_identity = None;
            r.view.attempt += 1;
            r.view.revision += 1;
            r.active = true;
            let guard = ci_tracking::create_marker(
                &path,
                &Marker {
                    database: self.ci_database_identity.clone(),
                    authorization_id: id,
                    attempt: r.view.attempt,
                    phase: r.phase.clone(),
                },
            )?;
            if matches!(r.phase.as_str(), "ready" | "merge") {
                r.gate_identity = Some(self.create_merge_gate(&r)?);
            }
            write(
                &tx,
                &r,
                if matches!(r.phase.as_str(), "ready" | "merge") {
                    "effect_dispatch_recorded"
                } else {
                    "read_started"
                },
            )?;
            tx.commit()?;
            let cancel = Arc::new(AtomicBool::new(false));
            *self.merge_running.lock().map_err(|_| Error::Poisoned)? =
                Some((id, r.view.attempt, Arc::clone(&cancel)));
            (r, guard, cancel)
        };
        let workspace = self.merge_workspace(record.view.id, record.view.attempt);
        let response = match fs::DirBuilder::new().mode(0o700).create(&workspace) {
            Err(_) => CommandResult::error(
                Outcome::Unknown,
                "Cannot create exact private merge attempt directory; preserve guard for local inspection",
            ),
            Ok(()) => {
                let finished = AtomicBool::new(false);
                std::thread::scope(|scope| {
                    scope.spawn(|| {
                        while !finished.load(Ordering::Acquire) {
                            let revoked = self
                                .state
                                .lock()
                                .ok()
                                .and_then(|s| read(&s.control, record.view.id).ok())
                                .is_none_or(|r| r.view.revoked);
                            let before_write = if matches!(record.phase.as_str(), "ready" | "merge")
                            {
                                fs::read(self.merge_gate_path(record.view.id, record.view.attempt))
                                    .ok()
                                    .and_then(|b| serde_json::from_slice::<WriteGate>(&b).ok())
                                    .is_some_and(|g| !g.write_started)
                            } else {
                                record.phase == "preflight"
                            };
                            if self.shutdown.load(Ordering::Acquire) || (revoked && before_write) {
                                cancel.store(true, Ordering::Release);
                            }
                            std::thread::sleep(Duration::from_millis(50));
                        }
                    });
                    let gate_path = self.merge_gate_path(record.view.id, record.view.attempt);
                    let gate_binding = hash(&record.request);
                    let response = match self.merge_policy(&record) {
                        Ok(policy) => self.host.run_merge_adapter(
                            &policy.adapter,
                            &adapter_input(&record),
                            &workspace,
                            Arc::clone(&guard),
                            &cancel,
                            record
                                .gate_identity
                                .as_ref()
                                .filter(|_| matches!(record.phase.as_str(), "ready" | "merge"))
                                .map(|g| {
                                    (
                                        gate_path.as_path(),
                                        gate_binding.as_str(),
                                        g.device,
                                        g.inode,
                                    )
                                }),
                        ),
                        Err(_) => CommandResult::error(
                            Outcome::Failure,
                            "Merge policy changed before process startup",
                        ),
                    };
                    finished.store(true, Ordering::Release);
                    response
                })
            }
        };
        let parsed = parse_response(&response, &record.phase);
        let receipt = Receipt {
            database: self.ci_database_identity.clone(),
            authorization_id: record.view.id,
            attempt: record.view.attempt,
            phase: record.phase.clone(),
            consent_sha256: hash(&record.request),
            finished_at: now()?,
            outcome: response.outcome,
            gate_identity: record.gate_identity.clone(),
            gate_write_started: self.merge_gate_started(&record).ok().flatten(),
            response: parsed,
        };
        // Separate fsynced receipt survives a DB write failure after accepted/merged response.
        // If receipt persistence fails, retain the guard and also attempt the bounded DB save.
        let receipt_saved = save_receipt(&workspace, &receipt).is_ok();
        if let Ok(mut running) = self.merge_running.lock()
            && running.as_ref().is_some_and(|(id, attempt, _)| {
                *id == record.view.id && *attempt == record.view.attempt
            })
        {
            *running = None;
        }
        let unknown = response.outcome == Outcome::Unknown;
        let gate_verified = !matches!(record.phase.as_str(), "ready" | "merge")
            || receipt.gate_write_started.is_some();
        {
            let mut state = self.state.lock().map_err(|_| Error::Poisoned)?;
            let tx = state
                .control
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut current = read(&tx, record.view.id)?;
            if current.view.attempt != record.view.attempt
                || current.phase != record.phase
                || !current.active
                || current.request != record.request
            {
                return Err(unavailable(
                    "merge_stale_attempt",
                    "Late response is fenced; durable receipt and guard retained for exact-attempt recovery",
                ));
            }
            apply_response(
                &mut current,
                response.outcome,
                receipt.response.as_ref(),
                receipt.finished_at,
            );
            if response.outcome != Outcome::Unknown {
                apply_no_write_proof(
                    &mut current,
                    receipt.gate_write_started,
                    receipt.finished_at,
                );
            }
            current.completed_response_attempt = Some(record.view.attempt);
            if let Some(value) = &receipt.response {
                current.view.latest_evidence =
                    Some(serde_json::to_value(value).expect("serializable receipt evidence"));
            }
            current.active = unknown || !receipt_saved || !gate_verified;
            current.view.revision += 1;
            if !gate_verified {
                diagnostic(
                    &mut current,
                    "process_unknown",
                    "merge_gate_identity_changed",
                    "The original phase-bound write gate is missing, busy or replaced; preserve all evidence and inspect exact local ownership before recovery",
                );
            }
            if !receipt_saved {
                diagnostic(
                    &mut current,
                    "process_unknown",
                    "merge_receipt_persist_failed",
                    "The effect response could not be saved to its durable recovery receipt; preserve guard and evidence and confirm exact local cleanup before reconciliation",
                );
            }
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
        if unknown || !receipt_saved || !gate_verified {
            return Ok(true);
        }
        let path = self.merge_guard_path();
        let cleanup = (|| -> io::Result<()> {
            if !ci_tracking::matches_file(&path, &guard) {
                return Err(io::Error::other("merge guard identity changed"));
            }
            fs::remove_dir_all(&workspace)?;
            let gate = self.merge_gate_path(record.view.id, record.view.attempt);
            if matches!(record.phase.as_str(), "ready" | "merge") {
                fs::remove_file(gate)?;
            }
            fs::remove_file(&path)?;
            ci_tracking::sync_parent(&path)
        })();
        if cleanup.is_err() {
            let mut state = self.state.lock().map_err(|_| Error::Poisoned)?;
            let tx = state
                .control
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut r = read(&tx, record.view.id)?;
            r.active = true;
            r.view.revision += 1;
            diagnostic(
                &mut r,
                "process_unknown",
                "merge_local_cleanup_failed",
                "Process stopped but guard/temp cleanup failed; exact local reconciliation is required",
            );
            write(&tx, &r, "cleanup_failed")?;
            tx.commit()?;
        }
        Ok(true)
    }
    pub fn merge_worker(&self) {
        while !self.shutdown.load(Ordering::Acquire) {
            match self.merge_work_once() {
                Ok(true) => (),
                Ok(false) => std::thread::sleep(Duration::from_millis(200)),
                Err(e) => {
                    eprintln!("merge worker: {e}");
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
    }
    /// Local operator-only ownership recovery. Does not grant another mutation.
    /// A live supervisor-inherited flock always prevents this operation.
    pub fn confirm_merge_stopped(
        &self,
        id: i64,
        attempt: u64,
        phase: &str,
        attest: bool,
    ) -> Result<MergeAuthorization> {
        if !attest
            || id <= 0
            || attempt == 0
            || !matches!(phase, "preflight" | "ready" | "merge" | "reconcile")
        {
            return Err(Error::Invalid(
                "exact authorization/attempt/phase and stopped attestation required".into(),
            ));
        }
        let mut state = self.state.lock().map_err(|_| Error::Poisoned)?;
        let tx = state
            .control
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.ci_validate_storage(&tx)?;
        let mut r = read(&tx, id)?;
        let path = self.merge_guard_path();
        let file = ci_tracking::open_marker(&path)?;
        if let Some(file) = &file {
            ci_tracking::flock(file).map_err(|_|unavailable("merge_process_live","The exact merge guard is still locked by a live observer/supervisor; stopped attestation refused"))?;
            let m = marker_contents(file)?;
            if m.database != self.ci_database_identity
                || m.authorization_id != id
                || m.attempt != attempt
                || m.phase != phase
                || !ci_tracking::matches_file(&path, file)
                || (attempt != r.view.attempt && !(attempt == r.view.attempt + 1 && !r.active))
                || (attempt == r.view.attempt && phase != r.phase)
            {
                return Err(unavailable(
                    "merge_recovery_identity_mismatch",
                    "Stopped confirmation does not match the exact durable database/authorization/attempt/phase guard",
                ));
            }
        } else if attempt != r.view.attempt || phase != r.phase || !r.active {
            return Err(unavailable(
                "merge_recovery_identity_mismatch",
                "No matching retained guard or active durable attempt exists",
            ));
        }
        let workspace = self.merge_workspace(id, attempt);
        let receipt = load_receipt(&workspace)?;
        let original_attempt = r.view.attempt;
        r.view.attempt = attempt;
        r.phase = phase.into();
        r.active = false;
        r.readonly_reconciliation = true;
        r.view.observation_only = true;
        r.next_operation = None;
        if let Some(receipt) = receipt {
            if receipt.database != self.ci_database_identity
                || receipt.authorization_id != id
                || receipt.attempt != attempt
                || receipt.phase != phase
                || receipt.consent_sha256 != hash(&r.request)
                || receipt.gate_identity != r.gate_identity
            {
                return Err(unavailable(
                    "merge_receipt_identity_mismatch",
                    "The durable response receipt does not match this exact immutable consent and attempt",
                ));
            }
            // The operator has established local cleanup. A previously validated server
            // result remains valid even when it arrived after expiry/revocation.
            apply_response(
                &mut r,
                Outcome::Success,
                receipt.response.as_ref(),
                receipt.finished_at,
            );
        } else if r.completed_response_attempt == Some(attempt) {
            let response = r
                .view
                .latest_evidence
                .clone()
                .and_then(|v| serde_json::from_value::<AdapterResponse>(v).ok());
            apply_response(&mut r, Outcome::Success, response.as_ref(), now()?);
        } else if matches!(phase, "ready" | "merge") && original_attempt == attempt {
            diagnostic(
                &mut r,
                "effect_unknown",
                "merge_operator_stopped_effect_unknown",
                "Operator confirmed local process tree stopped, but no complete effect receipt exists. Do not repeat the mutation; reconcile read-only",
            );
        } else {
            diagnostic(
                &mut r,
                "blocked",
                "merge_operator_confirmed_stopped",
                "Operator confirmed local cleanup; retained history is unchanged and any continuation is read-only",
            );
        }
        let stopped_gate = self.merge_gate_started(&r).ok().flatten();
        apply_no_write_proof(&mut r, stopped_gate, now()?);
        if !matches!(r.view.status.as_str(), "accepted" | "pending") {
            r.next_operation = None;
        }
        r.view.revision += 1;
        write(&tx, &r, "operator_confirmed_stopped")?;
        tx.commit()?;
        if let Some(file) = file {
            if !ci_tracking::matches_file(&path, &file) {
                return Err(Error::Invalid("merge guard changed during recovery".into()));
            }
            fs::remove_file(&path)
                .and_then(|_| ci_tracking::sync_parent(&path))
                .map_err(invalid_io)?;
        }
        // Bounded failed-attempt evidence is retained for the operator; a recovered
        // authorization never dispatches another ready or merge mutation.
        Ok(r.view)
    }
}

fn previous_scope(
    db: &Connection,
    repository: &str,
    pr_number: u64,
    repository_id: Option<u64>,
    pr_id: Option<u64>,
) -> Result<Option<Record>> {
    let mut q=db.prepare("SELECT authorization_id FROM app_merge_scopes WHERE (repository=?1 AND pr_number=?2) OR (repository_id=?3 AND pr_id=?4) LIMIT 2")?;
    let ids = q
        .query_map(params![repository, pr_number, repository_id, pr_id], |r| {
            r.get::<_, i64>(0)
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if ids.len() > 1 {
        return Err(unavailable(
            "merge_scope_ambiguous",
            "Conflicting numeric and path reservations exist; preserve all effects before operator reconciliation",
        ));
    }
    ids.first().map(|id| read(db, *id)).transpose()
}
fn no_unresolved_effect(r: &Record) -> bool {
    (!r.view.ready_dispatched || r.view.ready_confirmed || r.view.ready_write_not_started)
        && (!r.view.merge_dispatched || r.view.merge_write_not_started)
        && r.view.async_request.is_none()
}
fn safe_to_replace(r: &Record) -> bool {
    !r.active
        && ((matches!(r.view.status.as_str(), "revoked" | "expired") && no_unresolved_effect(r))
            || (r.view.status == "failed"
                && r.view.merge_failed_confirmed
                && r.view.async_request.is_some()))
}

/// A host-verified, phase-bound gate plus confirmed local cleanup proves the
/// attempted phase never reached HTTP dispatch. Preserve dispatch history, but
/// do not strand a safely revoked/expired target as an unknown external effect.
fn apply_no_write_proof(r: &mut Record, write_started: Option<bool>, at: u64) {
    if write_started == Some(false) {
        if r.phase == "ready" && !r.view.ready_confirmed {
            r.view.ready_write_not_started = true;
        }
        if r.phase == "merge"
            && r.view.async_request.is_none()
            && !matches!(
                r.view.observed_remote_status.as_deref(),
                Some(
                    "accepted" | "pending" | "merged" | "externally_merged" | "enqueued" | "failed"
                )
            )
        {
            r.view.merge_write_not_started = true;
        }
    }
    if no_unresolved_effect(r)
        && !matches!(r.view.status.as_str(), "merged" | "externally_merged")
        && (r.view.revoked || at >= r.view.deadline)
    {
        let revoked = r.view.revoked;
        diagnostic(
            r,
            if revoked { "revoked" } else { "expired" },
            "merge_no_unresolved_effect",
            "The authorization ended and the host proved no unresolved remote effect remains; fresh linked consent is required for any new work",
        );
    }
}
