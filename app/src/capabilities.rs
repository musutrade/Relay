//! Read-only discovery for explicitly configured native profiles. Catalog data is
//! evidence, not authentication, subscription entitlement, or an executed model.
//! All subprocesses remain under the trusted host's bounded subreaper lifecycle.
use crate::host::{CommandResult, CommandSpec, Host, Outcome};
use crate::providers::{MAX_PROTOCOL_LINE, NativeProfile, ProviderKind, ProviderResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_MODELS: usize = 256;
const MAX_PAGES: usize = 16;
const MAX_MESSAGES: usize = 1024;
const MAX_PROTOCOL_BYTES: usize = 1024 * 1024;
const MAX_CATALOG_BYTES: usize = 256 * 1024;
pub(crate) const MAX_CATALOG_RESPONSE_BYTES: usize = MAX_CATALOG_BYTES + 32 * 1024;
const MAX_FIELD: usize = 256;
const MAX_DESCRIPTION: usize = 1024;
const MAX_EFFORTS: usize = 32;
const CODEX_SOURCE: &str = "codex_app_server:model/list";
const HELP_SOURCE: &str = "configured_cli:version/help";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    Supported,
    Unsupported,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    pub state: CapabilityState,
    pub reason: String,
    pub source: String,
}
impl Capability {
    fn new(state: CapabilityState, reason: &str, source: &str) -> Self {
        Self {
            state,
            reason: reason.into(),
            source: source.into(),
        }
    }
    fn unknown(reason: &str, source: &str) -> Self {
        Self::new(CapabilityState::Unknown, reason, source)
    }
    fn supported(reason: &str, source: &str) -> Self {
        Self::new(CapabilityState::Supported, reason, source)
    }
    fn unsupported(reason: &str, source: &str) -> Self {
        Self::new(CapabilityState::Unsupported, reason, source)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffortCapability {
    pub effort: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCapability {
    pub id: String,
    pub model: String,
    pub display_name: String,
    pub description: Option<String>,
    pub default_effort: Option<String>,
    /// None means the provider did not report effort metadata.
    pub supported_efforts: Option<Vec<EffortCapability>>,
    pub is_default: Option<bool>,
    pub hidden: Option<bool>,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Selection {
    pub requested_model: Option<String>,
    pub requested_effort: Option<String>,
    /// Only a real execution can establish effective values. Discovery never does.
    pub effective_model: Option<String>,
    pub effective_effort: Option<String>,
    pub status: Capability,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileCatalog {
    pub provider: ProviderKind,
    pub checked_at_unix_ms: u64,
    pub cli_version: Option<String>,
    pub executable: Capability,
    pub compatibility: Capability,
    pub authentication: Capability,
    pub reviewer_isolation: Capability,
    pub permission_control: Capability,
    pub session_continuity: Capability,
    pub process_cleanup: Capability,
    pub startup_context: Capability,
    pub model_catalog: Capability,
    pub models: Vec<ModelCapability>,
    pub selection: Selection,
}
impl ProfileCatalog {
    fn unknown(profile: &NativeProfile) -> Self {
        Self {
            provider: profile.provider,
            checked_at_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u64::MAX as u128) as u64,
            cli_version: None,
            executable: Capability::unknown(
                "Configured executable has not been checked",
                "configured_path:metadata",
            ),
            compatibility: Capability::unknown(
                "Version/help compatibility has not been verified",
                HELP_SOURCE,
            ),
            authentication: Capability::unknown(
                "Read-only discovery does not authenticate or prove model access",
                "not_probed",
            ),
            reviewer_isolation: if profile.provider == ProviderKind::ClaudeCli {
                Capability::unknown(
                    "Restricted reviewer flags have not been verified",
                    HELP_SOURCE,
                )
            } else {
                Capability::unsupported(
                    "Relay has not verified the complete Codex no-execution, hooks, MCP and configuration-loading reviewer isolation contract",
                    "relay:reviewer_contract",
                )
            },
            permission_control: Capability::unknown(
                "Configured permission contract has not been verified",
                HELP_SOURCE,
            ),
            session_continuity: Capability::unknown(
                "Session continuation contract has not been verified",
                HELP_SOURCE,
            ),
            process_cleanup: Capability::supported(
                "No discovery subprocess has been started",
                "relay:supervisor",
            ),
            startup_context: Capability::unknown(
                "No native catalog process has been started",
                "not_probed",
            ),
            model_catalog: Capability::unknown("No verified catalog was returned", "not_probed"),
            models: Vec::new(),
            selection: Selection {
                requested_model: profile.model.clone(),
                requested_effort: profile.effort.clone(),
                effective_model: None,
                effective_effort: None,
                status: Capability::unknown(
                    "Configured values are requests; execution-effective values are unknown",
                    "configured_profile",
                ),
            },
        }
    }
}

const DISCOVERY_GUARD: &str = ".catalog-discovery-in-flight";
enum GuardError {
    Busy,
    Stale,
    Unavailable,
}
fn lock_guard(file: &File) -> io::Result<()> {
    // SAFETY: flock only operates on this owned descriptor and never signals a PID.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
fn guard_matches(path: &Path, file: &File) -> bool {
    fs::symlink_metadata(path)
        .ok()
        .zip(file.metadata().ok())
        .is_some_and(|(path, held)| {
            path.is_file() && path.dev() == held.dev() && path.ino() == held.ino()
        })
}
fn create_guard(path: &Path) -> Result<File, GuardError> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => {
            lock_guard(&file).map_err(|_| GuardError::Busy)?;
            if !guard_matches(path, &file) {
                return Err(GuardError::Unavailable);
            }
            Ok(file)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let file = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)
                .map_err(|_| GuardError::Unavailable)?;
            match lock_guard(&file) {
                Ok(()) if guard_matches(path, &file) => Err(GuardError::Stale),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => Err(GuardError::Busy),
                _ => Err(GuardError::Unavailable),
            }
        }
        Err(_) => Err(GuardError::Unavailable),
    }
}
/// Only a trusted operator who has confirmed retained discovery trees stopped
/// may call this recovery operation. It does not inspect, stop, or infer process
/// termination. A live discovery's lock cannot be cleared by this operation.
pub fn confirm_discovery_stopped(host: &Host) -> Result<bool, String> {
    let path = host.config().workspace_root.join(DISCOVERY_GUARD);
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err("Cannot open the exact discovery guard for reconciliation".into()),
    };
    lock_guard(&file)
        .map_err(|_| "Discovery is still active; its guard cannot be cleared".to_owned())?;
    if !guard_matches(&path, &file) {
        return Err("Discovery guard identity changed; no marker was removed".into());
    }
    fs::remove_file(&path).map_err(|_| "Cannot clear the reconciled discovery guard".to_owned())?;
    Ok(true)
}
pub(crate) fn discovery_guard_present(host: &Host) -> bool {
    !matches!(fs::symlink_metadata(host.config().workspace_root.join(DISCOVERY_GUARD)), Err(error) if error.kind() == io::ErrorKind::NotFound)
}

/// Refresh one trusted, explicitly configured profile. Ordinary cache reads must
/// not call this function. No account/login, thread/start, turn/start, or inference
/// requests are ever sent. No raw CLI output or environment values are returned.
pub fn discover(host: &Host, profile: &NativeProfile) -> ProfileCatalog {
    let mut catalog = ProfileCatalog::unknown(profile);
    let cleanup_marker = host.config().workspace_root.join(DISCOVERY_GUARD);
    let executable = fs::metadata(&profile.program)
        .ok()
        .is_some_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    catalog.executable = if executable {
        Capability::supported(
            "Configured executable exists and has executable permissions",
            "configured_path:metadata",
        )
    } else {
        Capability::unsupported(
            "Configured executable is absent, not a regular file, or not executable",
            "configured_path:metadata",
        )
    };
    if !executable || profile.validate().is_err() {
        catalog.compatibility = Capability::unsupported(
            "Configured native profile is invalid or unavailable",
            "configured_profile",
        );
        return catalog;
    }
    // Atomic cross-process exclusion also survives service crashes. It is removed
    // only after every child tree has been confirmed stopped by the supervisor.
    use std::io::Write;
    let mut marker = match create_guard(&cleanup_marker) {
        Ok(marker) => marker,
        Err(GuardError::Stale) => {
            catalog.process_cleanup = Capability::unknown(
                "Earlier discovery cleanup is unconfirmed; inspect the host and use doctor --confirm-catalog-stopped",
                "relay:supervisor",
            );
            catalog.model_catalog = Capability::unknown(
                "Discovery is blocked by a retained process guard",
                "relay:supervisor",
            );
            return catalog;
        }
        Err(GuardError::Busy) => {
            catalog.model_catalog = Capability::unknown(
                "Another discovery is active; retry after it finishes",
                "relay:supervisor",
            );
            return catalog;
        }
        Err(GuardError::Unavailable) => {
            catalog.model_catalog = Capability::unknown(
                "Cannot acquire the discovery process guard; no subprocess was started",
                "relay:supervisor",
            );
            return catalog;
        }
    };
    if writeln!(marker, "A catalog discovery is in flight or its cleanup is unconfirmed. Inspect retained .catalog-* workspaces before removing this marker.").and_then(|_| marker.sync_all()).is_err() {
        let _ = fs::remove_file(&cleanup_marker);
        catalog.compatibility = Capability::unknown("Cannot persist the discovery process guard", "relay:supervisor");
        return catalog;
    }
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let workspace = host.config().workspace_root.join(format!(
        ".catalog-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    if fs::DirBuilder::new()
        .mode(0o700)
        .create(&workspace)
        .is_err()
    {
        let _ = fs::remove_file(&cleanup_marker);
        catalog.compatibility = Capability::unknown(
            "Cannot create private discovery workspace",
            "relay:supervisor",
        );
        return catalog;
    }
    if fs::DirBuilder::new()
        .mode(0o700)
        .create(workspace.join("repository"))
        .is_err()
    {
        let _ = fs::remove_dir(&workspace);
        let _ = fs::remove_file(&cleanup_marker);
        catalog.compatibility = Capability::unknown(
            "Cannot create private discovery working directory",
            "relay:supervisor",
        );
        return catalog;
    }
    let cancellation = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(10);
    let reviewer_only =
        profile.native_permission == Some(crate::providers::NativePermission::ClaudeRestricted);
    match host.probe_profile(profile, reviewer_only, &cancellation, &workspace, deadline) {
        Ok(probe) => {
            catalog.process_cleanup = Capability::supported(
                "Version/help subprocess trees stopped and were reaped",
                "relay:supervisor",
            );
            catalog.cli_version = Some(probe.cli_version);
            catalog.compatibility = Capability::supported(
                "Configured execution arguments passed bounded version/help checks",
                HELP_SOURCE,
            );
            catalog.permission_control = Capability::unknown(
                "Adapter interface checks passed; requested native permission settings, runtime enforcement and mode eligibility remain unverified; no permission was granted",
                "relay:adapter_contract",
            );
            if profile.provider == ProviderKind::ClaudeCli {
                catalog.reviewer_isolation = if probe.read_only_supported {
                    Capability::supported(
                        "Relay restricted-reviewer flags are advertised; managed policy remains authoritative",
                        HELP_SOURCE,
                    )
                } else {
                    Capability::unsupported(
                        "Required restricted reviewer flags are not all advertised",
                        HELP_SOURCE,
                    )
                };
            }
            catalog.session_continuity = match profile.provider {
                ProviderKind::CodexCli => Capability::unsupported(
                    "The configured Codex exec adapter is ephemeral",
                    "relay:adapter_contract",
                ),
                ProviderKind::CodexAppServer => Capability::supported(
                    "The supported app-server adapter has explicit thread resume",
                    HELP_SOURCE,
                ),
                ProviderKind::ClaudeCli if profile.session_continuity => Capability::supported(
                    "Configured session-id/resume flags passed compatibility checks",
                    HELP_SOURCE,
                ),
                ProviderKind::ClaudeCli => Capability::unknown(
                    "Session continuation is not configured; resume flags were not probed",
                    "configured_profile",
                ),
            };
            match profile.provider {
                ProviderKind::CodexCli | ProviderKind::CodexAppServer => {
                    discover_codex(host, profile, &workspace, deadline, &mut catalog);
                }
                ProviderKind::ClaudeCli => {
                    catalog.startup_context = Capability::unknown(
                        "No Claude control session was started: restricted settings cannot guarantee suppression of managed startup hooks",
                        "claude_sdk:initialize_contract",
                    );
                    catalog.model_catalog = Capability::unknown(
                        "Claude supportedModels uses initialize metadata, but safe startup under configured managed policy is unverified; no API-key catalog is substituted",
                        "claude_sdk:initialize_contract",
                    );
                }
            }
        }
        Err(failure) => {
            catalog.compatibility =
                Capability::unknown(probe_failure_reason(&failure), HELP_SOURCE);
            record_cleanup(&mut catalog, &failure);
        }
    }
    if catalog.process_cleanup.state == CapabilityState::Supported {
        let _ = fs::remove_dir_all(&workspace);
        if !guard_matches(&cleanup_marker, &marker) || fs::remove_file(&cleanup_marker).is_err() {
            catalog.process_cleanup = Capability::unknown(
                "Processes stopped, but the durable discovery guard could not be removed; inspect the host before retrying",
                "relay:supervisor",
            );
        }
    }
    // Unconfirmed cleanup deliberately retains both marker and workspace. Never
    // infer process termination from an elapsed timeout or service restart.

    catalog
}

fn probe_failure_reason(failure: &CommandResult) -> &'static str {
    match failure.outcome {
        Outcome::TimedOut => "Discovery timed out within its bounded deadline",
        Outcome::Unknown => {
            "Discovery process cleanup is unconfirmed; manual host inspection is required"
        }
        Outcome::Cancelled => "Discovery was interrupted",
        _ => "Configured CLI version/help or protocol checks did not complete successfully",
    }
}
fn record_cleanup(catalog: &mut ProfileCatalog, result: &CommandResult) {
    catalog.process_cleanup = if result.outcome == Outcome::Unknown {
        Capability::unknown(
            "Process-tree cleanup was not confirmed; inspect the host before retrying",
            "relay:supervisor",
        )
    } else {
        Capability::supported(
            "Discovery subprocess tree stopped and was reaped",
            "relay:supervisor",
        )
    };
}
fn discover_codex(
    host: &Host,
    profile: &NativeProfile,
    workspace: &Path,
    deadline: Instant,
    catalog: &mut ProfileCatalog,
) {
    if Instant::now() >= deadline {
        catalog.model_catalog = Capability::unknown(
            "Discovery deadline was exhausted by version/help checks",
            CODEX_SOURCE,
        );
        return;
    }
    // For Codex exec profiles, independently verify the same configured binary's
    // app-server subcommand before using its read-only catalog endpoint.
    if profile.provider == ProviderKind::CodexCli {
        let mut app_profile = profile.clone();
        app_profile.provider = ProviderKind::CodexAppServer;
        let response = host.run_supervised(
            CommandSpec {
                workspace_lease: false,
                git_inventory: false,
                catalog: false,
                app_server: None,
                provider: None,
                read_only: false,
                clear_env: false,
                program: profile.program.clone(),
                args: app_profile.help_args(),
                env: profile.env.clone(),
                cwd: workspace.join("repository"),
                input: String::new(),
                timeout_ms: remaining_ms(deadline),
                output_limit_bytes: MAX_PROTOCOL_LINE,
            },
            &AtomicBool::new(false),
            workspace,
            "catalog-help",
        );
        record_cleanup(catalog, &response);
        if response.outcome != Outcome::Success
            || response.error.is_some()
            || response.stdout_truncated
            || response.stderr_truncated
            || app_profile
                .validate_probe(
                    catalog.cli_version.as_deref().unwrap_or_default(),
                    &format!("{}\n{}", response.stdout, response.stderr),
                    false,
                )
                .is_err()
        {
            catalog.model_catalog = Capability::unknown(
                "Configured Codex binary did not verify the read-only app-server discovery contract",
                HELP_SOURCE,
            );
            return;
        }
    }
    if Instant::now() >= deadline {
        catalog.model_catalog = Capability::unknown(
            "Discovery deadline was exhausted before model listing",
            CODEX_SOURCE,
        );
        return;
    }
    catalog.startup_context = Capability::supported(
        "Configured executable/environment in a private empty working directory; CLI user settings remain active; no thread or turn is started",
        "relay:codex_catalog_context",
    );
    let response = host.run_supervised(
        CommandSpec {
            workspace_lease: false,
            git_inventory: false,
            catalog: true,
            app_server: None,
            provider: Some(ProviderResult::new(profile, catalog.cli_version.clone())),
            read_only: true,
            clear_env: false,
            program: profile.program.clone(),
            args: vec!["app-server".into()],
            env: profile.env.clone(),
            cwd: workspace.join("repository"),
            input: String::new(),
            timeout_ms: remaining_ms(deadline),
            output_limit_bytes: 1024,
        },
        &AtomicBool::new(false),
        workspace,
        "catalog-models",
    );
    record_cleanup(catalog, &response);
    match response.catalog {
        Some(models) if response.outcome == Outcome::Success && response.error.is_none() => {
            catalog.models = models;
            catalog.model_catalog = Capability::supported(
                "Complete bounded model/list catalog returned by the configured CLI; access and authentication remain unverified",
                CODEX_SOURCE,
            );
            catalog.selection.status = selection_status(&catalog.selection, &catalog.models);
        }
        _ => {
            catalog.model_catalog =
                Capability::unknown(probe_failure_reason(&response), CODEX_SOURCE);
        }
    }
}
fn remaining_ms(deadline: Instant) -> u64 {
    deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .max(1)
        .min(u64::MAX as u128) as u64
}
fn selection_status(selection: &Selection, models: &[ModelCapability]) -> Capability {
    let Some(requested) = &selection.requested_model else {
        return Capability::unknown(
            "No model was explicitly configured; a catalog default does not establish execution-effective settings",
            CODEX_SOURCE,
        );
    };
    let Some(model) = models
        .iter()
        .find(|m| m.model == *requested || m.id == *requested)
    else {
        return Capability::unknown(
            "Configured model was not listed; aliases or provider-specific resolution remain unverified",
            CODEX_SOURCE,
        );
    };
    if let Some(effort) = &selection.requested_effort {
        match &model.supported_efforts {
            Some(efforts) if !efforts.iter().any(|item| item.effort == *effort) => {
                return Capability::unsupported(
                    "Configured effort is absent from this model's reported supported efforts",
                    CODEX_SOURCE,
                );
            }
            None => {
                return Capability::unknown(
                    "Configured model was listed, but effort metadata was not reported",
                    CODEX_SOURCE,
                );
            }
            _ => {}
        }
    }
    Capability::supported(
        "Configured selection is advertised by the catalog; execution-effective settings and access remain unverified",
        CODEX_SOURCE,
    )
}

/// Catalog-only JSONL state machine, driven by the same bounded supervisor as
/// native execution. Unknown server requests are denied and terminate discovery.
pub(crate) struct CatalogDriver {
    line: Vec<u8>,
    pending: Vec<u8>,
    request: u64,
    initialized: bool,
    complete: bool,
    error: Option<String>,
    models: Vec<ModelCapability>,
    ids: BTreeSet<String>,
    cursors: BTreeSet<String>,
    pages: usize,
    messages: usize,
    bytes: usize,
}
impl CatalogDriver {
    pub(crate) fn new() -> Self {
        let mut driver = Self {
            line: Vec::new(),
            pending: Vec::new(),
            request: 1,
            initialized: false,
            complete: false,
            error: None,
            models: Vec::new(),
            ids: BTreeSet::new(),
            cursors: BTreeSet::new(),
            pages: 0,
            messages: 0,
            bytes: 0,
        };
        driver.send(json!({"id":1,"method":"initialize","params":{
            "clientInfo":{"name":"relay_catalog","version":env!("CARGO_PKG_VERSION")},
            "capabilities":{"experimentalApi":false}
        }}));
        driver
    }
    fn send(&mut self, message: Value) {
        let bytes = serde_json::to_vec(&message).expect("JSON value");
        if self.pending.len() + bytes.len() + 1 > MAX_PROTOCOL_LINE {
            self.fail("catalog outbound protocol budget exceeded");
        } else {
            self.pending.extend(bytes);
            self.pending.push(b'\n');
        }
    }
    fn list(&mut self, cursor: Option<String>) {
        self.request += 1;
        let mut params = json!({"limit":32,"includeHidden":true});
        if let Some(cursor) = cursor {
            params["cursor"] = Value::String(cursor);
        }
        self.send(json!({"id":self.request,"method":"model/list","params":params}));
    }
    pub(crate) fn take_pending(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
    pub(crate) fn stopped(&self) -> bool {
        self.complete || self.error.is_some()
    }
    pub(crate) fn failure(&self) -> Option<&str> {
        self.error.as_deref()
    }
    fn fail(&mut self, reason: &str) {
        if self.error.is_none() {
            self.error = Some(reason.into());
        }
    }
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        self.bytes = self.bytes.saturating_add(bytes.len());
        if self.bytes > MAX_PROTOCOL_BYTES {
            self.fail("catalog protocol byte budget exceeded");
        }
        for &byte in bytes {
            if self.error.is_some() {
                break;
            }
            if byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                self.messages += 1;
                if self.messages > MAX_MESSAGES {
                    self.fail("catalog message budget exceeded");
                    break;
                }
                match serde_json::from_slice::<Value>(&line) {
                    Ok(event) => {
                        if let Err(reason) = self.event(&event) {
                            self.fail(reason);
                        }
                    }
                    Err(_) => self.fail("malformed catalog JSONL message"),
                }
            } else if self.line.len() >= MAX_PROTOCOL_LINE {
                self.fail("catalog JSONL line exceeds 64 KiB");
            } else {
                self.line.push(byte);
            }
        }
    }
    fn event(&mut self, event: &Value) -> Result<(), &'static str> {
        let object = event
            .as_object()
            .ok_or("catalog protocol message must be an object")?;
        if let Some(id) = object.get("id") {
            if object.contains_key("method") {
                // Bound untrusted IDs before reflecting them to the local CLI.
                if id.as_u64().is_some() || id.as_str().is_some_and(|s| s.len() <= MAX_FIELD) {
                    self.send(json!({"id":id,"error":{"code":-32601,"message":"Relay discovery does not authorize server requests"}}));
                }
                return Err(
                    "catalog server requested approval, authentication, or a client action",
                );
            }
            if self.complete || id.as_u64() != Some(self.request) {
                return Err("unexpected catalog response ID");
            }
            if object.get("error").is_some_and(|e| !e.is_null()) {
                return Err("catalog request was rejected by the provider");
            }
            let result = object
                .get("result")
                .filter(|r| r.is_object())
                .ok_or("catalog response requires an object result")?;
            if !self.initialized {
                self.initialized = true;
                self.send(json!({"method":"initialized","params":{}}));
                self.list(None);
                return Ok(());
            }
            self.pages += 1;
            if self.pages > MAX_PAGES {
                return Err("catalog pagination limit exceeded");
            }
            let data = result
                .get("data")
                .and_then(Value::as_array)
                .ok_or("catalog model/list requires a data array")?;
            if self.models.len() + data.len() > MAX_MODELS {
                return Err("catalog model limit exceeded");
            }
            for value in data {
                let model = parse_model(value)?;
                if !self.ids.insert(model.id.clone()) {
                    return Err("catalog contains duplicate model IDs");
                }
                self.models.push(model);
            }
            if serde_json::to_vec(&self.models)
                .map_err(|_| "cannot encode catalog")?
                .len()
                > MAX_CATALOG_BYTES
            {
                return Err("catalog result byte budget exceeded");
            }
            match result.get("nextCursor") {
                Some(Value::Null) => self.complete = true,
                Some(Value::String(cursor)) if valid_text(cursor, MAX_FIELD) => {
                    if !self.cursors.insert(cursor.clone()) {
                        return Err("catalog pagination cursor repeated");
                    }
                    if self.pages == MAX_PAGES {
                        return Err("catalog pagination limit exceeded");
                    }
                    self.list(Some(cursor.clone()));
                }
                _ => return Err("catalog response requires a bounded cursor or explicit null"),
            }
            return Ok(());
        }
        // Notifications contain no catalog evidence and are never copied out.
        if object
            .get("method")
            .and_then(Value::as_str)
            .is_some_and(|m| valid_text(m, MAX_FIELD))
        {
            return Ok(());
        }
        Err("catalog protocol message is neither a response nor a notification")
    }
    pub(crate) fn models(&self) -> Option<Vec<ModelCapability>> {
        (self.error.is_none() && self.complete && self.line.iter().all(u8::is_ascii_whitespace))
            .then(|| self.models.clone())
    }
    pub(crate) fn finish(mut self) -> Result<Vec<ModelCapability>, String> {
        if self.error.is_none() && !self.line.iter().all(u8::is_ascii_whitespace) {
            self.fail("catalog stream ended with an incomplete JSONL message");
        }
        if self.error.is_none() && !self.complete {
            self.fail("catalog stream ended before all pages completed");
        }
        match self.error {
            Some(error) => Err(error),
            None => Ok(self.models),
        }
    }
}
fn valid_text(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}
fn text_field(value: &Value, name: &str, limit: usize) -> Result<Option<String>, &'static str> {
    match value.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s))
            if valid_text(s, limit) || (name == "description" && s.is_empty()) =>
        {
            Ok(Some(s.clone()))
        }
        _ => Err("catalog string metadata is invalid or exceeds its bound"),
    }
}
fn bool_field(value: &Value, name: &str) -> Result<Option<bool>, &'static str> {
    match value.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        _ => Err("catalog boolean metadata is invalid"),
    }
}
fn parse_model(value: &Value) -> Result<ModelCapability, &'static str> {
    if !value.is_object() {
        return Err("catalog model must be an object");
    }
    let id = text_field(value, "id", MAX_FIELD)?.ok_or("catalog model requires an ID")?;
    let model =
        text_field(value, "model", MAX_FIELD)?.ok_or("catalog model requires a model name")?;
    let display_name =
        text_field(value, "displayName", MAX_FIELD)?.unwrap_or_else(|| model.clone());
    let supported_efforts = match value.get("supportedReasoningEfforts") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) if items.len() <= MAX_EFFORTS => {
            let mut efforts = Vec::new();
            let mut names = BTreeSet::new();
            for item in items {
                let effort = text_field(item, "reasoningEffort", MAX_FIELD)?
                    .ok_or("catalog effort requires a name")?;
                if !names.insert(effort.clone()) {
                    return Err("catalog contains duplicate efforts");
                }
                efforts.push(EffortCapability {
                    effort,
                    description: text_field(item, "description", MAX_DESCRIPTION)?,
                });
            }
            Some(efforts)
        }
        _ => return Err("catalog effort list is invalid or exceeds its bound"),
    };
    Ok(ModelCapability {
        id,
        model,
        display_name,
        description: text_field(value, "description", MAX_DESCRIPTION)?,
        default_effort: text_field(value, "defaultReasoningEffort", MAX_FIELD)?,
        supported_efforts,
        is_default: bool_field(value, "isDefault")?,
        hidden: bool_field(value, "hidden")?,
        source: CODEX_SOURCE.into(),
    })
}

/// Cache identity deliberately cannot be serialized or debug-printed. Profile
/// serialization is streamed into a hash, never retained as a credential copy.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ProfileStamp {
    profile: u64,
    executable: Option<ExecutableStamp>,
}
#[derive(Clone, PartialEq, Eq)]
struct ExecutableStamp {
    device: u64,
    inode: u64,
    length: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    mode: u32,
}
pub(crate) fn profile_stamp(profile: &NativeProfile) -> ProfileStamp {
    use std::hash::{DefaultHasher, Hasher};
    use std::os::unix::fs::MetadataExt;
    struct HashWriter(DefaultHasher);
    impl std::io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.write(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(DefaultHasher::new());
    // A NativeProfile consists entirely of serializable fields.
    serde_json::to_writer(&mut writer, profile).expect("serializable native profile");
    ProfileStamp {
        profile: writer.0.finish(),
        executable: fs::metadata(&profile.program)
            .ok()
            .map(|m| ExecutableStamp {
                device: m.dev(),
                inode: m.ino(),
                length: m.len(),
                modified: (m.mtime(), m.mtime_nsec()),
                changed: (m.ctime(), m.ctime_nsec()),
                mode: m.mode(),
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn feed(driver: &mut CatalogDriver, value: Value) {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        for chunk in bytes.chunks(7) {
            driver.feed(chunk);
        }
    }
    fn sent(driver: &mut CatalogDriver) -> Vec<Value> {
        String::from_utf8(driver.take_pending())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn ready() -> CatalogDriver {
        let mut d = CatalogDriver::new();
        let init = sent(&mut d);
        assert_eq!(init.len(), 1);
        assert_eq!(init[0]["method"], "initialize");
        feed(&mut d, json!({"id":1,"result":{}}));
        let requests = sent(&mut d);
        assert_eq!(requests[0]["method"], "initialized");
        assert_eq!(requests[1]["method"], "model/list");
        assert_eq!(
            requests[1]["params"],
            json!({"limit":32,"includeHidden":true})
        );
        d
    }
    fn model(id: &str) -> Value {
        json!({"id":id,"model":id,"displayName":"Fixture model"})
    }
    #[test]
    fn only_initializes_and_lists_all_bounded_pages() {
        let mut d = ready();
        let mut first = model("model-one");
        first["supportedReasoningEfforts"] =
            json!([{"reasoningEffort":"future-effort","description":"Reported, not hardcoded"}]);
        first["defaultReasoningEffort"] = json!("future-effort");
        first["isDefault"] = json!(true);
        feed(
            &mut d,
            json!({"id":2,"result":{"data":[first],"nextCursor":"page-2"}}),
        );
        let next = sent(&mut d);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0]["method"], "model/list");
        assert_eq!(next[0]["params"]["cursor"], "page-2");
        feed(
            &mut d,
            json!({"method":"account/updated","params":{"access_token":"never-return-this"}}),
        );
        feed(
            &mut d,
            json!({"id":3,"result":{"data":[model("model-two")],"nextCursor":null}}),
        );
        assert!(sent(&mut d).is_empty());
        let models = d.finish().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(
            models[0].supported_efforts.as_ref().unwrap()[0].effort,
            "future-effort"
        );
        assert!(models[1].supported_efforts.is_none());
        assert!(
            !serde_json::to_string(&models)
                .unwrap()
                .contains("never-return-this")
        );
    }
    #[test]
    fn rejects_errors_wrong_ids_partial_and_malformed_results() {
        for response in [
            json!({"id":88,"result":{"data":[],"nextCursor":null}}),
            json!({"id":2,"error":{"code":-32000,"message":"secret-provider-error"}}),
            json!({"id":2,"result":{"data":[]}}),
            json!({"id":2,"result":{"data":"wrong","nextCursor":null}}),
            json!({"id":2,"result":{"data":[{"id":"broken"}],"nextCursor":null}}),
        ] {
            let mut d = ready();
            feed(&mut d, response);
            let error = d.finish().unwrap_err();
            assert!(!error.contains("secret-provider-error"));
        }
        let mut d = ready();
        feed(
            &mut d,
            json!({"id":2,"result":{"data":[model("partial")],"nextCursor":"next"}}),
        );
        assert!(d.models().is_none());
        assert!(d.finish().is_err());
        let mut d = ready();
        d.feed(b"broken\n");
        assert!(d.finish().is_err());
    }
    #[test]
    fn rejects_duplicate_ids_and_looping_cursors() {
        let mut d = ready();
        feed(
            &mut d,
            json!({"id":2,"result":{"data":[model("dup"),model("dup")],"nextCursor":null}}),
        );
        assert!(d.finish().is_err());
        let mut d = ready();
        feed(
            &mut d,
            json!({"id":2,"result":{"data":[],"nextCursor":"same"}}),
        );
        sent(&mut d);
        feed(
            &mut d,
            json!({"id":3,"result":{"data":[],"nextCursor":"same"}}),
        );
        assert!(d.finish().is_err());
    }
    #[test]
    fn denies_server_actions_without_copying_parameters() {
        let mut d = ready();
        feed(
            &mut d,
            json!({"id":"approval","method":"account/login/start","params":{"apiKey":"secret-never-return"}}),
        );
        let denial = sent(&mut d);
        assert_eq!(denial.len(), 1);
        assert_eq!(denial[0]["error"]["code"], -32601);
        assert!(!denial[0].to_string().contains("secret-never-return"));
        assert!(d.finish().is_err());
    }
    #[test]
    fn enforces_model_page_line_message_and_total_byte_budgets() {
        let mut d = ready();
        let models: Vec<_> = (0..=MAX_MODELS).map(|n| model(&format!("m{n}"))).collect();
        feed(
            &mut d,
            json!({"id":2,"result":{"data":models,"nextCursor":null}}),
        );
        assert!(d.finish().is_err());
        let mut d = ready();
        for page in 0..MAX_PAGES {
            feed(
                &mut d,
                json!({"id":page+2,"result":{"data":[],"nextCursor":format!("p{page}")}}),
            );
            sent(&mut d);
        }
        assert!(d.finish().is_err());
        let mut d = ready();
        d.feed(&vec![b'x'; MAX_PROTOCOL_LINE + 1]);
        assert!(d.finish().is_err());
        let mut d = ready();
        for _ in 0..MAX_MESSAGES {
            feed(&mut d, json!({"method":"ignored"}));
        }
        assert!(d.finish().is_err());
        let mut d = ready();
        d.feed(&vec![b' '; MAX_PROTOCOL_BYTES + 1]);
        assert!(d.finish().is_err());
    }
    #[test]
    fn preserves_unknown_efforts_and_execution_effective_selection() {
        let profile: NativeProfile=serde_json::from_value(json!({"provider":"codex_app_server","program":"/bin/true","model":"fixture","effort":"high"})).unwrap();
        let mut catalog = ProfileCatalog::unknown(&profile);
        let mut m = parse_model(&model("fixture")).unwrap();
        assert_eq!(
            selection_status(&catalog.selection, &[m.clone()]).state,
            CapabilityState::Unknown
        );
        m.supported_efforts = Some(vec![]);
        assert_eq!(
            selection_status(&catalog.selection, &[m.clone()]).state,
            CapabilityState::Unsupported
        );
        m.supported_efforts = Some(vec![EffortCapability {
            effort: "high".into(),
            description: None,
        }]);
        catalog.selection.status = selection_status(&catalog.selection, &[m]);
        assert_eq!(catalog.selection.status.state, CapabilityState::Supported);
        assert!(
            catalog.selection.effective_model.is_none()
                && catalog.selection.effective_effort.is_none()
        );
        assert_eq!(catalog.authentication.state, CapabilityState::Unknown);
        assert_eq!(
            catalog.reviewer_isolation.state,
            CapabilityState::Unsupported
        );
    }
    #[test]
    fn stamp_changes_for_profile_and_executable_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("fake");
        fs::write(&program, "one").unwrap();
        let mut p: NativeProfile =
            serde_json::from_value(json!({"provider":"codex_app_server","program":program}))
                .unwrap();
        let a = profile_stamp(&p);
        assert!(a == profile_stamp(&p));
        p.env.insert("PRIVATE".into(), "do-not-export".into());
        assert!(a != profile_stamp(&p));
        let b = profile_stamp(&p);
        fs::write(program, "different-length").unwrap();
        assert!(b != profile_stamp(&p));
    }
}
