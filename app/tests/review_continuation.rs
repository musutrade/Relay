#![cfg(target_os = "linux")]

use relay_app::{
    Application, RetryRequest, ReviewContinuationRequest, Submission,
    host::{HostConfig, Outcome, RunResult},
};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Barrier, Mutex, MutexGuard},
};
use tempfile::TempDir;

// All provider invocations are local fixtures. Probe invocations do not count as
// model calls, and audit files live outside either guarded candidate checkout.
const DEVELOPER: &str = r#"#!/usr/bin/python3
import json, os, pathlib, sys
prompt = sys.stdin.read()
root = pathlib.Path(os.environ['FIXTURE_AUDIT'])
with (root / 'developer.jsonl').open('a') as log:
    log.write(json.dumps({'prompt': prompt, 'round': os.environ['RELAY_WORKFLOW_ROUND']}) + '\n')
pathlib.Path('changed.txt').write_text('exact candidate\n')
print('implemented fixture')
"#;

const TEST: &str = r#"#!/usr/bin/python3
import json, os, pathlib, subprocess, sys
root = pathlib.Path(os.environ['FIXTURE_AUDIT'])
sha = os.environ['RELAY_CANDIDATE_SHA']
assert subprocess.check_output(['/usr/bin/git', 'rev-parse', 'HEAD'], text=True).strip() == sha
assert subprocess.check_output(['/usr/bin/git', 'show', sha + ':changed.txt'], text=True) == pathlib.Path('changed.txt').read_text()
with (root / 'test.jsonl').open('a') as log:
    log.write(json.dumps({'candidate_sha': sha, 'round': os.environ['RELAY_WORKFLOW_ROUND']}) + '\n')
print('fixture tests passed')
"#;

const REVIEWER: &str = r#"#!/usr/bin/python3
import json, os, pathlib, sys
if '--version' in sys.argv:
    print('2.1.281 (Claude Code)'); sys.exit()
if '--help' in sys.argv:
    print('--output-format --verbose --permission-prompts --restricted --tools --allowedTools --disallowedTools --disable-slash-commands --strict-mcp-config --mcp-config --no-session-persistence --session-id --resume --max-turns --max-budget-usd'); sys.exit()
root = pathlib.Path(os.environ['FIXTURE_AUDIT'])
log = root / 'reviewer.jsonl'
count = len(log.read_text().splitlines()) if log.exists() else 0
prompt = sys.stdin.read()
assert '--restricted' in sys.argv
assert 'Read,Glob,Grep' in sys.argv and 'Bash,Edit,Write,NotebookEdit,Agent,Task,mcp__*' in sys.argv
assert '--strict-mcp-config' in sys.argv and '--mcp-config' in sys.argv
assert '--continue' not in sys.argv and '--no-session-persistence' not in sys.argv
assert sys.argv[sys.argv.index('--max-turns') + 1] == '2'
assert pathlib.Path.cwd().name == 'reviewer-repository'
assert pathlib.Path('.git/relay-review.patch').is_file()
if count:
    assert '--resume' in sys.argv and '--session-id' not in sys.argv
    sid = sys.argv[sys.argv.index('--resume') + 1]
    assert sid == json.loads(log.read_text().splitlines()[0])['session_id']
else:
    assert '--session-id' in sys.argv and '--resume' not in sys.argv
    sid = sys.argv[sys.argv.index('--session-id') + 1]
with log.open('a') as output:
    output.write(json.dumps({'session_id': sid, 'argv': sys.argv[1:], 'prompt': prompt, 'cwd': os.getcwd(), 'candidate_sha': os.environ['RELAY_CANDIDATE_SHA']}) + '\n')
print(json.dumps({'type': 'system', 'subtype': 'init', 'session_id': sid, 'model': 'fixture-model'}))
mode_path = root / 'review-mode'
mode = mode_path.read_text() if mode_path.exists() else 'fail_once'
if mode == 'fail_always' or (count == 0 and mode not in ('approve', 'reject', 'malformed')):
    print(json.dumps({'type': 'result', 'subtype': 'error_max_turns', 'is_error': True, 'session_id': sid, 'result': 'Maximum turns reached'}))
else:
    rejected = mode in ('reject', 'reject_resume')
    answer = json.dumps({'candidate_sha': os.environ['RELAY_CANDIDATE_SHA'], 'verdict': 'changes_requested' if rejected else 'approved', 'summary': 'Checked the preserved exact candidate', 'findings': ['A fixture issue remains'] if rejected else []})
    if mode == 'malformed': answer = 'A verdict was not produced'
    print(json.dumps({'type': 'result', 'subtype': 'success', 'is_error': False, 'session_id': sid, 'result': answer, 'permission_denials': []}))
"#;

struct Fixture {
    temp: TempDir,
    config: HostConfig,
    _serial: MutexGuard<'static, ()>,
}

// An unrelated concurrent fork can briefly inherit another fixture's CLOEXEC
// lease before exec, correctly making the nonblocking ownership check fail.
// Isolate fixture lifetimes rather than retrying or weakening that host guard.
// The dedicated race still uses independent Application connections to one DB.
static FIXTURES: Mutex<()> = Mutex::new(());

fn executable(path: &Path, script: &str) {
    fs::write(path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn git(repository: &Path, args: &[&str]) -> String {
    let output = Command::new("/usr/bin/git")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .current_dir(repository)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn request(key: &str) -> ReviewContinuationRequest {
    ReviewContinuationRequest {
        workspace_quota_bytes: None,
        key: key.into(),
        confirm_stopped_and_reconciled: true,
        revalidate_tests: true,
        review_focus: None,
    }
}

fn result(app: &Application, id: i64) -> RunResult {
    serde_json::from_str(app.get(id).unwrap().result.as_deref().unwrap()).unwrap()
}

fn read_json(path: impl AsRef<Path>) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

impl Fixture {
    fn new() -> Self {
        let serial = FIXTURES.lock().unwrap_or_else(|error| error.into_inner());
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let audit = temp.path().join("audit");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&audit).unwrap();
        fs::write(source.join("original.txt"), "original\n").unwrap();
        git(&source, &["init", "--initial-branch=main"]);
        git(&source, &["add", "."]);
        git(&source, &["commit", "-m", "base"]);
        let developer = temp.path().join("developer");
        let reviewer = temp.path().join("reviewer");
        let test = temp.path().join("test");
        executable(&developer, DEVELOPER);
        executable(&reviewer, REVIEWER);
        executable(&test, TEST);
        let env = json!({"FIXTURE_AUDIT": audit});
        let config = serde_json::from_value(json!({
            "workspace_root": temp.path().join("runs"),
            "repositories": {"fixture": source},
            "agents": {"developer": {"program": developer, "env": env}},
            "native_agents": {"reviewer": {"provider": "claude_cli", "program": reviewer,
                "env": env, "session_continuity": true, "max_turns": 2}},
            "tests": {"check": {"program": test, "env": env}},
            "workflows": {"checked": {"repository": "fixture", "developer": "developer",
                "reviewer": "reviewer", "test": "check", "max_repairs": 3,
                "review_focus": "Check changed.txt contains the intended fixture change"}},
            "timeout_seconds": 30,
            "output_limit_bytes": 128,
            "max_retained_workspaces": 1,
            "supervisor_program": env!("CARGO_BIN_EXE_relay-app")
        }))
        .unwrap();
        Self {
            temp,
            config,
            _serial: serial,
        }
    }

    fn db(&self) -> PathBuf {
        self.temp.path().join("relay.db")
    }

    fn open(&self) -> Arc<Application> {
        Application::open(self.db(), self.config.clone()).unwrap()
    }

    fn input(&self) -> Submission {
        serde_json::from_value(json!({"key": "original", "job": {
            "repository": "fixture", "agent": "developer", "workflow": "checked",
            "requirements": "DEVELOPER-ONLY-SECRET-INSTRUCTION: implement the fixture"
        }}))
        .unwrap()
    }

    fn failed_review(&self) -> (Arc<Application>, RunResult) {
        let app = self.open();
        app.submit(self.input()).unwrap();
        assert!(app.work_once().unwrap());
        let first = result(&app, 1);
        assert_eq!(first.outcome, Outcome::Failure, "{}", first.to_json());
        let workflow = first.workflow.as_ref().unwrap();
        let round = workflow.rounds.last().unwrap();
        assert_eq!(workflow.rounds.len(), 1);
        assert_eq!(round.tests.as_ref().unwrap().outcome, Outcome::Success);
        assert_eq!(round.reviewer.as_ref().unwrap().outcome, Outcome::Failure);
        assert!(round.review.is_none());
        assert!(workflow.publication.is_none());
        self.assert_calls(1, 1, 1);
        (app, first)
    }

    fn events(&self, stage: &str) -> Vec<Value> {
        let path = self
            .temp
            .path()
            .join("audit")
            .join(format!("{stage}.jsonl"));
        match fs::read_to_string(path) {
            Ok(text) => text
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("cannot read fixture audit: {error}"),
        }
    }

    fn assert_calls(&self, developer: usize, tests: usize, reviewer: usize) {
        assert_eq!(self.events("developer").len(), developer, "developer calls");
        assert_eq!(self.events("test").len(), tests, "test calls");
        assert_eq!(self.events("reviewer").len(), reviewer, "reviewer calls");
    }

    fn mode(&self, mode: &str) {
        fs::write(self.temp.path().join("audit/review-mode"), mode).unwrap();
    }
}

#[test]
fn review_only_resumes_exact_session_candidate_and_profile_without_development() {
    let fixture = Fixture::new();
    let (app, first) = fixture.failed_review();
    let original = app.get(1).unwrap();
    let root = first.workspace.as_ref().unwrap();
    let candidate = first
        .workflow
        .as_ref()
        .unwrap()
        .candidate_sha
        .as_ref()
        .unwrap();
    let checkpoint = read_json(root.join("sessions/reviewer.json"));
    assert_eq!(checkpoint["ready"], false);
    assert_eq!(
        checkpoint["session_id"],
        fixture.events("reviewer")[0]["session_id"]
    );
    let before_claim = read_json(root.join("claim.json"));
    let profile = serde_json::to_value(&fixture.config.native_agents["reviewer"]).unwrap();
    let focus = "Review only the preserved candidate and its intended fixture behavior";
    let mut input = request("review-again");
    input.review_focus = Some(focus.into());
    let child = app.continue_review(1, input).unwrap();
    let payload: Value = serde_json::from_str(&child.payload).unwrap();
    assert!(payload["continuation"]["review_only"].is_object());
    assert_eq!(payload["continuation"]["workspace_task_id"], 1);
    assert_eq!(payload["continuation"]["predecessor_task_id"], 1);
    assert!(app.work_once().unwrap());
    let next = result(&app, child.id);
    assert_eq!(next.outcome, Outcome::Success, "{}", next.to_json());
    assert!(next.agent.is_none());
    assert_eq!(next.workspace.as_ref(), Some(root));
    let workflow = next.workflow.as_ref().unwrap();
    assert_eq!(workflow.review_continuation, Some(1));
    assert_eq!(workflow.candidate_sha.as_ref(), Some(candidate));
    assert_eq!(workflow.reviewed_sha.as_ref(), Some(candidate));
    assert_eq!(
        git(&root.join("repository"), &["rev-parse", "HEAD"]),
        *candidate
    );
    fixture.assert_calls(1, 2, 2);
    for event in fixture
        .events("test")
        .iter()
        .chain(fixture.events("reviewer").iter())
    {
        assert_eq!(event["candidate_sha"], *candidate);
    }
    let reviews = fixture.events("reviewer");
    assert_eq!(reviews[0]["session_id"], reviews[1]["session_id"]);
    assert_eq!(reviews[0]["cwd"], reviews[1]["cwd"]);
    let args = reviews[1]["argv"].as_array().unwrap();
    let resume = args.iter().position(|arg| arg == "--resume").unwrap();
    assert_eq!(args[resume + 1], checkpoint["session_id"]);
    let limit = args.iter().position(|arg| arg == "--max-turns").unwrap();
    assert_eq!(args[limit + 1], "2");
    let prompt = reviews[1]["prompt"].as_str().unwrap();
    assert!(prompt.contains(focus));
    assert!(prompt.contains("Do not edit files, run tests, publish, monitor, or delegate"));
    assert!(prompt.contains("\"outcome\":\"success\""));
    assert!(!prompt.contains("DEVELOPER-ONLY-SECRET-INSTRUCTION"));
    assert!(
        !fixture.events("developer")[0]["prompt"]
            .as_str()
            .unwrap()
            .contains(focus)
    );
    assert_eq!(
        read_json(root.join("sessions/reviewer.json"))["ready"],
        true
    );
    let after_claim = read_json(root.join("claim.json"));
    assert!(after_claim["attempt"].as_u64().unwrap() > before_claim["attempt"].as_u64().unwrap());
    assert_eq!(after_claim["task_id"], child.id);
    assert_eq!(
        serde_json::to_value(&app.config.native_agents["reviewer"]).unwrap(),
        profile
    );
    assert_eq!(app.get(1).unwrap(), original);
    assert_eq!(
        fs::read_dir(&fixture.config.workspace_root)
            .unwrap()
            .count(),
        1
    );
    assert!(!app.work_once().unwrap());
    assert!(next.to_json().len() <= relay::MAX_RESULT_BYTES);
}

#[test]
fn both_confirmations_and_bounded_utf8_focus_are_required_before_reservation() {
    let fixture = Fixture::new();
    let (app, _) = fixture.failed_review();
    for (stopped, tests) in [(false, false), (false, true), (true, false)] {
        let mut input = request("invalid-confirmation");
        input.confirm_stopped_and_reconciled = stopped;
        input.revalidate_tests = tests;
        assert!(app.continue_review(1, input).is_err());
    }
    for focus in [
        "".into(),
        " \n\t".into(),
        "x".repeat(8193),
        "界".repeat(2731),
    ] {
        let mut input = request("invalid-focus");
        input.review_focus = Some(focus);
        assert!(app.continue_review(1, input).is_err());
    }
    for key in [String::new(), "x".repeat(129), "界".repeat(43)] {
        assert!(app.continue_review(1, request(&key)).is_err());
    }
    assert!(app.continue_review(1, request("original")).is_err());
    assert_eq!(app.list(None).unwrap().len(), 1);
    assert!(app.get_view(1).unwrap().continuation_status.is_none());
    fixture.assert_calls(1, 1, 1);
    let focus = format!("{}ab", "界".repeat(2730));
    assert_eq!(focus.len(), 8192);
    let mut input = request("bounded-focus");
    input.review_focus = Some(focus.clone());
    let child = app.continue_review(1, input).unwrap();
    app.work_once().unwrap();
    assert_eq!(result(&app, child.id).outcome, Outcome::Success);
    assert!(
        fixture.events("reviewer")[1]["prompt"]
            .as_str()
            .unwrap()
            .contains(&focus)
    );
    fixture.assert_calls(1, 2, 2);
}

#[test]
fn duplicate_requests_reload_and_mixed_retry_share_first_winner() {
    let fixture = Fixture::new();
    let (app, _) = fixture.failed_review();
    let mut input = request("first-winner");
    input.review_focus = Some("First accepted focus".into());
    let first = app.continue_review(1, input).unwrap();
    let mut second = request("later-click");
    second.review_focus = Some("Must not replace first accepted focus".into());
    assert_eq!(app.continue_review(1, second).unwrap(), first);
    assert_eq!(
        app.retry(
            1,
            RetryRequest {
                workspace_quota_bytes: None,
                key: "ordinary-retry".into(),
                confirm_stopped_and_reconciled: true
            }
        )
        .unwrap(),
        first
    );
    drop(app);
    let app = fixture.open();
    assert_eq!(
        app.continue_review(1, request("after-reload")).unwrap(),
        first
    );
    assert_eq!(
        app.get_view(1)
            .unwrap()
            .continuation_status
            .unwrap()
            .successor_id,
        Some(first.id)
    );
    assert_eq!(app.list(None).unwrap().len(), 2);
    app.work_once().unwrap();
    assert_eq!(result(&app, first.id).outcome, Outcome::Success);
    assert_eq!(
        app.continue_review(1, request("after-completion"))
            .unwrap()
            .id,
        first.id
    );
    fixture.assert_calls(1, 2, 2);
    let reviews = fixture.events("reviewer");
    let prompt = reviews[1]["prompt"].as_str().unwrap();
    assert!(prompt.contains("First accepted focus"));
    assert!(!prompt.contains("Must not replace first accepted focus"));
}

#[test]
fn cross_connection_race_reserves_only_one_review_successor() {
    let fixture = Fixture::new();
    let (app, _) = fixture.failed_review();
    let other = fixture.open();
    let barrier = Arc::new(Barrier::new(2));
    let left = {
        let app = Arc::clone(&app);
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            let mut input = request("left");
            input.review_focus = Some("Left review focus".into());
            app.continue_review(1, input).unwrap()
        })
    };
    let right = std::thread::spawn(move || {
        barrier.wait();
        let mut input = request("right");
        input.review_focus = Some("Right review focus".into());
        other.continue_review(1, input).unwrap()
    });
    let child = left.join().unwrap();
    assert_eq!(right.join().unwrap(), child);
    assert_eq!(app.list(None).unwrap().len(), 2);
    assert!(app.work_once().unwrap());
    assert!(!app.work_once().unwrap());
    assert_eq!(result(&app, child.id).outcome, Outcome::Success);
    fixture.assert_calls(1, 2, 2);
}

#[test]
fn ordinary_retry_reservation_also_wins_over_review_continuation() {
    let fixture = Fixture::new();
    let (app, _) = fixture.failed_review();
    let child = app
        .retry(
            1,
            RetryRequest {
                workspace_quota_bytes: None,
                key: "ordinary-first".into(),
                confirm_stopped_and_reconciled: true,
            },
        )
        .unwrap();
    assert_eq!(
        app.continue_review(1, request("review-second")).unwrap(),
        child
    );
    assert!(
        serde_json::from_str::<Value>(&child.payload).unwrap()["continuation"]
            .get("review_only")
            .is_none()
    );
    assert_eq!(app.list(None).unwrap().len(), 2);
    fixture.assert_calls(1, 1, 1);
}

#[test]
fn continued_review_rejection_never_invokes_developer_or_repairs() {
    let fixture = Fixture::new();
    let (app, first) = fixture.failed_review();
    fixture.mode("reject_resume");
    let child = app.continue_review(1, request("resume-reject")).unwrap();
    app.work_once().unwrap();
    let next = result(&app, child.id);
    assert_eq!(next.outcome, Outcome::Failure, "{}", next.to_json());
    assert!(next.agent.is_none());
    assert_eq!(
        next.workflow.as_ref().unwrap().candidate_sha,
        first.workflow.unwrap().candidate_sha
    );
    assert!(next.workflow.as_ref().unwrap().reviewed_sha.is_none());
    assert!(
        next.workflow
            .as_ref()
            .unwrap()
            .rounds
            .last()
            .unwrap()
            .review
            .is_some()
    );
    fixture.assert_calls(1, 2, 2);
    assert!(
        app.continue_review(child.id, request("cannot-resume-valid-rejection"))
            .is_err()
    );
}

#[test]
fn raw_index_and_head_mutations_after_queueing_stop_before_tests_or_review() {
    for mutation in [
        "raw",
        "hidden-raw",
        "index",
        "head",
        "untracked",
        "reviewer-raw",
    ] {
        let fixture = Fixture::new();
        let (app, first) = fixture.failed_review();
        let root = first.workspace.unwrap();
        let repository = root.join("repository");
        let child = app
            .continue_review(1, request("queued-before-change"))
            .unwrap();
        match mutation {
            "raw" => fs::write(repository.join("original.txt"), "changed raw content\n").unwrap(),
            "hidden-raw" => {
                git(
                    &repository,
                    &["update-index", "--assume-unchanged", "original.txt"],
                );
                fs::write(repository.join("original.txt"), "hidden raw content\n").unwrap();
            }
            "index" => {
                fs::write(repository.join("original.txt"), "staged content\n").unwrap();
                git(&repository, &["add", "original.txt"]);
                fs::write(repository.join("original.txt"), "original\n").unwrap();
            }
            "head" => {
                git(
                    &repository,
                    &["commit", "--allow-empty", "-m", "unexpected HEAD"],
                );
            }
            "untracked" => fs::write(repository.join("preserve-me.txt"), "user work").unwrap(),
            "reviewer-raw" => fs::write(
                root.join("reviewer-repository/original.txt"),
                "reviewer mutation\n",
            )
            .unwrap(),
            _ => unreachable!(),
        }
        assert!(app.work_once().unwrap());
        let next = result(&app, child.id);
        assert_eq!(
            next.outcome,
            Outcome::Failure,
            "{mutation}: {}",
            next.to_json()
        );
        assert!(next.agent.is_none(), "{mutation}");
        assert!(
            next.tests.is_none(),
            "{mutation}: tests must not execute before candidate verification"
        );
        fixture.assert_calls(1, 1, 1);
        assert_eq!(
            fs::read_dir(&fixture.config.workspace_root)
                .unwrap()
                .count(),
            1
        );
        if mutation == "untracked" {
            assert_eq!(
                fs::read_to_string(repository.join("preserve-me.txt")).unwrap(),
                "user work"
            );
        }
    }
}

#[test]
fn changed_external_test_script_is_rerun_and_failure_stops_review_and_repairs() {
    let fixture = Fixture::new();
    let (app, first) = fixture.failed_review();
    let child = app
        .continue_review(1, request("fresh-tests-required"))
        .unwrap();
    let script =
        format!("{TEST}\nprint('changed external test now fails', file=sys.stderr)\nsys.exit(7)\n");
    executable(&fixture.config.tests["check"].program, &script);
    assert!(app.work_once().unwrap());
    let next = result(&app, child.id);
    assert_eq!(next.outcome, Outcome::Failure, "{}", next.to_json());
    assert!(next.agent.is_none());
    assert_eq!(next.tests.as_ref().unwrap().exit_code, Some(7));
    assert_eq!(
        next.workflow.as_ref().unwrap().candidate_sha,
        first.workflow.unwrap().candidate_sha
    );
    fixture.assert_calls(1, 2, 1);
}

#[test]
fn missing_candidate_checkpoint_or_publication_marker_never_calls_a_model() {
    for mutation in [
        "candidate",
        "checkpoint",
        "checkpoint-id",
        "checkpoint-role",
        "reviewer-candidate-missing",
        "reviewer-candidate-changed",
        "publication",
        "claim",
    ] {
        let fixture = Fixture::new();
        let (app, first) = fixture.failed_review();
        let root = first.workspace.unwrap();
        let child = app
            .continue_review(1, request("queued-before-loss"))
            .unwrap();
        match mutation {
            "candidate" => {
                let sha = first.workflow.unwrap().candidate_sha.unwrap();
                fs::remove_file(
                    root.join("repository/.git/objects")
                        .join(&sha[..2])
                        .join(&sha[2..]),
                )
                .unwrap();
            }
            "checkpoint" => fs::remove_file(root.join("sessions/reviewer.json")).unwrap(),
            "checkpoint-id" => {
                let path = root.join("sessions/reviewer.json");
                let mut record = read_json(&path);
                record["session_id"] = Value::Null;
                fs::write(path, record.to_string()).unwrap();
            }
            "checkpoint-role" => {
                let path = root.join("sessions/reviewer.json");
                let mut record = read_json(&path);
                record["role"] = json!("developer");
                fs::write(path, record.to_string()).unwrap();
            }
            "reviewer-candidate-missing" => {
                fs::remove_file(root.join("reviewer-candidate.txt")).unwrap();
            }
            "reviewer-candidate-changed" => {
                fs::write(root.join("reviewer-candidate.txt"), "f".repeat(40)).unwrap();
            }
            "publication" => fs::write(root.join("publication-attempt.json"), "{}").unwrap(),
            "claim" => {
                let path = root.join("claim.json");
                let mut record = read_json(&path);
                record["task_id"] = json!(999);
                fs::write(path, record.to_string()).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(app.work_once().unwrap());
        let next = result(&app, child.id);
        assert_eq!(
            next.outcome,
            Outcome::Failure,
            "{mutation}: {}",
            next.to_json()
        );
        assert!(next.agent.is_none());
        assert!(next.tests.is_none(), "{mutation}");
        fixture.assert_calls(1, 1, 1);
        assert_eq!(
            fs::read_dir(&fixture.config.workspace_root)
                .unwrap()
                .count(),
            1
        );
    }
}

#[test]
fn profile_drift_is_rejected_before_submission_and_after_queueing() {
    for queued in [false, true] {
        let mut fixture = Fixture::new();
        let (app, _) = fixture.failed_review();
        let child = queued.then(|| app.continue_review(1, request("before-drift")).unwrap());
        fixture
            .config
            .native_agents
            .get_mut("reviewer")
            .unwrap()
            .max_turns = Some(3);
        let changed = fixture.open();
        if let Some(child) = child {
            assert!(changed.work_once().unwrap());
            let next = result(&changed, child.id);
            assert_eq!(next.outcome, Outcome::Failure, "{}", next.to_json());
            assert!(next.agent.is_none());
            assert!(next.tests.is_none());
        } else {
            assert!(changed.continue_review(1, request("after-drift")).is_err());
            assert_eq!(changed.list(None).unwrap().len(), 1);
        }
        fixture.assert_calls(1, 1, 1);
    }
}

#[test]
fn publication_marker_before_submission_blocks_review_continuation() {
    let fixture = Fixture::new();
    let (app, first) = fixture.failed_review();
    fs::write(
        first.workspace.unwrap().join("publication-attempt.json"),
        "{}",
    )
    .unwrap();
    assert!(app.continue_review(1, request("published")).is_err());
    assert_eq!(app.list(None).unwrap().len(), 1);
    fixture.assert_calls(1, 1, 1);
}

#[test]
fn only_stopped_unsuccessful_workflow_with_tested_candidate_and_no_verdict_is_eligible() {
    for mode in [
        "queued",
        "claimed",
        "approve",
        "reject",
        "tests-failed",
        "developer-failed",
        "no-workflow",
    ] {
        let mut fixture = Fixture::new();
        fixture
            .config
            .workflows
            .get_mut("checked")
            .unwrap()
            .max_repairs = 0;
        fixture.mode(mode);
        if mode == "tests-failed" {
            executable(
                &fixture.config.tests["check"].program,
                &format!("{TEST}\nsys.exit(7)\n"),
            );
        }
        if mode == "developer-failed" {
            executable(
                &fixture.config.agents["developer"].program,
                &format!("{DEVELOPER}\nsys.exit(7)\n"),
            );
        }
        let app = fixture.open();
        let mut input = fixture.input();
        if mode == "no-workflow" {
            input.job.workflow = None;
        }
        app.submit(input).unwrap();
        if mode == "claimed" {
            relay::Store::open(fixture.db())
                .unwrap()
                .claim_next("stopped-fixture")
                .unwrap()
                .unwrap();
        } else if mode != "queued" {
            app.work_once().unwrap();
        }
        let calls = (
            fixture.events("developer").len(),
            fixture.events("test").len(),
            fixture.events("reviewer").len(),
        );
        assert!(
            app.continue_review(1, request("ineligible")).is_err(),
            "{mode}"
        );
        assert_eq!(app.list(None).unwrap().len(), 1, "{mode}");
        fixture.assert_calls(calls.0, calls.1, calls.2);
    }
}

#[test]
fn direct_metadata_and_request_override_injections_are_rejected() {
    let fixture = Fixture::new();
    let (app, _) = fixture.failed_review();
    let child = app.continue_review(1, request("legitimate")).unwrap();
    let legitimate: relay_app::host::Job = serde_json::from_str(&child.payload).unwrap();
    let mut injected = fixture.input();
    injected.key = "injected-child".into();
    injected.job.continuation = legitimate.continuation;
    assert!(app.submit(injected).is_err());
    for (field, value) in [
        ("candidate_sha", json!("f".repeat(40))),
        ("max_turns", json!(100)),
        ("reviewer", json!("untrusted-reviewer")),
        ("workspace_task_id", json!(999)),
        ("skip_tests", json!(true)),
        ("continuation", json!({"review_only": {}})),
    ] {
        let mut input = json!({"key": "inject", "confirm_stopped_and_reconciled": true, "revalidate_tests": true});
        input[field] = value;
        assert!(
            serde_json::from_value::<ReviewContinuationRequest>(input).is_err(),
            "{field}"
        );
    }
    assert_eq!(app.list(None).unwrap().len(), 2);
    fixture.assert_calls(1, 1, 1);
}

#[test]
fn chained_review_continuation_inherits_focus_without_restarting_development() {
    let fixture = Fixture::new();
    let (app, original) = fixture.failed_review();
    let original_task = app.get(1).unwrap();
    let root = original.workspace.as_ref().unwrap();
    let candidate = original
        .workflow
        .as_ref()
        .unwrap()
        .candidate_sha
        .as_ref()
        .unwrap();
    fixture.mode("fail_always");
    let mut input = request("review-second-attempt");
    input.review_focus =
        Some("Keep this bounded acceptance focus on further review attempts".into());
    let second = app.continue_review(1, input).unwrap();
    assert!(app.work_once().unwrap());
    let failed = result(&app, second.id);
    assert_eq!(failed.outcome, Outcome::Failure, "{}", failed.to_json());
    assert!(failed.agent.is_none());
    let second_task = app.get(second.id).unwrap();
    fixture.mode("approve");
    let third = app
        .continue_review(second.id, request("review-third-attempt"))
        .unwrap();
    let payload: Value = serde_json::from_str(&third.payload).unwrap();
    assert_eq!(payload["continuation"]["workspace_task_id"], 1);
    assert_eq!(payload["continuation"]["predecessor_task_id"], second.id);
    assert!(app.work_once().unwrap());
    let completed = result(&app, third.id);
    assert_eq!(
        completed.outcome,
        Outcome::Success,
        "{}",
        completed.to_json()
    );
    assert!(completed.agent.is_none());
    assert_eq!(completed.workspace.as_ref(), Some(root));
    let workflow = completed.workflow.as_ref().unwrap();
    assert_eq!(workflow.review_continuation, Some(second.id));
    assert_eq!(workflow.candidate_sha.as_ref(), Some(candidate));
    assert_eq!(workflow.reviewed_sha.as_ref(), Some(candidate));
    fixture.assert_calls(1, 3, 3);
    let reviews = fixture.events("reviewer");
    for review in &reviews[1..] {
        assert_eq!(review["session_id"], reviews[0]["session_id"]);
        assert!(
            review["prompt"]
                .as_str()
                .unwrap()
                .contains("Keep this bounded acceptance focus on further review attempts")
        );
        assert!(
            !review["prompt"]
                .as_str()
                .unwrap()
                .contains("DEVELOPER-ONLY-SECRET-INSTRUCTION")
        );
    }
    assert_eq!(app.get(1).unwrap(), original_task);
    assert_eq!(app.get(second.id).unwrap(), second_task);
    assert_eq!(
        fs::read_dir(&fixture.config.workspace_root)
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn completed_provider_turn_with_malformed_verdict_can_resume_review_only() {
    let fixture = Fixture::new();
    fixture.mode("malformed");
    let app = fixture.open();
    app.submit(fixture.input()).unwrap();
    assert!(app.work_once().unwrap());
    let first = result(&app, 1);
    assert_eq!(first.outcome, Outcome::Failure, "{}", first.to_json());
    let round = first.workflow.as_ref().unwrap().rounds.last().unwrap();
    assert_eq!(round.reviewer.as_ref().unwrap().outcome, Outcome::Success);
    assert!(round.review.is_none());
    let root = first.workspace.unwrap();
    assert_eq!(
        read_json(root.join("sessions/reviewer.json"))["ready"],
        true
    );
    fixture.mode("approve");
    let child = app
        .continue_review(1, request("replace-malformed-verdict"))
        .unwrap();
    assert!(app.work_once().unwrap());
    let next = result(&app, child.id);
    assert_eq!(next.outcome, Outcome::Success, "{}", next.to_json());
    assert!(next.agent.is_none());
    fixture.assert_calls(1, 2, 2);
    let reviews = fixture.events("reviewer");
    assert_eq!(reviews[0]["session_id"], reviews[1]["session_id"]);
    assert!(
        reviews[1]["prompt"]
            .as_str()
            .unwrap()
            .contains("Check changed.txt contains the intended fixture change")
    );
}

// Forward the ordinary private supervisor protocol unchanged. The selected
// phase deliberately cannot attest cleanup, even though this local fixture has
// no escaped process. Unknown must remain authoritative over later Git guards.
const UNKNOWN_SUPERVISOR: &str = r#"#!/usr/bin/python3
import json, os, pathlib, select, subprocess, sys
root = pathlib.Path(__AUDIT__)
line = sys.stdin.buffer.readline()
spec = json.loads(line)
event = {'program': spec['program'], 'args': spec['args'], 'cwd': spec['cwd']}
with (root / 'supervisor.jsonl').open('a') as log:
    log.write(json.dumps(event) + '\n')
if (root / 'unknown-returned').exists():
    with (root / 'post-unknown.jsonl').open('a') as log:
        log.write(json.dumps(event) + '\n')
control = root / 'unknown-control.json'
selected = json.loads(control.read_text()) if control.exists() else {}
is_reviewer = spec['program'] == __REVIEWER__ and spec.get('provider') is not None and spec.get('read_only') is True
is_test = spec['program'] == __TEST__
if (selected.get('phase') == 'reviewer' and is_reviewer) or (selected.get('phase') == 'test' and is_test):
    cwd = pathlib.Path(spec['cwd'])
    workspace = cwd.parent
    (cwd / 'original.txt').write_text('mutation while cleanup is unknown\n')
    (workspace / 'repository' / 'original.txt').write_text('mutation while cleanup is unknown\n')
    (root / 'unknown-started').write_text(json.dumps(event))
    if selected.get('cancel'):
        # The test calls Application.cancel after observing unknown-started.
        # Host cancellation closes this existing supervisor control pipe.
        while os.read(sys.stdin.fileno(), 4096):
            pass
    (root / 'unknown-returned').write_text('yes')
    print(json.dumps({'outcome': 'unknown', 'exit_code': None, 'signal': None,
        'stdout': '', 'stderr': '', 'stdout_truncated': False, 'stderr_truncated': False,
        'duration_ms': 0, 'supervisor_pid': os.getpid(), 'error': 'fixture cleanup remains unknown'}), flush=True)
    sys.exit(0)
child = subprocess.Popen([__SUPERVISOR__, '__relay_host_supervisor'], stdin=subprocess.PIPE,
    pass_fds=(198,) if spec.get('workspace_lease') else ())
child.stdin.write(line)
child.stdin.flush()
control_open = True
while child.poll() is None:
    if control_open:
        ready, _, _ = select.select([sys.stdin], [], [], 0.01)
        if ready:
            data = os.read(sys.stdin.fileno(), 4096)
            if data:
                child.stdin.write(data)
                child.stdin.flush()
            else:
                child.stdin.close()
                control_open = False
    else:
        child.wait()
if control_open:
    child.stdin.close()
sys.exit(child.returncode)
"#;

impl Fixture {
    fn unknown_supervisor(&mut self) {
        let path = self.temp.path().join("unknown-supervisor");
        let script = UNKNOWN_SUPERVISOR
            .replace(
                "__AUDIT__",
                &json!(self.temp.path().join("audit")).to_string(),
            )
            .replace(
                "__REVIEWER__",
                &json!(self.config.native_agents["reviewer"].program).to_string(),
            )
            .replace(
                "__TEST__",
                &json!(self.config.tests["check"].program).to_string(),
            )
            .replace(
                "__SUPERVISOR__",
                &json!(env!("CARGO_BIN_EXE_relay-app")).to_string(),
            );
        executable(&path, &script);
        self.config.supervisor_program = Some(path);
        // This protocol fixture adds a forwarding process to every Git phase;
        // reserve headroom for a loaded aggregate test run, not production work.
        self.config.timeout_seconds = 90;
    }

    fn make_phase_unknown(&self, phase: &str, cancel: bool) {
        fs::write(
            self.temp.path().join("audit/unknown-control.json"),
            json!({"phase": phase, "cancel": cancel}).to_string(),
        )
        .unwrap();
    }

    fn execute_unknown(&self, app: &Arc<Application>, id: i64, cancel: bool) -> RunResult {
        let running = Arc::clone(app);
        let worker = std::thread::spawn(move || running.work_once());
        if cancel {
            let marker = self.temp.path().join("audit/unknown-started");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            while !marker.exists() {
                if worker.is_finished() {
                    let completed = worker.join();
                    panic!(
                        "worker exited before unknown fixture phase: result={completed:?}; task={:?}; status={:?}; last supervisor={:?}",
                        app.get(id),
                        app.status(),
                        self.events("supervisor").last(),
                    );
                }
                if std::time::Instant::now() >= deadline {
                    let before = app.status();
                    let cancellation = app.cancel(id);
                    // Never detach a running fixture when synchronization fails.
                    let completed = worker.join();
                    panic!(
                        "unknown fixture phase did not start within 60 seconds: status before cancel={before:?}; cancellation={cancellation:?}; result={completed:?}; task={:?}; status={:?}; last supervisor={:?}",
                        app.get(id),
                        app.status(),
                        self.events("supervisor").last(),
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert_eq!(app.cancel(id).unwrap()["requested"], true);
        }
        assert!(matches!(
            worker.join().unwrap(),
            Err(relay_app::Error::RecoveryRequired)
        ));
        let task = app.get(id).unwrap();
        assert_eq!(task.state, relay::State::Claimed);
        assert!(task.result.is_none());
        let status = app.status().unwrap();
        assert_eq!(status["active"]["id"], id);
        assert_eq!(status["recovery_required"], true);
        let diagnostic: RunResult =
            serde_json::from_str(status["diagnostic"].as_str().unwrap()).unwrap();
        assert_eq!(
            diagnostic.outcome,
            Outcome::Unknown,
            "{}",
            diagnostic.to_json()
        );
        assert!(diagnostic.draft_pr.is_none());
        let workflow = diagnostic.workflow.as_ref().unwrap();
        assert!(workflow.publication.is_none());
        assert!(workflow.reviewed_sha.is_none());
        assert!(self.temp.path().join("audit/unknown-returned").is_file());
        assert!(
            self.events("post-unknown").is_empty(),
            "host started a process after Unknown"
        );
        let workspace = diagnostic.workspace.as_ref().unwrap();
        assert_eq!(
            fs::read_to_string(workspace.join("repository/original.txt")).unwrap(),
            "mutation while cleanup is unknown\n"
        );
        assert!(!workspace.join("publication-attempt.json").exists());
        assert!(
            app.continue_review(id, request("cannot-continue-unknown"))
                .is_err()
        );
        assert!(
            app.retry(
                id,
                RetryRequest {
                    workspace_quota_bytes: None,
                    key: "cannot-retry-unknown".into(),
                    confirm_stopped_and_reconciled: true
                }
            )
            .is_err()
        );
        assert!(app.get_view(id).unwrap().continuation_status.is_none());
        diagnostic
    }
}

#[test]
fn unknown_normal_reviewer_retains_claim_despite_candidate_mutation_and_cancellation() {
    for cancel in [false, true] {
        let mut fixture = Fixture::new();
        fixture.unknown_supervisor();
        fixture.make_phase_unknown("reviewer", cancel);
        let app = fixture.open();
        app.submit(fixture.input()).unwrap();
        let diagnostic = fixture.execute_unknown(&app, 1, cancel);
        assert_eq!(
            diagnostic
                .workflow
                .as_ref()
                .unwrap()
                .rounds
                .last()
                .unwrap()
                .reviewer
                .as_ref()
                .unwrap()
                .outcome,
            Outcome::Unknown
        );
        fixture.assert_calls(1, 1, 0);
        assert_eq!(app.list(None).unwrap().len(), 1);
    }
}

#[test]
fn unknown_review_only_tests_or_reviewer_never_downgrade_or_release_claim() {
    for (phase, cancel) in [
        ("test", false),
        ("test", true),
        ("reviewer", false),
        ("reviewer", true),
    ] {
        let mut fixture = Fixture::new();
        fixture.unknown_supervisor();
        let (app, _) = fixture.failed_review();
        let original = app.get(1).unwrap();
        let child = app
            .continue_review(1, request("review-may-be-unknown"))
            .unwrap();
        fixture.make_phase_unknown(phase, cancel);
        let diagnostic = fixture.execute_unknown(&app, child.id, cancel);
        assert!(diagnostic.agent.is_none());
        let round = diagnostic.workflow.as_ref().unwrap().rounds.last().unwrap();
        if phase == "test" {
            assert_eq!(diagnostic.tests.as_ref().unwrap().outcome, Outcome::Unknown);
            assert!(round.reviewer.is_none());
            fixture.assert_calls(1, 1, 1);
        } else {
            assert_eq!(diagnostic.tests.as_ref().unwrap().outcome, Outcome::Success);
            assert_eq!(round.reviewer.as_ref().unwrap().outcome, Outcome::Unknown);
            fixture.assert_calls(1, 2, 1);
        }
        assert_eq!(app.get(1).unwrap(), original);
        assert_eq!(app.list(None).unwrap().len(), 2);
    }
}
