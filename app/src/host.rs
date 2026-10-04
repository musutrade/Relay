//! Trusted Linux development host, deliberately outside Relay's opaque queue core.
//!
//! Configured commands run with the local account's privileges. This is process
//! lifecycle management and private snapshot isolation, not an adversarial sandbox.
//! A separate subreaper process owns every command tree; the application process
//! never changes its process-wide child-reaping behavior.
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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;

const MAX_REQUIREMENTS: usize = 32 * 1024;
const MAX_CONFIG_BYTES: usize = 256 * 1024;
const MAX_CAPTURE: usize = 8192;
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
    pub agents: BTreeMap<String, CommandProfile>,
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
    #[serde(default = "default_snapshot_entries")]
    pub max_snapshot_entries: usize,
    #[serde(default = "default_retained_workspaces")]
    pub max_retained_workspaces: usize,
    /// Override only for packaging/tests; must implement `__relay_host_supervisor`.
    #[serde(default)]
    pub supervisor_program: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
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
        if !config.agents.contains_key(&self.agent) {
            return Err(HostError::Job("agent profile is not allowlisted".into()));
        }
        if self
            .test
            .as_ref()
            .is_some_and(|name| !config.tests.contains_key(name))
        {
            return Err(HostError::Job("test profile is not allowlisted".into()));
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
    fn error(outcome: Outcome, error: impl Into<String>) -> Self {
        Self {
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
    pub outcome: Outcome,
    pub workspace: Option<PathBuf>,
    pub agent: Option<CommandResult>,
    pub tests: Option<CommandResult>,
    pub draft_pr: Option<CommandResult>,
    pub error: Option<String>,
}
impl RunResult {
    fn new(outcome: Outcome, error: Option<String>) -> Self {
        Self {
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
        if let Some(error) = &mut value.error {
            truncate_utf8(error, 1024);
        }
        for command in [&mut value.agent, &mut value.tests, &mut value.draft_pr]
            .into_iter()
            .flatten()
        {
            if let Some(error) = &mut command.error {
                truncate_utf8(error, 1024);
            }
        }
        loop {
            let serialized = serde_json::to_string(&value).expect("serializable run result");
            if serialized.len() <= MAX_RESULT_BYTES {
                return serialized;
            }
            let mut reduced = false;
            for command in [&mut value.agent, &mut value.tests, &mut value.draft_pr]
                .into_iter()
                .flatten()
            {
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
                value.workspace = None;
                value.error = Some("result metadata exceeded the persistence budget".into());
                for command in [&mut value.agent, &mut value.tests, &mut value.draft_pr]
                    .into_iter()
                    .flatten()
                {
                    command.error = None;
                }
            }
        }
    }
}

pub struct Host {
    config: HostConfig,
    supervisor: PathBuf,
}
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
        if config.max_snapshot_bytes == 0
            || config.max_snapshot_bytes > 1024 * 1024 * 1024
            || config.max_snapshot_entries == 0
            || config.max_snapshot_entries > 100_000
        {
            return Err(HostError::Config(
                "snapshot limit is outside supported bounds".into(),
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
        if config.repositories.is_empty() || config.agents.is_empty() {
            return Err(HostError::Config(
                "at least one repository and agent are required".into(),
            ));
        }
        for name in config
            .repositories
            .keys()
            .chain(config.agents.keys())
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
        Ok(Self { config, supervisor })
    }

    pub fn config(&self) -> &HostConfig {
        &self.config
    }

    /// Only execute a newly claimed generation. A pre-existing workspace is unknown,
    /// never evidence that the previous process stopped. No method here requeues.
    pub fn execute(&self, task: &Task, cancellation: Arc<AtomicBool>) -> RunResult {
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
        let prior_workspace = self
            .config
            .workspace_root
            .join(format!("task-{}-generation-{}", task.id, task.generation));
        if fs::symlink_metadata(&prior_workspace).is_ok() {
            let mut result = RunResult::new(
                Outcome::Unknown,
                Some(
                    "generation workspace already exists; inspect old execution before recovery"
                        .into(),
                ),
            );
            result.workspace = Some(prior_workspace);
            return result;
        }
        if cancellation.load(Ordering::Acquire) {
            return RunResult::new(Outcome::Cancelled, None);
        }
        let started = Instant::now();
        let deadline = started + Duration::from_secs(self.config.timeout_seconds);
        match fs::read_dir(&self.config.workspace_root) {
            Ok(entries) => {
                if entries.take(self.config.max_retained_workspaces).count()
                    >= self.config.max_retained_workspaces
                {
                    return RunResult::new(Outcome::Failure, Some("workspace retention limit reached; trusted operator cleanup is required".into()));
                }
            }
            Err(error) => return RunResult::new(Outcome::Failure, Some(error.to_string())),
        }
        let workspace = self
            .config
            .workspace_root
            .join(format!("task-{}-generation-{}", task.id, task.generation));
        let mut result = RunResult::new(Outcome::Failure, None);
        result.workspace = Some(workspace.clone());
        if let Err(error) = fs::DirBuilder::new().mode(0o700).create(&workspace) {
            result.outcome = if error.kind() == io::ErrorKind::AlreadyExists {
                Outcome::Unknown
            } else {
                Outcome::Failure
            };
            result.error = Some(format!(
                "cannot create generation workspace; an existing workspace must be inspected before recovery: {error}"
            ));
            return result;
        }
        let repository = workspace.join("repository");
        let requirements_file = workspace.join("requirements.txt");
        let setup = (|| -> Result<(), HostError> {
            fs::DirBuilder::new().mode(0o700).create(&repository)?;
            let mut budget = SnapshotBudget {
                bytes: 0,
                entries: 0,
                config: &self.config,
                deadline,
                cancellation: &cancellation,
            };
            copy_snapshot(
                &self.config.repositories[&job.repository],
                &repository,
                &mut budget,
            )?;
            fs::write(&requirements_file, &job.requirements)?;
            fs::write(workspace.join("job.json"), serde_json::to_vec(&job)?)?;
            Ok(())
        })();
        if let Err(error) = setup {
            result.outcome = interrupted(&cancellation, deadline).unwrap_or(Outcome::Failure);
            result.error = Some(error.to_string());
            return result;
        }
        let mut phases = vec![("agent", &self.config.agents[&job.agent])];
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
            if let Some(outcome) = interrupted(&cancellation, deadline) {
                result.outcome = outcome;
                return result;
            }
            let expand = |arg: &str| -> String {
                match arg {
                    "{requirements}" => job.requirements.clone(),
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
                ("RELAY_REQUIREMENTS", job.requirements.clone()),
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
                program: profile.program.clone(),
                args: profile.args.iter().map(|arg| expand(arg)).collect(),
                env,
                cwd: repository.clone(),
                input: job.requirements.clone(),
                timeout_ms: deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .max(1) as u64,
                output_limit_bytes: self.config.output_limit_bytes,
            };
            let command = self.run_supervised(spec, &cancellation, &workspace, phase);
            let outcome = command.outcome;
            match phase {
                "agent" => result.agent = Some(command),
                "tests" => result.tests = Some(command),
                _ => result.draft_pr = Some(command),
            }
            result.outcome = outcome;
            if outcome != Outcome::Success {
                return result;
            }
        }
        result.outcome = Outcome::Success;
        result
    }

    fn run_supervised(
        &self,
        spec: CommandSpec,
        cancellation: &AtomicBool,
        workspace: &Path,
        phase: &str,
    ) -> CommandResult {
        let mut serialized = serde_json::to_vec(&spec).expect("serializable command spec");
        if serialized.len() > MAX_CONFIG_BYTES {
            return CommandResult::error(
                Outcome::Failure,
                "expanded command configuration exceeds 256 KiB",
            );
        }
        serialized.push(b'\n');
        let mut child = match Command::new(&self.supervisor)
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
        let mut output = Capture::new(128 * 1024);
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
                resource_error =
                    check_workspace_budget(&workspace.join("repository"), &self.config)
                        .err()
                        .map(|error| error.to_string());
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
                    let _ = output.drain(&mut stdout);
                    let _ = errors.drain(&mut stderr);
                    if status.success()
                        && !output.truncated
                        && let Ok(mut result) =
                            serde_json::from_slice::<CommandResult>(&output.bytes)
                    {
                        result.supervisor_pid = Some(pid);
                        if result.outcome != Outcome::Unknown {
                            let resource_error = resource_error.or_else(|| {
                                check_workspace_budget(&workspace.join("repository"), &self.config)
                                    .err()
                                    .map(|error| error.to_string())
                            });
                            if let Some(error) = resource_error {
                                result.outcome = Outcome::Failure;
                                result.error = Some(error);
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
fn copy_snapshot(
    source: &Path,
    destination: &Path,
    budget: &mut SnapshotBudget<'_>,
) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
        budget.check()?;
        let entry = entry?;
        let name = entry.file_name();
        if [".git", "target", "node_modules"]
            .iter()
            .any(|excluded| name == *excluded)
        {
            continue;
        }
        budget.entries += 1;
        if budget.entries > budget.config.max_snapshot_entries {
            return Err(io::Error::other("snapshot entry limit exceeded"));
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
                    return Err(io::Error::other("snapshot byte limit exceeded"));
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
                return Err(io::Error::other("running workspace entry limit exceeded"));
            }
            if metadata.is_dir() {
                directories.push(entry.path());
            } else if metadata.is_file() {
                bytes = bytes.saturating_add(metadata.len());
            }
            // Never follow new symlinks while inspecting a running workspace.
            if bytes > config.max_snapshot_bytes {
                return Err(io::Error::other("running workspace byte limit exceeded"));
            }
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandSpec {
    program: PathBuf,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: PathBuf,
    input: String,
    timeout_ms: u64,
    output_limit_bytes: usize,
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
            || spec.output_limit_bytes > MAX_CAPTURE
            || spec.input.len() > MAX_REQUIREMENTS
        {
            return Err(io::Error::other("invalid supervisor limits"));
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
    let mut child = match Command::new(&spec.program)
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
    let mut output = Capture::new(spec.output_limit_bytes);
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
            .drain(&mut stdout)
            .and_then(|_| errors.drain(&mut stderr))
        {
            error = Some(failure.to_string());
            break;
        }
        if let Some(writer) = &mut input {
            match writer.write(&spec.input.as_bytes()[input_offset..]) {
                Ok(written) => {
                    input_offset += written;
                    if input_offset == spec.input.len() {
                        input.take();
                    }
                }
                Err(failure)
                    if failure.kind() == io::ErrorKind::WouldBlock
                        || failure.kind() == io::ErrorKind::Interrupted => {}
                Err(failure) if failure.kind() == io::ErrorKind::BrokenPipe => {
                    input.take();
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
        let _ = output.drain(&mut stdout);
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
    let _ = output.drain(&mut stdout);
    let _ = errors.drain(&mut stderr);
    let (exit_code, signal) = status.unwrap_or((None, None));
    CommandResult {
        outcome,
        exit_code,
        signal,
        stdout: output.text(),
        stderr: errors.text(),
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
    bytes: Vec<u8>,
    limit: usize,
    truncated: bool,
}
impl Capture {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            truncated: false,
        }
    }
    fn drain(&mut self, reader: &mut impl Read) -> io::Result<()> {
        let mut buffer = [0; 8192];
        // A chatty command must not starve timeout or cancellation checks.
        for _ in 0..8 {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
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
