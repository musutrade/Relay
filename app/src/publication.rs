//! Explicit, bounded authorization to publish an already approved candidate.
//! This is an immutable app continuation, never a development retry or test cache.
use crate::{
    Application, Error, Result, StateData, action_unavailable,
    host::{HostConfig, Job, Outcome, RunResult},
    workspaces,
};
use relay::{State, Task};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const AUTHORIZATION_TTL_SECONDS: u64 = 24 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishApprovedRequest {
    pub key: String,
    pub confirm_publish: bool,
    pub accept_prior_test_evidence: bool,
    pub candidate_sha: String,
    pub github_repository: String,
    pub base_branch: String,
    pub draft_pr_adapter: String,
    pub publisher_binding: String,
}
impl PublishApprovedRequest {
    fn validate(&self) -> Result<()> {
        if self.key.is_empty() || self.key.len() > 128 {
            return Err(Error::Invalid(
                "publication key must contain 1-128 UTF-8 bytes".into(),
            ));
        }
        if !self.confirm_publish || !self.accept_prior_test_evidence {
            return Err(Error::Invalid("publish-approved requires confirm_publish=true and accept_prior_test_evidence=true: the original host tests will not rerun and their external inputs are not frozen".into()));
        }
        if self.candidate_sha.len() > 64
            || self.github_repository.len() > 256
            || self.base_branch.len() > 128
            || self.draft_pr_adapter.len() > 128
            || self.publisher_binding.len() > 128
        {
            return Err(Error::Invalid(
                "publication authorization scope exceeds its bounds".into(),
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedPublication {
    pub request: PublishApprovedRequest,
    pub base_sha: String,
    pub dry_run: bool,
    /// Exact immutable core-result bytes, including original tests and review audit.
    pub predecessor_result: String,
    pub predecessor_result_sha256: String,
    /// Exact on-disk checkpoint bytes are also frozen, not only parsed semantics.
    pub checkpoint_sha256: String,
    pub expires_at_unix_seconds: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicationReceipt {
    pub provenance: String,
    pub predecessor_task_id: i64,
    pub predecessor_result_sha256: String,
    pub accepted_prior_test_evidence: bool,
}
pub(crate) fn current(job: &Job) -> Option<&PinnedPublication> {
    job.continuation.as_ref()?.publish_approved.as_ref()
}
pub(crate) fn now() -> io::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_secs())
        .map_err(io::Error::other)
}
pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
/// Current workflow publisher scope for a NEW explicit authorization. Legacy
/// publish:false workspaces never authorized or froze this command. The fresh
/// preview and handoff do freeze it, and subsequent drift fails closed. Program
/// metadata is checked; arbitrary script/dependency bytes in args are not frozen.
pub(crate) fn publisher_policy(config: &HostConfig, job: &Job) -> Value {
    use std::os::unix::fs::MetadataExt;
    let name = job
        .workflow
        .as_ref()
        .and_then(|name| config.workflows.get(name))
        .and_then(|workflow| workflow.draft_pr_adapter.as_ref())
        .or(job.draft_pr_adapter.as_ref());
    let profile = name.and_then(|name| config.draft_pr_adapters.get(name));
    let Some(profile) = profile else {
        return Value::Null;
    };
    let executable = std::fs::metadata(&profile.program).ok().map(|metadata| {
        json!({
            "canonical":profile.program.canonicalize().ok(), "device":metadata.dev(),
            "inode":metadata.ino(), "length":metadata.len(), "modified":metadata.mtime(),
            "modified_nanos":metadata.mtime_nsec(), "changed":metadata.ctime(),
            "changed_nanos":metadata.ctime_nsec(), "mode":metadata.mode()
        })
    });
    json!({"name":name,"profile":profile,"executable":executable})
}
fn scope(config: &HostConfig, job: &Job, result: &RunResult) -> Result<Value> {
    if job.publish || current(job).is_some() {
        return Err(action_unavailable(
            "publish_approved_not_eligible",
            "only an unpublished approved predecessor can authorize publication",
        ));
    }
    let name = job
        .workflow
        .as_ref()
        .ok_or_else(|| Error::Invalid("publish-approved requires a workflow".into()))?;
    let workflow = config
        .workflows
        .get(name)
        .ok_or_else(|| Error::Invalid("workflow configuration is missing".into()))?;
    let (base, candidate) = crate::workflow::approved_publication_candidate(result)
        .map_err(|cause| action_unavailable("publish_approved_not_eligible", cause))?;
    if result
        .workflow
        .as_ref()
        .is_none_or(|result| result.name != *name)
    {
        return Err(Error::Invalid(
            "approved workflow identity differs from the job".into(),
        ));
    }
    let adapter = workflow.draft_pr_adapter.as_ref().ok_or_else(|| {
        action_unavailable(
            "publication_target_missing",
            "the original workflow has no pinned draft publisher",
        )
    })?;
    let repository = workflow.github_repository.as_ref().ok_or_else(|| {
        action_unavailable(
            "publication_target_missing",
            "the original workflow has no pinned GitHub repository",
        )
    })?;
    let profile = config
        .draft_pr_adapters
        .get(adapter)
        .ok_or_else(|| Error::Invalid("draft publisher configuration is missing".into()))?;
    let binding = digest(&serde_json::to_vec(&json!({"workflow":workflow,"publisher":publisher_policy(config,job),"workspace_config":workspaces::config_binding(config,job).map_err(|e|Error::Invalid(e.to_string()))?})).expect("serializable publisher scope"));
    Ok(
        json!({"id":"publish_approved","allowed":true,"ordinary_allowed":true,
        "candidate_sha":candidate,"base_sha":base,"github_repository":repository,
        "base_branch":workflow.base_branch,"draft_pr_adapter":adapter,"publisher_binding":binding,
        "draft":true,"dry_run":profile.env.get("RELAY_GITHUB_EXECUTE").is_none_or(|value|value!="1"),
        "requires_prior_test_acceptance":true,"authorization_ttl_seconds":AUTHORIZATION_TTL_SECONDS,
        "expires_at_unix_seconds":null}),
    )
}
fn matches_scope(input: &PublishApprovedRequest, scope: &Value) -> bool {
    scope["candidate_sha"] == input.candidate_sha
        && scope["github_repository"] == input.github_repository
        && scope["base_branch"] == input.base_branch
        && scope["draft_pr_adapter"] == input.draft_pr_adapter
        && scope["publisher_binding"] == input.publisher_binding
}
fn checkpoint(path: &Path, raw: &str) -> Result<String> {
    let text = workspaces::read_bounded_record(
        &path.join("last-result.json"),
        relay::MAX_RESULT_BYTES as u64,
    )
    .map_err(|e| action_unavailable("approved_result_unverified", e.to_string()))?;
    let parsed =
        |text: &str| serde_json::from_str::<Value>(text).map_err(|e| Error::Invalid(e.to_string()));
    if parsed(&text)? != parsed(raw)? {
        return Err(action_unavailable(
            "approved_result_unverified",
            "retained host result differs from the immutable approved predecessor",
        ));
    }
    Ok(digest(text.as_bytes()))
}
fn preflight(
    config: &HostConfig,
    task: &Task,
    job: &Job,
) -> Result<(Value, workspaces::Continuation, String)> {
    if task.state != State::Finished {
        return Err(action_unavailable(
            "publish_approved_not_eligible",
            "publication requires a durably finished successful audited task",
        ));
    }
    let raw = task
        .result
        .as_deref()
        .ok_or_else(|| Error::Invalid("approved result missing".into()))?;
    let result: RunResult = serde_json::from_str(raw).map_err(|e| Error::Invalid(e.to_string()))?;
    let scope = scope(config, job, &result)?;
    let path = workspaces::root(config, task, job);
    if result.workspace.as_ref() != Some(&path) {
        return Err(action_unavailable(
            "retained_workspace_unverified",
            "approved result does not identify the retained workspace",
        ));
    }
    let continuation = workspaces::continuation_under_lease(config, task, job)
        .map_err(|e| action_unavailable("workspace_recovery_unavailable", e.to_string()))?;
    workspaces::verify_candidate_checkpoint(config, job, &path)
        .map_err(|e| action_unavailable("candidate_checkpoint_changed", e.to_string()))?;
    let checkpoint_digest = checkpoint(&path, raw)?;
    let base: String = serde_json::from_str(
        &workspaces::read_marker(&path.join("workflow-base.txt"))
            .map_err(|e| Error::Invalid(e.to_string()))?,
    )
    .map_err(|e| Error::Invalid(e.to_string()))?;
    let candidate: String = serde_json::from_str(
        &workspaces::read_marker(&path.join("candidate-head.json"))
            .map_err(|e| Error::Invalid(e.to_string()))?,
    )
    .map_err(|e| Error::Invalid(e.to_string()))?;
    if scope["base_sha"] != base || scope["candidate_sha"] != candidate {
        return Err(action_unavailable(
            "candidate_checkpoint_changed",
            "approved audit differs from the retained candidate/base checkpoints",
        ));
    }
    Ok((scope, continuation, checkpoint_digest))
}
impl Application {
    pub fn publish_approved(&self, id: i64, input: PublishApprovedRequest) -> Result<Task> {
        input.validate()?;
        let mut state = self.state.lock().map_err(|_| Error::Poisoned)?;
        let StateData { store, control, .. } = &mut *state;
        let predecessor = store.get(id)?;
        let tx = control.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let reservation: Option<(String, String)> = tx
            .query_row(
                "SELECT key,payload FROM app_continuations WHERE predecessor_id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        // Retain the lease through the reservation commit. Cleanup takes the same
        // immediate transaction before its workspace lease, so neither can win a gap.
        let mut workspace_lease = None;
        let (key, payload) = if let Some((key, payload)) = reservation {
            let reserved: Job =
                serde_json::from_str(&payload).map_err(|e| Error::Invalid(e.to_string()))?;
            if key != input.key || current(&reserved).is_none_or(|pinned| pinned.request != input) {
                return Err(action_unavailable(
                    "publication_reservation_conflict",
                    "predecessor already has a different immutable successor reservation",
                ));
            }
            (key, payload)
        } else {
            let mut job = Job::from_payload(&predecessor.payload, self.host.config())
                .map_err(|e| Error::Invalid(e.to_string()))?;
            let path = workspaces::root(self.host.config(), &predecessor, &job);
            workspace_lease = Some(workspaces::lock(&path).map_err(|e| {
                action_unavailable("workspace_recovery_unavailable", e.to_string())
            })?);
            let (scope, mut continuation, checkpoint_sha256) =
                preflight(self.host.config(), &predecessor, &job)?;
            if !matches_scope(&input, &scope) {
                return Err(action_unavailable(
                    "publication_scope_changed",
                    "the exact candidate, target or publisher binding differs from the confirmed preview; refresh and inspect it again",
                ));
            }
            let predecessor_result = predecessor.result.clone().expect("verified result");
            continuation.publish_approved = Some(PinnedPublication {
                request: input.clone(),
                base_sha: scope["base_sha"].as_str().expect("scope").into(),
                dry_run: scope["dry_run"].as_bool().expect("scope"),
                predecessor_result_sha256: digest(predecessor_result.as_bytes()),
                predecessor_result,
                checkpoint_sha256,
                expires_at_unix_seconds: now()
                    .map_err(|e| Error::Invalid(e.to_string()))?
                    .checked_add(AUTHORIZATION_TTL_SECONDS)
                    .ok_or_else(|| Error::Invalid("authorization expiry overflow".into()))?,
            });
            job.continuation = Some(continuation);
            let payload = serde_json::to_string(&job).map_err(|e| Error::Invalid(e.to_string()))?;
            if payload.len() > relay::MAX_PAYLOAD_BYTES {
                return Err(Error::Invalid("publication handoff exceeds 64 KiB; the original evidence cannot be safely frozen".into()));
            }
            let conflicting: Option<String> = tx
                .query_row(
                    "SELECT payload FROM tasks WHERE key=?1",
                    [&input.key],
                    |row| row.get(0),
                )
                .optional()?;
            if conflicting.is_some_and(|old| old != payload) {
                return Err(relay::Error::IdempotencyConflict.into());
            }
            tx.execute(
                "INSERT INTO app_continuations(predecessor_id,key,payload) VALUES (?1,?2,?3)",
                params![id, input.key, payload],
            )?;
            (input.key, payload)
        };
        tx.commit()?;
        drop(workspace_lease);
        let task = match store.submit(&key, &payload) {
            Ok(task) => task,
            Err(error) => {
                if matches!(
                    error,
                    relay::Error::IdempotencyConflict | relay::Error::Invalid(_)
                ) {
                    // A different submission can win the key between reservation
                    // commit and core submit. Release only a proven unmaterialized
                    // reservation, never one whose exact successor already exists.
                    control.execute(
                        "DELETE FROM app_continuations WHERE predecessor_id=?1 AND key=?2 AND payload=?3 AND task_id IS NULL AND NOT EXISTS(SELECT 1 FROM tasks WHERE key=?2 AND payload=?3)",
                        params![id,key,payload],
                    )?;
                }
                return Err(error.into());
            }
        };
        control.execute(
            "UPDATE app_continuations SET task_id=?2 WHERE predecessor_id=?1",
            params![id, task.id],
        )?;
        Ok(task)
    }
}

/// Return a publication-specific operator view only for relevant successful
/// predecessors or an already frozen publication reservation.
pub(crate) fn operator(
    app: &Application,
    task: &Task,
    successor: Option<i64>,
    reservation: Option<&(String, String)>,
) -> Option<(Vec<Value>, Value, Option<String>)> {
    let reserved_job =
        reservation.and_then(|(_, payload)| serde_json::from_str::<Job>(payload).ok());
    let pinned = reserved_job.as_ref().and_then(current);
    let result = task
        .result
        .as_deref()
        .and_then(|raw| serde_json::from_str::<RunResult>(raw).ok());
    let job = Job::from_payload(&task.payload, app.host.config()).ok();
    if pinned.is_none()
        && !result
            .as_ref()
            .is_some_and(|result| result.outcome == Outcome::Success && result.workflow.is_some())
    {
        return None;
    }
    if let Some(id) = successor {
        return Some((
            vec![],
            Value::Null,
            Some(format!("immutable successor already created as task #{id}")),
        ));
    }
    if let Some(pinned) = pinned {
        let mut prior_job = reserved_job.as_ref().expect("pinned job").clone();
        prior_job.continuation = None;
        let prior: RunResult = match serde_json::from_str(&pinned.predecessor_result) {
            Ok(value) => value,
            Err(_) => {
                return Some((
                    vec![],
                    Value::Null,
                    Some("reserved approved evidence cannot be verified".into()),
                ));
            }
        };
        // The preview is the frozen authorization scope. Current configuration is
        // rechecked by execution; exact replay never silently creates a new scope.
        let mut action = match scope(app.host.config(), &prior_job, &prior) {
            Ok(action) if matches_scope(&pinned.request, &action)
                && action["dry_run"] == pinned.dry_run => action,
            _ => return Some((vec![],Value::Null,Some("reserved publication target or publisher configuration changed; restore its authorized binding before resuming".into()))),
        };
        if now().map_or(true, |now| now >= pinned.expires_at_unix_seconds) {
            return Some((vec![],Value::Null,Some("reserved publication authorization expired after 24 hours; no new authorization or retry is created".into())));
        }
        for key in [
            "candidate_sha",
            "github_repository",
            "base_branch",
            "draft_pr_adapter",
            "publisher_binding",
        ] {
            action[key] = serde_json::to_value(&pinned.request).expect("request")[key].clone();
        }
        action["base_sha"] = json!(pinned.base_sha);
        action["expires_at_unix_seconds"] = json!(pinned.expires_at_unix_seconds);
        let mut request = serde_json::to_value(&pinned.request).expect("request");
        request["action_id"] = json!("publish_approved");
        return Some((vec![action], request, None));
    }
    let Some(job) = job else {
        return Some((
            vec![],
            Value::Null,
            Some("original workflow configuration no longer matches".into()),
        ));
    };
    let path = workspaces::root(app.host.config(), task, &job);
    let view = workspaces::lock(&path)
        .map_err(|e| Error::Invalid(e.to_string()))
        .and_then(|_lease| preflight(app.host.config(), task, &job).map(|(scope, _, _)| scope));
    Some(match view {
        Ok(scope) => (vec![scope], Value::Null, None),
        Err(error) => (vec![], Value::Null, Some(error.to_string())),
    })
}

pub(crate) fn verify_execution(
    config: &HostConfig,
    job: &Job,
    path: &Path,
    pinned: &PinnedPublication,
) -> Result<RunResult> {
    pinned.request.validate()?;
    if now().map_err(|e| Error::Invalid(e.to_string()))? >= pinned.expires_at_unix_seconds {
        return Err(action_unavailable(
            "publication_authorization_expired",
            "publication authorization expired after 24 hours; no publisher ran; inspect and start a new audited task if still needed",
        ));
    }
    if digest(pinned.predecessor_result.as_bytes()) != pinned.predecessor_result_sha256
        || checkpoint(path, &pinned.predecessor_result)? != pinned.checkpoint_sha256
    {
        return Err(action_unavailable(
            "approved_result_unverified",
            "the exact original test and review audit bytes changed",
        ));
    }
    let previous: RunResult = serde_json::from_str(&pinned.predecessor_result)
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let mut original = job.clone();
    original.continuation = None;
    let scope = scope(config, &original, &previous)?;
    if !matches_scope(&pinned.request, &scope)
        || scope["base_sha"] != pinned.base_sha
        || scope["dry_run"] != pinned.dry_run
    {
        return Err(action_unavailable(
            "publication_scope_changed",
            "approved candidate or publisher/config target drifted; publication is blocked",
        ));
    }
    Ok(previous)
}

pub(crate) struct PendingRetention {
    pub protected: bool,
    pub expires_at: u64,
    pub reason: &'static str,
}
/// Called while cleanup holds an immediate transaction, or as a read-only
/// observation. An expired/cancelled queue entry never turns a timeout into
/// evidence that a claimed process stopped.
pub(crate) fn pending_retention(
    control: &rusqlite::Connection,
    predecessor_id: i64,
    now: u64,
) -> io::Result<Option<PendingRetention>> {
    type PendingRow = (String, Option<i64>, Option<i64>, Option<String>, bool);
    let reservation: Option<PendingRow> = control.query_row(
        "SELECT c.payload,c.task_id,t.id,t.state,EXISTS(SELECT 1 FROM app_cancellations x WHERE x.task_id=t.id)
         FROM app_continuations c LEFT JOIN tasks t ON t.key=c.key AND t.payload=c.payload WHERE c.predecessor_id=?1",
        [predecessor_id], |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)))
        .optional().map_err(io::Error::other)?;
    let Some((payload, checkpoint_id, task_id, state, cancelled)) = reservation else {
        return Ok(None);
    };
    let job: Job = serde_json::from_str(&payload).map_err(io::Error::other)?;
    let Some(pinned) = current(&job) else {
        return Ok(None);
    };
    if checkpoint_id.is_some() && checkpoint_id != task_id {
        return Err(io::Error::other(
            "publication successor identity cannot be verified",
        ));
    }
    let claimed = state.as_deref() == Some("claimed");
    let released = !claimed
        && (cancelled
            || state.as_deref() == Some("finished")
            || now >= pinned.expires_at_unix_seconds);
    Ok(Some(PendingRetention {
        protected: !released,
        expires_at: pinned.expires_at_unix_seconds,
        reason: if claimed {
            "publication successor has claimed or unknown execution; authorization expiry/cancellation cannot prove it stopped"
        } else if released {
            "publication reservation expired, was cancelled while queued, or finished before workspace acquisition; original success TTL applies"
        } else {
            "pending publish-approved successor protects this workspace until its fixed 24-hour authorization expiry"
        },
    }))
}
