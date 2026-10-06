#![cfg(target_os = "linux")]
use relay_app::{
    Application, RetryRequest, Submission,
    host::{HostConfig, Outcome, RunResult},
};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

// Keep fake process lifetimes disjoint: concurrent forks can temporarily inherit
// another test's CLOEXEC lease before exec. Production locking stays unchanged.
static FIXTURES: Mutex<()> = Mutex::new(());
const NATIVE: &str = r#"#!/usr/bin/python3
import json, pathlib, subprocess, sys, time, uuid
kind=pathlib.Path(sys.argv[0]).stem
if '--version' in sys.argv:
    print('codex-cli 0.160.0' if kind=='codex' else '2.1.281 (Claude Code)'); sys.exit()
if '--help' in sys.argv:
    print('app-server --model --output-format --verbose --permission-prompts --restricted --tools --allowedTools --disallowedTools --disable-slash-commands --strict-mcp-config --mcp-config --no-session-persistence --session-id --resume'); sys.exit()
def recv(): return json.loads(sys.stdin.readline())
def send(v): print(json.dumps(v),flush=True)
if kind=='codex':
    assert sys.argv[1:]==['app-server']
    request=recv(); assert request['method']=='initialize'; send({'id':request['id'],'result':{}})
    assert recv()['method']=='initialized'
    request=recv(); action=request['method']; params=request['params']
    assert action in ['thread/start','thread/resume']
    assert params['approvalPolicy']=='never' and params['sandbox']=='workspace-write'
    resumed=action=='thread/resume'
    sid=params['threadId'] if resumed else 'codex-'+str(uuid.uuid4())
    assert sid.startswith('codex-')
    if resumed: assert params['excludeTurns'] is True
    model=params['model']
    send({'id':request['id'],'result':{'thread':{'id':sid},'model':model}})
    request=recv(); assert request['method']=='turn/start'
    prompt=request['params']['input'][0]['text']
    assert request['params']['sandboxPolicy']['networkAccess'] is False
    turn='turn-'+str(uuid.uuid4()); send({'id':request['id'],'result':{'turn':{'id':turn}}})
else:
    prompt=sys.stdin.read()
    assert '--continue' not in sys.argv and '--no-session-persistence' not in sys.argv
    resumed='--resume' in sys.argv
    flag='--resume' if resumed else '--session-id'
    assert ('--session-id' not in sys.argv) if resumed else ('--resume' not in sys.argv)
    sid=sys.argv[sys.argv.index(flag)+1]; uuid.UUID(sid)
    model=sys.argv[sys.argv.index('--model')+1]
log=pathlib.Path('.git/native-turns.jsonl')
events=[json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
prior=[event for event in events if event['id']==sid]
assert bool(prior)==resumed
for event in events:
    assert event['id'] not in prompt, 'provider history leaked into new prompt'
if not events:
    subprocess.run(['/usr/bin/git','add','original.txt'],check=True)
    subprocess.run(['/usr/bin/git','-c','user.name=Fixture','-c','user.email=fixture@example.invalid','commit','--quiet','-m','base'],check=True)
    pathlib.Path('original.txt').write_text('retained dirty tracked file\n')
    pathlib.Path('untracked.txt').write_text('retained untracked file\n')
else:
    assert pathlib.Path('original.txt').read_text()=='retained dirty tracked file\n'
    assert pathlib.Path('untracked.txt').read_text()=='retained untracked file\n'
with log.open('a') as stream:
    stream.write(json.dumps({'provider':kind,'id':sid,'resume':resumed,'model':model})+'\n')
failed=model=='initial' or (model=='replacement-fail-once' and not prior)
text='untrusted provider diagnostic '+sid
if kind=='codex':
    send({'method':'item/completed','params':{'threadId':sid,'turnId':turn,'item':{'type':'agentMessage','phase':'final_answer','text':text}}})
    send({'method':'turn/completed','params':{'threadId':sid,'turn':{'id':turn,'status':'failed' if failed else 'completed','error':None}}})
    time.sleep(60)
else:
    send({'type':'system','subtype':'init','session_id':sid,'model':model})
    send({'type':'result','subtype':'error_during_execution' if failed else 'success','is_error':failed,'session_id':sid,'result':text,'permission_denials':[]})
"#;

struct Fixture {
    temp: tempfile::TempDir,
    config: HostConfig,
    _serial: MutexGuard<'static, ()>,
}
impl Fixture {
    fn new(initial: &str) -> Self {
        let serial = FIXTURES.lock().unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("original.txt"), "original\n").unwrap();
        for kind in ["codex", "claude"] {
            let script = temp.path().join(format!("{kind}.py"));
            fs::write(&script, NATIVE).unwrap();
            fs::set_permissions(script, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let config=serde_json::from_value(json!({
            "workspace_root":temp.path().join("runs"),"repositories":{"source":source},
            "native_agents":{
                "codex":{"provider":"codex_app_server","program":temp.path().join("codex.py"),"model":if initial=="codex" {"initial"}else{"replacement-success"}},
                "claude":{"provider":"claude_cli","program":temp.path().join("claude.py"),"session_continuity":true,"model":if initial=="claude" {"initial"}else{"replacement-success"}}
            },
            "timeout_seconds":15,"output_limit_bytes":4096,"supervisor_program":env!("CARGO_BIN_EXE_relay-app")
        })).unwrap();
        Self {
            temp,
            config,
            _serial: serial,
        }
    }
    fn app(&self) -> std::sync::Arc<Application> {
        Application::open(self.temp.path().join("relay.db"), self.config.clone()).unwrap()
    }
    fn initial(&self, app: &Application, profile: &str) -> (relay::Task, PathBuf) {
        let task=app.submit(serde_json::from_value::<Submission>(json!({"key":"initial","job":{"repository":"source","requirements":"Finish the retained change","agent":profile}})).unwrap()).unwrap();
        app.work_once().unwrap();
        let result = result(app, task.id);
        assert_eq!(result.outcome, Outcome::Failure, "{result:?}");
        assert_eq!(
            serde_json::to_value(&result).unwrap()["stopped_stage"]["role"],
            "developer"
        );
        let root = result.workspace.unwrap();
        assert_dirty(&root);
        (app.get(task.id).unwrap(), root)
    }
}
fn result(app: &Application, id: i64) -> RunResult {
    serde_json::from_str(app.get(id).unwrap().result.as_deref().unwrap()).unwrap()
}
fn retry(app: &Application, id: i64, key: &str, replacement: Option<Value>) -> relay::Task {
    app.retry(
        id,
        serde_json::from_value::<RetryRequest>(
            json!({"key":key,"confirm_stopped_and_reconciled":true,"replacement":replacement}),
        )
        .unwrap(),
    )
    .unwrap()
}
fn events(root: &Path) -> Vec<Value> {
    fs::read_to_string(root.join("repository/.git/native-turns.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn epoch(task: &relay::Task) -> String {
    let job: Value = serde_json::from_str(&task.payload).unwrap();
    let epoch = job["role_epochs"]["developer"].as_str().unwrap().to_owned();
    assert_eq!(job["continuation"]["replacement"]["session_epoch"], epoch);
    epoch
}
fn assert_dirty(root: &Path) {
    assert_eq!(
        fs::read_to_string(root.join("repository/original.txt")).unwrap(),
        "retained dirty tracked file\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("repository/untracked.txt")).unwrap(),
        "retained untracked file\n"
    );
}

#[test]
fn cross_provider_replacement_preserves_files_and_starts_fresh_both_directions() {
    for (initial, replacement) in [("codex", "claude"), ("claude", "codex")] {
        let fixture = Fixture::new(initial);
        let app = fixture.app();
        let (first, root) = fixture.initial(&app, initial);
        let legacy_path = root.join("sessions/developer.json");
        let legacy = fs::read(&legacy_path).unwrap();
        let successor = retry(
            &app,
            first.id,
            "replace",
            Some(json!({"profile":replacement})),
        );
        let epoch = epoch(&successor);
        assert_eq!(fs::read(&legacy_path).unwrap(), legacy);
        app.work_once().unwrap();
        let next = result(&app, successor.id);
        assert_eq!(next.outcome, Outcome::Success, "{next:?}");
        assert_eq!(next.workspace.as_ref(), Some(&root));
        assert_eq!(app.get(first.id).unwrap().result, first.result);
        assert_eq!(fs::read(&legacy_path).unwrap(), legacy);
        assert_dirty(&root);
        let journal = events(&root);
        assert_eq!(journal.len(), 2);
        assert_eq!(journal[0]["provider"], initial);
        assert_eq!(journal[1]["provider"], replacement);
        assert_eq!(journal[1]["resume"], false);
        assert_ne!(journal[0]["id"], journal[1]["id"]);
        let selected: Value = serde_json::from_slice(
            &fs::read(root.join(format!("sessions/developer-{epoch}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(selected["epoch"], epoch);
        assert_eq!(selected["session_id"], journal[1]["id"]);
        assert_eq!(selected["ready"], true);
    }
}

#[test]
fn same_provider_model_replacement_starts_a_new_epoch() {
    for provider in ["codex", "claude"] {
        let fixture = Fixture::new(provider);
        let app = fixture.app();
        let (first, root) = fixture.initial(&app, provider);
        let successor = retry(
            &app,
            first.id,
            "new-model",
            Some(
                json!({"profile":provider,"model":{"value":"replacement-success","source":"manual"}}),
            ),
        );
        let _epoch = epoch(&successor);
        app.work_once().unwrap();
        let next = result(&app, successor.id);
        assert_eq!(next.outcome, Outcome::Success, "{next:?}");
        let journal = events(&root);
        assert_eq!(journal.len(), 2);
        assert_eq!(journal[1]["provider"], provider);
        assert_eq!(journal[1]["model"], "replacement-success");
        assert_eq!(journal[1]["resume"], false);
        assert_ne!(journal[0]["id"], journal[1]["id"]);
        assert_dirty(&root);
    }
}

#[test]
fn ordinary_retry_inherits_epoch_and_resumes_exact_provider_id() {
    for provider in ["codex", "claude"] {
        let fixture = Fixture::new(provider);
        let app = fixture.app();
        let (first, root) = fixture.initial(&app, provider);
        let successor = retry(
            &app,
            first.id,
            "replace-fail-once",
            Some(
                json!({"profile":provider,"model":{"value":"replacement-fail-once","source":"manual"}}),
            ),
        );
        let epoch = epoch(&successor);
        app.work_once().unwrap();
        assert_eq!(result(&app, successor.id).outcome, Outcome::Failure);
        let resumed = retry(&app, successor.id, "resume", None);
        let resumed_job: Value = serde_json::from_str(&resumed.payload).unwrap();
        assert_eq!(resumed_job["role_epochs"]["developer"], epoch);
        assert!(resumed_job["continuation"].get("replacement").is_none());
        app.work_once().unwrap();
        let next = result(&app, resumed.id);
        assert_eq!(next.outcome, Outcome::Success, "{next:?}");
        assert_eq!(next.workspace.as_ref(), Some(&root));
        let journal = events(&root);
        assert_eq!(journal.len(), 3);
        assert_eq!(journal[1]["resume"], false);
        assert_eq!(journal[2]["resume"], true);
        assert_eq!(journal[1]["id"], journal[2]["id"]);
        assert_ne!(journal[0]["id"], journal[1]["id"]);
        assert_dirty(&root);
    }
}

#[test]
fn inherited_epoch_missing_record_or_id_fails_without_fresh_provider_run() {
    for missing_record in [false, true] {
        let fixture = Fixture::new("codex");
        let app = fixture.app();
        let (first, root) = fixture.initial(&app, "codex");
        let successor = retry(
            &app,
            first.id,
            "replace",
            Some(
                json!({"profile":"codex","model":{"value":"replacement-fail-once","source":"manual"}}),
            ),
        );
        let epoch = epoch(&successor);
        app.work_once().unwrap();
        assert_eq!(result(&app, successor.id).outcome, Outcome::Failure);
        let record_path = root.join(format!("sessions/developer-{epoch}.json"));
        if missing_record {
            fs::remove_file(&record_path).unwrap();
        } else {
            let mut record: Value =
                serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
            record["session_id"] = Value::Null;
            fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();
        }
        let resumed = retry(&app, successor.id, "resume-missing", None);
        app.work_once().unwrap();
        let next = result(&app, resumed.id);
        assert_eq!(next.outcome, Outcome::Failure, "{next:?}");
        assert!(next.agent.unwrap().error.unwrap().contains("bound session"));
        assert_eq!(events(&root).len(), 2);
        if missing_record {
            assert!(!record_path.exists());
        }
        assert_dirty(&root);
    }
}

#[test]
fn clients_cannot_submit_or_challenge_server_owned_role_epochs() {
    let fixture = Fixture::new("codex");
    let app = fixture.app();
    for epochs in [
        json!({}),
        json!({"developer":"00000000-0000-4000-8000-000000000000"}),
    ] {
        let job = json!({"repository":"source","requirements":"Do work","agent":"codex","role_epochs":epochs});
        let request =
            serde_json::from_value::<Submission>(json!({"key":"injected","job":job})).unwrap();
        assert!(app.submit(request).is_err());
        assert!(
            app.permission_challenge(serde_json::from_value(job).unwrap())
                .is_err()
        );
    }
    assert!(!fixture.config.workspace_root.join("task-1").exists());
}
