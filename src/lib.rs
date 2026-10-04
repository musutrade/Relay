//! Durable local task boundary. Payloads are opaque and never executed by this crate.
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use std::{path::Path, time::Duration};
use thiserror::Error;

pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024;
pub const MAX_RESULT_BYTES: usize = 16 * 1024;
pub const MAX_IDENTITY_BYTES: usize = 128;

#[derive(Debug, Error)]
pub enum Error {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("invalid or oversized {0}")]
    Invalid(&'static str),
    #[error("idempotency key already belongs to a different payload")]
    IdempotencyConflict,
    #[error("task not found")]
    NotFound,
    #[error("claim is stale, not active, or owned by another executor")]
    StaleClaim,
    #[error("database schema version is unsupported: {0}")]
    SchemaVersion(i64),
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Queued,
    Claimed,
    Finished,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Task {
    pub id: i64,
    pub key: String,
    pub payload: String,
    pub state: State,
    pub generation: i64,
    pub owner: Option<String>,
    pub result: Option<String>,
}

/// A capability within the trusted local account, not a network authentication token.
#[derive(Debug, Clone, Serialize)]
pub struct Claim {
    pub task_id: i64,
    pub generation: i64,
    pub owner: String,
}
impl Task {
    pub fn claim(&self) -> Option<Claim> {
        (self.state == State::Claimed).then(|| Claim {
            task_id: self.id,
            generation: self.generation,
            owner: self.owner.clone().expect("claimed tasks have an owner"),
        })
    }
}

pub struct Store {
    conn: Connection,
}
impl Store {
    /// The host must provide a private directory and trusted local filesystem.
    /// Opening a database preserves active claims, including after a process crash.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > 1 {
            return Err(Error::SchemaVersion(version));
        }
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS tasks (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            key TEXT NOT NULL UNIQUE CHECK(length(CAST(key AS BLOB)) BETWEEN 1 AND 128),
            payload TEXT NOT NULL CHECK(length(CAST(payload AS BLOB)) <= 65536),
            state TEXT NOT NULL CHECK(state IN ('queued', 'claimed', 'finished')),
            generation INTEGER NOT NULL DEFAULT 0 CHECK(generation >= 0),
            owner TEXT CHECK(owner IS NULL OR length(CAST(owner AS BLOB)) BETWEEN 1 AND 128),
            result TEXT CHECK(result IS NULL OR length(CAST(result AS BLOB)) <= 16384),
            CHECK((state = 'queued' AND owner IS NULL AND result IS NULL)
               OR (state = 'claimed' AND owner IS NOT NULL AND result IS NULL)
               OR (state = 'finished' AND owner IS NOT NULL AND result IS NOT NULL))
        );
        CREATE UNIQUE INDEX IF NOT EXISTS one_active_claim ON tasks(state) WHERE state = 'claimed';
        PRAGMA user_version = 1;",
        )?;
        tx.commit()?;
        Ok(Self { conn })
    }

    pub fn submit(&mut self, key: &str, payload: &str) -> Result<Task> {
        identity("key", key)?;
        bounded("payload", payload, MAX_PAYLOAD_BYTES)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO tasks(key,payload,state) VALUES (?1,?2,'queued') ON CONFLICT(key) DO NOTHING", params![key,payload])?;
        let task = tx.query_row(&format!("{SELECT} WHERE key=?1"), [key], row)?;
        if task.payload != payload {
            return Err(Error::IdempotencyConflict);
        }
        tx.commit()?;
        Ok(task)
    }

    /// Claims oldest queued task only when no other task is claimed globally.
    /// None means empty OR blocked by an active claim; inspect that claim before recovery.
    pub fn claim_next(&mut self, owner: &str) -> Result<Option<Task>> {
        identity("owner", owner)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let active: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE state='claimed')",
            [],
            |r| r.get(0),
        )?;
        if active {
            return Ok(None);
        }
        let id: Option<i64> = tx
            .query_row(
                "SELECT id FROM tasks WHERE state='queued' ORDER BY id LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let Some(id) = id else { return Ok(None) };
        tx.execute(
            "UPDATE tasks SET state='claimed',owner=?1,generation=generation+1 WHERE id=?2",
            params![owner, id],
        )?;
        let task = tx.query_row(&format!("{SELECT} WHERE id=?1"), [id], row)?;
        tx.commit()?;
        Ok(Some(task))
    }

    pub fn finish(&mut self, claim: &Claim, result: &str) -> Result<Task> {
        bounded("result", result, MAX_RESULT_BYTES)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = tx
            .query_row(&format!("{SELECT} WHERE id=?1"), [claim.task_id], row)
            .optional()?
            .ok_or(Error::NotFound)?;
        if task.generation != claim.generation
            || task.owner.as_deref() != Some(claim.owner.as_str())
        {
            return Err(Error::StaleClaim);
        }
        if task.state == State::Finished && task.result.as_deref() == Some(result) {
            tx.commit()?;
            return Ok(task);
        }
        if task.state != State::Claimed {
            return Err(Error::StaleClaim);
        }
        tx.execute(
            "UPDATE tasks SET state='finished',result=?1 WHERE id=?2",
            params![result, claim.task_id],
        )?;
        let task = tx.query_row(&format!("{SELECT} WHERE id=?1"), [claim.task_id], row)?;
        tx.commit()?;
        Ok(task)
    }

    /// Trusted-host operation, never triggered by timeout or restart.
    /// Caller must first establish that old execution/processes have stopped and
    /// decide whether repeating any external side effects is safe.
    pub fn confirm_stopped_and_requeue(&mut self, claim: &Claim) -> Result<Task> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute("UPDATE tasks SET state='queued',owner=NULL WHERE id=?1 AND state='claimed' AND generation=?2 AND owner=?3", params![claim.task_id,claim.generation,claim.owner])?;
        if changed != 1 {
            return Err(Error::StaleClaim);
        }
        let task = tx.query_row(&format!("{SELECT} WHERE id=?1"), [claim.task_id], row)?;
        tx.commit()?;
        Ok(task)
    }

    pub fn get(&self, id: i64) -> Result<Task> {
        self.conn
            .query_row(&format!("{SELECT} WHERE id=?1"), [id], row)
            .optional()?
            .ok_or(Error::NotFound)
    }

    /// Bounded cursor-based history, newest first. Payloads remain opaque.
    pub fn list(&self, before: Option<i64>, limit: usize) -> Result<Vec<Task>> {
        if limit == 0 || limit > 100 {
            return Err(Error::Invalid("list limit"));
        }
        let mut statement = self.conn.prepare(&format!(
            "{SELECT} WHERE (?1 IS NULL OR id < ?1) ORDER BY id DESC LIMIT ?2"
        ))?;
        let rows = statement.query_map(params![before, limit as i64], row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn active_claim(&self) -> Result<Option<Task>> {
        Ok(self
            .conn
            .query_row(&format!("{SELECT} WHERE state='claimed'"), [], row)
            .optional()?)
    }
}
const SELECT: &str = "SELECT id,key,payload,state,generation,owner,result FROM tasks";
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    let state: String = r.get(3)?;
    let state = match state.as_str() {
        "queued" => State::Queued,
        "claimed" => State::Claimed,
        "finished" => State::Finished,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    Ok(Task {
        id: r.get(0)?,
        key: r.get(1)?,
        payload: r.get(2)?,
        state,
        generation: r.get(4)?,
        owner: r.get(5)?,
        result: r.get(6)?,
    })
}
fn bounded(name: &'static str, value: &str, max: usize) -> Result<()> {
    if value.len() > max {
        Err(Error::Invalid(name))
    } else {
        Ok(())
    }
}
fn identity(name: &'static str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(Error::Invalid(name));
    }
    bounded(name, value, MAX_IDENTITY_BYTES)
}
