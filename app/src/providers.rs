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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    CodexCli,
    ClaudeCli,
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
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub max_budget_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderProbe {
    pub provider: ProviderKind,
    pub cli_version: String,
    pub read_only_supported: bool,
}

impl NativeProfile {
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
        if self.model.as_ref().is_some_and(|model| {
            model.is_empty()
                || model.starts_with('-')
                || model.len() > MAX_FIELD
                || model.chars().any(char::is_control)
        }) {
            return Err("native model must contain 1–256 bytes without control characters".into());
        }
        if let Some(effort) = &self.effort {
            let allowed = match self.provider {
                ProviderKind::CodexCli => &["minimal", "low", "medium", "high", "xhigh"][..],
                ProviderKind::ClaudeCli => &["low", "medium", "high", "xhigh", "max"][..],
            };
            if !allowed.contains(&effort.as_str()) {
                return Err("unsupported native effort setting".into());
            }
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
        if read_only && self.provider == ProviderKind::CodexCli {
            return Err("review_profile_unsupported: Codex project MCP/hooks cannot be disabled by the supported CLI contract".into());
        }
        let mut args: Vec<String> = match self.provider {
            ProviderKind::CodexCli => vec![
                "exec".into(),
                "--json".into(),
                "--ephemeral".into(),
                "--sandbox".into(),
                if read_only {
                    "read-only"
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
        if let Some(model) = &self.model {
            args.extend(["--model".into(), model.clone()]);
        }
        if let Some(effort) = &self.effort {
            match self.provider {
                ProviderKind::CodexCli => {
                    args.extend(["-c".into(), format!("model_reasoning_effort=\"{effort}\"")])
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
        if read_only && self.provider == ProviderKind::CodexCli {
            return Err("review_profile_unsupported: Codex project MCP/hooks cannot be disabled by the supported CLI contract".into());
        }
        let version = parse_version(version).ok_or("CLI version was not recognizable")?;
        if self.provider == ProviderKind::ClaudeCli && version.0 < (2, 1, 259) {
            return Err(
                "Claude CLI 2.1.259 or later is required for --permission-prompts none".into(),
            );
        }
        let mut required = match self.provider {
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderUsage {
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub total_cost_usd: Option<f64>,
    pub num_turns: Option<u64>,
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
        }
    }
    pub fn read_only(mut self, enabled: bool) -> Self {
        self.read_only = enabled;
        self
    }
    pub fn feed(&mut self, bytes: &[u8]) {
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
    }
    fn parse_line(&mut self) {
        if self.line.iter().all(u8::is_ascii_whitespace) {
            return;
        }
        let event: Value = match serde_json::from_slice(&self.line) {
            Ok(value) => value,
            Err(_) => {
                self.record_error("malformed provider JSONL event".into());
                return;
            }
        };
        if let Err(error) = self.event(&event) {
            self.record_error(error);
        }
    }
    fn event(&mut self, event: &Value) -> Result<(), String> {
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .ok_or("provider event requires a string type")?;
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
            ProviderKind::CodexCli => match kind {
                "thread.started" => {
                    self.active()?;
                    self.result.session_id = string_field(event, "thread_id", MAX_FIELD)?;
                    self.result.reported_model = string_field(event, "model", MAX_FIELD)?;
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
                    if event.get("subtype").and_then(Value::as_str) == Some("init") {
                        self.active()?;
                        self.result.session_id = string_field(event, "session_id", MAX_FIELD)?;
                        self.result.reported_model = string_field(event, "model", MAX_FIELD)?;
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
                        if let Some(model) = string_field(message, "model", MAX_FIELD)? {
                            self.result.reported_model = Some(model);
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
        if !self.discarding_line && !self.line.is_empty() {
            self.parse_line();
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
