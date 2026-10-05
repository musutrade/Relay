//! Application adapters around Relay's opaque durable core.
//! All direct database callers belong to the same trusted local OS account.
mod app_server;
pub mod auth;
mod git_inventory;
pub mod host;
pub mod http;
pub mod mcp;
pub mod providers;
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
    #[error(
        "task execution is unknown after restart; confirm the old process tree has stopped before local recovery"
    )]
    RecoveryRequired,
    #[error("internal state unavailable")]
    Poisoned,
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Submission {
    pub key: String,
    pub job: Job,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryRequest {
    pub key: String,
    pub confirm_stopped_and_reconciled: bool,
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
    pub fn submit(&self, input: Submission) -> Result<Task> {
        if input.job.continuation.is_some() {
            return Err(Error::Invalid(
                "use the explicit retry endpoint to continue preserved work".into(),
            ));
        }
        input
            .job
            .validate(&self.config)
            .map_err(|e| Error::Invalid(e.to_string()))?;
        let payload =
            serde_json::to_string(&input.job).map_err(|e| Error::Invalid(e.to_string()))?;
        self.state
            .lock()
            .map_err(|_| Error::Poisoned)?
            .store
            .submit(&input.key, &payload)
            .map_err(Into::into)
    }
    /// One explicit successor per predecessor. Reservation precedes core submission,
    /// and its stable key/payload make a crash between the two operations retryable.
    pub fn retry(&self, id: i64, input: RetryRequest) -> Result<Task> {
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
                return Err(Error::Invalid(
                    "only stopped unsuccessful tasks can continue".into(),
                ));
            }
            if result.draft_pr.is_some() {
                return Err(Error::Invalid(
                    "publication was attempted; local reconciliation is required".into(),
                ));
            }
            let mut job = Job::from_payload(&predecessor.payload, self.host.config())
                .map_err(|e| Error::Invalid(e.to_string()))?;
            job.continuation = Some(
                workspaces::continuation(self.host.config(), &predecessor, &job)
                    .map_err(|e| Error::Invalid(e.to_string()))?,
            );
            let payload = serde_json::to_string(&job).map_err(|e| Error::Invalid(e.to_string()))?;
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
                "effort": profile.effort, "authentication": "unknown"})
            })
            .collect();
        let workflows: Vec<_> = self.config.workflows.iter().map(|(name, workflow)| {
            json!({"name":name,"repository":workflow.repository,"developer":workflow.developer,
                "reviewer":workflow.reviewer,"test":workflow.test,"max_repairs":workflow.max_repairs})
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
