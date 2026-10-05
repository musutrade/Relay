//! Read-only, bounded host inventory and typed failures, outside the durable core.
//! Logical sizes are observations, not allocated blocks, reservations, or OS quotas.
use crate::host::HostConfig;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions, ReadDir};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_CAUSE_BYTES: usize = 1024;
const MAX_INSPECTION_BYTES: u64 = 1024 * 1024 * 1024 * 1024 + 1;
const INSPECTION_TIME: Duration = Duration::from_millis(100);
const MAX_DIRECTORY_DEPTH: usize = 128;
const CONTROL_ALLOWANCE_BYTES: u64 = 64 * 1024;
const ENFORCEMENT: &str = "logical_bytes_best_effort";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Failure {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_bytes: Option<u64>,
    pub code: String,
    pub stage: String,
    pub cause: String,
}
impl Failure {
    pub fn new(
        code: impl Into<String>,
        stage: impl Into<String>,
        cause: impl Into<String>,
    ) -> Self {
        Self {
            required_bytes: None,
            limit_bytes: None,
            code: code.into(),
            stage: stage.into(),
            cause: bounded(cause.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub logical_bytes: Option<u64>,
    pub complete: bool,
    pub measured_at: u64,
    pub reason: Option<String>,
}
impl Usage {
    pub fn observed(logical_bytes: u64, complete: bool) -> Self {
        Self {
            logical_bytes: Some(logical_bytes),
            complete,
            measured_at: now(),
            reason: (!complete)
                .then(|| "incomplete observation; logical bytes are a lower bound".into()),
        }
    }
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            logical_bytes: None,
            complete: false,
            measured_at: now(),
            reason: Some(bounded(reason.into())),
        }
    }
    fn partial(logical_bytes: u64, reason: impl Into<String>) -> Self {
        Self {
            reason: Some(bounded(reason.into())),
            ..Self::observed(logical_bytes, false)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceState {
    pub usage: Usage,
    pub quota_bytes: Option<u64>,
    pub host_policy_cap_bytes: u64,
    pub snapshot_cap_bytes: u64,
    pub enforcement: String,
    pub os_hard_quota: bool,
    pub disk_reserved: bool,
}
impl ResourceState {
    pub fn new(usage: Usage, quota_bytes: Option<u64>, config: &HostConfig) -> Self {
        Self {
            usage,
            quota_bytes,
            host_policy_cap_bytes: config.workspace_byte_limit(),
            snapshot_cap_bytes: config.max_snapshot_bytes,
            enforcement: ENFORCEMENT.into(),
            os_hard_quota: false,
            disk_reserved: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceEstimate {
    pub host_policy_cap_bytes: u64,
    pub default_quota_bytes: u64,
    pub snapshot_cap_bytes: u64,
    pub initial_estimate: InitialEstimate,
    pub build_growth: String,
    pub enforcement: String,
    pub os_hard_quota: bool,
    pub disk_reserved: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitialEstimate {
    pub source: String,
    pub snapshot_bytes: Option<u64>,
    pub git_metadata_reference_bytes: Option<u64>,
    pub reviewer_copy_bytes: Option<u64>,
    pub estimated_initial_bytes: Option<u64>,
    /// All inputs to the heuristic were measured; this never promises exact usage.
    pub complete: bool,
    pub notes: Vec<String>,
}

#[derive(Debug)]
pub struct BudgetError {
    pub required_bytes: Option<u64>,
    pub code: &'static str,
    pub usage: Usage,
    pub cap_bytes: u64,
    pub message: String,
}
impl std::fmt::Display for BudgetError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for BudgetError {}

pub fn budget_error(
    code: &'static str,
    usage: Usage,
    cap_bytes: u64,
    message: impl Into<String>,
) -> io::Error {
    io::Error::other(BudgetError {
        required_bytes: None,
        code,
        usage,
        cap_bytes,
        message: bounded(message.into()),
    })
}

pub fn admission_error(
    present: u64,
    required: u64,
    cap_bytes: u64,
    message: impl Into<String>,
) -> io::Error {
    io::Error::other(BudgetError {
        required_bytes: Some(required),
        code: "workspace_quota_exceeded",
        usage: Usage::observed(present, true),
        cap_bytes,
        message: bounded(message.into()),
    })
}

/// Classify only typed host evidence. Human-readable error text is never parsed.
pub fn failure_from_io(error: &io::Error, stage: &str) -> Failure {
    let typed = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<BudgetError>());
    let mut failure = Failure::new(
        typed.map_or("workspace_measurement_failed", |error| error.code),
        stage,
        error.to_string(),
    );
    if let Some(typed) = typed {
        failure.required_bytes = typed.required_bytes;
        if !typed.code.contains("entry_limit") {
            failure.limit_bytes = Some(typed.cap_bytes);
        }
    }
    failure
}

/// Includes every regular file beneath the actual canonical workspace directory,
/// including Git, reviewer copies, and controls. Symlinks are never traversed.
/// Bounds are checked between metadata operations; a blocked filesystem syscall
/// cannot be preempted by the cooperative 100 ms inspection deadline.
pub fn measure_workspace(root: &Path, config: &HostConfig) -> Usage {
    inspect(root, Selection::All, &mut InspectionBudget::new(config))
}

/// Estimate from trusted allowlisted names without Git commands, copying files,
/// workspace creation, or provider execution. It is not an admission decision.
pub fn estimate(
    config: &HostConfig,
    repository: &str,
    workflow: Option<&str>,
) -> Result<ResourceEstimate, String> {
    if !valid_name(repository) {
        return Err("repository must be an allowlisted name, not a path".into());
    }
    let source = config
        .repositories
        .get(repository)
        .ok_or("repository is not allowlisted")?;
    let workflow = workflow
        .map(|name| {
            if !valid_name(name) {
                return Err("workflow must be an allowlisted name, not a path".to_string());
            }
            let workflow = config
                .workflows
                .get(name)
                .ok_or("workflow is not allowlisted")?;
            if workflow.repository != repository {
                return Err("workflow repository does not match the selected repository".into());
            }
            Ok(workflow)
        })
        .transpose()?;
    let mut budget = InspectionBudget::new(config);
    let mut notes = vec![
        "Host metadata-only inventory of regular-file logical sizes; no symlinks are followed and no agent claims are used.".into(),
        "Inspection is bounded by the configured entry limit, 100 ms between filesystem calls, 1 TiB + 1 logical bytes, and 128 directory levels; files may change during inspection.".into(),
    ];
    let snapshot = inspect(
        source,
        if workflow.is_some() {
            Selection::WorkflowSource
        } else {
            Selection::OrdinarySource
        },
        &mut budget,
    );
    if let Some(reason) = &snapshot.reason {
        notes.push(format!(
            "Source worktree: {reason}. Any reported bytes are only a lower bound."
        ));
    }
    let git = git_reference(source, &mut budget);
    if let Some(reason) = &git.reason {
        notes.push(format!("Source Git metadata reference: {reason}."));
    }
    let isolated_reviewer = workflow
        .map(|workflow| {
            config
                .native_agents
                .get(&workflow.reviewer)
                .map(crate::sessions::enabled)
        })
        .unwrap_or(Some(false));
    let reviewer_copy_bytes = match isolated_reviewer {
        Some(true) => snapshot.logical_bytes,
        Some(false) => Some(0),
        None => {
            notes.push(
                "Reviewer profile is unavailable; an independent copy cannot be estimated.".into(),
            );
            None
        }
    };
    let estimated_initial_bytes = if workflow.is_some() {
        notes.push("Workflow worktree bytes are a conservative filesystem heuristic including target/node_modules and untracked or ignored files, excluding .git; this is not the exact tracked Git copy.".into());
        notes.push("Source .git bytes are a reference only. Clone/fetch object selection, packing, filters, new objects, and future build growth are unknown and can differ in either direction.".into());
        notes.push(format!("The initial heuristic adds worktree bytes, any independent reviewer worktree, a source Git metadata reference for each checkout, and a {CONTROL_ALLOWANCE_BYTES}-byte control-file allowance. This allowance is an estimate, not a bound."));
        match isolated_reviewer {
            Some(true) => notes.push("An independent reviewer checkout is expected because the configured reviewer has session continuity enabled.".into()),
            Some(false) => notes.push("The configured reviewer does not require an independent checkout; reviewer copy bytes are zero.".into()),
            None => (),
        }
        if snapshot.complete && git.complete {
            snapshot
                .logical_bytes
                .zip(git.logical_bytes)
                .zip(reviewer_copy_bytes)
                .zip(isolated_reviewer)
                .and_then(|(((snapshot, git), reviewer), isolated)| {
                    snapshot
                        .checked_add(reviewer)?
                        .checked_add(git.checked_mul(if isolated { 2 } else { 1 })?)?
                        .checked_add(CONTROL_ALLOWANCE_BYTES)
                })
        } else {
            None
        }
    } else {
        notes.push("Ordinary snapshot bytes exclude .git, target, and node_modules at every level. Symlinks and special files are not counted; execution separately validates whether inputs can be copied.".into());
        notes.push("Ordinary tasks initialize fresh private Git metadata instead of copying source .git. Source metadata, if present, is reference-only; fresh Git/control overhead and the initial total are unknown. A source without .git is allowed.".into());
        None
    };
    if estimated_initial_bytes.is_none() {
        notes.push("Initial total is unknown because a required component is unavailable, partial, or not defensibly predictable; unknown bytes are not treated as zero.".into());
    }
    Ok(ResourceEstimate {
        host_policy_cap_bytes: config.workspace_byte_limit(),
        default_quota_bytes: config.workspace_byte_limit(),
        snapshot_cap_bytes: config.max_snapshot_bytes,
        initial_estimate: InitialEstimate {
            source: "host_inventory".into(),
            snapshot_bytes: snapshot.logical_bytes,
            git_metadata_reference_bytes: git.logical_bytes,
            reviewer_copy_bytes,
            estimated_initial_bytes,
            complete: estimated_initial_bytes.is_some(),
            notes,
        },
        build_growth: "unknown".into(),
        enforcement: ENFORCEMENT.into(),
        os_hard_quota: false,
        disk_reserved: false,
    })
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
}
fn bounded(mut value: String) -> String {
    if value.len() > MAX_CAUSE_BYTES {
        let mut end = MAX_CAUSE_BYTES;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
    }
    value
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Clone, Copy)]
enum Selection {
    All,
    OrdinarySource,
    WorkflowSource,
}
impl Selection {
    fn excludes(self, name: &OsStr) -> bool {
        match self {
            Self::All => false,
            Self::OrdinarySource => [".git", "target", "node_modules"]
                .iter()
                .any(|x| name == *x),
            Self::WorkflowSource => name == ".git",
        }
    }
}
struct InspectionBudget {
    started: Instant,
    entries: usize,
    max_entries: usize,
    bytes: u64,
}
impl InspectionBudget {
    fn new(config: &HostConfig) -> Self {
        Self {
            started: Instant::now(),
            entries: 0,
            max_entries: config.max_snapshot_entries.min(100_000),
            bytes: 0,
        }
    }
    fn check_time(&self) -> Result<(), &'static str> {
        if self.started.elapsed() >= INSPECTION_TIME {
            Err("inspection time limit reached")
        } else {
            Ok(())
        }
    }
    fn entry(&mut self) -> Result<(), &'static str> {
        self.check_time()?;
        if self.entries >= self.max_entries {
            return Err("inspection entry limit reached");
        }
        self.entries += 1;
        Ok(())
    }
}

// Keep a directory fd alive while using its /proc path. Opening each child with
// O_NOFOLLOW prevents an entry replaced with a symlink from redirecting traversal.
struct Directory {
    file: File,
    entries: ReadDir,
}
impl Directory {
    fn new(file: File) -> io::Result<Self> {
        let entries = fs::read_dir(fd_path(&file))?;
        Ok(Self { file, entries })
    }
}
fn fd_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}
fn open_directory(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}
fn open_root(root: &Path, budget: &InspectionBudget) -> Result<File, String> {
    budget.check_time()?;
    let metadata =
        fs::symlink_metadata(root).map_err(|error| format!("directory is unavailable: {error}"))?;
    if !metadata.is_dir() || root.canonicalize().ok().as_deref() != Some(root) {
        return Err(
            "root must be an actual canonical directory, not a symlink or redirected path".into(),
        );
    }
    // Anchor every path component, including root ancestors, without following
    // source symlinks. canonicalize above is validation, never a traversal input.
    let mut file = open_directory(Path::new("/"))
        .map_err(|error| format!("cannot open directory: {error}"))?;
    for component in root.components() {
        budget.check_time()?;
        match component {
            Component::RootDir => (),
            Component::Normal(name) => {
                file = open_directory(&fd_path(&file).join(name)).map_err(|error| {
                    format!("cannot open directory without following symlinks: {error}")
                })?;
            }
            _ => return Err("root must be an actual canonical directory".into()),
        }
    }
    Ok(file)
}
fn inspect(root: &Path, selection: Selection, budget: &mut InspectionBudget) -> Usage {
    let file = match open_root(root, budget) {
        Ok(file) => file,
        Err(reason) => return Usage::unavailable(reason),
    };
    inspect_open(file, selection, budget)
}
fn inspect_open(file: File, selection: Selection, budget: &mut InspectionBudget) -> Usage {
    let directory = match Directory::new(file) {
        Ok(directory) => directory,
        Err(error) => return Usage::unavailable(format!("directory cannot be listed: {error}")),
    };
    let mut directories = vec![directory];
    let mut bytes = 0u64;
    while let Some(directory) = directories.last_mut() {
        if let Err(reason) = budget.check_time() {
            return Usage::partial(bytes, reason);
        }
        let entry = match directory.entries.next() {
            None => {
                directories.pop();
                continue;
            }
            Some(Ok(entry)) => entry,
            Some(Err(error)) => {
                return Usage::partial(
                    bytes,
                    format!("directory changed or cannot be read: {error}"),
                );
            }
        };
        if let Err(reason) = budget.entry() {
            return Usage::partial(bytes, reason);
        }
        let name = entry.file_name();
        if selection.excludes(&name) {
            continue;
        }
        let path = fd_path(&directory.file).join(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                return Usage::partial(
                    bytes,
                    format!("entry changed or cannot be inspected: {error}"),
                );
            }
        };
        if metadata.is_dir() {
            if directories.len() >= MAX_DIRECTORY_DEPTH {
                return Usage::partial(bytes, "inspection directory-depth limit reached");
            }
            match open_directory(&path).and_then(Directory::new) {
                Ok(directory) => directories.push(directory),
                Err(error) => {
                    return Usage::partial(
                        bytes,
                        format!(
                            "directory changed or cannot be opened without following symlinks: {error}"
                        ),
                    );
                }
            }
        } else if metadata.is_file() {
            let observed = metadata.len().min(MAX_INSPECTION_BYTES - budget.bytes);
            bytes += observed;
            budget.bytes += observed;
            if budget.bytes >= MAX_INSPECTION_BYTES {
                return Usage::partial(
                    bytes,
                    "inspection logical-byte limit reached (1 TiB + 1); bytes are a lower bound",
                );
            }
        }
        // Symlink targets and special files have no regular-file logical bytes.
    }
    Usage::observed(bytes, true)
}
fn git_reference(source: &Path, budget: &mut InspectionBudget) -> Usage {
    let source = match open_root(source, budget) {
        Ok(file) => file,
        Err(reason) => return Usage::unavailable(reason),
    };
    if let Err(reason) = budget.entry() {
        return Usage::unavailable(reason);
    }
    let path = fd_path(&source).join(".git");
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() => match open_directory(&path) {
            Ok(file) => inspect_open(file, Selection::All, budget),
            Err(error) => Usage::unavailable(format!(
                "Git directory cannot be opened without following symlinks: {error}"
            )),
        },
        Ok(_) => Usage::unavailable(
            ".git is not a directory; gitdir pointers and symlinks are never followed",
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Usage::unavailable("no source .git directory is present")
        }
        Err(error) => Usage::unavailable(format!("source .git is unavailable: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn config(root: &Path) -> HostConfig {
        serde_json::from_value(json!({
            "workspace_root": root.join("never-created"),
            "repositories": {"source": root},
            "max_snapshot_bytes": 10,
            "max_workspace_bytes": 100,
            "max_snapshot_entries": 1000,
            "native_agents": {
                "reviewer": {"provider":"claude_cli", "program":"/not/executed", "session_continuity":true}
            },
            "workflows": {
                "review": {"repository":"source", "developer":"dev", "reviewer":"reviewer", "test":"test"}
            }
        })).unwrap()
    }
    fn write(root: &Path, name: &str, bytes: u64) {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        File::create(path).unwrap().set_len(bytes).unwrap();
    }
    #[test]
    fn whole_workspace_is_exact_even_over_policy_cap() {
        let temp = TempDir::new().unwrap();
        for (name, bytes) in [
            ("repository/source", 90),
            ("repository/.git/objects/object", 20),
            ("reviewer-repository/source", 90),
            ("target/output", 8),
            ("control.json", 3),
        ] {
            write(temp.path(), name, bytes);
        }
        let usage = measure_workspace(temp.path(), &config(temp.path()));
        assert_eq!(usage.logical_bytes, Some(211));
        assert!(usage.complete);
        assert!(usage.reason.is_none());
        assert!(usage.measured_at > 0);
    }
    #[test]
    fn entry_cap_reports_a_lower_bound_and_exact_cap_completes() {
        let temp = TempDir::new().unwrap();
        write(temp.path(), "a", 7);
        let mut config = config(temp.path());
        config.max_snapshot_entries = 1;
        assert!(measure_workspace(temp.path(), &config).complete);
        write(temp.path(), "b", 7);
        let usage = measure_workspace(temp.path(), &config);
        assert_eq!(usage.logical_bytes, Some(7));
        assert!(!usage.complete);
        assert!(usage.reason.unwrap().contains("entry limit"));
    }
    #[test]
    fn inspection_byte_cap_is_a_bounded_lower_bound() {
        let temp = TempDir::new().unwrap();
        write(temp.path(), "sparse", MAX_INSPECTION_BYTES + 1000);
        let usage = measure_workspace(temp.path(), &config(temp.path()));
        assert_eq!(usage.logical_bytes, Some(MAX_INSPECTION_BYTES));
        assert!(!usage.complete);
        assert!(usage.reason.unwrap().contains("logical-byte limit"));
    }
    #[test]
    fn missing_or_redirected_roots_are_unknown_not_zero() {
        let temp = TempDir::new().unwrap();
        let config = config(temp.path());
        let missing = temp.path().join("missing");
        assert!(measure_workspace(&missing, &config).logical_bytes.is_none());
        assert!(!missing.exists());
        write(temp.path(), "regular", 1);
        assert!(
            measure_workspace(&temp.path().join("regular"), &config)
                .logical_bytes
                .is_none()
        );
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(temp.path(), &link).unwrap();
        assert!(measure_workspace(&link, &config).logical_bytes.is_none());
        assert!(
            measure_workspace(&temp.path().join(".."), &config)
                .logical_bytes
                .is_none()
        );
    }
    #[test]
    fn symlink_targets_and_cycles_are_not_counted() {
        let temp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        write(outside.path(), "big", 10000);
        write(temp.path(), "actual", 7);
        std::os::unix::fs::symlink(outside.path(), temp.path().join("outside")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("big"), temp.path().join("file")).unwrap();
        std::os::unix::fs::symlink(temp.path(), temp.path().join("loop")).unwrap();
        let usage = measure_workspace(temp.path(), &config(temp.path()));
        assert_eq!(usage.logical_bytes, Some(7));
        assert!(usage.complete);
    }
    #[test]
    fn deadline_is_deterministically_reported() {
        let temp = TempDir::new().unwrap();
        let mut budget = InspectionBudget::new(&config(temp.path()));
        budget.started = Instant::now() - INSPECTION_TIME;
        let usage = inspect(temp.path(), Selection::All, &mut budget);
        assert_eq!(usage.logical_bytes, None);
        assert!(!usage.complete);
        assert!(usage.reason.unwrap().contains("time limit"));
        let usage = inspect_open(
            open_directory(temp.path()).unwrap(),
            Selection::All,
            &mut budget,
        );
        assert_eq!(usage.logical_bytes, Some(0));
        assert!(!usage.complete);
    }
    #[test]
    fn empty_existing_directory_is_known_zero_and_missing_estimate_is_unknown() {
        let temp = TempDir::new().unwrap();
        let mut config = config(temp.path());
        let usage = measure_workspace(temp.path(), &config);
        assert_eq!(usage.logical_bytes, Some(0));
        assert!(usage.complete);
        config
            .repositories
            .insert("source".into(), temp.path().join("missing"));
        let result = estimate(&config, "source", Some("review")).unwrap();
        assert_eq!(result.initial_estimate.snapshot_bytes, None);
        assert_eq!(result.initial_estimate.git_metadata_reference_bytes, None);
        assert_eq!(result.initial_estimate.reviewer_copy_bytes, None);
        assert_eq!(result.initial_estimate.estimated_initial_bytes, None);
        assert!(!result.initial_estimate.complete);
    }
    #[test]
    fn ordinary_estimate_excludes_snapshot_names_without_guessing_git_overhead() {
        let temp = TempDir::new().unwrap();
        for (name, bytes) in [
            ("src/file", 7),
            ("nested/target/a", 200),
            ("node_modules/a", 300),
            ("nested/.git/a", 400),
            (".git/objects/a", 30),
        ] {
            write(temp.path(), name, bytes);
        }
        let config = config(temp.path());
        let estimate = estimate(&config, "source", None).unwrap();
        assert_eq!(estimate.initial_estimate.snapshot_bytes, Some(7));
        assert_eq!(
            estimate.initial_estimate.git_metadata_reference_bytes,
            Some(30)
        );
        assert_eq!(estimate.initial_estimate.reviewer_copy_bytes, Some(0));
        assert_eq!(estimate.initial_estimate.estimated_initial_bytes, None);
        assert!(!estimate.initial_estimate.complete);
        assert!(!config.workspace_root.exists());
        assert_eq!(estimate.enforcement, "logical_bytes_best_effort");
        assert!(!estimate.os_hard_quota && !estimate.disk_reserved);
    }
    #[test]
    fn nongit_source_is_valid_and_unknown_metadata_is_not_zero() {
        let temp = TempDir::new().unwrap();
        write(temp.path(), "source", 7);
        let estimate = estimate(&config(temp.path()), "source", None).unwrap();
        assert_eq!(estimate.initial_estimate.snapshot_bytes, Some(7));
        assert_eq!(estimate.initial_estimate.git_metadata_reference_bytes, None);
        assert_eq!(estimate.initial_estimate.estimated_initial_bytes, None);
    }
    #[test]
    fn workflow_heuristic_includes_only_required_independent_copies() {
        let temp = TempDir::new().unwrap();
        write(temp.path(), "source", 7);
        write(temp.path(), "target/build", 9);
        write(temp.path(), ".git/objects/object", 30);
        let mut config = config(temp.path());
        let result = estimate(&config, "source", Some("review")).unwrap();
        assert_eq!(result.initial_estimate.snapshot_bytes, Some(16));
        assert_eq!(result.initial_estimate.reviewer_copy_bytes, Some(16));
        assert_eq!(
            result.initial_estimate.estimated_initial_bytes,
            Some(2 * (16 + 30) + CONTROL_ALLOWANCE_BYTES)
        );
        assert!(result.initial_estimate.complete);
        assert!(
            result
                .initial_estimate
                .notes
                .iter()
                .any(|n| n.contains("not the exact tracked Git copy"))
        );
        config
            .native_agents
            .get_mut("reviewer")
            .unwrap()
            .session_continuity = false;
        let result = estimate(&config, "source", Some("review")).unwrap();
        assert_eq!(result.initial_estimate.reviewer_copy_bytes, Some(0));
        assert_eq!(
            result.initial_estimate.estimated_initial_bytes,
            Some(16 + 30 + CONTROL_ALLOWANCE_BYTES)
        );
        config.native_agents.remove("reviewer");
        let result = estimate(&config, "source", Some("review")).unwrap();
        assert_eq!(result.initial_estimate.reviewer_copy_bytes, None);
        assert_eq!(result.initial_estimate.estimated_initial_bytes, None);
    }
    #[test]
    fn gitdir_pointer_and_partial_inputs_prevent_a_workflow_total() {
        let temp = TempDir::new().unwrap();
        write(temp.path(), "source", 7);
        fs::write(temp.path().join(".git"), "gitdir: /not/followed\n").unwrap();
        let mut config = config(temp.path());
        let result = estimate(&config, "source", Some("review")).unwrap();
        assert_eq!(result.initial_estimate.git_metadata_reference_bytes, None);
        assert_eq!(result.initial_estimate.estimated_initial_bytes, None);
        config.max_snapshot_entries = 1;
        let result = estimate(&config, "source", Some("review")).unwrap();
        assert!(!result.initial_estimate.complete);
        assert_eq!(result.initial_estimate.estimated_initial_bytes, None);
    }
    #[test]
    fn selectors_are_allowlisted_names_and_workflow_must_match() {
        let temp = TempDir::new().unwrap();
        let mut config = config(temp.path());
        for name in [
            "missing",
            "../source",
            "/source",
            ".",
            "..",
            "source/child",
            "",
        ] {
            assert!(estimate(&config, name, None).is_err());
        }
        for name in ["missing", "../review", "/review", ""] {
            assert!(estimate(&config, "source", Some(name)).is_err());
        }
        config.workflows.get_mut("review").unwrap().repository = "other".into();
        assert!(estimate(&config, "source", Some("review")).is_err());
    }
    #[test]
    fn failure_causes_are_utf8_bounded_and_codes_are_typed() {
        let failure = Failure::new("test", "snapshot", "界".repeat(1000));
        assert!(failure.cause.len() <= 1024);
        assert!(failure.cause.len().is_multiple_of(3));
        let error = budget_error(
            "workspace_quota_exceeded",
            Usage::observed(101, false),
            100,
            "over budget",
        );
        let typed = error
            .get_ref()
            .unwrap()
            .downcast_ref::<BudgetError>()
            .unwrap();
        assert_eq!(typed.cap_bytes, 100);
        assert_eq!(typed.usage.logical_bytes, Some(101));
        assert_eq!(
            failure_from_io(&error, "admission").code,
            "workspace_quota_exceeded"
        );
        let untyped = io::Error::other("workspace_quota_exceeded");
        assert_eq!(
            failure_from_io(&untyped, "admission").code,
            "workspace_measurement_failed"
        );
        let restored: Failure =
            serde_json::from_str(&serde_json::to_string(&failure).unwrap()).unwrap();
        assert_eq!(restored.cause, failure.cause);
    }
    #[test]
    fn resource_state_uses_policy_fallback_and_preserves_unknown_quota() {
        let temp = TempDir::new().unwrap();
        let mut config = config(temp.path());
        config.max_workspace_bytes = None;
        let state = ResourceState::new(Usage::unavailable("workspace not created"), None, &config);
        assert_eq!(state.quota_bytes, None);
        assert_eq!(state.host_policy_cap_bytes, config.max_snapshot_bytes);
        assert_eq!(state.usage.logical_bytes, None);
        assert!(!state.os_hard_quota && !state.disk_reserved);
    }
}
