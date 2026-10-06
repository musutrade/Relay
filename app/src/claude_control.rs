//! One bounded initialize exchange inside an already-authorized Claude task.
//! The outer JSONL parser enforces its 64 KiB line limit. The supervisor owns
//! the ten-second initialization deadline, stdin writes, and process lifetime.
//! This driver never starts a process or requests a second inference turn.
use crate::capabilities::{EffortCapability, ModelCapability};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::{self, Write};

const INITIALIZE_ID: &str = "relay-initialize-1";
// JSON can expand each input byte to a six-byte escape. The inbound 64 KiB
// JSONL limit does not reduce the existing 64 KiB task-prompt contract.
const MAX_PENDING: usize = 6 * crate::host::MAX_PHASE_INPUT + 1024;
const MAX_MODELS: usize = 256;
const MAX_CATALOG_BYTES: usize = 256 * 1024;
const MAX_FIELD: usize = 256;
const MAX_DESCRIPTION: usize = 1024;
const MAX_EFFORTS: usize = 32;
const SOURCE: &str = "claude_cli:initialize/models";
const DENIAL: &str = "Relay does not authorize server control requests";

pub(crate) struct Driver {
    prompt: Option<String>,
    pending: Vec<u8>,
    pending_denial: bool,
    initialize_sent: bool,
    initialized: bool,
    models: Option<Vec<ModelCapability>>,
    error: Option<&'static str>,
}

impl Driver {
    pub(crate) fn new(prompt: String) -> Self {
        let mut driver = Self {
            prompt: None,
            pending: Vec::new(),
            pending_denial: false,
            initialize_sent: false,
            initialized: false,
            models: None,
            error: None,
        };
        if prompt.len() > crate::host::MAX_PHASE_INPUT {
            driver.fail("Claude task prompt exceeds its byte budget");
            return driver;
        }
        driver.prompt = Some(prompt);
        // Official Python SDK Query.initialize / _send_control_request envelope.
        // No hooks, plugins, client capabilities, permissions or account queries.
        driver.pending = encode_line(&json!({
            "type": "control_request",
            "request_id": INITIALIZE_ID,
            "request": {"subtype": "initialize", "hooks": null}
        }))
        .expect("fixed initialize request is bounded");
        driver
    }

    pub(crate) fn pending(&mut self) -> Vec<u8> {
        if !self.pending.is_empty() && self.error.is_none() && !self.initialized {
            self.initialize_sent = true;
        }
        self.pending_denial = false;
        std::mem::take(&mut self.pending)
    }

    pub(crate) fn initialized(&self) -> bool {
        self.initialized
    }

    /// No further task input is owed. The caller must also finish writing every
    /// byte previously returned by `pending` before closing stdin. On failure,
    /// discard any caller-buffered task prompt rather than continuing to write it.
    pub(crate) fn input_done(&self) -> bool {
        (self.initialized || self.error.is_some()) && self.pending.is_empty()
    }

    pub(crate) fn models(&self) -> Option<Vec<ModelCapability>> {
        self.error.is_none().then(|| self.models.clone()).flatten()
    }

    pub(crate) fn failure(&self) -> Option<&str> {
        self.error
    }

    /// Cancel unsent task input after a sticky outer-parser error. A bounded,
    /// static denial may still be flushed, but neither initialize nor a prompt.
    pub(crate) fn abort(&mut self) {
        self.fail("Claude task control exchange was aborted");
    }

    fn fail(&mut self, reason: &'static str) {
        self.error.get_or_insert(reason);
        self.prompt = None;
        self.models = None;
        if !self.pending_denial {
            self.pending.clear();
        }
    }

    /// Only control_response, control_request and control_cancel_request belong
    /// here. Normal provider events remain the outer parser's responsibility.
    pub(crate) fn event(&mut self, event: &Value) -> Result<(), String> {
        if let Some(error) = self.error {
            return Err(error.into());
        }
        if let Err(reason) = self.control_event(event) {
            self.fail(reason);
            return Err(reason.into());
        }
        Ok(())
    }

    fn control_event(&mut self, event: &Value) -> Result<(), &'static str> {
        match event.get("type").and_then(Value::as_str) {
            Some("control_request") => {
                self.deny(event);
                Err("Claude requested approval, input, or an unsupported client action")
            }
            Some("control_cancel_request") => {
                // Relay never accepts an incoming request, so there is no
                // outstanding client action whose cancellation could be valid.
                Err("unexpected Claude control cancellation")
            }
            Some("control_response") => self.initialize_response(event),
            _ => Ok(()),
        }
    }

    fn deny(&mut self, request: &Value) {
        self.pending.clear();
        self.pending_denial = false;
        // Reflect only a short scalar correlation ID, never request parameters,
        // provider error messages, credentials or callback payloads.
        if let Some(id) = request
            .get("request_id")
            .and_then(Value::as_str)
            .filter(|id| valid_text(id, MAX_FIELD))
        {
            self.pending = encode_line(&json!({
                "type": "control_response",
                "response": {"subtype": "error", "request_id": id, "error": DENIAL}
            }))
            .expect("bounded denial request ID");
            self.pending_denial = true;
        }
    }

    fn initialize_response(&mut self, event: &Value) -> Result<(), &'static str> {
        if self.initialized {
            return Err("duplicate Claude initialize response");
        }
        if !self.initialize_sent {
            return Err("Claude responded before initialize was sent");
        }
        let response = event
            .get("response")
            .filter(|value| value.is_object())
            .ok_or("Claude control response requires an object envelope")?;
        if response.get("request_id").and_then(Value::as_str) != Some(INITIALIZE_ID) {
            return Err("unexpected Claude control response ID");
        }
        if response.get("subtype").and_then(Value::as_str) != Some("success")
            || response.get("error").is_some_and(|value| !value.is_null())
        {
            return Err("Claude initialize was rejected or malformed");
        }
        let payload = response
            .get("response")
            .filter(|value| value.is_object())
            .ok_or("Claude initialize response requires an object payload")?;
        for name in [
            "pending_permission_requests",
            "pending_user_dialog_requests",
        ] {
            match response.get(name) {
                // This driver is gated to CLI >=2.1.291; the native contract
                // always includes both arrays from 2.1.268 onward.
                None => return Err("Claude initialize omitted pending client-action metadata"),
                Some(Value::Array(items)) if items.is_empty() => {}
                Some(Value::Array(items)) => {
                    self.deny(&items[0]);
                    return Err("Claude initialize requires an unsupported client action");
                }
                _ => return Err("Claude initialize has malformed pending requests"),
            }
        }
        if [response, payload].iter().any(|value| {
            value.get("session_state").and_then(Value::as_str) == Some("requires_action")
        }) {
            return Err("Claude initialize requires an unsupported client action");
        }
        // Catalog metadata is optional evidence, not an execution prerequisite.
        // Reject the whole catalog on incomplete, contradictory or oversized
        // metadata, but a structurally successful initialization can still run.
        self.models = parse_models(payload);
        let prompt = self
            .prompt
            .take()
            .ok_or("Claude task prompt is no longer available")?;
        self.pending = encode_line(&json!({
            "type": "user",
            "message": {"role": "user", "content": prompt},
            "parent_tool_use_id": null
        }))
        .ok_or("Claude outbound task input exceeds its byte budget")?;
        self.initialized = true;
        Ok(())
    }
}

fn valid_text(text: &str, limit: usize) -> bool {
    !text.trim().is_empty() && text.len() <= limit && !text.chars().any(char::is_control)
}

fn text_field(value: &Value, name: &str, limit: usize) -> Option<Option<String>> {
    match value.get(name) {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(text))
            if valid_text(text, limit) || (name == "description" && text.is_empty()) =>
        {
            Some(Some(text.clone()))
        }
        _ => None,
    }
}

fn bool_field(value: &Value, name: &str) -> Option<Option<bool>> {
    match value.get(name) {
        None | Some(Value::Null) => Some(None),
        Some(Value::Bool(flag)) => Some(Some(*flag)),
        _ => None,
    }
}

fn parse_model(value: &Value) -> Option<ModelCapability> {
    value.as_object()?;
    let selection = text_field(value, "value", MAX_FIELD)??;
    let supports_effort = bool_field(value, "supportsEffort")?;
    let supported_efforts = match value.get("supportedEffortLevels") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) if items.len() <= MAX_EFFORTS => {
            if supports_effort == Some(false) && !items.is_empty() {
                return None;
            }
            let mut names = BTreeSet::new();
            let mut efforts = Vec::with_capacity(items.len());
            for item in items {
                let effort = item.as_str().filter(|text| valid_text(text, MAX_FIELD))?;
                if !names.insert(effort) {
                    return None;
                }
                efforts.push(EffortCapability {
                    effort: effort.into(),
                    description: None,
                });
            }
            Some(efforts)
        }
        _ => return None,
    };
    Some(ModelCapability {
        id: selection.clone(),
        model: selection,
        display_name: text_field(value, "displayName", MAX_FIELD)??,
        description: Some(text_field(value, "description", MAX_DESCRIPTION)??),
        default_effort: None,
        supported_efforts,
        is_default: None,
        hidden: None,
        source: SOURCE.into(),
        resolved_model: text_field(value, "resolvedModel", MAX_FIELD)?,
        supports_effort,
        supports_adaptive_thinking: bool_field(value, "supportsAdaptiveThinking")?,
        supports_fast_mode: bool_field(value, "supportsFastMode")?,
        supports_auto_mode: bool_field(value, "supportsAutoMode")?,
    })
}

fn parse_models(payload: &Value) -> Option<Vec<ModelCapability>> {
    let values = payload.get("models")?.as_array()?;
    if values.len() > MAX_MODELS || !fits_json(values, MAX_CATALOG_BYTES) {
        return None;
    }
    let mut ids = BTreeSet::new();
    let mut models = Vec::with_capacity(values.len());
    for value in values {
        let model = parse_model(value)?;
        if !ids.insert(model.id.clone()) {
            return None;
        }
        models.push(model);
    }
    fits_json(&models, MAX_CATALOG_BYTES).then_some(models)
}

/// Count serialization without allocating an unbounded copy of provider data.
fn fits_json(value: &impl Serialize, limit: usize) -> bool {
    struct Budget(usize);
    impl Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.0 {
                return Err(io::Error::other("JSON byte budget exceeded"));
            }
            self.0 -= bytes.len();
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(limit), value).is_ok()
}

fn encode_line(value: &Value) -> Option<Vec<u8>> {
    if !fits_json(value, MAX_PENDING - 1) {
        return None;
    }
    let mut bytes = serde_json::to_vec(value).ok()?;
    bytes.push(b'\n');
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str) -> Value {
        json!({"value": name, "displayName": "Observed model", "description": ""})
    }

    fn response(payload: Value) -> Value {
        json!({"type": "control_response", "response": {
            "subtype": "success", "request_id": INITIALIZE_ID, "response": payload,
            "pending_permission_requests": [], "pending_user_dialog_requests": []
        }})
    }

    fn driver() -> Driver {
        let mut driver = Driver::new("One task prompt\nwith Unicode 界".into());
        let initialize = serde_json::from_slice::<Value>(&driver.pending()).unwrap();
        assert_eq!(
            initialize,
            json!({"type": "control_request", "request_id": INITIALIZE_ID,
            "request": {"subtype": "initialize", "hooks": null}})
        );
        driver
    }

    #[test]
    fn initialize_then_exactly_one_prompt_and_close_after_drain() {
        let mut driver = driver();
        assert!(!driver.initialized());
        assert!(!driver.input_done());
        assert!(driver.pending().is_empty());
        driver
            .event(&response(json!({"models": [row("selection-alias")]})))
            .unwrap();
        assert!(driver.initialized());
        assert!(!driver.input_done());
        let bytes = driver.pending();
        assert_eq!(bytes.iter().filter(|&&byte| byte == b'\n').count(), 1);
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({
                "type": "user", "message": {"role": "user", "content": "One task prompt\nwith Unicode 界"},
                "parent_tool_use_id": null
            })
        );
        assert!(driver.input_done());
        assert!(driver.pending().is_empty());
        assert_eq!(driver.models().unwrap()[0].model, "selection-alias");
    }

    #[test]
    fn missing_pending_action_metadata_is_not_assumed_empty() {
        for field in [
            "pending_permission_requests",
            "pending_user_dialog_requests",
        ] {
            let mut driver = driver();
            let mut value = response(json!({"models":[]}));
            value["response"].as_object_mut().unwrap().remove(field);
            assert!(driver.event(&value).is_err());
            assert!(driver.pending().is_empty());
            assert!(driver.models().is_none());
        }
    }

    #[test]
    fn preserves_observed_values_without_inventing_optional_metadata() {
        let mut model = row("alias");
        model["resolvedModel"] = json!("canonical-wire-name");
        model["supportsEffort"] = json!(true);
        model["supportedEffortLevels"] = json!(["future-effort", "high"]);
        model["supportsAdaptiveThinking"] = json!(false);
        model["supportsFastMode"] = json!(true);
        let mut driver = driver();
        driver
            .event(&response(json!({"models": [model, row("another-model")]})))
            .unwrap();
        let models = driver.models().unwrap();
        assert_eq!(models[0].id, "alias");
        assert_eq!(models[0].model, "alias");
        assert_eq!(
            models[0].resolved_model.as_deref(),
            Some("canonical-wire-name")
        );
        assert_eq!(
            models[0].supported_efforts.as_ref().unwrap()[0].effort,
            "future-effort"
        );
        assert_eq!(models[0].supports_effort, Some(true));
        assert_eq!(models[0].supports_adaptive_thinking, Some(false));
        assert_eq!(models[0].supports_fast_mode, Some(true));
        assert_eq!(models[0].supports_auto_mode, None);
        assert_eq!(models[1].supports_effort, None);
        assert!(models[1].supported_efforts.is_none());
        assert!(models.iter().all(|model| model.default_effort.is_none()
            && model.is_default.is_none()
            && model.hidden.is_none()
            && model.source == SOURCE));
    }

    #[test]
    fn absent_effort_list_is_unknown_even_with_an_observed_support_flag() {
        for flag in [false, true] {
            let mut model = row("alias");
            model["supportsEffort"] = json!(flag);
            let model = parse_model(&model).unwrap();
            assert_eq!(model.supports_effort, Some(flag));
            assert!(model.supported_efforts.is_none());
        }
    }

    #[test]
    fn bad_catalog_is_discarded_but_successful_initialize_runs_task() {
        let invalid_models = [
            Value::Null,
            json!({}),
            json!([{}]),
            json!([{"value": "alias", "displayName": "Alias"}]),
            json!([row("duplicate"), row("duplicate")]),
            json!([{"value":"alias", "displayName":"Alias", "description":"",
                "supportsEffort":false, "supportedEffortLevels":["high"]}]),
            json!([{"value":"alias", "displayName":"Alias", "description":"",
                "supportedEffortLevels":["high", "high"]}]),
        ];
        for models in invalid_models {
            let mut driver = driver();
            driver.event(&response(json!({"models": models}))).unwrap();
            assert!(driver.initialized());
            assert!(driver.models().is_none());
            assert_eq!(
                serde_json::from_slice::<Value>(&driver.pending()).unwrap()["type"],
                "user"
            );
        }
        let mut driver = driver();
        driver.event(&response(json!({}))).unwrap();
        assert!(driver.models().is_none());
        assert!(!driver.pending().is_empty());
    }

    #[test]
    fn all_metadata_bounds_are_bytes_and_apply_to_the_complete_catalog() {
        for (field, invalid) in [
            ("value", json!("m".repeat(MAX_FIELD + 1))),
            ("displayName", json!("界".repeat(MAX_FIELD / 3 + 1))),
            ("description", json!("d".repeat(MAX_DESCRIPTION + 1))),
            ("resolvedModel", json!("r".repeat(MAX_FIELD + 1))),
            ("supportsEffort", json!("true")),
            ("supportsAdaptiveThinking", json!(1)),
            ("supportsFastMode", json!([])),
            ("supportsAutoMode", json!({})),
            ("supportedEffortLevels", json!(["e".repeat(MAX_FIELD + 1)])),
            (
                "supportedEffortLevels",
                json!((0..=MAX_EFFORTS).map(|n| n.to_string()).collect::<Vec<_>>()),
            ),
        ] {
            let mut model = row("bounded");
            model[field] = invalid;
            assert!(
                parse_models(&json!({"models": [model]})).is_none(),
                "{field}"
            );
        }
        assert!(parse_models(&json!({"models": (0..=MAX_MODELS).map(|n| row(&n.to_string())).collect::<Vec<_>>()})).is_none());
        let models: Vec<_> = (0..MAX_MODELS)
            .map(|n| {
                let mut model = row(&n.to_string());
                model["description"] = json!("d".repeat(MAX_DESCRIPTION));
                model
            })
            .collect();
        assert!(parse_models(&json!({"models": models})).is_none());
        let mut model = row(&"m".repeat(MAX_FIELD));
        model["description"] = json!("d".repeat(MAX_DESCRIPTION));
        assert!(parse_models(&json!({"models": [model]})).is_some());
    }

    #[test]
    fn rejections_malformed_envelopes_and_wrong_ids_never_send_prompt() {
        for event in [
            json!({"type":"control_response"}),
            json!({"type":"control_response", "response": []}),
            json!({"type":"control_response", "response": {"subtype":"success", "request_id":"other", "response":{}}}),
            json!({"type":"control_response", "response": {"subtype":"error", "request_id":INITIALIZE_ID, "error":"provider-secret"}}),
            json!({"type":"control_response", "response": {"subtype":"success", "request_id":INITIALIZE_ID, "response":[]}}),
            json!({"type":"control_response", "response": {"subtype":"success", "request_id":INITIALIZE_ID, "response":{}, "error":"provider-secret"}}),
            json!({"type":"control_cancel_request", "request_id": INITIALIZE_ID}),
        ] {
            let mut driver = driver();
            let error = driver.event(&event).unwrap_err();
            assert!(!error.contains("provider-secret"));
            assert!(driver.pending().is_empty());
            assert!(driver.models().is_none());
            assert!(
                driver
                    .event(&response(json!({"models": [row("later")]})))
                    .is_err()
            );
            assert_eq!(driver.failure(), Some(error.as_str()));
            assert!(driver.pending().is_empty());
        }
    }

    #[test]
    fn duplicates_or_outer_abort_clear_a_queued_prompt_and_catalog() {
        for abort in [false, true] {
            let mut driver = driver();
            driver
                .event(&response(json!({"models": [row("observed")]})))
                .unwrap();
            if abort {
                driver.abort();
            } else {
                assert!(
                    driver
                        .event(&response(json!({"models": [row("duplicate")]})))
                        .is_err()
                );
            }
            assert!(driver.pending().is_empty());
            assert!(driver.models().is_none());
            assert!(driver.input_done());
        }
        let mut driver = Driver::new("prompt".into());
        assert!(driver.event(&response(json!({}))).is_err());
        assert!(driver.pending().is_empty());
    }

    #[test]
    fn server_requests_get_only_a_bounded_static_denial() {
        for initialized in [false, true] {
            for id in [
                json!("server-request"),
                json!("x".repeat(MAX_FIELD + 1)),
                json!({"secret": "parameters"}),
                json!(1),
                json!("bad\nid"),
            ] {
                let mut driver = driver();
                if initialized {
                    driver
                        .event(&response(json!({"models": [row("model")]})))
                        .unwrap();
                }
                assert!(
                    driver
                        .event(&json!({"type":"control_request", "request_id":id,
                    "request":{"subtype":"can_use_tool", "input":{"token":"parameters-secret"}}}))
                        .is_err()
                );
                driver.abort();
                let outgoing = driver.pending();
                if id == json!("server-request") {
                    assert_eq!(
                        serde_json::from_slice::<Value>(&outgoing).unwrap(),
                        json!({
                            "type":"control_response", "response": {
                                "subtype":"error", "request_id":"server-request", "error":DENIAL
                            }
                        })
                    );
                } else {
                    assert!(outgoing.is_empty());
                }
                assert!(driver.models().is_none());
                assert!(driver.pending().is_empty());
            }
        }
    }

    #[test]
    fn pending_permissions_do_not_turn_into_an_authorized_task() {
        for name in [
            "pending_permission_requests",
            "pending_user_dialog_requests",
        ] {
            let mut driver = driver();
            let mut event = response(json!({"models": [row("observed")]}));
            event["response"][name] = json!([{"type":"control_request", "request_id":"pending", "request":{"input":"secret"}}]);
            assert!(driver.event(&event).is_err());
            let denial = serde_json::from_slice::<Value>(&driver.pending()).unwrap();
            assert_eq!(denial["response"]["subtype"], "error");
            assert!(driver.models().is_none());
            assert!(!driver.initialized());
        }
    }

    #[test]
    fn account_commands_and_unknown_metadata_are_never_copied() {
        let mut driver = driver();
        let mut model = row("alias");
        model["credentials"] = json!({"api_key":"model-secret"});
        driver
            .event(&response(json!({
                "models": [model], "account": {"email":"account-secret"},
                "commands":[{"name":"command-secret"}], "agents":["agent-secret"]
            })))
            .unwrap();
        let models = serde_json::to_string(&driver.models()).unwrap();
        assert!(!models.contains("secret"));
        assert!(
            !String::from_utf8(driver.pending())
                .unwrap()
                .contains("secret")
        );
    }

    #[test]
    fn bounded_prompt_and_normal_events_do_not_add_control_work() {
        let mut huge = Driver::new("x".repeat(crate::host::MAX_PHASE_INPUT + 1));
        assert!(huge.failure().is_some());
        assert!(huge.pending().is_empty());
        let mut driver = driver();
        for event in [
            json!({"type":"system", "subtype":"init", "model":"not-a-catalog"}),
            json!({"type":"assistant"}),
            json!({"type":"result"}),
        ] {
            driver.event(&event).unwrap();
        }
        assert!(!driver.initialized());
        assert!(driver.models().is_none());
        assert!(driver.pending().is_empty());
        assert!(!fits_json(&json!("x".repeat(256)), 256));
        assert!(fits_json(&json!("x".repeat(254)), 256));
    }

    #[test]
    fn escaped_prompt_keeps_the_full_existing_input_budget() {
        let prompt = "\0".repeat(crate::host::MAX_PHASE_INPUT);
        let mut driver = Driver::new(prompt.clone());
        assert!(driver.failure().is_none());
        driver.pending();
        driver.event(&response(json!({}))).unwrap();
        let bytes = driver.pending();
        assert!(bytes.len() > crate::host::MAX_PHASE_INPUT);
        assert!(bytes.len() <= MAX_PENDING);
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap()["message"]["content"],
            prompt
        );
        assert!(driver.input_done());
    }
}
