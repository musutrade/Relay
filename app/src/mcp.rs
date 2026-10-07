//! Thin newline-delimited JSON-RPC MCP stdio adapter. stdout is protocol-only.
use crate::{Application, Submission};
use serde_json::{Value, json};
use std::io::{BufRead, Write};

pub const MAX_MESSAGE_BYTES: usize = 128 * 1024;

fn role_selection_schema() -> Value {
    json!({"type":"object","required":["profile"],"additionalProperties":false,"properties":{
        "profile":{"type":"string","minLength":1,"maxLength":128},
        "model":{"type":"object","required":["value","source"],"additionalProperties":false,"properties":{
            "value":{"type":"string","minLength":1,"maxLength":256},"source":{"enum":["catalog","manual"]},
            "catalog":{"type":"object","required":["cache_epoch","generation"],"additionalProperties":false,"properties":{
                "cache_epoch":{"type":"string","pattern":"^[0-9a-fA-F]{32}$"},"generation":{"type":"integer","minimum":1}
            }}
        }},
        "effort":{"type":"string","minLength":1,"maxLength":256,"description":"Requires this model's current catalog-supported effort metadata"},
        "native_permission":{"enum":["codex_workspace_write","codex_full_access","claude_dont_ask","claude_auto","claude_bypass_permissions","claude_restricted","kiro_workspace_write"]},
        "confirm_permission_expansion":{"type":"boolean","const":true,"description":"Explicitly confirm the native permission change. Full access/bypass expands filesystem AND network access. Host policy and fixed reviewer restrictions still apply."}
    }})
}
fn role_selections_schema() -> Value {
    let choice = role_selection_schema();
    json!({"type":"object","minProperties":1,"additionalProperties":false,"properties":{"developer":choice,"reviewer":choice}})
}

pub fn handle(app: &Application, request: Value) -> Option<Value> {
    let id = request.get("id").cloned();
    let method = request.get("method").and_then(Value::as_str);
    if !request.is_object()
        || request.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || method.is_none_or(str::is_empty)
        || request.get("params").is_some_and(|p| !p.is_object())
    {
        return Some(error(Value::Null, -32600, "invalid JSON-RPC request"));
    }
    // MCP request methods require IDs. Ignore every genuine notification, not just a name prefix.
    let id = id?;
    if !(id.is_string() || id.is_i64() || id.is_u64()) {
        return Some(error(
            Value::Null,
            -32600,
            "request id must be a string or integer",
        ));
    }
    let result = match method {
        Some("initialize") => {
            let params = &request["params"];
            if !params["protocolVersion"].is_string()
                || !params["capabilities"].is_object()
                || !params["clientInfo"]["name"].is_string()
                || !params["clientInfo"]["version"].is_string()
            {
                return Some(error(
                    id,
                    -32602,
                    "initialize requires protocolVersion, capabilities and clientInfo",
                ));
            }
            json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"relay","version":env!("CARGO_PKG_VERSION")}})
        }
        Some("ping") => json!({}),
        Some("tools/list") => json!({"tools":[
            {"name":"relay_submit","description":"Submit a requirement to the durable development queue; use the same key for retries.","inputSchema":{"type":"object","required":["key","job"],"properties":{"permission_challenge":{"type":"string","minLength":64,"maxLength":64},"key":{"type":"string","minLength":1,"maxLength":128},"job":{"type":"object","required":["repository","requirements","agent"],"properties":{"repository":{"type":"string"},"requirements":{"type":"string"},"agent":{"type":"string"},"test":{"type":["string","null"]},"publish":{"type":"boolean","default":false},"draft_pr_adapter":{"type":["string","null"]},"workflow":{"type":["string","null"]},"workspace_quota_bytes":{"type":["integer","null"],"minimum":1,"maximum":1099511627776u64},"role_selections":role_selections_schema()},"additionalProperties":false}},"additionalProperties":false}},
            {"name":"relay_retry","description":"Explicitly continue a stopped unsuccessful task in its preserved workspace. Inspect side effects first; repeated calls return the same successor.","inputSchema":{"type":"object","required":["id","key","confirm_stopped_and_reconciled"],"properties":{"replacement":role_selection_schema(),"permission_challenge":{"type":"string","minLength":64,"maxLength":64},"id":{"type":"integer","minimum":1},"key":{"type":"string","minLength":1,"maxLength":128},"confirm_stopped_and_reconciled":{"type":"boolean","const":true},"workspace_quota_bytes":{"type":["integer","null"],"minimum":1,"maximum":1099511627776u64}},"additionalProperties":false}},
            {"name":"relay_continue_review","description":"Explicitly continue only the interrupted review of the preserved candidate, rerunning its configured tests first and never invoking the developer. Confirm stopped execution and reconciled side effects. Optional review_focus is 1–8192 UTF-8 bytes; omit to retain the original focus. Original publication choice still applies after approval; prior publication attempts are not replayed. Shares one durable successor with relay_retry; the first reservation fixes its mode and focus.","inputSchema":{"type":"object","required":["id","key","confirm_stopped_and_reconciled","revalidate_tests"],"properties":{"replacement":role_selection_schema(),"permission_challenge":{"type":"string","minLength":64,"maxLength":64},"id":{"type":"integer","minimum":1},"key":{"type":"string","minLength":1,"maxLength":128},"confirm_stopped_and_reconciled":{"type":"boolean","const":true},"workspace_quota_bytes":{"type":["integer","null"],"minimum":1,"maximum":1099511627776u64},"revalidate_tests":{"type":"boolean","const":true},"review_focus":{"type":["string","null"],"minLength":1,"maxLength":8192,"description":"Optional review acceptance criteria, at most 8192 UTF-8 bytes. Omit or null to retain original focus."}},"additionalProperties":false}},
            {"name":"relay_replacement_challenge","description":"Preview permission expansion for only the changed role at a proven stopped stage; scope binds predecessor, action, selection and composed host policy. Explicit attestation remains required.","inputSchema":{"type":"object","required":["id","action","replacement"],"properties":{"id":{"type":"integer","minimum":1},"action":{"enum":["retry","continue_review"]},"replacement":role_selection_schema()},"additionalProperties":false}},
            {"name":"relay_get","description":"Read one task and its durable result.","inputSchema":{"type":"object","required":["id"],"properties":{"id":{"type":"integer","minimum":1}},"additionalProperties":false}},
            {"name":"relay_list","description":"List the newest 100 tasks, optionally before a task ID.","inputSchema":{"type":"object","properties":{"before":{"type":"integer","minimum":1}},"additionalProperties":false}},
            {"name":"relay_resources","description":"Read a bounded host-derived initial workspace estimate for allowlisted repository/workflow names and optional selected reviewer_profile. No model execution or disk reservation.","inputSchema":{"type":"object","required":["repository"],"properties":{"repository":{"type":"string"},"workflow":{"type":["string","null"]},"reviewer_profile":{"type":["string","null"],"maxLength":128}},"additionalProperties":false}},
            {"name":"relay_operator","description":"Read structured failure, logical resource usage and server-declared recovery actions for one attempt.","inputSchema":{"type":"object","required":["id"],"properties":{"id":{"type":"integer","minimum":1}},"additionalProperties":false}},
            {"name":"relay_config","description":"Read configured repository, agent and test profile identifiers.","inputSchema":{"type":"object","additionalProperties":false}},
            {"name":"relay_permission_challenge","description":"Preview exact selected native permission scope and issue a short-lived challenge. This grants no permission: explicit confirm_permission_expansion and this challenge must accompany the matching initial submission. Full access/bypass expands filesystem AND network access.","inputSchema":{"type":"object","required":["job"],"properties":{"job":{"type":"object","required":["repository","requirements","agent","role_selections"],"properties":{"repository":{"type":"string"},"requirements":{"type":"string"},"agent":{"type":"string"},"workflow":{"type":["string","null"]},"test":{"type":["string","null"]},"publish":{"type":"boolean"},"draft_pr_adapter":{"type":["string","null"]},"workspace_quota_bytes":{"type":["integer","null"]},"role_selections":role_selections_schema()},"additionalProperties":false}},"additionalProperties":false}}
        ]}),
        Some("tools/call") => {
            let params = &request["params"];
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            if !params["name"].is_string() || !arguments.is_object() {
                return Some(error(id, -32602, "tool name and object arguments required"));
            }
            let call: Result<Value, String> = match params["name"].as_str() {
                Some("relay_submit") => serde_json::from_value::<Submission>(arguments)
                    .map_err(|e| e.to_string())
                    .and_then(|submission| {
                        app.submit(submission)
                            .map(|task| json!(task))
                            .map_err(|e| e.to_string())
                    }),
                Some("relay_permission_challenge") => {
                    #[derive(serde::Deserialize)]
                    #[serde(deny_unknown_fields)]
                    struct Input {
                        job: crate::host::Job,
                    }
                    serde_json::from_value::<Input>(arguments)
                        .map_err(|e| e.to_string())
                        .and_then(|input| {
                            app.permission_challenge(input.job)
                                .map_err(|e| e.to_string())
                        })
                }
                Some("relay_replacement_challenge") => {
                    let mut arguments = arguments;
                    let id = arguments
                        .as_object_mut()
                        .and_then(|o| o.remove("id"))
                        .and_then(|id| id.as_i64());
                    match id.filter(|id| *id > 0) {
                        Some(id) => {
                            serde_json::from_value::<crate::ReplacementChallengeRequest>(arguments)
                                .map_err(|e| e.to_string())
                                .and_then(|input| {
                                    app.replacement_challenge(id, input)
                                        .map_err(application_error)
                                })
                        }
                        None => Err("positive task id required".into()),
                    }
                }
                Some("relay_retry") => {
                    let mut arguments = arguments;
                    let id = arguments
                        .as_object_mut()
                        .and_then(|object| object.remove("id"))
                        .and_then(|id| id.as_i64());
                    match id.filter(|id| *id > 0) {
                        Some(id) => serde_json::from_value::<crate::RetryRequest>(arguments)
                            .map_err(|e| e.to_string())
                            .and_then(|input| {
                                app.retry(id, input)
                                    .map(|task| json!(task))
                                    .map_err(application_error)
                            }),
                        None => Err("positive task id required".into()),
                    }
                }
                Some("relay_continue_review") => {
                    let mut arguments = arguments;
                    let id = arguments
                        .as_object_mut()
                        .and_then(|object| object.remove("id"))
                        .and_then(|id| id.as_i64());
                    match id.filter(|id| *id > 0) {
                        Some(id) => {
                            serde_json::from_value::<crate::ReviewContinuationRequest>(arguments)
                                .map_err(|e| e.to_string())
                                .and_then(|input| {
                                    app.continue_review(id, input)
                                        .map(|task| json!(task))
                                        .map_err(|e| e.to_string())
                                })
                        }
                        None => Err("positive task id required".into()),
                    }
                }
                Some("relay_get") => arguments["id"]
                    .as_i64()
                    .filter(|id| *id > 0)
                    .ok_or("positive task id required".to_string())
                    .and_then(|id| {
                        app.get_view(id)
                            .map(|task| json!(task))
                            .map_err(|e| e.to_string())
                    }),
                Some("relay_list") => {
                    let before = arguments.get("before");
                    if before.is_some_and(|v| v.as_i64().is_none_or(|n| n <= 0)) {
                        Err("positive before cursor required".into())
                    } else {
                        app.list_views(before.and_then(Value::as_i64))
                            .map(|tasks| json!(tasks))
                            .map_err(|e| e.to_string())
                    }
                }
                Some("relay_resources") => {
                    #[derive(serde::Deserialize)]
                    #[serde(deny_unknown_fields)]
                    struct Input {
                        repository: String,
                        workflow: Option<String>,
                        reviewer_profile: Option<String>,
                    }
                    serde_json::from_value::<Input>(arguments)
                        .map_err(|error| error.to_string())
                        .and_then(|input| {
                            app.resource_estimate_with_reviewer(
                                &input.repository,
                                input.workflow.as_deref(),
                                input.reviewer_profile.as_deref(),
                            )
                            .map(|estimate| json!(estimate))
                            .map_err(|error| error.to_string())
                        })
                }
                Some("relay_operator") => {
                    #[derive(serde::Deserialize)]
                    #[serde(deny_unknown_fields)]
                    struct Input {
                        id: i64,
                    }
                    serde_json::from_value::<Input>(arguments)
                        .map_err(|error| error.to_string())
                        .and_then(|input| {
                            if input.id > 0 {
                                app.operator(input.id).map_err(|error| error.to_string())
                            } else {
                                Err("positive task id required".into())
                            }
                        })
                }
                Some("relay_config") => Ok(app.public_config()),
                _ => return Some(error(id, -32602, "unknown tool")),
            };
            match call {
                Ok(value) => {
                    json!({"content":[{"type":"text","text":value.to_string()}],"isError":false})
                }
                Err(message) => json!({"content":[{"type":"text","text":message}],"isError":true}),
            }
        }
        _ => return Some(error(id, -32601, "method not found")),
    };
    Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
}
fn error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

pub fn serve(
    app: &Application,
    mut input: impl BufRead,
    mut output: impl Write,
) -> std::io::Result<()> {
    loop {
        let mut bytes = Vec::new();
        // read_until via a limited view avoids unbounded allocation on malicious stdin.
        let read = std::io::Read::take(&mut input, (MAX_MESSAGE_BYTES + 1) as u64)
            .read_until(b'\n', &mut bytes)?;
        if read == 0 {
            return Ok(());
        }
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "MCP message too large",
            ));
        }
        let response = match serde_json::from_slice(&bytes) {
            Ok(request) => handle(app, request),
            Err(_) => Some(error(Value::Null, -32700, "parse error")),
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut output, &response)?;
            writeln!(output)?;
            output.flush()?;
        }
    }
}

fn application_error(error: crate::Error) -> String {
    match error {
        crate::Error::ActionUnavailable { code, cause } => json!({"error":cause,"failure":crate::resources::Failure::new(code,"request",cause.clone())}).to_string(),
        other => other.to_string(),
    }
}
