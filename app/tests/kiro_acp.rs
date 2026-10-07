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
import fcntl, json, os, pathlib, sys, time
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
session={'sessionId':'kiro-fresh'}
if mode.startswith('tail_'):
    session['configOptions']=[{'id':'model','type':'select','currentValue':'session-model-before-completion'},{'id':'effortLevel','type':'select','currentValue':'high'}]
send({'jsonrpc':'2.0','id':new['id'],'result':session})
prompt=get()
assert prompt['method']=='session/prompt' and prompt['params']['sessionId']=='kiro-fresh'
assert prompt['params']['prompt'][0]['text']
pathlib.Path(os.environ['LEADER']).write_text(str(os.getpid()))
pathlib.Path('changed.txt').write_text('fresh development')
if mode in ('timeout','cancel','descendant') or mode.startswith('tail_'):
    if os.fork()==0:
        os.setsid(); pathlib.Path(os.environ['CHILD']).write_text(str(os.getpid())); time.sleep(60); sys.exit()
    deadline=time.monotonic()+2
    while not pathlib.Path(os.environ['CHILD']).exists() and time.monotonic()<deadline: time.sleep(.01)
    if mode in ('timeout','cancel'): time.sleep(60)
if mode=='permission':
    send({'jsonrpc':'2.0','id':'ask','method':'session/request_permission','params':{'sessionId':'kiro-fresh','toolCall':{'toolCallId':'1','title':'PRIVATE_APPROVAL_INPUT'},'options':[{'optionId':'deny','kind':'reject_once','name':'Reject'}]}});time.sleep(60)
if mode=='malformed': print('not json',flush=True);time.sleep(60)
if mode=='nonzero': sys.exit(2)
if mode=='missing': sys.exit()
if mode=='partial': sys.stdout.write('{"jsonrpc":"2.0"');sys.stdout.flush();sys.exit()
if mode.startswith('tail_'):
    # Protocol-derived, reconstructed fixtures, not captured real-session tails.
    # Keep the final answer, correlated completion and tail in one OS write.
    def update(value, session='kiro-fresh'):
        return {'jsonrpc':'2.0','method':'session/update','params':{'sessionId':session,'update':value}}
    completion={'jsonrpc':'2.0','id':prompt['id'],'result':{'stopReason':'end_turn'}}
    frames=[update({'sessionUpdate':'agent_message_chunk','messageId':'final-message','content':{'type':'text','text':'完成✓'}}),completion]
    metadata=[
        update({'sessionUpdate':'session_info_update','_meta':{'kiro':{'kind':'turn_completion','promptTurnSummaries':[{'usage':0.25,'unit':'credit'}]}}}),
        update({'sessionUpdate':'session_info_update','_meta':{'kiro':{'kind':'context_usage','usagePercentage':12.5}}}),
        update({'sessionUpdate':'usage_update','used':25000,'size':200000,'cost':{'amount':0.25,'currency':'credits'}}),
        update({'sessionUpdate':'config_option_update','configOptions':[{'id':'model','type':'select','currentValue':'different-model-after-completion'},{'id':'effortLevel','type':'select','currentValue':'low'}]}),
        update({'sessionUpdate':'future_metadata_extension','_meta':{'kiro':{'kind':'future_metering','credits':1.5}},'payload':{'synthetic':True}}),
        {'jsonrpc':'2.0','method':'_kiro/future/metadata','params':{'sessionId':'kiro-fresh','_meta':{'kiro':{'kind':'future_notice'}},'synthetic':True}},
    ]
    raw_tail=None
    if mode in ('tail_metadata_batch','tail_metadata_boundary','tail_metadata_exit'):
        frames+=metadata
    elif mode in ('tail_foreign','tail_foreign_exit'): frames.append(update({'sessionUpdate':'usage_update'},'other-synthetic-session'))
    elif mode=='tail_nonobject_params': frames.append({'jsonrpc':'2.0','method':'session/update','params':[]})
    elif mode=='tail_nonobject_update': frames.append(update([]))
    elif mode=='tail_missing_discriminator': frames.append(update({'_meta':{'kiro':{'kind':'turn_completion'}}}))
    elif mode=='tail_malformed_meta': frames.append(update({'sessionUpdate':'session_info_update','_meta':{'kiro':[]}}))
    elif mode=='tail_failure_meta': frames.append(update({'sessionUpdate':'session_info_update','_meta':{'kiro':{'failureReason':'PRIVATE_TAIL_FAILURE'}}}))
    elif mode=='tail_replay_meta': frames.append(update({'sessionUpdate':'session_info_update','_meta':{'kiro':{'replay':True}}}))
    elif mode=='tail_malformed_envelope': frames.append({'jsonrpc':'2.0','method':'session/update','result':{},'params':{'sessionId':'kiro-fresh','update':{'sessionUpdate':'usage_update'}}})
    elif mode=='tail_invalid_config': frames.append(update({'sessionUpdate':'config_option_update','configOptions':'invalid'}))
    elif mode=='tail_tool': frames.append(update({'sessionUpdate':'tool_call','toolCallId':'late-tool','status':'pending'}))
    elif mode=='tail_tool_update': frames.append(update({'sessionUpdate':'tool_call_update','toolCallId':'late-tool','status':'completed'}))
    elif mode in ('tail_message','tail_message_boundary','tail_thought','tail_user'):
        kind={'tail_message':'agent_message_chunk','tail_message_boundary':'agent_message_chunk','tail_thought':'agent_thought_chunk','tail_user':'user_message_chunk'}[mode]
        frames.append(update({'sessionUpdate':kind,'content':{'type':'text','text':'late activity'}}))
    elif mode=='tail_plan': frames.append(update({'sessionUpdate':'plan','entries':[]}))
    elif mode=='tail_permission': frames.append({'jsonrpc':'2.0','id':'late-permission','method':'session/request_permission','params':{'sessionId':'kiro-fresh','toolCall':{'toolCallId':'late-tool','title':'PRIVATE_APPROVAL_INPUT'},'options':[{'optionId':'deny','kind':'reject_once','name':'Reject'}]}})
    elif mode=='tail_duplicate': frames.append(completion)
    elif mode=='tail_malformed_json': raw_tail=b'not json\n'
    elif mode=='tail_invalid_utf8': raw_tail=b'{"jsonrpc":"2.0","method":"\xff"}\n'
    elif mode=='tail_partial': raw_tail=b'{"jsonrpc":"2.0","method":"session/update"'
    else: raise AssertionError('unknown tail fixture mode')
    across_boundary=mode.endswith('_boundary') or mode.endswith('_exit')
    encode=lambda frame:(json.dumps(frame,ensure_ascii=False)+'\n').encode()
    pipe_capacity=fcntl.fcntl(sys.stdout.fileno(),fcntl.F_GETPIPE_SZ)
    if across_boundary:
        filler=update({'sessionUpdate':'future_metadata_extension','padding':''})
        room=pipe_capacity-sum(map(lambda frame:len(encode(frame)),frames))-len(encode(filler))-len(raw_tail or b'')
        assert room>=0
        filler['params']['update']['padding']='x'*min(12288,room)
        frames.insert(2,filler)
    payload=b''.join(map(encode,frames))+(raw_tail or b'')
    # No earlier stdout remains once Relay has dispatched this prompt. Keep the
    # complete tail within that empty pipe, including on low-capacity Linux hosts,
    # so supervisor shutdown cannot interrupt a blocked partial tail write.
    assert len(payload)<=pipe_capacity
    if across_boundary: assert (len(payload)>8192)==(pipe_capacity>8192)
    else: assert len(payload)<4096
    record({'tail_bytes':len(payload),'pipe_capacity':pipe_capacity,'tail_fixture':'protocol-derived'})
    assert os.write(sys.stdout.fileno(),payload)==len(payload)
    if mode.endswith('_exit'): os._exit(0)
    time.sleep(60)
send({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'wrong-session' if mode=='foreign' else 'kiro-fresh','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'完成✓'}}}})
if mode=='failed': send({'jsonrpc':'2.0','id':prompt['id'],'error':{'code':-32000,'message':'PRIVATE_ERROR_DETAIL'}})
else: send({'jsonrpc':'2.0','id':prompt['id'],'result':{'stopReason':'cancelled' if mode=='interrupted' else 'end_turn'}})
time.sleep(60)
"#;
const CHECK: &str = r#"#!/usr/bin/python3
import json, os, pathlib
def reaped(name):
    path=pathlib.Path(os.environ[name])
    return not path.exists() or not pathlib.Path('/proc',path.read_text().strip()).exists()
leader_reaped=reaped('LEADER')
child_reaped=reaped('CHILD')
with pathlib.Path(os.environ['TEST_TRACE']).open('a') as trace:
    trace.write(json.dumps({'leader_reaped':leader_reaped,'child_reaped':child_reaped})+'\n')
assert leader_reaped and child_reaped, 'test stage began before ACP process-tree cleanup'
assert pathlib.Path('changed.txt').is_file()
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
        let check = temp.path().join("check.py");
        fs::write(&check, CHECK).unwrap();
        fs::set_permissions(&check, fs::Permissions::from_mode(0o700)).unwrap();
        let config:HostConfig=serde_json::from_value(json!({"workspace_root":temp.path().join("runs"),"repositories":{"repo":temp.path().join("source")},"native_agents":{"kiro":{"provider":"kiro_cli","program":program,"model":"future-model","env":{"MODE":mode,"TRACE":temp.path().join("trace"),"CHILD":temp.path().join("child"),"LEADER":temp.path().join("leader")}}},"timeout_seconds":2,"tests":{"check":{"program":check,"env":{"TEST_TRACE":temp.path().join("test-trace"),"CHILD":temp.path().join("child"),"LEADER":temp.path().join("leader")}}},"supervisor_program":env!("CARGO_BIN_EXE_relay-app")})).unwrap();
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
    fn assert_tests_once_after_cleanup(&self) {
        let trace = fs::read_to_string(self.temp.path().join("test-trace")).unwrap();
        let calls: Vec<Value> = trace
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            calls,
            vec![json!({"leader_reaped":true,"child_reaped":true})]
        );
    }
    fn assert_tests_not_run(&self) {
        assert!(!self.temp.path().join("test-trace").exists());
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
        f.assert_tests_not_run();
    }
}
#[test]
fn kiro_terminal_metadata_is_drained_without_changing_the_completed_result() {
    for mode in [
        "tail_metadata_batch",
        "tail_metadata_boundary",
        "tail_metadata_exit",
    ] {
        let f = Fixture::new(mode);
        let id = f.submit();
        assert!(f.app.work_once().unwrap());
        let result = f.result(id);
        assert_eq!(result["outcome"], "success", "{mode}: {result}");
        assert_eq!(result["tests"]["outcome"], "success", "{mode}: {result}");
        let provider = &result["agent"]["provider"];
        assert_eq!(provider["summary"], "完成✓", "{mode}: {result}");
        assert_eq!(provider["summary_truncated"], false);
        assert_eq!(provider["session_id"], "kiro-fresh");
        assert_eq!(provider["terminal_reason"], "end_turn");
        assert_eq!(provider["requested_model"], "future-model");
        assert!(provider["reported_model"].is_null());
        assert!(provider["selection"]["observed"]["model"].is_null());
        assert_eq!(
            provider["selection"]["verification"]["model"],
            "session_reported"
        );
        let settings = &provider["selection"]["session_settings"];
        assert_eq!(settings["model"], "session-model-before-completion");
        assert_eq!(settings["effort"], "high");
        assert_eq!(settings["source"], "kiro.session/new.configOptions");
        assert!(
            provider["usage"]
                .as_object()
                .unwrap()
                .values()
                .all(Value::is_null),
            "context occupancy and credit metadata must not become token or USD usage: {result}"
        );
        assert_eq!(result["agent"]["stdout"], "");
        assert!(!result.to_string().contains("PRIVATE_"));
        assert!(
            !result
                .to_string()
                .contains("different-model-after-completion")
        );
        let trace = f.trace();
        let tail = trace
            .iter()
            .find(|entry| entry.get("tail_bytes").is_some())
            .unwrap();
        assert_eq!(tail["tail_fixture"], "protocol-derived");
        assert_eq!(
            tail["tail_bytes"].as_u64().unwrap() > 8192,
            mode != "tail_metadata_batch" && tail["pipe_capacity"].as_u64().unwrap() > 8192
        );
        for method in ["initialize", "session/new", "session/prompt"] {
            assert_eq!(
                trace
                    .iter()
                    .filter(|entry| entry["method"] == method)
                    .count(),
                1
            );
        }
        // Natural exit and verified supervisor shutdown may race; both are
        // valid once the correlated turn and complete tail were drained.
        f.assert_child_reaped();
        assert!(f.app.status().unwrap()["active"].is_null());
        assert!(!f.app.work_once().unwrap());
        f.assert_tests_once_after_cleanup();
    }
}
#[test]
fn kiro_invalid_or_active_terminal_tails_never_start_tests() {
    for mode in [
        "tail_foreign",
        "tail_foreign_exit",
        "tail_nonobject_params",
        "tail_nonobject_update",
        "tail_missing_discriminator",
        "tail_malformed_meta",
        "tail_failure_meta",
        "tail_replay_meta",
        "tail_malformed_envelope",
        "tail_invalid_config",
        "tail_tool",
        "tail_tool_update",
        "tail_message",
        "tail_message_boundary",
        "tail_thought",
        "tail_user",
        "tail_plan",
        "tail_permission",
        "tail_duplicate",
        "tail_malformed_json",
        "tail_invalid_utf8",
        "tail_partial",
    ] {
        let f = Fixture::new(mode);
        let id = f.submit();
        assert!(f.app.work_once().unwrap());
        let result = f.result(id);
        assert_eq!(result["outcome"], "failure", "{mode}: {result}");
        assert_eq!(result["agent"]["outcome"], "failure", "{mode}: {result}");
        assert_eq!(result["agent"]["provider"]["summary"], "完成✓");
        assert_eq!(result["agent"]["provider"]["terminal_reason"], "end_turn");
        assert!(result["tests"].is_null(), "{mode}: {result}");
        assert!(!result.to_string().contains("PRIVATE_"), "{mode}: {result}");
        f.assert_child_reaped();
        f.assert_tests_not_run();
        assert!(f.app.status().unwrap()["active"].is_null());
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
