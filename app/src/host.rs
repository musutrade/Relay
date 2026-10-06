//! Trusted Linux development host, deliberately outside Relay's opaque queue core.
//!
//! Configured commands run with the local account's privileges. This is process
//! lifecycle management and private snapshot isolation, not an adversarial sandbox.
//! A separate subreaper process owns every command tree; the application process
//! never changes its process-wide child-reaping behavior.
use crate::providers::{
    NativeProfile, ProtocolParser, ProviderKind, ProviderProbe, ProviderResult,
};
use crate::resources::{self, Failure, ResourceState};
use crate::workflow::{self, WorkflowConfig, WorkflowResult};
use relay::{MAX_PAYLOAD_BYTES, MAX_RESULT_BYTES, State, Task};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;

// Trusted-host logical-byte budgets stay finite, independently of disk capacity.
const MAX_STORAGE_BYTES: u64 = 1024 * 1024 * 1024 * 1024; // 1 TiB
const MAX_REQUIREMENTS: usize = 32 * 1024;
pub(crate) const MAX_PHASE_INPUT: usize = 64 * 1024;
const MAX_CONFIG_BYTES: usize = 256 * 1024;
const MAX_CAPTURE: usize = 8192;
const MAX_PROBE_CAPTURE: usize = 64 * 1024;
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);
const TICK: Duration = Duration::from_millis(10);

fn default_timeout() -> u64 {
    300
}
fn default_output() -> usize {
    2048
}
fn default_snapshot_bytes() -> u64 {
    50 * 1024 * 1024
}
fn default_snapshot_entries() -> usize {
    20_000
}
fn default_retained_workspaces() -> usize {
    100
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandProfile {
    /// Absolute executable path, supplied only by trusted host configuration.
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    pub workspace_root: PathBuf,
    pub repositories: BTreeMap<String, PathBuf>,
    #[serde(default)]
    pub agents: BTreeMap<String, CommandProfile>,
    #[serde(default)]
    pub native_agents: BTreeMap<String, NativeProfile>,
    #[serde(default)]
    pub workflows: BTreeMap<String, WorkflowConfig>,
    #[serde(default)]
    pub tests: BTreeMap<String, CommandProfile>,
    #[serde(default)]
    pub draft_pr_adapters: BTreeMap<String, CommandProfile>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_output")]
    pub output_limit_bytes: usize,
    #[serde(default = "default_snapshot_bytes")]
    pub max_snapshot_bytes: u64,
    /// Whole task tree, including Git and reviewer copies. Omission preserves legacy behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_workspace_bytes: Option<u64>,
    #[serde(default = "default_snapshot_entries")]
    pub max_snapshot_entries: usize,
    #[serde(default = "default_retained_workspaces")]
    pub max_retained_workspaces: usize,
    /// Opt-in TTL applies only to durably finished successful workspaces.
    #[serde(default)]
    pub successful_workspace_retention_seconds: Option<u64>,
    /// Override only for packaging/tests; must implement `__relay_host_supervisor`.
    #[serde(default)]
    pub supervisor_program: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    /// Durable active role identity; server-owned and retained across continuation chains.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_epochs: Option<crate::sessions::RoleEpochs>,
    /// Server-owned admission record; external submissions cannot supply this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_binding: Option<crate::selection::RoleBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_selections: Option<crate::selection::RoleSelections>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_quota_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<crate::workspaces::Continuation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    pub repository: String,
    pub requirements: String,
    pub agent: String,
    #[serde(default)]
    pub test: Option<String>,
    #[serde(default)]
    pub publish: bool,
    #[serde(default)]
    pub draft_pr_adapter: Option<String>,
}

#[derive(Debug, Error)]
pub enum HostError {
    #[error("invalid host configuration: {0}")]
    Config(String),
    #[error("invalid development job: {0}")]
    Job(String),
    #[error("host I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

impl HostConfig {
    pub fn workspace_byte_limit(&self) -> u64 {
        self.max_workspace_bytes.unwrap_or(self.max_snapshot_bytes)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, HostError> {
        let mut bytes = Vec::new();
        File::open(path)?
            .take((MAX_CONFIG_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(HostError::Config("configuration exceeds 256 KiB".into()));
        }
        Ok(serde_json::from_slice(&bytes)?)
    }
}

impl Job {
    pub fn from_payload(payload: &str, config: &HostConfig) -> Result<Self, HostError> {
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(HostError::Job("payload exceeds 64 KiB".into()));
        }
        let job: Self = serde_json::from_str(payload)?;
        job.validate(config)?;
        Ok(job)
    }

    pub fn validate(&self, config: &HostConfig) -> Result<(), HostError> {
        if let Some(epochs) = &self.role_epochs {
            epochs.validate()?;
        }
        if self
            .workspace_quota_bytes
            .is_some_and(|bytes| bytes == 0 || bytes > config.workspace_byte_limit())
        {
            return Err(HostError::Job(
                "workspace_quota_bytes must be positive and no greater than the host policy cap"
                    .into(),
            ));
        }
        if self.continuation.as_ref().is_some_and(|c| {
            c.workspace_task_id <= 0
                || c.predecessor_task_id < c.workspace_task_id
                || c.predecessor_generation <= 0
        }) {
            return Err(HostError::Job("invalid continuation reference".into()));
        }
        if self.requirements.trim().is_empty() || self.requirements.len() > MAX_REQUIREMENTS {
            return Err(HostError::Job(
                "requirements must contain 1–32768 UTF-8 bytes".into(),
            ));
        }
        if self.requirements.contains('\0') {
            return Err(HostError::Job("requirements cannot contain NUL".into()));
        }
        if !config.repositories.contains_key(&self.repository) {
            return Err(HostError::Job("repository is not allowlisted".into()));
        }
        if !config.agents.contains_key(&self.agent)
            && !config.native_agents.contains_key(&self.agent)
        {
            return Err(HostError::Job("agent profile is not allowlisted".into()));
        }
        if self
            .test
            .as_ref()
            .is_some_and(|name| !config.tests.contains_key(name))
        {
            return Err(HostError::Job("test profile is not allowlisted".into()));
        }
        if let Some(name) = &self.workflow {
            let workflow = config
                .workflows
                .get(name)
                .ok_or_else(|| HostError::Job("workflow profile is not allowlisted".into()))?;
            workflow.validate_job(self)?;
        }
        match (self.publish, self.draft_pr_adapter.as_ref()) {
            (true, Some(name)) if config.draft_pr_adapters.contains_key(name) => {}
            (true, _) => {
                return Err(HostError::Job(
                    "publishing requires an allowlisted draft PR adapter".into(),
                ));
            }
            (false, Some(_)) => {
                return Err(HostError::Job(
                    "draft PR adapter requires publish=true".into(),
                ));
            }
            (false, None) => {}
        }
        crate::selection::validate_job(self, config)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Success,
    Failure,
    Cancelled,
    TimedOut,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<Vec<crate::capabilities::ModelCapability>>,
    pub outcome: Outcome,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub duration_ms: u64,
    pub supervisor_pid: Option<u32>,
    pub error: Option<String>,
}
impl CommandResult {
    pub(crate) fn error(outcome: Outcome, error: impl Into<String>) -> Self {
        Self {
            failure: None,
            provider: None,
            catalog: None,
            outcome,
            exit_code: None,
            signal: None,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            duration_ms: 0,
            supervisor_pid: None,
            error: Some(error.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_stage: Option<crate::replacement::StoppedStage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<WorkflowResult>,
    pub outcome: Outcome,
    pub workspace: Option<PathBuf>,
    pub agent: Option<CommandResult>,
    pub tests: Option<CommandResult>,
    pub draft_pr: Option<CommandResult>,
    pub error: Option<String>,
}
impl RunResult {
    pub(crate) fn new(outcome: Outcome, error: Option<String>) -> Self {
        Self {
            stopped_stage: None,
            failure: None,
            resources: None,
            workflow: None,
            outcome,
            workspace: None,
            agent: None,
            tests: None,
            draft_pr: None,
            error,
        }
    }

    /// Includes JSON escaping in the core's 16 KiB result budget.
    pub fn to_json(&self) -> String {
        let mut value = self.clone();
        if let Some(failure) = &mut value.failure {
            truncate_utf8(&mut failure.cause, 1024);
            truncate_utf8(&mut failure.code, 80);
            truncate_utf8(&mut failure.stage, 128);
        }
        if let Some(resources) = &mut value.resources {
            truncate_utf8(&mut resources.enforcement, 64);
            if let Some(reason) = &mut resources.usage.reason {
                truncate_utf8(reason, 512);
            }
        }
        if let Some(error) = &mut value.error {
            truncate_utf8(error, 1024);
        }
        for command in [&mut value.agent, &mut value.tests, &mut value.draft_pr]
            .into_iter()
            .flatten()
        {
            if let Some(failure) = &mut command.failure {
                truncate_utf8(&mut failure.cause, 1024);
                truncate_utf8(&mut failure.code, 80);
                truncate_utf8(&mut failure.stage, 128);
            }
            if let Some(provider) = &mut command.provider {
                provider.bound();
            }
            if let Some(error) = &mut command.error {
                truncate_utf8(error, 1024);
            }
        }
        loop {
            let serialized = serde_json::to_string(&value).expect("serializable run result");
            if serialized.len() <= MAX_RESULT_BYTES {
                return serialized;
            }
            let mut reduced = value.workflow.as_mut().is_some_and(WorkflowResult::shrink);
            for command in [&mut value.agent, &mut value.tests, &mut value.draft_pr]
                .into_iter()
                .flatten()
            {
                if let Some(provider) = &mut command.provider {
                    reduced |= provider.shrink();
                }
                for (text, truncated) in [
                    (&mut command.stdout, &mut command.stdout_truncated),
                    (&mut command.stderr, &mut command.stderr_truncated),
                ] {
                    if !text.is_empty() {
                        truncate_utf8(text, text.len() / 2);
                        *truncated = true;
                        reduced = true;
                    }
                }
            }
            if !reduced {
                reduced = value
                    .workflow
                    .as_mut()
                    .is_some_and(WorkflowResult::shrink_review_evidence);
            }
            if !reduced {
                value.workspace = None;
                value.error = Some("result metadata exceeded the persistence budget".into());
                value.failure = Some(Failure::new(
                    "result_evidence_truncated",
                    "persistence",
                    "result metadata exceeded the persistence budget; retained workspace identity cannot be confirmed from this result",
                ));
                value.stopped_stage = None;
                value.workflow = None;
                value.agent = None;
                value.tests = None;
                value.draft_pr = None;
                // No loop can retain unbounded caller-provided metadata. The bounded
                // failure and resource limits remain visible; workspace=None blocks recovery.
                return serde_json::to_string(&value).expect("serializable bounded fallback");
            }
        }
    }
}

fn failure_for_outcome(outcome: Outcome, stage: &str, cause: String) -> Failure {
    let code = match outcome {
        Outcome::Unknown => "execution_unknown",
        Outcome::TimedOut => "execution_timed_out",
        Outcome::Cancelled => "execution_cancelled",
        _ => "command_failed",
    };
    Failure::new(code, stage, cause)
}
impl RunResult {
    fn complete_metadata(&mut self, policy: &HostConfig, quota: u64) {
        if self.resources.is_none() {
            let usage = self
                .workspace
                .as_deref()
                .map(|path| resources::measure_workspace(path, policy))
                .unwrap_or_else(|| {
                    resources::Usage::unavailable("workspace has not been measured")
                });
            self.resources = Some(ResourceState::new(usage, Some(quota), policy));
        }
        if self.outcome == Outcome::Success {
            self.failure = None;
            return;
        }
        if self.failure.is_some() {
            return;
        }
        for (stage, command) in [
            ("publication", &self.draft_pr),
            ("tests", &self.tests),
            ("developer", &self.agent),
        ] {
            if let Some(command) = command
                && command.outcome != Outcome::Success
            {
                self.failure = command.failure.clone().or_else(|| {
                    Some(failure_for_outcome(
                        command.outcome,
                        stage,
                        command
                            .error
                            .clone()
                            .unwrap_or_else(|| command.stderr.clone()),
                    ))
                });
                return;
            }
        }
        self.failure = Some(failure_for_outcome(
            self.outcome,
            if self.workspace.is_some() {
                "workspace_setup"
            } else {
                "validation"
            },
            self.error
                .clone()
                .unwrap_or_else(|| "execution stopped before the next stage".into()),
        ));
    }
}

pub struct Host {
    config: HostConfig,
    supervisor: PathBuf,
    leases: Mutex<BTreeMap<PathBuf, Arc<File>>>,
}
struct ExecutionLease<'a> {
    leases: &'a Mutex<BTreeMap<PathBuf, Arc<File>>>,
    path: PathBuf,
}
impl Drop for ExecutionLease<'_> {
    fn drop(&mut self) {
        if let Ok(mut leases) = self.leases.lock() {
            leases.remove(&self.path);
        }
    }
}
const SUPERVISOR_LEASE_FD: i32 = 198;
impl Host {
    pub fn new(mut config: HostConfig) -> Result<Self, HostError> {
        if !cfg!(target_os = "linux") {
            return Err(HostError::Config(
                "the trusted host requires Linux subreaper support".into(),
            ));
        }
        if config.timeout_seconds == 0 || config.timeout_seconds > 3600 {
            return Err(HostError::Config(
                "timeout_seconds must be between 1 and 3600".into(),
            ));
        }
        if config.output_limit_bytes == 0 || config.output_limit_bytes > MAX_CAPTURE {
            return Err(HostError::Config(
                "output_limit_bytes must be between 1 and 8192".into(),
            ));
        }
        for (name, bytes) in [
            ("max_snapshot_bytes", config.max_snapshot_bytes),
            ("max_workspace_bytes", config.workspace_byte_limit()),
        ] {
            if !(1..=MAX_STORAGE_BYTES).contains(&bytes) {
                return Err(HostError::Config(format!(
                    "{name} must be between 1 and {MAX_STORAGE_BYTES} bytes (1 TiB)"
                )));
            }
        }
        if config.max_snapshot_entries == 0 || config.max_snapshot_entries > 100_000 {
            return Err(HostError::Config(
                "snapshot limit is outside supported bounds".into(),
            ));
        }
        if config
            .successful_workspace_retention_seconds
            .is_some_and(|seconds| !(60..=31_536_000).contains(&seconds))
        {
            return Err(HostError::Config(
                "successful_workspace_retention_seconds must be 60–31536000, or omitted".into(),
            ));
        }
        if config.max_retained_workspaces == 0 || config.max_retained_workspaces > 10_000 {
            return Err(HostError::Config(
                "max_retained_workspaces must be between 1 and 10000".into(),
            ));
        }
        if serde_json::to_vec(&config)?.len() > MAX_CONFIG_BYTES {
            return Err(HostError::Config("configuration exceeds 256 KiB".into()));
        }
        if config.repositories.is_empty()
            || (config.agents.is_empty() && config.native_agents.is_empty())
        {
            return Err(HostError::Config(
                "at least one repository and agent are required".into(),
            ));
        }
        for name in config
            .repositories
            .keys()
            .chain(config.agents.keys())
            .chain(config.native_agents.keys())
            .chain(config.workflows.keys())
            .chain(config.tests.keys())
            .chain(config.draft_pr_adapters.keys())
        {
            if name.is_empty()
                || name.len() > 128
                || !name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
            {
                return Err(HostError::Config(
                    "profile identifiers must use ASCII letters, numbers, dot, dash, or underscore"
                        .into(),
                ));
            }
        }
        for (name, profile) in &config.native_agents {
            if config.agents.contains_key(name) {
                return Err(HostError::Config(
                    "generic and native agent names must not collide".into(),
                ));
            }
            profile.validate().map_err(HostError::Config)?;
        }
        for profile in config
            .agents
            .values()
            .chain(config.tests.values())
            .chain(config.draft_pr_adapters.values())
        {
            if !profile.program.is_absolute() || !profile.program.is_file() {
                return Err(HostError::Config(
                    "commands require an existing absolute executable path".into(),
                ));
            }
            if serde_json::to_vec(profile)?.len() > 64 * 1024 {
                return Err(HostError::Config("command profile exceeds 64 KiB".into()));
            }
            if profile.args.iter().any(|arg| arg.contains('\0'))
                || profile.env.iter().any(|(key, value)| {
                    key.is_empty() || key.contains(['=', '\0']) || value.contains('\0')
                })
            {
                return Err(HostError::Config(
                    "invalid command argument or environment".into(),
                ));
            }
        }
        for workflow in config.workflows.values() {
            workflow.validate(&config)?;
        }
        config.workspace_root = future_directory(&config.workspace_root)?;
        for repository in config.repositories.values_mut() {
            *repository = repository.canonicalize()?;
            if !repository.is_dir() {
                return Err(HostError::Config("repository must be a directory".into()));
            }
            if config.workspace_root.starts_with(&repository)
                || repository.starts_with(&config.workspace_root)
            {
                return Err(HostError::Config(
                    "workspace root and source repositories must not overlap".into(),
                ));
            }
        }
        fs::create_dir_all(&config.workspace_root)?;
        config.workspace_root = config.workspace_root.canonicalize()?;
        let supervisor = config
            .supervisor_program
            .clone()
            .map(Ok)
            .unwrap_or_else(std::env::current_exe)?;
        if !supervisor.is_absolute() || !supervisor.is_file() {
            return Err(HostError::Config(
                "supervisor must be an existing absolute executable path".into(),
            ));
        }
        Ok(Self {
            config,
            supervisor,
            leases: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn config(&self) -> &HostConfig {
        &self.config
    }

    /// Only execute a newly claimed generation. A pre-existing workspace is unknown,
    /// never evidence that the previous process stopped. No method here requeues.
    pub fn execute(&self, task: &Task, cancellation: Arc<AtomicBool>) -> RunResult {
        let quota = Job::from_payload(&task.payload, &self.config)
            .ok()
            .and_then(|job| job.workspace_quota_bytes)
            .unwrap_or_else(|| self.config.workspace_byte_limit());
        let mut config = self.config.clone();
        config.max_workspace_bytes = Some(quota);
        let attempt = Self {
            config,
            supervisor: self.supervisor.clone(),
            leases: Mutex::new(BTreeMap::new()),
        };
        let mut result = attempt.execute_at_limit(task, cancellation, &self.config);
        result.complete_metadata(&self.config, quota);
        result
    }
    fn execute_at_limit(
        &self,
        task: &Task,
        cancellation: Arc<AtomicBool>,
        policy: &HostConfig,
    ) -> RunResult {
        if task.state != State::Claimed
            || task.id <= 0
            || task.generation <= 0
            || task.owner.is_none()
        {
            return RunResult::new(
                Outcome::Failure,
                Some("execution requires a valid active claim".into()),
            );
        }
        let job = match Job::from_payload(&task.payload, &self.config) {
            Ok(job) => job,
            Err(error) => return RunResult::new(Outcome::Failure, Some(error.to_string())),
        };
        if cancellation.load(Ordering::Acquire)
            && !crate::workspaces::exists_for(&self.config, task, &job)
        {
            return RunResult::new(Outcome::Cancelled, None);
        }
        if job.role_selections.is_some() && job.role_binding.is_none() {
            return RunResult::new(
                Outcome::Failure,
                Some("selected job is missing its server-owned admission binding".into()),
            );
        }
        let workspace_state = match crate::workspaces::prepare(&self.config, task, &job) {
            Ok(value) => value,
            Err(result) => return *result,
        };
        let workspace = workspace_state.path.clone();
        if let Ok(mut leases) = self.leases.lock() {
            leases.insert(workspace.clone(), workspace_state.file.clone());
        } else {
            return RunResult::new(
                Outcome::Unknown,
                Some("workspace ownership registry unavailable".into()),
            );
        }
        let _lease = ExecutionLease {
            leases: &self.leases,
            path: workspace.clone(),
        };
        let mut result =
            self.execute_in(task, &job, &workspace, workspace_state.reused, cancellation);
        result.complete_metadata(policy, self.config.workspace_byte_limit());
        if result.outcome != Outcome::Unknown
            && let Err(error) = crate::sessions::atomic_write(
                &workspace.join("last-result.json"),
                &serde_json::from_str::<serde_json::Value>(&result.to_json())
                    .expect("bounded result"),
            )
        {
            result.outcome = Outcome::Unknown;
            result.error = Some(format!("cannot persist stopped execution result: {error}"));
            result.failure = Some(Failure::new(
                "result_persistence_failed",
                "persistence",
                result.error.clone().unwrap_or_default(),
            ));
        }
        result
    }
    fn execute_in(
        &self,
        task: &Task,
        job: &Job,
        workspace: &Path,
        reused: bool,
        cancellation: Arc<AtomicBool>,
    ) -> RunResult {
        let deadline = Instant::now() + Duration::from_secs(self.config.timeout_seconds);
        let mut result = RunResult::new(Outcome::Failure, None);
        result.workspace = Some(workspace.to_owned());
        let repository = workspace.join("repository");
        let requirements_file = workspace.join("requirements.txt");
        let setup = (|| -> Result<(), HostError> {
            if reused {
                check_workspace_budget(workspace, &self.config)?;
                if repository.canonicalize()? != repository
                    || !fs::symlink_metadata(&repository)?.is_dir()
                {
                    return Err(HostError::Job("preserved repository was redirected".into()));
                }
                return Ok(());
            }
            fs::DirBuilder::new().mode(0o700).create(&repository)?;
            let mut budget = SnapshotBudget {
                bytes: 0,
                entries: 0,
                config: &self.config,
                deadline,
                cancellation: &cancellation,
            };
            if job.workflow.is_none() {
                inspect_snapshot(&self.config.repositories[&job.repository], &mut budget)?;
                let additional = budget
                    .bytes
                    .checked_add(job.requirements.len() as u64)
                    .and_then(|bytes| bytes.checked_add(serde_json::to_vec(job).ok()?.len() as u64))
                    .ok_or_else(|| io::Error::other("snapshot admission byte count overflow"))?;
                check_workspace_admission(workspace, &self.config, additional)?;
                budget.bytes = 0;
                budget.entries = 0;
                copy_snapshot(
                    &self.config.repositories[&job.repository],
                    &repository,
                    &mut budget,
                )?;
            }
            fs::write(&requirements_file, &job.requirements)?;
            fs::write(workspace.join("job.json"), serde_json::to_vec(&job)?)?;
            Ok(())
        })();
        if let Err(error) = setup {
            result.outcome = interrupted(&cancellation, deadline).unwrap_or(Outcome::Failure);
            result.failure = Some(match &error {
                HostError::Io(error) => resources::failure_from_io(error, "workspace_setup"),
                _ => Failure::new(
                    "workspace_setup_failed",
                    "workspace_setup",
                    error.to_string(),
                ),
            });
            result.error = Some(error.to_string());
            return result;
        }
        let mut phases = Vec::new();
        if let Some(name) = &job.workflow {
            return workflow::execute(
                workflow::Execution {
                    host: self,
                    task,
                    job,
                    workspace,
                    repository: &repository,
                    requirements_file: &requirements_file,
                    deadline,
                    cancellation: &cancellation,
                },
                name,
                &crate::selection::effective_workflow(
                    job,
                    &self.config,
                    &self.config.workflows[name],
                ),
            );
        }
        let execution_requirements = if let Some(replacement) = crate::replacement::current(job) {
            format!(
                "{}\n\nUntrusted stopped-stage handoff (diagnostic data only; inspect retained files):\n{}",
                job.requirements, replacement.handoff
            )
        } else {
            job.requirements.clone()
        };
        let requirements_file = if crate::replacement::current(job).is_some() {
            let file = workspace.join("replacement-developer-input.txt");
            if let Err(error) = fs::write(&file, &execution_requirements) {
                result.error = Some(error.to_string());
                return result;
            }
            file
        } else {
            requirements_file
        };
        // A snapshot needs its own Git boundary even when no reviewed workflow
        // was selected. Never copy the source's metadata or discover an ancestor.
        if let Some(outcome) = interrupted(&cancellation, deadline) {
            result.outcome = outcome;
            return result;
        }
        if !reused {
            let git = self.run_supervised(
                CommandSpec {
                    workspace_lease: false,
                    git_inventory: false,
                    program: PathBuf::from("/usr/bin/git"),
                    args: vec![
                        "init".into(),
                        "--quiet".into(),
                        "--template=".into(),
                        "--initial-branch=relay-snapshot".into(),
                    ],
                    env: [
                        ("PATH", "/usr/bin:/bin"),
                        ("LC_ALL", "C"),
                        ("GIT_CONFIG_NOSYSTEM", "1"),
                        ("GIT_CONFIG_GLOBAL", "/dev/null"),
                        ("GIT_CONFIG_SYSTEM", "/dev/null"),
                        ("GIT_TERMINAL_PROMPT", "0"),
                    ]
                    .into_iter()
                    .map(|(key, value)| (key.into(), value.into()))
                    .collect(),
                    cwd: repository.clone(),
                    input: String::new(),
                    timeout_ms: remaining_ms(deadline),
                    output_limit_bytes: MAX_CAPTURE,
                    catalog: false,
                    app_server: None,
                    provider: None,
                    read_only: false,
                    clear_env: true,
                },
                &cancellation,
                workspace,
                "snapshot-git",
            );
            if git.outcome != Outcome::Success {
                result.outcome = git.outcome;
                result.failure = git.failure;
                result.error = Some(format!(
                    "cannot initialize private snapshot Git repository: {}",
                    git.error.unwrap_or_else(|| "Git init failed".into())
                ));
                return result;
            }
            if let Err(error) = OpenOptions::new()
                .append(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(repository.join(".git/config"))
                .and_then(|mut config| {
                    config.write_all(b"\n[core]\n\thooksPath = /dev/null\n\tfsmonitor = false\n")
                })
            {
                result.error = Some(format!("cannot configure private snapshot Git: {error}"));
                return result;
            }
            if let Err(error) = crate::workspaces::mark_ready(workspace) {
                result.error = Some(error.to_string());
                return result;
            }
        }
        if let Some(profile) = crate::selection::native_profile(job, &self.config, false)
            .expect("validated role selection")
        {
            let mut command = self.run_native(
                &profile,
                &execution_requirements,
                false,
                &cancellation,
                (workspace, &repository, false),
                deadline,
            );
            crate::selection::annotate(&mut command, job, &self.config, false);
            result.outcome = command.outcome;
            result.agent = Some(command);
            if result.outcome != Outcome::Success {
                if result.outcome != Outcome::Unknown {
                    result.stopped_stage = Some(crate::replacement::StoppedStage::developer(
                        0, None, None, 0,
                    ));
                }
                return result;
            }
        } else {
            phases.push(("agent", &self.config.agents[&job.agent]));
        }
        if let Some(test) = &job.test {
            phases.push(("tests", &self.config.tests[test]));
        }
        if job.publish {
            phases.push((
                "draft_pr",
                &self.config.draft_pr_adapters
                    [job.draft_pr_adapter.as_ref().expect("validated adapter")],
            ));
        }
        for (phase, profile) in phases {
            if phase == "draft_pr"
                && let Err(error) = crate::workspaces::mark_publication(workspace, task)
            {
                result.error = Some(error.to_string());
                return result;
            }
            if let Some(outcome) = interrupted(&cancellation, deadline) {
                result.outcome = outcome;
                return result;
            }
            let expand = |arg: &str| -> String {
                match arg {
                    "{requirements}" => execution_requirements.clone(),
                    "{requirements_file}" => requirements_file.to_string_lossy().into_owned(),
                    "{workspace}" => repository.to_string_lossy().into_owned(),
                    "{repository}" => job.repository.clone(),
                    "{task_id}" => task.id.to_string(),
                    "{generation}" => task.generation.to_string(),
                    _ => arg.to_owned(),
                }
            };
            let mut env = profile.env.clone();
            for (key, value) in [
                ("RELAY_REQUIREMENTS", execution_requirements.clone()),
                (
                    "RELAY_REQUIREMENTS_FILE",
                    requirements_file.to_string_lossy().into_owned(),
                ),
                ("RELAY_WORKSPACE", repository.to_string_lossy().into_owned()),
                ("RELAY_REPOSITORY", job.repository.clone()),
                ("RELAY_TASK_ID", task.id.to_string()),
                ("RELAY_GENERATION", task.generation.to_string()),
                (
                    "RELAY_DRAFT_PR",
                    if phase == "draft_pr" { "true" } else { "false" }.into(),
                ),
            ] {
                env.insert(key.into(), value);
            }
            let spec = CommandSpec {
                workspace_lease: false,
                git_inventory: false,
                catalog: false,
                app_server: None,
                provider: None,
                read_only: false,
                clear_env: false,
                program: profile.program.clone(),
                args: profile.args.iter().map(|arg| expand(arg)).collect(),
                env,
                cwd: repository.clone(),
                input: execution_requirements.clone(),
                timeout_ms: deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .max(1) as u64,
                output_limit_bytes: self.config.output_limit_bytes,
            };
            let command = self.run_supervised(spec, &cancellation, workspace, phase);
            let outcome = command.outcome;
            match phase {
                "agent" => result.agent = Some(command),
                "tests" => result.tests = Some(command),
                _ => result.draft_pr = Some(command),
            }
            result.outcome = outcome;
            if outcome != Outcome::Success {
                if phase == "agent" && outcome != Outcome::Unknown {
                    result.stopped_stage = Some(crate::replacement::StoppedStage::developer(
                        0, None, None, 0,
                    ));
                }
                return result;
            }
        }
        result.outcome = Outcome::Success;
        result
    }

    /// Probe only --version/--help. Authentication and model availability are not tested.
    pub fn probe_native(&self, name: &str, read_only: bool) -> Result<ProviderProbe, HostError> {
        static NEXT_PROBE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let profile = self
            .config
            .native_agents
            .get(name)
            .ok_or_else(|| HostError::Config("native profile is not allowlisted".into()))?;
        let workspace = self.config.workspace_root.join(format!(
            ".probe-{}-{}",
            std::process::id(),
            NEXT_PROBE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&workspace)?;
        fs::DirBuilder::new()
            .mode(0o700)
            .create(workspace.join("repository"))?;
        let result = self.probe_profile(
            profile,
            read_only,
            &AtomicBool::new(false),
            &workspace,
            Instant::now() + Duration::from_secs(10),
        );
        // Unknown is deliberately retained for operator inspection, just like jobs.
        if result
            .as_ref()
            .err()
            .is_none_or(|failure| failure.outcome != Outcome::Unknown)
        {
            fs::remove_dir_all(&workspace)?;
        }
        result.map_err(|failure| {
            HostError::Config(format!(
                "native probe {:?}: {}",
                failure.outcome,
                failure.error.unwrap_or_else(|| "CLI probe failed".into())
            ))
        })
    }

    pub(crate) fn probe_profile(
        &self,
        profile: &NativeProfile,
        read_only: bool,
        cancellation: &AtomicBool,
        workspace: &Path,
        deadline: Instant,
    ) -> Result<ProviderProbe, Box<CommandResult>> {
        let deadline = deadline.min(Instant::now() + Duration::from_secs(10));
        let probe = |phase: &str, args: Vec<String>| -> Result<CommandResult, Box<CommandResult>> {
            if let Some(outcome) = interrupted(cancellation, deadline) {
                return Err(Box::new(CommandResult::error(
                    outcome,
                    "native compatibility probe interrupted",
                )));
            }
            let spec = CommandSpec {
                workspace_lease: false,
                git_inventory: false,
                catalog: false,
                app_server: None,
                provider: None,
                read_only: false,
                clear_env: false,
                program: profile.program.clone(),
                args,
                env: profile.env.clone(),
                cwd: workspace.join("repository"),
                input: String::new(),
                timeout_ms: remaining_ms(deadline),
                output_limit_bytes: MAX_PROBE_CAPTURE,
            };
            let mut response = self.run_supervised(spec, cancellation, workspace, phase);
            if response.error.is_some() {
                if response.outcome == Outcome::Success {
                    response.outcome = Outcome::Failure;
                }
                return Err(Box::new(response));
            }
            if !matches!(response.outcome, Outcome::Success | Outcome::Failure) {
                return Err(Box::new(response));
            }
            if response.stdout_truncated || response.stderr_truncated {
                return Err(Box::new(CommandResult::error(
                    Outcome::Failure,
                    format!("native {phase} probe output exceeded its bound"),
                )));
            }
            Ok(response)
        };
        let mut responses = Vec::new();
        for (phase, args) in [
            ("version", vec!["--version".into()]),
            ("help", profile.help_args()),
        ] {
            let response = probe(phase, args)?;
            if response.outcome != Outcome::Success {
                return Err(Box::new(CommandResult::error(
                    response.outcome,
                    format!("native {phase} probe did not complete successfully"),
                )));
            }
            responses.push(format!("{}\n{}", response.stdout, response.stderr));
        }
        let hidden_max_turns = profile.hidden_max_turns_probe_needed(&responses[1]);
        if hidden_max_turns {
            // Only this documented hidden flag has a fallback. Check the version
            // and every other required capability before making additional probes.
            let mut without_turns = profile.clone();
            without_turns.max_turns = None;
            without_turns
                .validate_probe(&responses[0], &responses[1], read_only)
                .map_err(|error| CommandResult::error(Outcome::Failure, error))?;
            // A successful --help can ignore unknown options. Require the parser
            // to specifically recognize a missing argument, then accept our value.
            // Both invocations retain --help and have no prompt/model operation.
            let missing = probe(
                "max-turns-missing",
                vec!["--help".into(), "--max-turns".into()],
            )?;
            let diagnostic = format!("{}\n{}", missing.stdout, missing.stderr);
            let recognized = diagnostic.lines().any(|line| {
                line.trim()
                    .strip_prefix("error: option '--max-turns <")
                    .and_then(|rest| rest.strip_suffix(">' argument missing"))
                    .is_some_and(|name| {
                        !name.is_empty()
                            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                    })
            });
            if missing.outcome != Outcome::Failure
                || missing.exit_code != Some(1)
                || missing.signal.is_some()
                || !recognized
            {
                return Err(Box::new(CommandResult::error(
                    Outcome::Failure,
                    "CLI could not verify hidden --max-turns argument parsing",
                )));
            }
            let valid = probe(
                "max-turns-help",
                vec![
                    "--max-turns".into(),
                    profile.max_turns.expect("configured max_turns").to_string(),
                    "--help".into(),
                ],
            )?;
            if valid.outcome != Outcome::Success {
                return Err(Box::new(CommandResult::error(
                    valid.outcome,
                    "CLI rejected configured --max-turns in help probe",
                )));
            }
        }
        let cli_version = profile
            .validate_probe_with_max_turns(
                &responses[0],
                &responses[1],
                read_only,
                hidden_max_turns,
            )
            .map_err(|error| CommandResult::error(Outcome::Failure, error))?;
        Ok(ProviderProbe {
            provider: profile.provider,
            cli_version,
            read_only_supported: profile
                .validate_probe_with_max_turns(&responses[0], &responses[1], true, hidden_max_turns)
                .is_ok(),
        })
    }

    pub(crate) fn run_native(
        &self,
        profile: &NativeProfile,
        input: &str,
        read_only: bool,
        cancellation: &AtomicBool,
        paths: (&Path, &Path, bool),
        deadline: Instant,
    ) -> CommandResult {
        let (workspace, repository, require_resume) = paths;
        let mut compiled = match profile.compile(read_only) {
            Ok(compiled) => compiled,
            Err(error) => {
                let mut result = CommandResult::error(Outcome::Failure, error);
                result.provider = Some(ProviderResult::new(profile, None));
                return result;
            }
        };
        let probe = match self.probe_profile(profile, read_only, cancellation, workspace, deadline)
        {
            Ok(probe) => probe,
            Err(failure) => {
                let mut failure = *failure;
                failure.provider = Some(ProviderResult::new(profile, None));
                return failure;
            }
        };
        if let Some(outcome) = interrupted(cancellation, deadline) {
            let mut failure =
                CommandResult::error(outcome, "native command interrupted before execution");
            failure.provider = Some(ProviderResult::new(profile, Some(probe.cli_version)));
            return failure;
        }
        let session = if crate::sessions::enabled(profile) {
            match crate::workspaces::attempt(workspace).and_then(|attempt| {
                crate::sessions::Session::begin(
                    workspace,
                    repository,
                    profile,
                    read_only,
                    attempt,
                    require_resume
                        || (crate::workspaces::role_epoch(workspace, read_only)?.is_some()
                            && !crate::workspaces::fresh_role_epoch(workspace, read_only)?),
                    crate::workspaces::role_epoch(workspace, read_only)?.as_deref(),
                )
            }) {
                Ok(session) => {
                    if let Err(error) =
                        crate::workspaces::consume_fresh_role_epoch(workspace, read_only)
                    {
                        return CommandResult::error(
                            Outcome::Failure,
                            format!("cannot checkpoint active role epoch: {error}"),
                        );
                    }
                    Some(session)
                }
                Err(error) => {
                    return CommandResult::error(
                        Outcome::Failure,
                        format!("cannot start bound session: {error}"),
                    );
                }
            }
        } else {
            None
        };
        if profile.provider == ProviderKind::ClaudeCli
            && let Some(session) = &session
        {
            if let Some(id) = &session.resume {
                compiled.args.extend(["--resume".into(), id.clone()]);
            } else if let Some(id) = &session.fresh_claude {
                compiled.args.extend(["--session-id".into(), id.clone()]);
            }
        }
        let spec = CommandSpec {
            workspace_lease: false,
            git_inventory: false,
            catalog: false,
            app_server: if profile.provider == ProviderKind::CodexAppServer {
                Some(crate::app_server::Start {
                    cwd: repository.to_owned(),
                    prompt: input.to_owned(),
                    model: profile.model.clone(),
                    effort: profile.effort.clone(),
                    native_permission: profile.native_permission,
                    resume: session.as_ref().and_then(|s| s.resume.clone()),
                    checkpoint: session.as_ref().map(|s| s.checkpoint()),
                })
            } else {
                None
            },
            provider: Some(ProviderResult::new(profile, Some(probe.cli_version))),
            read_only,
            clear_env: false,
            program: compiled.program,
            args: compiled.args,
            env: compiled.env,
            cwd: repository.to_owned(),
            input: input.to_owned(),
            timeout_ms: remaining_ms(deadline),
            output_limit_bytes: self.config.output_limit_bytes,
        };
        let metadata = spec.provider.clone();
        let mut result = self.run_supervised(
            spec,
            cancellation,
            workspace,
            if read_only { "review" } else { "agent" },
        );
        if result.provider.is_none() {
            result.provider = metadata;
        }
        if result.outcome == Outcome::Success
            && let Some(session) = session
            && let Err(error) = session.complete(
                result
                    .provider
                    .as_ref()
                    .and_then(|p| p.session_id.as_deref()),
            )
        {
            result.outcome = Outcome::Failure;
            result.error = Some(format!("cannot persist completed session: {error}"));
        }
        result
    }

    pub(crate) fn run_supervised(
        &self,
        spec: CommandSpec,
        cancellation: &AtomicBool,
        workspace: &Path,
        phase: &str,
    ) -> CommandResult {
        let mut result = self.run_supervised_inner(spec, cancellation, workspace, phase);
        if result.outcome != Outcome::Success && result.failure.is_none() {
            result.failure = Some(failure_for_outcome(
                result.outcome,
                phase,
                result.error.clone().unwrap_or_else(|| {
                    if result.stderr.is_empty() {
                        format!("command stopped with exit {:?}", result.exit_code)
                    } else {
                        result.stderr.clone()
                    }
                }),
            ));
        }
        result
    }
    fn run_supervised_inner(
        &self,
        mut spec: CommandSpec,
        cancellation: &AtomicBool,
        workspace: &Path,
        phase: &str,
    ) -> CommandResult {
        // Reject an already oversized task before starting any command or model.
        if let Err(error) = check_workspace_budget(workspace, &self.config) {
            let mut result = CommandResult::error(Outcome::Failure, error.to_string());
            result.failure = Some(resources::failure_from_io(&error, phase));
            return result;
        }
        let lease = match self.leases.lock() {
            Ok(leases) => leases.get(workspace).cloned(),
            Err(_) => {
                return CommandResult::error(
                    Outcome::Unknown,
                    "workspace ownership registry unavailable",
                );
            }
        };
        spec.workspace_lease = lease.is_some();
        let mut serialized = serde_json::to_vec(&spec).expect("serializable command spec");
        if serialized.len() > MAX_CONFIG_BYTES {
            return CommandResult::error(
                Outcome::Failure,
                "expanded command configuration exceeds 256 KiB",
            );
        }
        serialized.push(b'\n');
        let mut supervisor = Command::new(&self.supervisor);
        if let Some(lease) = &lease {
            let fd = lease.as_raw_fd();
            // SAFETY: only async-signal-safe descriptor operations run in the forked
            // child. This shared open-file description retains flock across parent death.
            unsafe {
                supervisor.pre_exec(move || {
                    if libc::dup2(fd, SUPERVISOR_LEASE_FD) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::fcntl(SUPERVISOR_LEASE_FD, libc::F_SETFD, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let mut child = match supervisor
            .env_remove("RELAY_TOKEN")
            .arg("__relay_host_supervisor")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                return CommandResult::error(
                    Outcome::Failure,
                    format!("cannot start supervisor: {error}"),
                );
            }
        };
        let pid = child.id();
        let _ = fs::write(
            workspace.join(format!("{phase}-supervisor.json")),
            serde_json::json!({"pid":pid,"phase":phase}).to_string(),
        );
        let watchdog = Instant::now()
            + Duration::from_millis(spec.timeout_ms)
            + CLEANUP_TIMEOUT
            + Duration::from_secs(2);
        let input = child.stdin.take().expect("piped supervisor stdin");
        let mut stdout = child.stdout.take().expect("piped supervisor stdout");
        let mut stderr = child.stderr.take().expect("piped supervisor stderr");
        if let Err(error) = nonblocking(&input)
            .and_then(|_| nonblocking(&stdout))
            .and_then(|_| nonblocking(&stderr))
        {
            drop(input);
            reap_later(child);
            return CommandResult::error(
                Outcome::Unknown,
                format!("supervisor initialization failed (pid {pid}): {error}"),
            );
        }
        let mut input = Some(input);
        let mut input_offset = 0;
        let mut resource_error = None;
        let mut next_workspace_check = Instant::now();
        // JSON can escape each inventory byte to six bytes. stderr retains its
        // ordinary control-output budget; no task log budget is increased.
        let mut output = Capture::new(if spec.catalog {
            2 * crate::capabilities::MAX_CATALOG_RESPONSE_BYTES
        } else if spec.git_inventory {
            6 * (crate::git_inventory::MAX_BYTES + MAX_PROBE_CAPTURE) + 128 * 1024
        } else if spec.output_limit_bytes > MAX_CAPTURE {
            1024 * 1024
        } else {
            128 * 1024
        });
        let mut errors = Capture::new(1024);
        loop {
            if cancellation.load(Ordering::Acquire) || resource_error.is_some() {
                input.take();
            }
            if let Some(writer) = &mut input
                && input_offset < serialized.len()
            {
                match writer.write(&serialized[input_offset..]) {
                    Ok(written) => input_offset += written,
                    Err(error)
                        if error.kind() == io::ErrorKind::WouldBlock
                            || error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        input.take();
                    }
                }
            }
            if Instant::now() >= next_workspace_check && resource_error.is_none() {
                resource_error = check_workspace_budget(workspace, &self.config).err();
                next_workspace_check = Instant::now() + Duration::from_millis(250);
                if resource_error.is_some() {
                    input.take();
                }
            }
            if let Err(error) = output
                .drain(&mut stdout)
                .and_then(|_| errors.drain(&mut stderr))
            {
                input.take();
                reap_later(child);
                return CommandResult::error(
                    Outcome::Unknown,
                    format!("supervisor communication failed (pid {pid}): {error}"),
                );
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    if output
                        .drain_to_eof(&mut stdout, None, watchdog)
                        .and_then(|_| errors.drain_to_eof(&mut stderr, None, watchdog))
                        .is_err()
                    {
                        return CommandResult::error(
                            Outcome::Unknown,
                            "supervisor response did not reach EOF within its bound",
                        );
                    }
                    if status.success()
                        && !output.truncated
                        && let Ok(mut result) =
                            serde_json::from_slice::<CommandResult>(&output.bytes)
                    {
                        result.supervisor_pid = Some(pid);
                        if result.outcome != Outcome::Unknown {
                            let resource_error = resource_error
                                .or_else(|| check_workspace_budget(workspace, &self.config).err());
                            if let Some(error) = resource_error {
                                result.outcome = Outcome::Failure;
                                result.failure = Some(resources::failure_from_io(&error, phase));
                                result.error = Some(error.to_string());
                            } else if cancellation.load(Ordering::Acquire) {
                                result.outcome = Outcome::Cancelled;
                            }
                        }
                        return result;
                    }
                    return CommandResult::error(
                        Outcome::Unknown,
                        format!(
                            "supervisor exited without verified cleanup (pid {pid}, status {status}): {}",
                            errors.text()
                        ),
                    );
                }
                Ok(None) => {}
                Err(error) => {
                    input.take();
                    reap_later(child);
                    return CommandResult::error(
                        Outcome::Unknown,
                        format!("cannot wait for supervisor (pid {pid}): {error}"),
                    );
                }
            }
            if Instant::now() >= watchdog {
                input.take();
                reap_later(child);
                return CommandResult::error(
                    Outcome::Unknown,
                    format!(
                        "supervisor cleanup was not confirmed within its bound (pid {pid}); do not requeue automatically"
                    ),
                );
            }
            thread::sleep(TICK);
        }
    }
}

fn future_directory(path: &Path) -> io::Result<PathBuf> {
    if path
        .components()
        .any(|component| component == std::path::Component::ParentDir)
    {
        return Err(io::Error::other(
            "workspace path cannot contain parent traversal",
        ));
    }
    let mut existing = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut suffix = Vec::new();
    while !existing.exists() {
        suffix.push(
            existing
                .file_name()
                .ok_or_else(|| io::Error::other("invalid workspace root"))?
                .to_os_string(),
        );
        if !existing.pop() {
            return Err(io::Error::other("invalid workspace root"));
        }
    }
    let mut resolved = existing.canonicalize()?;
    for component in suffix.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn remaining_ms(deadline: Instant) -> u64 {
    deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .max(1) as u64
}

fn reap_later(mut child: Child) {
    thread::spawn(move || {
        let _ = child.wait();
    });
}
fn interrupted(cancellation: &AtomicBool, deadline: Instant) -> Option<Outcome> {
    if cancellation.load(Ordering::Acquire) {
        Some(Outcome::Cancelled)
    } else if Instant::now() >= deadline {
        Some(Outcome::TimedOut)
    } else {
        None
    }
}

struct SnapshotBudget<'a> {
    bytes: u64,
    entries: usize,
    config: &'a HostConfig,
    deadline: Instant,
    cancellation: &'a AtomicBool,
}
impl SnapshotBudget<'_> {
    fn check(&self) -> io::Result<()> {
        if interrupted(self.cancellation, self.deadline).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "snapshot interrupted",
            ));
        }
        Ok(())
    }
}
// Metadata-only lower-bound admission; copy_snapshot still checks changing inputs.
fn inspect_snapshot(source: &Path, budget: &mut SnapshotBudget<'_>) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
        budget.check()?;
        let entry = entry?;
        if snapshot_excluded(&entry.file_name()) {
            continue;
        }
        budget.entries += 1;
        if budget.entries > budget.config.max_snapshot_entries {
            return Err(resources::budget_error(
                "snapshot_entry_limit_exceeded",
                resources::Usage::observed(budget.bytes, false),
                budget.config.max_snapshot_entries as u64,
                "snapshot entry limit exceeded",
            ));
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            inspect_snapshot(&entry.path(), budget)?;
        } else if metadata.is_file() {
            budget.bytes = budget
                .bytes
                .checked_add(metadata.len())
                .ok_or_else(|| io::Error::other("snapshot admission byte count overflow"))?;
            if budget.bytes > budget.config.max_snapshot_bytes {
                return Err(resources::budget_error(
                    "snapshot_limit_exceeded",
                    resources::Usage::observed(budget.bytes, false),
                    budget.config.max_snapshot_bytes,
                    format!(
                        "snapshot byte limit exceeded: at least {} logical bytes, max_snapshot_bytes={}",
                        budget.bytes, budget.config.max_snapshot_bytes
                    ),
                ));
            }
        } else {
            return Err(io::Error::other(
                "snapshot rejects symlink or special files",
            ));
        }
    }
    Ok(())
}
fn snapshot_excluded(name: &std::ffi::OsStr) -> bool {
    [".git", "target", "node_modules"]
        .iter()
        .any(|excluded| name == *excluded)
}

fn copy_snapshot(
    source: &Path,
    destination: &Path,
    budget: &mut SnapshotBudget<'_>,
) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
        budget.check()?;
        let entry = entry?;
        let name = entry.file_name();
        if snapshot_excluded(&name) {
            continue;
        }
        budget.entries += 1;
        if budget.entries > budget.config.max_snapshot_entries {
            return Err(resources::budget_error(
                "snapshot_entry_limit_exceeded",
                resources::Usage::observed(budget.bytes, false),
                budget.config.max_snapshot_entries as u64,
                "snapshot entry limit exceeded",
            ));
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        let target = destination.join(&name);
        if metadata.file_type().is_symlink() {
            return Err(io::Error::other(format!(
                "snapshot rejects symlink: {}",
                entry.path().display()
            )));
        }
        if metadata.is_dir() {
            fs::DirBuilder::new().mode(0o700).create(&target)?;
            copy_snapshot(&entry.path(), &target, budget)?;
        } else if metadata.is_file() {
            let mut source = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(entry.path())?;
            if !source.metadata()?.is_file() {
                return Err(io::Error::other("snapshot source changed file type"));
            }
            let mut target = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(target)?;
            let mut buffer = [0; 64 * 1024];
            loop {
                budget.check()?;
                let read = source.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                budget.bytes = budget.bytes.saturating_add(read as u64);
                if budget.bytes > budget.config.max_snapshot_bytes {
                    return Err(resources::budget_error(
                        "snapshot_limit_exceeded",
                        resources::Usage::observed(budget.bytes, false),
                        budget.config.max_snapshot_bytes,
                        "snapshot byte limit exceeded",
                    ));
                }
                target.write_all(&buffer[..read])?;
            }
            // Preserve executable bits only; never inherit writable group/world permissions.
            use std::os::unix::fs::PermissionsExt;
            target.set_permissions(fs::Permissions::from_mode(
                0o600 | (metadata.permissions().mode() & 0o111),
            ))?;
        } else {
            return Err(io::Error::other("snapshot rejects special files"));
        }
    }
    Ok(())
}

/// Best-effort admission/running limit, not a disk quota or a filesystem sandbox.
fn check_workspace_budget(root: &Path, config: &HostConfig) -> io::Result<()> {
    workspace_bytes(root, config).map(|_| ())
}

/// Includes the whole task tree; callers can check an estimated additional copy.
pub(crate) fn check_workspace_admission(
    root: &Path,
    config: &HostConfig,
    additional_bytes: u64,
) -> io::Result<()> {
    let present = workspace_bytes(root, config)?;
    let required = present
        .checked_add(additional_bytes)
        .ok_or_else(|| io::Error::other("workspace admission byte count overflow"))?;
    if required > config.workspace_byte_limit() {
        return Err(resources::admission_error(
            present,
            required,
            config.workspace_byte_limit(),
            format!(
                "workspace admission estimate is {required} logical bytes ({present} present + {additional_bytes} planned file bytes), exceeding max_workspace_bytes={} (omitted: max_snapshot_bytes); Git objects, checkpoints and build growth may require more",
                config.workspace_byte_limit()
            ),
        ));
    }
    Ok(())
}

fn workspace_bytes(root: &Path, config: &HostConfig) -> io::Result<u64> {
    let mut directories = vec![root.to_path_buf()];
    let mut entries = 0usize;
    let mut bytes = 0u64;
    while let Some(directory) = directories.pop() {
        let listing = match fs::read_dir(directory) {
            Ok(listing) => listing,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in listing {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let metadata = match fs::symlink_metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            entries += 1;
            if entries > config.max_snapshot_entries {
                return Err(resources::budget_error(
                    "workspace_entry_limit_exceeded",
                    resources::Usage::observed(bytes, false),
                    config.max_snapshot_entries as u64,
                    "running workspace entry limit exceeded",
                ));
            }
            if metadata.is_dir() {
                directories.push(entry.path());
            } else if metadata.is_file() {
                bytes = bytes.saturating_add(metadata.len());
            }
            // Never follow new symlinks while inspecting a running workspace.
            if bytes > config.workspace_byte_limit() {
                return Err(resources::budget_error(
                    "workspace_quota_exceeded",
                    resources::Usage::observed(bytes, false),
                    config.workspace_byte_limit(),
                    format!(
                        "running workspace byte limit exceeded: at least {bytes} logical bytes, max_workspace_bytes={} (omitted: max_snapshot_bytes); includes Git metadata, reviewer copies and task control files",
                        config.workspace_byte_limit()
                    ),
                ));
            }
        }
    }
    Ok(bytes)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommandSpec {
    #[serde(default)]
    pub(crate) git_inventory: bool,
    #[serde(default)]
    pub(crate) workspace_lease: bool,
    #[serde(default)]
    pub(crate) provider: Option<ProviderResult>,
    #[serde(default)]
    pub(crate) app_server: Option<crate::app_server::Start>,
    #[serde(default)]
    pub(crate) catalog: bool,
    #[serde(default)]
    pub(crate) read_only: bool,
    #[serde(default)]
    pub(crate) clear_env: bool,
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<String>,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) cwd: PathBuf,
    pub(crate) input: String,
    pub(crate) timeout_ms: u64,
    pub(crate) output_limit_bytes: usize,
}

/// The binary must dispatch its private `__relay_host_supervisor` mode here before
/// starting HTTP/CLI handling. Input is a bounded host-generated JSON line. This
/// entry point is available only to the already-trusted local OS account.
pub fn supervisor_main() -> i32 {
    let result = (|| -> io::Result<CommandResult> {
        let mut bytes = Vec::new();
        let mut stdin = io::stdin();
        let mut byte = [0u8; 1];
        while bytes.len() <= MAX_CONFIG_BYTES {
            if stdin.read(&mut byte)? == 0 {
                return Err(io::Error::other("missing supervisor input"));
            }
            if byte[0] == b'\n' {
                break;
            }
            bytes.push(byte[0]);
        }
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(io::Error::other("supervisor input too large"));
        }
        let spec: CommandSpec = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if !spec.program.is_absolute()
            || !spec.cwd.is_absolute()
            || spec.timeout_ms == 0
            || spec.timeout_ms > 3_600_000
            || spec.output_limit_bytes == 0
            || spec.output_limit_bytes > MAX_PROBE_CAPTURE
            || spec.input.len() > MAX_PHASE_INPUT
            || (spec.catalog
                && (spec.provider.is_none()
                    || spec.app_server.is_some()
                    || spec.git_inventory
                    || !spec.input.is_empty()))
        {
            return Err(io::Error::other("invalid supervisor limits"));
        }
        if spec.workspace_lease {
            // Retain the inherited lock for this supervisor's entire lifetime, but
            // never let model/test grandchildren keep it after verified cleanup.
            // SAFETY: the host supplied this private descriptor via pre_exec.
            if unsafe { libc::fcntl(SUPERVISOR_LEASE_FD, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        // SAFETY: This dedicated single-threaded subprocess has no other jobs.
        if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        nonblocking(&stdin)?;
        Ok(supervise(spec, stdin))
    })();
    match result {
        Ok(result) => {
            if serde_json::to_writer(io::stdout(), &result).is_ok() {
                0
            } else {
                1
            }
        }
        Err(error) => {
            let result = CommandResult::error(
                Outcome::Failure,
                format!("supervisor setup failed: {error}"),
            );
            if serde_json::to_writer(io::stdout(), &result).is_ok() {
                0
            } else {
                1
            }
        }
    }
}

fn supervise(spec: CommandSpec, mut control: io::Stdin) -> CommandResult {
    let started = Instant::now();
    let mut command = Command::new(&spec.program);
    if spec.clear_env {
        command.env_clear();
    }
    // Do not let inherited Git redirections select the service's repository.
    // Trusted per-command values (such as the workflow index) remain supported.
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_SHALLOW_FILE",
    ] {
        command.env_remove(key);
    }
    // Keep discovery bounded even if an agent removes its private .git directory.
    if let Some(parent) = spec.cwd.parent() {
        let ceiling = match std::env::join_paths([parent]) {
            Ok(ceiling) => ceiling,
            Err(error) => {
                return CommandResult::error(
                    Outcome::Failure,
                    format!("cannot encode private Git discovery ceiling: {error}"),
                );
            }
        };
        command.env("GIT_CEILING_DIRECTORIES", ceiling);
    }
    let mut child = match command
        .args(&spec.args)
        .envs(&spec.env)
        .env_remove("RELAY_TOKEN")
        .current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return CommandResult::error(
                Outcome::Failure,
                format!("cannot start configured command: {error}"),
            );
        }
    };
    let pid = child.id() as libc::pid_t;
    let mut stdout = child.stdout.take().expect("piped command stdout");
    let mut stderr = child.stderr.take().expect("piped command stderr");
    let input = child.stdin.take().expect("piped command stdin");
    let setup = nonblocking(&stdout)
        .and_then(|_| nonblocking(&stderr))
        .and_then(|_| nonblocking(&input));
    let mut input = Some(input);
    let mut input_offset = 0;
    let mut output = Capture::new(if spec.git_inventory {
        crate::git_inventory::MAX_BYTES
    } else {
        spec.output_limit_bytes
    });
    if spec.git_inventory {
        output.inventory = Some(crate::git_inventory::Framing::default());
    }
    let bidirectional = spec.app_server.is_some() || spec.catalog;
    let mut protocol = spec.provider.map(|result| {
        if spec.catalog {
            ProtocolParser::catalog(result)
        } else if let Some(start) = spec.app_server {
            ProtocolParser::app_server(result, start)
        } else {
            ProtocolParser::new(result).read_only(spec.read_only)
        }
    });
    let mut input_bytes = if bidirectional {
        protocol.as_mut().expect("native protocol").pending()
    } else {
        spec.input.into_bytes()
    };
    let mut errors = Capture::new(spec.output_limit_bytes);
    let mut error = setup.err().map(|error| error.to_string());
    let mut outcome = Outcome::Failure;
    let mut status = None;
    let mut control_buffer = [0; 32];
    while error.is_none() {
        // WNOWAIT preserves the leader PID until the entire tree has been signaled,
        // avoiding a recycled process-group ID between observation and termination.
        match observed_exit(pid) {
            Ok(Some(exit)) => {
                status = Some(exit);
                outcome = if exit.0 == Some(0) {
                    Outcome::Success
                } else {
                    Outcome::Failure
                };
                break;
            }
            Ok(None) => {}
            Err(failure) => {
                error = Some(failure.to_string());
                break;
            }
        }
        match control.read(&mut control_buffer) {
            Ok(_) => {
                outcome = Outcome::Cancelled;
                break;
            }
            Err(failure) if failure.kind() == io::ErrorKind::WouldBlock => {}
            Err(failure) if failure.kind() == io::ErrorKind::Interrupted => continue,
            Err(failure) => {
                error = Some(failure.to_string());
                break;
            }
        }
        if started.elapsed() >= Duration::from_millis(spec.timeout_ms) {
            outcome = Outcome::TimedOut;
            break;
        }
        if let Err(failure) = output
            .drain_protocol(&mut stdout, protocol.as_mut())
            .and_then(|_| errors.drain(&mut stderr))
        {
            error = Some(failure.to_string());
            break;
        }
        if !bidirectional && let Some(failure) = protocol.as_ref().and_then(ProtocolParser::failure)
        {
            error = Some(failure.to_owned());
            outcome = Outcome::Failure;
            break;
        }
        if bidirectional && input_offset == input_bytes.len() {
            input_bytes = protocol.as_mut().expect("native protocol").pending();
            input_offset = 0;
            if input_bytes.is_empty() && protocol.as_ref().is_some_and(ProtocolParser::stopped) {
                error = protocol
                    .as_ref()
                    .and_then(ProtocolParser::failure)
                    .map(str::to_owned);
                outcome = if error.is_some() {
                    Outcome::Failure
                } else {
                    Outcome::Success
                };
                break;
            }
        }
        if let Some(writer) = &mut input {
            match writer.write(&input_bytes[input_offset..]) {
                Ok(written) => {
                    input_offset += written;
                    if !bidirectional && input_offset == input_bytes.len() {
                        input.take();
                    }
                }
                Err(failure)
                    if failure.kind() == io::ErrorKind::WouldBlock
                        || failure.kind() == io::ErrorKind::Interrupted => {}
                Err(failure) if failure.kind() == io::ErrorKind::BrokenPipe => {
                    input.take();
                    if bidirectional {
                        error = Some("app-server closed protocol stdin".into());
                        break;
                    }
                }
                Err(failure) => {
                    error = Some(failure.to_string());
                    break;
                }
            }
        }
        thread::sleep(TICK);
    }
    drop(input);
    // Even successful command leaders may leave background jobs. Never proceed to
    // tests or publication until the group and adopted descendants are stopped.
    // SAFETY: pid is the unreaped child group leader created above.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let cleanup_deadline = Instant::now() + CLEANUP_TIMEOUT;
    let clean = loop {
        // Subreaper adoption also covers descendants that called setsid/setpgid.
        // These IDs are our own unreaped children, not arbitrary PID-file contents.
        match direct_children() {
            Ok(children) => {
                for child in children {
                    // SAFETY: a current direct child is retained until waitpid below.
                    unsafe {
                        libc::kill(child, libc::SIGKILL);
                    }
                }
            }
            Err(failure) => error = Some(format!("cannot inspect adopted descendants: {failure}")),
        }
        let mut raw_status = 0;
        // SAFETY: only this dedicated supervisor owns these children.
        let waited = unsafe { libc::waitpid(-1, &mut raw_status, libc::WNOHANG) };
        if waited == pid && status.is_none() {
            status = Some(decode_status(raw_status));
        }
        if waited < 0 {
            let failure = io::Error::last_os_error();
            if failure.raw_os_error() == Some(libc::ECHILD) {
                break true;
            }
            if failure.kind() != io::ErrorKind::Interrupted {
                error = Some(failure.to_string());
                break false;
            }
        }
        if waited > 0 {
            continue;
        }
        let _ = output.drain_protocol(&mut stdout, protocol.as_mut());
        let _ = errors.drain(&mut stderr);
        if Instant::now() >= cleanup_deadline {
            break false;
        }
        thread::sleep(TICK);
    };
    if !clean {
        outcome = Outcome::Unknown;
        error = Some(
            "not all descendants could be stopped and reaped; manual inspection required".into(),
        );
    }
    if let Err(failure) = output
        .drain_to_eof(&mut stdout, protocol.as_mut(), cleanup_deadline)
        .and_then(|_| errors.drain_to_eof(&mut stderr, None, cleanup_deadline))
    {
        if outcome == Outcome::Success {
            outcome = Outcome::Failure;
        }
        if error.is_none() {
            error = Some(format!(
                "command output could not be completely drained: {failure}"
            ));
        }
    }
    let catalog = protocol.as_ref().and_then(ProtocolParser::catalog_result);
    let provider = protocol.map(|parser| {
        let (result, protocol_error) = parser.finish();
        if let Some(failure) = protocol_error {
            if outcome == Outcome::Success {
                outcome = Outcome::Failure;
            }
            if error.is_none() {
                error = Some(failure);
            }
        }
        result
    });
    let (exit_code, signal) = status.unwrap_or((None, None));
    CommandResult {
        failure: None,
        provider: if spec.catalog { None } else { provider },
        catalog: if outcome == Outcome::Success && error.is_none() {
            catalog
        } else {
            None
        },
        outcome,
        exit_code,
        signal,
        stdout: if spec.catalog {
            String::new()
        } else {
            output.text()
        },
        stderr: if spec.catalog {
            String::new()
        } else {
            errors.text()
        },
        stdout_truncated: output.is_truncated(),
        stderr_truncated: errors.is_truncated(),
        duration_ms: started.elapsed().as_millis() as u64,
        supervisor_pid: Some(std::process::id()),
        error,
    }
}

fn direct_children() -> io::Result<Vec<libc::pid_t>> {
    let own_pid = std::process::id();
    if let Ok(children) = fs::read_to_string(format!("/proc/self/task/{own_pid}/children")) {
        return Ok(children
            .split_whitespace()
            .filter_map(|pid| pid.parse().ok())
            .collect());
    }
    // Some Linux runtimes omit the optional /proc/.../children interface. In that
    // case inspect bounded process metadata, retaining only this supervisor's own
    // direct children. Never signal a process based on user-controlled PID files.
    let mut children = Vec::new();
    for (index, entry) in fs::read_dir("/proc")?.enumerate() {
        if index >= 100_000 {
            return Err(io::Error::other(
                "process metadata inspection limit exceeded",
            ));
        }
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some((_, fields)) = stat.rsplit_once(')') else {
            continue;
        };
        let parent = fields
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse::<u32>().ok());
        if parent == Some(own_pid) {
            children.push(pid);
        }
    }
    Ok(children)
}

fn observed_exit(pid: libc::pid_t) -> io::Result<Option<(Option<i32>, Option<i32>)>> {
    // SAFETY: initialized storage, valid pid, and waitid writes only siginfo_t.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        if libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        ) != 0
        {
            return Err(io::Error::last_os_error());
        }
        if info.si_pid() == 0 {
            Ok(None)
        } else if info.si_code == libc::CLD_EXITED {
            Ok(Some((Some(info.si_status()), None)))
        } else {
            Ok(Some((None, Some(info.si_status()))))
        }
    }
}
fn decode_status(status: i32) -> (Option<i32>, Option<i32>) {
    if libc::WIFEXITED(status) {
        (Some(libc::WEXITSTATUS(status)), None)
    } else if libc::WIFSIGNALED(status) {
        (None, Some(libc::WTERMSIG(status)))
    } else {
        (None, None)
    }
}
fn nonblocking(value: &impl AsRawFd) -> io::Result<()> {
    // SAFETY: fcntl reads/updates the flags of a live descriptor without ownership transfer.
    unsafe {
        let flags = libc::fcntl(value.as_raw_fd(), libc::F_GETFL);
        if flags < 0 || libc::fcntl(value.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}
struct Capture {
    eof: bool,
    bytes: Vec<u8>,
    limit: usize,
    truncated: bool,
    inventory: Option<crate::git_inventory::Framing>,
}
impl Capture {
    fn new(limit: usize) -> Self {
        Self {
            eof: false,
            bytes: Vec::new(),
            limit,
            truncated: false,
            inventory: None,
        }
    }
    fn drain(&mut self, reader: &mut impl Read) -> io::Result<()> {
        self.drain_protocol(reader, None)
    }
    fn drain_protocol(
        &mut self,
        reader: &mut impl Read,
        mut protocol: Option<&mut ProtocolParser>,
    ) -> io::Result<()> {
        let mut buffer = [0; 8192];
        // A chatty command must not starve timeout or cancellation checks.
        for _ in 0..8 {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    self.eof = true;
                    if let Some(inventory) = self.inventory.take()
                        && let Err(error) = inventory.finish()
                    {
                        self.truncated = true;
                        return Err(error);
                    }
                    break;
                }
                Ok(read) => {
                    if let Some(parser) = &mut protocol {
                        parser.feed(&buffer[..read]);
                    }
                    if let Some(inventory) = &mut self.inventory
                        && let Err(error) = inventory.feed(&buffer[..read])
                    {
                        self.inventory = None;
                        self.truncated = true;
                        return Err(error);
                    }
                    let keep = read.min(self.limit.saturating_sub(self.bytes.len()));
                    self.bytes.extend_from_slice(&buffer[..keep]);
                    self.truncated |= keep < read;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
    fn drain_to_eof(
        &mut self,
        reader: &mut impl Read,
        mut protocol: Option<&mut ProtocolParser>,
        deadline: Instant,
    ) -> io::Result<()> {
        while !self.eof {
            self.drain_protocol(reader, protocol.as_deref_mut())?;
            if self.eof {
                break;
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "output EOF was not observed",
                ));
            }
            thread::sleep(TICK);
        }
        Ok(())
    }
    fn is_truncated(&self) -> bool {
        self.truncated || String::from_utf8_lossy(&self.bytes).len() > self.limit
    }
    fn text(&self) -> String {
        let mut text = String::from_utf8_lossy(&self.bytes).into_owned();
        truncate_utf8(&mut text, self.limit);
        text
    }
}
fn truncate_utf8(text: &mut String, maximum: usize) {
    let mut end = maximum.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

#[cfg(test)]
mod capture_tests {
    use super::*;
    #[test]
    fn inventory_capture_and_json_envelope_remain_bounded() {
        // The worst-case serializer expansion is six bytes per input byte.
        let mut response = CommandResult::error(Outcome::Failure, "fixture");
        response.stdout = "\0".repeat(crate::git_inventory::MAX_BYTES);
        response.stderr = "\0".repeat(MAX_PROBE_CAPTURE);
        let encoded = serde_json::to_vec(&response).unwrap();
        let envelope_limit = 6 * (crate::git_inventory::MAX_BYTES + MAX_PROBE_CAPTURE) + 128 * 1024;
        assert!(encoded.len() <= envelope_limit);
        let mut capture = Capture::new(crate::git_inventory::MAX_BYTES);
        capture.inventory = Some(crate::git_inventory::Framing::default());
        let entry = format!("100644 blob {}\tname\0", "a".repeat(40));
        let mut input = io::Cursor::new(entry.repeat(7000).into_bytes());
        capture
            .drain_to_eof(&mut input, None, Instant::now() + Duration::from_secs(2))
            .unwrap();
        assert!(!capture.is_truncated());
        assert!(capture.bytes.len() > 367_835);
        assert_eq!(capture.text(), entry.repeat(7000));
        let mut generic = Capture::new(MAX_PROBE_CAPTURE);
        generic
            .drain_to_eof(
                &mut io::Cursor::new(vec![b'x'; MAX_PROBE_CAPTURE + 1]),
                None,
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap();
        assert!(generic.is_truncated());
        assert_eq!(generic.bytes.len(), MAX_PROBE_CAPTURE);
    }
    #[test]
    fn final_drain_observes_error_beyond_first_sixty_four_kib() {
        let profile: NativeProfile = serde_json::from_value(serde_json::json!({
            "provider":"codex_cli", "program":"/bin/true"
        }))
        .unwrap();
        let mut stream = b"{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"done\"}}\n{\"type\":\"turn.completed\"}\n".to_vec();
        for _ in 0..10000 {
            stream.extend_from_slice(b"{\"type\":\"future.progress\"}\n");
        }
        stream.extend_from_slice(b"{\"type\":\"error\",\"message\":\"late failure\"}\n");
        let mut reader = io::Cursor::new(stream);
        let mut output = Capture::new(128);
        let mut parser = ProtocolParser::new(ProviderResult::new(&profile, None));
        output
            .drain_protocol(&mut reader, Some(&mut parser))
            .unwrap();
        assert!(!output.eof);
        output
            .drain_to_eof(
                &mut reader,
                Some(&mut parser),
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap();
        assert!(output.eof);
        assert!(output.truncated);
        assert!(parser.finish().1.unwrap().contains("error"));
    }
}

#[cfg(test)]
mod workspace_budget_tests {
    use super::*;
    #[test]
    fn logical_accounting_includes_git_reviewers_controls_and_no_symlink_targets() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut config: HostConfig = serde_json::from_value(serde_json::json!({
            "workspace_root":temp.path(), "repositories":{},
            "max_snapshot_bytes":1, "max_workspace_bytes": 2 * 1024u64.pow(3)
        }))
        .unwrap();
        let sizes = [
            ("repository/source", 600 * 1024u64.pow(2)),
            ("repository/.git/objects/pack", 100 * 1024u64.pow(2)),
            ("reviewer-repository/source", 600 * 1024u64.pow(2)),
            (
                "reviewer-repository/.git/objects/pack",
                100 * 1024u64.pow(2),
            ),
            ("last-result.json", 16 * 1024),
        ];
        for (name, bytes) in sizes {
            let path = temp.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            File::create(path).unwrap().set_len(bytes).unwrap();
        }
        std::os::unix::fs::symlink("/", temp.path().join("link")).unwrap();
        let total: u64 = sizes.iter().map(|(_, bytes)| bytes).sum();
        assert_eq!(workspace_bytes(temp.path(), &config).unwrap(), total);
        config.max_workspace_bytes = Some(total);
        check_workspace_admission(temp.path(), &config, 0).unwrap();
        assert!(check_workspace_admission(temp.path(), &config, 1).is_err());
        assert!(check_workspace_admission(temp.path(), &config, u64::MAX).is_err());
        config.max_workspace_bytes = Some(total - 1);
        assert!(check_workspace_budget(temp.path(), &config).is_err());
        config.max_workspace_bytes = Some(2 * 1024u64.pow(3));
        config.max_snapshot_entries = 1;
        assert!(
            check_workspace_budget(temp.path(), &config)
                .unwrap_err()
                .to_string()
                .contains("entry limit")
        );
    }
}
