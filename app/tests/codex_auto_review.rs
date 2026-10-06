#![cfg(target_os = "linux")]
use relay_app::{
    Application, RetryRequest, Submission,
    host::{HostConfig, Outcome, RunResult},
};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
};

// These fixtures exercise only local fake protocol processes, never a model.
const CODEX: &str = r#"#!/usr/bin/python3
import copy, json, os, pathlib, sys, time
if '--version' in sys.argv: print('codex-cli 0.160.0'); sys.exit()
if '--help' in sys.argv: print('codex app-server --help'); sys.exit()
assert sys.argv[1:]==['app-server']
def recv():
    line=sys.stdin.readline()
    if not line: sys.exit()
    value=json.loads(line)
    with open(os.environ['TRACE'], 'a') as f: f.write(json.dumps(value)+'\n')
    return value
def send(value): print(json.dumps(value), flush=True)
def batch(*values):
    # One small pipe write makes the incompatible update available alongside
    # the successful response, before Relay may flush a queued turn/start.
    data=''.join(json.dumps(value)+'\n' for value in values).encode()
    assert len(data)<4096
    assert os.write(sys.stdout.fileno(), data)==len(data)
def mutate(settings, mode):
    top={'reviewer':'approvalsReviewer','policy':'approvalPolicy','sandbox':'sandbox','cwd':'cwd'}
    sandbox={'sandbox_type':'type','network':'networkAccess','roots':'writableRoots','tmpdir':'excludeTmpdirEnvVar','slash_tmp':'excludeSlashTmp'}
    wrong={'reviewer':'user','policy':'never','sandbox':{'type':'dangerFullAccess'},'cwd':'/',
           'sandbox_type':'dangerFullAccess','network':True,'roots':['/'], 'tmpdir':True,'slash_tmp':True}
    if mode.startswith(('missing_', 'wrong_')):
        kind, field=mode.split('_', 1)
        target=settings if field in top else settings['sandbox']
        key=(top if field in top else sandbox)[field]
        if kind=='missing': del target[key]
        else: target[key]=wrong[field]
    if mode=='null_reviewer': settings['approvalsReviewer']=None
    if mode=='malformed_reviewer': settings['approvalsReviewer']={'type':'auto_review'}
    if mode=='empty_roots': settings['sandbox']['writableRoots']=[]
def update(settings, mode):
    current=copy.deepcopy(settings)
    mutate(current, mode)
    current.pop('thread')
    if 'sandbox' in current: current['sandboxPolicy']=current.pop('sandbox')
    current.update({'modelProvider':'fixture','collaborationMode':{'mode':'default','settings':{'model':'fixture-model'}}})
    return {'method':'thread/settings/updated','params':{'threadId':'auto-thread','threadSettings':current}}
a=recv(); assert a['method']=='initialize'; send({'id':a['id'],'result':{}})
assert recv()['method']=='initialized'
a=recv(); p=a['params']; resumed=a['method']=='thread/resume'
assert a['method'] in ['thread/start','thread/resume']
assert p['cwd']==os.getcwd() and p['sandbox']=='workspace-write'
assert p['approvalPolicy']=='on-request' and p['approvalsReviewer']=='auto_review'
for key, value in {'network_access':False,'writable_roots':[os.getcwd()],'exclude_tmpdir_env_var':False,'exclude_slash_tmp':False}.items():
    assert p['config']['sandbox_workspace_write.'+key]==value
if resumed: assert p['threadId']=='auto-thread' and p['excludeTurns'] is True
mode=os.environ['RESUME_MODE' if resumed else 'START_MODE']
settings={'thread':{'id':'auto-thread'},'model':'fixture-model','cwd':os.getcwd(),
          'sandbox':{'type':'workspaceWrite','networkAccess':False,'writableRoots':[os.getcwd()],'excludeTmpdirEnvVar':False,'excludeSlashTmp':False},
          'approvalPolicy':'on-request','approvalsReviewer':'auto_review'}
if mode.startswith('batch_'):
    batch({'id':a['id'],'result':settings}, update(settings, mode.removeprefix('batch_')))
else:
    mutate(settings, mode)
    send({'id':a['id'],'result':settings})
a=recv(); assert a['method']=='turn/start'; p=a['params']
assert p['approvalPolicy']=='on-request' and p['approvalsReviewer']=='auto_review'
assert p['sandboxPolicy']=={'type':'workspaceWrite','networkAccess':False,'writableRoots':[os.getcwd()],'excludeTmpdirEnvVar':False,'excludeSlashTmp':False}
turn='resumed-turn' if resumed else 'first-turn'
send({'id':a['id'],'result':{'turn':{'id':turn}}})
if mode=='request':
    send({'id':'permission-1','method':'item/commandExecution/requestApproval','params':{'threadId':'auto-thread','turnId':turn}})
    reply=recv(); assert 'result' not in reply
    time.sleep(60)
if mode.startswith('update_'):
    send(update(settings, mode.removeprefix('update_')))
    time.sleep(60)
if mode=='native_denied':
    review={'method':'item/autoApprovalReview/completed','params':{
        'threadId':'auto-thread','turnId':turn,'reviewId':'native-review-1','targetItemId':'denied-command',
        'startedAtMs':1,'completedAtMs':2,'decisionSource':'agent',
        'action':{'type':'command','command':'fixture-sensitive-command','cwd':os.getcwd(),'source':'shell'},
        'review':{'status':'denied','rationale':'Native policy refused the requested escalation'}}}
    declined={'method':'item/completed','params':{'threadId':'auto-thread','turnId':turn,'item':{
        'id':'denied-command','type':'commandExecution','command':'fixture-sensitive-command','cwd':os.getcwd(),
        'status':'declined','commandActions':[],'aggregatedOutput':'','exitCode':None,'durationMs':None}}}
    batch(review, declined)
    time.sleep(60)
pathlib.Path('changed.txt').write_text('resumed' if resumed else 'first')
send({'method':'item/completed','params':{'threadId':'auto-thread','turnId':turn,'item':{'type':'agentMessage','phase':'final_answer','text':'done'}}})
send({'method':'turn/completed','params':{'threadId':'auto-thread','turn':{'id':turn,'status':'failed' if mode=='fail' else 'completed','error':None}}})
time.sleep(60)
"#;

const POLICY_FAILURES: &[&str] = &[
    "missing_reviewer",
    "null_reviewer",
    "wrong_reviewer",
    "malformed_reviewer",
    "missing_policy",
    "wrong_policy",
    "missing_sandbox",
    "wrong_sandbox",
    "missing_cwd",
    "wrong_cwd",
    "wrong_sandbox_type",
    "missing_network",
    "wrong_network",
    "missing_roots",
    "wrong_roots",
    "missing_tmpdir",
    "wrong_tmpdir",
    "missing_slash_tmp",
    "wrong_slash_tmp",
    "missing_sandbox_type",
];

static FIXTURES: Mutex<()> = Mutex::new(());
struct Fixture {
    temp: tempfile::TempDir,
    config: HostConfig,
    _serial: MutexGuard<'static, ()>,
}
impl Fixture {
    fn new(start: &str, resume: &str) -> Self {
        // Keep fork/exec fixture lifetimes disjoint so an inherited CLOEXEC
        // workspace lease cannot race another fixture's ownership check.
        let serial = FIXTURES.lock().unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("original.txt"), "original\n").unwrap();
        let program = temp.path().join("fake-codex");
        fs::write(&program, CODEX).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let config = serde_json::from_value(json!({
            "workspace_root":temp.path().join("runs"),
            "repositories":{"repo":source},
            "native_agents":{"dev":{
                "provider":"codex_app_server","program":program,
                "allowed_permission_modes":["codex_auto_review"],
                "env":{"TRACE":temp.path().join("trace"),"START_MODE":start,"RESUME_MODE":resume}
            }},
            "timeout_seconds":10,"output_limit_bytes":256,
            "supervisor_program":env!("CARGO_BIN_EXE_relay-app")
        }))
        .unwrap();
        Self {
            temp,
            config,
            _serial: serial,
        }
    }
    fn open(&self) -> Arc<Application> {
        Application::open(self.temp.path().join("queue.db"), self.config.clone()).unwrap()
    }
    fn submit(&self, app: &Application) -> relay::Task {
        let mut job = json!({
            "repository":"repo","requirements":"Make a bounded fixture change","agent":"dev",
            "role_selections":{"developer":{"profile":"dev","native_permission":"codex_auto_review"}}
        });
        let challenge = app
            .permission_challenge(serde_json::from_value(job.clone()).unwrap())
            .unwrap();
        job["role_selections"]["developer"]["confirm_permission_expansion"] = json!(true);
        app.submit(
            serde_json::from_value::<Submission>(json!({
                "key":"auto","job":job,"permission_challenge":challenge["challenge"]
            }))
            .unwrap(),
        )
        .unwrap()
    }
    fn trace(&self) -> Vec<Value> {
        fs::read_to_string(self.temp.path().join("trace"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}
fn run(app: &Application, id: i64) -> RunResult {
    assert!(app.work_once().unwrap());
    serde_json::from_str(app.get(id).unwrap().result.as_deref().unwrap()).unwrap()
}
fn retry(app: &Application, id: i64) -> relay::Task {
    app.retry(
        id,
        RetryRequest {
            replacement: None,
            permission_challenge: None,
            workspace_quota_bytes: None,
            key: "retry-auto".into(),
            confirm_stopped_and_reconciled: true,
        },
    )
    .unwrap()
}
fn assert_unready(root: &Path) {
    let state: Value =
        serde_json::from_slice(&fs::read(root.join("sessions/developer.json")).unwrap()).unwrap();
    assert_eq!(state["ready"], false, "{state}");
}
fn assert_policy_verification(result: &RunResult, mode: &str) {
    let verification = &result
        .agent
        .as_ref()
        .unwrap()
        .provider
        .as_ref()
        .unwrap()
        .selection
        .as_ref()
        .unwrap()
        .verification
        .permission;
    assert_eq!(
        verification,
        if mode.starts_with("wrong_") {
            "mismatch"
        } else {
            "unknown"
        },
        "{mode}: {result:?}"
    );
}
fn turn_count(trace: &[Value]) -> usize {
    trace
        .iter()
        .filter(|request| request["method"] == "turn/start")
        .count()
}

#[test]
fn auto_review_start_and_explicit_retry_resume_keep_native_policy_and_evidence() {
    let fixture = Fixture::new("fail", "valid");
    let app = fixture.open();
    let first = fixture.submit(&app);
    let failed = run(&app, first.id);
    assert_eq!(failed.outcome, Outcome::Failure, "{failed:?}");
    let root = failed.workspace.unwrap();
    assert_unready(&root);
    let next = retry(&app, first.id);
    let result = run(&app, next.id);
    assert_eq!(result.outcome, Outcome::Success, "{result:?}");
    assert_eq!(result.workspace.as_ref(), Some(&root));
    assert_eq!(
        fs::read_to_string(root.join("repository/changed.txt")).unwrap(),
        "resumed"
    );
    let provider = result.agent.as_ref().unwrap().provider.as_ref().unwrap();
    let evidence = serde_json::to_value(provider.selection.as_ref().unwrap()).unwrap();
    assert_eq!(
        evidence["requested"]["native_permission"],
        "codex_auto_review"
    );
    assert_eq!(evidence["requested"]["approvals_reviewer"], "auto_review");
    assert_eq!(
        evidence["session_settings"]["approvals_reviewer"],
        "auto_review"
    );
    assert_eq!(
        evidence["session_settings"]["source"],
        "codex.thread/resume"
    );
    assert_eq!(evidence["verification"]["permission"], "session_reported");
    assert!(provider.reported_model.is_none());
    let trace = fixture.trace();
    assert_eq!(turn_count(&trace), 2);
    assert_eq!(
        trace
            .iter()
            .filter(|request| request["method"] == "thread/start")
            .count(),
        1
    );
    assert_eq!(
        trace
            .iter()
            .filter(|request| request["method"] == "thread/resume")
            .count(),
        1
    );
}

#[test]
fn auto_review_missing_or_mismatched_start_policy_never_sends_a_turn() {
    for mode in POLICY_FAILURES {
        let fixture = Fixture::new(mode, "valid");
        let app = fixture.open();
        let task = fixture.submit(&app);
        let result = run(&app, task.id);
        assert_eq!(result.outcome, Outcome::Failure, "{mode}: {result:?}");
        assert_policy_verification(&result, mode);
        let root = result.workspace.unwrap();
        assert_unready(&root);
        assert!(!root.join("repository/changed.txt").exists(), "{mode}");
        assert_eq!(turn_count(&fixture.trace()), 0, "{mode}");
    }
}

#[test]
fn auto_review_resume_revalidates_policy_before_sending_another_turn() {
    for mode in POLICY_FAILURES {
        let fixture = Fixture::new("fail", mode);
        let app = fixture.open();
        let first = fixture.submit(&app);
        let failed = run(&app, first.id);
        assert_eq!(failed.outcome, Outcome::Failure, "{mode}: {failed:?}");
        let root = failed.workspace.unwrap();
        let next = retry(&app, first.id);
        let result = run(&app, next.id);
        assert_eq!(result.outcome, Outcome::Failure, "{mode}: {result:?}");
        assert_policy_verification(&result, mode);
        assert_unready(&root);
        assert_eq!(
            fs::read_to_string(root.join("repository/changed.txt")).unwrap(),
            "first"
        );
        let trace = fixture.trace();
        assert_eq!(
            trace
                .iter()
                .filter(|request| request["method"] == "thread/resume")
                .count(),
            1,
            "{mode}: {trace:?}"
        );
        assert_eq!(turn_count(&trace), 1, "{mode}: {trace:?}");
    }
}

#[test]
fn auto_review_does_not_authorize_relay_client_approval_requests() {
    let fixture = Fixture::new("request", "valid");
    let app = fixture.open();
    let task = fixture.submit(&app);
    let result = run(&app, task.id);
    assert_eq!(result.outcome, Outcome::Failure, "{result:?}");
    let root = result.workspace.unwrap();
    assert_unready(&root);
    assert!(!root.join("repository/changed.txt").exists());
    let trace = fixture.trace();
    assert_eq!(turn_count(&trace), 1, "{trace:?}");
    // Once failure is known, closing stdin without an acknowledgement is safe.
    // No approval response, alternative policy, or second turn is authorized.
    assert!(
        !trace
            .iter()
            .any(|response| response["id"] == "permission-1" && response.get("result").is_some()),
        "{trace:?}"
    );
}

#[test]
fn auto_review_runtime_updates_revalidate_the_entire_baseline() {
    for field in [
        "wrong_reviewer",
        "missing_reviewer",
        "wrong_network",
        "missing_network",
        "wrong_roots",
        "missing_roots",
        "wrong_cwd",
        "missing_cwd",
        "wrong_tmpdir",
        "missing_tmpdir",
        "wrong_slash_tmp",
        "missing_slash_tmp",
    ] {
        let mode = format!("update_{field}");
        let fixture = Fixture::new(&mode, "valid");
        let app = fixture.open();
        let task = fixture.submit(&app);
        let result = run(&app, task.id);
        assert_eq!(result.outcome, Outcome::Failure, "{mode}: {result:?}");
        assert_policy_verification(&result, field);
        let evidence = serde_json::to_value(
            result
                .agent
                .as_ref()
                .unwrap()
                .provider
                .as_ref()
                .unwrap()
                .selection
                .as_ref()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            evidence["session_settings"]["source"],
            "codex.thread/settings/updated"
        );
        if field == "wrong_reviewer" {
            assert_eq!(evidence["session_settings"]["approvals_reviewer"], "user");
        } else if field == "missing_reviewer" {
            assert!(evidence["session_settings"]["approvals_reviewer"].is_null());
        }
        let root = result.workspace.unwrap();
        assert_unready(&root);
        assert!(!root.join("repository/changed.txt").exists());
        assert_eq!(turn_count(&fixture.trace()), 1, "{mode}");
    }
}

#[test]
fn auto_review_batched_policy_drift_never_flushes_the_pending_turn() {
    for resumed in [false, true] {
        for field in [
            "wrong_reviewer",
            "wrong_network",
            "wrong_roots",
            "missing_cwd",
        ] {
            let mode = format!("batch_{field}");
            let fixture = Fixture::new(if resumed { "fail" } else { &mode }, &mode);
            let app = fixture.open();
            let first = fixture.submit(&app);
            let first_result = run(&app, first.id);
            let result = if resumed {
                assert_eq!(first_result.outcome, Outcome::Failure, "{first_result:?}");
                let next = retry(&app, first.id);
                run(&app, next.id)
            } else {
                first_result
            };
            assert_eq!(result.outcome, Outcome::Failure, "{mode}: {result:?}");
            assert_policy_verification(&result, field);
            let root = result.workspace.unwrap();
            assert_unready(&root);
            let trace = fixture.trace();
            assert_eq!(
                turn_count(&trace),
                usize::from(resumed),
                "{mode}: {trace:?}"
            );
            if resumed {
                assert_eq!(
                    fs::read_to_string(root.join("repository/changed.txt")).unwrap(),
                    "first"
                );
            } else {
                assert!(!root.join("repository/changed.txt").exists());
            }
        }
    }
}

#[test]
fn auto_review_native_denial_is_recorded_and_declined_action_stops_without_fallback() {
    let fixture = Fixture::new("native_denied", "valid");
    let app = fixture.open();
    let task = fixture.submit(&app);
    let result = run(&app, task.id);
    assert_eq!(result.outcome, Outcome::Failure, "{result:?}");
    let provider = result.agent.as_ref().unwrap().provider.as_ref().unwrap();
    let evidence = serde_json::to_value(provider.selection.as_ref().unwrap()).unwrap();
    assert_eq!(
        evidence["observed"]["native_approval_reviews"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let review = &evidence["observed"]["native_approval_reviews"][0];
    assert_eq!(review["status"], "denied");
    assert_eq!(review["action_type"], "command");
    assert_eq!(review["review_id"], "native-review-1");
    assert!(
        review["rationale"]
            .as_str()
            .unwrap()
            .contains("Native policy refused")
    );
    assert!(!evidence.to_string().contains("fixture-sensitive-command"));
    assert!(provider.reported_model.is_none());
    let root = result.workspace.unwrap();
    assert_unready(&root);
    assert!(!root.join("repository/changed.txt").exists());
    let trace = fixture.trace();
    assert_eq!(turn_count(&trace), 1, "{trace:?}");
    assert!(
        trace.iter().all(|message| message.get("result").is_none()),
        "{trace:?}"
    );
}

#[test]
fn auto_review_accepts_explicit_or_implicit_workspace_roots() {
    for mode in ["valid", "empty_roots"] {
        let fixture = Fixture::new(mode, "valid");
        let app = fixture.open();
        let task = fixture.submit(&app);
        let result = run(&app, task.id);
        assert_eq!(result.outcome, Outcome::Success, "{mode}: {result:?}");
        assert_eq!(turn_count(&fixture.trace()), 1);
    }
    let fixture = Fixture::new("fail", "empty_roots");
    let app = fixture.open();
    let first = fixture.submit(&app);
    assert_eq!(run(&app, first.id).outcome, Outcome::Failure);
    let next = retry(&app, first.id);
    let result = run(&app, next.id);
    assert_eq!(result.outcome, Outcome::Success, "{result:?}");
    assert_eq!(turn_count(&fixture.trace()), 2);
}
