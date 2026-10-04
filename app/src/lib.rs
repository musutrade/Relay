//! Application adapters around Relay's opaque durable core.
//! All direct database callers belong to the same trusted local OS account.
mod app_server;
pub mod auth;
pub mod host;
pub mod http;
pub mod mcp;
pub mod providers;
mod sessions;
pub mod workflow;

use host::{Host, HostConfig, Job};
use relay::{Store, Task};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Deserialize;
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
        control.execute_batch("CREATE TABLE IF NOT EXISTS app_cancellations(task_id INTEGER PRIMARY KEY REFERENCES tasks(id)); CREATE TABLE IF NOT EXISTS app_diagnostics(task_id INTEGER PRIMARY KEY, generation INTEGER NOT NULL, result TEXT NOT NULL);")?;
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
        state.control.execute(
            "DELETE FROM app_cancellations WHERE task_id=?1",
            params![task.id],
        )?;
        Ok(true)
    }
    pub fn worker(&self) {
        while !self.shutdown.load(Ordering::SeqCst) {
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
