#![cfg(target_os = "linux")]
use relay::{State, Task};
use relay_app::host::{Host, HostConfig, Outcome};
use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard, atomic::AtomicBool};
use tempfile::TempDir;

const CODEX: &str = r#"#!/usr/bin/python3
import json, os, pathlib, sys, time
if '--version' in sys.argv: print('codex-cli 0.160.0'); sys.exit()
if '--help' in sys.argv: print('codex app-server --help'); sys.exit()
assert sys.argv[1:] == ['app-server']
def recv(): return json.loads(sys.stdin.readline())
def send(value): print(json.dumps(value),flush=True)
a=recv(); assert a['method']=='initialize'; send({'id':a['id'],'result':{}})
assert recv()['method']=='initialized'
a=recv(); count=pathlib.Path('.git/turn-count'); n=int(count.read_text()) if count.exists() else 0
assert a['method']==('thread/resume' if n else 'thread/start')
assert a['params']['sandbox']=='workspace-write' and a['params']['approvalPolicy']=='never'
if n: assert a['params']['threadId']=='developer-thread' and a['params']['excludeTurns'] is True
send({'id':a['id'],'result':{'thread':{'id':'developer-thread'},'model':'fixture-model'}})
if n:
    # Cold resume restores historical usage before the new turn starts.
    send({'method':'thread/tokenUsage/updated','params':{'threadId':'developer-thread','turnId':'turn-'+str(n-1),'tokenUsage':{'last':{'inputTokens':900}}}})
a=recv(); assert a['method']=='turn/start'; p=a['params']; assert p['sandboxPolicy']['networkAccess'] is False
assert p['cwd']==os.getcwd() and p['approvalPolicy']=='never'
turn='turn-'+str(n);send({'id':a['id'],'result':{'turn':{'id':turn}}})
mode=os.environ.get('MODE','success')
if mode=='request':
    send({'id':'permission-1','method':'item/commandExecution/requestApproval','params':{}})
    response=recv(); assert response['error']['code']==-32601
    pathlib.Path('.git/denied').write_text('yes'); time.sleep(60)
if mode=='sleep':
    pathlib.Path('.git/started').write_text(str(os.getpid())); time.sleep(60)
if mode=='oversize': print('x'*65537,flush=True); time.sleep(60)
if mode=='malformed': print('bad',flush=True); time.sleep(60)
if mode=='wrong': turn='wrong-turn'
if n and os.environ.get('RELAY_WORKFLOW_ROUND')=='1': assert 'Fix the fixture issue' in p['input'][0]['text']
pathlib.Path('changed.txt').write_text('round '+str(n))
count.write_text(str(n+1))
for method in ['turn/diff/updated','turn/plan/updated','turn/moderationMetadata']:
    send({'method':method,'params':{'threadId':'developer-thread','turnId':turn}})
send({'method':'item/completed','params':{'threadId':'developer-thread','turnId':turn,'item':{'type':'agentMessage','phase':'final_answer','text':'done'}}})
if mode=='missing': sys.exit(0)
send({'method':'turn/completed','params':{'threadId':'developer-thread','turn':{'id':turn,'status':'failed' if mode=='failed' or (mode=='fail_once' and n==0) else 'completed','error':None}}})
# Real app-server stays alive after its turn. Relay must stop and reap it.
time.sleep(60)
"#;
const CLAUDE: &str = r#"#!/usr/bin/python3
import json, os, pathlib, sys
if '--version' in sys.argv: print('2.1.281 (Claude Code)'); sys.exit()
if '--help' in sys.argv:
    print('--output-format --verbose --permission-prompts --restricted --tools --allowedTools --disallowedTools --disable-slash-commands --strict-mcp-config --mcp-config --no-session-persistence --session-id --resume');sys.exit()
prompt=sys.stdin.read();review='--restricted' in sys.argv
assert '--no-session-persistence' not in sys.argv and '--continue' not in sys.argv
count=pathlib.Path('.git/claude-turn-count'); n=int(count.read_text()) if count.exists() else 0
saved=pathlib.Path('.git/claude-session')
if n:
    assert '--resume' in sys.argv and '--session-id' not in sys.argv
    sid=sys.argv[sys.argv.index('--resume')+1]; assert sid==saved.read_text()
else:
    assert '--session-id' in sys.argv and '--resume' not in sys.argv
    sid=sys.argv[sys.argv.index('--session-id')+1]; saved.write_text(sid)
if review:
    assert 'Read,Glob,Grep' in sys.argv and 'Bash,Edit,Write,NotebookEdit,Agent,Task,mcp__*' in sys.argv
    assert pathlib.Path.cwd().name=='reviewer-repository'
    assert not pathlib.Path('.git/turn-count').exists()
    assert pathlib.Path('.git/relay-review.patch').is_file()
    verdict='changes_requested' if n==0 else 'approved'
    answer=json.dumps({'candidate_sha':os.environ['RELAY_CANDIDATE_SHA'],'verdict':verdict,'summary':'Reviewed candidate','findings':['Fix the fixture issue'] if n==0 else []})
else:
    if n: assert 'Fix the fixture issue' in prompt
    pathlib.Path('changed.txt').write_text('round '+str(n));answer='done'
count.write_text(str(n+1))
print(json.dumps({'type':'system','subtype':'init','session_id':sid,'model':'fixture-model'}))
print(json.dumps({'type':'result','subtype':'success','is_error':False,'session_id':sid,'result':answer,'permission_denials':[]}))
"#;
struct Fixture {
    _temp: TempDir,
    config: HostConfig,
    _serial: MutexGuard<'static, ()>,
}
// A concurrent fork can transiently inherit another fixture's CLOEXEC lease
// before exec, so its nonblocking ownership check correctly reports EAGAIN.
// Keep fixture lifetimes disjoint, as in review_continuation.rs; never retry
// ownership checks or weaken production lease behavior to accommodate a test.
static FIXTURES: Mutex<()> = Mutex::new(());
impl Fixture {
    fn new(provider: &str, workflow: bool) -> Self {
        let serial = FIXTURES.lock().unwrap_or_else(|error| error.into_inner());
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("original.txt"), "original").unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["add", "."],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "base",
            ],
        ] {
            assert!(
                Command::new("/usr/bin/git")
                    .args(args)
                    .current_dir(&source)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let codex = temp.path().join("codex.py");
        let claude = temp.path().join("claude.py");
        for (path, script) in [(&codex, CODEX), (&claude, CLAUDE)] {
            fs::write(path, script).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let config=serde_json::from_value(json!({
            "workspace_root":temp.path().join("runs"),"repositories":{"source":source},
            "native_agents":{"developer":{"provider":provider,"program":if provider=="claude_cli" {&claude} else {&codex},"session_continuity":true},"reviewer":{"provider":"claude_cli","program":claude,"session_continuity":true}},
            "tests":{"test":{"program":"/bin/true"}},
            "workflows":if workflow {json!({"reviewed":{"repository":"source","developer":"developer","reviewer":"reviewer","test":"test","max_repairs":1}})} else {json!({})},
            "timeout_seconds":30,"output_limit_bytes":64,"supervisor_program":env!("CARGO_BIN_EXE_relay-app")
        })).unwrap();
        Self {
            _temp: temp,
            config,
            _serial: serial,
        }
    }
    fn task(&self, id: i64) -> Task {
        let payload=json!({"repository":"source","requirements":"Implement one bounded change","agent":"developer","workflow":if self.config.workflows.is_empty(){None}else{Some("reviewed")}}).to_string();
        Task {
            id,
            key: format!("task-{id}"),
            payload,
            state: State::Claimed,
            generation: 1,
            owner: Some("fixture".into()),
            result: None,
        }
    }
}
#[test]
fn native_rounds_resume_explicit_role_sessions_in_fixed_separate_checkouts() {
    for provider in ["codex_app_server", "claude_cli"] {
        let f = Fixture::new(provider, true);
        let host = Host::new(f.config.clone()).unwrap();
        let result = host.execute(&f.task(1), Arc::new(AtomicBool::new(false)));
        assert_eq!(result.outcome, Outcome::Success, "{result:?}");
        let root = result.workspace.unwrap();
        let dev: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("sessions/developer.json")).unwrap())
                .unwrap();
        let review: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("sessions/reviewer.json")).unwrap())
                .unwrap();
        assert_ne!(dev["session_id"], review["session_id"]);
        assert_ne!(dev["cwd"], review["cwd"]);
        assert_eq!(dev["ready"], true);
        assert_eq!(review["ready"], true);
        assert_eq!(
            fs::read_to_string(root.join("reviewer-repository/.git/claude-turn-count")).unwrap(),
            "2"
        );
        assert_eq!(result.workflow.unwrap().rounds.len(), 2);
        let second = host.execute(&f.task(2), Arc::new(AtomicBool::new(false)));
        assert_eq!(second.outcome, Outcome::Success, "{second:?}");
        let second_root = second.workspace.unwrap();
        assert_ne!(root, second_root);
        let second_review: serde_json::Value =
            serde_json::from_slice(&fs::read(second_root.join("sessions/reviewer.json")).unwrap())
                .unwrap();
        assert_ne!(review["session_id"], second_review["session_id"]);
    }
}
#[test]
fn app_server_failure_protocol_never_becomes_success_or_resumable() {
    for mode in [
        "request",
        "failed",
        "wrong",
        "missing",
        "oversize",
        "malformed",
    ] {
        let mut f = Fixture::new("codex_app_server", false);
        f.config
            .native_agents
            .get_mut("developer")
            .unwrap()
            .env
            .insert("MODE".into(), mode.into());
        let result = Host::new(f.config.clone())
            .unwrap()
            .execute(&f.task(1), Arc::new(AtomicBool::new(false)));
        assert_eq!(result.outcome, Outcome::Failure, "{mode}: {result:?}");
        let state: serde_json::Value = serde_json::from_slice(
            &fs::read(result.workspace.unwrap().join("sessions/developer.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(state["ready"], false);
    }
}

#[test]
fn app_server_timeout_and_cancellation_stop_process_before_returning() {
    for cancel in [false, true] {
        let mut f = Fixture::new("codex_app_server", false);
        f.config
            .native_agents
            .get_mut("developer")
            .unwrap()
            .env
            .insert("MODE".into(), "sleep".into());
        if !cancel {
            f.config.timeout_seconds = 1;
        }
        let task = f.task(1);
        let host = Host::new(f.config.clone()).unwrap();
        let cancellation = Arc::new(AtomicBool::new(false));
        let stop = cancellation.clone();
        let root = f.config.workspace_root.join("task-1");
        let run = std::thread::spawn(move || host.execute(&task, cancellation));
        let marker = root.join("repository/.git/started");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(marker.exists());
        let pid: i32 = fs::read_to_string(marker).unwrap().parse().unwrap();
        if cancel {
            stop.store(true, std::sync::atomic::Ordering::Release);
        }
        let result = run.join().unwrap();
        assert_eq!(
            result.outcome,
            if cancel {
                Outcome::Cancelled
            } else {
                Outcome::TimedOut
            },
            "{result:?}"
        );
        // SAFETY: signal 0 only observes whether the fixture PID remains.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        let state: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("sessions/developer.json")).unwrap())
                .unwrap();
        assert_eq!(state["ready"], false);
    }
}

#[test]
fn explicitly_retried_failed_app_server_turn_resumes_checkpointed_id_and_files() {
    let mut f = Fixture::new("codex_app_server", false);
    f.config
        .native_agents
        .get_mut("developer")
        .unwrap()
        .env
        .insert("MODE".into(), "fail_once".into());
    let app =
        relay_app::Application::open(f._temp.path().join("retry.db"), f.config.clone()).unwrap();
    let job = serde_json::from_str(&f.task(1).payload).unwrap();
    app.submit(relay_app::Submission {
        permission_challenge: None,
        key: "first".into(),
        job,
    })
    .unwrap();
    app.work_once().unwrap();
    let first: relay_app::host::RunResult =
        serde_json::from_str(app.get(1).unwrap().result.as_ref().unwrap()).unwrap();
    assert_eq!(first.outcome, Outcome::Failure);
    let root = first.workspace.unwrap();
    let before: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("sessions/developer.json")).unwrap()).unwrap();
    assert_eq!(before["session_id"], "developer-thread");
    assert_eq!(before["ready"], false);
    app.retry(
        1,
        relay_app::RetryRequest {
            workspace_quota_bytes: None,
            key: "continue".into(),
            confirm_stopped_and_reconciled: true,
        },
    )
    .unwrap();
    app.work_once().unwrap_or_else(|error| {
        panic!(
            "continued execution failed: {error}; application status: {:?}",
            app.status()
        )
    });
    let next: relay_app::host::RunResult =
        serde_json::from_str(app.get(2).unwrap().result.as_ref().unwrap()).unwrap();
    assert_eq!(next.outcome, Outcome::Success, "{next:?}");
    assert_eq!(next.workspace.as_ref(), Some(&root));
    assert_eq!(
        fs::read_to_string(root.join("repository/.git/turn-count")).unwrap(),
        "2"
    );
}

#[test]
fn reviewer_copy_shares_budget_and_reused_checkout_is_not_counted_twice() {
    let mut f = Fixture::new("codex_app_server", true);
    let source = &f.config.repositories["source"];
    fs::write(source.join("large"), vec![b'x'; 1024 * 1024]).unwrap();
    for args in [
        vec!["add", "large"],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-m",
            "sized baseline",
        ],
    ] {
        assert!(
            Command::new("/usr/bin/git")
                .args(args)
                .current_dir(source)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    f.config.max_workspace_bytes = Some(2560 * 1024);
    let result = Host::new(f.config.clone())
        .unwrap()
        .execute(&f.task(1), Arc::new(AtomicBool::new(false)));
    assert_eq!(result.outcome, Outcome::Success, "{result:?}");
    assert_eq!(result.workflow.unwrap().rounds.len(), 2);
    let workspace = result.workspace.unwrap();
    assert!(workspace.join("repository/large").exists());
    assert!(workspace.join("reviewer-repository/large").exists());
}

#[test]
fn codex_explicit_permission_mismatch_never_starts_a_turn_or_becomes_resumable() {
    use relay_app::providers::NativePermission;
    let mut f = Fixture::new("codex_app_server", false);
    let profile = f.config.native_agents.get_mut("developer").unwrap();
    profile.native_permission = Some(NativePermission::CodexWorkspaceWrite);
    fs::write(&profile.program, CODEX.replace(
        "'model':'fixture-model'", "'model':'fixture-model','sandbox':{'type':'dangerFullAccess'},'approvalPolicy':'never'"
    )).unwrap();
    let result = Host::new(f.config.clone())
        .unwrap()
        .execute(&f.task(1), Arc::new(AtomicBool::new(false)));
    assert_eq!(result.outcome, Outcome::Failure, "{result:?}");
    let provider = result
        .agent
        .as_ref()
        .and_then(|stage| stage.provider.as_ref())
        .unwrap();
    assert_eq!(
        provider.selection.as_ref().unwrap().verification.permission,
        "mismatch"
    );
    let root = result.workspace.unwrap();
    assert!(!root.join("repository/changed.txt").exists());
    assert!(!root.join("repository/.git/turn-count").exists());
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("sessions/developer.json")).unwrap()).unwrap();
    assert_eq!(state["ready"], false);
}

#[test]
fn claude_permission_evidence_survives_host_and_mismatch_stops_sleeping_process() {
    use relay_app::providers::NativePermission;
    const SCRIPT: &str = r#"#!/usr/bin/python3
import json, os, pathlib, sys, time
if '--version' in sys.argv: print('2.1.281 (Claude Code)'); sys.exit()
if '--help' in sys.argv:
    print('--output-format --verbose --permission-prompts --permission-mode --session-id --resume --model --effort');sys.exit()
assert sys.argv[sys.argv.index('--permission-mode')+1]=='auto'
assert sys.argv[sys.argv.index('--permission-prompts')+1]=='none'
prompt=sys.stdin.read(); sid=sys.argv[sys.argv.index('--session-id')+1]
pathlib.Path('.git/permission-pid').write_text(str(os.getpid()))
mismatch=os.environ.get('MISMATCH')=='yes'
print(json.dumps({'type':'system','subtype':'init','session_id':sid,'model':'session-model','effort':'high','permissionMode':'bypassPermissions' if mismatch else 'auto'}),flush=True)
if mismatch: time.sleep(30)
print(json.dumps({'type':'assistant','message':{'model':'main-model'}}),flush=True)
print(json.dumps({'type':'assistant','parent_tool_use_id':'nested','message':{'model':'subagent-model'}}),flush=True)
print(json.dumps({'type':'result','subtype':'success','is_error':False,'session_id':sid,'result':'done'}),flush=True)
"#;
    for mismatch in [false, true] {
        let mut f = Fixture::new("claude_cli", false);
        let profile = f.config.native_agents.get_mut("developer").unwrap();
        profile.native_permission = Some(NativePermission::ClaudeAuto);
        profile.allowed_permission_modes = vec![NativePermission::ClaudeAuto];
        profile.model = Some("requested-model".into());
        profile.effort = Some("high".into());
        if mismatch {
            profile.env.insert("MISMATCH".into(), "yes".into());
        }
        fs::write(&profile.program, SCRIPT).unwrap();
        let mut job: serde_json::Value = serde_json::from_str(&f.task(1).payload).unwrap();
        job["role_selections"] =
            json!({"developer":{"profile":"developer","native_permission":"claude_auto"}});
        let app = relay_app::Application::open(
            f._temp.path().join("permission-evidence.db"),
            f.config.clone(),
        )
        .unwrap();
        let challenge = app
            .permission_challenge(serde_json::from_value(job.clone()).unwrap())
            .unwrap();
        job["role_selections"]["developer"]["confirm_permission_expansion"] = json!(true);
        let task = app.submit(serde_json::from_value(json!({
            "key":"permission-evidence","job":job,"permission_challenge":challenge["challenge"],
        })).unwrap()).unwrap();
        let started = std::time::Instant::now();
        assert!(app.work_once().unwrap());
        let result: relay_app::host::RunResult =
            serde_json::from_str(app.get(task.id).unwrap().result.as_deref().unwrap()).unwrap();
        assert_eq!(
            result.outcome,
            if mismatch {
                Outcome::Failure
            } else {
                Outcome::Success
            },
            "{result:?}"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        let provider = result
            .agent
            .as_ref()
            .and_then(|stage| stage.provider.as_ref())
            .unwrap();
        let evidence = provider.selection.as_ref().unwrap();
        assert_eq!(evidence.requested.profile.as_deref(), Some("developer"));
        assert_eq!(evidence.requested.model.as_deref(), Some("requested-model"));
        assert_eq!(
            evidence.verification.permission,
            if mismatch {
                "mismatch"
            } else {
                "session_reported"
            }
        );
        assert_eq!(
            evidence.session_settings.as_ref().unwrap().model.as_deref(),
            Some("session-model")
        );
        if !mismatch {
            assert_eq!(provider.reported_model.as_deref(), Some("main-model"));
        }
        let root = result.workspace.unwrap();
        let pid: i32 = fs::read_to_string(root.join("repository/.git/permission-pid"))
            .unwrap()
            .parse()
            .unwrap();
        // SAFETY: signal 0 observes the fixture process after host cleanup.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        let state: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("sessions/developer.json")).unwrap())
                .unwrap();
        assert_eq!(state["ready"], !mismatch);
    }
}
