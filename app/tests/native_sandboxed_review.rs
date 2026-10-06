#![cfg(target_os = "linux")]

use relay_app::{
    Application, Error, ReviewContinuationRequest, Submission,
    host::{Host, HostConfig, Job, Outcome, RunResult},
    providers::ProviderKind,
};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, MutexGuard},
};
use tempfile::TempDir;

const MODE: &str = "codex_native_sandboxed_review";

// No model, network service, or installed Codex is used. Both roles run this
// local protocol fixture, so an accidental reviewer resume of a developer
// thread is observable. Audit files are outside the candidate workspaces.
const CODEX: &str = r#"#!/usr/bin/python3
import json, os, pathlib, subprocess, sys, time
root = pathlib.Path(os.environ['FIXTURE_AUDIT'])
review = os.environ['FIXTURE_ROLE'] == 'reviewer'
if review:
    # Probes and native startup must not receive the task through environment.
    assert 'RELAY_REQUIREMENTS' not in os.environ
    assert 'RELAY_REQUIREMENTS_FILE' not in os.environ
if '--version' in sys.argv:
    path = root / 'version'
    print('codex-cli ' + (path.read_text() if path.exists() else '0.160.1')); sys.exit()
if '--help' in sys.argv: print('codex app-server --help'); sys.exit()
assert sys.argv[1:] == ['app-server'], sys.argv
# Native settings are deliberately preserved, not claimed to be sandboxed.
assert pathlib.Path(os.environ['CODEX_HOME'], 'config.toml').read_text() == '# trusted fixture native settings\n'
def log(kind, **fields):
    with (root / (kind + '.jsonl')).open('a') as out:
        out.write(json.dumps(fields) + '\n')
def recv():
    line = sys.stdin.readline()
    if not line: sys.exit()
    return json.loads(line)
def send(value): print(json.dumps(value), flush=True)
role = 'reviewer' if review else 'developer'
log('process', role=role, argv=sys.argv[1:], cwd=os.getcwd())
a = recv(); assert a['method'] == 'initialize'; send({'id': a['id'], 'result': {}})
assert recv()['method'] == 'initialized'
a = recv()
turn_log = root / (role + '.jsonl')
n = len(turn_log.read_text().splitlines()) if turn_log.exists() else 0
mode_path = root / 'review-mode'
mode = mode_path.read_text() if mode_path.exists() else 'approve'
sid = 'reviewer-thread-' + str(n) if review else 'developer-thread'
if review:
    assert a['method'] == 'thread/start' and 'threadId' not in a['params'], a
    assert a['params']['ephemeral'] is True
    assert a['params']['sandbox'] == 'read-only'
    assert pathlib.Path.cwd().name == 'reviewer-repository'
    assert pathlib.Path('.git/relay-review.patch').is_file()
    assert not pathlib.Path('.git/developer-session-marker').exists()
    thread = {'id': sid, 'ephemeral': True}
    policy = {'type': 'readOnly', 'networkAccess': False}
else:
    assert a['method'] == ('thread/resume' if n else 'thread/start'), a
    assert a['params']['sandbox'] == 'workspace-write'
    if n: assert a['params']['threadId'] == sid and a['params']['excludeTurns'] is True
    thread = {'id': sid}
    policy = {'type': 'workspaceWrite', 'networkAccess': False}
assert a['params']['approvalPolicy'] == 'never'
assert a['params']['cwd'] == os.getcwd()
log('thread', role=role, method=a['method'], params=a['params'], session_id=sid)
result = {'thread': thread, 'model': 'fixture-model', 'approvalPolicy': 'never', 'sandbox': policy}
if review:
    if mode == 'missing_policy': result.pop('sandbox')
    if mode == 'missing_approval': result.pop('approvalPolicy')
    if mode == 'missing_network': result['sandbox'].pop('networkAccess')
    if mode == 'string_policy': result['sandbox'] = 'read-only'
    if mode == 'write_policy': result['sandbox']['type'] = 'workspaceWrite'
    if mode == 'full_policy': result['sandbox']['type'] = 'dangerFullAccess'
    if mode == 'network_policy': result['sandbox']['networkAccess'] = True
    if mode == 'approval_policy': result['approvalPolicy'] = 'on-request'
    if mode == 'missing_ephemeral': result['thread'].pop('ephemeral')
    if mode == 'persistent_thread': result['thread']['ephemeral'] = False
if review and mode in ('approval_before_turn', 'malformed_before_turn'):
    # A single pipe write makes both lines available to the same stdout drain.
    # The valid response would queue the prompt before the next line fails.
    failure = json.dumps({'id': 'early-permission', 'method': 'item/commandExecution/requestApproval', 'params': {}}) if mode == 'approval_before_turn' else 'not valid JSON'
    batch = (json.dumps({'id': a['id'], 'result': result}) + '\n' + failure + '\n').encode()
    assert len(batch) < 4096
    os.write(sys.stdout.fileno(), batch)
    data = os.read(sys.stdin.fileno(), 65536)
    log('early-input', data=data.decode())
    # No prompt, even a partial one, should be written after either failure.
    # A static refusal or closed stdin are both acceptable.
    if data:
        for line in data.splitlines():
            response = json.loads(line)
            assert 'method' not in response and response['error']['code'] == -32601
    time.sleep(60)
send({'id': a['id'], 'result': result})
a = recv()
# Recording before assertions proves whether any task prompt escaped a failed
# session-policy check, even if the turn request itself is malformed.
log('received-after-thread', role=role, request=a)
assert a['method'] == 'turn/start'
p = a['params']; prompt = p['input'][0]['text']
assert p['threadId'] == sid and p['cwd'] == os.getcwd()
assert p['approvalPolicy'] == 'never'
if review: assert p['sandboxPolicy'] == {'type': 'readOnly', 'networkAccess': False}
else: assert p['sandboxPolicy']['type'] == 'workspaceWrite' and p['sandboxPolicy']['networkAccess'] is False
candidate = os.environ.get('RELAY_CANDIDATE_SHA')
log(role, session_id=sid, candidate_sha=candidate, cwd=os.getcwd(), prompt=prompt, turn=p,
    round=os.environ['RELAY_WORKFLOW_ROUND'])
tid = role + '-turn-' + str(n)
send({'id': a['id'], 'result': {'turn': {'id': tid}}})
if review:
    assert subprocess.check_output(['/usr/bin/git', 'rev-parse', 'HEAD'], text=True).strip() == candidate
    assert candidate in prompt
    if mode == 'approval_request':
        send({'id': 'unexpected-permission', 'method': 'item/commandExecution/requestApproval', 'params': {}})
        line = sys.stdin.readline()
        response = json.loads(line) if line else None
        log('refusal', response=response)
        assert response is None or response['error']['code'] == -32601
        time.sleep(60)
    if mode in ('mutate', 'hidden_mutation'):
        if mode == 'hidden_mutation': subprocess.run(['/usr/bin/git', 'update-index', '--assume-unchanged', 'original.txt'], check=True)
        pathlib.Path('original.txt').write_text('reviewer changed source\n')
    if mode == 'change_head':
        subprocess.run(['/usr/bin/git', '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '--allow-empty', '-m', 'unexpected'], check=True, stdout=sys.stderr)
    verdict = 'changes_requested' if mode == 'reject' or (mode == 'repair' and n == 0) else 'approved'
    answer = json.dumps({'candidate_sha': 'f' * 40 if mode == 'wrong_sha' else candidate,
        'verdict': verdict, 'summary': 'Checked the exact candidate',
        'findings': ['Fix the fixture issue'] if verdict == 'changes_requested' else []})
    if mode == 'no_verdict': answer = 'PROVIDER-HISTORY-' + sid + ': no verdict was returned'
else:
    if n: assert 'Fix the fixture issue' in prompt
    pathlib.Path('changed.txt').write_text('candidate ' + str(n) + '\n')
    pathlib.Path('.git/developer-session-marker').write_text(sid)
    answer = 'implemented'
send({'method': 'item/completed', 'params': {'threadId': sid, 'turnId': tid,
    'item': {'type': 'agentMessage', 'phase': 'final_answer', 'text': answer}}})
send({'method': 'turn/completed', 'params': {'threadId': sid,
    'turn': {'id': tid, 'status': 'completed', 'error': None}}})
# Relay is responsible for stopping and reaping the app-server after the turn.
time.sleep(60)
"#;

const STRICT_REVIEWER: &str = r#"#!/usr/bin/python3
import json, os, pathlib, sys
if '--version' in sys.argv: print('2.1.281 (Claude Code)'); sys.exit()
if '--help' in sys.argv:
    print('--output-format --verbose --permission-prompts --restricted --tools --allowedTools --disallowedTools --disable-slash-commands --strict-mcp-config --mcp-config --no-session-persistence --session-id --resume'); sys.exit()
root = pathlib.Path(os.environ['FIXTURE_AUDIT'])
assert '--restricted' in sys.argv and 'Read,Glob,Grep' in sys.argv
assert '--strict-mcp-config' in sys.argv and '--mcp-config' in sys.argv
assert '--resume' not in sys.argv and '--continue' not in sys.argv
if os.environ['FIXTURE_CONTINUITY'] == 'true':
    assert '--no-session-persistence' not in sys.argv
    sid = sys.argv[sys.argv.index('--session-id') + 1]
    assert pathlib.Path.cwd().name == 'reviewer-repository'
else:
    assert '--no-session-persistence' in sys.argv and '--session-id' not in sys.argv
    sid = 'stateless-strict'
    assert pathlib.Path.cwd().name == 'repository'
prompt = sys.stdin.read()
with (root / 'reviewer.jsonl').open('a') as log:
    log.write(json.dumps({'session_id': sid, 'candidate_sha': os.environ['RELAY_CANDIDATE_SHA'],
        'cwd': os.getcwd(), 'prompt': prompt, 'argv': sys.argv[1:]}) + '\n')
mode = (root / 'review-mode').read_text()
answer = 'PROVIDER-HISTORY-' + sid + ': no verdict was returned' if mode == 'no_verdict' else json.dumps({
    'candidate_sha': os.environ['RELAY_CANDIDATE_SHA'], 'verdict': 'approved',
    'summary': 'Checked the exact candidate', 'findings': []})
print(json.dumps({'type': 'system', 'subtype': 'init', 'session_id': sid, 'model': 'fixture-model'}))
print(json.dumps({'type': 'result', 'subtype': 'success', 'is_error': False, 'session_id': sid,
    'result': answer, 'permission_denials': []}))
"#;

const TEST: &str = r#"#!/usr/bin/python3
import json, os, pathlib, subprocess
candidate = os.environ['RELAY_CANDIDATE_SHA']
assert subprocess.check_output(['/usr/bin/git', 'rev-parse', 'HEAD'], text=True).strip() == candidate
assert subprocess.check_output(['/usr/bin/git', 'show', candidate + ':changed.txt'], text=True) == pathlib.Path('changed.txt').read_text()
with (pathlib.Path(os.environ['FIXTURE_AUDIT']) / 'test.jsonl').open('a') as log:
    log.write(json.dumps({'candidate_sha': candidate, 'cwd': os.getcwd()}) + '\n')
print('tests passed')
"#;

const PUBLISH: &str = r#"#!/usr/bin/python3
import json, os, pathlib
candidate = os.environ['RELAY_CANDIDATE_SHA']
assert candidate == os.environ['RELAY_REVIEWED_SHA']
assert os.environ['RELAY_REVIEW_VERDICT'] == 'approved'
assert os.environ['RELAY_TEST_OUTCOME'] == 'success'
with (pathlib.Path(os.environ['FIXTURE_AUDIT']) / 'publish.jsonl').open('a') as log:
    log.write(json.dumps({'candidate_sha': candidate}) + '\n')
print(json.dumps({'dry_run': True, 'draft': True, 'repository': 'example/project',
    'branch': 'relay/task-' + os.environ['RELAY_TASK_ID'] + '-g' + os.environ['RELAY_GENERATION'],
    'candidate_sha': candidate, 'reconciliation_required': False}))
"#;

// Concurrent fork can briefly retain another fixture's CLOEXEC lease. Keep
// lifetimes disjoint rather than weakening or retrying production lease checks.
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
fn submit(
    app: &Application,
    key: &str,
    job: Value,
    challenge: Option<&Value>,
) -> Result<relay::Task, Error> {
    app.submit(
        serde_json::from_value::<Submission>(json!({
            "key": key, "job": job, "permission_challenge": challenge.map(|c| &c["challenge"])
        }))
        .unwrap(),
    )
}
fn result(app: &Application, id: i64) -> RunResult {
    serde_json::from_str(app.get(id).unwrap().result.as_deref().unwrap()).unwrap()
}
impl Fixture {
    fn new() -> Self {
        let serial = FIXTURES.lock().unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let audit = temp.path().join("audit");
        let native_home = temp.path().join("native-home");
        for path in [&source, &audit, &native_home] {
            fs::create_dir(path).unwrap();
        }
        fs::write(
            native_home.join("config.toml"),
            "# trusted fixture native settings\n",
        )
        .unwrap();
        fs::write(source.join("original.txt"), "original\n").unwrap();
        git(&source, &["init", "--initial-branch=main"]);
        git(&source, &["add", "."]);
        git(&source, &["commit", "-m", "base"]);
        let codex = temp.path().join("codex");
        let test = temp.path().join("test");
        let publish = temp.path().join("publish");
        executable(&codex, CODEX);
        executable(&test, TEST);
        executable(&publish, PUBLISH);
        let shared_env = json!({"FIXTURE_AUDIT": audit});
        let config = serde_json::from_value(json!({
            "workspace_root": temp.path().join("runs"), "repositories": {"repo": source},
            "native_agents": {
                "developer": {"provider": "codex_app_server", "program": codex, "session_continuity": true,
                    "env": {"FIXTURE_AUDIT": audit, "CODEX_HOME": native_home, "FIXTURE_ROLE": "developer"}},
                "strict": {"provider": "claude_cli", "program": "/bin/true"},
                "native-review": {"provider": "codex_app_server", "program": codex,
                    "native_permission": MODE, "allowed_permission_modes": [MODE], "session_continuity": false,
                    "env": {"FIXTURE_AUDIT": audit, "CODEX_HOME": native_home, "FIXTURE_ROLE": "reviewer"}}
            },
            "tests": {"check": {"program": test, "env": shared_env}},
            "draft_pr_adapters": {"publish": {"program": publish, "env": shared_env}},
            "workflows": {"checked": {"repository": "repo", "developer": "developer", "reviewer": "strict",
                "selectable_reviewers": ["native-review"], "test": "check", "max_repairs": 0,
                "draft_pr_adapter": "publish", "github_repository": "example/project",
                "review_focus": "Verify changed.txt contains the requested candidate"}},
            "timeout_seconds": 30, "output_limit_bytes": 128,
            "supervisor_program": env!("CARGO_BIN_EXE_relay-app")
        })).unwrap();
        Self {
            temp,
            config,
            _serial: serial,
        }
    }
    fn configure_strict_reviewer(&mut self, continuity: bool) {
        let script = self.temp.path().join("strict-reviewer");
        executable(&script, STRICT_REVIEWER);
        let profile = self.config.native_agents.get_mut("strict").unwrap();
        profile.program = script;
        profile.session_continuity = continuity;
        profile.env.insert(
            "FIXTURE_AUDIT".into(),
            self.temp
                .path()
                .join("audit")
                .to_string_lossy()
                .into_owned(),
        );
        profile
            .env
            .insert("FIXTURE_CONTINUITY".into(), continuity.to_string());
    }
    fn initial_reviewer(&self, app: &Application, native: bool) -> relay::Task {
        if native {
            self.accepted(app)
        } else {
            let mut job = self.job();
            job["role_selections"]["reviewer"] =
                json!({"profile": "strict", "native_permission": "claude_restricted"});
            submit(app, "initial", job, None).unwrap()
        }
    }
    fn db(&self) -> PathBuf {
        self.temp.path().join("queue.db")
    }
    fn open(&self) -> Arc<Application> {
        Application::open(self.db(), self.config.clone()).unwrap()
    }
    fn job(&self) -> Value {
        json!({"repository": "repo", "agent": "developer", "workflow": "checked",
            "requirements": "DEVELOPER-ONLY-INSTRUCTION: implement the fixture",
            "publish": true, "draft_pr_adapter": "publish",
            "role_selections": {"reviewer": {"profile": "native-review", "native_permission": MODE,
                "confirm_permission_expansion": true}}})
    }
    fn challenge(&self, app: &Application, job: Value) -> Value {
        app.permission_challenge(serde_json::from_value(job).unwrap())
            .unwrap()
    }
    fn accepted(&self, app: &Application) -> relay::Task {
        let job = self.job();
        let issued = self.challenge(app, job.clone());
        submit(app, "initial", job, Some(&issued)).unwrap()
    }
    fn mode(&self, mode: &str) {
        fs::write(self.temp.path().join("audit/review-mode"), mode).unwrap();
    }
    fn events(&self, name: &str) -> Vec<Value> {
        fs::read_to_string(self.temp.path().join("audit").join(format!("{name}.jsonl")))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn run(&self) -> RunResult {
        let app = self.open();
        let task = self.accepted(&app);
        assert!(app.work_once().unwrap());
        result(&app, task.id)
    }
}

#[test]
fn native_reviewer_approves_and_publishes_only_the_exact_isolated_candidate() {
    let fixture = Fixture::new();
    let source = fixture.temp.path().join("source");
    let base = git(&source, &["rev-parse", "HEAD"]);
    let result = fixture.run();
    assert_eq!(result.outcome, Outcome::Success, "{}", result.to_json());
    let root = result.workspace.as_ref().unwrap();
    let workflow = result.workflow.as_ref().unwrap();
    let candidate = workflow.candidate_sha.as_ref().unwrap();
    assert_eq!(workflow.base_sha.as_ref(), Some(&base));
    assert_eq!(workflow.reviewed_sha.as_ref(), Some(candidate));
    assert_eq!(workflow.rounds.len(), 1);
    for name in ["test", "reviewer", "publish"] {
        let events = fixture.events(name);
        assert_eq!(events.len(), 1, "{name}: {events:?}");
        assert_eq!(events[0]["candidate_sha"], *candidate);
    }
    let dev = &fixture.events("developer")[0];
    let review = &fixture.events("reviewer")[0];
    assert_ne!(dev["session_id"], review["session_id"]);
    assert_eq!(dev["cwd"], root.join("repository").to_str().unwrap());
    assert_eq!(
        review["cwd"],
        root.join("reviewer-repository").to_str().unwrap()
    );
    assert!(
        !review["prompt"]
            .as_str()
            .unwrap()
            .contains("DEVELOPER-ONLY-INSTRUCTION")
    );
    assert!(root.join("sessions/developer.json").is_file());
    assert!(!root.join("sessions/reviewer.json").exists());
    assert_eq!(
        git(&root.join("reviewer-repository"), &["rev-parse", "HEAD"]),
        *candidate
    );
    assert_eq!(git(&source, &["rev-parse", "HEAD"]), base);
    assert!(!source.join("changed.txt").exists());
}

#[test]
fn native_review_is_default_off_provider_specific_host_allowlisted_and_fresh_only() {
    let fixture = Fixture::new();
    let profile = &fixture.config.native_agents["native-review"];
    for variant in [
        "no_mode",
        "no_allowlist",
        "codex_cli",
        "claude_cli",
        "continuity",
    ] {
        let mut changed = profile.clone();
        match variant {
            "no_mode" => changed.native_permission = None,
            "no_allowlist" => changed.allowed_permission_modes.clear(),
            "codex_cli" => changed.provider = ProviderKind::CodexCli,
            "claude_cli" => changed.provider = ProviderKind::ClaudeCli,
            "continuity" => changed.session_continuity = true,
            _ => unreachable!(),
        }
        assert!(changed.compile(true).is_err(), "{variant}");
    }
    assert!(
        profile.compile(false).is_err(),
        "review mode cannot be used for development"
    );
    let mut config = fixture.config.clone();
    config
        .workflows
        .get_mut("checked")
        .unwrap()
        .selectable_reviewers = None;
    let job: Job = serde_json::from_value(fixture.job()).unwrap();
    assert!(
        job.validate(&config)
            .unwrap_err()
            .to_string()
            .contains("allowlisted")
    );
    config.workflows.get_mut("checked").unwrap().reviewer = "native-review".into();
    Host::new(config.clone()).unwrap();
    let mut omitted = fixture.job();
    omitted.as_object_mut().unwrap().remove("role_selections");
    assert!(
        serde_json::from_value::<Job>(omitted)
            .unwrap()
            .validate(&config)
            .is_err()
    );
    let mut omitted_mode = fixture.job();
    omitted_mode["role_selections"]["reviewer"]
        .as_object_mut()
        .unwrap()
        .remove("native_permission");
    assert!(
        serde_json::from_value::<Job>(omitted_mode)
            .unwrap()
            .validate(&config)
            .is_err()
    );
}

#[test]
fn reviewer_consent_requires_acknowledgement_and_exact_single_use_server_challenge() {
    let fixture = Fixture::new();
    let app = fixture.open();
    let selected = fixture.job();
    let issued = fixture.challenge(&app, selected.clone());
    assert_eq!(issued["scope"]["reviewer"]["profile"], "native-review");
    assert_eq!(issued["scope"]["reviewer"]["native_permission"], MODE);
    let warning = issued["confirmation_text"].as_str().unwrap();
    assert!(
        warning.contains("reviewer") && warning.contains("MCP") && warning.contains("sandbox"),
        "{warning}"
    );
    assert!(app.list(None).unwrap().is_empty());
    let mut unacknowledged = selected.clone();
    unacknowledged["role_selections"]["reviewer"]
        .as_object_mut()
        .unwrap()
        .remove("confirm_permission_expansion");
    assert!(submit(&app, "no-ack", unacknowledged, Some(&issued)).is_err());
    assert!(submit(&app, "no-challenge", selected.clone(), None).is_err());
    let mut changed = selected.clone();
    changed["role_selections"]["reviewer"]["model"] =
        json!({"value": "different-model", "source": "manual"});
    assert!(
        submit(&app, "different-model", changed, Some(&issued))
            .unwrap_err()
            .to_string()
            .contains("no longer matches")
    );
    let accepted = submit(&app, "accepted", selected.clone(), Some(&issued)).unwrap();
    let payload: Value = serde_json::from_str(&accepted.payload).unwrap();
    assert_eq!(
        payload["role_binding"]["reviewer"]["native_permission"],
        MODE
    );
    assert!(submit(&app, "reused-token", selected.clone(), Some(&issued)).is_err());
    assert_eq!(
        submit(&app, "accepted", selected.clone(), Some(&issued)).unwrap(),
        accepted
    );
    assert!(
        fixture.events("process").is_empty(),
        "consent must not launch native processes"
    );
    drop(app);
    let app = fixture.open();
    assert_eq!(
        submit(&app, "accepted", selected, Some(&issued)).unwrap(),
        accepted
    );
}

#[test]
fn unconsumed_challenge_is_invalid_after_restart_and_accepted_policy_drift_prevents_launch() {
    let mut fixture = Fixture::new();
    let app = fixture.open();
    let selected = fixture.job();
    let stale = fixture.challenge(&app, selected.clone());
    drop(app);
    let app = fixture.open();
    assert!(submit(&app, "stale", selected.clone(), Some(&stale)).is_err());
    let issued = fixture.challenge(&app, selected.clone());
    let accepted = submit(&app, "accepted", selected.clone(), Some(&issued)).unwrap();
    drop(app);
    fixture
        .config
        .native_agents
        .get_mut("native-review")
        .unwrap()
        .model = Some("changed-default".into());
    let app = fixture.open();
    assert_eq!(
        submit(&app, "accepted", selected, Some(&issued)).unwrap(),
        accepted
    );
    assert!(app.work_once().unwrap());
    let failed = result(&app, accepted.id);
    assert_eq!(failed.outcome, Outcome::Failure);
    assert!(
        failed
            .error
            .as_deref()
            .unwrap()
            .contains("changed after acceptance")
    );
    assert!(failed.workspace.is_none());
    assert!(fixture.events("process").is_empty());
}

#[test]
fn unverified_or_weakened_session_policy_never_receives_a_review_prompt() {
    for mode in [
        "missing_policy",
        "missing_approval",
        "missing_network",
        "string_policy",
        "write_policy",
        "full_policy",
        "network_policy",
        "approval_policy",
        "missing_ephemeral",
        "persistent_thread",
    ] {
        let fixture = Fixture::new();
        fixture.mode(mode);
        let failed = fixture.run();
        assert_eq!(
            failed.outcome,
            Outcome::Failure,
            "{mode}: {}",
            failed.to_json()
        );
        assert_eq!(
            fixture
                .events("thread")
                .iter()
                .filter(|event| event["role"] == "reviewer")
                .count(),
            1,
            "{mode}"
        );
        assert!(
            fixture
                .events("received-after-thread")
                .iter()
                .all(|event| event["role"] != "reviewer"),
            "{mode}: prompt escaped the policy gate"
        );
        assert!(fixture.events("reviewer").is_empty(), "{mode}");
        assert!(fixture.events("publish").is_empty(), "{mode}");
    }
}

#[test]
fn native_review_rejects_unpinned_codex_versions_before_starting_a_reviewer() {
    for version in ["0.160.0", "0.160.2", "0.161.0"] {
        let fixture = Fixture::new();
        fs::write(fixture.temp.path().join("audit/version"), version).unwrap();
        let failed = fixture.run();
        assert_eq!(
            failed.outcome,
            Outcome::Failure,
            "{version}: {}",
            failed.to_json()
        );
        assert!(
            fixture
                .events("process")
                .iter()
                .all(|event| event["role"] != "reviewer"),
            "{version}"
        );
        assert!(fixture.events("publish").is_empty());
    }
}

#[test]
fn unexpected_native_approval_request_is_refused_and_cannot_publish() {
    let fixture = Fixture::new();
    fixture.mode("approval_request");
    let failed = fixture.run();
    assert_eq!(failed.outcome, Outcome::Failure, "{}", failed.to_json());
    let denied = fixture.events("refusal");
    // Closing stdin and stopping the process is also a refusal. The fixture
    // may be reaped before it can log EOF; a denial response is not required.
    assert!(
        failed.to_json().contains("requested approval"),
        "{}",
        failed.to_json()
    );
    for event in denied {
        if !event["response"].is_null() {
            assert_eq!(event["response"]["id"], "unexpected-permission");
            assert_eq!(event["response"]["error"]["code"], -32601);
        }
    }
    assert!(fixture.events("publish").is_empty());
    assert!(
        !failed
            .workspace
            .unwrap()
            .join("sessions/reviewer.json")
            .exists()
    );
}

#[test]
fn same_stdout_drain_failure_never_flushes_a_queued_review_prompt() {
    for mode in ["approval_before_turn", "malformed_before_turn"] {
        let fixture = Fixture::new();
        fixture.mode(mode);
        let failed = fixture.run();
        assert_eq!(
            failed.outcome,
            Outcome::Failure,
            "{mode}: {}",
            failed.to_json()
        );
        let expected = if mode == "approval_before_turn" {
            "requested approval"
        } else {
            "JSON"
        };
        assert!(
            failed.to_json().contains(expected),
            "{mode}: {}",
            failed.to_json()
        );
        assert_eq!(
            fixture
                .events("thread")
                .iter()
                .filter(|event| event["role"] == "reviewer")
                .count(),
            1
        );
        assert!(fixture.events("reviewer").is_empty());
        for event in fixture.events("early-input") {
            for line in event["data"].as_str().unwrap().lines() {
                let response: Value =
                    serde_json::from_str(line).expect("no partial task prompt may escape");
                assert!(response.get("method").is_none(), "{mode}: {response}");
                assert_eq!(response["error"]["code"], -32601, "{mode}: {response}");
            }
        }
        assert!(fixture.events("publish").is_empty());
    }
}

#[test]
fn mutation_wrong_sha_or_missing_verdict_never_approves_the_candidate() {
    for mode in [
        "mutate",
        "hidden_mutation",
        "change_head",
        "wrong_sha",
        "no_verdict",
    ] {
        let fixture = Fixture::new();
        fixture.mode(mode);
        let failed = fixture.run();
        assert_eq!(
            failed.outcome,
            Outcome::Failure,
            "{mode}: {}",
            failed.to_json()
        );
        assert_eq!(fixture.events("reviewer").len(), 1, "{mode}");
        assert!(fixture.events("publish").is_empty(), "{mode}");
        assert!(
            failed.workflow.as_ref().unwrap().reviewed_sha.is_none(),
            "{mode}"
        );
        let root = failed.workspace.unwrap();
        assert_eq!(
            fs::read_to_string(root.join("repository/original.txt")).unwrap(),
            "original\n"
        );
        assert_eq!(
            fs::read_to_string(fixture.temp.path().join("source/original.txt")).unwrap(),
            "original\n"
        );
    }
}

#[test]
fn repair_resumes_only_developer_and_starts_fresh_isolated_reviewer_each_round() {
    let mut fixture = Fixture::new();
    fixture
        .config
        .workflows
        .get_mut("checked")
        .unwrap()
        .max_repairs = 1;
    fixture.mode("repair");
    let completed = fixture.run();
    assert_eq!(
        completed.outcome,
        Outcome::Success,
        "{}",
        completed.to_json()
    );
    assert_eq!(completed.workflow.as_ref().unwrap().rounds.len(), 2);
    let developers = fixture.events("developer");
    let reviewers = fixture.events("reviewer");
    let tests = fixture.events("test");
    assert_eq!(developers.len(), 2);
    assert_eq!(reviewers.len(), 2);
    assert_eq!(tests.len(), 2);
    assert_eq!(developers[0]["session_id"], developers[1]["session_id"]);
    assert_ne!(reviewers[0]["session_id"], reviewers[1]["session_id"]);
    assert_ne!(reviewers[0]["candidate_sha"], reviewers[1]["candidate_sha"]);
    for i in 0..2 {
        assert_eq!(tests[i]["candidate_sha"], reviewers[i]["candidate_sha"]);
        assert_ne!(developers[i]["session_id"], reviewers[i]["session_id"]);
        assert_ne!(developers[i]["cwd"], reviewers[i]["cwd"]);
    }
    assert_eq!(reviewers[0]["cwd"], reviewers[1]["cwd"]);
    let threads = fixture.events("thread");
    let developer_methods: Vec<_> = threads
        .iter()
        .filter(|event| event["role"] == "developer")
        .map(|event| event["method"].as_str().unwrap())
        .collect();
    assert_eq!(developer_methods, ["thread/start", "thread/resume"]);
    assert!(
        threads
            .iter()
            .filter(|event| event["role"] == "reviewer")
            .all(|event| event["method"] == "thread/start")
    );
    assert_eq!(
        fixture.events("publish")[0]["candidate_sha"],
        reviewers[1]["candidate_sha"]
    );
    assert!(
        !completed
            .workspace
            .unwrap()
            .join("sessions/reviewer.json")
            .exists()
    );
}

#[test]
fn review_continuation_retests_same_candidate_with_fresh_thread_and_no_developer_replay() {
    let fixture = Fixture::new();
    fixture.mode("no_verdict");
    let app = fixture.open();
    let task = fixture.accepted(&app);
    assert!(app.work_once().unwrap());
    let failed = result(&app, task.id);
    assert_eq!(failed.outcome, Outcome::Failure, "{}", failed.to_json());
    let candidate = failed
        .workflow
        .as_ref()
        .unwrap()
        .candidate_sha
        .clone()
        .unwrap();
    let first_developer_session = fs::read(
        failed
            .workspace
            .as_ref()
            .unwrap()
            .join("sessions/developer.json"),
    )
    .unwrap();
    fixture.mode("approve");
    let child = app
        .continue_review(
            task.id,
            ReviewContinuationRequest {
                key: "review-again".into(),
                confirm_stopped_and_reconciled: true,
                revalidate_tests: true,
                review_focus: Some("Check the preserved candidate once more".into()),
                replacement: None,
                permission_challenge: None,
                workspace_quota_bytes: None,
            },
        )
        .unwrap();
    assert!(app.work_once().unwrap());
    let completed = result(&app, child.id);
    assert_eq!(
        completed.outcome,
        Outcome::Success,
        "{}",
        completed.to_json()
    );
    assert!(completed.agent.is_none());
    assert_eq!(completed.workspace, failed.workspace);
    assert_eq!(
        completed.workflow.as_ref().unwrap().candidate_sha.as_ref(),
        Some(&candidate)
    );
    assert_eq!(
        completed.workflow.as_ref().unwrap().reviewed_sha.as_ref(),
        Some(&candidate)
    );
    assert_eq!(fixture.events("developer").len(), 1);
    let reviews = fixture.events("reviewer");
    assert_eq!(reviews.len(), 2);
    assert_ne!(reviews[0]["session_id"], reviews[1]["session_id"]);
    assert_eq!(reviews[0]["cwd"], reviews[1]["cwd"]);
    assert!(
        reviews[1]["prompt"]
            .as_str()
            .unwrap()
            .contains("Check the preserved candidate once more")
    );
    let tests = fixture.events("test");
    assert_eq!(tests.len(), 2);
    for event in tests.iter().chain(reviews.iter()) {
        assert_eq!(event["candidate_sha"], candidate);
    }
    let root = completed.workspace.unwrap();
    assert!(!root.join("sessions/reviewer.json").exists());
    assert_eq!(
        fs::read(root.join("sessions/developer.json")).unwrap(),
        first_developer_session
    );
    assert_eq!(fixture.events("publish").len(), 1);
}

fn replacement_request(selection: Value, challenge: Option<&Value>) -> ReviewContinuationRequest {
    serde_json::from_value(json!({
        "key": "replace-reviewer", "confirm_stopped_and_reconciled": true, "revalidate_tests": true,
        "replacement": selection, "permission_challenge": challenge.map(|value| &value["challenge"])
    }))
    .unwrap()
}

#[test]
fn same_topology_cross_provider_reviewer_replacement_starts_new_epoch_without_history_reuse() {
    for initial_native in [true, false] {
        let mut fixture = Fixture::new();
        fixture.configure_strict_reviewer(true);
        fixture.mode("no_verdict");
        let app = fixture.open();
        let initial = fixture.initial_reviewer(&app, initial_native);
        assert!(app.work_once().unwrap());
        let failed = result(&app, initial.id);
        assert_eq!(failed.outcome, Outcome::Failure, "{}", failed.to_json());
        let root = failed.workspace.as_ref().unwrap();
        let old_session = fs::read(root.join("sessions/reviewer.json")).ok();
        assert_eq!(old_session.is_some(), !initial_native);
        let developer_session = fs::read(root.join("sessions/developer.json")).unwrap();
        let original = app.get(initial.id).unwrap();
        fixture.mode("approve");
        let choice = if initial_native {
            json!({"profile": "strict", "native_permission": "claude_restricted"})
        } else {
            fixture.job()["role_selections"]["reviewer"].clone()
        };
        let challenge = if initial_native {
            None
        } else {
            assert!(
                app.continue_review(initial.id, replacement_request(choice.clone(), None))
                    .is_err()
            );
            let issued = app
                .replacement_challenge(
                    initial.id,
                    serde_json::from_value(
                        json!({"action": "continue_review", "replacement": choice}),
                    )
                    .unwrap(),
                )
                .unwrap();
            assert_eq!(issued["scope"]["role"], "reviewer");
            assert_eq!(issued["scope"]["predecessor_task_id"], initial.id);
            Some(issued)
        };
        let child = app
            .continue_review(initial.id, replacement_request(choice, challenge.as_ref()))
            .unwrap();
        let payload: Value = serde_json::from_str(&child.payload).unwrap();
        let epoch = payload["role_epochs"]["reviewer"].as_str().unwrap();
        assert_eq!(
            payload["continuation"]["replacement"]["session_epoch"],
            epoch
        );
        assert!(payload["role_epochs"].get("developer").is_none());
        assert!(app.work_once().unwrap());
        let completed = result(&app, child.id);
        assert_eq!(
            completed.outcome,
            Outcome::Success,
            "{}",
            completed.to_json()
        );
        assert_eq!(completed.workspace.as_ref(), Some(root));
        assert!(completed.agent.is_none());
        assert_eq!(
            completed.workflow.as_ref().unwrap().candidate_sha,
            failed.workflow.as_ref().unwrap().candidate_sha
        );
        assert_eq!(
            fs::read(root.join("sessions/developer.json")).unwrap(),
            developer_session
        );
        assert_eq!(
            fs::read(root.join("sessions/reviewer.json")).ok(),
            old_session
        );
        assert_eq!(app.get(initial.id).unwrap(), original);
        assert_eq!(fixture.events("developer").len(), 1);
        assert_eq!(fixture.events("test").len(), 2);
        let reviews = fixture.events("reviewer");
        assert_eq!(reviews.len(), 2);
        assert_ne!(reviews[0]["session_id"], reviews[1]["session_id"]);
        assert_eq!(reviews[0]["cwd"], reviews[1]["cwd"]);
        assert_eq!(reviews[0]["candidate_sha"], reviews[1]["candidate_sha"]);
        let next_prompt = reviews[1]["prompt"].as_str().unwrap();
        assert!(!next_prompt.contains(reviews[0]["session_id"].as_str().unwrap()));
        assert!(!next_prompt.contains("PROVIDER-HISTORY-"));
        let next_session = root.join(format!("sessions/reviewer-{epoch}.json"));
        if initial_native {
            let saved: Value = serde_json::from_slice(&fs::read(next_session).unwrap()).unwrap();
            assert_eq!(saved["epoch"], epoch);
            assert_eq!(saved["session_id"], reviews[1]["session_id"]);
            assert_eq!(saved["ready"], true);
            let args = reviews[1]["argv"].as_array().unwrap();
            assert!(args.iter().any(|arg| arg == "--session-id"));
            assert!(!args.iter().any(|arg| arg == "--resume"));
        } else {
            assert!(!next_session.exists());
            let threads = fixture.events("thread");
            let reviewer = threads
                .iter()
                .find(|event| event["role"] == "reviewer")
                .unwrap();
            assert_eq!(reviewer["method"], "thread/start");
            assert_eq!(reviewer["params"]["ephemeral"], true);
            assert!(reviewer["params"].get("threadId").is_none());
        }
        assert_eq!(fixture.events("publish").len(), 1);
    }
}

#[test]
fn native_and_stateless_strict_reviewer_replacement_rejects_checkout_topology_changes() {
    for initial_native in [true, false] {
        let mut fixture = Fixture::new();
        fixture.configure_strict_reviewer(false);
        fixture.mode("no_verdict");
        let app = fixture.open();
        let initial = fixture.initial_reviewer(&app, initial_native);
        assert!(app.work_once().unwrap());
        let failed = result(&app, initial.id);
        assert_eq!(failed.outcome, Outcome::Failure, "{}", failed.to_json());
        let root = failed.workspace.as_ref().unwrap();
        let claim = fs::read(root.join("claim.json")).unwrap();
        let candidate = git(&root.join("repository"), &["rev-parse", "HEAD"]);
        let choice = if initial_native {
            json!({"profile": "strict"})
        } else {
            fixture.job()["role_selections"]["reviewer"].clone()
        };
        let error = app
            .continue_review(initial.id, replacement_request(choice, None))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("reviewer_topology_change_unsupported"),
            "{error}"
        );
        assert_eq!(fs::read(root.join("claim.json")).unwrap(), claim);
        assert_eq!(
            git(&root.join("repository"), &["rev-parse", "HEAD"]),
            candidate
        );
        assert_eq!(root.join("reviewer-repository").exists(), initial_native);
        assert_eq!(app.list(None).unwrap().len(), 1);
        assert_eq!(fixture.events("developer").len(), 1);
        assert_eq!(fixture.events("test").len(), 1);
        assert_eq!(fixture.events("reviewer").len(), 1);
        assert!(fixture.events("publish").is_empty());
    }
}
