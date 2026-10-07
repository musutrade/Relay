//! Bounded, fresh-session Kiro CLI V3 ACP client. The host owns authentication
//! preflight, the process tree, and native integration trust. ACP permission
//! choices are not a sandbox, and this client never executes server callbacks.
use crate::providers::{
    MAX_PROTOCOL_LINE, NativePermission, ProviderKind, ProviderResult, SessionSettings,
    validate_selection_value,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;

const MAX_PENDING: usize = 256 * 1024;
const MAX_SUMMARY: usize = 4096;
const MAX_IGNORED_NOTIFICATIONS: usize = 1024;
const MAX_CONFIG_OPTIONS: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Start {
    pub cwd: PathBuf,
    pub prompt: String,
    pub model: Option<String>,
    #[serde(default)]
    pub native_permission: Option<NativePermission>,
}

// Preserve absent vs explicit null, and let serde reject duplicate envelope
// fields. A null method/id must not be reinterpreted as an ordinary response.
#[derive(Default)]
struct Field(Option<Value>);
impl<'de> Deserialize<'de> for Field {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Value::deserialize(deserializer).map(|value| Self(Some(value)))
    }
}
#[derive(Deserialize)]
struct Envelope {
    jsonrpc: String,
    #[serde(default)]
    id: Field,
    #[serde(default)]
    method: Field,
    #[serde(default)]
    params: Field,
    #[serde(default)]
    result: Field,
    #[serde(default)]
    error: Field,
}

pub(crate) struct Driver {
    result: ProviderResult,
    start: Start,
    line: Vec<u8>,
    pending: Vec<u8>,
    request: u64,
    dispatched: bool,
    taken_request: Option<u64>,
    provisional_session: Option<String>,
    message_id: Option<String>,
    ignored_notifications: usize,
    answer_seen: bool,
    terminal: bool,
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
            dispatched: false,
            taken_request: None,
            provisional_session: None,
            message_id: None,
            ignored_notifications: 0,
            answer_seen: false,
            terminal: false,
            error: None,
        };
        if driver.result.provider != ProviderKind::KiroCli
            || driver
                .start
                .native_permission
                .is_some_and(|mode| mode != NativePermission::KiroWorkspaceWrite)
        {
            driver.fail("native permission mode is incompatible with Kiro ACP");
        } else if !driver.start.cwd.is_absolute() || driver.start.cwd.to_str().is_none() {
            driver.fail("Kiro ACP requires an absolute UTF-8 workspace path");
        } else if driver
            .start
            .model
            .as_deref()
            .is_some_and(|model| !validate_selection_value(model))
        {
            driver.fail("invalid Kiro ACP model selection");
        } else {
            driver.send(json!({
                "jsonrpc":"2.0","id":1,"method":"initialize","params":{
                    "protocolVersion":1,
                    "clientInfo":{"name":"relay","version":env!("CARGO_PKG_VERSION")},
                    "clientCapabilities":{
                        "fs":{"readTextFile":false,"writeTextFile":false},
                        "terminal":false
                    }
                }
            }));
        }
        driver
    }
    fn send(&mut self, message: Value) {
        let bytes = serde_json::to_vec(&message).expect("JSON value");
        if self.pending.len() + bytes.len() + 1 > MAX_PENDING {
            self.fail("Kiro ACP outbound protocol budget exceeded");
        } else {
            self.pending.extend(bytes);
            self.pending.push(b'\n');
        }
    }
    pub fn take_pending(&mut self) -> Vec<u8> {
        if !self.pending.is_empty() && self.error.is_none() {
            self.taken_request = Some(self.request);
        }
        std::mem::take(&mut self.pending)
    }
    /// The supervisor calls this only after writing every byte of a nonempty
    /// batch returned by take_pending. Taking bytes alone is not dispatch proof.
    pub fn mark_input_complete(&mut self) {
        if self.error.is_some() {
            return;
        }
        if self.taken_request.take() != Some(self.request) || self.dispatched {
            self.fail("Kiro ACP input dispatch acknowledgement is out of sequence");
        } else {
            self.dispatched = true;
        }
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
            // A failed transcript can never release a queued session or prompt.
            self.pending.clear();
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
                if line.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
                    self.fail("Kiro ACP requires JSON-RPC object messages");
                    continue;
                }
                match serde_json::from_slice::<Envelope>(&line) {
                    Ok(event) => {
                        if let Err(error) = self.event(event) {
                            self.fail(&error);
                        }
                    }
                    Err(_) => self.fail("malformed Kiro ACP UTF-8/JSONL message"),
                }
            } else if self.line.len() >= MAX_PROTOCOL_LINE {
                self.fail("Kiro ACP JSONL line exceeds 64 KiB");
            } else {
                self.line.push(byte);
            }
        }
    }
    fn event(&mut self, event: Envelope) -> Result<(), String> {
        if event.jsonrpc != "2.0" {
            return Err("Kiro ACP requires JSON-RPC 2.0".into());
        }
        if let Some(method) = event.method.0.as_ref() {
            let method = identifier(method)?;
            if event.result.0.is_some() || event.error.0.is_some() {
                return Err("Kiro ACP method message contains response fields".into());
            }
            if let Some(id) = event.id.0.as_ref() {
                return self.deny_request(id, method, event.params.0.as_ref());
            }
            return self.notification(method, event.params.0.as_ref());
        }
        if event.params.0.is_some() {
            return Err("Kiro ACP response contains request parameters".into());
        }
        if event.id.0.as_ref().and_then(Value::as_u64) != Some(self.request) || self.terminal {
            return Err("unexpected or duplicate Kiro ACP response ID".into());
        }
        if !self.dispatched {
            return Err("Kiro ACP response arrived before request dispatch completed".into());
        }
        if event.error.0.is_some() {
            return Err("Kiro ACP request failed; no automatic replay or authentication".into());
        }
        let result = event
            .result
            .0
            .as_ref()
            .filter(|value| value.is_object())
            .ok_or("Kiro ACP response requires an object result")?;
        check_kiro_meta(result)?;
        match self.request {
            1 => {
                if result.get("protocolVersion").and_then(Value::as_u64) != Some(1) {
                    return Err("Kiro ACP did not negotiate protocol version 1".into());
                }
                let mut params = json!({"cwd":self.start.cwd,"mcpServers":[]});
                let mut kiro = serde_json::Map::new();
                if let Some(model) = &self.start.model {
                    kiro.insert("modelId".into(), json!(model));
                }
                if self.start.native_permission == Some(NativePermission::KiroWorkspaceWrite) {
                    kiro.insert("policyPreset".into(), json!(["edit-workspace"]));
                }
                if !kiro.is_empty() {
                    params["_meta"] = json!({"kiro":kiro});
                }
                self.next_request("session/new", params);
            }
            2 => {
                let session = identifier(&result["sessionId"])?;
                if self
                    .provisional_session
                    .as_deref()
                    .is_some_and(|expected| expected != session)
                {
                    return Err(
                        "Kiro ACP session/new disagrees with startup session updates".into(),
                    );
                }
                self.result.session_id = Some(session.into());
                if let Some(options) = result.get("configOptions") {
                    self.config_options(options, "kiro.session/new.configOptions")?;
                }
                self.next_request(
                    "session/prompt",
                    json!({"sessionId":session,"prompt":[{"type":"text","text":self.start.prompt}]}),
                );
            }
            3 => {
                let reason = identifier(&result["stopReason"])?;
                self.result.terminal_reason = Some(reason.into());
                if reason != "end_turn" {
                    return Err("Kiro ACP prompt ended without end_turn".into());
                }
                if !self.answer_seen {
                    return Err("Kiro ACP turn contained no final assistant text".into());
                }
                self.terminal = true;
            }
            _ => return Err("unexpected Kiro ACP request state".into()),
        }
        Ok(())
    }
    fn next_request(&mut self, method: &str, params: Value) {
        self.request += 1;
        self.dispatched = false;
        self.send(json!({"jsonrpc":"2.0","id":self.request,"method":method,"params":params}));
    }
    fn deny_request(
        &mut self,
        id: &Value,
        method: &str,
        params: Option<&Value>,
    ) -> Result<(), String> {
        if !(id.is_i64() || id.is_u64() || id.as_str().is_some_and(valid_identifier)) {
            return Err("invalid Kiro ACP server request ID".into());
        }
        if method == "session/request_permission" {
            let valid_session = params
                .and_then(|params| params.get("sessionId"))
                .and_then(Value::as_str)
                .is_some_and(|id| self.result.session_id.as_deref() == Some(id));
            self.fail(if valid_session {
                "Kiro ACP permission request denied; interactive approval is unsupported"
            } else {
                "Kiro ACP permission request has an unknown or foreign session"
            });
            // Cancelled is a standard denial; never manufacture an optionId or
            // persist consent. Shutdown may occur before this best-effort reply.
            self.send(
                json!({"jsonrpc":"2.0","id":id,"result":{"outcome":{"outcome":"cancelled"}}}),
            );
        } else {
            self.fail("Kiro ACP requested an unsupported client action; callback denied");
            self.send(json!({"jsonrpc":"2.0","id":id,"error":{
                "code":-32601,"message":"Relay does not support client callbacks"
            }}));
        }
        Ok(())
    }
    fn notification(&mut self, method: &str, params: Option<&Value>) -> Result<(), String> {
        if method == "session/update" {
            if self.terminal {
                return Err("Kiro ACP session changed after prompt completion".into());
            }
            let params = params
                .filter(|params| params.is_object())
                .ok_or("Kiro ACP session/update requires object params")?;
            self.check_session(&params["sessionId"])?;
            let update = params
                .get("update")
                .filter(|update| update.is_object())
                .ok_or("Kiro ACP session/update requires an object update")?;
            check_kiro_meta(update)?;
            let kind = identifier(&update["sessionUpdate"])?;
            match kind {
                "agent_message_chunk" => {
                    self.require_active_prompt()?;
                    let content = update
                        .get("content")
                        .filter(|content| content.is_object())
                        .ok_or("Kiro ACP message chunk requires content")?;
                    let content_type = identifier(&content["type"])?;
                    if content_type == "text" {
                        let text = content["text"]
                            .as_str()
                            .ok_or("Kiro ACP text chunk requires text")?;
                        if let Some(id) = update.get("messageId") {
                            let id = identifier(id)?;
                            if self.message_id.as_deref() != Some(id) {
                                self.clear_answer();
                                self.message_id = Some(id.into());
                            }
                        }
                        self.answer_seen |= !text.trim().is_empty();
                        self.append_answer(text);
                    } else {
                        self.ignore_notification()?;
                    }
                }
                "tool_call" | "tool_call_update" => {
                    self.require_active_prompt()?;
                    identifier(&update["toolCallId"])?;
                    if let Some(status) = update.get("status")
                        && !matches!(
                            status.as_str(),
                            Some("pending" | "in_progress" | "completed" | "failed")
                        )
                    {
                        return Err("invalid Kiro ACP tool status".into());
                    }
                    // Pre-tool commentary is not the completed turn's answer.
                    self.clear_answer();
                }
                "agent_thought_chunk" | "user_message_chunk" | "plan" => {
                    self.require_active_prompt()?;
                }
                "config_option_update" => {
                    self.config_options(
                        &update["configOptions"],
                        "kiro.session/update.configOptions",
                    )?;
                }
                "available_commands_update"
                | "current_mode_update"
                | "session_info_update"
                | "usage_update" => {
                    // Context percentage and credit metering are not token
                    // counters or USD. turn_end/turn_completion is not terminal.
                }
                _ => self.ignore_notification()?,
            }
            return Ok(());
        }
        if method == "session/request_permission"
            || method == "error"
            || method.starts_with("_kiro/error/")
            || matches!(
                method,
                "_kiro/customAgent/not_found" | "_kiro/customAgent/config_error"
            )
        {
            return Err("Kiro ACP reported an error or malformed permission request".into());
        }
        if let Some(session) = params.and_then(|params| params.get("sessionId")) {
            if self.terminal {
                return Err("Kiro ACP session changed after prompt completion".into());
            }
            self.check_session(session)?;
        }
        self.ignore_notification()
    }
    fn check_session(&mut self, session: &Value) -> Result<(), String> {
        let session = identifier(session)?;
        if let Some(expected) = self
            .result
            .session_id
            .as_deref()
            .or(self.provisional_session.as_deref())
        {
            if expected != session {
                return Err("Kiro ACP update has a foreign session ID".into());
            }
        } else if self.request == 2 && self.dispatched {
            // Startup metadata can precede session/new's response. Bind it
            // provisionally and require the response to confirm the same ID.
            self.provisional_session = Some(session.into());
        } else {
            return Err("Kiro ACP session update arrived before session creation".into());
        }
        Ok(())
    }
    fn require_active_prompt(&self) -> Result<(), String> {
        if self.request != 3 || !self.dispatched || self.terminal {
            Err("Kiro ACP turn update arrived before prompt dispatch completed".into())
        } else {
            Ok(())
        }
    }
    fn ignore_notification(&mut self) -> Result<(), String> {
        self.ignored_notifications += 1;
        if self.ignored_notifications > MAX_IGNORED_NOTIFICATIONS {
            Err("Kiro ACP ignored-notification budget exceeded".into())
        } else {
            Ok(())
        }
    }
    fn config_options(&mut self, options: &Value, source: &str) -> Result<(), String> {
        let options = options
            .as_array()
            .filter(|options| options.len() <= MAX_CONFIG_OPTIONS)
            .ok_or("Kiro ACP configOptions must be a bounded array")?;
        let mut model = None;
        let mut effort = None;
        for option in options {
            let id = identifier(&option["id"])?;
            let target = match id {
                "model" => &mut model,
                "effortLevel" => &mut effort,
                _ => continue,
            };
            if target.is_some() || option["type"] != "select" {
                return Err(
                    "Kiro ACP reported duplicate or invalid model/effort configuration".into(),
                );
            }
            *target = Some(identifier(&option["currentValue"])?.to_owned());
        }
        if let Some(selection) = &mut self.result.selection {
            selection.verification.model = if model.is_some() {
                "session_reported"
            } else {
                "unknown"
            }
            .into();
            selection.session_settings = Some(SessionSettings {
                cwd: None,
                model,
                effort,
                approval_policy: None,
                approvals_reviewer: None,
                sandbox: None,
                permission_mode: None,
                source: source.into(),
            });
        }
        // Session configuration never sets reported_model/observed.model: it is
        // not evidence of the model that actually executed this prompt.
        Ok(())
    }
    fn clear_answer(&mut self) {
        self.result.summary.clear();
        self.result.summary_truncated = false;
        self.answer_seen = false;
        self.message_id = None;
    }
    fn append_answer(&mut self, text: &str) {
        if self.result.summary_truncated {
            return;
        }
        let available = MAX_SUMMARY.saturating_sub(self.result.summary.len());
        let mut end = available.min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.result.summary.push_str(&text[..end]);
        self.result.summary_truncated = end < text.len();
    }
    pub fn finish(mut self) -> (ProviderResult, Option<String>) {
        if !self.line.is_empty() {
            self.fail("incomplete Kiro ACP JSONL message");
        }
        if !self.terminal || self.request != 3 || !self.dispatched {
            self.fail("Kiro ACP ended without a correlated completed prompt");
        }
        if !self.answer_seen {
            self.fail("Kiro ACP turn contained no final assistant text");
        }
        self.result.bound();
        (self.result, self.error)
    }
}

fn valid_identifier(text: &str) -> bool {
    !text.is_empty() && text.len() <= 256 && !text.chars().any(char::is_control)
}
fn identifier(value: &Value) -> Result<&str, String> {
    value
        .as_str()
        .filter(|text| valid_identifier(text))
        .ok_or_else(|| "Kiro ACP requires a bounded nonempty string identifier".into())
}
fn check_kiro_meta(value: &Value) -> Result<(), String> {
    let Some(meta) = value.get("_meta").filter(|meta| !meta.is_null()) else {
        return Ok(());
    };
    let meta = meta.as_object().ok_or("malformed Kiro ACP metadata")?;
    let Some(kiro) = meta.get("kiro").filter(|kiro| !kiro.is_null()) else {
        return Ok(());
    };
    let kiro = kiro.as_object().ok_or("malformed Kiro ACP metadata")?;
    if kiro.contains_key("failureReason") {
        return Err("Kiro ACP reported a native failureReason".into());
    }
    if let Some(replay) = kiro.get("replay")
        && replay.as_bool() != Some(false)
    {
        return Err("Kiro ACP fresh session received replayed or malformed replay state".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::NativeProfile;

    fn driver(permission: Option<NativePermission>) -> Driver {
        let profile: NativeProfile = serde_json::from_value(json!({
            "provider":"kiro_cli","program":"/bin/true","model":"requested-model",
            "native_permission":permission
        }))
        .unwrap();
        Driver::new(
            ProviderResult::new(&profile, Some("2.28.0".into())),
            Start {
                cwd: "/tmp/task/repository".into(),
                prompt: "literal prompt\n✓".into(),
                model: profile.model,
                native_permission: permission,
            },
        )
    }
    fn feed(driver: &mut Driver, event: Value) {
        let mut bytes = serde_json::to_vec(&event).unwrap();
        bytes.push(b'\n');
        for chunk in bytes.chunks(7) {
            driver.feed(chunk);
        }
    }
    fn dispatch(driver: &mut Driver) -> Value {
        let pending = driver.take_pending();
        assert!(!pending.is_empty());
        let message = serde_json::from_slice(&pending).unwrap();
        driver.mark_input_complete();
        assert!(driver.failure().is_none());
        message
    }
    fn response(driver: &mut Driver, id: u64, result: Value) {
        feed(driver, json!({"jsonrpc":"2.0","id":id,"result":result}));
    }
    fn initialized(driver: &mut Driver) {
        let request = dispatch(driver);
        assert_eq!(request["method"], "initialize");
        response(driver, 1, json!({"protocolVersion":1}));
    }
    fn active(driver: &mut Driver) {
        initialized(driver);
        assert_eq!(dispatch(driver)["method"], "session/new");
        response(driver, 2, json!({"sessionId":"session-1"}));
        assert_eq!(dispatch(driver)["method"], "session/prompt");
    }
    fn update(driver: &mut Driver, session: &str, update: Value) {
        feed(
            driver,
            json!({"jsonrpc":"2.0","method":"session/update","params":{
                "sessionId":session,"update":update
            }}),
        );
    }
    fn answer(driver: &mut Driver, text: &str) {
        update(
            driver,
            "session-1",
            json!({
                "sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}
            }),
        );
    }
    fn completed(driver: &mut Driver) {
        response(driver, 3, json!({"stopReason":"end_turn"}));
    }
    fn assert_failed(driver: Driver, fragment: &str) {
        let (_, error) = driver.finish();
        assert!(
            error
                .as_deref()
                .is_some_and(|error| error.contains(fragment)),
            "{error:?}"
        );
    }

    #[test]
    fn canonical_fresh_transcript_and_unknown_usage() {
        let mut d = driver(None);
        let init = dispatch(&mut d);
        assert_eq!(init["jsonrpc"], "2.0");
        assert_eq!(init["params"]["protocolVersion"], 1);
        assert_eq!(
            init["params"]["clientCapabilities"],
            json!({
                "fs":{"readTextFile":false,"writeTextFile":false},"terminal":false
            })
        );
        response(
            &mut d,
            1,
            json!({"protocolVersion":1,"authMethods":[{"id":"cli"}]}),
        );
        let new_session = dispatch(&mut d);
        assert_eq!(new_session["params"]["cwd"], "/tmp/task/repository");
        assert_eq!(new_session["params"]["mcpServers"], json!([]));
        assert_eq!(
            new_session["params"]["_meta"]["kiro"],
            json!({"modelId":"requested-model"})
        );
        response(
            &mut d,
            2,
            json!({"sessionId":"session-1","configOptions":[{
                "id":"model","type":"select","currentValue":"configured-model"
            }]}),
        );
        let prompt = dispatch(&mut d);
        assert_eq!(
            prompt["params"],
            json!({"sessionId":"session-1","prompt":[{
                "type":"text","text":"literal prompt\n✓"
            }]})
        );
        answer(&mut d, "done ");
        answer(&mut d, "✓");
        completed(&mut d);
        assert!(d.stopped());
        let (result, error) = d.finish();
        assert!(error.is_none(), "{error:?}");
        assert_eq!(result.summary, "done ✓");
        assert!(!result.summary_truncated);
        assert_eq!(result.terminal_reason.as_deref(), Some("end_turn"));
        assert_eq!(result.session_id.as_deref(), Some("session-1"));
        assert_eq!(result.requested_model.as_deref(), Some("requested-model"));
        assert!(result.reported_model.is_none());
        let selection = result.selection.unwrap();
        assert_eq!(
            selection.session_settings.unwrap().model.as_deref(),
            Some("configured-model")
        );
        assert_eq!(selection.verification.model, "session_reported");
        assert!(selection.observed.model.is_none());
        assert!(result.usage.input_tokens.is_none());
        assert!(result.usage.output_tokens.is_none());
        assert!(result.usage.total_cost_usd.is_none());
        assert!(result.usage.num_turns.is_none());
    }

    #[test]
    fn only_explicit_workspace_write_selects_policy_preset() {
        let mut d = driver(Some(NativePermission::KiroWorkspaceWrite));
        initialized(&mut d);
        let new_session = dispatch(&mut d);
        assert_eq!(
            new_session["params"]["_meta"]["kiro"]["policyPreset"],
            json!(["edit-workspace"])
        );
        assert!(new_session["params"].get("additionalDirectories").is_none());
        assert!(new_session["params"].get("policyPreset").is_none());
        let base = driver(None);
        let mut start = base.start;
        start.model = None;
        let mut no_overrides = Driver::new(base.result, start);
        initialized(&mut no_overrides);
        assert!(dispatch(&mut no_overrides)["params"].get("_meta").is_none());
    }

    #[test]
    fn start_rejects_resume_and_unsupported_permissions() {
        for field in ["resume", "checkpoint", "effort", "read_only"] {
            let mut start = json!({"cwd":"/tmp/repo","prompt":"hello","model":null});
            start[field] = json!("unsupported");
            assert!(serde_json::from_value::<Start>(start).is_err());
        }
        let d = driver(Some(NativePermission::CodexFullAccess));
        assert_failed(d, "incompatible");
    }

    #[test]
    fn dispatch_acknowledgement_is_required_for_each_response() {
        for stage in 1..=3 {
            for take_only in [false, true] {
                let mut d = driver(None);
                if stage >= 2 {
                    initialized(&mut d);
                }
                if stage >= 3 {
                    dispatch(&mut d);
                    response(&mut d, 2, json!({"sessionId":"session-1"}));
                }
                if take_only {
                    assert!(!d.take_pending().is_empty());
                }
                response(
                    &mut d,
                    stage,
                    match stage {
                        1 => json!({"protocolVersion":1}),
                        2 => json!({"sessionId":"session-1"}),
                        _ => json!({"stopReason":"end_turn"}),
                    },
                );
                assert!(d.take_pending().is_empty());
                assert_failed(d, "before request dispatch");
            }
        }
        let mut d = driver(None);
        d.mark_input_complete();
        assert_failed(d, "out of sequence");
        let mut d = driver(None);
        dispatch(&mut d);
        d.mark_input_complete();
        assert_failed(d, "out of sequence");
    }

    #[test]
    fn coalesced_future_response_cannot_release_prompt() {
        let mut d = driver(None);
        dispatch(&mut d);
        d.feed(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":1}}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"sessionId\":\"session-1\"}}\n");
        assert!(d.take_pending().is_empty());
        assert_failed(d, "before request dispatch");
    }

    #[test]
    fn unexpected_duplicate_and_noninteger_response_ids_fail() {
        for id in [json!(0), json!(2), json!("1"), json!(1.0), json!(null)] {
            let mut d = driver(None);
            dispatch(&mut d);
            feed(
                &mut d,
                json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1}}),
            );
            assert_failed(d, "response ID");
        }
        let mut d = driver(None);
        initialized(&mut d);
        response(&mut d, 1, json!({"protocolVersion":1}));
        assert_failed(d, "response ID");
        let mut d = driver(None);
        active(&mut d);
        answer(&mut d, "done");
        completed(&mut d);
        completed(&mut d);
        assert_failed(d, "response ID");
    }

    #[test]
    fn malformed_rpc_and_utf8_are_rejected() {
        let cases: &[&[u8]] = &[
            b"not json\n",
            b"[]\n",
            b"{}\n",
            b"{\"jsonrpc\":\"1.0\",\"id\":1,\"result\":{\"protocolVersion\":1}}\n",
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"id\":1,\"result\":{\"protocolVersion\":1}}\n",
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":null,\"result\":{}}\n",
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"params\":{},\"result\":{}}\n",
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null}\n",
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"1\"}}\n",
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":2}}\n",
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"bad\":\"\xff\"}}\n",
        ];
        for bytes in cases {
            let mut d = driver(None);
            dispatch(&mut d);
            d.feed(bytes);
            assert!(d.failure().is_some(), "{bytes:?}");
        }
    }

    #[test]
    fn partial_tail_and_oversized_lines_cannot_succeed() {
        for tail in [b"{".as_slice(), b" ".as_slice(), b"\xc3".as_slice()] {
            let mut d = driver(None);
            active(&mut d);
            answer(&mut d, "done");
            completed(&mut d);
            d.feed(tail);
            assert_failed(d, "incomplete");
        }
        let mut d = driver(None);
        d.feed(&vec![b' '; MAX_PROTOCOL_LINE + 1]);
        assert_failed(d, "64 KiB");
    }

    #[test]
    fn permissions_always_cancel_without_persisting_and_failure_is_sticky() {
        for options in [
            json!([]),
            json!([
                {"optionId":"allow","kind":"allow_once"},
                {"optionId":"remember","kind":"allow_always"},
                {"optionId":"deny","kind":"reject_once"}
            ]),
        ] {
            let mut d = driver(None);
            active(&mut d);
            feed(
                &mut d,
                json!({"jsonrpc":"2.0","id":"server-1","method":"session/request_permission","params":{
                    "sessionId":"session-1","options":options,
                    "toolCall":{"toolCallId":"tool-1","title":"Sensitive action"}
                }}),
            );
            let reply: Value = serde_json::from_slice(&d.take_pending()).unwrap();
            assert_eq!(
                reply,
                json!({"jsonrpc":"2.0","id":"server-1","result":{"outcome":{"outcome":"cancelled"}}})
            );
            answer(&mut d, "should not become success");
            completed(&mut d);
            assert_failed(d, "permission request denied");
        }
    }

    #[test]
    fn all_authentication_file_terminal_and_unknown_callbacks_are_denied() {
        for method in [
            "_kiro/auth/getAccessToken",
            "fs/read_text_file",
            "fs/write_text_file",
            "terminal/create",
            "_kiro/openExternalUrl",
            "_kiro/hooks/executeHook",
            "arbitrary/method",
        ] {
            let mut d = driver(None);
            initialized(&mut d);
            feed(
                &mut d,
                json!({"jsonrpc":"2.0","id":42,"method":method,"params":{"secret":"never echo"}}),
            );
            let pending = d.take_pending();
            assert!(!String::from_utf8_lossy(&pending).contains("never echo"));
            let reply: Value = serde_json::from_slice(&pending).unwrap();
            assert_eq!(reply["error"]["code"], -32601);
            assert!(reply.get("result").is_none());
            assert_failed(d, "callback denied");
        }
    }

    #[test]
    fn startup_metadata_must_match_new_session_identity() {
        for returned in ["session-1", "foreign"] {
            let mut d = driver(None);
            initialized(&mut d);
            dispatch(&mut d);
            update(
                &mut d,
                "session-1",
                json!({"sessionUpdate":"available_commands_update","availableCommands":[]}),
            );
            response(&mut d, 2, json!({"sessionId":returned}));
            if returned == "foreign" {
                assert_failed(d, "disagrees");
            } else {
                dispatch(&mut d);
                answer(&mut d, "done");
                completed(&mut d);
                assert!(d.finish().1.is_none());
            }
        }
    }

    #[test]
    fn foreign_session_missing_session_and_predispatch_text_fail() {
        for session in [json!("foreign"), json!(null), json!(""), json!("a\nb")] {
            let mut d = driver(None);
            active(&mut d);
            feed(
                &mut d,
                json!({"jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":session,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"done"}}
                }}),
            );
            assert!(d.finish().1.is_some());
        }
        for before_new_response in [false, true] {
            let mut d = driver(None);
            initialized(&mut d);
            dispatch(&mut d);
            if !before_new_response {
                response(&mut d, 2, json!({"sessionId":"session-1"}));
                d.take_pending();
            }
            answer(&mut d, "premature");
            assert_failed(d, "before prompt dispatch");
        }
    }

    #[test]
    fn result_errors_failure_reason_and_missing_answer_never_succeed() {
        for reason in [
            "cancelled",
            "max_tokens",
            "max_turn_requests",
            "refusal",
            "unknown",
        ] {
            let mut d = driver(None);
            active(&mut d);
            answer(&mut d, "partial");
            response(&mut d, 3, json!({"stopReason":reason}));
            assert_failed(d, "without end_turn");
        }
        for failure in [
            json!("denied"),
            json!("cancelled"),
            json!("error"),
            json!(false),
            json!(null),
        ] {
            let mut d = driver(None);
            active(&mut d);
            answer(&mut d, "partial");
            response(
                &mut d,
                3,
                json!({"stopReason":"end_turn","_meta":{"kiro":{"failureReason":failure}}}),
            );
            assert_failed(d, "failureReason");
        }
        for stage in 1..=3 {
            let mut d = driver(None);
            if stage == 1 {
                dispatch(&mut d);
            } else if stage == 2 {
                initialized(&mut d);
                dispatch(&mut d);
            } else {
                active(&mut d);
                answer(&mut d, "partial");
            }
            feed(
                &mut d,
                json!({"jsonrpc":"2.0","id":stage,"error":{"code":-32000,"message":"credentials must not echo"}}),
            );
            assert_failed(d, "request failed");
        }
        for text in ["", " \n\t"] {
            let mut d = driver(None);
            active(&mut d);
            answer(&mut d, text);
            completed(&mut d);
            assert_failed(d, "no final assistant text");
        }
    }

    #[test]
    fn turn_end_and_credit_usage_are_not_completion_or_usd() {
        let mut d = driver(None);
        active(&mut d);
        answer(&mut d, "in progress");
        for kind in ["turn_end", "turn_completion"] {
            update(
                &mut d,
                "session-1",
                json!({"sessionUpdate":"session_info_update","_meta":{"kiro":{
                    "kind":kind,"promptTurnSummaries":[{"unit":"credit","usage":1.5}],"usagePercentage":42
                }}}),
            );
        }
        update(&mut d, "session-1", json!({"sessionUpdate":"TurnEnd"}));
        assert!(!d.stopped());
        assert!(d.result.usage.total_cost_usd.is_none());
        assert_failed(d, "without a correlated completed prompt");
    }

    #[test]
    fn final_answer_is_bounded_utf8_prefix_and_resets_across_tools_and_messages() {
        let mut d = driver(None);
        active(&mut d);
        answer(&mut d, "Interim text");
        update(
            &mut d,
            "session-1",
            json!({"sessionUpdate":"tool_call","toolCallId":"tool-1","status":"pending"}),
        );
        update(
            &mut d,
            "session-1",
            json!({"sessionUpdate":"tool_call_update","toolCallId":"tool-1","status":"completed"}),
        );
        let text = format!("{}✓", "a".repeat(MAX_SUMMARY - 1));
        answer(&mut d, &text);
        answer(&mut d, "must not fill the truncated UTF-8 gap");
        assert_eq!(d.result.summary, "a".repeat(MAX_SUMMARY - 1));
        assert!(d.result.summary_truncated);
        update(
            &mut d,
            "session-1",
            json!({"sessionUpdate":"agent_message_chunk","messageId":"final-message","content":{"type":"text","text":"final ✓"}}),
        );
        completed(&mut d);
        let (result, error) = d.finish();
        assert!(error.is_none());
        assert_eq!(result.summary, "final ✓");
        assert!(!result.summary_truncated);
        let mut d = driver(None);
        active(&mut d);
        for _ in 0..100 {
            answer(&mut d, &"✓".repeat(2000));
        }
        completed(&mut d);
        let (result, error) = d.finish();
        assert!(error.is_none());
        assert_eq!(result.summary.len(), 4095);
        assert!(result.summary_truncated);
    }

    #[test]
    fn commentary_before_last_tool_is_not_final_answer() {
        let mut d = driver(None);
        active(&mut d);
        answer(&mut d, "I will edit that");
        update(
            &mut d,
            "session-1",
            json!({"sessionUpdate":"tool_call","toolCallId":"tool-1"}),
        );
        completed(&mut d);
        assert_failed(d, "no final assistant text");
    }

    #[test]
    fn unknown_notifications_are_bounded_and_late_updates_fail() {
        let mut d = driver(None);
        active(&mut d);
        for _ in 0..MAX_IGNORED_NOTIFICATIONS {
            feed(
                &mut d,
                json!({"jsonrpc":"2.0","method":"_kiro/future","params":{}}),
            );
        }
        assert!(d.failure().is_none());
        feed(
            &mut d,
            json!({"jsonrpc":"2.0","method":"_kiro/future","params":{}}),
        );
        assert_failed(d, "notification budget");
        let mut d = driver(None);
        active(&mut d);
        answer(&mut d, "done");
        completed(&mut d);
        answer(&mut d, "late text");
        assert_failed(d, "after prompt completion");
        let mut d = driver(None);
        active(&mut d);
        answer(&mut d, "done");
        completed(&mut d);
        feed(
            &mut d,
            json!({"jsonrpc":"2.0","method":"_kiro/future","params":{"sessionId":"session-1"}}),
        );
        assert_failed(d, "after prompt completion");
    }

    #[test]
    fn malformed_critical_updates_and_fresh_session_replay_fail() {
        for invalid in [
            json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":false}}),
            json!({"sessionUpdate":"tool_call","toolCallId":"tool-1","status":"unknown"}),
            json!({"sessionUpdate":"config_option_update","configOptions":{}}),
            json!({"sessionUpdate":"config_option_update","configOptions":[{"id":"model","type":"select","currentValue":null}]}),
            json!({"sessionUpdate":"session_info_update","_meta":{"kiro":{"failureReason":"denied"}}}),
            json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"replay"},"_meta":{"kiro":{"replay":true}}}),
        ] {
            let mut d = driver(None);
            active(&mut d);
            update(&mut d, "session-1", invalid);
            assert!(d.finish().1.is_some());
        }
    }

    #[test]
    fn outbound_and_config_budgets_fail_before_prompt() {
        let original = driver(None);
        let mut start = original.start;
        start.prompt = "x".repeat(MAX_PENDING);
        let mut d = Driver::new(original.result, start);
        initialized(&mut d);
        dispatch(&mut d);
        response(&mut d, 2, json!({"sessionId":"session-1"}));
        assert!(d.take_pending().is_empty());
        assert_failed(d, "outbound protocol budget");
        for options in [
            json!([{"id":"model","type":"select","currentValue":"a"},{"id":"model","type":"select","currentValue":"b"}]),
            json!(vec![json!({"id":"future"}); MAX_CONFIG_OPTIONS + 1]),
        ] {
            let mut d = driver(None);
            initialized(&mut d);
            dispatch(&mut d);
            response(
                &mut d,
                2,
                json!({"sessionId":"session-1","configOptions":options}),
            );
            assert!(d.take_pending().is_empty());
            assert!(d.failure().is_some());
        }
    }

    #[test]
    fn configuration_updates_replace_only_session_evidence() {
        let mut d = driver(None);
        active(&mut d);
        update(
            &mut d,
            "session-1",
            json!({"sessionUpdate":"config_option_update","configOptions":[
                {"id":"model","type":"select","currentValue":"configured"},
                {"id":"effortLevel","type":"select","currentValue":"high"}
            ]}),
        );
        let evidence = d.result.selection.as_ref().unwrap();
        let settings = evidence.session_settings.as_ref().unwrap();
        assert_eq!(settings.model.as_deref(), Some("configured"));
        assert_eq!(settings.effort.as_deref(), Some("high"));
        assert!(settings.sandbox.is_none());
        assert!(settings.permission_mode.is_none());
        assert_eq!(evidence.verification.permission, "unknown");
        assert!(evidence.observed.model.is_none());
        assert!(d.result.reported_model.is_none());
        update(
            &mut d,
            "session-1",
            json!({"sessionUpdate":"config_option_update","configOptions":[]}),
        );
        assert!(
            d.result
                .selection
                .as_ref()
                .unwrap()
                .session_settings
                .as_ref()
                .unwrap()
                .model
                .is_none()
        );
        answer(&mut d, "done");
        completed(&mut d);
        assert!(d.finish().1.is_none());
    }
}
