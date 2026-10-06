//! Application adapters around Relay's opaque durable core.
//! All direct database callers belong to the same trusted local OS account.
mod app_server;
pub mod auth;
pub mod capabilities;
mod catalog_cache;
mod claude_control;
mod git_inventory;
pub mod host;
pub mod http;
pub mod mcp;
pub mod providers;
pub mod replacement;
pub mod resources;
pub mod selection;
mod sessions;
pub mod workflow;
mod workspaces;

use host::{Host, HostConfig, Job};
use relay::{Store, Task};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Core(#[from] relay::Error),
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    DiscoveryUnavailable(String),
    #[error("{cause}")]
    ActionUnavailable { code: String, cause: String },
    #[error(
        "task execution is unknown after restart; confirm the old process tree has stopped before local recovery"
    )]
    RecoveryRequired,
    #[error("internal state unavailable")]
    Poisoned,
}
pub type Result<T> = std::result::Result<T, Error>;

/// One explicit operator attestation. It is not a credential or a persistent permission.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogRefreshRequest {
    #[serde(default)]
    pub confirm_startup_effects: bool,
    #[serde(default)]
    pub confirmation_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Submission {
    #[serde(default)]
    pub permission_challenge: Option<String>,
    pub key: String,
    pub job: Job,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryRequest {
    #[serde(default)]
    pub replacement: Option<selection::RoleSelection>,
    #[serde(default)]
    pub permission_challenge: Option<String>,
    #[serde(default)]
    pub workspace_quota_bytes: Option<u64>,
    pub key: String,
    pub confirm_stopped_and_reconciled: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewContinuationRequest {
    #[serde(default)]
    pub replacement: Option<selection::RoleSelection>,
    #[serde(default)]
    pub permission_challenge: Option<String>,
    #[serde(default)]
    pub workspace_quota_bytes: Option<u64>,
    pub key: String,
    pub confirm_stopped_and_reconciled: bool,
    pub revalidate_tests: bool,
    #[serde(default)]
    pub review_focus: Option<String>,
}

/// Local operator acceptance of one complete, previously inspected response.
/// This is not a new model run or automatic reuse of test evidence.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewAdoptionRequest {
    pub key: String,
    pub confirm_stopped_and_reconciled: bool,
    pub adoption: workflow::ReviewAdoption,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplacementChallengeRequest {
    pub action: String,
    pub replacement: selection::RoleSelection,
}

/// Adapter-owned continuation metadata; the core task and result stay immutable.
#[derive(Serialize)]
pub struct TaskView {
    #[serde(flatten)]
    pub task: Task,
    pub continuation_status: Option<ContinuationStatus>,
}
#[derive(Serialize)]
pub struct ContinuationStatus {
    /// None means a durable reservation still needs its idempotent submission retried.
    pub successor_id: Option<i64>,
}

fn task_view(control: &Connection, task: Task) -> Result<TaskView> {
    // Resolve a submission committed before the task_id checkpoint without writing
    // on reads. Match both immutable key and payload, never just a caller's key.
    let continuation_status = control
        .query_row(
            "SELECT COALESCE(c.task_id, t.id) FROM app_continuations c
             LEFT JOIN tasks t ON t.key=c.key AND t.payload=c.payload
             WHERE c.predecessor_id=?1",
            [task.id],
            |row| {
                Ok(ContinuationStatus {
                    successor_id: row.get(0)?,
                })
            },
        )
        .optional()?;
    Ok(TaskView {
        task,
        continuation_status,
    })
}

struct StateData {
    store: Store,
    control: Connection,
    running: Option<(relay::Claim, Arc<AtomicBool>)>,
}
pub struct Application {
    permission_challenges: Mutex<selection::PermissionChallenges>,
    resource_measurements:
        Mutex<std::collections::BTreeMap<(i64, i64), (std::time::Instant, resources::Usage)>>,
    catalogs: Mutex<catalog_cache::CatalogCache>,
    state: Mutex<StateData>,
    pub host: Host,
    pub config: HostConfig,
    pub shutdown: AtomicBool,
}
impl Application {
    pub fn open(db: impl AsRef<Path>, config: HostConfig) -> Result<Arc<Self>> {
        let host = Host::new(config.clone()).map_err(|e| Error::Invalid(e.to_string()))?;
        let store = Store::open(&db)?;
        let control = Connection::open(&db)?;
        control.busy_timeout(Duration::from_secs(5))?;
        // Adapter-owned metadata. It does not change the core queue state machine.
        control.execute_batch("CREATE TABLE IF NOT EXISTS app_continuations(predecessor_id INTEGER PRIMARY KEY, key TEXT NOT NULL, payload TEXT NOT NULL, task_id INTEGER); CREATE TABLE IF NOT EXISTS app_cancellations(task_id INTEGER PRIMARY KEY REFERENCES tasks(id)); CREATE TABLE IF NOT EXISTS app_diagnostics(task_id INTEGER PRIMARY KEY, generation INTEGER NOT NULL, result TEXT NOT NULL);")?;
        Ok(Arc::new(Self {
            permission_challenges: Mutex::new(selection::PermissionChallenges::default()),
            resource_measurements: Mutex::new(std::collections::BTreeMap::new()),
            catalogs: Mutex::new(catalog_cache::CatalogCache::default()),
            state: Mutex::new(StateData {
                store,
                control,
                running: None,
            }),
            host,
            config,
            shutdown: AtomicBool::new(false),
        }))
    }
    pub fn permission_challenge(&self, mut job: Job) -> Result<Value> {
        if job.continuation.is_some() || job.role_binding.is_some() || job.role_epochs.is_some() {
            return Err(Error::Invalid(
                "permission challenges are only for new user-authored selections".into(),
            ));
        }
        if job.requirements.trim().is_empty() {
            job.requirements = "Permission scope preview".into();
        }
        // This only previews a scope. The actual submission still requires its
        // own explicit attestation and the issued, unexpired challenge.
        if let Some(roles) = &mut job.role_selections {
            for role in [&mut roles.developer, &mut roles.reviewer]
                .into_iter()
                .flatten()
            {
                role.confirm_permission_expansion = Some(true);
            }
        }
        job.validate(self.host.config())
            .map_err(|e| Error::Invalid(e.to_string()))?;
        self.permission_challenges
            .lock()
            .map_err(|_| Error::Poisoned)?
            .issue(&job, self.host.config())
            .map_err(Error::Invalid)
    }
    pub fn replacement_challenge(
        &self,
        id: i64,
        mut input: ReplacementChallengeRequest,
    ) -> Result<Value> {
        let reviewer = match input.action.as_str() {
            "retry" => false,
            "continue_review" => true,
            _ => {
                return Err(Error::Invalid(
                    "replacement action must be retry or continue_review".into(),
                ));
            }
        };
        let state = self.state.lock().map_err(|_| Error::Poisoned)?;
        let task = state.store.get(id)?;
        if task.state != relay::State::Finished {
            return Err(Error::RecoveryRequired);
        }
        let job = Job::from_payload(&task.payload, self.host.config())
            .map_err(|e| Error::Invalid(e.to_string()))?;
        let result: host::RunResult = serde_json::from_str(task.result.as_deref().unwrap_or(""))
            .map_err(|_| Error::Invalid("missing stopped result".into()))?;
        let proof = replacement::stage(&result, &job, self.host.config(), reviewer)
            .map_err(Error::Invalid)?;
        let path = workspaces::root(self.host.config(), &task, &job);
        let _lease = workspaces::lock(&path).map_err(|e| Error::Invalid(e.to_string()))?;
        workspaces::continuation_under_lease(self.host.config(), &task, &job)
            .map_err(|e| Error::Invalid(e.to_string()))?;
        replacement::verify_stage_checkpoint(&proof, &path)
            .map_err(|e| Error::Invalid(e.to_string()))?;
        workspaces::verify_candidate_checkpoint(self.host.config(), &job, &path)
            .map_err(|e| Error::Invalid(e.to_string()))?;
        input.replacement.confirm_permission_expansion = Some(true);
        let next = replacement::compose(&job, &input.replacement, reviewer, self.host.config())
            .map_err(|e| Error::Invalid(e.to_string()))?;
        if !selection::native_profile(&next, self.host.config(), reviewer)
            .map_err(|e| Error::Invalid(e.to_string()))?
            .and_then(|p| p.native_permission)
            .is_some_and(providers::NativePermission::requires_confirmation)
        {
            return Err(Error::Invalid(
                "changed role does not require a permission-expansion challenge".into(),
            ));
        }
        self.permission_challenges
            .lock()
            .map_err(|_| Error::Poisoned)?
            .issue_replacement(&next, self.host.config(), id, reviewer)
            .map_err(Error::Invalid)
    }
    pub fn submit(&self, mut input: Submission) -> Result<Task> {
        if input.job.continuation.is_some()
            || input.job.role_binding.is_some()
            || input.job.role_epochs.is_some()
        {
            return Err(Error::Invalid(
                "use explicit continuation endpoints; role_binding is server-owned".into(),
            ));
        }
        let requested =
            serde_json::to_string(&input.job).map_err(|e| Error::Invalid(e.to_string()))?;
        let mut state = self.state.lock().map_err(|_| Error::Poisoned)?;
        let existing: Option<String> = state
            .control
            .query_row(
                "SELECT payload FROM tasks WHERE key=?1",
                [&input.key],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(payload) = existing {
            let original_request =
                serde_json::from_str::<Job>(&payload)
                    .ok()
                    .and_then(|mut job| {
                        if let Some(reference) = job
                            .role_binding
                            .as_ref()
                            .and_then(|binding| binding.acceptance_reference.as_ref())
                        {
                            let supplied = input
                                .permission_challenge
                                .as_deref()
                                .and_then(|token| selection::acceptance_reference(token).ok());
                            if supplied.as_ref() != Some(reference) {
                                return None;
                            }
                        }
                        job.role_binding = None;
                        serde_json::to_string(&job).ok()
                    });
            if payload == requested || original_request.as_deref() == Some(&requested) {
                return state.store.submit(&input.key, &payload).map_err(Into::into);
            }
            return Err(Error::Core(relay::Error::IdempotencyConflict));
        }
        input
            .job
            .validate(self.host.config())
            .map_err(|e| Error::Invalid(e.to_string()))?;
        if let Some(roles) = &input.job.role_selections {
            let mut cache = self.catalogs.lock().map_err(|_| Error::Poisoned)?;
            for selection in [&roles.developer, &roles.reviewer].into_iter().flatten() {
                if let Some(profile) = self.config.native_agents.get(&selection.profile) {
                    let view = cache.view(&selection.profile, profile);
                    selection::validate_catalog(selection, &view).map_err(Error::Invalid)?;
                }
            }
        }
        let expanded = selection::needs_confirmation(&input.job, self.host.config());
        let mut challenges = self
            .permission_challenges
            .lock()
            .map_err(|_| Error::Poisoned)?;
        if expanded {
            challenges
                .validate(
                    input.permission_challenge.as_deref(),
                    &input.job,
                    self.host.config(),
                )
                .map_err(Error::Invalid)?;
        }
        if input.job.role_selections.is_some() {
            input.job.role_binding = Some(
                selection::binding(&input.job, self.host.config())
                    .map_err(|e| Error::Invalid(e.to_string()))?,
            );
        }
        if expanded {
            input
                .job
                .role_binding
                .as_mut()
                .expect("expanded role binding")
                .acceptance_reference = Some(
                selection::acceptance_reference(
                    input
                        .permission_challenge
                        .as_deref()
                        .expect("validated challenge"),
                )
                .map_err(Error::Invalid)?,
            );
        }
        let payload =
            serde_json::to_string(&input.job).map_err(|e| Error::Invalid(e.to_string()))?;
        let task = state.store.submit(&input.key, &payload)?;
        if expanded {
            challenges.consume(
                input
                    .permission_challenge
                    .as_deref()
                    .expect("validated challenge"),
            );
        }
        Ok(task)
    }
    /// One explicit successor per predecessor. Reservation precedes core submission,
    /// and its stable key/payload make a crash between the two operations retryable.
    pub fn retry(&self, id: i64, input: RetryRequest) -> Result<Task> {
        self.continue_task(id, input, None, None)
    }
    /// Explicit operator action: preserve the candidate, revalidate host tests once,
    /// and resume only the compatible reviewer. No developer or repair phase runs.
    pub fn continue_review(&self, id: i64, input: ReviewContinuationRequest) -> Result<Task> {
        if !input.revalidate_tests {
            return Err(Error::Invalid("review-only continuation requires revalidate_tests=true: prior results do not bind all external test inputs; rerun the configured host tests once without redevelopment".into()));
        }
        workflow::validate_review_focus(input.review_focus.as_deref()).map_err(Error::Invalid)?;
        self.continue_task(
            id,
            RetryRequest {
                replacement: input.replacement,
                permission_challenge: input.permission_challenge,
                workspace_quota_bytes: input.workspace_quota_bytes,
                key: input.key,
                confirm_stopped_and_reconciled: input.confirm_stopped_and_reconciled,
            },
            Some(input.review_focus),
            None,
        )
    }
    pub fn adopt_review(&self, id: i64, input: ReviewAdoptionRequest) -> Result<Task> {
        input.adoption.validate().map_err(Error::Invalid)?;
        self.continue_task(
            id,
            RetryRequest {
                replacement: None,
                permission_challenge: None,
                workspace_quota_bytes: None,
                key: input.key,
                confirm_stopped_and_reconciled: input.confirm_stopped_and_reconciled,
            },
            None,
            Some(input.adoption),
        )
    }
    fn continue_task(
        &self,
        id: i64,
        input: RetryRequest,
        review_focus: Option<Option<String>>,
        adoption: Option<workflow::ReviewAdoption>,
    ) -> Result<Task> {
        if !input.confirm_stopped_and_reconciled {
            return Err(Error::Invalid("confirm inspection of the stopped run and its possible side effects before continuing".into()));
        }
        if input.key.is_empty() || input.key.len() > 128 {
            return Err(Error::Invalid("retry key must contain 1-128 bytes".into()));
        }
        let mut state = self.state.lock().map_err(|_| Error::Poisoned)?;
        let StateData { store, control, .. } = &mut *state;
        let predecessor = store.get(id)?;
        let tx = control.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let reservation: Option<(String, String)> = tx
            .query_row(
                "SELECT key,payload FROM app_continuations WHERE predecessor_id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (key, payload) = if let Some(reservation) = reservation {
            // An adoption retry must not silently accept a different response,
            // candidate, or an earlier reservation for a different action.
            let reserved: Job =
                serde_json::from_str(&reservation.1).map_err(|e| Error::Invalid(e.to_string()))?;
            if adoption.is_none()
                && reserved
                    .continuation
                    .as_ref()
                    .is_some_and(|c| c.operator_adoption.is_some())
            {
                return Err(Error::Invalid("an operator-adoption reservation must be resumed with the same local adopt-review request".into()));
            }
            if let Some(request) = &adoption
                && reserved
                    .continuation
                    .as_ref()
                    .and_then(|c| c.operator_adoption.as_ref())
                    .map(|pinned| &pinned.request)
                    != Some(request)
            {
                return Err(Error::Invalid(
                    "predecessor already has a different continuation reservation".into(),
                ));
            }
            reservation
        } else {
            if predecessor.state != relay::State::Finished {
                return Err(Error::RecoveryRequired);
            }
            let result: host::RunResult =
                serde_json::from_str(predecessor.result.as_deref().unwrap_or("")).map_err(
                    |_| Error::Invalid("predecessor has no verified stopped host result".into()),
                )?;
            if !matches!(
                result.outcome,
                host::Outcome::Failure | host::Outcome::TimedOut | host::Outcome::Cancelled
            ) {
                return Err(action_unavailable(
                    "continuation_not_eligible",
                    "only stopped unsuccessful tasks can continue",
                ));
            }
            if result.draft_pr.is_some()
                || result.workflow.as_ref().is_some_and(|workflow| {
                    workflow.reconciliation_required || workflow.publication.is_some()
                })
            {
                return Err(action_unavailable(
                    "publication_reconciliation_required",
                    "publication was attempted; local reconciliation is required",
                ));
            }
            let mut job = Job::from_payload(&predecessor.payload, self.host.config())
                .map_err(|e| Error::Invalid(e.to_string()))?;
            let predecessor_job = job.clone();
            let prior_quota = recorded_quota(&result, &job);
            let inherited_quota = job
                .workspace_quota_bytes
                .unwrap_or_else(|| self.config.workspace_byte_limit());
            let path = workspaces::root(self.host.config(), &predecessor, &job);
            let _workspace_lease = workspaces::lock(&path)
                .map_err(|e| action_unavailable("workspace_recovery_unavailable", e.to_string()))?;
            workspaces::verify_candidate_checkpoint(self.host.config(), &job, &path).map_err(
                |error| action_unavailable("candidate_checkpoint_changed", error.to_string()),
            )?;
            if result.workspace.as_ref() != Some(&path) {
                return Err(action_unavailable(
                    "retained_workspace_unverified",
                    "the stopped result does not retain a matching workspace identity",
                ));
            }
            let usage = resources::measure_workspace(&path, self.host.config());
            let eligibility = resource_eligibility(
                &result,
                &usage,
                prior_quota,
                inherited_quota,
                self.config.workspace_byte_limit(),
            );
            if let Some(requested) = input.workspace_quota_bytes {
                if prior_quota.is_none_or(|prior| requested <= prior)
                    || requested > self.config.workspace_byte_limit()
                {
                    return Err(action_unavailable(
                        "invalid_workspace_quota_increase",
                        "workspace quota must strictly increase the predecessor quota and remain within the host policy cap",
                    ));
                }
                if eligibility
                    .increase_min
                    .is_none_or(|minimum| requested < minimum)
                {
                    return Err(action_unavailable("workspace_quota_increase_unavailable", eligibility.reason.unwrap_or_else(|| "a quota increase requires complete current usage, matching stopped quota proof and host-policy headroom".into())));
                }
            } else if !eligibility.same_quota {
                return Err(action_unavailable("workspace_quota_increase_required", eligibility.reason.unwrap_or_else(|| "explicitly choose an eligible higher quota before continuing the retained workspace".into())));
            }
            let prior_review_focus = job
                .continuation
                .as_ref()
                .and_then(|c| c.review_only.as_ref())
                .and_then(|c| c.review_focus.clone());
            job.continuation = Some(
                workspaces::continuation_under_lease(self.host.config(), &predecessor, &job)
                    .map_err(|error| {
                        action_unavailable("workspace_recovery_unavailable", error.to_string())
                    })?,
            );
            if let Some(requested) = input.workspace_quota_bytes {
                job.workspace_quota_bytes = Some(requested);
                job.continuation
                    .as_mut()
                    .expect("continuation recorded")
                    .quota_increase = Some(workspaces::QuotaIncrease {
                    previous_bytes: prior_quota.expect("validated previous quota"),
                    new_bytes: requested,
                });
                // Validate the exact override against the host-owned predecessor
                // record before reserving it, then repeat the guard at execution.
                workspaces::continuation_under_lease(self.host.config(), &predecessor, &job)
                    .map_err(|error| {
                        action_unavailable("predecessor_quota_unverified", error.to_string())
                    })?;
            }
            if let Some(focus) = review_focus {
                (if input.replacement.is_some() {
                    workspaces::verify_review_candidate_checkpoint(
                        self.host.config(),
                        &job,
                        &result,
                        &path,
                    )
                } else {
                    workspaces::verify_review_checkpoint(self.host.config(), &job, &result, &path)
                })
                .map_err(|error| {
                    action_unavailable("review_checkpoint_unverified", error.to_string())
                })?;
                let review = workflow::review_continuation(&result, focus.or(prior_review_focus))
                    .map_err(Error::Invalid)?;
                if job.workflow.is_none() {
                    return Err(Error::Invalid(
                        "review-only continuation requires a workflow".into(),
                    ));
                }
                job.continuation
                    .as_mut()
                    .expect("continuation recorded")
                    .review_only = Some(review);
            }
            if let Some(request) = adoption {
                let pinned =
                    workflow::pin_review_adoption(&result, request).map_err(Error::Invalid)?;
                if job.workflow.is_none() {
                    return Err(Error::Invalid("review adoption requires a workflow".into()));
                }
                job.continuation
                    .as_mut()
                    .expect("continuation recorded")
                    .operator_adoption = Some(pinned);
            }

            if input.replacement.is_none()
                && job
                    .continuation
                    .as_ref()
                    .is_some_and(|c| c.review_only.is_none() && c.operator_adoption.is_none())
                && replacement::requires_stage_retry(&job)
            {
                let stage =
                    replacement::stage(&result, &predecessor_job, self.host.config(), false)
                        .map_err(|e| action_unavailable("preserved_developer_stage_required", e))?;
                replacement::verify_stage_checkpoint(&stage, &path).map_err(|e| {
                    action_unavailable("replacement_stage_unverified", e.to_string())
                })?;
                job.continuation
                    .as_mut()
                    .expect("continuation")
                    .developer_stage = Some(stage);
            }
            if let Some(selected) = &input.replacement {
                let reviewer = job
                    .continuation
                    .as_ref()
                    .is_some_and(|c| c.review_only.is_some());
                let stage =
                    replacement::stage(&result, &predecessor_job, self.host.config(), reviewer)
                        .map_err(|e| action_unavailable("replacement_stage_unsupported", e))?;
                replacement::verify_stage_checkpoint(&stage, &path).map_err(|e| {
                    action_unavailable("replacement_stage_unverified", e.to_string())
                })?;
                let stopped = workspaces::read_stopped_result(&path)
                    .map_err(|e| Error::Invalid(e.to_string()))?;
                if replacement::digest(
                    &serde_json::from_str::<Value>(&stopped.to_json()).expect("host result"),
                )
                .map_err(|e| Error::Invalid(e.to_string()))?
                    != replacement::digest(
                        &serde_json::from_str::<Value>(&result.to_json()).expect("host result"),
                    )
                    .map_err(|e| Error::Invalid(e.to_string()))?
                {
                    return Err(action_unavailable(
                        "replacement_result_unverified",
                        "retained stopped result differs from immutable predecessor",
                    ));
                }
                let mut next = replacement::compose(&job, selected, reviewer, self.host.config())
                    .map_err(|e| Error::Invalid(e.to_string()))?;
                if let Some(profile) = self.config.native_agents.get(&selected.profile) {
                    let view = self
                        .catalogs
                        .lock()
                        .map_err(|_| Error::Poisoned)?
                        .view(&selected.profile, profile);
                    selection::validate_catalog(selected, &view).map_err(Error::Invalid)?;
                }
                let expanded = selection::native_profile(&next, self.host.config(), reviewer)
                    .map_err(|e| Error::Invalid(e.to_string()))?
                    .and_then(|p| p.native_permission)
                    .is_some_and(providers::NativePermission::requires_confirmation);
                if expanded {
                    self.permission_challenges
                        .lock()
                        .map_err(|_| Error::Poisoned)?
                        .validate_replacement(
                            input.permission_challenge.as_deref(),
                            &next,
                            self.host.config(),
                            id,
                            reviewer,
                        )
                        .map_err(Error::Invalid)?;
                    next.role_binding
                        .as_mut()
                        .expect("new role binding")
                        .acceptance_reference = Some(
                        selection::acceptance_reference(
                            input
                                .permission_challenge
                                .as_deref()
                                .expect("validated challenge"),
                        )
                        .map_err(Error::Invalid)?,
                    );
                }
                replacement::freeze(
                    &predecessor_job,
                    &mut next,
                    &result,
                    predecessor.owner.as_deref().unwrap_or(""),
                    reviewer,
                    self.host.config(),
                )
                .map_err(|e| Error::Invalid(e.to_string()))?;
                replacement::verify_transition(
                    &predecessor_job,
                    &next,
                    &stopped,
                    predecessor.owner.as_deref().unwrap_or(""),
                    self.host.config(),
                )
                .map_err(|e| Error::Invalid(e.to_string()))?;
                job = next;
            } else if input.permission_challenge.is_some() {
                return Err(Error::Invalid(
                    "permission_challenge requires replacement".into(),
                ));
            }
            let payload = serde_json::to_string(&job).map_err(|e| Error::Invalid(e.to_string()))?;
            if payload.len() > relay::MAX_PAYLOAD_BYTES {
                return Err(Error::Invalid("continuation payload exceeds 64 KiB; shorten the bounded review focus or selection".into()));
            }
            let conflicting: Option<String> = tx
                .query_row(
                    "SELECT payload FROM tasks WHERE key=?1",
                    [&input.key],
                    |r| r.get(0),
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
        let task = match store.submit(&key, &payload) {
            Ok(task) => task,
            Err(error) => {
                if matches!(
                    error,
                    relay::Error::IdempotencyConflict | relay::Error::Invalid(_)
                ) {
                    control.execute("DELETE FROM app_continuations WHERE predecessor_id=?1 AND key=?2 AND task_id IS NULL",params![id,key])?;
                }
                return Err(error.into());
            }
        };
        control.execute(
            "UPDATE app_continuations SET task_id=?2 WHERE predecessor_id=?1",
            params![id, task.id],
        )?;
        if let Some(token) = input.permission_challenge.as_deref()
            && serde_json::from_str::<Job>(&payload)
                .ok()
                .and_then(|job| job.role_binding)
                .and_then(|binding| binding.acceptance_reference)
                == selection::acceptance_reference(token).ok()
        {
            self.permission_challenges
                .lock()
                .map_err(|_| Error::Poisoned)?
                .consume(token);
        }
        Ok(task)
    }
    pub fn get(&self, id: i64) -> Result<Task> {
        Ok(self
            .state
            .lock()
            .map_err(|_| Error::Poisoned)?
            .store
            .get(id)?)
    }
    pub fn list(&self, before: Option<i64>) -> Result<Vec<Task>> {
        Ok(self
            .state
            .lock()
            .map_err(|_| Error::Poisoned)?
            .store
            .list(before, 100)?)
    }
    pub fn get_view(&self, id: i64) -> Result<TaskView> {
        let state = self.state.lock().map_err(|_| Error::Poisoned)?;
        task_view(&state.control, state.store.get(id)?)
    }
    pub fn list_views(&self, before: Option<i64>) -> Result<Vec<TaskView>> {
        let state = self.state.lock().map_err(|_| Error::Poisoned)?;
        state
            .store
            .list(before, 100)?
            .into_iter()
            .map(|task| task_view(&state.control, task))
            .collect()
    }
    pub fn status(&self) -> Result<Value> {
        let state = self.state.lock().map_err(|_| Error::Poisoned)?;
        let active = state.store.active_claim()?;
        let recovery_required = active.as_ref().is_some_and(|task| {
            state
                .running
                .as_ref()
                .is_none_or(|r| !matches_claim(task, &r.0))
        });
        let diagnostic: Option<String> = match &active {
            Some(task) => state
                .control
                .query_row(
                    "SELECT result FROM app_diagnostics WHERE task_id=?1 AND generation=?2",
                    params![task.id, task.generation],
                    |r| r.get(0),
                )
                .optional()?,
            None => None,
        };
        Ok(json!({"active":active,"recovery_required":recovery_required,"diagnostic":diagnostic}))
    }
    /// Cached catalog reads never launch agents or authenticate with providers.
    pub fn capabilities(&self) -> Result<Vec<catalog_cache::CatalogView>> {
        let mut cache = self.catalogs.lock().map_err(|_| Error::Poisoned)?;
        Ok(self
            .config
            .native_agents
            .iter()
            .map(|(name, profile)| cache.view(name, profile))
            .collect())
    }
    /// Explicit operator request; does not start a user turn or submit work.
    pub fn refresh_capabilities(&self, name: &str) -> Result<catalog_cache::CatalogView> {
        self.refresh_capabilities_confirmed(name, &CatalogRefreshRequest::default())
    }
    pub fn refresh_capabilities_confirmed(
        &self,
        name: &str,
        request: &CatalogRefreshRequest,
    ) -> Result<catalog_cache::CatalogView> {
        let profile = self
            .config
            .native_agents
            .get(name)
            .ok_or_else(|| Error::Invalid("native profile is not allowlisted".into()))?;
        let (generation, stamp) = {
            let mut cache = self.catalogs.lock().map_err(|_| Error::Poisoned)?;
            cache.reconciled_guard(capabilities::discovery_guard_present(&self.host));
            match cache
                .begin_confirmed(name, profile, request)
                .map_err(|error| Error::DiscoveryUnavailable(error.into()))?
            {
                Some(start) => start,
                None => return Ok(cache.view(name, profile)),
            }
        };
        let catalog = capabilities::discover_confirmed(&self.host, profile, &stamp);
        let cleanup_confirmed =
            catalog.process_cleanup.state == capabilities::CapabilityState::Supported;
        let mut cache = self.catalogs.lock().map_err(|_| Error::Poisoned)?;
        Ok(cache.finish(name, profile, generation, catalog, cleanup_confirmed))
    }
    /// Bounded metadata-only source estimate; never invokes a configured command.
    pub fn resource_estimate(
        &self,
        repository: &str,
        workflow: Option<&str>,
    ) -> Result<resources::ResourceEstimate> {
        self.resource_estimate_with_reviewer(repository, workflow, None)
    }
    pub fn resource_estimate_with_reviewer(
        &self,
        repository: &str,
        workflow: Option<&str>,
        reviewer_profile: Option<&str>,
    ) -> Result<resources::ResourceEstimate> {
        resources::estimate_with_reviewer(
            self.host.config(),
            repository,
            workflow,
            reviewer_profile,
        )
        .map_err(Error::Invalid)
    }
    /// Explicit bounded read of configured workspace ownership and retention.
    pub fn workspace_inventory(&self, before: Option<i64>) -> Result<Value> {
        if before.is_some_and(|id| id <= 0) {
            return Err(Error::Invalid(
                "workspace cursor must be a positive task ID".into(),
            ));
        }
        let state = self.state.lock().map_err(|_| Error::Poisoned)?;
        Ok(workspaces::inventory(
            self.host.config(),
            &state.store,
            &state.control,
            before,
        ))
    }
    /// Read a single attempt's operator controls. Filesystem measurements have a
    /// short cache; eligibility and every action are checked independently.
    pub fn operator(&self, id: i64) -> Result<Value> {
        let (task, successor, reservation, diagnostic) = {
            let state = self.state.lock().map_err(|_| Error::Poisoned)?;
            let view = task_view(&state.control, state.store.get(id)?)?;
            let reservation: Option<(String, String)> = state
                .control
                .query_row(
                    "SELECT key,payload FROM app_continuations WHERE predecessor_id=?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let diagnostic: Option<String> = state
                .control
                .query_row(
                    "SELECT result FROM app_diagnostics WHERE task_id=?1 AND generation=?2",
                    params![id, view.task.generation],
                    |row| row.get(0),
                )
                .optional()?;
            (
                view.task,
                view.continuation_status
                    .and_then(|status| status.successor_id),
                reservation,
                diagnostic,
            )
        };
        let job = Job::from_payload(&task.payload, self.host.config()).ok();
        let result = task
            .result
            .as_deref()
            .or(diagnostic.as_deref())
            .and_then(|text| serde_json::from_str::<host::RunResult>(text).ok());
        let path = job
            .as_ref()
            .map(|job| workspaces::root(self.host.config(), &task, job));
        let inherited_quota = job.as_ref().map(|job| {
            job.workspace_quota_bytes
                .unwrap_or_else(|| self.config.workspace_byte_limit())
        });
        let quota = result
            .as_ref()
            .and_then(|result| result.resources.as_ref())
            .and_then(|resources| resources.quota_bytes)
            .or_else(|| job.as_ref().and_then(|job| job.workspace_quota_bytes))
            .or_else(|| {
                (task.state == relay::State::Queued)
                    .then_some(inherited_quota)
                    .flatten()
            });
        let workspace_retained = path.as_ref().is_some_and(|path| {
            std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
                && path
                    .canonicalize()
                    .is_ok_and(|canonical| canonical == *path)
        });
        let usage = {
            let mut cache = self
                .resource_measurements
                .lock()
                .map_err(|_| Error::Poisoned)?;
            let key = (task.id, task.generation);
            if let Some((time, usage)) = cache.get(&key)
                && time.elapsed() < Duration::from_secs(5)
            {
                usage.clone()
            } else {
                let usage = path
                    .as_deref()
                    .map(|path| resources::measure_workspace(path, self.host.config()))
                    .unwrap_or_else(|| {
                        resources::Usage::unavailable("task has no valid host workspace binding")
                    });
                if cache.len() >= 100 {
                    cache.clear();
                }
                cache.insert(key, (std::time::Instant::now(), usage.clone()));
                usage
            }
        };
        let resources = resources::ResourceState::new(usage.clone(), quota, self.host.config());
        let mut actions = Vec::new();
        let mut reserved_request = Value::Null;
        let mut blocked_reason = None;
        if let Some(successor) = successor {
            blocked_reason = Some(format!("continuation already created as task #{successor}"));
        } else if let Some((key, payload)) = reservation {
            if let Ok(reserved) = serde_json::from_str::<Job>(&payload) {
                if reserved
                    .continuation
                    .as_ref()
                    .is_some_and(|c| c.operator_adoption.is_some())
                {
                    blocked_reason = Some("operator review adoption is reserved; resume with the same local adopt-review request after inspecting the complete response and accepting prior host tests".into());
                } else {
                    let review = reserved
                        .continuation
                        .as_ref()
                        .and_then(|continuation| continuation.review_only.as_ref());
                    let action = if review.is_some() {
                        "continue_review"
                    } else {
                        "retry"
                    };
                    reserved_request = json!({"action_id":action,"key":key,"workspace_quota_bytes":reserved.continuation.as_ref().and_then(|c| c.quota_increase.as_ref()).map(|q|q.new_bytes),"revalidate_tests":review.is_some(),"review_focus":review.and_then(|review|review.review_focus.as_ref())});
                    if let Some(proof) = replacement::current(&reserved) {
                        reserved_request["replacement"] =
                            serde_json::to_value(selection::role(&reserved, proof.role.reviewer()))
                                .expect("role serializable");
                    }
                    actions.push(json!({"allowed":true,"ordinary_allowed":true,"id":action,"quota_increase_allowed":false,"quota_increase_required":false,"min_quota_bytes":null,"max_quota_bytes":self.config.workspace_byte_limit(),"requires_test_revalidation":review.is_some()}));
                }
            } else {
                blocked_reason = Some("reserved continuation payload cannot be verified".into());
            }
        } else if task.state != relay::State::Finished {
            blocked_reason = Some("only durably stopped unsuccessful attempts can continue; unknown or active ownership requires host reconciliation".into());
        } else if let (Some(job), Some(result), Some(path), Some(inherited_quota)) =
            (&job, &result, &path, inherited_quota)
        {
            if !matches!(
                result.outcome,
                host::Outcome::Failure | host::Outcome::TimedOut | host::Outcome::Cancelled
            ) {
                blocked_reason = Some("only stopped unsuccessful attempts can continue".into());
            } else if result.draft_pr.is_some()
                || result.workflow.as_ref().is_some_and(|workflow| {
                    workflow.reconciliation_required || workflow.publication.is_some()
                })
            {
                blocked_reason = Some("publication was attempted or is ambiguous; reconcile external effects before recovery".into());
            } else if !workspace_retained || result.workspace.as_ref() != Some(path) {
                blocked_reason = Some(
                    "the retained workspace is missing or does not match the stopped result".into(),
                );
            } else if let Err(error) = workspaces::continuation(self.host.config(), &task, job)
                .and_then(|_| {
                    workspaces::verify_candidate_checkpoint(self.host.config(), job, path)
                })
            {
                blocked_reason = Some(error.to_string());
            } else {
                let eligibility = resource_eligibility(
                    result,
                    &usage,
                    quota,
                    inherited_quota,
                    self.config.workspace_byte_limit(),
                );
                blocked_reason = eligibility.reason;
                if eligibility.same_quota || eligibility.increase_min.is_some() {
                    let action = |id: &str, review: bool| json!({"allowed":true,"ordinary_allowed":true,"replacement":replacement::capability(job,result,self.host.config(),path,review),"id":id,"quota_increase_allowed":eligibility.increase_min.is_some(),"quota_increase_required":!eligibility.same_quota,"min_quota_bytes":eligibility.increase_min,"max_quota_bytes":self.config.workspace_byte_limit(),"requires_test_revalidation":review});
                    let mut retry = action("retry", false);
                    if replacement::requires_stage_retry(job)
                        && replacement::stage(result, job, self.host.config(), false).is_err()
                    {
                        retry["ordinary_allowed"] = json!(false);
                        retry["allowed"] = json!(false);
                        blocked_reason=Some("ordinary retry cannot reset the repair budget of a replacement chain; only a proven stopped developer stage can resume".into());
                    }
                    if retry["ordinary_allowed"] == true || retry["replacement"]["allowed"] == true
                    {
                        actions.push(retry);
                    }

                    if workflow::review_continuation(result, None).is_ok() {
                        match workspaces::verify_review_checkpoint(
                            self.host.config(),
                            job,
                            result,
                            path,
                        ) {
                            Ok(()) => actions.push(action("continue_review", true)),
                            Err(error) => {
                                let mut replacement_only = action("continue_review", true);
                                replacement_only["ordinary_allowed"] = json!(false);
                                replacement_only["allowed"] = json!(false);
                                if replacement_only["replacement"]["allowed"] == true {
                                    actions.push(replacement_only);
                                }
                                blocked_reason = Some(format!(
                                    "review-only continuation is unavailable: {error}"
                                ));
                            }
                        }
                    }
                }
            }
        } else {
            blocked_reason =
                Some("task payload or stopped result is not a verified host record".into());
        }
        let failure = result.as_ref().and_then(|result| result.failure.clone()).or_else(|| result.as_ref().filter(|result|result.outcome != host::Outcome::Success).map(|result|resources::Failure::new("legacy_failure", "unknown", result.error.clone().unwrap_or_else(|| "legacy result has no structured failure; inspect the retained stage output".into()))));
        Ok(
            json!({"task_id":task.id,"generation":task.generation,"failure":failure,"resources":resources,"retained_result":{"available":task.result.is_some(),"immutable":true},"workspace_retained":workspace_retained,"recovery":{"inherited_quota_bytes":inherited_quota,"actions":actions,"blocked_reason":blocked_reason,"successor_id":successor,"reserved_request":reserved_request}}),
        )
    }
    pub fn public_config(&self) -> Value {
        let agents: Vec<_> = self
            .config
            .agents
            .keys()
            .chain(self.config.native_agents.keys())
            .collect();
        let native_agents: Vec<_> = self
            .config
            .native_agents
            .iter()
            .map(|(name, profile)| {
                json!({"name": name, "provider": profile.provider, "model": profile.model,
                "effort": profile.effort, "authentication": "unknown",
                "native_permission":profile.native_permission,"permission_modes":selection::permission_choices(profile),
                "allow_startup_discovery":profile.allow_startup_discovery,
                "reviewer_supported":profile.provider == providers::ProviderKind::ClaudeCli && profile.native_permission.is_none_or(|mode|mode.compatible(profile.provider,true))})
            })
            .collect();
        let workflows: Vec<_> = self.config.workflows.iter().map(|(name, workflow)| {
            json!({"name":name,"repository":workflow.repository,"developer":workflow.developer,
                "reviewer":workflow.reviewer,"test":workflow.test,"max_repairs":workflow.max_repairs,
                "selectable_developers":selection::selectable(&workflow.developer,workflow.selectable_developers.as_ref()),
                "selectable_reviewers":selection::selectable(&workflow.reviewer,workflow.selectable_reviewers.as_ref())})
        }).collect();
        json!({"repositories":self.config.repositories.keys().collect::<Vec<_>>(),"agents":agents,"native_agents":native_agents,"tests":self.config.tests.keys().collect::<Vec<_>>(),"workflows":workflows})
    }
    pub fn cancel(&self, id: i64) -> Result<Value> {
        let mut state = self.state.lock().map_err(|_| Error::Poisoned)?;
        let StateData {
            control, running, ..
        } = &mut *state;
        // Serialize the observed core state and request against claims by any process.
        let tx = control.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task: Option<(String, i64, Option<String>)> = tx
            .query_row(
                "SELECT state,generation,owner FROM tasks WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (task_state, generation, owner) = task.ok_or(relay::Error::NotFound)?;
        if task_state == "finished" {
            return Ok(json!({"requested":false,"finished":true}));
        }
        let owned = running.as_ref().is_some_and(|(claim, _)| {
            claim.task_id == id
                && claim.generation == generation
                && owner.as_deref() == Some(&claim.owner)
        });
        if task_state == "claimed" && !owned {
            return Err(Error::RecoveryRequired);
        }
        tx.execute(
            "INSERT OR IGNORE INTO app_cancellations(task_id) VALUES (?1)",
            [id],
        )?;
        tx.commit()?;
        if owned && let Some((_, flag)) = running {
            flag.store(true, Ordering::SeqCst);
        }
        Ok(json!({"requested":true}))
    }
    /// One blocking worker. SQLite serializes claims across processes; unknown claims stay blocked.
    pub fn work_once(&self) -> Result<bool> {
        let (task, cancellation) = {
            let mut state = self.state.lock().map_err(|_| Error::Poisoned)?;
            if self.shutdown.load(Ordering::SeqCst) {
                return Ok(false);
            }
            let Some(task) = state
                .store
                .claim_next(&format!("host-{}", std::process::id()))?
            else {
                return Ok(false);
            };
            let cancelled = state
                .control
                .query_row(
                    "SELECT task_id FROM app_cancellations WHERE task_id=?1",
                    [task.id],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
                .is_some();
            let flag = Arc::new(AtomicBool::new(cancelled));
            state.running = Some((task.claim().expect("fresh claim"), flag.clone()));
            (task, flag)
        };
        let execution = self.host.execute(&task, cancellation);
        // Observations are task-scoped, bounded and transient. They never enter
        // durable task payloads/results or authorize a cross-context selection.
        for (name, observation, stamp) in self.host.take_model_observations() {
            if let Some(profile) = self.config.native_agents.get(&name) {
                self.catalogs.lock().map_err(|_| Error::Poisoned)?.observe(
                    &name,
                    profile,
                    observation,
                    stamp,
                );
            }
        }
        let result = execution.to_json();
        let mut state = self.state.lock().map_err(|_| Error::Poisoned)?;
        if execution.outcome == host::Outcome::Unknown {
            if state
                .running
                .as_ref()
                .is_some_and(|r| matches_claim(&task, &r.0))
            {
                state.running = None;
            }
            if !matches_claim(
                &state.store.get(task.id)?,
                &task.claim().expect("fresh claim"),
            ) {
                return Err(relay::Error::StaleClaim.into());
            }
            state.control.execute("INSERT INTO app_diagnostics(task_id,generation,result) VALUES (?1,?2,?3) ON CONFLICT(task_id) DO UPDATE SET generation=excluded.generation,result=excluded.result WHERE excluded.generation>=app_diagnostics.generation", params![task.id, task.generation,result])?;
            return Err(Error::RecoveryRequired);
        }
        // A failure to persist keeps the core claim active. Never automatically retry execution.
        let finished = state
            .store
            .finish(&task.claim().expect("fresh claim"), &result);
        if state
            .running
            .as_ref()
            .is_some_and(|r| matches_claim(&task, &r.0))
        {
            state.running = None;
        }
        finished?;
        if execution.outcome == host::Outcome::Success
            && let Some(workspace) = &execution.workspace
            && let Err(error) = workspaces::mark_finished(workspace, &task)
        {
            eprintln!(
                "successful workspace retained because completion marker could not be saved: {error}"
            );
        }
        state.control.execute(
            "DELETE FROM app_cancellations WHERE task_id=?1",
            params![task.id],
        )?;
        Ok(true)
    }
    pub fn cleanup_completed(&self) -> Result<usize> {
        let state = self.state.lock().map_err(|_| Error::Poisoned)?;
        workspaces::cleanup(self.host.config(), &state.store)
            .map_err(|e| Error::Invalid(e.to_string()))
    }
    pub fn worker(&self) {
        let mut next_cleanup = std::time::Instant::now();
        while !self.shutdown.load(Ordering::SeqCst) {
            if std::time::Instant::now() >= next_cleanup {
                if let Err(error) = self.cleanup_completed() {
                    eprintln!("workspace cleanup: {error}");
                }
                next_cleanup = std::time::Instant::now() + Duration::from_secs(60);
            }
            match self.work_once() {
                Ok(true) => (),
                Ok(false) => std::thread::sleep(Duration::from_millis(100)),
                Err(error) => {
                    eprintln!("worker: {error}");
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
    }
    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Ok(state) = self.state.lock()
            && let Some((_, flag)) = &state.running
        {
            flag.store(true, Ordering::SeqCst);
        }
    }
}

fn matches_claim(task: &Task, claim: &relay::Claim) -> bool {
    task.id == claim.task_id
        && task.generation == claim.generation
        && task.owner.as_deref() == Some(claim.owner.as_str())
}

fn action_unavailable(code: impl Into<String>, cause: impl Into<String>) -> Error {
    Error::ActionUnavailable {
        code: code.into(),
        cause: cause.into(),
    }
}

struct ResourceEligibility {
    same_quota: bool,
    increase_min: Option<u64>,
    reason: Option<String>,
}
/// GET controls and POST validation deliberately share this decision. An incomplete
/// metadata walk is not proof of an incomplete workspace initialization: the latter
/// remains an independent, unconditional ownership/ready-marker guard.
fn resource_eligibility(
    result: &host::RunResult,
    usage: &resources::Usage,
    prior_quota: Option<u64>,
    inherited_quota: u64,
    cap: u64,
) -> ResourceEligibility {
    let blocked = |reason: &str| ResourceEligibility {
        same_quota: false,
        increase_min: None,
        reason: Some(reason.into()),
    };
    let code = result.failure.as_ref().map(|failure| failure.code.as_str());
    if matches!(
        code,
        Some("snapshot_limit_exceeded" | "snapshot_entry_limit_exceeded")
    ) {
        return blocked(
            "source snapshot admission failed; a workspace quota increase cannot change the source limit",
        );
    }
    let Some(bytes) = usage.logical_bytes else {
        return blocked("current workspace usage is unavailable; recovery cannot be verified");
    };
    if !usage.complete {
        if bytes > inherited_quota
            || matches!(
                code,
                Some("workspace_quota_exceeded" | "workspace_entry_limit_exceeded")
            )
        {
            return blocked(
                "resource failure requires a complete fresh workspace measurement before recovery",
            );
        }
        return ResourceEligibility { same_quota: true, increase_min: None, reason: Some("usage is an incomplete lower bound; quota increases are unavailable and the original limit will be checked again before execution".into()) };
    }
    let has_proof = prior_quota.is_some()
        && result
            .resources
            .as_ref()
            .and_then(|resources| resources.quota_bytes)
            == prior_quota;
    let quota = prior_quota.unwrap_or(inherited_quota);
    let admission_required = result
        .failure
        .as_ref()
        .and_then(|failure| failure.required_bytes)
        .unwrap_or(0);
    let increase_min = bytes
        .checked_add(1)
        .map(|bytes| bytes.max(quota.saturating_add(1)).max(admission_required))
        .filter(|minimum| has_proof && *minimum <= cap);
    ResourceEligibility {
        same_quota: bytes <= inherited_quota && admission_required <= inherited_quota,
        increase_min,
        reason: if bytes > inherited_quota || admission_required > inherited_quota {
            Some(if increase_min.is_some() { "retained usage or known planned-copy admission exceeds this attempt quota; choose an explicit increase to continue" } else { "retained usage exceeds this attempt quota and no verified increase fits the host policy cap" }.into())
        } else if quota >= cap {
            Some(
                "this attempt already uses the host policy cap; no quota increase is available"
                    .into(),
            )
        } else if !has_proof {
            Some(
                "stopped result has no matching quota proof; only same-quota recovery is available"
                    .into(),
            )
        } else {
            None
        },
    }
}

fn recorded_quota(result: &host::RunResult, job: &Job) -> Option<u64> {
    result
        .resources
        .as_ref()
        .and_then(|resources| resources.quota_bytes)
        .or(job.workspace_quota_bytes)
}

#[cfg(test)]
mod resource_policy_tests {
    use super::*;
    fn result(code: &str, quota: u64) -> host::RunResult {
        serde_json::from_value(json!({"outcome":"failure","workspace":null,"agent":null,"tests":null,"draft_pr":null,"error":null,
            "failure":{"code":code,"stage":"workspace_admission","cause":"fixture"},
            "resources":{"usage":{"logical_bytes":10,"complete":true,"measured_at":0,"reason":null},"quota_bytes":quota,"host_policy_cap_bytes":1000,"snapshot_cap_bytes":1000,"enforcement":"logical_bytes_best_effort","os_hard_quota":false,"disk_reserved":false}})).unwrap()
    }
    #[test]
    fn planned_admission_requires_the_known_minimum_even_when_current_usage_is_low() {
        let mut result = result("workspace_quota_exceeded", 100);
        result.failure.as_mut().unwrap().required_bytes = Some(300);
        let eligibility = resource_eligibility(
            &result,
            &resources::Usage::observed(10, true),
            Some(100),
            100,
            1000,
        );
        assert!(!eligibility.same_quota);
        assert_eq!(eligibility.increase_min, Some(300));
        let blocked = resource_eligibility(
            &result,
            &resources::Usage::observed(10, true),
            Some(100),
            100,
            200,
        );
        assert!(!blocked.same_quota);
        assert_eq!(blocked.increase_min, None);
    }
    #[test]
    fn incomplete_resource_failure_differs_from_nonresource_failure_and_unavailable_usage() {
        for code in ["workspace_quota_exceeded", "workspace_entry_limit_exceeded"] {
            let eligibility = resource_eligibility(
                &result(code, 100),
                &resources::Usage::observed(10, false),
                Some(100),
                100,
                1000,
            );
            assert!(!eligibility.same_quota);
            assert_eq!(eligibility.increase_min, None);
        }
        let result = result("command_failed", 100);
        assert!(
            resource_eligibility(
                &result,
                &resources::Usage::observed(10, false),
                Some(100),
                100,
                1000
            )
            .same_quota
        );
        assert!(
            !resource_eligibility(
                &result,
                &resources::Usage::unavailable("missing"),
                Some(100),
                100,
                1000
            )
            .same_quota
        );
    }
    #[test]
    fn prior_quota_and_unchanged_default_payload_are_distinct_after_policy_change() {
        let result = result("workspace_quota_exceeded", 600);
        let eligibility = resource_eligibility(
            &result,
            &resources::Usage::observed(650, true),
            Some(600),
            800,
            800,
        );
        assert!(eligibility.same_quota);
        assert_eq!(eligibility.increase_min, Some(651));
        let legacy = resource_eligibility(
            &result,
            &resources::Usage::observed(650, true),
            None,
            800,
            800,
        );
        assert!(legacy.same_quota);
        assert_eq!(legacy.increase_min, None);
    }
}
