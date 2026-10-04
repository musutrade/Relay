use relay_app::providers::{
    MAX_PROTOCOL_LINE, NativeProfile, ProtocolParser, ProviderKind, ProviderResult,
};
use serde_json::json;

fn profile(kind: ProviderKind) -> NativeProfile {
    serde_json::from_value(json!({"provider":kind,"program":"/bin/true"})).unwrap()
}
fn parse(kind: ProviderKind, text: &str) -> (ProviderResult, Option<String>) {
    let mut parser =
        ProtocolParser::new(ProviderResult::new(&profile(kind), Some("2.1.259".into())));
    for bytes in text.as_bytes().chunks(7) {
        parser.feed(bytes);
    }
    parser.finish()
}
const CODEX_OK: &str = "{\"type\":\"thread.started\",\"thread_id\":\"session-1\"}\n{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"完成✓\"}}\n{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":12,\"cached_input_tokens\":3,\"output_tokens\":4}}\n";
const CLAUDE_OK: &str = "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"c-1\",\"model\":\"reported\"}\n{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\",\"permission_denials\":[],\"usage\":{\"input_tokens\":9,\"output_tokens\":4},\"total_cost_usd\":0.25,\"num_turns\":2}\n";

#[test]
fn compiles_literal_typed_profiles_and_read_only_variants() {
    let mut p = profile(ProviderKind::CodexCli);
    p.model = Some("trusted model; literal".into());
    p.effort = Some("high".into());
    let command = p.compile(false).unwrap();
    assert_eq!(
        &command.args[..6],
        [
            "exec",
            "--json",
            "--ephemeral",
            "--sandbox",
            "workspace-write",
            "--skip-git-repo-check"
        ]
    );
    assert_eq!(command.args.last().unwrap(), "-");
    assert!(command.args.contains(&"trusted model; literal".into()));
    assert!(
        command
            .args
            .contains(&"model_reasoning_effort=\"high\"".into())
    );
    assert!(
        p.compile(true)
            .unwrap_err()
            .contains("review_profile_unsupported")
    );
    let mut p = profile(ProviderKind::ClaudeCli);
    p.max_turns = Some(5);
    p.max_budget_usd = Some(2.5);
    let command = p.compile(true).unwrap();
    assert_eq!(
        &command.args[..6],
        [
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--permission-prompts",
            "none"
        ]
    );
    assert!(
        command
            .args
            .windows(2)
            .any(|pair| pair == ["--max-turns", "5"])
    );
    assert!(
        command
            .args
            .windows(2)
            .any(|pair| pair == ["--max-budget-usd", "2.5"])
    );
    assert!(command.args.contains(&"--restricted".into()));
    assert!(command.args.contains(&"Read,Glob,Grep".into()));
    assert!(command.args.iter().any(|arg| arg.contains("mcp__*")));
    assert!(
        !command
            .args
            .iter()
            .any(|arg| arg.contains("bypassPermissions") || arg.contains("dangerously"))
    );
}

#[test]
fn rejects_unbounded_or_untyped_settings() {
    for value in [
        json!({"args":["--dangerously-skip-permissions"]}),
        json!({"provider":"unknown"}),
    ] {
        let mut base = json!({"provider":"claude_cli","program":"/bin/true"});
        base.as_object_mut()
            .unwrap()
            .extend(value.as_object().unwrap().clone());
        assert!(serde_json::from_value::<NativeProfile>(base).is_err());
    }
    let mut p = profile(ProviderKind::ClaudeCli);
    p.max_turns = Some(101);
    assert!(p.validate().is_err());
    p.max_turns = None;
    p.max_budget_usd = Some(f64::NAN);
    assert!(p.validate().is_err());
    p.max_budget_usd = Some(0.0);
    assert!(p.validate().is_err());
    p.max_budget_usd = None;
    p.model = Some("x\0y".into());
    assert!(p.validate().is_err());
    p.model = None;
    p.effort = Some("--bad".into());
    assert!(p.validate().is_err());
    let mut p = profile(ProviderKind::CodexCli);
    p.max_turns = Some(2);
    assert!(p.validate().is_err());
}

#[test]
fn version_and_help_fail_closed_and_read_only_needs_capabilities() {
    let p = profile(ProviderKind::ClaudeCli);
    let help = "--output-format --verbose --permission-prompts --no-session-persistence";
    assert!(
        p.validate_probe("2.1.258 (Claude Code)", help, false)
            .is_err()
    );
    assert!(p.validate_probe("2.1.259-beta", help, false).is_err());
    assert_eq!(
        p.validate_probe("2.1.259 (Claude Code)", help, false)
            .unwrap(),
        "2.1.259"
    );
    assert!(p.validate_probe("2.1.259", help, true).is_err());
    assert!(
        p.validate_probe(
            "2.1.259",
            "--output-format --verbose --permission-prompts-fake --no-session-persistence",
            false
        )
        .is_err()
    );
    let help = format!(
        "{help} --restricted --tools --allowedTools --disallowedTools --disable-slash-commands --strict-mcp-config --mcp-config"
    );
    assert!(p.validate_probe("3.0.0", &help, true).is_ok());
}

#[test]
fn normalizes_codex_and_claude_without_trusting_requested_model_as_reported() {
    let (result, error) = parse(ProviderKind::CodexCli, CODEX_OK);
    assert!(error.is_none(), "{error:?}");
    assert_eq!(result.summary, "完成✓");
    assert_eq!(result.session_id.as_deref(), Some("session-1"));
    assert_eq!(result.reported_model, None);
    assert_eq!(result.usage.cached_input_tokens, Some(3));
    let (result, error) = parse(ProviderKind::ClaudeCli, CLAUDE_OK);
    assert!(error.is_none(), "{error:?}");
    assert_eq!(result.reported_model.as_deref(), Some("reported"));
    assert_eq!(result.usage.total_cost_usd, Some(0.25));
}

#[test]
fn malformed_missing_failed_unknown_and_duplicate_terminals_fail() {
    for text in [
        "garbage\n".into(),
        "{\"type\":\"turn.completed\"}\n".into(),
        "{\"type\":\"turn.failed\"}\n".into(),
        format!("{CODEX_OK}{{\"type\":\"turn.completed\"}}\n"),
        format!("{CODEX_OK}{{\"type\":\"future.completed\"}}\n"),
        format!(
            "{{\"type\":\"item.completed\",\"item\":{{\"type\":\"command_execution\",\"status\":\"declined\"}}}}\n{CODEX_OK}"
        ),
        format!("{{\"type\":\"notice\",\"permission_denials\":[{{}}]}}\n{CODEX_OK}"),
    ] {
        assert!(parse(ProviderKind::CodexCli, &text).1.is_some(), "{text}");
    }
    for text in [
        "{\"type\":\"result\",\"subtype\":\"error_max_turns\",\"is_error\":true}\n".into(),
        CLAUDE_OK.replace("\"success\"", "\"future_success\""),
        CLAUDE_OK.replace("\"permission_denials\":[]", "\"permission_denials\":[{}]"),
        CLAUDE_OK.replace("\"input_tokens\":9", "\"input_tokens\":-9"),
        format!("{{\"type\":\"system\",\"subtype\":\"permission_denied\"}}\n{CLAUDE_OK}"),
    ] {
        assert!(parse(ProviderKind::ClaudeCli, &text).1.is_some(), "{text}");
    }
}

#[test]
fn bounds_lines_summaries_identifiers_and_accepts_benign_unknown_events() {
    let mut parser =
        ProtocolParser::new(ProviderResult::new(&profile(ProviderKind::CodexCli), None));
    for _ in 0..100_000 {
        parser.feed(b"{\"type\":\"future.progress\",\"ignored\":1}\n");
    }
    parser.feed(CODEX_OK.as_bytes());
    assert!(parser.finish().1.is_none());
    let mut parser =
        ProtocolParser::new(ProviderResult::new(&profile(ProviderKind::CodexCli), None));
    parser.feed(&vec![b'x'; MAX_PROTOCOL_LINE + 1]);
    parser.feed(CODEX_OK.as_bytes());
    assert!(parser.finish().1.unwrap().contains("64 KiB"));
    let big =
        json!({"type":"result","subtype":"success","is_error":false,"result":"界".repeat(4000)})
            .to_string();
    let (result, error) = parse(ProviderKind::ClaudeCli, &big);
    assert!(error.is_none());
    assert!(result.summary.len() <= 4096);
    assert!(result.summary_truncated);
    let long_id = format!(
        "{{\"type\":\"thread.started\",\"thread_id\":\"{}\"}}\n{CODEX_OK}",
        "s".repeat(257)
    );
    assert!(parse(ProviderKind::CodexCli, &long_id).1.is_some());
}

#[test]
fn read_only_protocol_rejects_mutating_and_external_tools() {
    for (kind, event, success) in [
        (
            ProviderKind::CodexCli,
            json!({"type":"item.started","item":{"type":"file_change"}}),
            CODEX_OK,
        ),
        (
            ProviderKind::CodexCli,
            json!({"type":"item.started","item":{"type":"mcp_tool_call"}}),
            CODEX_OK,
        ),
        (
            ProviderKind::ClaudeCli,
            json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}),
            CLAUDE_OK,
        ),
    ] {
        let mut parser =
            ProtocolParser::new(ProviderResult::new(&profile(kind), None)).read_only(true);
        parser.feed(format!("{event}\n{success}").as_bytes());
        assert!(parser.finish().1.is_some());
    }
}

#[test]
fn claude_read_only_accepts_implicit_nonmutating_end_conversation() {
    let mut parser =
        ProtocolParser::new(ProviderResult::new(&profile(ProviderKind::ClaudeCli), None))
            .read_only(true);
    parser.feed(b"{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"EndConversation\"}]}}\n");
    parser.feed(CLAUDE_OK.replace("{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"c-1\",\"model\":\"reported\"}\n", "").as_bytes());
    assert!(parser.finish().1.is_none());
}

#[test]
fn codex_nonfatal_items_preserve_the_last_answer_and_usage() {
    // Codex's SDK calls ErrorItem non-fatal; tool completion may also be failed.
    // https://github.com/openai/codex/blob/main/sdk/typescript/src/items.ts
    for item in [
        json!({"id":"e1","type":"error","message":"tool unavailable"}),
        json!({"id":"c1","type":"command_execution","command":"false","aggregated_output":"","exit_code":1,"status":"failed"}),
        json!({"id":"m1","type":"mcp_tool_call","server":"test","tool":"lookup","arguments":{},"error":{"message":"unavailable"},"status":"failed"}),
        json!({"id":"f1","type":"file_change","changes":[],"status":"failed"}),
    ] {
        let stream = format!(
            "{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"initial plan\"}}}}\n{}\n{CODEX_OK}",
            json!({"type":"item.completed","item":item})
        );
        let (result, error) = parse(ProviderKind::CodexCli, &stream);
        assert!(error.is_none(), "{error:?}: {stream}");
        assert_eq!(result.summary, "完成✓");
        assert_eq!(result.terminal_reason.as_deref(), Some("turn.completed"));
        assert_eq!(result.usage.input_tokens, Some(12));
        assert_eq!(result.usage.output_tokens, Some(4));
        // Non-fatal does not itself imply success without a turn terminal.
        let (_, error) = parse(
            ProviderKind::CodexCli,
            &json!({"type":"item.completed","item":item}).to_string(),
        );
        assert!(error.unwrap().contains("without a recognized"));
    }
}

#[test]
fn fatal_errors_stay_failed_while_later_metadata_is_collected() {
    for prefix in [
        "not json".into(),
        "{\"type\":42}".into(),
        "{\"type\":\"item.completed\",\"item\":{\"type\":\"command_execution\",\"status\":42}}"
            .into(),
        "{\"type\":\"error\",\"message\":\"transport failed\"}".into(),
        "{\"type\":\"item.completed\",\"item\":{\"type\":\"error\",\"message\":42}}".into(),
        "{\"type\":\"item.completed\",\"item\":{\"type\":\"mcp_tool_call\",\"error\":\"invalid\"}}"
            .into(),
        "x".repeat(MAX_PROTOCOL_LINE + 1),
    ] {
        let stream = format!("{prefix}\n{CODEX_OK}");
        // Test both same-buffer continuation and boundaries inside every line.
        for size in [1, 7, stream.len()] {
            let mut parser =
                ProtocolParser::new(ProviderResult::new(&profile(ProviderKind::CodexCli), None));
            for chunk in stream.as_bytes().chunks(size) {
                parser.feed(chunk);
            }
            let (result, error) = parser.finish();
            assert!(error.is_some(), "{prefix}");
            assert_eq!(result.summary, "完成✓");
            assert_eq!(result.usage.input_tokens, Some(12));
            assert_eq!(result.terminal_reason.as_deref(), Some("turn.completed"));
        }
    }
    let (_, error) = parse(ProviderKind::CodexCli, &format!("bad\n{{}}\n{CODEX_OK}"));
    assert_eq!(error.as_deref(), Some("malformed provider JSONL event"));
}

#[test]
fn failed_turn_is_terminal_and_cannot_be_corrected_into_success() {
    let failed = "{\"type\":\"turn.failed\",\"error\":{\"message\":\"model unavailable\"}}\n";
    for suffix in ["", CODEX_OK] {
        let (result, error) = parse(ProviderKind::CodexCli, &format!("{failed}{suffix}"));
        assert_eq!(error.as_deref(), Some("provider turn failed"));
        assert_eq!(result.terminal_reason.as_deref(), Some("turn.failed"));
    }
}

#[test]
fn oversized_line_resynchronizes_only_at_newline_and_accepts_final_unterminated_line() {
    let mut parser =
        ProtocolParser::new(ProviderResult::new(&profile(ProviderKind::CodexCli), None));
    parser.feed(&vec![b'x'; MAX_PROTOCOL_LINE + 1]);
    parser.feed(b"{\"type\":\"turn.completed\"}"); // Still the oversized line.
    parser.feed(b"\n");
    parser.feed(CODEX_OK.trim_end().as_bytes());
    let (result, error) = parser.finish();
    assert!(error.unwrap().contains("64 KiB"));
    assert_eq!(result.summary, "完成✓");
    assert_eq!(result.usage.output_tokens, Some(4));
}

#[test]
fn invalid_utf8_remains_fatal_and_preserves_later_diagnostics() {
    let mut parser =
        ProtocolParser::new(ProviderResult::new(&profile(ProviderKind::CodexCli), None));
    parser.feed(&[0xff, b'\n']);
    parser.feed(CODEX_OK.as_bytes());
    let (result, error) = parser.finish();
    assert_eq!(error.as_deref(), Some("malformed provider JSONL event"));
    assert_eq!(result.summary, "完成✓");
    assert_eq!(result.usage.output_tokens, Some(4));
    let (result, error) = parse(
        ProviderKind::CodexCli,
        "{\"type\":\"error\",\"message\":\"transport failed\"}",
    );
    assert!(error.unwrap().contains("unrecoverable"));
    assert_eq!(result.terminal_reason.as_deref(), Some("provider_error"));
}
