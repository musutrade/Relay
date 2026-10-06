#![cfg(target_os = "linux")]
use relay_app::{Application, RetryRequest, Submission, host::HostConfig};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

static FIXTURES: Mutex<()> = Mutex::new(());
const CLI: &str = r#"#!/usr/bin/python3
import json, os, pathlib, sys, time
trace=pathlib.Path(os.environ['TRACE'])
def record(value):
    with trace.open('a') as f: f.write(json.dumps(value)+'\n')
record({'argv':sys.argv[1:]})
mode=os.environ.get('MODE','success')
if '--version' in sys.argv: print('2.1.281 (Claude Code)' if mode=='old' else '2.1.291 (Claude Code)');sys.exit()
if '--help' in sys.argv:
    print('--output-format --verbose --permission-prompts --no-session-persistence --model --effort --session-id --resume --restricted --tools --allowedTools --disallowedTools --disable-slash-commands --strict-mcp-config --mcp-config'+('' if mode=='no_flag' else ' --input-format'));sys.exit()
assert '--bare' not in sys.argv and '--safe-mode' not in sys.argv
assert sys.argv[sys.argv.index('--permission-prompts')+1]=='none'
def send(value): print(json.dumps(value),flush=True,end='' if mode=='eof' and value.get('type')=='result' else '\n')
if mode in ('old','no_flag'):
    assert '--input-format' not in sys.argv
    prompt=sys.stdin.read();record({'text_prompt':prompt})
else:
    assert sys.argv[sys.argv.index('--input-format')+1]=='stream-json'
    init=json.loads(sys.stdin.readline());record(init)
    assert init['type']=='control_request' and init['request']=={'subtype':'initialize','hooks':None}
    if mode=='timeout': time.sleep(60)
    if mode=='request':
        send({'type':'control_request','request_id':'approval','request':{'subtype':'can_use_tool','input':{'secret':'NEVER_EXPOSE'}}});time.sleep(60)
    response={'type':'control_response','response':{'subtype':'success','request_id':init['request_id'],'pending_permission_requests':[],'pending_user_dialog_requests':[],'response':{'models':[{'value':'dynamic-model','resolvedModel':'dynamic-wire-id','displayName':'<img src=x>','description':'fixture','supportsEffort':True,'supportedEffortLevels':['future-effort'],'supportsAutoMode':False}], 'account':{'email':'NEVER_EXPOSE','token':'NEVER_EXPOSE'}}}}
    if mode.startswith('bad_type_'):
        response['type']={'bad_type_null':None,'bad_type_numeric':1}.get(mode)
        if mode=='bad_type_missing': del response['type']
    if mode=='missing_pending': del response['response']['pending_permission_requests']
    if mode=='error': response['response']={'subtype':'error','request_id':init['request_id'],'error':'NEVER_EXPOSE'}
    if mode=='malformed_models': response['response']['response']['models']='NEVER_EXPOSE'
    if mode=='premature_result':
        sys.stdout.write(json.dumps(response)+'\n'+json.dumps({'type':'result','subtype':'success','is_error':False,'result':'unrequested','permission_denials':[]})+'\n');sys.stdout.flush();sys.exit()
    if mode=='duplicate':
        sys.stdout.write((json.dumps(response)+'\n')*2);sys.stdout.flush();time.sleep(60)
    send(response)
    if mode=='error' or mode.startswith('bad_type_'): time.sleep(60)
    user=json.loads(sys.stdin.readline());record(user)
    assert user['type']=='user' and user['message']['role']=='user'
    prompt=user['message']['content']
    assert sys.stdin.read()=='' # Relay must close stdin after exactly one task prompt.
    if mode=='descendant':
        if os.fork()==0:
            os.setsid();pathlib.Path(os.environ['CHILD']).write_text(str(os.getpid()));time.sleep(60);sys.exit()
        limit=time.monotonic()+2
        while not pathlib.Path(os.environ['CHILD']).exists() and time.monotonic()<limit:time.sleep(.01)
    if mode=='drift':
        with pathlib.Path(__file__).open('a') as f:f.write('\n# drift\n')
assert prompt
sid=sys.argv[sys.argv.index('--resume')+1] if '--resume' in sys.argv else sys.argv[sys.argv.index('--session-id')+1] if '--session-id' in sys.argv else 'fixture-session'
send({'type':'system','subtype':'init','session_id':sid,'model':'actual-model'})
send({'type':'result','subtype':'success','is_error':False,'session_id':sid,'result':'done','permission_denials':[]})
"#;
struct Fixture {
    temp: tempfile::TempDir,
    app: Arc<Application>,
    _serial: MutexGuard<'static, ()>,
}
impl Fixture {
    fn new(mode: &str, sessions: bool) -> Self {
        let serial = FIXTURES.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("source")).unwrap();
        fs::write(temp.path().join("source/original.txt"), "original").unwrap();
        let program = temp.path().join("fake.py");
        fs::write(&program, CLI).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let config:HostConfig=serde_json::from_value(json!({"workspace_root":temp.path().join("runs"),"repositories":{"repo":temp.path().join("source")},"native_agents":{"claude":{"provider":"claude_cli","program":program,"model":"manual-model","session_continuity":sessions,"env":{"MODE":mode,"TRACE":temp.path().join("trace"),"CHILD":temp.path().join("child")}}},"tests":{"fail":{"program":"/bin/false"}},"supervisor_program":env!("CARGO_BIN_EXE_relay-app")})).unwrap();
        let app = Application::open(temp.path().join("db"), config).unwrap();
        Self {
            temp,
            app,
            _serial: serial,
        }
    }
    fn submit(&self, test: bool) -> i64 {
        let mut job = json!({"repository":"repo","agent":"claude","requirements":"one task\nwith quotes \" and UTF-8 ✓"});
        if test {
            job["test"] = json!("fail");
        }
        self.app
            .submit(serde_json::from_value::<Submission>(json!({"key":"first","job":job})).unwrap())
            .unwrap()
            .id
    }
    fn result(&self, id: i64) -> Value {
        serde_json::from_str(self.app.get(id).unwrap().result.as_deref().unwrap()).unwrap()
    }
    fn trace(&self) -> Vec<Value> {
        fs::read_to_string(self.temp.path().join("trace"))
            .unwrap()
            .lines()
            .map(|v| serde_json::from_str(v).unwrap())
            .collect()
    }
}
#[test]
fn task_observes_models_without_an_extra_launch_turn_or_secret_copy() {
    let f = Fixture::new("success", false);
    assert!(f.app.capabilities().unwrap()[0].task_observation.is_none());
    let id = f.submit(false);
    assert!(f.app.work_once().unwrap());
    let result = f.result(id);
    assert_eq!(result["outcome"], "success", "{result}");
    assert_eq!(
        result["agent"]["provider"]["selection"]["session_settings"]["model"],
        "actual-model"
    );
    assert!(!result.to_string().contains("NEVER_EXPOSE"));
    assert!(result["agent"].get("catalog").is_none());
    let views = f.app.capabilities().unwrap();
    let observation = views[0].task_observation.as_ref().unwrap();
    assert_eq!(observation.task_id, id);
    assert_eq!(observation.repository, "repo");
    assert_eq!(observation.role, "developer");
    assert_eq!(observation.models[0].model, "dynamic-model");
    assert_eq!(
        observation.models[0].resolved_model.as_deref(),
        Some("dynamic-wire-id")
    );
    assert_eq!(
        observation.models[0].supported_efforts.as_ref().unwrap()[0].effort,
        "future-effort"
    );
    assert_eq!(observation.requested_model.as_deref(), Some("manual-model"));
    assert!(!views[0].task_observation_stale);
    assert!(views[0].catalog.is_none());
    let trace = f.trace();
    assert_eq!(trace.iter().filter(|v| v.get("argv").is_some()).count(), 3);
    assert_eq!(trace.iter().filter(|v| v["type"] == "user").count(), 1);
    assert_eq!(
        trace
            .iter()
            .filter(|v| v["type"] == "control_request")
            .count(),
        1
    );
    let selected = json!({"key":"different-task","job":{"repository":"repo","agent":"claude","requirements":"next task","role_selections":{"developer":{"profile":"claude","model":{"value":"dynamic-model","source":"catalog","catalog":{"cache_epoch":views[0].cache_epoch,"generation":views[0].generation}}}}}});
    let error = f
        .app
        .submit(serde_json::from_value::<Submission>(selected.clone()).unwrap())
        .unwrap_err();
    assert!(error.to_string().contains("catalog"));
    let mut manual = selected;
    manual["job"]["role_selections"]["developer"]["model"] =
        json!({"value":"dynamic-model","source":"manual"});
    assert!(
        f.app
            .submit(serde_json::from_value::<Submission>(manual).unwrap())
            .is_ok()
    );
    let refreshed = f.app.refresh_capabilities("claude").unwrap();
    assert!(refreshed.catalog.unwrap().models.is_empty());
    assert!(refreshed.task_observation.is_some());
    assert_eq!(f.trace().iter().filter(|v| v["type"] == "user").count(), 1);
}
#[test]
fn older_or_unadvertised_control_stream_keeps_the_working_text_execution() {
    for mode in ["old", "no_flag"] {
        let f = Fixture::new(mode, false);
        let id = f.submit(false);
        f.app.work_once().unwrap();
        assert_eq!(f.result(id)["outcome"], "success");
        assert!(f.app.capabilities().unwrap()[0].task_observation.is_none());
        assert_eq!(
            f.trace()
                .iter()
                .filter(|v| v.get("text_prompt").is_some())
                .count(),
            1
        );
    }
}
#[test]
fn init_failures_never_send_a_prompt_or_retry() {
    for mode in [
        "error",
        "request",
        "duplicate",
        "missing_pending",
        "bad_type_missing",
        "bad_type_null",
        "bad_type_numeric",
        "premature_result",
    ] {
        let f = Fixture::new(mode, false);
        let id = f.submit(false);
        let start = Instant::now();
        f.app.work_once().unwrap();
        assert_eq!(f.result(id)["outcome"], "failure");
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(!f.result(id).to_string().contains("NEVER_EXPOSE"));
        assert_eq!(f.trace().iter().filter(|v| v["type"] == "user").count(), 0);
        assert_eq!(
            f.trace().iter().filter(|v| v.get("argv").is_some()).count(),
            3
        );
        assert!(f.app.capabilities().unwrap()[0].task_observation.is_none());
    }
}
#[test]
fn invalid_optional_models_do_not_break_the_task_and_binary_drift_discards_evidence() {
    for mode in ["malformed_models", "drift"] {
        let f = Fixture::new(mode, false);
        let id = f.submit(false);
        f.app.work_once().unwrap();
        assert_eq!(f.result(id)["outcome"], "success");
        assert!(f.app.capabilities().unwrap()[0].task_observation.is_none());
        assert!(!f.result(id).to_string().contains("NEVER_EXPOSE"));
    }
}
#[test]
fn timeout_before_prompt_is_bounded_and_reaps_the_process() {
    let f = Fixture::new("timeout", false);
    let id = f.submit(false);
    let start = Instant::now();
    f.app.work_once().unwrap();
    assert_eq!(f.result(id)["outcome"], "timed_out");
    assert!(start.elapsed() < Duration::from_secs(15));
    assert_eq!(f.trace().iter().filter(|v| v["type"] == "user").count(), 0);
}
#[test]
fn successful_stream_reaps_escaped_descendants_and_keeps_observation() {
    let f = Fixture::new("descendant", false);
    let id = f.submit(false);
    f.app.work_once().unwrap();
    assert_eq!(f.result(id)["outcome"], "success");
    assert!(f.app.capabilities().unwrap()[0].task_observation.is_some());
    let pid: i32 = fs::read_to_string(f.temp.path().join("child"))
        .unwrap()
        .parse()
        .unwrap();
    // SAFETY: signal 0 only observes our fixture; it does not signal a process.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}
#[test]
fn explicit_retry_preserves_session_and_sends_one_prompt_per_authorized_attempt() {
    let f = Fixture::new("success", true);
    let id = f.submit(true);
    f.app.work_once().unwrap();
    assert_eq!(f.result(id)["outcome"], "failure");
    let next = f
        .app
        .retry(
            id,
            serde_json::from_value::<RetryRequest>(
                json!({"key":"second","confirm_stopped_and_reconciled":true}),
            )
            .unwrap(),
        )
        .unwrap();
    f.app.work_once().unwrap();
    assert_eq!(f.result(next.id)["outcome"], "failure");
    let trace = f.trace();
    let args: Vec<_> = trace
        .iter()
        .filter_map(|v| v["argv"].as_array())
        .filter(|args| args.iter().any(|v| v == "-p"))
        .collect();
    assert_eq!(args.len(), 2);
    assert!(args[0].iter().any(|v| v == "--session-id"));
    assert!(args[1].iter().any(|v| v == "--resume"));
    let sid = args[0][args[0].iter().position(|v| v == "--session-id").unwrap() + 1].clone();
    assert_eq!(
        args[1][args[1].iter().position(|v| v == "--resume").unwrap() + 1],
        sid
    );
    assert_eq!(trace.iter().filter(|v| v["type"] == "user").count(), 2);
    assert_eq!(
        f.app.capabilities().unwrap()[0]
            .task_observation
            .as_ref()
            .unwrap()
            .task_id,
        next.id
    );
}

#[test]
fn accepted_final_event_without_newline_preserves_filtered_stdout() {
    let f = Fixture::new("eof", false);
    let id = f.submit(false);
    f.app.work_once().unwrap();
    let result = f.result(id);
    assert_eq!(result["outcome"], "success");
    assert!(result["agent"]["stdout"].as_str().unwrap().contains("done"));
    assert!(!result.to_string().contains("NEVER_EXPOSE"));
    assert!(f.app.capabilities().unwrap()[0].task_observation.is_some());
}
