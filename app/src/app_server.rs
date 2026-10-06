//! Bounded Codex app-server stdio client. The supervisor owns the process tree;
//! this driver owns only request correlation and one explicitly identified turn.
use crate::providers::{
    MAX_PROTOCOL_LINE, MAX_SESSION_POLICY, ModelReroute, NativeApprovalReview, NativePermission,
    ProviderKind, ProviderResult, ProviderUsage, SessionSettings, TokenCounts,
    optional_string_field,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;

const MAX_PENDING: usize = 256 * 1024;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Start {
    pub cwd: PathBuf,
    pub prompt: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    #[serde(default)]
    pub native_permission: Option<NativePermission>,
    pub resume: Option<String>,
    pub checkpoint: Option<crate::sessions::Checkpoint>,
}

pub(crate) struct Driver {
    result: ProviderResult,
    start: Start,
    line: Vec<u8>,
    pending: Vec<u8>,
    request: u64,
    turn: Option<String>,
    resume_usage: Vec<(String, UsageSnapshot)>,
    baseline: Option<TokenCounts>,
    latest_total: Option<TokenCounts>,
    totals_invalid: bool,
    terminal: bool,
    answer_seen: bool,
    error: Option<String>,
}
impl Driver {
    pub fn new(result: ProviderResult, start: Start) -> Self {
        let mut driver = Self {
            result,
            start,
            line: Vec::new(),
            pending: Vec::new(),
            request: 1,
            turn: None,
            resume_usage: Vec::new(),
            baseline: None,
            latest_total: None,
            totals_invalid: false,
            terminal: false,
            answer_seen: false,
            error: None,
        };
        driver.result.usage.usage_scope = Some("last_snapshot".into());
        let native_review =
            driver.start.native_permission == Some(NativePermission::CodexNativeSandboxedReview);
        if driver
            .start
            .native_permission
            .is_some_and(|mode| !mode.compatible(ProviderKind::CodexAppServer, native_review))
        {
            driver.fail("native permission mode is incompatible with Codex app-server");
            return driver;
        }
        if native_review && (driver.start.resume.is_some() || driver.start.checkpoint.is_some()) {
            driver.fail("Codex native sandboxed review supports fresh sessions only; resume/checkpoint is unsupported");
            return driver;
        }
        if driver.start.resume.is_none() {
            driver.baseline = Some(TokenCounts {
                input_tokens: Some(0),
                cached_input_tokens: Some(0),
                output_tokens: Some(0),
                reasoning_output_tokens: Some(0),
            });
        }
        driver.send(json!({"id":1,"method":"initialize","params":{
            "clientInfo":{"name":"relay","version":env!("CARGO_PKG_VERSION")},
            "capabilities":{"experimentalApi":false}
        }}));
        driver
    }
    fn auto_review(&self) -> bool {
        self.start.native_permission == Some(NativePermission::CodexAutoReview)
    }
    fn approval_policy(&self) -> &'static str {
        if self.auto_review() {
            "on-request"
        } else {
            "never"
        }
    }
    fn send(&mut self, message: Value) {
        let bytes = serde_json::to_vec(&message).expect("JSON value");
        if self.pending.len() + bytes.len() + 1 > MAX_PENDING {
            self.fail("app-server outbound protocol budget exceeded");
        } else {
            self.pending.extend(bytes);
            self.pending.push(b'\n');
        }
    }
    pub fn take_pending(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
    pub fn stopped(&self) -> bool {
        self.terminal || self.error.is_some()
    }
    pub fn failure(&self) -> Option<&str> {
        self.error.as_deref()
    }
    fn fail(&mut self, text: &str) {
        if self.error.is_none() {
            self.error = Some(text.to_owned());
            // A queued turn can share a drain with a later policy failure.
            self.pending.clear();
        }
    }
    fn bind_turn(&mut self, id: String) {
        if self.turn.is_some() {
            return;
        }
        let pending = std::mem::take(&mut self.resume_usage);
        // Only historical snapshots observed in the cold-resume window can
        // establish the pre-turn baseline. A live snapshot is never a baseline.
        let mut historical_high_water = TokenCounts::default();
        for (_, snapshot) in pending.iter().filter(|(turn, _)| turn != &id) {
            if let Some(total) = &snapshot.total {
                if total.decreased_from(&historical_high_water) {
                    self.totals_invalid = true;
                }
                historical_high_water = total.with_missing_from(&historical_high_water);
                self.baseline = Some(total.clone());
            } else {
                // A newer historical update without totals cannot certify a baseline.
                self.baseline = None;
            }
        }
        if self.start.resume.is_some() {
            self.latest_total = Some(historical_high_water);
        }
        self.turn = Some(id.clone());
        for (_, snapshot) in pending.into_iter().filter(|(turn, _)| turn == &id) {
            self.apply_usage(snapshot);
        }
    }
    fn apply_usage(&mut self, snapshot: UsageSnapshot) {
        if let Some(total) = &snapshot.total {
            if self
                .latest_total
                .as_ref()
                .or(self.baseline.as_ref())
                .is_some_and(|old| total.decreased_from(old))
            {
                // Counter resets and reordered older snapshots cannot be
                // distinguished reliably. Keep totals unknown for this run.
                self.totals_invalid = true;
            }
            let previous = self
                .latest_total
                .take()
                .or_else(|| self.baseline.clone())
                .unwrap_or_default();
            self.latest_total = Some(total.with_missing_from(&previous));
        }
        self.result.usage = snapshot.last;
        self.result.usage.usage_scope = Some("last_snapshot".into());
        if !self.totals_invalid {
            self.result.usage.turn_total = snapshot
                .total
                .as_ref()
                .zip(self.baseline.as_ref())
                .map(|(total, base)| total.delta(base));
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.error.is_some() {
                break;
            }
            if byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                match serde_json::from_slice::<Value>(&line) {
                    Ok(event) => {
                        if let Err(error) = self.event(&event) {
                            self.fail(&error);
                        }
                    }
                    Err(_) => self.fail("malformed app-server JSONL message"),
                }
            } else if self.line.len() >= MAX_PROTOCOL_LINE {
                self.fail("app-server JSONL line exceeds 64 KiB");
            } else {
                self.line.push(byte);
            }
        }
    }
    fn event(&mut self, event: &Value) -> Result<(), String> {
        let method = event.get("method").and_then(Value::as_str);
        if let Some(id) = event.get("id") {
            if method.is_some() {
                // Never grant new permissions, supply credentials/user answers, or
                // dispatch dynamic tools. Stop and close stdin without flushing any queued prompt.
                return Err(
                    "app-server requested approval, input, or unsupported client action".into(),
                );
            }
            if id.as_u64() != Some(self.request) || self.request > 3 {
                return Err("unexpected app-server response ID".into());
            }
            if event.get("error").is_some_and(|error| !error.is_null()) {
                return Err("app-server request failed; no automatic replay".into());
            }
            let result = event
                .get("result")
                .filter(|v| v.is_object())
                .ok_or("app-server response requires result")?;
            match self.request {
                1 => {
                    self.send(json!({"method":"initialized","params":{}}));
                    let native_review = self.start.native_permission
                        == Some(NativePermission::CodexNativeSandboxedReview);
                    let sandbox = if native_review {
                        "read-only"
                    } else if self.start.native_permission
                        == Some(NativePermission::CodexFullAccess)
                    {
                        "danger-full-access"
                    } else {
                        "workspace-write"
                    };
                    let mut params = json!({"cwd":self.start.cwd,"approvalPolicy":self.approval_policy(),"sandbox":sandbox,"model":self.start.model});
                    if self.auto_review() {
                        params["approvalsReviewer"] = json!("auto_review");
                        params["config"] = json!({
                            "sandbox_workspace_write.network_access": false,
                            "sandbox_workspace_write.writable_roots": [self.start.cwd],
                            "sandbox_workspace_write.exclude_tmpdir_env_var": false,
                            "sandbox_workspace_write.exclude_slash_tmp": false
                        });
                    }
                    if native_review {
                        params["ephemeral"] = json!(true);
                    }
                    let method = if let Some(id) = &self.start.resume {
                        valid_id(id)?;
                        params["threadId"] = json!(id);
                        params["excludeTurns"] = json!(true);
                        "thread/resume"
                    } else {
                        "thread/start"
                    };
                    self.send(json!({"id":2,"method":method,"params":params}));
                }
                2 => {
                    let id = id_field(&result["thread"], "id")?;
                    if self.start.native_permission
                        == Some(NativePermission::CodexNativeSandboxedReview)
                        && result["thread"]["ephemeral"] != true
                    {
                        return Err(
                            "native reviewer did not confirm a fresh ephemeral thread".into()
                        );
                    }
                    if self
                        .start
                        .resume
                        .as_ref()
                        .is_some_and(|expected| expected != &id)
                    {
                        return Err("app-server resumed a different thread".into());
                    }
                    if let Some(checkpoint) = &self.start.checkpoint {
                        checkpoint
                            .started(&id)
                            .map_err(|e| format!("cannot persist started thread: {e}"))?;
                    }
                    self.result.session_id = Some(id.clone());
                    self.session_settings(
                        result,
                        if self.start.resume.is_some() {
                            "codex.thread/resume"
                        } else {
                            "codex.thread/start"
                        },
                    )?;
                    let sandbox_policy = if self.start.native_permission
                        == Some(NativePermission::CodexNativeSandboxedReview)
                    {
                        json!({"type":"readOnly","networkAccess":false})
                    } else if self.start.native_permission
                        == Some(NativePermission::CodexFullAccess)
                    {
                        json!({"type":"dangerFullAccess"})
                    } else {
                        json!({"type":"workspaceWrite","writableRoots":[self.start.cwd],
                            "networkAccess":false,"excludeTmpdirEnvVar":false,"excludeSlashTmp":false})
                    };
                    let mut params = json!({
                        "threadId":id,"input":[{"type":"text","text":self.start.prompt}],
                        "cwd":self.start.cwd,"approvalPolicy":self.approval_policy(),"model":self.start.model,
                        "effort":self.start.effort,
                        "sandboxPolicy":sandbox_policy
                    });
                    if self.auto_review() {
                        params["approvalsReviewer"] = json!("auto_review");
                    }
                    self.send(json!({"id":3,"method":"turn/start","params":params}));
                }
                3 => {
                    let id = id_field(&result["turn"], "id")?;
                    if self.turn.as_ref().is_some_and(|expected| expected != &id) {
                        return Err("app-server turn response changed the active turn".into());
                    }
                    self.bind_turn(id);
                }
                _ => unreachable!(),
            }
            self.request += 1;
            return Ok(());
        }
        let method = method.ok_or("app-server notification requires a method")?;
        if method.len() > 256 {
            return Err("app-server method exceeds its bound".into());
        }
        let params = &event["params"];
        if method == "thread/settings/updated" && self.auto_review() {
            // Early startup notifications cannot establish a session. The correlated
            // start/resume response must still report the complete requested mode.
            if self.result.session_id.is_none() {
                return Ok(());
            }
            if params.get("threadId").and_then(Value::as_str) != self.result.session_id.as_deref()
                || self.terminal
            {
                return Err(
                    "app-server settings update belongs to another or completed thread".into(),
                );
            }
            let settings = &params["threadSettings"];
            let normalized = json!({"cwd":settings.get("cwd"), "model":settings.get("model"), "reasoningEffort":settings.get("effort"),
                "approvalPolicy":settings.get("approvalPolicy"), "approvalsReviewer":settings.get("approvalsReviewer"),
                "sandbox":settings.get("sandboxPolicy")});
            self.session_settings(&normalized, "codex.thread/settings/updated")?;
            return Ok(());
        }
        if method == "thread/settings/updated"
            && self.start.native_permission == Some(NativePermission::CodexNativeSandboxedReview)
        {
            if self.terminal
                || self.result.session_id.is_none()
                || params["threadId"].as_str() != self.result.session_id.as_deref()
            {
                return Err("native reviewer settings update is outside the active thread".into());
            }
            let settings = &params["threadSettings"];
            if settings["approvalPolicy"] != "never"
                || settings["sandboxPolicy"]["type"] != "readOnly"
                || settings["sandboxPolicy"]["networkAccess"] != false
                || settings["cwd"].as_str() != self.start.cwd.to_str()
            {
                if let Some(selection) = &mut self.result.selection {
                    selection.verification.permission = "mismatch".into();
                }
                return Err("native reviewer session settings changed or omitted the required local policy/candidate directory".into());
            }
            return Ok(());
        }
        if matches!(
            method,
            "turn/started"
                | "turn/completed"
                | "turn/failed"
                | "turn/cancelled"
                | "item/completed"
                | "error"
                | "thread/tokenUsage/updated"
                | "turn/diff/updated"
                | "turn/plan/updated"
                | "turn/moderationMetadata"
                | "model/rerouted"
                | "item/autoApprovalReview/completed"
        ) {
            if self.result.session_id.is_none()
                || params.get("threadId").and_then(Value::as_str)
                    != self.result.session_id.as_deref()
            {
                return Err("app-server notification belongs to another thread".into());
            }
            if self.terminal {
                return Err("app-server continued after terminal event".into());
            }
            let turn = if matches!(
                method,
                "turn/started" | "turn/completed" | "turn/failed" | "turn/cancelled"
            ) {
                id_field(&params["turn"], "id")?
            } else {
                id_field(params, "turnId")?
            };
            if method == "thread/tokenUsage/updated"
                && self.start.resume.is_some()
                && self.request == 3
                && self.turn.is_none()
            {
                // Buffer bounded snapshots until the active turn is identified.
                // Historical totals establish a baseline, never this run's usage.
                if self.resume_usage.len() >= 32 {
                    return Err("app-server pending usage budget exceeded".into());
                }
                self.resume_usage.push((turn, token_usage(params)?));
                return Ok(());
            }
            if self.turn.is_none() && method == "turn/started" && self.request == 3 {
                self.bind_turn(turn.clone());
            }
            if self.turn.as_ref() != Some(&turn) {
                return Err("app-server notification belongs to another turn".into());
            }
        }
        match method {
            "item/autoApprovalReview/completed" => {
                // Observe native decisions only. Relay neither replaces the reviewer
                // nor authorizes a retry when native policy denies an action.
                let review = NativeApprovalReview {
                    thread_id: id_field(params, "threadId")?,
                    turn_id: id_field(params, "turnId")?,
                    review_id: id_field(params, "reviewId")?,
                    status: optional_string_field(&params["review"], "status", 256)?
                        .ok_or("app-server approval review requires status")?,
                    action_type: optional_string_field(&params["action"], "type", 256)?,
                    rationale: optional_string_field(
                        &params["review"],
                        "rationale",
                        MAX_PROTOCOL_LINE,
                    )?,
                    source: "codex.item/autoApprovalReview/completed".into(),
                };
                if let Some(selection) = &mut self.result.selection {
                    if selection.observed.native_approval_reviews.len() == 8 {
                        selection.observed.native_approval_reviews.remove(0);
                        selection.truncated = true;
                    }
                    selection.observed.native_approval_reviews.push(review);
                    selection.bound();
                }
            }
            "turn/completed" => {
                let status = params["turn"]["status"]
                    .as_str()
                    .ok_or("missing app-server terminal status")?;
                self.result.terminal_reason = Some(status.to_owned());
                self.terminal = true;
                if status != "completed"
                    || params["turn"].get("error").is_some_and(|e| !e.is_null())
                {
                    return Err("app-server turn failed or was interrupted".into());
                }
            }
            "turn/failed" | "turn/cancelled" => {
                return Err("app-server turn failed or was interrupted".into());
            }
            "error" => {
                if params.get("willRetry") != Some(&Value::Bool(true)) {
                    return Err("app-server reported an unrecoverable turn error".into());
                }
            }
            "item/completed" => {
                let item = &params["item"];
                if item["type"] == "agentMessage"
                    && item
                        .get("phase")
                        .is_none_or(|phase| phase.is_null() || phase == "final_answer")
                {
                    let text = item["text"]
                        .as_str()
                        .ok_or("app-server agent message requires text")?;
                    self.result.summary = text.to_owned();
                    self.result.bound();
                    self.answer_seen = true;
                }
                if item["type"] == "agentMessage"
                    && let Some(model) = optional_string_field(item, "model", 256)?
                {
                    self.result
                        .observe_message_model(model, "codex.item/agentMessage");
                }
                if item["status"] == "declined" {
                    return Err("app-server tool permission denied".into());
                }
            }
            "thread/tokenUsage/updated" => {
                self.apply_usage(token_usage(params)?);
            }
            "model/rerouted" => {
                let reroute = ModelReroute {
                    thread_id: id_field(params, "threadId")?,
                    turn_id: id_field(params, "turnId")?,
                    from_model: optional_string_field(params, "fromModel", 256)?
                        .ok_or("app-server reroute requires fromModel")?,
                    to_model: optional_string_field(params, "toModel", 256)?
                        .ok_or("app-server reroute requires toModel")?,
                    reason: optional_string_field(params, "reason", 256)?
                        .ok_or("app-server reroute requires reason")?,
                };
                if let Some(selection) = &mut self.result.selection {
                    selection.observed.model = Some(reroute.to_model.clone());
                    selection.observed.source = Some("codex.model/rerouted".into());
                    selection.verification.model = "rerouted".into();
                    if selection.observed.reroutes.len() == 8 {
                        selection.observed.reroutes.remove(0);
                        selection.truncated = true;
                    }
                    selection.observed.reroutes.push(reroute);
                }
            }
            "turn/started"
            | "turn/diff/updated"
            | "turn/plan/updated"
            | "turn/moderationMetadata" => {}
            _ if method.starts_with("turn/") => {
                return Err("unknown app-server turn notification".into());
            }
            _ => {} // Ignore bounded ancillary notifications; never infer success from them.
        }
        Ok(())
    }
    fn session_settings(&mut self, result: &Value, source: &str) -> Result<(), String> {
        let (approval_policy, approval_truncated) = reported_setting(result, "approvalPolicy")?;
        let (sandbox, sandbox_truncated) = reported_setting(result, "sandbox")?;
        let settings = SessionSettings {
            cwd: optional_string_field(result, "cwd", MAX_SESSION_POLICY)?,
            model: optional_string_field(result, "model", 256)?,
            effort: optional_string_field(result, "reasoningEffort", 256)?,
            approval_policy,
            approvals_reviewer: optional_string_field(result, "approvalsReviewer", 256)?,
            sandbox,
            permission_mode: None,
            source: source.into(),
        };
        let actual_sandbox = result.get("sandbox").filter(|value| !value.is_null());
        let sandbox_kind = actual_sandbox.and_then(|value| {
            value
                .as_str()
                .or_else(|| value.get("type").and_then(Value::as_str))
        });
        let actual_approval = result
            .get("approvalPolicy")
            .filter(|value| !value.is_null());
        let expected_sandbox = match self.start.native_permission {
            Some(NativePermission::CodexNativeSandboxedReview) => Some(["read-only", "readOnly"]),
            Some(NativePermission::CodexWorkspaceWrite | NativePermission::CodexAutoReview) => {
                Some(["workspace-write", "workspaceWrite"])
            }
            Some(NativePermission::CodexFullAccess) => {
                Some(["danger-full-access", "dangerFullAccess"])
            }
            _ => None,
        };
        let native_review =
            self.start.native_permission == Some(NativePermission::CodexNativeSandboxedReview);
        // This tier must observe the complete local policy before sending any
        // task prompt. A request or a missing/partial snapshot is not evidence.
        let native_review_unverified = native_review
            && (actual_approval != Some(&json!("never"))
                || actual_sandbox.is_none_or(|policy| {
                    policy["type"] != "readOnly" || policy["networkAccess"] != false
                })
                || approval_truncated
                || sandbox_truncated);
        let (baseline_mismatch, baseline_missing) = if self.auto_review() {
            auto_review_baseline(result, &self.start.cwd)
        } else {
            (false, false)
        };
        let permission_mismatch = native_review_unverified
            || baseline_mismatch
            || expected_sandbox.is_some_and(|expected| {
                (!self.auto_review()
                    && actual_sandbox.is_some()
                    && sandbox_kind.is_none_or(|actual| !expected.contains(&actual)))
                    || actual_approval
                        .is_some_and(|actual| actual.as_str() != Some(self.approval_policy()))
            });
        let reviewer_mismatch = self.auto_review()
            && settings
                .approvals_reviewer
                .as_deref()
                .is_some_and(|value| value != "auto_review");
        let missing_auto_settings = self.auto_review()
            && (settings.approvals_reviewer.is_none()
                || settings.approval_policy.is_none()
                || settings.sandbox.is_none()
                || baseline_missing);
        if let Some(selection) = &mut self.result.selection {
            selection.truncated |= approval_truncated || sandbox_truncated;
            if settings.model.is_some() && selection.observed.model.is_none() {
                selection.verification.model = "session_reported".into();
            }
            // Thread config predates the requested turn effort override. Neither
            // this response nor turn/start certifies the current turn's effort.
            if permission_mismatch || reviewer_mismatch {
                selection.verification.permission = "mismatch".into();
            } else if missing_auto_settings {
                selection.verification.permission = "unknown".into();
            } else if expected_sandbox.is_some()
                && settings.sandbox.is_some()
                && settings.approval_policy.is_some()
            {
                selection.verification.permission = "session_reported".into();
            }
            selection.session_settings = Some(settings);
        }
        if permission_mismatch || reviewer_mismatch {
            return Err("native permission mismatch: Codex session reported a different workspace baseline, sandbox, approval policy, or approvals reviewer".into());
        }
        if missing_auto_settings {
            return Err("native Auto-review unavailable: Codex did not report the complete workspace baseline, approval policy, and approvals reviewer; requested mode is unverified".into());
        }
        Ok(())
    }
    pub fn finish(mut self) -> (ProviderResult, Option<String>) {
        if !self.line.iter().all(u8::is_ascii_whitespace) {
            self.fail("incomplete app-server JSONL message");
        }
        if !self.terminal || self.request != 4 {
            self.fail("app-server ended without a correlated completed turn");
        }
        if !self.answer_seen {
            self.fail("app-server turn contained no final answer");
        }
        (self.result, self.error)
    }
}
/// Validate the concrete native baseline. workspaceWrite implicitly includes cwd;
/// an empty writableRoots array is therefore equivalent to listing cwd only.
/// Missing evidence is unknown, never an assumption about native defaults.
fn auto_review_baseline(result: &Value, cwd: &std::path::Path) -> (bool, bool) {
    let mut mismatch = false;
    let mut missing = false;
    match result.get("cwd").filter(|value| !value.is_null()) {
        Some(value) => mismatch |= value.as_str() != cwd.to_str(),
        None => missing = true,
    }
    let Some(policy) = result.get("sandbox").filter(|value| !value.is_null()) else {
        return (mismatch, true);
    };
    if !policy.is_object() {
        return (true, missing);
    }
    match policy.get("type").filter(|value| !value.is_null()) {
        Some(value) => mismatch |= value.as_str() != Some("workspaceWrite"),
        None => missing = true,
    }
    for key in ["networkAccess", "excludeTmpdirEnvVar", "excludeSlashTmp"] {
        match policy.get(key).filter(|value| !value.is_null()) {
            Some(value) => mismatch |= value.as_bool() != Some(false),
            None => missing = true,
        }
    }
    match policy.get("writableRoots").filter(|value| !value.is_null()) {
        Some(value) => {
            mismatch |= value
                .as_array()
                .is_none_or(|roots| roots.iter().any(|root| root.as_str() != cwd.to_str()))
        }
        None => missing = true,
    }
    (mismatch, missing)
}

fn reported_setting(value: &Value, key: &str) -> Result<(Option<String>, bool), String> {
    match value.get(key) {
        None | Some(Value::Null) => Ok((None, false)),
        Some(Value::String(_)) => optional_string_field(value, key, 256).map(|text| (text, false)),
        Some(setting) if setting.is_object() => {
            // Preserve returned policy details independently from the native kind
            // used for permission comparison. These settings describe the thread,
            // not effective turn execution or enforced access.
            let mut text = serde_json::to_string(setting).map_err(|error| error.to_string())?;
            let truncated = text.len() > MAX_SESSION_POLICY;
            let mut end = text.len().min(MAX_SESSION_POLICY);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            Ok((Some(text), truncated))
        }
        _ => Err(format!("invalid app-server {key} setting")),
    }
}
fn valid_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 256
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        Err("invalid bounded provider session/turn ID".into())
    } else {
        Ok(())
    }
}
fn id_field(value: &Value, name: &str) -> Result<String, String> {
    let id = value
        .get(name)
        .and_then(Value::as_str)
        .ok_or("missing provider session/turn ID")?;
    valid_id(id)?;
    Ok(id.to_owned())
}
fn number(value: &Value, key: &str) -> Result<Option<u64>, String> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| "invalid app-server usage count".into()),
    }
}
struct UsageSnapshot {
    last: ProviderUsage,
    total: Option<TokenCounts>,
}
impl TokenCounts {
    fn values(&self) -> [Option<u64>; 4] {
        [
            self.input_tokens,
            self.cached_input_tokens,
            self.output_tokens,
            self.reasoning_output_tokens,
        ]
    }
    fn with_missing_from(&self, previous: &Self) -> Self {
        Self {
            input_tokens: self.input_tokens.or(previous.input_tokens),
            cached_input_tokens: self.cached_input_tokens.or(previous.cached_input_tokens),
            output_tokens: self.output_tokens.or(previous.output_tokens),
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .or(previous.reasoning_output_tokens),
        }
    }
    fn decreased_from(&self, previous: &Self) -> bool {
        self.values()
            .into_iter()
            .zip(previous.values())
            .any(|(now, old)| now.zip(old).is_some_and(|(now, old)| now < old))
    }
    fn delta(&self, baseline: &Self) -> Self {
        let subtract =
            |now: Option<u64>, old: Option<u64>| now.zip(old).and_then(|(a, b)| a.checked_sub(b));
        Self {
            input_tokens: subtract(self.input_tokens, baseline.input_tokens),
            cached_input_tokens: subtract(self.cached_input_tokens, baseline.cached_input_tokens),
            output_tokens: subtract(self.output_tokens, baseline.output_tokens),
            reasoning_output_tokens: subtract(
                self.reasoning_output_tokens,
                baseline.reasoning_output_tokens,
            ),
        }
    }
}
fn counts(value: &Value) -> Result<TokenCounts, String> {
    if !value.is_object() {
        return Err("invalid app-server usage object".into());
    }
    Ok(TokenCounts {
        input_tokens: number(value, "inputTokens")?,
        cached_input_tokens: number(value, "cachedInputTokens")?,
        output_tokens: number(value, "outputTokens")?,
        reasoning_output_tokens: number(value, "reasoningOutputTokens")?,
    })
}
fn token_usage(params: &Value) -> Result<UsageSnapshot, String> {
    let usage = &params["tokenUsage"];
    let last = match usage.get("last").filter(|v| !v.is_null()) {
        Some(value) => counts(value)?,
        None => TokenCounts::default(),
    };
    Ok(UsageSnapshot {
        last: ProviderUsage {
            input_tokens: last.input_tokens,
            cached_input_tokens: last.cached_input_tokens,
            output_tokens: last.output_tokens,
            reasoning_output_tokens: last.reasoning_output_tokens,
            ..ProviderUsage::default()
        },
        total: usage
            .get("total")
            .filter(|v| !v.is_null())
            .map(counts)
            .transpose()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::NativeProfile;
    fn driver(resume: Option<&str>) -> Driver {
        let profile: NativeProfile =
            serde_json::from_value(json!({"provider":"codex_app_server","program":"/bin/true"}))
                .unwrap();
        Driver::new(
            ProviderResult::new(&profile, Some("0.160.0".into())),
            Start {
                cwd: "/tmp/task/repository".into(),
                prompt: "literal prompt\n✓".into(),
                model: Some("model".into()),
                effort: Some("high".into()),
                native_permission: None,
                resume: resume.map(str::to_owned),
                checkpoint: None,
            },
        )
    }
    fn feed(driver: &mut Driver, value: Value) {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        for chunk in bytes.chunks(7) {
            driver.feed(chunk);
        }
    }
    fn start_pending(driver: &mut Driver) {
        assert!(
            String::from_utf8(driver.take_pending())
                .unwrap()
                .contains("initialize")
        );
        feed(driver, json!({"id":1,"result":{}}));
        let request = String::from_utf8(driver.take_pending()).unwrap();
        assert!(request.contains("initialized"));
        assert!(request.contains("workspace-write"));
        if driver.start.resume.is_some() {
            assert!(request.contains("\"excludeTurns\":true"));
        }
        feed(
            driver,
            json!({"id":2,"result":{"thread":{"id":"thread-1"},"model":"reported"}}),
        );
        let request = String::from_utf8(driver.take_pending()).unwrap();
        assert!(request.contains("turn/start"));
        assert!(request.contains("\"networkAccess\":false"));
        assert!(request.contains("\"approvalPolicy\":\"never\""));
    }
    fn start(driver: &mut Driver) {
        start_pending(driver);
        feed(driver, json!({"id":3,"result":{"turn":{"id":"turn-1"}}}));
    }
    fn native_review_driver(resume: Option<&str>) -> Driver {
        let original = driver(resume);
        let mut start = original.start;
        start.native_permission = Some(NativePermission::CodexNativeSandboxedReview);
        Driver::new(original.result, start)
    }
    #[test]
    fn native_review_requires_fresh_ephemeral_and_complete_local_policy_before_prompt() {
        let cases = [
            json!({"thread":{"id":"thread-1","ephemeral":true}}),
            json!({"thread":{"id":"thread-1","ephemeral":true},"approvalPolicy":"never","sandbox":"read-only"}),
            json!({"thread":{"id":"thread-1","ephemeral":true},"approvalPolicy":"never","sandbox":{"type":"readOnly"}}),
            json!({"thread":{"id":"thread-1","ephemeral":true},"approvalPolicy":"never","sandbox":{"type":"readOnly","networkAccess":true}}),
            json!({"thread":{"id":"thread-1","ephemeral":true},"approvalPolicy":"on-request","sandbox":{"type":"readOnly","networkAccess":false}}),
            json!({"thread":{"id":"thread-1","ephemeral":false},"approvalPolicy":"never","sandbox":{"type":"readOnly","networkAccess":false}}),
            json!({"thread":{"id":"thread-1","ephemeral":true},"approvalPolicy":"never","sandbox":{"type":"workspaceWrite","networkAccess":false}}),
        ];
        for settings in cases {
            let mut d = native_review_driver(None);
            d.take_pending();
            feed(&mut d, json!({"id":1,"result":{}}));
            let sent = String::from_utf8(d.take_pending()).unwrap();
            assert!(
                sent.contains("thread/start")
                    && sent.contains("read-only")
                    && sent.contains("\"ephemeral\":true")
            );
            assert!(!sent.contains("literal prompt"));
            feed(&mut d, json!({"id":2,"result":settings}));
            assert!(d.failure().is_some());
            assert!(d.take_pending().is_empty());
        }
        let mut resumed = native_review_driver(Some("old-thread"));
        assert!(resumed.failure().unwrap().contains("fresh sessions only"));
        assert!(resumed.take_pending().is_empty());
        let mut d = native_review_driver(None);
        d.take_pending();
        feed(&mut d, json!({"id":1,"result":{}}));
        d.take_pending();
        feed(
            &mut d,
            json!({"id":2,"result":{"thread":{"id":"thread-1","ephemeral":true},"approvalPolicy":"never","sandbox":{"type":"readOnly","networkAccess":false}}}),
        );
        let sent: Value = serde_json::from_slice(&d.take_pending()).unwrap();
        assert_eq!(sent["method"], "turn/start");
        assert_eq!(sent["params"]["approvalPolicy"], "never");
        assert_eq!(
            sent["params"]["sandboxPolicy"],
            json!({"type":"readOnly","networkAccess":false})
        );
        assert_eq!(sent["params"]["input"][0]["text"], "literal prompt\n✓");
        feed(
            &mut d,
            json!({"method":"thread/settings/updated","params":{
                "threadId":"thread-1","threadSettings":{"cwd":"/tmp/task/repository","approvalPolicy":"never","sandboxPolicy":{"type":"readOnly","networkAccess":false}}
            }}),
        );
        assert!(d.failure().is_none());
        feed(
            &mut d,
            json!({"id":4,"method":"item/commandExecution/requestApproval","params":{}}),
        );
        assert!(d.failure().unwrap().contains("requested approval"));
        assert!(d.take_pending().is_empty());
    }
    #[test]
    fn native_review_rejects_observed_policy_or_candidate_directory_drift() {
        for change in [
            json!({}),
            json!({"approvalPolicy":"on-request"}),
            json!({"sandboxPolicy":{"type":"readOnly","networkAccess":true}}),
            json!({"cwd":"/other"}),
        ] {
            let mut d = native_review_driver(None);
            d.take_pending();
            feed(&mut d, json!({"id":1,"result":{}}));
            d.take_pending();
            feed(
                &mut d,
                json!({"id":2,"result":{"thread":{"id":"thread-1","ephemeral":true},"approvalPolicy":"never","sandbox":{"type":"readOnly","networkAccess":false}}}),
            );
            d.take_pending();
            let mut settings = json!({"cwd":"/tmp/task/repository","approvalPolicy":"never","sandboxPolicy":{"type":"readOnly","networkAccess":false}});
            if change.as_object().unwrap().is_empty() {
                settings = json!({});
            } else {
                for (key, value) in change.as_object().unwrap() {
                    settings[key] = value.clone();
                }
            }
            feed(
                &mut d,
                json!({"method":"thread/settings/updated","params":{"threadId":"thread-1","threadSettings":settings}}),
            );
            assert!(d.failure().unwrap().contains("settings changed"));
            assert_eq!(
                d.result.selection.as_ref().unwrap().verification.permission,
                "mismatch"
            );
        }
    }
    fn usage(thread: &str, turn: &str, input: u64) -> Value {
        json!({"method":"thread/tokenUsage/updated","params":{
            "threadId":thread,"turnId":turn,"tokenUsage":{"last":{"inputTokens":input}}
        }})
    }
    #[test]
    fn resume_usage_replay_does_not_bind_or_charge_the_new_turn() {
        for started_first in [false, true] {
            let mut d = driver(Some("thread-1"));
            // The turn/start request is already sent, but its response is pending.
            start_pending(&mut d);
            feed(&mut d, usage("thread-1", "previous-turn", 900));
            assert!(d.failure().is_none(), "{:?}", d.failure());
            assert!(d.turn.is_none());
            assert!(d.result.usage.input_tokens.is_none());
            if started_first {
                feed(
                    &mut d,
                    json!({"method":"turn/started","params":{
                        "threadId":"thread-1","turn":{"id":"turn-1"}
                    }}),
                );
                assert!(d.result.usage.input_tokens.is_none());
                // Live usage before the response must still be retained.
                feed(&mut d, usage("thread-1", "turn-1", 12));
            }
            feed(&mut d, json!({"id":3,"result":{"turn":{"id":"turn-1"}}}));
            if !started_first {
                assert!(d.result.usage.input_tokens.is_none());
                feed(&mut d, usage("thread-1", "turn-1", 12));
            }
            answer(&mut d);
            complete(&mut d, "completed");
            let (result, error) = d.finish();
            assert!(error.is_none(), "{error:?}");
            assert_eq!(result.usage.input_tokens, Some(12));
        }
    }
    #[test]
    fn pending_current_usage_is_retained_only_after_matching_turn_identity() {
        for started_first in [false, true] {
            let mut d = driver(Some("thread-1"));
            start_pending(&mut d);
            feed(&mut d, usage("thread-1", "previous-turn", 900));
            feed(&mut d, usage("thread-1", "turn-1", 12));
            assert!(d.turn.is_none());
            assert!(d.result.usage.input_tokens.is_none());
            if started_first {
                feed(
                    &mut d,
                    json!({"method":"turn/started","params":{
                        "threadId":"thread-1","turn":{"id":"turn-1"}
                    }}),
                );
            }
            feed(&mut d, json!({"id":3,"result":{"turn":{"id":"turn-1"}}}));
            answer(&mut d);
            complete(&mut d, "completed");
            let (result, error) = d.finish();
            assert!(error.is_none(), "{error:?}");
            assert_eq!(result.usage.input_tokens, Some(12));
        }
        let mut d = driver(Some("thread-1"));
        start_pending(&mut d);
        feed(&mut d, usage("thread-1", "previous-turn", 900));
        assert!(d.finish().1.is_some()); // Usage alone cannot complete a turn.
    }
    #[test]
    fn resume_usage_window_does_not_relax_thread_or_turn_fencing() {
        let bad = [
            usage("other-thread", "previous-turn", 900),
            usage("thread-1", "", 900),
            json!({"method":"thread/tokenUsage/updated","params":{
                "threadId":"thread-1","turnId":"previous-turn",
                "tokenUsage":{"last":{"inputTokens":-1}}
            }}),
            json!({"method":"turn/completed","params":{
                "threadId":"thread-1","turn":{"id":"previous-turn","status":"completed"}
            }}),
            json!({"method":"item/completed","params":{
                "threadId":"thread-1","turnId":"previous-turn",
                "item":{"type":"agentMessage","text":"old answer"}
            }}),
        ];
        for event in bad {
            let mut d = driver(Some("thread-1"));
            start_pending(&mut d);
            feed(&mut d, event);
            assert!(d.failure().is_some());
        }
        let mut fresh = driver(None);
        start_pending(&mut fresh);
        feed(&mut fresh, usage("thread-1", "previous-turn", 900));
        assert!(fresh.failure().is_some());

        let mut before_resume = driver(Some("thread-1"));
        before_resume.take_pending();
        feed(&mut before_resume, json!({"id":1,"result":{}}));
        feed(&mut before_resume, usage("thread-1", "previous-turn", 900));
        assert!(before_resume.failure().is_some());

        for started_first in [false, true] {
            let mut active = driver(Some("thread-1"));
            start_pending(&mut active);
            if started_first {
                feed(
                    &mut active,
                    json!({"method":"turn/started","params":{
                        "threadId":"thread-1","turn":{"id":"turn-1"}
                    }}),
                );
            } else {
                feed(
                    &mut active,
                    json!({"id":3,"result":{"turn":{"id":"turn-1"}}}),
                );
            }
            feed(&mut active, usage("thread-1", "previous-turn", 900));
            assert!(active.failure().is_some());
        }
        let mut changed = driver(Some("thread-1"));
        start_pending(&mut changed);
        feed(&mut changed, usage("thread-1", "turn-1", 12));
        feed(
            &mut changed,
            json!({"method":"turn/started","params":{
                "threadId":"thread-1","turn":{"id":"turn-1"}
            }}),
        );
        feed(
            &mut changed,
            json!({"id":3,"result":{"turn":{"id":"other-turn"}}}),
        );
        assert!(changed.failure().is_some());
    }
    fn cumulative(turn: &str, input: u64, cached: u64, output: u64, reasoning: u64) -> Value {
        json!({"method":"thread/tokenUsage/updated","params":{
            "threadId":"thread-1","turnId":turn,"tokenUsage":{
                "last":{"inputTokens":7,"outputTokens":2},
                "total":{"inputTokens":input,"cachedInputTokens":cached,
                    "outputTokens":output,"reasoningOutputTokens":reasoning}
            }
        }})
    }
    #[test]
    fn cumulative_turn_delta_excludes_history_and_deduplicates_snapshots() {
        for started_first in [false, true] {
            let mut d = driver(Some("thread-1"));
            start_pending(&mut d);
            feed(&mut d, cumulative("old-turn", 100, 80, 20, 3));
            if started_first {
                feed(
                    &mut d,
                    json!({"method":"turn/started","params":{
                    "threadId":"thread-1","turn":{"id":"turn-1"}}}),
                );
            }
            feed(&mut d, cumulative("turn-1", 150, 110, 25, 3));
            feed(&mut d, json!({"id":3,"result":{"turn":{"id":"turn-1"}}}));
            let final_usage = cumulative("turn-1", 190, 130, 31, 5);
            feed(&mut d, final_usage.clone());
            feed(&mut d, final_usage);
            answer(&mut d);
            complete(&mut d, "completed");
            let (result, error) = d.finish();
            assert!(error.is_none(), "{error:?}");
            assert_eq!(result.usage.input_tokens, Some(7));
            assert_eq!(
                result.usage.turn_total,
                Some(TokenCounts {
                    input_tokens: Some(90),
                    cached_input_tokens: Some(50),
                    output_tokens: Some(11),
                    reasoning_output_tokens: Some(2),
                })
            );
            assert!(result.usage.total_cost_usd.is_none());
            assert!(result.usage.num_turns.is_none());
        }
    }
    #[test]
    fn absent_baselines_and_fields_remain_unknown_and_zero_is_known() {
        let mut resumed = driver(Some("thread-1"));
        start(&mut resumed);
        feed(&mut resumed, cumulative("turn-1", 150, 110, 25, 3));
        assert!(resumed.result.usage.turn_total.is_none());
        let mut fresh = driver(None);
        start(&mut fresh);
        feed(&mut fresh, cumulative("turn-1", 30, 0, 8, 0));
        assert_eq!(
            fresh.result.usage.turn_total.as_ref().unwrap().input_tokens,
            Some(30)
        );
        assert_eq!(
            fresh
                .result
                .usage
                .turn_total
                .as_ref()
                .unwrap()
                .reasoning_output_tokens,
            Some(0)
        );
        let mut partial = cumulative("turn-1", 35, 0, 9, 0);
        partial["params"]["tokenUsage"]["total"]
            .as_object_mut()
            .unwrap()
            .remove("cachedInputTokens");
        feed(&mut fresh, partial);
        assert!(
            fresh
                .result
                .usage
                .turn_total
                .as_ref()
                .unwrap()
                .cached_input_tokens
                .is_none()
        );
        assert_eq!(
            fresh.result.usage.turn_total.as_ref().unwrap().input_tokens,
            Some(35)
        );
        feed(&mut fresh, usage("thread-1", "turn-1", 7));
        assert!(fresh.result.usage.turn_total.is_none());
    }
    #[test]
    fn reset_or_reordered_counters_invalidate_totals_without_negative_values() {
        let mut d = driver(None);
        start(&mut d);
        feed(&mut d, cumulative("turn-1", 100, 60, 20, 5));
        feed(&mut d, cumulative("turn-1", 90, 60, 20, 5));
        assert!(d.result.usage.turn_total.is_none());
        feed(&mut d, cumulative("turn-1", 200, 100, 30, 6));
        assert!(d.result.usage.turn_total.is_none());
        assert!(d.failure().is_none());
    }
    #[test]
    fn missing_fields_do_not_hide_historical_or_current_counter_resets() {
        for historical in [true, false] {
            let mut d = driver(Some("thread-1"));
            start_pending(&mut d);
            feed(&mut d, cumulative("old-turn", 100, 80, 20, 3));
            if !historical {
                feed(&mut d, json!({"id":3,"result":{"turn":{"id":"turn-1"}}}));
            }
            let turn = if historical { "old-turn" } else { "turn-1" };
            let mut partial = cumulative(turn, 100, 80, 20, 3);
            partial["params"]["tokenUsage"]["total"] = json!({});
            feed(&mut d, partial);
            feed(&mut d, cumulative(turn, 90, 80, 20, 3));
            if historical {
                feed(&mut d, json!({"id":3,"result":{"turn":{"id":"turn-1"}}}));
            }
            feed(&mut d, cumulative("turn-1", 120, 80, 20, 3));
            assert!(d.result.usage.turn_total.is_none());
            assert!(d.failure().is_none());
        }
    }
    #[test]
    fn partial_baseline_keeps_historical_high_water_for_reset_detection() {
        let mut d = driver(Some("thread-1"));
        start_pending(&mut d);
        feed(&mut d, cumulative("old-turn", 100, 80, 20, 3));
        let mut partial = cumulative("old-turn", 100, 80, 20, 3);
        partial["params"]["tokenUsage"]["total"]
            .as_object_mut()
            .unwrap()
            .remove("inputTokens");
        feed(&mut d, partial);
        feed(&mut d, json!({"id":3,"result":{"turn":{"id":"turn-1"}}}));
        feed(&mut d, cumulative("turn-1", 90, 80, 25, 3));
        assert!(d.result.usage.turn_total.is_none());
    }
    #[test]
    fn failed_turn_keeps_observed_usage_without_claiming_success() {
        let mut d = driver(None);
        start(&mut d);
        feed(&mut d, cumulative("turn-1", 100, 60, 20, 5));
        complete(&mut d, "failed");
        let (result, error) = d.finish();
        assert!(error.is_some());
        assert_eq!(result.usage.turn_total.unwrap().input_tokens, Some(100));
    }
    #[test]
    fn pending_usage_is_bounded_and_legacy_usage_deserializes() {
        let legacy: ProviderUsage = serde_json::from_value(json!({"input_tokens":7})).unwrap();
        assert!(legacy.turn_total.is_none());
        assert!(legacy.usage_scope.is_none());
        let mut d = driver(Some("thread-1"));
        start_pending(&mut d);
        for _ in 0..33 {
            feed(&mut d, cumulative("old-turn", 100, 80, 20, 3));
        }
        assert!(d.failure().unwrap().contains("budget"));
    }
    fn answer(driver: &mut Driver) {
        feed(
            driver,
            json!({"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1","item":{"type":"agentMessage","phase":"final_answer","text":"done ✓"}}}),
        );
    }
    fn complete(driver: &mut Driver, status: &str) {
        feed(
            driver,
            json!({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":status,"error":null}}}),
        );
    }
    #[test]
    fn handshake_and_explicit_resume_normalize_a_correlated_turn() {
        for resume in [None, Some("thread-1")] {
            let mut d = driver(resume);
            start(&mut d);
            for method in [
                "turn/diff/updated",
                "turn/plan/updated",
                "turn/moderationMetadata",
            ] {
                feed(
                    &mut d,
                    json!({"method":method,"params":{"threadId":"thread-1","turnId":"turn-1"}}),
                );
            }
            answer(&mut d);
            complete(&mut d, "completed");
            assert!(d.stopped());
            let (result, error) = d.finish();
            assert!(error.is_none(), "{error:?}");
            assert_eq!(result.session_id.as_deref(), Some("thread-1"));
            assert_eq!(result.summary, "done ✓");
        }
    }
    #[test]
    fn no_success_from_wrong_ids_duplicate_terminals_or_missing_answers() {
        let bad = [
            json!({"id":99,"result":{}}),
            json!({"method":"turn/completed","params":{"threadId":"other","turn":{"id":"turn-1","status":"completed"}}}),
            json!({"method":"item/completed","params":{"threadId":"thread-1","turnId":"other","item":{"type":"agentMessage","text":"bad"}}}),
            json!({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"mystery"}}}),
        ];
        for event in bad {
            let mut d = driver(None);
            start(&mut d);
            feed(&mut d, event);
            answer(&mut d);
            complete(&mut d, "completed");
            assert!(d.finish().1.is_some());
        }
        let mut d = driver(None);
        start(&mut d);
        complete(&mut d, "completed");
        assert!(d.finish().1.is_some());
        let mut d = driver(None);
        start(&mut d);
        answer(&mut d);
        complete(&mut d, "completed");
        complete(&mut d, "completed");
        assert!(d.finish().1.is_some());
    }
    #[test]
    fn server_requests_are_never_approved_and_errors_are_sticky() {
        for method in [
            "item/commandExecution/requestApproval",
            "item/fileChange/requestApproval",
            "item/tool/requestUserInput",
            "item/tool/call",
            "unknown",
        ] {
            let mut d = driver(None);
            start(&mut d);
            feed(
                &mut d,
                json!({"id":"server-request","method":method,"params":{}}),
            );
            let response = String::from_utf8(d.take_pending()).unwrap();
            assert!(response.is_empty());
            assert!(d.finish().1.is_some());
        }
        let mut d = driver(None);
        start(&mut d);
        d.feed(&vec![b'x'; MAX_PROTOCOL_LINE + 1]);
        assert!(d.finish().1.unwrap().contains("64 KiB"));
        let mut d = driver(None);
        start(&mut d);
        d.feed(b"bad\n");
        answer(&mut d);
        complete(&mut d, "completed");
        assert!(d.finish().1.unwrap().contains("malformed"));
    }
    #[test]
    fn resume_cannot_switch_thread_or_fall_back_to_new() {
        let mut d = driver(Some("other-thread"));
        d.take_pending();
        feed(&mut d, json!({"id":1,"result":{}}));
        d.take_pending();
        feed(
            &mut d,
            json!({"id":2,"result":{"thread":{"id":"thread-1"}}}),
        );
        assert!(d.finish().1.unwrap().contains("different thread"));
    }

    fn select_permission(driver: &mut Driver, permission: NativePermission) {
        driver.start.native_permission = Some(permission);
        driver
            .result
            .selection
            .as_mut()
            .unwrap()
            .requested
            .native_permission = Some(permission);
    }
    fn pending_values(driver: &mut Driver) -> Vec<Value> {
        String::from_utf8(driver.take_pending())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    #[test]
    fn explicit_native_modes_match_thread_and_turn_and_keep_session_evidence_separate() {
        for resume in [None, Some("thread-1")] {
            for (mode, thread_mode, turn_mode) in [
                (
                    NativePermission::CodexWorkspaceWrite,
                    "workspace-write",
                    "workspaceWrite",
                ),
                (
                    NativePermission::CodexFullAccess,
                    "danger-full-access",
                    "dangerFullAccess",
                ),
            ] {
                let mut d = driver(resume);
                select_permission(&mut d, mode);
                d.take_pending();
                feed(&mut d, json!({"id":1,"result":{}}));
                let outgoing = pending_values(&mut d);
                assert_eq!(outgoing[1]["params"]["sandbox"], thread_mode);
                assert_eq!(outgoing[1]["params"]["approvalPolicy"], "never");
                feed(
                    &mut d,
                    json!({"id":2,"result":{
                        "thread":{"id":"thread-1"},"model":"resolved-session-model",
                        "reasoningEffort":"low","approvalPolicy":"never","sandbox":{"type":turn_mode}
                    }}),
                );
                let outgoing = pending_values(&mut d);
                assert_eq!(outgoing[0]["method"], "turn/start");
                assert_eq!(outgoing[0]["params"]["sandboxPolicy"]["type"], turn_mode);
                assert_eq!(outgoing[0]["params"]["approvalPolicy"], "never");
                assert_eq!(outgoing[0]["params"]["effort"], "high");
                if mode == NativePermission::CodexFullAccess {
                    assert_eq!(
                        outgoing[0]["params"]["sandboxPolicy"],
                        json!({"type":"dangerFullAccess"})
                    );
                }
                feed(
                    &mut d,
                    json!({"id":3,"result":{"turn":{"id":"turn-1","model":"not-effective","effort":"not-effective"}}}),
                );
                answer(&mut d);
                complete(&mut d, "completed");
                let (result, error) = d.finish();
                assert!(error.is_none(), "{error:?}");
                assert!(result.reported_model.is_none());
                let evidence = result.selection.unwrap();
                assert!(evidence.observed.model.is_none());
                assert_eq!(evidence.verification.model, "session_reported");
                assert_eq!(evidence.verification.effort, "unknown");
                assert_eq!(evidence.verification.permission, "session_reported");
                let settings = evidence.session_settings.unwrap();
                assert_eq!(settings.model.as_deref(), Some("resolved-session-model"));
                assert_eq!(settings.effort.as_deref(), Some("low"));
                assert_eq!(
                    serde_json::from_str::<Value>(settings.sandbox.as_ref().unwrap()).unwrap(),
                    json!({"type": turn_mode})
                );
            }
        }
    }
    #[test]
    fn explicit_permission_mismatch_stops_before_turn_and_partial_evidence_stays_unknown() {
        for settings in [
            json!({"sandbox":{"type":"dangerFullAccess"},"approvalPolicy":"never"}),
            json!({"sandbox":{"type":"workspaceWrite"},"approvalPolicy":"on-request"}),
            json!({"sandbox":"workspace-write","approvalPolicy":{"type":"never"}}),
        ] {
            let mut d = driver(None);
            select_permission(&mut d, NativePermission::CodexWorkspaceWrite);
            d.take_pending();
            feed(&mut d, json!({"id":1,"result":{}}));
            d.take_pending();
            let mut result = settings;
            result["thread"] = json!({"id":"thread-1"});
            feed(&mut d, json!({"id":2,"result":result}));
            assert!(d.failure().unwrap().contains("native permission mismatch"));
            assert!(d.stopped());
            assert!(d.take_pending().is_empty());
            assert_eq!(
                d.result.selection.as_ref().unwrap().verification.permission,
                "mismatch"
            );
        }
        for settings in [
            json!({}),
            json!({"sandbox":{"type":"workspaceWrite"}}),
            json!({"approvalPolicy":"never"}),
        ] {
            let mut d = driver(None);
            select_permission(&mut d, NativePermission::CodexWorkspaceWrite);
            d.take_pending();
            feed(&mut d, json!({"id":1,"result":{}}));
            d.take_pending();
            let mut result = settings;
            result["thread"] = json!({"id":"thread-1"});
            feed(&mut d, json!({"id":2,"result":result}));
            assert!(d.failure().is_none());
            assert_eq!(pending_values(&mut d)[0]["method"], "turn/start");
            assert_eq!(
                d.result.selection.as_ref().unwrap().verification.permission,
                "unknown"
            );
        }
    }
    fn reroute(thread: &str, turn: &str, model: &str) -> Value {
        json!({"method":"model/rerouted","params":{
            "threadId":thread,"turnId":turn,"fromModel":"requested-model","toModel":model,"reason":"rateLimit"
        }})
    }
    #[test]
    fn reroutes_are_exact_turn_evidence_bounded_and_never_main_message_models() {
        let mut d = driver(None);
        start(&mut d);
        for index in 0..10 {
            feed(
                &mut d,
                reroute("thread-1", "turn-1", &format!("route-{index}")),
            );
        }
        assert!(d.failure().is_none());
        assert!(d.result.reported_model.is_none());
        let evidence = d.result.selection.as_ref().unwrap();
        assert_eq!(evidence.verification.model, "rerouted");
        assert_eq!(evidence.observed.model.as_deref(), Some("route-9"));
        assert_eq!(evidence.observed.reroutes.len(), 8);
        assert_eq!(evidence.observed.reroutes[0].to_model, "route-2");
        assert_eq!(evidence.observed.reroutes[0].from_model, "requested-model");
        assert!(evidence.truncated);
        feed(
            &mut d,
            json!({"method":"item/completed","params":{
                "threadId":"thread-1","turnId":"turn-1",
                "item":{"type":"agentMessage","text":"answer","model":"main-message-model"}
            }}),
        );
        complete(&mut d, "completed");
        let (result, error) = d.finish();
        assert!(error.is_none());
        assert_eq!(result.reported_model.as_deref(), Some("main-message-model"));
        assert_eq!(
            result.selection.unwrap().verification.model,
            "message_reported"
        );
        for event in [
            reroute("other-thread", "turn-1", "model"),
            reroute("thread-1", "old-turn", "model"),
        ] {
            let mut d = driver(Some("thread-1"));
            start(&mut d);
            feed(&mut d, event);
            assert!(d.failure().is_some());
            assert!(d.result.selection.unwrap().observed.reroutes.is_empty());
        }
    }
    #[test]
    fn reroutes_cannot_bind_pending_turns_or_accept_unbounded_fields() {
        let mut d = driver(Some("thread-1"));
        start_pending(&mut d);
        feed(&mut d, reroute("thread-1", "old-turn", "model"));
        assert!(d.failure().is_some());
        assert!(d.turn.is_none());
        let mut d = driver(None);
        start(&mut d);
        feed(&mut d, reroute("thread-1", "turn-1", &"m".repeat(257)));
        assert!(d.failure().is_some());
    }

    #[test]
    fn thread_sandbox_details_are_preserved_and_bounded_independently_of_kind_checks() {
        for long in [false, true] {
            let mut d = driver(None);
            select_permission(&mut d, NativePermission::CodexWorkspaceWrite);
            d.take_pending();
            feed(&mut d, json!({"id":1,"result":{}}));
            d.take_pending();
            let policy = json!({"type":"workspaceWrite","networkAccess":true,
                "writableRoots":[if long { "界".repeat(2000) } else { "/configured/root".into() }]});
            feed(
                &mut d,
                json!({"id":2,"result":{
                    "thread":{"id":"thread-1"},"sandbox":policy,"approvalPolicy":"never"
                }}),
            );
            assert!(d.failure().is_none());
            let evidence = d.result.selection.as_ref().unwrap();
            assert_eq!(evidence.verification.permission, "session_reported");
            assert_eq!(evidence.truncated, long);
            let captured = evidence
                .session_settings
                .as_ref()
                .unwrap()
                .sandbox
                .as_ref()
                .unwrap();
            assert!(captured.len() <= MAX_SESSION_POLICY);
            assert!(captured.contains("networkAccess"));
            if !long {
                assert_eq!(serde_json::from_str::<Value>(captured).unwrap(), policy);
            }
            // The server's thread settings are shown as returned, separately
            // from Relay's subsequent explicit turn policy override.
            assert_eq!(
                pending_values(&mut d)[0]["params"]["sandboxPolicy"]["networkAccess"],
                false
            );
            d.result.bound();
            assert_eq!(d.result.selection.as_ref().unwrap().truncated, long);
        }
    }
    fn native_review(status: &str, index: usize) -> Value {
        json!({"method":"item/autoApprovalReview/completed","params":{
            "threadId":"thread-1","turnId":"turn-1","reviewId":format!("review-{index}"),
            "action":{"type":"networkAccess","host":"must-not-be-copied.example"},
            "review":{"status":status,"rationale":"Native policy reason","model":"not-the-developer-model"}
        }})
    }
    #[test]
    fn native_approval_decisions_are_bounded_observations_not_relay_approvals() {
        let mut d = driver(None);
        start(&mut d);
        d.take_pending();
        for (index, status) in [
            "approved",
            "denied",
            "timedOut",
            "aborted",
            "futureStatus",
            "denied",
            "approved",
            "denied",
            "approved",
            "denied",
        ]
        .iter()
        .enumerate()
        {
            let mut event = native_review(status, index);
            event["params"]["review"]["rationale"] = json!("界".repeat(200));
            feed(&mut d, event);
        }
        assert!(d.failure().is_none());
        // Native denials do not send an approval, grant access, or cause a Relay
        // retry. Codex may finish after a materially safer path in the same turn.
        assert!(d.take_pending().is_empty());
        let evidence = d.result.selection.as_ref().unwrap();
        let reviews = &evidence.observed.native_approval_reviews;
        assert_eq!(reviews.len(), 8);
        assert_eq!(reviews[0].review_id, "review-2");
        assert_eq!(reviews[7].status, "denied");
        assert!(reviews[0].rationale.as_ref().unwrap().len() <= 256);
        assert!(evidence.truncated);
        assert!(evidence.observed.model.is_none());
        assert!(
            !serde_json::to_string(evidence)
                .unwrap()
                .contains("must-not-be-copied")
        );
        answer(&mut d);
        complete(&mut d, "completed");
        let (mut result, error) = d.finish();
        assert!(error.is_none());
        result.shrink();
        assert_eq!(
            result
                .selection
                .unwrap()
                .observed
                .native_approval_reviews
                .len(),
            4
        );
    }
    #[test]
    fn native_approval_evidence_cannot_cross_threads_turns_or_terminal_boundaries() {
        for (field, value) in [
            ("threadId", "other-thread"),
            ("turnId", "old-turn"),
            ("reviewId", ""),
        ] {
            let mut d = driver(None);
            start(&mut d);
            let mut event = native_review("approved", 0);
            event["params"][field] = json!(value);
            feed(&mut d, event);
            assert!(d.failure().is_some());
            assert!(
                d.result
                    .selection
                    .as_ref()
                    .unwrap()
                    .observed
                    .native_approval_reviews
                    .is_empty()
            );
        }
        let mut d = driver(None);
        start(&mut d);
        answer(&mut d);
        complete(&mut d, "completed");
        feed(&mut d, native_review("approved", 0));
        assert!(d.failure().unwrap().contains("terminal"));
    }
    #[test]
    fn native_auto_settings_do_not_overwrite_stronger_model_observations() {
        for message_observed in [false, true] {
            let mut d = driver(None);
            start(&mut d);
            select_permission(&mut d, NativePermission::CodexAutoReview);
            feed(&mut d, reroute("thread-1", "turn-1", "rerouted-model"));
            if message_observed {
                feed(
                    &mut d,
                    json!({"method":"item/completed","params":{
                        "threadId":"thread-1","turnId":"turn-1",
                        "item":{"type":"agentMessage","text":"done", "model":"message-model"}
                    }}),
                );
            }
            let cwd = d.start.cwd.clone();
            feed(
                &mut d,
                json!({"method":"thread/settings/updated","params":{
                    "threadId":"thread-1","threadSettings":{
                        "cwd":cwd,"model":"configured-only", "effort":"low", "approvalPolicy":"on-request", "approvalsReviewer":"auto_review",
                        "sandboxPolicy":{"type":"workspaceWrite", "writableRoots":[cwd], "networkAccess":false, "excludeSlashTmp":false,"excludeTmpdirEnvVar":false}
                    }
                }}),
            );
            assert!(d.failure().is_none());
            let evidence = d.result.selection.as_ref().unwrap();
            assert_eq!(
                evidence.verification.model,
                if message_observed {
                    "message_reported"
                } else {
                    "rerouted"
                }
            );
            assert_eq!(
                evidence.session_settings.as_ref().unwrap().model.as_deref(),
                Some("configured-only")
            );
            assert_eq!(
                evidence.observed.model.as_deref(),
                Some(if message_observed {
                    "message-model"
                } else {
                    "rerouted-model"
                })
            );
        }
    }
}
