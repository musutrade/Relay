//! Bounded Codex app-server stdio client. The supervisor owns the process tree;
//! this driver owns only request correlation and one explicitly identified turn.
use crate::providers::{MAX_PROTOCOL_LINE, ProviderResult};
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
    pub resume: Option<String>,
}

pub(crate) struct Driver {
    result: ProviderResult,
    start: Start,
    line: Vec<u8>,
    pending: Vec<u8>,
    request: u64,
    turn: Option<String>,
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
            terminal: false,
            answer_seen: false,
            error: None,
        };
        driver.send(json!({"id":1,"method":"initialize","params":{
            "clientInfo":{"name":"relay","version":env!("CARGO_PKG_VERSION")},
            "capabilities":{"experimentalApi":false}
        }}));
        driver
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
                // dispatch dynamic tools. Unsupported requests are answered, then stopped.
                self.send(json!({"id":id,"error":{"code":-32601,"message":"Relay does not authorize server requests"}}));
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
                    let mut params = json!({"cwd":self.start.cwd,"approvalPolicy":"never","sandbox":"workspace-write","model":self.start.model});
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
                    if self
                        .start
                        .resume
                        .as_ref()
                        .is_some_and(|expected| expected != &id)
                    {
                        return Err("app-server resumed a different thread".into());
                    }
                    self.result.session_id = Some(id.clone());
                    self.result.reported_model = result
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    self.result.bound();
                    self.send(json!({"id":3,"method":"turn/start","params":{
                        "threadId":id,"input":[{"type":"text","text":self.start.prompt}],
                        "cwd":self.start.cwd,"approvalPolicy":"never","model":self.start.model,
                        "effort":self.start.effort,
                        "sandboxPolicy":{"type":"workspaceWrite","writableRoots":[self.start.cwd],
                            "networkAccess":false,"excludeTmpdirEnvVar":false,"excludeSlashTmp":false}
                    }}));
                }
                3 => {
                    let id = id_field(&result["turn"], "id")?;
                    if self.turn.as_ref().is_some_and(|expected| expected != &id) {
                        return Err("app-server turn response changed the active turn".into());
                    }
                    self.turn = Some(id);
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
            if self.turn.is_none() && method == "turn/started" && self.request == 3 {
                self.turn = Some(turn.clone());
            }
            if self.turn.as_ref() != Some(&turn) {
                return Err("app-server notification belongs to another turn".into());
            }
        }
        match method {
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
                if item["status"] == "declined" {
                    return Err("app-server tool permission denied".into());
                }
            }
            "thread/tokenUsage/updated" => {
                let usage = &params["tokenUsage"]["last"];
                self.result.usage.input_tokens = number(usage, "inputTokens")?;
                self.result.usage.cached_input_tokens = number(usage, "cachedInputTokens")?;
                self.result.usage.output_tokens = number(usage, "outputTokens")?;
                self.result.usage.reasoning_output_tokens = number(usage, "reasoningOutputTokens")?;
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
                resume: resume.map(str::to_owned),
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
    fn start(driver: &mut Driver) {
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
        feed(driver, json!({"id":3,"result":{"turn":{"id":"turn-1"}}}));
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
            assert!(response.contains("-32601"));
            assert!(!response.contains("accept"));
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
}
