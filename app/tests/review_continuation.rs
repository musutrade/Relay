#![cfg(target_os = "linux")]

use relay_app::{
    Application, RetryRequest, ReviewAdoptionRequest, ReviewContinuationRequest, Submission,
    host::{HostConfig, Outcome, RunResult},
    workflow::ReviewAdoption,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
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
if mode == 'fail_always' or (count == 0 and mode not in ('approve', 'reject', 'malformed', 'prose')):
    print(json.dumps({'type': 'result', 'subtype': 'error_max_turns', 'is_error': True, 'session_id': sid, 'result': 'Maximum turns reached'}))
else:
    rejected = mode in ('reject', 'reject_resume')
    answer = json.dumps({'candidate_sha': os.environ['RELAY_CANDIDATE_SHA'], 'verdict': 'changes_requested' if rejected else 'approved', 'summary': 'Checked the preserved exact candidate', 'findings': ['A fixture issue remains'] if rejected else []})
    if mode == 'malformed': answer = 'A verdict was not produced'
    if mode == 'prose':
        summary = ('The preserved exact candidate was checked against the intended fixture behavior. ' +
            'The isolated checkout contains the expected committed text and no unrelated edits. ' * 10 +
            'Caveat: external integrations were not exercised and deployment remains a separate decision.')
        answer = ('Read-only review completed successfully. The complete verdict follows.\n```json\n' +
            json.dumps({'candidate_sha': os.environ['RELAY_CANDIDATE_SHA'], 'verdict': 'approved', 'summary': summary, 'findings': []}) +
            '\n```\nCaveat: this review approves only the exact local candidate, not future changes.')
    (root / 'reviewer-response.txt').write_text(answer)
    print(json.dumps({'type': 'result', 'subtype': 'success', 'is_error': False, 'session_id': sid, 'result': answer, 'permission_denials': []}))
"#;

const ADOPTION_PUBLISHER: &str = r#"#!/usr/bin/python3
import json, os, pathlib
sha = os.environ['RELAY_CANDIDATE_SHA']
assert sha == os.environ['RELAY_REVIEWED_SHA']
assert os.environ['RELAY_REVIEW_VERDICT'] == 'approved'
assert os.environ['RELAY_TEST_OUTCOME'] == 'success'
assert os.environ['RELAY_GITHUB_REPOSITORY'] == 'example/project'
with (pathlib.Path(os.environ['FIXTURE_AUDIT']) / 'publisher.jsonl').open('a') as log:
    log.write(json.dumps({'candidate_sha': sha, 'task_id': os.environ['RELAY_TASK_ID']}) + '\n')
print(json.dumps({'dry_run': True, 'draft': True, 'repository': 'example/project',
    'branch': 'relay/task-' + os.environ['RELAY_TASK_ID'] + '-g' + os.environ['RELAY_GENERATION'],
    'candidate_sha': sha, 'reconciliation_required': False}))
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
        key: key.into(),
        confirm_stopped_and_reconciled: true,
        revalidate_tests: true,
        review_focus: None,
    }
}

fn adoption_request(key: &str, candidate: &str, raw: &str) -> ReviewAdoptionRequest {
    ReviewAdoptionRequest {
        key: key.into(),
        confirm_stopped_and_reconciled: true,
        adoption: ReviewAdoption {
            candidate_sha: candidate.into(),
            raw_response: raw.into(),
            raw_sha256: format!("{:x}", Sha256::digest(raw.as_bytes())),
            confirm_complete_successful_response: true,
            confirm_entire_response_reviewed: true,
            accept_prior_host_tests: true,
        },
    }
}

fn approved_response(candidate: &str) -> String {
    json!({
        "candidate_sha": candidate,
        "verdict": "approved",
        "summary": "Operator supplied the complete approved review response",
        "findings": []
    })
    .to_string()
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

    fn prose_review(&self) -> (Arc<Application>, RunResult, String) {
        self.mode("prose");
        let app = self.open();
        app.submit(self.input()).unwrap();
        assert!(app.work_once().unwrap());
        let first = result(&app, 1);
        assert_eq!(first.outcome, Outcome::Failure, "{}", first.to_json());
        assert_eq!(
            first.error.as_deref(),
            Some("reviewer did not return the required JSON verdict")
        );
        let workflow = first.workflow.as_ref().unwrap();
        let round = workflow.rounds.last().unwrap();
        assert_eq!(round.tests.as_ref().unwrap().outcome, Outcome::Success);
        assert_eq!(round.reviewer.as_ref().unwrap().outcome, Outcome::Success);
        assert_eq!(round.reviewer.as_ref().unwrap().exit_code, Some(0));
        assert!(round.review.is_none());
        assert!(workflow.reviewed_sha.is_none());
        let raw = fs::read_to_string(self.temp.path().join("audit/reviewer-response.txt")).unwrap();
        assert!(raw.len() > 950 && raw.len() <= 4096);
        self.assert_calls(1, 1, 1);
        (app, first, raw)
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

#[test]
fn adoption_preserves_full_attested_response_and_original_failure_across_restart() {
    let fixture = Fixture::new();
    let (app, first, raw) = fixture.prose_review();
    let original = app.get(1).unwrap();
    let candidate = first
        .workflow
        .as_ref()
        .unwrap()
        .candidate_sha
        .as_ref()
        .unwrap();
    let root = first.workspace.as_ref().unwrap();
    let checkpoint = read_json(root.join("sessions/reviewer.json"));
    assert_eq!(checkpoint["ready"], true);
    let input = adoption_request("attested-review", candidate, &raw);
    let digest = input.adoption.raw_sha256.clone();
    let child = app.adopt_review(1, input).unwrap();
    let payload: Value = serde_json::from_str(&child.payload).unwrap();
    let saved = &payload["continuation"]["operator_adoption"]["request"];
    assert_eq!(saved["raw_response"], raw);
    assert_eq!(saved["raw_sha256"], digest);
    assert_eq!(saved["candidate_sha"], *candidate);
    assert_eq!(payload["continuation"]["predecessor_task_id"], 1);
    assert_eq!(payload["continuation"]["workspace_task_id"], 1);
    let original_payload: Value = serde_json::from_str(&original.payload).unwrap();
    for (key, value) in original_payload.as_object().unwrap() {
        if key != "continuation" {
            assert_eq!(payload[key], *value, "original job field {key}");
        }
    }
    assert_eq!(app.get(1).unwrap(), original);
    let mut direct = fixture.input();
    direct.key = "injected-adoption".into();
    direct.job.continuation = serde_json::from_str::<relay_app::host::Job>(&child.payload)
        .unwrap()
        .continuation;
    assert!(app.submit(direct).is_err());
    let altered_raw = format!("{raw}\nAn additional operator note.");
    assert!(
        app.adopt_review(
            1,
            adoption_request("attested-review", candidate, &altered_raw)
        )
        .is_err()
    );
    fixture.assert_calls(1, 1, 1);
    drop(app);

    let app = fixture.open();
    assert_eq!(
        app.adopt_review(1, adoption_request("attested-review", candidate, &raw))
            .unwrap(),
        child
    );
    assert!(app.work_once().unwrap());
    let completed = result(&app, child.id);
    assert_eq!(
        completed.outcome,
        Outcome::Success,
        "{}",
        completed.to_json()
    );
    assert!(completed.agent.is_none());
    assert_eq!(completed.workspace.as_ref(), Some(root));
    let workflow = completed.workflow.as_ref().unwrap();
    assert_eq!(workflow.review_continuation, Some(1));
    assert_eq!(workflow.candidate_sha.as_ref(), Some(candidate));
    assert_eq!(workflow.reviewed_sha.as_ref(), Some(candidate));
    assert!(workflow.publication.is_none());
    let review = workflow.rounds.last().unwrap().review.as_ref().unwrap();
    assert!(workflow.rounds.last().unwrap().reviewer.is_none());
    assert_eq!(
        serde_json::to_value(&completed.tests).unwrap(),
        serde_json::to_value(&first.tests).unwrap()
    );
    let expected: Value = serde_json::from_str(
        raw.split("```json\n")
            .nth(1)
            .unwrap()
            .split("\n```")
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(review.summary, expected["summary"].as_str().unwrap());
    assert!(review.summary.len() > 950);
    assert!(
        review
            .summary
            .ends_with("deployment remains a separate decision.")
    );
    let value: Value = serde_json::from_str(&completed.to_json()).unwrap();
    let receipt = &value["workflow"]["operator_adoption"];
    assert_eq!(receipt["provenance"], "operator_attested");
    assert_eq!(receipt["predecessor_task_id"], 1);
    assert_eq!(receipt["raw_sha256"], digest);
    assert_eq!(receipt["accepted_prior_host_tests"], true);
    assert_eq!(
        receipt["predecessor_result_sha256"],
        format!("{:x}", Sha256::digest(serde_json::to_vec(&first).unwrap()))
    );
    assert_eq!(app.get(1).unwrap(), original);
    assert_eq!(app.get(child.id).unwrap().payload, child.payload);
    assert_eq!(read_json(root.join("sessions/reviewer.json")), checkpoint);
    assert_eq!(
        git(&root.join("repository"), &["rev-parse", "HEAD"]),
        *candidate
    );
    assert!(completed.to_json().len() <= relay::MAX_RESULT_BYTES);
    fixture.assert_calls(1, 1, 1);
    drop(app);

    let app = fixture.open();
    assert_eq!(
        app.adopt_review(1, adoption_request("attested-review", candidate, &raw))
            .unwrap()
            .id,
        child.id
    );
    assert!(!app.work_once().unwrap());
    assert_eq!(app.list(None).unwrap().len(), 2);
    assert_eq!(app.get(1).unwrap(), original);
    fixture.assert_calls(1, 1, 1);
}

#[test]
fn adoption_requires_all_attestations_and_exact_lowercase_digest_before_reservation() {
    let fixture = Fixture::new();
    let (app, first, raw) = fixture.prose_review();
    let candidate = first
        .workflow
        .as_ref()
        .unwrap()
        .candidate_sha
        .as_ref()
        .unwrap();
    for field in ["stopped", "complete", "reviewed", "prior-tests"] {
        let mut input = adoption_request("invalid-attestation", candidate, &raw);
        match field {
            "stopped" => input.confirm_stopped_and_reconciled = false,
            "complete" => input.adoption.confirm_complete_successful_response = false,
            "reviewed" => input.adoption.confirm_entire_response_reviewed = false,
            "prior-tests" => input.adoption.accept_prior_host_tests = false,
            _ => unreachable!(),
        }
        assert!(app.adopt_review(1, input).is_err(), "{field}");
    }
    for field in [
        "candidate_sha",
        "raw_response",
        "raw_sha256",
        "confirm_complete_successful_response",
        "confirm_entire_response_reviewed",
        "accept_prior_host_tests",
    ] {
        let input = adoption_request("missing-attestation", candidate, &raw);
        let mut value = json!({
            "key": input.key,
            "confirm_stopped_and_reconciled": input.confirm_stopped_and_reconciled,
            "adoption": input.adoption,
        });
        value["adoption"].as_object_mut().unwrap().remove(field);
        let decoded = serde_json::from_value::<ReviewAdoptionRequest>(value);
        assert!(
            decoded.is_err() || app.adopt_review(1, decoded.unwrap()).is_err(),
            "{field}"
        );
    }
    for digest in [
        String::new(),
        "f".repeat(64),
        "0".repeat(63),
        "g".repeat(64),
        format!("{:X}", Sha256::digest(raw.as_bytes())),
    ] {
        let mut input = adoption_request("invalid-digest", candidate, &raw);
        input.adoption.raw_sha256 = digest;
        assert!(app.adopt_review(1, input).is_err());
    }
    let mut changed = adoption_request("changed-raw", candidate, &raw);
    changed.adoption.raw_response.push('\n');
    assert!(app.adopt_review(1, changed).is_err());
    let mut wrong = adoption_request("wrong-candidate", candidate, &raw);
    wrong.adoption.candidate_sha = "f".repeat(40);
    assert!(app.adopt_review(1, wrong).is_err());
    for oversized in [
        format!("{raw}{}", "x".repeat(4097)),
        format!("{raw}{}", "界".repeat(1366)),
    ] {
        assert!(
            app.adopt_review(
                1,
                adoption_request("oversized-response", candidate, &oversized)
            )
            .is_err()
        );
    }
    assert_eq!(app.list(None).unwrap().len(), 1);
    assert!(app.get_view(1).unwrap().continuation_status.is_none());
    fixture.assert_calls(1, 1, 1);
    assert!(
        app.adopt_review(
            1,
            adoption_request("valid-after-rejections", candidate, &raw)
        )
        .is_ok()
    );
}

#[test]
fn adoption_cli_reads_bounded_regular_requests_and_returns_one_durable_child() {
    let fixture = Fixture::new();
    let (app, first, raw) = fixture.prose_review();
    let original = app.get(1).unwrap();
    let candidate = first
        .workflow
        .as_ref()
        .unwrap()
        .candidate_sha
        .as_ref()
        .unwrap();
    let config = fixture.temp.path().join("config.json");
    fs::write(&config, serde_json::to_vec(&fixture.config).unwrap()).unwrap();
    let request_path = fixture.temp.path().join("adoption.json");
    let input = adoption_request("cli-adoption", candidate, &raw);
    let valid = json!({
        "key": input.key,
        "confirm_stopped_and_reconciled": input.confirm_stopped_and_reconciled,
        "adoption": input.adoption,
    });
    let invoke = |path: &Path| {
        Command::new(env!("CARGO_BIN_EXE_relay-app"))
            .arg("adopt-review")
            .arg(&config)
            .arg(fixture.db())
            .arg("1")
            .arg(path)
            .output()
            .unwrap()
    };
    let mut invalid = valid.clone();
    invalid["confirm_stopped_and_reconciled"] = json!(false);
    fs::write(&request_path, invalid.to_string()).unwrap();
    let denied = invoke(&request_path);
    assert!(!denied.status.success());
    assert!(denied.stdout.is_empty());
    fs::write(&request_path, " ".repeat(16 * 1024 + 1)).unwrap();
    let oversized = invoke(&request_path);
    assert!(!oversized.status.success());
    assert!(String::from_utf8_lossy(&oversized.stderr).contains("exceeds 16 KiB"));
    fs::write(&request_path, valid.to_string()).unwrap();
    let link = fixture.temp.path().join("adoption-link.json");
    std::os::unix::fs::symlink(&request_path, &link).unwrap();
    assert!(!invoke(&link).status.success());
    let directory = fixture.temp.path().join("request-directory");
    fs::create_dir(&directory).unwrap();
    let nonregular = invoke(&directory);
    assert!(!nonregular.status.success());
    assert!(String::from_utf8_lossy(&nonregular.stderr).contains("regular JSON file"));
    assert_eq!(app.list(None).unwrap().len(), 1);
    assert_eq!(app.get(1).unwrap(), original);
    fixture.assert_calls(1, 1, 1);

    let accepted = invoke(&request_path);
    assert!(
        accepted.status.success(),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    let response: Value = serde_json::from_slice(&accepted.stdout).unwrap();
    let child = app.get(response["id"].as_i64().unwrap()).unwrap();
    assert_eq!(child.id, 2);
    assert_eq!(child.state, relay::State::Queued);
    assert_eq!(serde_json::to_value(&child).unwrap(), response);
    assert!(app.work_once().unwrap());
    assert_eq!(result(&app, child.id).outcome, Outcome::Success);
    let duplicate = invoke(&request_path);
    assert!(
        duplicate.status.success(),
        "{}",
        String::from_utf8_lossy(&duplicate.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&duplicate.stdout).unwrap()["id"],
        child.id
    );
    assert!(!app.work_once().unwrap());
    assert_eq!(app.list(None).unwrap().len(), 2);
    assert_eq!(app.get(1).unwrap(), original);
    fixture.assert_calls(1, 1, 1);
}

#[test]
fn adoption_cross_connection_duplicate_reserves_one_immutable_successor() {
    let fixture = Fixture::new();
    let (app, first, raw) = fixture.prose_review();
    let candidate = first.workflow.unwrap().candidate_sha.unwrap();
    let other = fixture.open();
    let barrier = Arc::new(Barrier::new(2));
    let left = {
        let app = Arc::clone(&app);
        let barrier = Arc::clone(&barrier);
        let input = adoption_request("left-adoption", &candidate, &raw);
        std::thread::spawn(move || {
            barrier.wait();
            app.adopt_review(1, input).unwrap()
        })
    };
    let right = std::thread::spawn(move || {
        barrier.wait();
        other
            .adopt_review(1, adoption_request("right-adoption", &candidate, &raw))
            .unwrap()
    });
    let child = left.join().unwrap();
    assert_eq!(right.join().unwrap(), child);
    assert_eq!(app.list(None).unwrap().len(), 2);
    assert!(app.work_once().unwrap());
    assert_eq!(result(&app, child.id).outcome, Outcome::Success);
    assert!(!app.work_once().unwrap());
    fixture.assert_calls(1, 1, 1);
}

#[test]
fn adoption_rejects_ambiguous_fences_objects_rejections_and_false_candidate_sha() {
    let fixture = Fixture::new();
    let (app, first, raw) = fixture.prose_review();
    let candidate = first
        .workflow
        .as_ref()
        .unwrap()
        .candidate_sha
        .as_ref()
        .unwrap();
    let approved = approved_response(candidate);
    let rejected = raw
        .replace("\"approved\"", "\"changes_requested\"")
        .replace(
            "\"findings\": []",
            "\"findings\": [\"A material issue remains\"]",
        );
    let invalid = [
        format!("{raw}\n{approved}"),
        format!("{raw}\n```json\n{approved}\n```"),
        format!("{raw}\n```text\nAn extra code block\n```"),
        format!("{raw}\n{{\"another\": true}}"),
        raw.replace("```json\n", "```javascript\n"),
        raw.replace("\n```\nCaveat", "\nCaveat"),
        raw.replace(candidate, &"f".repeat(40)),
        rejected,
    ];
    for (index, response) in invalid.iter().enumerate() {
        assert!(
            app.adopt_review(
                1,
                adoption_request(&format!("invalid-format-{index}"), candidate, response)
            )
            .is_err(),
            "invalid response {index}"
        );
    }
    assert_eq!(app.list(None).unwrap().len(), 1);
    fixture.assert_calls(1, 1, 1);
}

#[test]
fn adoption_cannot_turn_an_unsuccessful_reviewer_or_failed_test_into_approval() {
    for mode in [
        "fail_always",
        "approve",
        "reject",
        "tests-failed",
        "developer-failed",
        "queued",
        "claimed",
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
        app.submit(fixture.input()).unwrap();
        if mode == "claimed" {
            relay::Store::open(fixture.db())
                .unwrap()
                .claim_next("stopped-fixture")
                .unwrap()
                .unwrap();
        } else if mode != "queued" {
            assert!(app.work_once().unwrap());
        }
        let task = app.get(1).unwrap();
        let candidate = task
            .result
            .as_deref()
            .and_then(|text| serde_json::from_str::<RunResult>(text).ok())
            .and_then(|run| run.workflow.and_then(|workflow| workflow.candidate_sha))
            .unwrap_or_else(|| "f".repeat(40));
        let calls = (
            fixture.events("developer").len(),
            fixture.events("test").len(),
            fixture.events("reviewer").len(),
        );
        assert!(
            app.adopt_review(
                1,
                adoption_request(
                    "ineligible-adoption",
                    &candidate,
                    &approved_response(&candidate)
                )
            )
            .is_err(),
            "{mode}"
        );
        assert_eq!(app.get(1).unwrap(), task);
        assert_eq!(app.list(None).unwrap().len(), 1, "{mode}");
        fixture.assert_calls(calls.0, calls.1, calls.2);
    }
}

#[test]
fn adoption_rechecks_candidate_index_checkpoint_result_and_publication_before_execution() {
    for mutation in [
        "raw",
        "hidden-raw",
        "index",
        "head",
        "reviewer-raw",
        "checkpoint",
        "checkpoint-ready",
        "last-result",
        "publication",
    ] {
        let fixture = Fixture::new();
        let (app, first, raw) = fixture.prose_review();
        let original = app.get(1).unwrap();
        let candidate = first
            .workflow
            .as_ref()
            .unwrap()
            .candidate_sha
            .as_ref()
            .unwrap();
        let root = first.workspace.as_ref().unwrap();
        let repository = root.join("repository");
        let child = app
            .adopt_review(1, adoption_request("queued-adoption", candidate, &raw))
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
            "reviewer-raw" => fs::write(
                root.join("reviewer-repository/original.txt"),
                "reviewer mutation\n",
            )
            .unwrap(),
            "checkpoint" => fs::remove_file(root.join("sessions/reviewer.json")).unwrap(),
            "checkpoint-ready" => {
                let path = root.join("sessions/reviewer.json");
                let mut checkpoint = read_json(&path);
                checkpoint["ready"] = json!(false);
                fs::write(path, checkpoint.to_string()).unwrap();
            }
            "last-result" => {
                let path = root.join("last-result.json");
                let mut value = read_json(&path);
                value["error"] = json!("Changed predecessor diagnostic after submission");
                fs::write(path, value.to_string()).unwrap();
            }
            "publication" => fs::write(root.join("publication-attempt.json"), "{}").unwrap(),
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
        assert!(
            next.workflow
                .as_ref()
                .is_none_or(|workflow| workflow.publication.is_none())
        );
        assert_eq!(app.get(1).unwrap(), original);
        fixture.assert_calls(1, 1, 1);
    }
}

#[test]
fn adoption_profile_drift_and_preexisting_publication_marker_fail_closed() {
    for mutation in ["profile", "publication"] {
        for queued in [false, true] {
            let mut fixture = Fixture::new();
            let (app, first, raw) = fixture.prose_review();
            let candidate = first
                .workflow
                .as_ref()
                .unwrap()
                .candidate_sha
                .as_ref()
                .unwrap();
            let root = first.workspace.as_ref().unwrap();
            let child = queued.then(|| {
                app.adopt_review(1, adoption_request("before-drift", candidate, &raw))
                    .unwrap()
            });
            match mutation {
                "profile" => {
                    fixture
                        .config
                        .native_agents
                        .get_mut("reviewer")
                        .unwrap()
                        .max_turns = Some(3)
                }
                "publication" => fs::write(root.join("publication-attempt.json"), "{}").unwrap(),
                _ => unreachable!(),
            }
            let changed = fixture.open();
            if let Some(child) = child {
                assert!(changed.work_once().unwrap());
                assert_eq!(
                    result(&changed, child.id).outcome,
                    Outcome::Failure,
                    "{mutation}"
                );
            } else {
                assert!(
                    changed
                        .adopt_review(1, adoption_request("after-drift", candidate, &raw))
                        .is_err(),
                    "{mutation}"
                );
                assert_eq!(changed.list(None).unwrap().len(), 1);
            }
            fixture.assert_calls(1, 1, 1);
        }
    }
}

#[test]
fn adoption_uses_only_original_publication_choice_and_duplicate_does_not_republish() {
    for publish in [false, true] {
        let mut fixture = Fixture::new();
        // The existing publisher requires a complete JSON terminal record.
        fixture.config.output_limit_bytes = 4096;
        let publisher = fixture.temp.path().join("publisher");
        executable(&publisher, ADOPTION_PUBLISHER);
        fixture.config.draft_pr_adapters.insert(
            "publisher".into(),
            serde_json::from_value(json!({
                "program": publisher, "env": {"FIXTURE_AUDIT": fixture.temp.path().join("audit")}
            }))
            .unwrap(),
        );
        let workflow = fixture.config.workflows.get_mut("checked").unwrap();
        workflow.draft_pr_adapter = Some("publisher".into());
        workflow.github_repository = Some("example/project".into());
        fixture.mode("prose");
        let app = fixture.open();
        let mut input = fixture.input();
        input.job.publish = publish;
        input.job.draft_pr_adapter = publish.then(|| "publisher".into());
        app.submit(input).unwrap();
        assert!(app.work_once().unwrap());
        let first = result(&app, 1);
        assert_eq!(first.outcome, Outcome::Failure);
        assert!(fixture.events("publisher").is_empty());
        let original = app.get(1).unwrap();
        let candidate = first
            .workflow
            .as_ref()
            .unwrap()
            .candidate_sha
            .as_ref()
            .unwrap();
        let raw =
            fs::read_to_string(fixture.temp.path().join("audit/reviewer-response.txt")).unwrap();
        let child = app
            .adopt_review(1, adoption_request("publish-adoption", candidate, &raw))
            .unwrap();
        assert!(app.work_once().unwrap());
        let completed = result(&app, child.id);
        assert_eq!(
            completed.outcome,
            Outcome::Success,
            "{}",
            completed.to_json()
        );
        assert_eq!(
            completed.workflow.as_ref().unwrap().publication.is_some(),
            publish
        );
        assert_eq!(fixture.events("publisher").len(), usize::from(publish));
        if publish {
            assert_eq!(fixture.events("publisher")[0]["candidate_sha"], *candidate);
        }
        drop(app);
        let app = fixture.open();
        assert_eq!(
            app.adopt_review(1, adoption_request("publish-adoption", candidate, &raw))
                .unwrap()
                .id,
            child.id
        );
        assert!(!app.work_once().unwrap());
        assert_eq!(fixture.events("publisher").len(), usize::from(publish));
        assert_eq!(app.get(1).unwrap(), original);
        fixture.assert_calls(1, 1, 1);
    }
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
