#![cfg(target_os = "linux")]

use relay::{State, Store};
use relay_app::{
    Application, PublishApprovedRequest, RetryRequest, ReviewContinuationRequest, Submission,
    host::{HostConfig, Outcome, RunResult},
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

// Every configured command is an offline fixture. Audit logs live outside the
// guarded candidate checkout, and publisher success never contacts GitHub.
const DEVELOPER: &str = r#"#!/usr/bin/python3
import json, os, pathlib, sys
prompt = sys.stdin.read()
root = pathlib.Path(os.environ['FIXTURE_AUDIT'])
with (root / 'developer.jsonl').open('a') as log:
    log.write(json.dumps({'prompt': prompt}) + '\n')
pathlib.Path('changed.txt').write_text('exact approved candidate\n')
print('implemented fixture')
"#;
const TEST: &str = r#"#!/usr/bin/python3
import json, os, pathlib, subprocess
sha = os.environ['RELAY_CANDIDATE_SHA']
assert subprocess.check_output(['/usr/bin/git', 'rev-parse', 'HEAD'], text=True).strip() == sha
assert subprocess.check_output(['/usr/bin/git', 'show', sha + ':changed.txt'], text=True) == pathlib.Path('changed.txt').read_text()
with (pathlib.Path(os.environ['FIXTURE_AUDIT']) / 'test.jsonl').open('a') as log:
    log.write(json.dumps({'candidate_sha': sha}) + '\n')
print('fixture tests passed')
"#;
const REVIEWER: &str = r#"#!/usr/bin/python3
import json, os, pathlib, sys
if '--version' in sys.argv:
    print('2.1.281 (Claude Code)'); sys.exit()
if '--help' in sys.argv:
    print('--model --output-format --verbose --permission-prompts --restricted --tools --allowedTools --disallowedTools --disable-slash-commands --strict-mcp-config --mcp-config --no-session-persistence --session-id --resume --max-turns --max-budget-usd'); sys.exit()
root = pathlib.Path(os.environ['FIXTURE_AUDIT'])
sha = os.environ['RELAY_CANDIDATE_SHA']
prompt = sys.stdin.read()
assert '--restricted' in sys.argv and 'Read,Glob,Grep' in sys.argv
assert pathlib.Path.cwd().name == 'reviewer-repository'
sid = sys.argv[sys.argv.index('--session-id') + 1]
with (root / 'reviewer.jsonl').open('a') as log:
    log.write(json.dumps({'candidate_sha': sha, 'session_id': sid, 'prompt': prompt}) + '\n')
print(json.dumps({'type': 'system', 'subtype': 'init', 'session_id': sid, 'model': 'fixture-model'}))
answer = json.dumps({'candidate_sha': sha, 'verdict': 'approved', 'summary': 'Checked the exact fixture candidate; external services were not exercised', 'findings': []})
print(json.dumps({'type': 'result', 'subtype': 'success', 'is_error': False, 'session_id': sid, 'result': answer, 'permission_denials': []}))
"#;
const PUBLISHER: &str = r#"#!/usr/bin/python3
import json, os, pathlib, subprocess, sys
sha = os.environ['RELAY_CANDIDATE_SHA']
assert sha == os.environ['RELAY_REVIEWED_SHA']
assert subprocess.check_output(['/usr/bin/git', 'rev-parse', 'HEAD'], text=True).strip() == sha
assert os.environ['RELAY_REVIEW_VERDICT'] == 'approved'
assert os.environ['RELAY_TEST_OUTCOME'] == 'success'
assert os.environ['RELAY_GITHUB_REPOSITORY'] == 'example/project'
assert os.environ['RELAY_GITHUB_BASE'] == 'main'
assert os.environ['RELAY_DRAFT_PR'] == 'true'
assert os.environ['RELAY_GITHUB_EXECUTE'] == '0'
root = pathlib.Path(os.environ['FIXTURE_AUDIT'])
with (root / 'publisher.jsonl').open('a') as log:
    log.write(json.dumps({'candidate_sha': sha, 'base_sha': os.environ['RELAY_BASE_SHA'], 'task_id': os.environ['RELAY_TASK_ID'], 'evidence': os.environ['RELAY_REVIEW_EVIDENCE']}) + '\n')
if (root / 'publisher-fails').exists():
    print('effect status needs reconciliation', file=sys.stderr); sys.exit(7)
print(json.dumps({'dry_run': True, 'draft': True, 'repository': 'example/project', 'branch': 'relay/task-' + os.environ['RELAY_TASK_ID'] + '-g' + os.environ['RELAY_GENERATION'], 'candidate_sha': sha, 'reconciliation_required': False}))
"#;
const UNKNOWN_SUPERVISOR: &str = r#"#!/usr/bin/python3
import json, pathlib, subprocess, sys
line = sys.stdin.buffer.readline()
spec = json.loads(line)
child = subprocess.Popen([__SUPERVISOR__, '__relay_host_supervisor'], stdin=subprocess.PIPE, stdout=subprocess.PIPE, pass_fds=(198,) if spec.get('workspace_lease') else ())
child.stdin.write(line)
child.stdin.flush()
output = child.stdout.read()
child.wait()
child.stdin.close()
if spec['program'] == __PUBLISHER__ and pathlib.Path(__CONTROL__).exists():
    print(json.dumps({'outcome': 'unknown', 'exit_code': None, 'signal': None, 'stdout': '', 'stderr': '', 'stdout_truncated': False, 'stderr_truncated': False, 'duration_ms': 0, 'supervisor_pid': None, 'error': 'fixture cannot attest publisher process cleanup'}))
else:
    sys.stdout.buffer.write(output)
sys.exit(child.returncode)
"#;

// A concurrently forked fixture can momentarily inherit another fixture's
// CLOEXEC workspace lease. Keep lifetimes serial while explicitly testing DB races.
static FIXTURES: Mutex<()> = Mutex::new(());
struct Fixture {
    temp: TempDir,
    config: HostConfig,
    _serial: MutexGuard<'static, ()>,
}
fn executable(path: &Path, script: &str) {
    fs::write(path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("/usr/bin/git")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .current_dir(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
fn read_json(path: impl AsRef<Path>) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
fn change(path: impl AsRef<Path>, field: &str, value: Value) {
    let path = path.as_ref();
    let mut record = read_json(path);
    record[field] = value;
    fs::write(path, record.to_string()).unwrap();
}
fn result(app: &Application, id: i64) -> RunResult {
    serde_json::from_str(app.get(id).unwrap().result.as_deref().unwrap()).unwrap()
}
fn action(app: &Application, id: i64) -> Value {
    let operator = app.operator(id).unwrap();
    operator["recovery"]["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == "publish_approved")
        .unwrap_or_else(|| panic!("publish action absent: {operator}"))
        .clone()
}
fn request_value(key: &str, scope: &Value) -> Value {
    json!({"key": key, "confirm_publish": true, "accept_prior_test_evidence": true,
        "candidate_sha": scope["candidate_sha"], "github_repository": scope["github_repository"],
        "base_branch": scope["base_branch"], "draft_pr_adapter": scope["draft_pr_adapter"],
        "publisher_binding": scope["publisher_binding"]})
}
fn request(key: &str, scope: &Value) -> PublishApprovedRequest {
    serde_json::from_value(request_value(key, scope)).unwrap()
}
fn retry(key: &str) -> RetryRequest {
    serde_json::from_value(json!({"key": key, "confirm_stopped_and_reconciled": true})).unwrap()
}
fn review(key: &str) -> ReviewContinuationRequest {
    serde_json::from_value(
        json!({"key": key, "confirm_stopped_and_reconciled": true, "revalidate_tests": true}),
    )
    .unwrap()
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
        for (name, script) in [
            ("developer", DEVELOPER),
            ("test", TEST),
            ("reviewer", REVIEWER),
            ("publisher", PUBLISHER),
        ] {
            executable(&temp.path().join(name), script);
        }
        let env = json!({"FIXTURE_AUDIT": audit});
        let config = serde_json::from_value(json!({
            "workspace_root": temp.path().join("runs"), "repositories": {"fixture": source},
            "agents": {"developer": {"program": temp.path().join("developer"), "env": env}},
            "tests": {"check": {"program": temp.path().join("test"), "env": env}},
            "native_agents": {"reviewer": {"provider": "claude_cli", "program": temp.path().join("reviewer"), "env": env, "session_continuity": true, "max_turns": 2}},
            "draft_pr_adapters": {"publish": {"program": temp.path().join("publisher"), "env": env}},
            "workflows": {"checked": {"repository": "fixture", "developer": "developer", "reviewer": "reviewer", "test": "check", "draft_pr_adapter": "publish", "github_repository": "example/project", "max_repairs": 0}},
            "timeout_seconds": 60, "output_limit_bytes": 128, "max_retained_workspaces": 1,
            "supervisor_program": env!("CARGO_BIN_EXE_relay-app")
        })).unwrap();
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
        serde_json::from_value(json!({"key": "original", "job": {"repository": "fixture", "agent": "developer", "workflow": "checked", "requirements": "Implement the exact approved fixture", "publish": false}})).unwrap()
    }
    fn approved(&self) -> (Arc<Application>, RunResult, Value) {
        let app = self.open();
        let original = app.submit(self.input()).unwrap();
        assert_eq!(original.id, 1);
        assert!(app.work_once().unwrap());
        let first = result(&app, 1);
        assert_eq!(first.outcome, Outcome::Success, "{}", first.to_json());
        let workflow = first.workflow.as_ref().unwrap();
        assert!(workflow.publication.is_none());
        assert_eq!(workflow.candidate_sha, workflow.reviewed_sha);
        assert_eq!(first.tests.as_ref().unwrap().outcome, Outcome::Success);
        self.assert_calls(1, 1, 1, 0);
        let scope = action(&app, 1);
        (app, first, scope)
    }
    fn events(&self, stage: &str) -> Vec<Value> {
        match fs::read_to_string(
            self.temp
                .path()
                .join("audit")
                .join(format!("{stage}.jsonl")),
        ) {
            Ok(text) => text
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("cannot read fixture audit: {error}"),
        }
    }
    fn assert_calls(&self, developer: usize, tests: usize, reviewer: usize, publisher: usize) {
        for (stage, expected) in [
            ("developer", developer),
            ("test", tests),
            ("reviewer", reviewer),
            ("publisher", publisher),
        ] {
            assert_eq!(self.events(stage).len(), expected, "{stage} calls");
        }
    }
    fn unknown_supervisor(&mut self) {
        let script = UNKNOWN_SUPERVISOR
            .replace(
                "__SUPERVISOR__",
                &json!(env!("CARGO_BIN_EXE_relay-app")).to_string(),
            )
            .replace(
                "__PUBLISHER__",
                &json!(self.config.draft_pr_adapters["publish"].program).to_string(),
            )
            .replace(
                "__CONTROL__",
                &json!(self.temp.path().join("audit/publisher-unknown")).to_string(),
            );
        let path = self.temp.path().join("supervisor");
        executable(&path, &script);
        self.config.supervisor_program = Some(path);
        self.config.timeout_seconds = 90;
    }
}

#[test]
fn approved_handoff_publishes_only_exact_candidate_and_preserves_evidence() {
    let fixture = Fixture::new();
    let (app, first, scope) = fixture.approved();
    let original = app.get(1).unwrap();
    let root = first.workspace.as_ref().unwrap();
    let checkpoint = fs::read(root.join("last-result.json")).unwrap();
    let before = serde_json::to_value(&first).unwrap();
    assert_eq!(scope["candidate_sha"], before["workflow"]["candidate_sha"]);
    assert_eq!(scope["base_sha"], before["workflow"]["base_sha"]);
    assert_eq!(scope["github_repository"], "example/project");
    assert_eq!(scope["base_branch"], "main");
    assert_eq!(scope["draft_pr_adapter"], "publish");
    assert_eq!(scope["draft"], true);
    assert_eq!(scope["dry_run"], true);
    assert_eq!(scope["authorization_ttl_seconds"], 86400);
    assert!(!scope["publisher_binding"].as_str().unwrap().is_empty());
    let child = app
        .publish_approved(1, request("publish-approved", &scope))
        .unwrap();
    let payload: Value = serde_json::from_str(&child.payload).unwrap();
    let pinned = &payload["continuation"]["publish_approved"];
    assert_eq!(payload["continuation"]["workspace_task_id"], 1);
    assert_eq!(payload["continuation"]["predecessor_task_id"], 1);
    assert_eq!(pinned["request"], request_value("publish-approved", &scope));
    assert_eq!(pinned["base_sha"], before["workflow"]["base_sha"]);
    assert_eq!(
        pinned["predecessor_result"],
        original.result.as_deref().unwrap()
    );
    assert_eq!(
        pinned["checkpoint_sha256"],
        format!("{:x}", Sha256::digest(&checkpoint))
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
    let after = serde_json::to_value(&completed).unwrap();
    assert_eq!(after["tests"], before["tests"]);
    for field in [
        "base_sha",
        "candidate_sha",
        "reviewed_sha",
        "rounds",
        "operator_adoption",
    ] {
        assert_eq!(
            after["workflow"][field], before["workflow"][field],
            "{field}"
        );
    }
    let receipt = &after["workflow"]["publish_approved"];
    assert_eq!(receipt["predecessor_task_id"], 1);
    assert_eq!(receipt["accepted_prior_test_evidence"], true);
    assert_eq!(receipt["provenance"], "inherited_approved_evidence");
    assert_eq!(
        receipt["predecessor_result_sha256"],
        pinned["predecessor_result_sha256"]
    );
    assert_eq!(
        after["workflow"]["publication"]["candidate_sha"],
        scope["candidate_sha"]
    );
    assert_eq!(after["workflow"]["publication"]["draft"], true);
    assert_eq!(after["workflow"]["publication"]["dry_run"], true);
    fixture.assert_calls(1, 1, 1, 1);
    assert_eq!(
        fixture.events("publisher")[0]["candidate_sha"],
        scope["candidate_sha"]
    );
    assert_eq!(
        fixture.events("publisher")[0]["base_sha"],
        scope["base_sha"]
    );
    assert_eq!(app.get(1).unwrap(), original);
    assert_eq!(app.get(child.id).unwrap().payload, child.payload);
    assert_eq!(
        fs::read_dir(&fixture.config.workspace_root)
            .unwrap()
            .count(),
        1
    );
    assert!(root.join("publication-attempt.json").is_file());
    assert!(completed.to_json().len() <= relay::MAX_RESULT_BYTES);
    assert!(!app.work_once().unwrap());
}

#[test]
fn explicit_confirmations_exact_scope_and_bounded_key_precede_reservation() {
    let fixture = Fixture::new();
    let (app, _, scope) = fixture.approved();
    for (publish, tests) in [(false, false), (false, true), (true, false)] {
        let mut input = request("missing-confirmation", &scope);
        input.confirm_publish = publish;
        input.accept_prior_test_evidence = tests;
        assert!(app.publish_approved(1, input).is_err());
    }
    for field in ["confirm_publish", "accept_prior_test_evidence"] {
        let mut value = request_value("missing-field", &scope);
        value.as_object_mut().unwrap().remove(field);
        if let Ok(input) = serde_json::from_value::<PublishApprovedRequest>(value) {
            assert!(app.publish_approved(1, input).is_err(), "{field}");
        }
    }
    for (field, value) in [
        ("candidate_sha", json!("f".repeat(40))),
        ("github_repository", json!("other/project")),
        ("base_branch", json!("other-base")),
        ("draft_pr_adapter", json!("other-publisher")),
        ("publisher_binding", json!("wrong-binding")),
    ] {
        let mut input = request_value("changed-scope", &scope);
        input[field] = value;
        assert!(
            app.publish_approved(1, serde_json::from_value(input).unwrap())
                .is_err(),
            "{field}"
        );
    }
    for key in [
        String::new(),
        "x".repeat(129),
        "界".repeat(43),
        "original".into(),
    ] {
        assert!(
            app.publish_approved(1, request(&key, &scope)).is_err(),
            "key={key}"
        );
    }
    for field in [
        "base_sha",
        "draft",
        "dry_run",
        "continuation",
        "expires_at_unix_seconds",
        "skip_tests",
    ] {
        let mut input = request_value("injected-scope", &scope);
        input[field] = json!(true);
        assert!(
            serde_json::from_value::<PublishApprovedRequest>(input).is_err(),
            "{field}"
        );
    }
    assert_eq!(app.list(None).unwrap().len(), 1);
    assert!(app.get_view(1).unwrap().continuation_status.is_none());
    fixture.assert_calls(1, 1, 1, 0);
    app.publish_approved(1, request("accepted", &scope))
        .unwrap();
}

#[test]
fn duplicate_and_restart_require_identical_request_and_reuse_successor() {
    let fixture = Fixture::new();
    let (app, _, scope) = fixture.approved();
    let first = app
        .publish_approved(1, request("same-request", &scope))
        .unwrap();
    assert_eq!(
        app.publish_approved(1, request("same-request", &scope))
            .unwrap(),
        first
    );
    assert!(
        app.publish_approved(1, request("different-key", &scope))
            .is_err()
    );
    let mut changed = request("same-request", &scope);
    changed.base_branch = "changed".into();
    assert!(app.publish_approved(1, changed).is_err());
    assert!(app.retry(1, retry("ordinary-retry")).is_err());
    assert!(app.continue_review(1, review("ordinary-review")).is_err());
    drop(app);
    let app = fixture.open();
    assert_eq!(
        app.publish_approved(1, request("same-request", &scope))
            .unwrap(),
        first
    );
    assert_eq!(app.list(None).unwrap().len(), 2);
    assert!(app.work_once().unwrap());
    let finished = app.get(first.id).unwrap();
    assert_eq!(
        app.publish_approved(1, request("same-request", &scope))
            .unwrap(),
        finished
    );
    assert!(!app.work_once().unwrap());
    fixture.assert_calls(1, 1, 1, 1);
}

#[test]
fn reservation_crash_gap_cannot_be_consumed_by_another_action() {
    let mut fixture = Fixture::new();
    fixture.config.successful_workspace_retention_seconds = Some(60);
    let (app, first, scope) = fixture.approved();
    let root = first.workspace.unwrap();
    let child = app
        .publish_approved(1, request("reserved-publication", &scope))
        .unwrap();
    drop(app);
    let db = rusqlite::Connection::open(fixture.db()).unwrap();
    db.execute(
        "UPDATE app_continuations SET task_id=NULL WHERE predecessor_id=1",
        [],
    )
    .unwrap();
    // A core submit may have committed before the task-id checkpoint. Reads
    // resolve the same immutable key/payload, and replay returns the same task.
    let app = fixture.open();
    assert_eq!(
        app.get_view(1)
            .unwrap()
            .continuation_status
            .unwrap()
            .successor_id,
        Some(child.id)
    );
    assert_eq!(
        app.publish_approved(1, request("reserved-publication", &scope))
            .unwrap(),
        child
    );
    drop(app);
    db.execute(
        "UPDATE app_continuations SET task_id=NULL WHERE predecessor_id=1",
        [],
    )
    .unwrap();
    db.execute("DELETE FROM tasks WHERE id=?1", [child.id])
        .unwrap();
    drop(db);
    let app = fixture.open();
    change(root.join("finished.json"), "finished_at", json!(1));
    assert_eq!(app.cleanup_completed().unwrap(), 0);
    assert!(root.exists());
    assert_eq!(
        app.workspace_inventory(None).unwrap()["workspaces"][0]["retention"]["status"],
        "protected"
    );
    assert!(app.retry(1, retry("wrong-retry")).is_err());
    assert!(app.continue_review(1, review("wrong-review")).is_err());
    assert!(
        app.publish_approved(1, request("wrong-key", &scope))
            .is_err()
    );
    assert_eq!(app.list(None).unwrap().len(), 1);
    let restored = app
        .publish_approved(1, request("reserved-publication", &scope))
        .unwrap();
    assert_eq!(restored.payload, child.payload);
    assert_eq!(restored.key, child.key);
    assert_eq!(
        app.publish_approved(1, request("reserved-publication", &scope))
            .unwrap(),
        restored
    );
    assert!(app.work_once().unwrap());
    assert_eq!(result(&app, restored.id).outcome, Outcome::Success);
    fixture.assert_calls(1, 1, 1, 1);
}

#[test]
fn cross_connection_same_key_and_different_key_races_create_one_successor() {
    for same_key in [true, false] {
        let fixture = Fixture::new();
        let (app, _, scope) = fixture.approved();
        let other = fixture.open();
        let barrier = Arc::new(Barrier::new(2));
        let left = {
            let app = Arc::clone(&app);
            let barrier = Arc::clone(&barrier);
            let input = request("left", &scope);
            std::thread::spawn(move || {
                barrier.wait();
                app.publish_approved(1, input)
            })
        };
        let input = request(if same_key { "left" } else { "right" }, &scope);
        let right = std::thread::spawn(move || {
            barrier.wait();
            other.publish_approved(1, input)
        });
        let left = left.join().unwrap();
        let right = right.join().unwrap();
        if same_key {
            assert_eq!(left.unwrap(), right.unwrap());
        } else {
            assert_ne!(
                left.is_ok(),
                right.is_ok(),
                "only the identical accepted request may resume"
            );
        }
        assert_eq!(app.list(None).unwrap().len(), 2);
        assert!(app.work_once().unwrap());
        assert!(!app.work_once().unwrap());
        fixture.assert_calls(1, 1, 1, 1);
    }
}

#[test]
fn direct_submission_cannot_inject_publication_continuation() {
    let fixture = Fixture::new();
    let (app, _, scope) = fixture.approved();
    let child = app
        .publish_approved(1, request("legitimate", &scope))
        .unwrap();
    let job: relay_app::host::Job = serde_json::from_str(&child.payload).unwrap();
    let mut input = fixture.input();
    input.key = "injected".into();
    input.job.continuation = job.continuation;
    assert!(app.submit(input).is_err());
    assert_eq!(app.list(None).unwrap().len(), 2);
    fixture.assert_calls(1, 1, 1, 0);
}

#[test]
fn configuration_and_same_named_publisher_drift_fail_before_effects() {
    for queued in [false, true] {
        for mutation in [
            "publisher-program",
            "publisher-env",
            "publisher-args",
            "base-branch",
            "target",
            "reviewer-config",
        ] {
            let mut fixture = Fixture::new();
            let (app, _, scope) = fixture.approved();
            let child = queued.then(|| {
                app.publish_approved(1, request("before-drift", &scope))
                    .unwrap()
            });
            match mutation {
                "publisher-program" => executable(
                    &fixture.config.draft_pr_adapters["publish"].program,
                    &format!("{PUBLISHER}\n# same name, replaced executable bytes\n"),
                ),
                "publisher-env" => {
                    fixture
                        .config
                        .draft_pr_adapters
                        .get_mut("publish")
                        .unwrap()
                        .env
                        .insert("EXTRA_SCOPE".into(), "changed".into());
                }
                "publisher-args" => fixture
                    .config
                    .draft_pr_adapters
                    .get_mut("publish")
                    .unwrap()
                    .args
                    .push("--changed".into()),
                "base-branch" => {
                    fixture
                        .config
                        .workflows
                        .get_mut("checked")
                        .unwrap()
                        .base_branch = "changed".into()
                }
                "target" => {
                    fixture
                        .config
                        .workflows
                        .get_mut("checked")
                        .unwrap()
                        .github_repository = Some("other/project".into())
                }
                "reviewer-config" => {
                    fixture
                        .config
                        .native_agents
                        .get_mut("reviewer")
                        .unwrap()
                        .max_turns = Some(3)
                }
                _ => unreachable!(),
            }
            let changed = fixture.open();
            if let Some(child) = child {
                assert!(changed.work_once().unwrap(), "{mutation}");
                let stopped = result(&changed, child.id);
                assert_eq!(
                    stopped.outcome,
                    Outcome::Failure,
                    "{mutation}: {}",
                    stopped.to_json()
                );
            } else {
                assert!(
                    changed
                        .publish_approved(1, request("after-drift", &scope))
                        .is_err(),
                    "{mutation}"
                );
                assert_eq!(changed.list(None).unwrap().len(), 1);
            }
            fixture.assert_calls(1, 1, 1, 0);
        }
    }
}

#[test]
fn changed_source_base_and_candidate_checkpoints_fail_closed() {
    for queued in [false, true] {
        for mutation in [
            "source-head",
            "candidate-head",
            "base-checkpoint",
            "candidate-checkpoint",
            "claim",
            "last-result",
            "last-result-missing",
            "publication-marker",
        ] {
            let fixture = Fixture::new();
            let (app, first, scope) = fixture.approved();
            let child = queued.then(|| {
                app.publish_approved(1, request("before-tampering", &scope))
                    .unwrap()
            });
            let root = first.workspace.unwrap();
            match mutation {
                "source-head" => {
                    git(
                        &fixture.config.repositories["fixture"],
                        &["commit", "--allow-empty", "-m", "base changed"],
                    );
                }
                "candidate-head" => {
                    git(
                        &root.join("repository"),
                        &["commit", "--allow-empty", "-m", "candidate changed"],
                    );
                }
                "base-checkpoint" => fs::write(
                    root.join("workflow-base.txt"),
                    json!("f".repeat(40)).to_string(),
                )
                .unwrap(),
                "candidate-checkpoint" => fs::write(
                    root.join("candidate-head.json"),
                    json!("f".repeat(40)).to_string(),
                )
                .unwrap(),
                "claim" => change(root.join("claim.json"), "task_id", json!(999)),
                "last-result" => change(
                    root.join("last-result.json"),
                    "error",
                    json!("tampered result evidence"),
                ),
                "last-result-missing" => fs::remove_file(root.join("last-result.json")).unwrap(),
                "publication-marker" => {
                    fs::write(root.join("publication-attempt.json"), "{}").unwrap()
                }
                _ => unreachable!(),
            }
            if let Some(child) = child {
                assert!(app.work_once().unwrap(), "{mutation}");
                let stopped = result(&app, child.id);
                assert_eq!(
                    stopped.outcome,
                    Outcome::Failure,
                    "{mutation}: {}",
                    stopped.to_json()
                );
            } else {
                assert!(
                    app.publish_approved(1, request("after-tampering", &scope))
                        .is_err(),
                    "{mutation}"
                );
                assert_eq!(app.list(None).unwrap().len(), 1);
            }
            fixture.assert_calls(1, 1, 1, 0);
        }
    }
}

#[test]
fn queued_raw_index_and_reviewer_checkout_changes_never_publish() {
    for mutation in [
        "raw",
        "hidden-raw",
        "index",
        "untracked",
        "reviewer-raw",
        "reviewer-checkpoint",
    ] {
        let fixture = Fixture::new();
        let (app, first, scope) = fixture.approved();
        let child = app
            .publish_approved(1, request("queued-before-mutation", &scope))
            .unwrap();
        let root = first.workspace.unwrap();
        let repository = root.join("repository");
        match mutation {
            "raw" => fs::write(repository.join("original.txt"), "changed\n").unwrap(),
            "hidden-raw" => {
                git(
                    &repository,
                    &["update-index", "--assume-unchanged", "original.txt"],
                );
                fs::write(repository.join("original.txt"), "hidden change\n").unwrap();
            }
            "index" => {
                fs::write(repository.join("original.txt"), "staged\n").unwrap();
                git(&repository, &["add", "original.txt"]);
                fs::write(repository.join("original.txt"), "original\n").unwrap();
            }
            "untracked" => fs::write(repository.join("user-work.txt"), "preserve this").unwrap(),
            "reviewer-raw" => fs::write(
                root.join("reviewer-repository/original.txt"),
                "reviewer changed\n",
            )
            .unwrap(),
            "reviewer-checkpoint" => {
                fs::write(root.join("reviewer-candidate.txt"), "f".repeat(40)).unwrap()
            }
            _ => unreachable!(),
        }
        assert!(app.work_once().unwrap());
        let stopped = result(&app, child.id);
        assert_eq!(
            stopped.outcome,
            Outcome::Failure,
            "{mutation}: {}",
            stopped.to_json()
        );
        fixture.assert_calls(1, 1, 1, 0);
        if mutation == "untracked" {
            assert_eq!(
                fs::read_to_string(repository.join("user-work.txt")).unwrap(),
                "preserve this"
            );
        }
    }
}

#[test]
fn failed_publication_is_never_eligible_for_another_effect_attempt() {
    let fixture = Fixture::new();
    let (app, first, scope) = fixture.approved();
    fs::write(fixture.temp.path().join("audit/publisher-fails"), "yes").unwrap();
    let child = app
        .publish_approved(1, request("publisher-fails", &scope))
        .unwrap();
    assert!(app.work_once().unwrap());
    let failed = result(&app, child.id);
    assert_eq!(failed.outcome, Outcome::Failure, "{}", failed.to_json());
    assert!(
        first
            .workspace
            .unwrap()
            .join("publication-attempt.json")
            .exists()
    );
    assert!(app.retry(child.id, retry("retry-effect")).is_err());
    assert!(
        app.continue_review(child.id, review("review-effect"))
            .is_err()
    );
    assert!(
        app.publish_approved(child.id, request("republish", &scope))
            .is_err()
    );
    assert!(
        app.publish_approved(1, request("different-attempt", &scope))
            .is_err()
    );
    assert_eq!(
        app.publish_approved(1, request("publisher-fails", &scope))
            .unwrap()
            .id,
        child.id
    );
    assert!(!app.work_once().unwrap());
    fixture.assert_calls(1, 1, 1, 1);
}

#[test]
fn unknown_publication_keeps_claim_and_manual_requeue_never_republishes() {
    for requeue_state in ["unchanged", "expired", "cancelled"] {
        let mut fixture = Fixture::new();
        fixture.unknown_supervisor();
        let (app, first, scope) = fixture.approved();
        fs::write(fixture.temp.path().join("audit/publisher-unknown"), "yes").unwrap();
        let child = app
            .publish_approved(1, request("unknown-effect", &scope))
            .unwrap();
        assert!(matches!(
            app.work_once(),
            Err(relay_app::Error::RecoveryRequired)
        ));
        let active = app.get(child.id).unwrap();
        assert_eq!(active.state, State::Claimed);
        assert!(active.result.is_none());
        assert!(
            first
                .workspace
                .unwrap()
                .join("publication-attempt.json")
                .exists()
        );
        fixture.assert_calls(1, 1, 1, 1);
        assert!(app.retry(child.id, retry("retry-unknown")).is_err());
        // The local fixture has reaped the child, so this is a genuine trusted-host
        // stop confirmation. It does not authorize replaying the external effect.
        Store::open(fixture.db())
            .unwrap()
            .confirm_stopped_and_requeue(&active.claim().unwrap())
            .unwrap();
        match requeue_state {
            "expired" => expire_reservation(&fixture, &child),
            "cancelled" => assert_eq!(app.cancel(child.id).unwrap()["requested"], true),
            "unchanged" => (),
            _ => unreachable!(),
        }
        drop(app);
        let app = fixture.open();
        assert!(app.work_once().unwrap());
        assert_eq!(app.get(child.id).unwrap().generation, 2);
        let stopped = result(&app, child.id);
        assert_eq!(
            stopped.outcome,
            Outcome::Failure,
            "{requeue_state}: {}",
            stopped.to_json()
        );
        let error = stopped.error.as_deref().unwrap();
        assert!(error.contains("reconcile"), "{requeue_state}: {error}");
        assert!(
            !error.contains("no publisher ran"),
            "{requeue_state}: {error}"
        );
        assert!(!app.work_once().unwrap());
        fixture.assert_calls(1, 1, 1, 1);
    }
}

fn expire_reservation(fixture: &Fixture, child: &relay::Task) {
    // Trusted fault-injection simulates time passage without sleeping for a day.
    // Production callers cannot alter either immutable payload.
    let mut payload: Value = serde_json::from_str(&child.payload).unwrap();
    payload["continuation"]["publish_approved"]["expires_at_unix_seconds"] = json!(1);
    let payload = serde_json::to_string(&payload).unwrap();
    let mut db = rusqlite::Connection::open(fixture.db()).unwrap();
    let tx = db.transaction().unwrap();
    tx.execute(
        "UPDATE app_continuations SET payload=?1 WHERE predecessor_id=1",
        [&payload],
    )
    .unwrap();
    tx.execute(
        "UPDATE tasks SET payload=?1 WHERE id=?2",
        rusqlite::params![payload, child.id],
    )
    .unwrap();
    tx.commit().unwrap();
}

#[test]
fn reservation_protects_successful_workspace_and_expiry_uses_original_ttl() {
    let mut fixture = Fixture::new();
    fixture.config.successful_workspace_retention_seconds = Some(60);
    let (app, first, scope) = fixture.approved();
    let root = first.workspace.unwrap();
    let child = app
        .publish_approved(1, request("ttl-protection", &scope))
        .unwrap();
    change(root.join("finished.json"), "finished_at", json!(1));
    assert_eq!(app.cleanup_completed().unwrap(), 0);
    assert!(root.exists());
    let preview = app.workspace_inventory(None).unwrap();
    assert_eq!(preview["workspaces"][0]["retention"]["status"], "protected");
    expire_reservation(&fixture, &child);
    let preview = app.workspace_inventory(None).unwrap();
    assert_eq!(preview["workspaces"][0]["retention"]["status"], "eligible");
    assert_eq!(preview["workspaces"][0]["retention"]["eligible_at"], 61);
    assert_eq!(app.cleanup_completed().unwrap(), 1);
    assert!(!root.exists());
    assert!(app.work_once().unwrap());
    assert_eq!(result(&app, child.id).outcome, Outcome::Failure);
    fixture.assert_calls(1, 1, 1, 0);
}

#[test]
fn expired_authorization_never_invokes_publisher_with_retained_workspace() {
    let fixture = Fixture::new();
    let (app, _, scope) = fixture.approved();
    let child = app
        .publish_approved(1, request("expires-before-execution", &scope))
        .unwrap();
    expire_reservation(&fixture, &child);
    assert!(app.work_once().unwrap());
    let stopped = result(&app, child.id);
    assert_eq!(stopped.outcome, Outcome::Failure, "{}", stopped.to_json());
    fixture.assert_calls(1, 1, 1, 0);
}

#[test]
fn queued_cancellation_releases_retention_without_taking_workspace_ownership() {
    let mut fixture = Fixture::new();
    fixture.config.successful_workspace_retention_seconds = Some(60);
    let (app, first, scope) = fixture.approved();
    let root = first.workspace.unwrap();
    let before_claim = fs::read(root.join("claim.json")).unwrap();
    let child = app
        .publish_approved(1, request("cancel-before-publish", &scope))
        .unwrap();
    change(root.join("finished.json"), "finished_at", json!(1));
    assert_eq!(app.cleanup_completed().unwrap(), 0);
    assert_eq!(app.cancel(child.id).unwrap()["requested"], true);
    assert_eq!(fs::read(root.join("claim.json")).unwrap(), before_claim);
    assert_eq!(app.cleanup_completed().unwrap(), 1);
    assert!(!root.exists());
    assert!(app.work_once().unwrap());
    assert_eq!(result(&app, child.id).outcome, Outcome::Cancelled);
    fixture.assert_calls(1, 1, 1, 0);
}

#[test]
fn historical_binding_remains_eligible_and_fresh_preview_authorizes_current_publisher() {
    let mut fixture = Fixture::new();
    let (app, first, old_scope) = fixture.approved();
    let original = app.get(1).unwrap();
    let job: relay_app::host::Job = serde_json::from_str(&original.payload).unwrap();
    let workflow = &fixture.config.workflows["checked"];
    // Freeze the pre-feature config-binding format. Historical publish:false
    // records did not bind the unused publisher profile or executable identity.
    let old_value = json!({
        "source": fixture.config.repositories.get(&job.repository),
        "agent": fixture.config.agents.get(&job.agent),
        "native": fixture.config.native_agents.get(&job.agent),
        "workflow": workflow,
        "reviewer": fixture.config.native_agents.get(&workflow.reviewer),
        "test": fixture.config.tests.get(&workflow.test),
        "publisher": job.draft_pr_adapter.as_ref().and_then(|name| fixture.config.draft_pr_adapters.get(name))
    });
    let hash = serde_json::to_vec(&old_value)
        .unwrap()
        .iter()
        .fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    let binding = format!("fnv1a-v1-{hash:016x}");
    let root = first.workspace.unwrap();
    assert_eq!(
        read_json(root.join("claim.json"))["config_binding"],
        binding
    );
    fixture
        .config
        .draft_pr_adapters
        .get_mut("publish")
        .unwrap()
        .env
        .insert(
            "NEW_PUBLISHER_SETTING".into(),
            "explicitly previewed".into(),
        );
    drop(app);
    let app = fixture.open();
    assert!(
        app.publish_approved(1, request("stale-preview", &old_scope))
            .is_err()
    );
    let fresh_scope = action(&app, 1);
    assert_ne!(
        fresh_scope["publisher_binding"],
        old_scope["publisher_binding"]
    );
    let child = app
        .publish_approved(1, request("new-explicit-authorization", &fresh_scope))
        .unwrap();
    assert!(app.work_once().unwrap());
    let completed = result(&app, child.id);
    assert_eq!(
        completed.outcome,
        Outcome::Success,
        "{}",
        completed.to_json()
    );
    assert_eq!(app.get(1).unwrap(), original);
    fixture.assert_calls(1, 1, 1, 1);
}

#[test]
fn adopted_review_audit_is_preserved_when_the_successor_is_published() {
    let fixture = Fixture::new();
    let prose_script = REVIEWER.replace(
        "print(json.dumps({'type': 'result'",
        "answer = 'Complete response follows:\\n```json\\n' + answer + '\\n```\\nExternal integrations were not checked.'\n(root / 'review-response.txt').write_text(answer)\nprint(json.dumps({'type': 'result'",
    );
    executable(
        &fixture.config.native_agents["reviewer"].program,
        &prose_script,
    );
    let app = fixture.open();
    app.submit(fixture.input()).unwrap();
    assert!(app.work_once().unwrap());
    let failed = result(&app, 1);
    assert_eq!(failed.outcome, Outcome::Failure);
    let candidate = failed
        .workflow
        .as_ref()
        .unwrap()
        .candidate_sha
        .as_ref()
        .unwrap();
    let raw = fs::read_to_string(fixture.temp.path().join("audit/review-response.txt")).unwrap();
    let adopted = app.adopt_review(1, serde_json::from_value(json!({
        "key": "operator-adopts", "confirm_stopped_and_reconciled": true,
        "adoption": {"candidate_sha": candidate, "raw_response": raw,
            "raw_sha256": format!("{:x}", Sha256::digest(raw.as_bytes())),
            "confirm_complete_successful_response": true, "confirm_entire_response_reviewed": true,
            "accept_prior_host_tests": true}
    })).unwrap()).unwrap();
    assert!(app.work_once().unwrap());
    let approved = result(&app, adopted.id);
    assert_eq!(approved.outcome, Outcome::Success, "{}", approved.to_json());
    let before = serde_json::to_value(&approved).unwrap();
    assert_eq!(
        before["workflow"]["operator_adoption"]["provenance"],
        "operator_attested"
    );
    let original_adoption = app.get(adopted.id).unwrap();
    let scope = action(&app, adopted.id);
    let child = app
        .publish_approved(adopted.id, request("publish-adopted-review", &scope))
        .unwrap();
    assert!(app.work_once().unwrap());
    let completed = result(&app, child.id);
    assert_eq!(
        completed.outcome,
        Outcome::Success,
        "{}",
        completed.to_json()
    );
    let after = serde_json::to_value(&completed).unwrap();
    for field in [
        "operator_adoption",
        "review_continuation",
        "rounds",
        "base_sha",
        "candidate_sha",
        "reviewed_sha",
    ] {
        assert_eq!(
            after["workflow"][field], before["workflow"][field],
            "{field}"
        );
    }
    assert_eq!(after["tests"], before["tests"]);
    assert_eq!(app.get(adopted.id).unwrap(), original_adoption);
    assert!(
        fixture.events("publisher")[0]["evidence"]
            .as_str()
            .unwrap()
            .contains("Operator-attested review adoption")
    );
    fixture.assert_calls(1, 1, 1, 1);
}

#[test]
fn expired_authorization_does_not_release_a_claimed_unknown_owner() {
    for queued_cancellation in [false, true] {
        let mut fixture = Fixture::new();
        fixture.config.successful_workspace_retention_seconds = Some(60);
        let (app, first, scope) = fixture.approved();
        let root = first.workspace.unwrap();
        let child = app
            .publish_approved(1, request("claimed-before-expiry", &scope))
            .unwrap();
        if queued_cancellation {
            assert_eq!(app.cancel(child.id).unwrap()["requested"], true);
        }
        change(root.join("finished.json"), "finished_at", json!(1));
        let mut store = Store::open(fixture.db()).unwrap();
        let active = store.claim_next("disconnected-host").unwrap().unwrap();
        assert_eq!(active.id, child.id);
        expire_reservation(&fixture, &child);
        assert_eq!(app.cleanup_completed().unwrap(), 0);
        assert!(root.exists());
        assert_eq!(
            app.workspace_inventory(None).unwrap()["workspaces"][0]["retention"]["status"],
            "protected"
        );
        assert!(matches!(
            app.cancel(child.id),
            Err(relay_app::Error::RecoveryRequired)
        ));
        // No command was started by this fixture. Explicit trusted-host stop
        // confirmation, rather than elapsed authorization time, releases the claim.
        store
            .confirm_stopped_and_requeue(&active.claim().unwrap())
            .unwrap();
        assert_eq!(app.cleanup_completed().unwrap(), 1);
        fixture.assert_calls(1, 1, 1, 0);
    }
}

#[test]
fn lost_reserved_key_releases_only_unmaterialized_publication_reservation() {
    let fixture = Fixture::new();
    let (app, _, scope) = fixture.approved();
    let child = app
        .publish_approved(1, request("crash-gap-key", &scope))
        .unwrap();
    drop(app);
    let db = rusqlite::Connection::open(fixture.db()).unwrap();
    db.execute(
        "UPDATE app_continuations SET task_id=NULL WHERE predecessor_id=1",
        [],
    )
    .unwrap();
    db.execute("DELETE FROM tasks WHERE id=?1", [child.id])
        .unwrap();
    drop(db);
    let app = fixture.open();
    let mut unrelated = fixture.input();
    unrelated.key = "crash-gap-key".into();
    unrelated.job.requirements = "An unrelated submission won the reserved key".into();
    let unrelated = app.submit(unrelated).unwrap();
    assert!(matches!(
        app.publish_approved(1, request("crash-gap-key", &scope)),
        Err(relay_app::Error::Core(relay::Error::IdempotencyConflict))
    ));
    assert_eq!(app.get(unrelated.id).unwrap(), unrelated);
    assert!(app.get_view(1).unwrap().continuation_status.is_none());
    let replacement = app
        .publish_approved(1, request("new-explicit-key", &scope))
        .unwrap();
    assert_ne!(replacement.id, unrelated.id);
    assert_eq!(app.cancel(unrelated.id).unwrap()["requested"], true);
    assert!(app.work_once().unwrap());
    assert_eq!(result(&app, unrelated.id).outcome, Outcome::Cancelled);
    assert!(app.work_once().unwrap());
    let completed = result(&app, replacement.id);
    assert_eq!(
        completed.outcome,
        Outcome::Success,
        "{}",
        completed.to_json()
    );
    assert!(!app.work_once().unwrap());
    fixture.assert_calls(1, 1, 1, 1);
}
