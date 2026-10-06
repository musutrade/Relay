//! Typed, trusted native CLI profiles and bounded streaming protocol normalization.
//! Provider configuration never comes from a job. This module does not authenticate,
//! install a CLI, choose credentials, or treat a tool allowlist as an OS sandbox.
use crate::host::CommandProfile;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const MAX_PROTOCOL_LINE: usize = 64 * 1024;
const MAX_FIELD: usize = 256;
const MAX_SUMMARY: usize = 4096;
pub(crate) const MAX_SESSION_POLICY: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    CodexCli,
    CodexAppServer,
    ClaudeCli,
}

/// An explicit native developer permission mode, or Relay's fixed reviewer contract.
/// Availability and account/model eligibility are still owned by the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativePermission {
    CodexWorkspaceWrite,
    CodexFullAccess,
    /// Local command sandbox only. Native startup/integrations remain trusted.
    CodexNativeSandboxedReview,
    ClaudeDontAsk,
    ClaudeAuto,
    ClaudeBypassPermissions,
    ClaudeRestricted,
}
impl NativePermission {
    pub fn requires_confirmation(self) -> bool {
        !matches!(self, Self::CodexWorkspaceWrite | Self::ClaudeRestricted)
    }
    pub fn compatible(self, provider: ProviderKind, read_only: bool) -> bool {
        matches!(
            (self, provider, read_only),
            (
                Self::CodexWorkspaceWrite | Self::CodexFullAccess,
                ProviderKind::CodexCli | ProviderKind::CodexAppServer,
                false,
            ) | (
                Self::ClaudeDontAsk | Self::ClaudeAuto | Self::ClaudeBypassPermissions,
                ProviderKind::ClaudeCli,
                false,
            ) | (Self::ClaudeRestricted, ProviderKind::ClaudeCli, true)
                | (
                    Self::CodexNativeSandboxedReview,
                    ProviderKind::CodexAppServer,
                    true
                )
        )
    }
    pub(crate) fn claude_mode(self) -> Option<&'static str> {
        match self {
            Self::ClaudeDontAsk => Some("dontAsk"),
            Self::ClaudeAuto => Some("auto"),
            Self::ClaudeBypassPermissions => Some("bypassPermissions"),
            _ => None,
        }
    }
}

/// Selection values are literals, never flags or unescaped configuration syntax.
pub fn validate_selection_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_FIELD
        && !value.starts_with('-')
        && !value.chars().any(char::is_control)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeProfile {
    pub provider: ProviderKind,
    pub program: PathBuf,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_permission: Option<NativePermission>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_permission_modes: Vec<NativePermission>,
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub max_budget_usd: Option<f64>,
    /// Retain an explicit, task/role-scoped Claude session. Codex app-server always retains its thread.
    #[serde(default)]
    pub session_continuity: bool,
    /// Host opt-in only. Each standalone startup still needs fresh confirmation.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_startup_discovery: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderProbe {
    pub provider: ProviderKind,
    pub cli_version: String,
    pub read_only_supported: bool,
    /// An advertised, current control stream may observe metadata in an existing task.
    #[serde(default)]
    pub task_model_observation_supported: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl NativeProfile {
    pub fn native_sandboxed_review(&self) -> bool {
        self.provider == ProviderKind::CodexAppServer
            && self.native_permission == Some(NativePermission::CodexNativeSandboxedReview)
    }
    pub fn reviewer_supported(&self) -> bool {
        if self.native_sandboxed_review() {
            self.allowed_permission_modes
                .contains(&NativePermission::CodexNativeSandboxedReview)
                && !self.session_continuity
        } else {
            self.provider == ProviderKind::ClaudeCli
                && self
                    .native_permission
                    .is_none_or(|mode| mode.compatible(self.provider, true))
        }
    }
    pub fn reviewer_contract(&self) -> &'static str {
        if !self.reviewer_supported() {
            "unsupported"
        } else if self.native_sandboxed_review() {
            "native_local_read_only"
        } else {
            "strict_no_execution"
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        if !self.program.is_absolute() || !self.program.is_file() {
            return Err("native CLI requires an existing absolute executable path".into());
        }
        if self
            .env
            .iter()
            .any(|(key, value)| key.is_empty() || key.contains(['=', '\0']) || value.contains('\0'))
        {
            return Err("invalid native CLI environment".into());
        }
        if self
            .model
            .as_ref()
            .is_some_and(|model| !validate_selection_value(model))
        {
            return Err("native model must contain 1–256 bytes without control characters".into());
        }
        if self
            .effort
            .as_ref()
            .is_some_and(|effort| !validate_selection_value(effort))
        {
            return Err("native effort must contain 1–256 bytes without control characters or a leading '-'".into());
        }
        if self.allowed_permission_modes.len() > 7
            || self
                .native_permission
                .iter()
                .chain(&self.allowed_permission_modes)
                .any(|mode| {
                    !mode.compatible(self.provider, false) && !mode.compatible(self.provider, true)
                })
        {
            return Err(
                "native permission modes must be bounded and compatible with the provider".into(),
            );
        }
        if self
            .max_turns
            .is_some_and(|value| value == 0 || value > 100)
            || self
                .max_budget_usd
                .is_some_and(|value| !value.is_finite() || value <= 0.0 || value > 1000.0)
        {
            return Err("Claude limits require max_turns 1–100 and max_budget_usd >0–1000".into());
        }
        if self.provider != ProviderKind::ClaudeCli
            && (self.max_turns.is_some() || self.max_budget_usd.is_some())
        {
            return Err("max_turns and max_budget_usd are Claude-only settings".into());
        }
        if self.allow_startup_discovery && self.provider != ProviderKind::ClaudeCli {
            return Err(
                "allow_startup_discovery is only supported for Claude native profiles".into(),
            );
        }
        if self.session_continuity && self.provider == ProviderKind::CodexCli {
            return Err("Codex session continuity requires provider codex_app_server".into());
        }
        if self.native_sandboxed_review() && self.session_continuity {
            return Err(
                "Codex native sandboxed review supports fresh sessions only; resume is unsupported"
                    .into(),
            );
        }
        if serde_json::to_vec(self)
            .map_err(|error| error.to_string())?
            .len()
            > 64 * 1024
        {
            return Err("native profile exceeds 64 KiB".into());
        }
        Ok(())
    }

    /// Compile only trusted typed settings. The prompt is always sent over stdin.
    /// Callers must verify version/help capabilities before executing these arguments.
    pub fn compile(&self, read_only: bool) -> Result<CommandProfile, String> {
        self.validate()?;
        if read_only && !self.reviewer_supported() {
            return Err("review_profile_unsupported: requires restricted Claude or explicitly host-allowed Codex native sandboxed review; strict Codex reviewer isolation contract is unproven".into());
        }
        if self
            .native_permission
            .is_some_and(|mode| !mode.compatible(self.provider, read_only))
        {
            return Err("native permission mode is incompatible with the provider role".into());
        }
        if self.provider == ProviderKind::CodexAppServer {
            return Ok(CommandProfile {
                program: self.program.clone(),
                args: vec!["app-server".into()],
                env: self.env.clone(),
            });
        }
        let mut args: Vec<String> = match self.provider {
            ProviderKind::CodexAppServer => unreachable!(),
            ProviderKind::CodexCli => vec![
                "exec".into(),
                "--json".into(),
                "--ephemeral".into(),
                "--sandbox".into(),
                if self.native_permission == Some(NativePermission::CodexFullAccess) {
                    "danger-full-access"
                } else {
                    "workspace-write"
                }
                .into(),
                "--skip-git-repo-check".into(),
            ],
            ProviderKind::ClaudeCli => vec![
                "-p".into(),
                "--output-format".into(),
                "stream-json".into(),
                "--verbose".into(),
                "--permission-prompts".into(),
                "none".into(),
                "--no-session-persistence".into(),
            ],
        };
        if let Some(permission) = self.native_permission {
            match self.provider {
                ProviderKind::CodexCli => {
                    args.extend(["-c".into(), "approval_policy=\"never\"".into()]);
                }
                ProviderKind::ClaudeCli => {
                    if let Some(mode) = permission.claude_mode() {
                        args.extend(["--permission-mode".into(), mode.into()]);
                    }
                }
                ProviderKind::CodexAppServer => unreachable!(),
            }
        }
        if let Some(model) = &self.model {
            args.extend(["--model".into(), model.clone()]);
        }
        if let Some(effort) = &self.effort {
            match self.provider {
                ProviderKind::CodexCli | ProviderKind::CodexAppServer => {
                    let literal =
                        serde_json::to_string(effort).map_err(|error| error.to_string())?;
                    args.extend(["-c".into(), format!("model_reasoning_effort={literal}")])
                }
                ProviderKind::ClaudeCli => args.extend(["--effort".into(), effort.clone()]),
            }
        }
        if let Some(turns) = self.max_turns {
            args.extend(["--max-turns".into(), turns.to_string()]);
        }
        if let Some(budget) = self.max_budget_usd {
            args.extend(["--max-budget-usd".into(), budget.to_string()]);
        }
        if read_only && self.provider == ProviderKind::ClaudeCli {
            // --tools restricts built-ins, not MCP. Restricted mode avoids project/
            // user hooks and settings; deny MCP, code execution and delegation too.
            args.extend([
                "--restricted".into(),
                "--tools".into(),
                "Read,Glob,Grep".into(),
                "--allowedTools".into(),
                "Read,Glob,Grep".into(),
                "--disallowedTools".into(),
                "Bash,Edit,Write,NotebookEdit,Agent,Task,mcp__*".into(),
                "--disable-slash-commands".into(),
                "--strict-mcp-config".into(),
                "--mcp-config".into(),
                "{\"mcpServers\":{}}".into(),
            ]);
        }
        if self.session_continuity {
            args.retain(|arg| arg != "--no-session-persistence");
        }
        if self.provider == ProviderKind::CodexCli {
            args.push("-".into());
        }
        Ok(CommandProfile {
            program: self.program.clone(),
            args,
            env: self.env.clone(),
        })
    }

    pub fn help_args(&self) -> Vec<String> {
        match self.provider {
            ProviderKind::CodexCli => vec!["exec".into(), "--help".into()],
            ProviderKind::CodexAppServer => vec!["app-server".into(), "--help".into()],
            ProviderKind::ClaudeCli => vec!["--help".into()],
        }
    }

    /// Fail closed when the probed CLI cannot demonstrate the compiled contract.
    pub fn validate_probe(
        &self,
        version: &str,
        help: &str,
        read_only: bool,
    ) -> Result<String, String> {
        self.validate_probe_with_max_turns(version, help, read_only, false)
    }

    pub(crate) fn task_model_observation_supported(&self, version: &str, help: &str) -> bool {
        self.provider == ProviderKind::ClaudeCli
            && parse_version(version).is_some_and(|version| version.0 >= (2, 1, 291))
            && help_has_flag(help, "--input-format")
    }

    pub(crate) fn hidden_max_turns_probe_needed(&self, help: &str) -> bool {
        self.provider == ProviderKind::ClaudeCli
            && self.max_turns.is_some()
            && !help_has_flag(help, "--max-turns")
    }

    pub(crate) fn validate_probe_with_max_turns(
        &self,
        version: &str,
        help: &str,
        read_only: bool,
        hidden_max_turns_verified: bool,
    ) -> Result<String, String> {
        if read_only && !self.reviewer_supported() {
            return Err("review_profile_unsupported: requires restricted Claude or explicitly host-allowed Codex native sandboxed review; strict Codex reviewer isolation contract is unproven".into());
        }
        if self
            .native_permission
            .is_some_and(|mode| !mode.compatible(self.provider, read_only))
        {
            return Err("native permission mode is incompatible with the provider role".into());
        }
        let version = parse_version(version).ok_or("CLI version was not recognizable")?;
        if self.provider == ProviderKind::ClaudeCli && version.0 < (2, 1, 259) {
            return Err(
                "Claude CLI 2.1.259 or later is required for --permission-prompts none".into(),
            );
        }
        if self.provider == ProviderKind::CodexAppServer {
            if read_only && self.native_sandboxed_review() && version.0 != (0, 160, 1) {
                return Err("Codex native sandboxed review requires verified app-server 0.160.1; other versions are unsupported".into());
            }
            if version.0 < (0, 160, 0) || !help.contains("app-server") {
                return Err("Codex app-server 0.160.0 or later is required".into());
            }
            return Ok(version.1);
        }
        let mut required = match self.provider {
            ProviderKind::CodexAppServer => unreachable!(),
            ProviderKind::CodexCli => vec![
                "--json",
                "--sandbox",
                "--skip-git-repo-check",
                "--ephemeral",
            ],
            ProviderKind::ClaudeCli => vec![
                "--output-format",
                "--verbose",
                "--permission-prompts",
                "--no-session-persistence",
            ],
        };
        if self.session_continuity {
            required.retain(|flag| *flag != "--no-session-persistence");
            required.extend(["--resume", "--session-id"]);
        }
        if self.model.is_some() {
            required.push("--model");
        }
        if self.effort.is_some() {
            required.push(if self.provider == ProviderKind::CodexCli {
                "--config"
            } else {
                "--effort"
            });
        }
        if let Some(permission) = self.native_permission {
            if self.provider == ProviderKind::CodexCli {
                required.push("--config");
            } else if permission.claude_mode().is_some() {
                // Flag availability does not certify Claude auto eligibility for
                // this model/provider. Native rejection is surfaced, never bypassed.
                required.push("--permission-mode");
            }
        }
        if self.max_turns.is_some() && !hidden_max_turns_verified {
            required.push("--max-turns");
        }
        if self.max_budget_usd.is_some() {
            required.push("--max-budget-usd");
        }
        if read_only && self.provider == ProviderKind::ClaudeCli {
            required.extend([
                "--restricted",
                "--tools",
                "--allowedTools",
                "--disallowedTools",
                "--disable-slash-commands",
                "--strict-mcp-config",
                "--mcp-config",
            ]);
        }
        for flag in required {
            if !help_has_flag(help, flag) {
                return Err(format!(
                    "CLI help does not advertise required capability {flag}"
                ));
            }
        }
        Ok(version.1)
    }
}

fn help_has_flag(help: &str, flag: &str) -> bool {
    help.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .any(|word| word == flag)
}

fn parse_version(text: &str) -> Option<((u64, u64, u64), String)> {
    for word in text.split_whitespace().take(32) {
        let word = word.trim_start_matches('v');
        let mut parts = word.split('.');
        let (Some(major), Some(minor), Some(patch)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        // Prereleases do not establish support for the minimum stable contract.
        if parts.next().is_some() {
            continue;
        }
        if let (Ok(major), Ok(minor), Ok(patch)) = (major.parse(), minor.parse(), patch.parse()) {
            return Some(((major, minor, patch), format!("{major}.{minor}.{patch}")));
        }
    }
    None
}

/// Provider token counters; cache and reasoning are subsets for Codex.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenCounts {
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderUsage {
    /// Absent on legacy records and other providers: do not infer turn scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_total: Option<TokenCounts>,
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub total_cost_usd: Option<f64>,
    pub num_turns: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestedSelection {
    pub profile: Option<String>,
    pub provider: ProviderKind,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub native_permission: Option<NativePermission>,
    pub model_source: Option<String>,
}

/// Provider-reported session configuration is not necessarily the model or effort
/// used for the current turn, particularly after a turn override or reroute.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSettings {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub approval_policy: Option<String>,
    /// Native string kind or bounded serialized object, never turn-execution proof.
    pub sandbox: Option<String>,
    pub permission_mode: Option<String>,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelReroute {
    pub thread_id: String,
    pub turn_id: String,
    pub from_model: String,
    pub to_model: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ObservedSelection {
    pub model: Option<String>,
    pub source: Option<String>,
    #[serde(default)]
    pub reroutes: Vec<ModelReroute>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionVerification {
    pub model: String,
    pub effort: String,
    pub permission: String,
}
impl Default for SelectionVerification {
    fn default() -> Self {
        Self {
            model: "unknown".into(),
            effort: "unknown".into(),
            permission: "unknown".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionEvidence {
    pub requested: RequestedSelection,
    pub session_settings: Option<SessionSettings>,
    #[serde(default)]
    pub observed: ObservedSelection,
    #[serde(default)]
    pub verification: SelectionVerification,
    #[serde(default)]
    pub truncated: bool,
}
impl SelectionEvidence {
    pub(crate) fn bound(&mut self) {
        for text in [
            &mut self.requested.profile,
            &mut self.requested.model,
            &mut self.requested.effort,
            &mut self.requested.model_source,
            &mut self.observed.model,
            &mut self.observed.source,
        ]
        .into_iter()
        .flatten()
        {
            self.truncated |= text.len() > MAX_FIELD;
            truncate(text, MAX_FIELD);
        }
        for text in [
            &mut self.verification.model,
            &mut self.verification.effort,
            &mut self.verification.permission,
        ] {
            self.truncated |= text.len() > MAX_FIELD;
            truncate(text, MAX_FIELD);
        }
        if let Some(settings) = &mut self.session_settings {
            for text in [
                &mut settings.model,
                &mut settings.effort,
                &mut settings.permission_mode,
            ]
            .into_iter()
            .flatten()
            {
                self.truncated |= text.len() > MAX_FIELD;
                truncate(text, MAX_FIELD);
            }
            for text in [&mut settings.approval_policy, &mut settings.sandbox]
                .into_iter()
                .flatten()
            {
                self.truncated |= text.len() > MAX_SESSION_POLICY;
                truncate(text, MAX_SESSION_POLICY);
            }
            self.truncated |= settings.source.len() > MAX_FIELD;
            truncate(&mut settings.source, MAX_FIELD);
        }
        self.truncated |= self.observed.reroutes.len() > 8;
        self.observed.reroutes.truncate(8);
        for reroute in &mut self.observed.reroutes {
            for text in [
                &mut reroute.thread_id,
                &mut reroute.turn_id,
                &mut reroute.from_model,
                &mut reroute.to_model,
                &mut reroute.reason,
            ] {
                self.truncated |= text.len() > MAX_FIELD;
                truncate(text, MAX_FIELD);
            }
        }
    }
    pub(crate) fn shrink(&mut self) -> bool {
        let mut changed = false;
        if !self.observed.reroutes.is_empty() {
            self.observed
                .reroutes
                .truncate(self.observed.reroutes.len() / 2);
            changed = true;
        }
        for text in [
            &mut self.requested.profile,
            &mut self.requested.model,
            &mut self.requested.effort,
            &mut self.requested.model_source,
            &mut self.observed.model,
            &mut self.observed.source,
        ] {
            changed |= shrink_optional(text);
        }
        if let Some(settings) = &mut self.session_settings {
            for text in [
                &mut settings.model,
                &mut settings.effort,
                &mut settings.approval_policy,
                &mut settings.sandbox,
                &mut settings.permission_mode,
            ] {
                changed |= shrink_optional(text);
            }
        }
        self.truncated |= changed;
        changed
    }
}
fn shrink_optional(text: &mut Option<String>) -> bool {
    let Some(value) = text else {
        return false;
    };
    truncate(value, value.len() / 2);
    if value.is_empty() {
        *text = None;
    }
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderResult {
    pub provider: ProviderKind,
    pub cli_version: Option<String>,
    pub requested_model: Option<String>,
    pub reported_model: Option<String>,
    pub session_id: Option<String>,
    pub terminal_reason: Option<String>,
    pub summary: String,
    #[serde(default)]
    pub summary_truncated: bool,
    pub usage: ProviderUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<SelectionEvidence>,
}
impl ProviderResult {
    pub fn new(profile: &NativeProfile, cli_version: Option<String>) -> Self {
        Self {
            provider: profile.provider,
            cli_version,
            requested_model: profile.model.clone(),
            reported_model: None,
            session_id: None,
            terminal_reason: None,
            summary: String::new(),
            summary_truncated: false,
            usage: ProviderUsage::default(),
            selection: Some(SelectionEvidence {
                requested: RequestedSelection {
                    profile: None,
                    provider: profile.provider,
                    model: profile.model.clone(),
                    effort: profile.effort.clone(),
                    native_permission: profile.native_permission,
                    model_source: None,
                },
                session_settings: None,
                observed: ObservedSelection::default(),
                verification: SelectionVerification::default(),
                truncated: false,
            }),
        }
    }
    pub(crate) fn observe_message_model(&mut self, model: String, source: &str) {
        self.reported_model = Some(model.clone());
        if let Some(selection) = &mut self.selection {
            selection.observed.model = Some(model);
            selection.observed.source = Some(source.into());
            selection.verification.model = "message_reported".into();
        }
    }
    pub(crate) fn bound(&mut self) {
        for text in [
            &mut self.cli_version,
            &mut self.requested_model,
            &mut self.reported_model,
            &mut self.session_id,
            &mut self.terminal_reason,
        ]
        .into_iter()
        .flatten()
        {
            truncate(text, MAX_FIELD);
        }
        self.summary_truncated |= self.summary.len() > MAX_SUMMARY;
        truncate(&mut self.summary, MAX_SUMMARY);
        if let Some(selection) = &mut self.selection {
            selection.bound();
        }
    }
    pub(crate) fn shrink(&mut self) -> bool {
        let mut changed = false;
        for text in [
            &mut self.cli_version,
            &mut self.requested_model,
            &mut self.reported_model,
            &mut self.session_id,
            &mut self.terminal_reason,
        ]
        .into_iter()
        .flatten()
        {
            if !text.is_empty() {
                truncate(text, text.len() / 2);
                changed = true;
            }
        }
        if !self.summary.is_empty() {
            self.summary_truncated = true;
            let len = self.summary.len() / 2;
            truncate(&mut self.summary, len);
            changed = true;
        }
        if let Some(selection) = &mut self.selection {
            changed |= selection.shrink();
        }
        changed
    }
}

/// Streaming JSONL parser. Storage is independent of total log size; captured stdout
/// is never used to decide success. Fatal errors are sticky, but later lines are
/// still parsed for bounded diagnostics. Oversized lines are skipped to a newline.
pub struct ProtocolParser {
    result: ProviderResult,
    line: Vec<u8>,
    error: Option<String>,
    terminal: bool,
    discarding_line: bool,
    read_only: bool,
    answer_seen: bool,
    app_server: Option<crate::app_server::Driver>,
    catalog: Option<crate::capabilities::CatalogDriver>,
    claude_control: Option<crate::claude_control::Driver>,
    task_output: Vec<u8>,
    task_catalog_time: Option<u64>,
    task_prompt_dispatched: bool,
    catalog_bytes: usize,
    catalog_messages: usize,
    catalog_input_dispatched: bool,
}
impl ProtocolParser {
    pub fn new(mut result: ProviderResult) -> Self {
        result.bound();
        Self {
            result,
            line: Vec::new(),
            error: None,
            terminal: false,
            discarding_line: false,
            read_only: false,
            answer_seen: false,
            app_server: None,
            catalog: None,
            claude_control: None,
            task_output: Vec::new(),
            task_catalog_time: None,
            task_prompt_dispatched: false,
            catalog_bytes: 0,
            catalog_messages: 0,
            catalog_input_dispatched: false,
        }
    }
    pub(crate) fn app_server(result: ProviderResult, start: crate::app_server::Start) -> Self {
        let mut parser = Self::new(result.clone());
        parser.app_server = Some(crate::app_server::Driver::new(result, start));
        parser
    }
    pub(crate) fn catalog(result: ProviderResult) -> Self {
        let mut parser = Self::new(result);
        if parser.result.provider == ProviderKind::ClaudeCli {
            parser.claude_control = Some(crate::claude_control::Driver::catalog());
        } else {
            parser.catalog = Some(crate::capabilities::CatalogDriver::new());
        }
        parser
    }
    pub(crate) fn claude_task(result: ProviderResult, prompt: String) -> Self {
        let mut parser = Self::new(result);
        parser.claude_control = Some(crate::claude_control::Driver::new(prompt));
        parser
    }
    pub(crate) fn mark_catalog_input_complete(&mut self) {
        if self.error.is_none()
            && self
                .claude_control
                .as_ref()
                .is_some_and(|driver| driver.is_catalog())
        {
            self.catalog_input_dispatched = true;
        }
    }
    /// Called only after the supervisor has written every byte of the single
    /// authorized prompt. A queued prompt is not a dispatched task.
    pub(crate) fn mark_task_input_complete(&mut self) {
        if self.error.is_none()
            && self
                .claude_control
                .as_ref()
                .is_some_and(|driver| driver.initialized() && driver.input_done())
        {
            self.task_prompt_dispatched = true;
        }
    }
    pub(crate) fn task_catalog_time(&self) -> Option<u64> {
        self.task_catalog_time
    }
    pub(crate) fn task_initialization_pending(&self) -> bool {
        self.claude_control
            .as_ref()
            .is_some_and(|driver| !driver.initialized())
    }
    pub(crate) fn input_done(&self) -> bool {
        self.claude_control
            .as_ref()
            .is_some_and(|driver| driver.input_done())
    }
    pub(crate) fn task_output_eof(&mut self) -> Option<Vec<u8>> {
        if self.claude_control.as_ref()?.is_catalog() {
            return Some(Vec::new());
        }
        if !self.discarding_line && !self.line.is_empty() {
            self.parse_line();
            self.line.clear();
        }
        self.task_output()
    }
    pub(crate) fn task_output(&mut self) -> Option<Vec<u8>> {
        self.claude_control.as_ref()?;
        Some(std::mem::take(&mut self.task_output))
    }
    pub(crate) fn catalog_result(&self) -> Option<Vec<crate::capabilities::ModelCapability>> {
        if (self
            .claude_control
            .as_ref()
            .is_some_and(|driver| driver.is_catalog())
            && !self.catalog_input_dispatched)
            || self.error.is_some()
            || self.discarding_line
            || !self.line.iter().all(u8::is_ascii_whitespace)
        {
            return None;
        }
        self.catalog
            .as_ref()
            .and_then(|driver| driver.models())
            .or_else(|| {
                self.claude_control
                    .as_ref()
                    .and_then(|driver| driver.models())
            })
    }
    pub(crate) fn pending(&mut self) -> Vec<u8> {
        if let Some(driver) = &mut self.claude_control {
            return driver.pending();
        }
        if let Some(driver) = &mut self.catalog {
            return driver.take_pending();
        }
        self.app_server
            .as_mut()
            .map(|driver| driver.take_pending())
            .unwrap_or_default()
    }
    pub(crate) fn stopped(&self) -> bool {
        if let Some(driver) = &self.catalog {
            return driver.stopped();
        }
        self.app_server
            .as_ref()
            .is_some_and(|driver| driver.stopped())
            || self.claude_control.as_ref().is_some_and(|driver| {
                driver.is_catalog() && (driver.catalog_complete() || driver.failure().is_some())
            })
            || self.error.is_some()
    }
    pub(crate) fn failure(&self) -> Option<&str> {
        if let Some(driver) = &self.catalog {
            return driver.failure();
        }
        self.app_server
            .as_ref()
            .and_then(|driver| driver.failure())
            .or(self.error.as_deref())
            .or_else(|| {
                self.claude_control
                    .as_ref()
                    .and_then(|driver| driver.failure())
            })
    }
    pub fn read_only(mut self, enabled: bool) -> Self {
        self.read_only = enabled;
        self
    }
    pub fn feed(&mut self, bytes: &[u8]) {
        if let Some(driver) = &mut self.catalog {
            driver.feed(bytes);
            return;
        }
        if let Some(driver) = &mut self.app_server {
            driver.feed(bytes);
            return;
        }
        if self
            .claude_control
            .as_ref()
            .is_some_and(|driver| driver.is_catalog())
        {
            self.catalog_bytes = self.catalog_bytes.saturating_add(bytes.len());
            if self.catalog_bytes > 1024 * 1024 {
                self.record_error("Claude catalog protocol exceeds its byte budget".into());
                return;
            }
        }
        for &byte in bytes {
            if byte == b'\n' {
                if !self.discarding_line {
                    self.parse_line();
                }
                self.line.clear();
                self.discarding_line = false;
            } else if self.discarding_line {
                continue;
            } else if self.line.len() < MAX_PROTOCOL_LINE {
                self.line.push(byte);
            } else {
                self.record_error("provider JSONL line exceeds 64 KiB".into());
                self.line.clear();
                self.discarding_line = true;
            }
        }
    }
    fn record_error(&mut self, error: String) {
        // A later successful terminal must never erase a fatal protocol failure.
        if self.error.is_none() {
            self.error = Some(error);
        }
        if let Some(driver) = &mut self.claude_control {
            driver.abort();
        }
    }
    fn parse_line(&mut self) {
        if self.line.iter().all(u8::is_ascii_whitespace) {
            return;
        }
        if self
            .claude_control
            .as_ref()
            .is_some_and(|driver| driver.is_catalog())
        {
            self.catalog_messages += 1;
            if self.catalog_messages > 1024 {
                self.record_error("Claude catalog protocol exceeds its message budget".into());
                return;
            }
        }
        let event: Value = match serde_json::from_slice(&self.line) {
            Ok(value) => value,
            Err(_) => {
                self.record_error("malformed provider JSONL event".into());
                return;
            }
        };
        // Metadata/control envelopes are never task logs, even when malformed.
        // Require a recognized ordinary task frame after initialization and a
        // successful protocol parse, rather than trusting an absent type tag.
        let task_frame = self
            .claude_control
            .as_ref()
            .is_some_and(|driver| driver.initialized())
            && matches!(
                event.get("type").and_then(Value::as_str),
                Some(
                    "system"
                        | "assistant"
                        | "user"
                        | "result"
                        | "stream_event"
                        | "tool_progress"
                        | "tool_use_summary"
                )
            )
            && event.get("response").is_none()
            && event.get("request").is_none()
            && event.get("account").is_none();
        match self.event(&event) {
            Err(error) => self.record_error(error),
            Ok(()) if task_frame => {
                self.task_output.extend_from_slice(&self.line);
                self.task_output.push(b'\n');
            }
            Ok(()) => {}
        }
    }

    fn event(&mut self, event: &Value) -> Result<(), String> {
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .ok_or("provider event requires a string type")?;
        if let Some(driver) = &mut self.claude_control {
            if driver.is_catalog() {
                if kind == "control_response" && !self.catalog_input_dispatched {
                    return Err(
                        "Claude replied before the initialize request was dispatched".into(),
                    );
                }
                return driver.event(event);
            }
            if kind.starts_with("control_") {
                let result = driver.event(event);
                if result.is_ok() && driver.initialized() && self.task_catalog_time.is_none() {
                    self.task_catalog_time = Some(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis()
                            .min(u64::MAX as u128) as u64,
                    );
                }
                return result;
            }
            if !self.task_prompt_dispatched
                && matches!(kind, "assistant" | "user" | "result" | "stream_event")
            {
                return Err(
                    "Claude emitted a task event before the task prompt was dispatched".into(),
                );
            }
        }
        if kind.len() > MAX_FIELD {
            return Err("provider event type exceeds bound".into());
        }
        if kind == "result" {
            self.result.terminal_reason = string_field(event, "subtype", MAX_FIELD)?;
        }
        if self.result.provider == ProviderKind::CodexCli && kind == "turn.failed" {
            self.finish_terminal(kind)?;
            return Err("provider turn failed".into());
        }
        if self.result.provider == ProviderKind::CodexCli && kind == "error" {
            if self.result.terminal_reason.is_none() {
                self.result.terminal_reason = Some("provider_error".into());
            }
            return Err("provider reported an unrecoverable stream error".into());
        }
        if let Some(denials) = event.get("permission_denials")
            && !denials.as_array().is_some_and(Vec::is_empty)
        {
            return Err("provider reported permission denials".into());
        }
        if event.get("error").is_some_and(|value| !value.is_null())
            || event
                .get("is_error")
                .is_some_and(|value| value != &Value::Bool(false))
            || kind == "error"
            || kind == "permission_denied"
            || event.get("subtype").and_then(Value::as_str) == Some("permission_denied")
        {
            return Err("provider reported an error or denied permission".into());
        }
        match self.result.provider {
            ProviderKind::CodexAppServer => {
                return Err("app-server requires bidirectional protocol driver".into());
            }
            ProviderKind::CodexCli => match kind {
                "thread.started" => {
                    self.active()?;
                    self.result.session_id = string_field(event, "thread_id", MAX_FIELD)?;
                    // The exec thread.started contract has no effective model or
                    // effort. Unrecognized extra fields cannot verify either.
                }
                "turn.started" => self.active()?,
                "item.started" | "item.updated" | "item.completed" => {
                    self.active()?;
                    let item = event
                        .get("item")
                        .filter(|value| value.is_object())
                        .ok_or("provider item must be an object")?;
                    let item_type = string_field(item, "type", MAX_FIELD)?
                        .ok_or("provider item requires a string type")?;
                    // Codex ErrorItem and failed tool items are non-fatal. The
                    // agent may recover; only the turn terminal decides success.
                    // Keep validating the fields we interpret, and do not relax
                    // explicit permission denials or read-only tool restrictions.
                    if item_type == "error" {
                        item.get("message")
                            .and_then(Value::as_str)
                            .ok_or("provider error item requires a message")?;
                    }
                    if let Some(error) = item.get("error").filter(|value| !value.is_null()) {
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .ok_or("provider item error requires a message")?;
                    }
                    if string_field(item, "status", MAX_FIELD)?.as_deref() == Some("declined") {
                        return Err("provider item denied permission".into());
                    }
                    if self.read_only
                        && matches!(
                            item.get("type").and_then(Value::as_str),
                            Some("file_change" | "mcp_tool_call" | "collab_tool_call")
                        )
                    {
                        return Err(
                            "read-only provider attempted a mutating or external tool".into()
                        );
                    }
                    if kind == "item.completed"
                        && item.get("type").and_then(Value::as_str) == Some("agent_message")
                    {
                        self.set_summary(item, "text")?;
                        if let Some(model) = optional_string_field(item, "model", MAX_FIELD)? {
                            self.result
                                .observe_message_model(model, "codex.item/agent_message");
                        }
                    }
                }
                "turn.completed" => {
                    self.finish_terminal("turn.completed")?;
                    self.usage(event)?;
                }
                _ if kind.starts_with("turn.") || terminal_name(kind) => {
                    return Err("unrecognized provider terminal event".into());
                }
                _ => {}
            },
            ProviderKind::ClaudeCli => match kind {
                "system" => {
                    if event.get("subtype").and_then(Value::as_str) == Some("init")
                        && event.get("parent_tool_use_id").is_none_or(Value::is_null)
                    {
                        self.active()?;
                        self.result.session_id = string_field(event, "session_id", MAX_FIELD)?;
                        let settings = SessionSettings {
                            model: optional_string_field(event, "model", MAX_FIELD)?,
                            effort: optional_string_field(event, "effort", MAX_FIELD)?,
                            approval_policy: None,
                            sandbox: None,
                            permission_mode: optional_string_field(
                                event,
                                "permissionMode",
                                MAX_FIELD,
                            )?,
                            source: "claude.system/init".into(),
                        };
                        let mut permission_mismatch = false;
                        if let Some(selection) = &mut self.result.selection {
                            if settings.model.is_some() && selection.observed.model.is_none() {
                                selection.verification.model = "session_reported".into();
                            }
                            if settings.effort.is_some() {
                                selection.verification.effort = "session_reported".into();
                            }
                            // Restricted is Relay's reviewer contract, not a native
                            // permissionMode value. Missing evidence stays unknown.
                            if let Some(expected) = selection
                                .requested
                                .native_permission
                                .and_then(NativePermission::claude_mode)
                                && let Some(actual) = &settings.permission_mode
                            {
                                permission_mismatch = actual != expected;
                                selection.verification.permission = if permission_mismatch {
                                    "mismatch"
                                } else {
                                    "session_reported"
                                }
                                .into();
                            }
                            selection.session_settings = Some(settings);
                        }
                        if permission_mismatch {
                            return Err("native permission mismatch: Claude session reported a different permission mode".into());
                        }
                    }
                }
                "assistant" => {
                    self.active()?;
                    if let Some(message) = event.get("message") {
                        if !message.is_object() {
                            return Err("assistant message must be an object".into());
                        }
                        if self.read_only
                            && let Some(content) = message.get("content")
                        {
                            let content = content
                                .as_array()
                                .ok_or("assistant content must be an array")?;
                            for block in content {
                                if block.get("type").and_then(Value::as_str) == Some("tool_use")
                                    && !matches!(
                                        block.get("name").and_then(Value::as_str),
                                        Some("Read" | "Glob" | "Grep" | "EndConversation")
                                    )
                                {
                                    return Err(
                                        "read-only provider attempted a non-read tool".into()
                                    );
                                }
                            }
                        }
                        if event.get("parent_tool_use_id").is_none_or(Value::is_null)
                            && let Some(model) = optional_string_field(message, "model", MAX_FIELD)?
                        {
                            self.result
                                .observe_message_model(model, "claude.assistant.message");
                        }
                    }
                }
                "result" => {
                    let subtype = event
                        .get("subtype")
                        .and_then(Value::as_str)
                        .ok_or("provider result has no subtype")?;
                    if subtype != "success" || event.get("is_error") != Some(&Value::Bool(false)) {
                        self.result.terminal_reason = Some(bounded(subtype, MAX_FIELD));
                        return Err("provider result did not report success".into());
                    }
                    self.finish_terminal(subtype)?;
                    self.set_summary(event, "result")?;
                    if let Some(id) = string_field(event, "session_id", MAX_FIELD)? {
                        self.result.session_id = Some(id);
                    }
                    self.usage(event)?;
                }
                _ if terminal_name(kind) => {
                    return Err("unrecognized provider terminal event".into());
                }
                _ => {}
            },
        }
        Ok(())
    }
    fn set_summary(&mut self, value: &Value, key: &str) -> Result<(), String> {
        let text = value
            .get(key)
            .and_then(Value::as_str)
            .ok_or("provider final answer requires text")?;
        self.result.summary_truncated = text.len() > MAX_SUMMARY;
        self.result.summary = bounded(text, MAX_SUMMARY);
        self.answer_seen = true;
        Ok(())
    }
    fn active(&self) -> Result<(), String> {
        if self.terminal {
            Err("provider continued after its terminal event".into())
        } else {
            Ok(())
        }
    }
    fn finish_terminal(&mut self, reason: &str) -> Result<(), String> {
        self.active()?;
        self.terminal = true;
        self.result.terminal_reason = Some(bounded(reason, MAX_FIELD));
        Ok(())
    }
    fn usage(&mut self, event: &Value) -> Result<(), String> {
        if let Some(usage) = event.get("usage") {
            if !usage.is_object() {
                return Err("provider usage must be an object".into());
            }
            self.result.usage.input_tokens = number_field(usage, "input_tokens")?;
            self.result.usage.output_tokens = number_field(usage, "output_tokens")?;
            self.result.usage.cached_input_tokens = number_field(usage, "cached_input_tokens")?
                .or(number_field(usage, "cache_read_input_tokens")?);
            self.result.usage.reasoning_output_tokens =
                number_field(usage, "reasoning_output_tokens")?;
            self.result.usage.cache_creation_input_tokens =
                number_field(usage, "cache_creation_input_tokens")?;
        }
        self.result.usage.num_turns = number_field(event, "num_turns")?;
        if let Some(cost) = event.get("total_cost_usd") {
            let cost = cost
                .as_f64()
                .filter(|v| v.is_finite() && *v >= 0.0 && *v <= 1e12)
                .ok_or("invalid provider usage cost")?;
            self.result.usage.total_cost_usd = Some(cost);
        }
        Ok(())
    }
    pub fn finish(mut self) -> (ProviderResult, Option<String>) {
        if let Some(driver) = self.catalog.take() {
            return (self.result, driver.finish().err());
        }
        if let Some(driver) = self.app_server.take() {
            return driver.finish();
        }
        if !self.discarding_line
            && !self.line.is_empty()
            && !self
                .claude_control
                .as_ref()
                .is_some_and(|driver| driver.is_catalog())
        {
            self.parse_line();
        }
        if self
            .claude_control
            .as_ref()
            .is_some_and(|driver| driver.is_catalog())
        {
            if !self.catalog_input_dispatched {
                self.record_error("Claude exited before initialize was dispatched".into());
            }
            // Catalog framing requires newline-terminated responses. Unlike task
            // output, do not reinterpret an incomplete metadata tail as evidence.
            if !self.line.iter().all(u8::is_ascii_whitespace) {
                self.record_error("Claude catalog ended with an incomplete JSONL message".into());
            }
            let result = self
                .claude_control
                .take()
                .expect("catalog driver")
                .finish_catalog();
            return (self.result, self.error.or_else(|| result.err()));
        }
        if self.error.is_none() && self.claude_control.is_some() && !self.task_prompt_dispatched {
            self.error = Some("Claude exited before the task prompt was dispatched".into());
        }
        if self.error.is_none() && !self.terminal {
            self.error =
                Some("provider stream ended without a recognized successful terminal event".into());
        }
        if self.error.is_none() && !self.answer_seen {
            self.error = Some("provider stream has no final answer".into());
        }
        if self.error.is_some() && self.result.terminal_reason.is_none() {
            self.result.terminal_reason = Some("protocol_error".into());
        }
        (self.result, self.error)
    }
}
fn terminal_name(name: &str) -> bool {
    [
        "result",
        "terminal",
        "completed",
        "finished",
        "failed",
        "error",
    ]
    .iter()
    .any(|part| name.split(['.', '_']).any(|word| word == *part))
}
fn string_field(value: &Value, key: &str, limit: usize) -> Result<Option<String>, String> {
    value
        .get(key)
        .map(|value| {
            value
                .as_str()
                .filter(|text| text.len() <= limit)
                .map(str::to_owned)
                .ok_or_else(|| format!("provider {key} must be a bounded string"))
        })
        .transpose()
}
pub(crate) fn optional_string_field(
    value: &Value,
    key: &str,
    limit: usize,
) -> Result<Option<String>, String> {
    if value.get(key).is_none_or(Value::is_null) {
        Ok(None)
    } else {
        string_field(value, key, limit)
    }
}
fn number_field(value: &Value, key: &str) -> Result<Option<u64>, String> {
    value
        .get(key)
        .map(|value| {
            value
                .as_u64()
                .ok_or_else(|| format!("provider {key} must be an unsigned integer"))
        })
        .transpose()
}
fn bounded(text: &str, limit: usize) -> String {
    let mut text = text.to_owned();
    truncate(&mut text, limit);
    text
}
fn truncate(text: &mut String, limit: usize) {
    let mut end = limit.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn selection_evidence_is_utf8_bounded_and_shrinks_without_claiming_full_evidence() {
        let profile: NativeProfile = serde_json::from_value(json!({
            "provider":"claude_cli","program":"/bin/true","native_permission":"claude_auto"
        }))
        .unwrap();
        let mut result = ProviderResult::new(&profile, None);
        let evidence = result.selection.as_mut().unwrap();
        evidence.requested.profile = Some("界".repeat(256));
        evidence.session_settings = Some(SessionSettings {
            model: Some("界".repeat(256)),
            effort: Some("x".repeat(500)),
            approval_policy: None,
            sandbox: None,
            permission_mode: Some("auto".into()),
            source: "claude.system/init".into(),
        });
        evidence.observed.reroutes = (0..10)
            .map(|_| ModelReroute {
                thread_id: "thread".into(),
                turn_id: "turn".into(),
                from_model: "界".repeat(256),
                to_model: "x".repeat(500),
                reason: "reason".repeat(100),
            })
            .collect();
        result.bound();
        let evidence = result.selection.as_ref().unwrap();
        assert!(evidence.truncated);
        assert!(evidence.requested.profile.as_ref().unwrap().len() <= 256);
        assert_eq!(evidence.observed.reroutes.len(), 8);
        for entry in &evidence.observed.reroutes {
            assert!(entry.from_model.len() <= 256);
            assert_eq!(entry.to_model.len(), 256);
            assert_eq!(entry.reason.len(), 256);
        }
        let before = serde_json::to_vec(&result).unwrap().len();
        let mut shrinks = 0;
        while result.shrink() {
            shrinks += 1;
            assert!(shrinks < 32);
        }
        assert!(shrinks > 0);
        assert!(serde_json::to_vec(&result).unwrap().len() < before);
        let evidence = result.selection.unwrap();
        assert!(evidence.truncated);
        assert!(evidence.observed.reroutes.is_empty());
        assert_eq!(
            evidence.requested.native_permission,
            Some(NativePermission::ClaudeAuto)
        );
        assert_eq!(evidence.verification.permission, "unknown");
    }

    #[test]
    fn catalog_requires_actual_initialize_dispatch_and_complete_framing() {
        let profile: NativeProfile = serde_json::from_value(
            serde_json::json!({"provider":"claude_cli","program":"/bin/true"}),
        )
        .unwrap();
        let response = serde_json::json!({"type":"control_response","response":{"subtype":"success","request_id":"relay-initialize-1","pending_permission_requests":[],"pending_user_dialog_requests":[],"response":{"models":[]}}}).to_string()+"\n";
        let parser =
            || ProtocolParser::catalog(ProviderResult::new(&profile, Some("2.1.291".into())));
        let mut early = parser();
        assert!(!early.pending().is_empty());
        early.feed(response.as_bytes());
        assert!(early.failure().unwrap().contains("dispatched"));
        assert!(early.catalog_result().is_none());
        assert!(early.finish().1.is_some());
        let mut valid = parser();
        valid.pending();
        valid.mark_catalog_input_complete();
        valid.feed(response.as_bytes());
        assert!(valid.stopped());
        assert_eq!(valid.catalog_result().unwrap().len(), 0);
        assert!(valid.finish().1.is_none());
        let mut partial = parser();
        partial.pending();
        partial.mark_catalog_input_complete();
        partial.feed(response.as_bytes());
        partial.feed(b"{");
        assert!(partial.catalog_result().is_none());
        partial.task_output_eof();
        assert!(partial.finish().1.is_some());
    }

    #[test]
    fn native_permission_mismatch_is_exposed_promptly_and_errors_are_sticky() {
        let profile: NativeProfile = serde_json::from_value(json!({
            "provider":"claude_cli","program":"/bin/true","native_permission":"claude_auto"
        }))
        .unwrap();
        let mut parser = ProtocolParser::new(ProviderResult::new(&profile, None));
        parser.feed(b"{\"type\":\"system\",\"subtype\":\"init\",\"permissionMode\":\"bypassPermissions\"}\n");
        assert!(parser.stopped());
        assert!(
            parser
                .failure()
                .unwrap()
                .contains("native permission mismatch")
        );
        parser.feed(b"{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\"}\n");
        let (result, error) = parser.finish();
        assert!(error.unwrap().contains("native permission mismatch"));
        assert_eq!(
            result.selection.unwrap().verification.permission,
            "mismatch"
        );
    }
}
