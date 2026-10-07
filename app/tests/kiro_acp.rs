#![cfg(target_os = "linux")]
use relay_app::{Application, Submission, host::HostConfig};
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
trace = pathlib.Path(os.environ['TRACE'])
mode = os.environ.get('MODE', 'success')
def record(value):
    with trace.open('a') as f: f.write(json.dumps(value)+'\n')
record({'argv':sys.argv[1:]})
if sys.argv[1:] == ['--version']: print('kiro-cli 2.27.0' if mode=='old' else 'kiro-cli 2.28.0'); sys.exit()
if sys.argv[1:] == ['acp','--help']: print('--agent-engine --auth-method'); sys.exit()
if sys.argv[1:] == ['whoami']:
    print('PRIVATE_IDENTITY_DO_NOT_LOG')
    if mode in ('signed_out','auth_truncated'):
        print('PRIVATE_STDERR_DO_NOT_LOG' * (100 if mode=='auth_truncated' else 1), file=sys.stderr)
        sys.exit(1)
    sys.exit(0)
assert sys.argv[1:] == ['acp','--agent-engine','v3','--auth-method','cli']
def get():
    value=json.loads(sys.stdin.readline()); record(value); return value
def send(value): print(json.dumps(value), flush=True)
init=get()
assert init['method']=='initialize' and init['params']['protocolVersion']==1
assert not init['params']['clientCapabilities'].get('terminal',False)
send({'jsonrpc':'2.0','id':init['id'],'result':{'protocolVersion':1,'agentCapabilities':{'loadSession':True},'authMethods':[{'id':'PRIVATE_AUTH_METADATA'}]}})
new=get()
assert new['method']=='session/new' and new['params']['mcpServers']==[]
assert pathlib.Path(new['params']['cwd']).resolve()==pathlib.Path.cwd()
assert new['params']['_meta']['kiro']['modelId']=='future-model'
assert 'policyPreset' not in new['params']['_meta']['kiro']
send({'jsonrpc':'2.0','id':new['id'],'result':{'sessionId':'kiro-fresh'}})
prompt=get()
assert prompt['method']=='session/prompt' and prompt['params']['sessionId']=='kiro-fresh'
assert prompt['params']['prompt'][0]['text']
pathlib.Path('changed.txt').write_text('fresh development')
if mode in ('timeout','cancel','descendant'):
    if os.fork()==0:
        os.setsid(); pathlib.Path(os.environ['CHILD']).write_text(str(os.getpid())); time.sleep(60); sys.exit()
    deadline=time.monotonic()+2
    while not pathlib.Path(os.environ['CHILD']).exists() and time.monotonic()<deadline: time.sleep(.01)
    if mode!='descendant': time.sleep(60)
if mode=='permission':
    send({'jsonrpc':'2.0','id':'ask','method':'session/request_permission','params':{'sessionId':'kiro-fresh','toolCall':{'toolCallId':'1','title':'PRIVATE_APPROVAL_INPUT'},'options':[{'optionId':'deny','kind':'reject_once','name':'Reject'}]}});time.sleep(60)
if mode=='malformed': print('not json',flush=True);time.sleep(60)
if mode=='nonzero': sys.exit(2)
if mode=='missing': sys.exit()
if mode=='partial': sys.stdout.write('{"jsonrpc":"2.0"');sys.stdout.flush();sys.exit()
send({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'wrong-session' if mode=='foreign' else 'kiro-fresh','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'完成✓'}}}})
if mode=='failed': send({'jsonrpc':'2.0','id':prompt['id'],'error':{'code':-32000,'message':'PRIVATE_ERROR_DETAIL'}})
else: send({'jsonrpc':'2.0','id':prompt['id'],'result':{'stopReason':'cancelled' if mode=='interrupted' else 'end_turn'}})
time.sleep(60)
"#;
struct Fixture {
    temp: tempfile::TempDir,
    app: Arc<Application>,
    _serial: MutexGuard<'static, ()>,
}
impl Fixture {
    fn new(mode: &str) -> Self {
        let serial = FIXTURES.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("source")).unwrap();
        fs::write(temp.path().join("source/original.txt"), "original").unwrap();
        let program = temp.path().join("kiro.py");
        fs::write(&program, CLI).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let config:HostConfig=serde_json::from_value(json!({"workspace_root":temp.path().join("runs"),"repositories":{"repo":temp.path().join("source")},"native_agents":{"kiro":{"provider":"kiro_cli","program":program,"model":"future-model","env":{"MODE":mode,"TRACE":temp.path().join("trace"),"CHILD":temp.path().join("child")}}},"timeout_seconds":2,"tests":{"check":{"program":"/bin/sh","args":["-c","test -f changed.txt"]}},"supervisor_program":env!("CARGO_BIN_EXE_relay-app")})).unwrap();
        let app = Application::open(temp.path().join("db"), config).unwrap();
        Self {
            temp,
            app,
            _serial: serial,
        }
    }
    fn submit(&self) -> i64 {
        self.app.submit(serde_json::from_value::<Submission>(json!({"key":"first","job":{"repository":"repo","agent":"kiro","requirements":"one task\nwith quotes \" and UTF-8 ✓","test":"check"}})).unwrap()).unwrap().id
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
    fn assert_child_reaped(&self) {
        let pid = fs::read_to_string(self.temp.path().join("child")).unwrap();
        assert!(!std::path::Path::new(&format!("/proc/{}", pid.trim())).exists());
    }
}
#[test]
fn kiro_fresh_task_uses_correlated_acp_without_auth_or_config_leaks() {
    let f = Fixture::new("success");
    let id = f.submit();
    f.app.work_once().unwrap();
    let result = f.result(id);
    assert_eq!(result["outcome"], "success", "{result}");
    assert_eq!(result["agent"]["provider"]["summary"], "完成✓");
    assert_eq!(result["agent"]["provider"]["provider"], "kiro_cli");
    assert_eq!(result["agent"]["provider"]["session_id"], "kiro-fresh");
    assert!(result["agent"]["provider"]["reported_model"].is_null());
    assert!(result["agent"]["provider"]["usage"]["input_tokens"].is_null());
    assert!(!result.to_string().contains("PRIVATE_"));
    assert_eq!(result["agent"]["stdout"], "");
    assert!(!f.temp.path().join("source/changed.txt").exists());
    let trace = f.trace();
    assert_eq!(trace.iter().filter(|v| v.get("argv").is_some()).count(), 4);
    assert_eq!(
        trace
            .iter()
            .filter(|v| v["method"] == "session/new")
            .count(),
        1
    );
    assert_eq!(
        trace
            .iter()
            .filter(|v| v["method"] == "session/prompt")
            .count(),
        1
    );
    assert!(
        !trace
            .iter()
            .any(|v| v["method"] == "session/load" || v["method"] == "authenticate")
    );
}
#[test]
fn kiro_preflight_never_starts_login_or_an_unqualified_acp_session() {
    for mode in ["signed_out", "auth_truncated", "old"] {
        let f = Fixture::new(mode);
        let id = f.submit();
        f.app.work_once().unwrap();
        let result = f.result(id);
        assert_eq!(result["outcome"], "failure", "{mode}: {result}");
        assert!(!result.to_string().contains("PRIVATE_"));
        assert!(!f.trace().iter().any(|v| v["method"] == "initialize"));
    }
}
#[test]
fn kiro_protocol_failure_never_passes_tests() {
    for mode in [
        "permission",
        "malformed",
        "missing",
        "partial",
        "foreign",
        "failed",
        "nonzero",
        "interrupted",
    ] {
        let f = Fixture::new(mode);
        let id = f.submit();
        f.app.work_once().unwrap();
        let result = f.result(id);
        assert_eq!(result["outcome"], "failure", "{mode}: {result}");
        assert!(result["tests"].is_null(), "{mode}: {result}");
        assert!(!result.to_string().contains("PRIVATE_"), "{mode}: {result}");
    }
}
#[test]
fn kiro_discovery_runs_only_version_help_and_keeps_catalog_unknown() {
    let f = Fixture::new("success");
    let view = f.app.refresh_capabilities("kiro").unwrap();
    let catalog = view.catalog.unwrap();
    assert!(catalog.models.is_empty());
    assert_eq!(
        serde_json::to_value(catalog.model_catalog).unwrap()["state"],
        "unknown"
    );
    assert_eq!(f.trace().len(), 2);
    assert!(!f.trace().iter().any(|v| v["argv"] == json!(["whoami"])));
}
#[test]
fn kiro_timeout_and_success_reap_native_descendants() {
    for mode in ["timeout", "descendant"] {
        let f = Fixture::new(mode);
        let id = f.submit();
        f.app.work_once().unwrap();
        let result = f.result(id);
        assert_eq!(
            result["outcome"],
            if mode == "timeout" {
                "timed_out"
            } else {
                "success"
            },
            "{result}"
        );
        f.assert_child_reaped();
    }
}
#[test]
fn kiro_cancel_reaps_native_descendants_before_releasing_claim() {
    let f = Fixture::new("cancel");
    let id = f.submit();
    let app = f.app.clone();
    let worker = std::thread::spawn(move || app.work_once());
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f.temp.path().join("child").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    f.app.cancel(id).unwrap();
    worker.join().unwrap().unwrap();
    assert_eq!(f.result(id)["outcome"], "cancelled");
    f.assert_child_reaped();
    assert!(f.app.status().unwrap()["active"].is_null());
}
