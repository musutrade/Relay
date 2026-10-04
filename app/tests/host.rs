#![cfg(target_os = "linux")]
use relay::{MAX_RESULT_BYTES, State, Task};
use relay_app::host::{Host, HostConfig, Job, Outcome, RunResult};
use serde_json::json;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Fixture {
    temp: TempDir,
    config: HostConfig,
}
impl Fixture {
    fn new(script: &str) -> Self {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("original.txt"), "original\n").unwrap();
        fs::create_dir(source.join(".git")).unwrap();
        fs::write(source.join(".git/config"), "source git configuration").unwrap();
        let config = serde_json::from_value(json!({
            "workspace_root":temp.path().join("runs"),
            "repositories":{"fixture":source},
            "agents":{"fake":{"program":"/bin/sh","args":["-c",script,"relay-fake","{requirements}"]}},
            "tests":{"check":{"program":"/bin/sh","args":["-c","test -f changed.txt && printf 'tests passed'"]}},
            "draft_pr_adapters":{"fake-pr":{"program":"/bin/sh","args":["-c","test \"$RELAY_DRAFT_PR\" = true && printf 'https://github.example.invalid/fixture/pull/1 (draft)' && touch published.txt"]}},
            "timeout_seconds":3,"output_limit_bytes":1024,
            "supervisor_program":env!("CARGO_BIN_EXE_relay-app")
        })).unwrap();
        Self { temp, config }
    }
    fn task(&self, id: i64) -> Task {
        Task {
            id,
            key: format!("fixture-{id}"),
            payload: serde_json::to_string(&self.job()).unwrap(),
            state: State::Claimed,
            generation: 1,
            owner: Some("fixture-host".into()),
            result: None,
        }
    }
    fn job(&self) -> Job {
        Job {
            workflow: None,
            repository: "fixture".into(),
            requirements: "Implement the fixture change".into(),
            agent: "fake".into(),
            test: None,
            publish: false,
            draft_pr_adapter: None,
        }
    }
    fn workspace(&self, id: i64) -> PathBuf {
        self.config
            .workspace_root
            .join(format!("task-{id}-generation-1/repository"))
    }
    fn run(&self, task: &Task) -> RunResult {
        Host::new(self.config.clone())
            .unwrap()
            .execute(task, Arc::new(AtomicBool::new(false)))
    }
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.is_file() {
        assert!(
            Instant::now() < deadline,
            "file did not appear: {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}
fn assert_pid_reaped(path: &Path) {
    let pid: u32 = fs::read_to_string(path).unwrap().trim().parse().unwrap();
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "process {pid} remains after command cleanup"
    );
}

#[test]
fn fake_agent_tests_and_draft_adapter_use_private_snapshot_and_literal_input() {
    let fixture = Fixture::new(
        "cat > stdin.txt; printf '%s' \"$1\" > argument.txt; printf changed > changed.txt; printf 'agent done'",
    );
    let injection_target = fixture.temp.path().join("must-not-exist");
    let mut task = fixture.task(1);
    let mut job = fixture.job();
    job.requirements = format!(
        "Implement text; $(touch {}) \" ' 🚀",
        injection_target.display()
    );
    job.test = Some("check".into());
    job.publish = true;
    job.draft_pr_adapter = Some("fake-pr".into());
    task.payload = serde_json::to_string(&job).unwrap();
    let result = fixture.run(&task);
    assert_eq!(result.outcome, Outcome::Success, "{}", result.to_json());
    assert_eq!(result.agent.as_ref().unwrap().stdout, "agent done");
    assert_eq!(result.tests.as_ref().unwrap().stdout, "tests passed");
    assert!(result.draft_pr.as_ref().unwrap().stdout.contains("(draft)"));
    let workspace = fixture.workspace(1);
    assert_eq!(
        fs::read_to_string(workspace.join("stdin.txt")).unwrap(),
        job.requirements
    );
    assert_eq!(
        fs::read_to_string(workspace.join("argument.txt")).unwrap(),
        job.requirements
    );
    assert_eq!(
        fs::read_to_string(workspace.join("original.txt")).unwrap(),
        "original\n"
    );
    assert!(!workspace.join(".git").exists());
    assert!(workspace.join("published.txt").exists());
    assert!(!fixture.temp.path().join("source/changed.txt").exists());
    assert!(!injection_target.exists());
    assert_eq!(
        fs::metadata(workspace.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

#[test]
fn failure_stops_later_stages() {
    let mut fixture = Fixture::new("printf failed >&2; exit 7");
    let mut job = fixture.job();
    job.test = Some("check".into());
    job.publish = true;
    job.draft_pr_adapter = Some("fake-pr".into());
    let mut task = fixture.task(1);
    task.payload = serde_json::to_string(&job).unwrap();
    let result = fixture.run(&task);
    assert_eq!(result.outcome, Outcome::Failure);
    assert_eq!(result.agent.unwrap().exit_code, Some(7));
    assert!(result.tests.is_none());
    assert!(result.draft_pr.is_none());
    fixture.config.agents.get_mut("fake").unwrap().args[1] = "printf done".into();
    task.id = 2;
    let result = fixture.run(&task);
    assert_eq!(result.outcome, Outcome::Failure);
    assert_eq!(result.agent.unwrap().outcome, Outcome::Success);
    assert_eq!(result.tests.unwrap().outcome, Outcome::Failure);
    assert!(result.draft_pr.is_none());
}

#[test]
fn command_fields_and_unknown_profiles_are_rejected() {
    let fixture = Fixture::new("exit 0");
    let mut job = fixture.job();
    job.repository = "../../etc".into();
    assert!(job.validate(&fixture.config).is_err());
    job = fixture.job();
    job.agent = "/bin/sh".into();
    assert!(job.validate(&fixture.config).is_err());
    job = fixture.job();
    job.test = Some("arbitrary".into());
    assert!(job.validate(&fixture.config).is_err());
    job = fixture.job();
    job.publish = true;
    assert!(job.validate(&fixture.config).is_err());
    job = fixture.job();
    job.draft_pr_adapter = Some("fake-pr".into());
    assert!(job.validate(&fixture.config).is_err());
    job = fixture.job();
    job.requirements = " ".into();
    assert!(job.validate(&fixture.config).is_err());
    job.requirements = "x".repeat(32769);
    assert!(job.validate(&fixture.config).is_err());
    let mut value = serde_json::to_value(fixture.job()).unwrap();
    value["program"] = json!("/bin/sh");
    assert!(Job::from_payload(&value.to_string(), &fixture.config).is_err());
    let mut task = fixture.task(1);
    task.state = State::Queued;
    assert_eq!(fixture.run(&task).outcome, Outcome::Failure);
}

#[test]
fn symlink_escape_is_rejected_before_running_agent() {
    let fixture = Fixture::new("printf changed > escape");
    let outside = fixture.temp.path().join("outside.txt");
    fs::write(&outside, "safe").unwrap();
    symlink(&outside, fixture.temp.path().join("source/escape")).unwrap();
    let result = fixture.run(&fixture.task(1));
    assert_eq!(result.outcome, Outcome::Failure);
    assert!(result.error.unwrap().contains("symlink"));
    assert!(result.agent.is_none());
    assert_eq!(fs::read_to_string(outside).unwrap(), "safe");
}

#[test]
fn snapshot_limits_and_workspace_overlap_are_rejected() {
    let mut fixture = Fixture::new("exit 0");
    fixture.config.max_snapshot_bytes = 4;
    let result = fixture.run(&fixture.task(1));
    assert_eq!(result.outcome, Outcome::Failure);
    assert!(result.error.unwrap().contains("byte limit"));
    fixture.config.max_snapshot_bytes = 1024;
    fixture.config.max_snapshot_entries = 1;
    fs::write(fixture.temp.path().join("source/second.txt"), "second").unwrap();
    let result = fixture.run(&fixture.task(2));
    assert_eq!(result.outcome, Outcome::Failure);
    assert!(result.error.unwrap().contains("entry limit"));
    fixture.config.workspace_root = fixture.temp.path().join("source/runs");
    assert!(Host::new(fixture.config).is_err());
    assert!(!fixture.temp.path().join("source/runs").exists());
}

#[test]
fn same_generation_is_never_executed_twice() {
    let fixture = Fixture::new("printf 'once' >> executions.txt");
    let task = fixture.task(1);
    assert_eq!(fixture.run(&task).outcome, Outcome::Success);
    let result = fixture.run(&task);
    assert_eq!(result.outcome, Outcome::Unknown);
    assert_eq!(
        fs::read_to_string(fixture.workspace(1).join("executions.txt")).unwrap(),
        "once"
    );
}

const TREE_SCRIPT: &str = "echo $$ > leader.pid; sleep 60 & echo $! > child.pid; /bin/sh -c 'sleep 60 & echo $! > grandchild.pid; wait' & echo $! > branch.pid; wait";

#[test]
fn timeout_stops_and_reaps_group_and_grandchildren() {
    let mut fixture = Fixture::new(TREE_SCRIPT);
    fixture.config.timeout_seconds = 1;
    let started = Instant::now();
    let result = fixture.run(&fixture.task(1));
    assert_eq!(result.outcome, Outcome::TimedOut, "{}", result.to_json());
    assert!(started.elapsed() < Duration::from_secs(5));
    for file in ["leader.pid", "child.pid", "branch.pid", "grandchild.pid"] {
        assert_pid_reaped(&fixture.workspace(1).join(file));
    }
}

#[test]
fn cancellation_stops_and_reaps_process_tree_before_return() {
    let fixture = Fixture::new(TREE_SCRIPT);
    let host = Host::new(fixture.config.clone()).unwrap();
    let task = fixture.task(1);
    let cancellation = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancellation);
    let worker = thread::spawn(move || host.execute(&task, flag));
    wait_for_file(&fixture.workspace(1).join("grandchild.pid"));
    cancellation.store(true, Ordering::Release);
    let result = worker.join().unwrap();
    assert_eq!(result.outcome, Outcome::Cancelled, "{}", result.to_json());
    for file in ["leader.pid", "child.pid", "branch.pid", "grandchild.pid"] {
        assert_pid_reaped(&fixture.workspace(1).join(file));
    }
}

#[test]
fn successful_leader_does_not_leave_background_child() {
    let fixture = Fixture::new("sleep 60 & echo $! > child.pid; exit 0");
    let result = fixture.run(&fixture.task(1));
    assert_eq!(result.outcome, Outcome::Success, "{}", result.to_json());
    assert_pid_reaped(&fixture.workspace(1).join("child.pid"));
}

#[test]
fn adopted_session_descendant_is_stopped_and_reaped() {
    let mut fixture = Fixture::new(
        "/usr/bin/setsid /bin/sh -c 'echo $$ > escaped.pid; sleep 60 & echo $! > escaped-child.pid; wait' & wait",
    );
    fixture.config.timeout_seconds = 1;
    let result = fixture.run(&fixture.task(1));
    assert_eq!(result.outcome, Outcome::TimedOut, "{}", result.to_json());
    assert_pid_reaped(&fixture.workspace(1).join("escaped.pid"));
    assert_pid_reaped(&fixture.workspace(1).join("escaped-child.pid"));
}

#[test]
fn noisy_output_is_drained_and_json_result_is_bounded() {
    let mut fixture = Fixture::new(
        "head -c 200000 /dev/zero; head -c 200000 /dev/zero >&2; printf changed > changed.txt",
    );
    fixture.config.output_limit_bytes = 8192;
    let mut job = fixture.job();
    job.test = Some("check".into());
    let mut task = fixture.task(1);
    task.payload = serde_json::to_string(&job).unwrap();
    let result = fixture.run(&task);
    assert_eq!(result.outcome, Outcome::Success, "{}", result.to_json());
    assert!(result.agent.as_ref().unwrap().stdout_truncated);
    assert!(result.agent.as_ref().unwrap().stderr_truncated);
    assert!(result.to_json().len() <= MAX_RESULT_BYTES);
    let bounded: RunResult = serde_json::from_str(&result.to_json()).unwrap();
    assert_eq!(bounded.outcome, Outcome::Success);
    assert!(bounded.agent.unwrap().stdout.len() < 8192);
}

#[test]
fn one_wall_clock_budget_covers_all_phases() {
    let mut fixture = Fixture::new("sleep 0.6; touch changed.txt");
    fixture.config.timeout_seconds = 1;
    fixture.config.tests.get_mut("check").unwrap().args[1] = "sleep 0.6".into();
    let mut job = fixture.job();
    job.test = Some("check".into());
    let mut task = fixture.task(1);
    task.payload = serde_json::to_string(&job).unwrap();
    let result = fixture.run(&task);
    assert_eq!(result.outcome, Outcome::TimedOut, "{}", result.to_json());
    assert_eq!(result.agent.unwrap().outcome, Outcome::Success);
    assert_eq!(result.tests.unwrap().outcome, Outcome::TimedOut);
}

#[test]
fn pre_cancelled_job_creates_no_workspace() {
    let fixture = Fixture::new("exit 0");
    let result = Host::new(fixture.config.clone())
        .unwrap()
        .execute(&fixture.task(1), Arc::new(AtomicBool::new(true)));
    assert_eq!(result.outcome, Outcome::Cancelled);
    assert!(result.workspace.is_none());
    assert!(!fixture.workspace(1).exists());
}

#[test]
fn workspace_growth_and_retention_limits_fail_closed() {
    let mut fixture = Fixture::new("head -c 65536 /dev/zero > large.bin; sleep 60");
    fixture.config.max_snapshot_bytes = 1024;
    let result = fixture.run(&fixture.task(1));
    assert_eq!(result.outcome, Outcome::Failure, "{}", result.to_json());
    assert!(
        result
            .agent
            .unwrap()
            .error
            .unwrap()
            .contains("workspace byte limit")
    );
    fixture.config.max_retained_workspaces = 1;
    let result = fixture.run(&fixture.task(2));
    assert_eq!(result.outcome, Outcome::Failure);
    assert!(result.error.unwrap().contains("retention limit"));
}

#[test]
fn failed_supervisor_is_unknown_not_an_execution_success() {
    let mut fixture = Fixture::new("touch should-not-run");
    fixture.config.supervisor_program = Some(PathBuf::from("/bin/false"));
    let result = fixture.run(&fixture.task(1));
    assert_eq!(result.outcome, Outcome::Unknown);
    assert!(!fixture.workspace(1).join("should-not-run").exists());
}

#[test]
fn independent_groups_do_not_cancel_each_other() {
    let mut fixture = Fixture::new("echo $$ > leader.pid; sleep 60");
    let first = Host::new(fixture.config.clone()).unwrap();
    fixture.config.agents.get_mut("fake").unwrap().args[1] = "sleep 0.4; printf completed".into();
    let second = Host::new(fixture.config.clone()).unwrap();
    let flag = Arc::new(AtomicBool::new(false));
    let first_flag = Arc::clone(&flag);
    let task1 = fixture.task(1);
    let task2 = fixture.task(2);
    let run1 = thread::spawn(move || first.execute(&task1, first_flag));
    let run2 = thread::spawn(move || second.execute(&task2, Arc::new(AtomicBool::new(false))));
    wait_for_file(&fixture.workspace(1).join("leader.pid"));
    flag.store(true, Ordering::Release);
    assert_eq!(run1.join().unwrap().outcome, Outcome::Cancelled);
    let result2 = run2.join().unwrap();
    assert_eq!(result2.outcome, Outcome::Success, "{}", result2.to_json());
    assert_eq!(result2.agent.unwrap().stdout, "completed");
}

#[test]
fn host_api_token_is_not_passed_to_commands() {
    let mut fixture = Fixture::new("test -z \"${RELAY_TOKEN+x}\" && printf safe");
    fixture
        .config
        .agents
        .get_mut("fake")
        .unwrap()
        .env
        .insert("RELAY_TOKEN".into(), "must-not-leak".into());
    let result = fixture.run(&fixture.task(1));
    assert_eq!(result.outcome, Outcome::Success, "{}", result.to_json());
    assert_eq!(result.agent.unwrap().stdout, "safe");
}

#[test]
fn existing_generation_stays_unknown_even_with_cancel_or_retention_limit() {
    let mut fixture = Fixture::new("exit 0");
    assert_eq!(fixture.run(&fixture.task(1)).outcome, Outcome::Success);
    fixture.config.max_retained_workspaces = 1;
    let result = Host::new(fixture.config.clone())
        .unwrap()
        .execute(&fixture.task(1), Arc::new(AtomicBool::new(true)));
    assert_eq!(result.outcome, Outcome::Unknown);
}

#[test]
fn fast_workspace_growth_is_checked_before_success() {
    let mut fixture = Fixture::new("head -c 65536 /dev/zero > large.bin");
    fixture.config.max_snapshot_bytes = 1024;
    let result = fixture.run(&fixture.task(1));
    assert_eq!(result.outcome, Outcome::Failure, "{}", result.to_json());
}

fn native_fixture(provider: &str, body: &str, version: &str) -> Fixture {
    let mut f = Fixture::new("exit 77");
    let script = f.temp.path().join("fake-native.py");
    let help = "--json --ephemeral --sandbox --skip-git-repo-check --config --ignore-user-config --ignore-rules --model --effort --output-format --verbose --permission-prompts --no-session-persistence --max-turns --max-budget-usd --restricted --tools --allowedTools --disallowedTools --disable-slash-commands --strict-mcp-config --mcp-config";
    fs::write(&script,format!("#!/usr/bin/python3\nimport json, os, sys, subprocess, time\nif '--version' in sys.argv:\n print({})\nelif '--help' in sys.argv:\n print({})\nelse:\n prompt = sys.stdin.read()\n with open('invocation.json','w') as file: json.dump({{'args':sys.argv[1:],'input':prompt}},file)\n{}\n",serde_json::to_string(version).unwrap(),serde_json::to_string(help).unwrap(),body.lines().map(|line|format!(" {line}")).collect::<Vec<_>>().join("\n"))).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    f.config.agents.clear();
    f.config.native_agents.insert(
        "fake".into(),
        serde_json::from_value(
            json!({"provider":provider,"program":script,"model":"requested-model"}),
        )
        .unwrap(),
    );
    f
}
const NATIVE_CODEX_SUCCESS: &str = "print(json.dumps({'type':'thread.started','thread_id':'fake-session'}))\nprint(json.dumps({'type':'item.completed','item':{'type':'agent_message','text':'native completed'}}))\nprint(json.dumps({'type':'turn.completed','usage':{'input_tokens':9,'output_tokens':3}}))";
const NATIVE_CLAUDE_SUCCESS: &str = "print(json.dumps({'type':'system','subtype':'init','session_id':'fake-claude','model':'reported-model'}))\nprint(json.dumps({'type':'result','subtype':'success','is_error':False,'result':'claude completed','permission_denials':[],'usage':{'input_tokens':7,'output_tokens':2}}))";

#[test]
fn native_codex_protocol_survives_truncated_capture_and_uses_stdin() {
    let body = format!(
        "for _ in range(12000): print(json.dumps({{'type':'future.progress','text':'x'*100}}))\n{NATIVE_CODEX_SUCCESS}"
    );
    let f = native_fixture("codex_cli", &body, "codex-cli 0.200.0");
    let mut task = f.task(1);
    let mut job = f.job();
    job.requirements = "literal $(touch injected)\n--dangerously-skip-permissions".into();
    task.payload = serde_json::to_string(&job).unwrap();
    let result = f.run(&task);
    assert_eq!(result.outcome, Outcome::Success, "{result:?}");
    let command = result.agent.unwrap();
    assert!(command.stdout_truncated);
    let provider = command.provider.unwrap();
    assert_eq!(provider.summary, "native completed");
    assert_eq!(provider.cli_version.as_deref(), Some("0.200.0"));
    assert_eq!(provider.requested_model.as_deref(), Some("requested-model"));
    assert_eq!(provider.reported_model, None);
    let invocation: serde_json::Value =
        serde_json::from_slice(&fs::read(f.workspace(1).join("invocation.json")).unwrap()).unwrap();
    assert_eq!(invocation["input"], job.requirements);
    assert!(
        !invocation["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|arg| arg == &job.requirements)
    );
    assert_eq!(invocation["args"].as_array().unwrap().last().unwrap(), "-");
    assert!(!f.workspace(1).join("injected").exists());
}

#[test]
fn native_claude_version_gate_never_starts_unsupported_model_run() {
    let f = native_fixture("claude_cli", NATIVE_CLAUDE_SUCCESS, "2.1.258 (Claude Code)");
    let result = f.run(&f.task(1));
    assert_eq!(result.outcome, Outcome::Failure);
    assert!(!f.workspace(1).join("invocation.json").exists());
    assert!(result.agent.unwrap().error.unwrap().contains("2.1.259"));
    let f = native_fixture("claude_cli", NATIVE_CLAUDE_SUCCESS, "2.1.259 (Claude Code)");
    let result = f.run(&f.task(1));
    assert_eq!(result.outcome, Outcome::Success, "{result:?}");
    assert_eq!(
        result
            .agent
            .unwrap()
            .provider
            .unwrap()
            .reported_model
            .as_deref(),
        Some("reported-model")
    );
}

#[test]
fn native_fail_closed_on_protocol_errors_denials_missing_terminal_and_nonzero() {
    for (provider, body) in [
        ("codex_cli", "print('malformed')".into()),
        (
            "codex_cli",
            "print(json.dumps({'type':'turn.started'}))".into(),
        ),
        ("codex_cli", format!("{NATIVE_CODEX_SUCCESS}\nsys.exit(4)")),
        (
            "codex_cli",
            format!("print('x'*65537)\n{NATIVE_CODEX_SUCCESS}"),
        ),
        (
            "claude_cli",
            format!(
                "print(json.dumps({{'type':'system','subtype':'permission_denied'}}))\n{NATIVE_CLAUDE_SUCCESS}"
            ),
        ),
    ] {
        let f = native_fixture(provider, &body, "2.1.259");
        let result = f.run(&f.task(1));
        assert_eq!(result.outcome, Outcome::Failure, "{result:?}");
        assert!(result.agent.unwrap().provider.is_some());
    }
}

#[test]
fn native_config_rejects_collisions_and_doctor_probe_has_no_model_call() {
    let mut f = native_fixture("codex_cli", NATIVE_CODEX_SUCCESS, "codex-cli 0.200.0");
    let host = Host::new(f.config.clone()).unwrap();
    let probe = host.probe_native("fake", false).unwrap();
    assert!(!probe.read_only_supported);
    assert_eq!(fs::read_dir(&f.config.workspace_root).unwrap().count(), 0);
    f.config.agents.insert(
        "fake".into(),
        serde_json::from_value(json!({"program":"/bin/true"})).unwrap(),
    );
    assert!(Host::new(f.config).is_err());
}

#[test]
fn native_cleanup_reaps_descendants_even_after_successful_terminal() {
    let body = format!(
        "child = subprocess.Popen(['/bin/sleep','60'],start_new_session=True)\nwith open('child.pid','w') as file: file.write(str(child.pid))\n{NATIVE_CODEX_SUCCESS}"
    );
    let f = native_fixture("codex_cli", &body, "codex-cli 0.200.0");
    let result = f.run(&f.task(1));
    assert_eq!(result.outcome, Outcome::Success, "{result:?}");
    assert_pid_reaped(&f.workspace(1).join("child.pid"));
}

#[test]
fn native_cancellation_keeps_cleanup_and_timeout_outcomes() {
    let body = "with open('running.pid','w') as file: file.write(str(os.getpid()))\nwhile True: print(json.dumps({'type':'future.progress'}),flush=True); time.sleep(0.01)";
    let mut f = native_fixture("codex_cli", body, "codex-cli 0.200.0");
    f.config.timeout_seconds = 3;
    let result = f.run(&f.task(1));
    assert_eq!(result.outcome, Outcome::TimedOut, "{result:?}");
    assert_pid_reaped(&f.workspace(1).join("running.pid"));
    f.config.timeout_seconds = 5;
    let host = Host::new(f.config.clone()).unwrap();
    let task = f.task(2);
    let flag = Arc::new(AtomicBool::new(false));
    let cloned = flag.clone();
    let runner = thread::spawn(move || host.execute(&task, cloned));
    wait_for_file(&f.workspace(2).join("running.pid"));
    flag.store(true, Ordering::Release);
    let result = runner.join().unwrap();
    assert_eq!(result.outcome, Outcome::Cancelled, "{result:?}");
    assert_pid_reaped(&f.workspace(2).join("running.pid"));
}

#[test]
fn native_metadata_and_escaped_text_converge_to_persistence_budget() {
    let body = "print(json.dumps({'type':'result','subtype':'success','is_error':False,'result':'\\x01'*10000}))";
    let f = native_fixture("claude_cli", body, "2.1.259");
    let mut result = f.run(&f.task(1));
    assert_eq!(result.outcome, Outcome::Success, "{result:?}");
    let command = result.agent.as_mut().unwrap();
    command.stdout = "\u{1}".repeat(8192);
    command.stderr = command.stdout.clone();
    result.tests = result.agent.clone();
    result.draft_pr = result.agent.clone();
    let encoded = result.to_json();
    assert!(encoded.len() <= MAX_RESULT_BYTES);
    let restored: RunResult = serde_json::from_str(&encoded).unwrap();
    assert!(restored.agent.unwrap().provider.unwrap().summary_truncated);
}

#[test]
fn probe_escaped_output_fits_supervisor_envelope_and_never_calls_model() {
    let f = native_fixture(
        "codex_cli",
        "raise RuntimeError('must not call model')",
        "codex-cli 0.200.0",
    );
    let script = f.temp.path().join("fake-native.py");
    let source = fs::read_to_string(&script).unwrap().replace(" print(\"codex-cli 0.200.0\")", " print(\"codex-cli 0.200.0\"); sys.stdout.write('\\x01'*60000); sys.stderr.write('\\x02'*60000)");
    fs::write(script, source).unwrap();
    let probe = Host::new(f.config.clone())
        .unwrap()
        .probe_native("fake", false)
        .unwrap();
    assert_eq!(probe.cli_version, "0.200.0");
    assert_eq!(fs::read_dir(&f.config.workspace_root).unwrap().count(), 0);
}

#[test]
fn complete_local_cli_example_validates_after_path_substitution() {
    // Validate the shipped full schema and workflow references without invoking a CLI.
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let mut config: HostConfig =
        serde_json::from_str(include_str!("../../examples/local-cli-config.json")).unwrap();
    config.workspace_root = temp.path().join("workspaces");
    config.repositories.insert("project".into(), source);
    for profile in config.native_agents.values_mut() {
        profile.program = "/bin/true".into();
    }
    config.tests.get_mut("check").unwrap().program = "/bin/true".into();
    for workflow in config.workflows.values_mut() {
        workflow.git_program = "/bin/true".into();
    }
    assert!(config.draft_pr_adapters.is_empty());
    Host::new(config.clone()).unwrap();
    for (agent, workflow) in [("codex", "codex-reviewed"), ("claude", "claude-reviewed")] {
        let payload = json!({"repository":"project", "requirements":"Implement change", "agent":agent, "workflow":workflow});
        let job = Job::from_payload(&payload.to_string(), &config).unwrap();
        assert!(!job.publish);
    }
}
