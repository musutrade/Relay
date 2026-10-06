#![cfg(target_os = "linux")]
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use relay_app::{
    Application, CatalogRefreshRequest,
    capabilities::{CapabilityState, ProfileCatalog, discover},
    host::{Host, HostConfig},
    http,
};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tower::ServiceExt;

// Serializing fixtures avoids concurrent supervisor forks racing CLOEXEC setup.
static FIXTURES: Mutex<()> = Mutex::new(());
const TOKEN: &str = "startup-discovery-fixture-token-123456789";
const SOURCE: &str = "claude_cli:initialize/models";
const CLI: &str = r#"#!/usr/bin/python3
import json, os, pathlib, sys, time
trace=pathlib.Path(os.environ['TRACE'])
def record(value):
    with trace.open('a') as f: f.write(json.dumps(value)+'\n')
keys=['MODE','TRACE','CHILD','HOME','CLAUDE_CONFIG_DIR','ANTHROPIC_AUTH_TOKEN','CLAUDE_CODE_USE_BEDROCK','PRIVATE']
record({'argv':sys.argv[1:],'env':{k:os.environ.get(k) for k in keys},'cwd':os.getcwd(),'cwd_entries':sorted(os.listdir('.')),'pid':os.getpid()})
mode=os.environ['MODE']
if '--version' in sys.argv:
    print('2.1.281 (Claude Code)' if mode=='old' else '2.1.291 (Claude Code)');sys.exit()
if '--help' in sys.argv:
    print('--output-format --verbose --permission-prompts --no-session-persistence --model --effort --session-id --resume --permission-mode --max-turns --max-budget-usd --restricted --tools --allowedTools --disallowedTools --disable-slash-commands --strict-mcp-config --mcp-config'+('' if mode=='no_input_flag' else ' --input-format'));sys.exit()
assert '--bare' not in sys.argv and '--safe-mode' not in sys.argv
assert '--session-id' not in sys.argv and '--resume' not in sys.argv
assert '--no-session-persistence' in sys.argv
assert sys.argv[sys.argv.index('--input-format')+1]=='stream-json'
assert sys.argv[sys.argv.index('--permission-prompts')+1]=='none'
init=json.loads(sys.stdin.readline());record({'input':init})
assert init['type']=='control_request' and init['request']=={'subtype':'initialize','hooks':None}
print('NEVER_EXPOSE_STDERR',file=sys.stderr,flush=True)
def send(value): print(json.dumps(value),flush=True)
def read_remaining():
    for line in sys.stdin:
        record({'input':json.loads(line)})
    record({'stdin_eof':True})
    time.sleep(60)
if mode in ('descendant','timeout_child'):
    if os.fork()==0:
        os.setsid();pathlib.Path(os.environ['CHILD']).write_text(str(os.getpid()));time.sleep(60);os._exit(0)
    limit=time.monotonic()+2
    while not pathlib.Path(os.environ['CHILD']).exists() and time.monotonic()<limit:time.sleep(.01)
if mode=='timeout_child': time.sleep(60)
if mode=='request':
    send({'type':'control_request','request_id':'approval','request':{'subtype':'can_use_tool','input':{'secret':'NEVER_EXPOSE_ACCOUNT'}}})
    read_remaining();sys.exit()
if mode=='malformed': print('NEVER_EXPOSE_MALFORMED',flush=True);read_remaining();sys.exit()
if mode=='line_budget': print('x'*65537,flush=True);read_remaining();sys.exit()
if mode=='message_budget':
    for _ in range(1025): send({'type':'system','subtype':'status'})
if mode=='byte_budget':
    for _ in range(18): send({'type':'system','subtype':'status','opaque':'x'*60000})
row={'value':'dynamic-model','displayName':'<img src=x onerror=alert(1)>','description':'Advertised fixture model','resolvedModel':'wire-model','supportsEffort':True,'supportedEffortLevels':['high','future-effort'],'supportsAdaptiveThinking':True,'supportsFastMode':False,'supportsAutoMode':False}
models=[row,{'value':'minimal-model','displayName':'Minimal','description':''}]
response={'type':'control_response','response':{'subtype':'success','request_id':init['request_id'],'pending_permission_requests':[],'pending_user_dialog_requests':[],'response':{'models':models,'account':{'email':'NEVER_EXPOSE_ACCOUNT','token':'NEVER_EXPOSE_ACCOUNT'}}}}
envelope=response['response']
if mode=='empty': envelope['response']['models']=[]
if mode=='error': envelope.update(subtype='error',error='NEVER_EXPOSE_ACCOUNT')
if mode=='wrong_id': envelope['request_id']='other-initialize'
if mode=='missing_pending': del envelope['pending_permission_requests']
if mode=='pending_permission': envelope['pending_permission_requests']=[{'request_id':'approval','secret':'NEVER_EXPOSE_ACCOUNT'}]
if mode=='pending_dialog': envelope['pending_user_dialog_requests']=[{'request_id':'dialog','secret':'NEVER_EXPOSE_ACCOUNT'}]
if mode=='requires_action': envelope['response']['session_state']='requires_action'
if mode=='malformed_models': envelope['response']['models']='NEVER_EXPOSE_ACCOUNT'
if mode=='partial_models': models.append({'value':'invalid-row'})
if mode=='model_budget': envelope['response']['models']=[{'value':'m'+str(i),'displayName':'M','description':''} for i in range(257)]
if mode=='task_frame': send({'type':'assistant','message':{'role':'assistant','content':'NEVER_EXPOSE_ACCOUNT'}})
if mode=='partial': sys.stdout.write(json.dumps(response)[:90]);sys.stdout.flush();sys.exit()
if mode=='duplicate': sys.stdout.write((json.dumps(response)+'\n')*2);sys.stdout.flush()
else:
    send({'type':'system','subtype':'init','model':'not-effective-evidence','account':{'token':'NEVER_EXPOSE_ACCOUNT'}})
    send(response)
read_remaining()
"#;

struct Fixture {
    temp: tempfile::TempDir,
    config: HostConfig,
    app: Arc<Application>,
    _serial: MutexGuard<'static, ()>,
}
impl Fixture {
    fn new(mode: &str, enabled: bool) -> Self {
        let serial = FIXTURES.lock().unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().unwrap();
        for dir in ["source", "home", "native-settings"] {
            fs::create_dir(temp.path().join(dir)).unwrap();
        }
        fs::write(
            temp.path().join("source/original.txt"),
            "source is untouched",
        )
        .unwrap();
        fs::write(temp.path().join("native-settings/settings.json"), "{}\n").unwrap();
        let program = temp.path().join("fake.py");
        fs::write(&program, CLI).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let profile = json!({
            "provider":"claude_cli", "program":program,
            "model":"dynamic-model", "effort":"high", "native_permission":"claude_dont_ask",
            "max_turns":7, "max_budget_usd":3.5, "session_continuity":true,
            "allow_startup_discovery":enabled,
            "env":{
                "MODE":mode, "TRACE":temp.path().join("trace"), "CHILD":temp.path().join("child"),
                "HOME":temp.path().join("home"), "CLAUDE_CONFIG_DIR":temp.path().join("native-settings"),
                "ANTHROPIC_AUTH_TOKEN":"fixture-subscription-token", "CLAUDE_CODE_USE_BEDROCK":"0",
                "PRIVATE":"NEVER_EXPOSE_ENV"
            }
        });
        let config: HostConfig = serde_json::from_value(json!({
            "workspace_root":temp.path().join("runs"),
            "repositories":{"repo":temp.path().join("source")},
            "native_agents":{"claude":profile.clone(),"other":profile},
            "tests":{"pass":{"program":"/bin/true"}},
            "supervisor_program":env!("CARGO_BIN_EXE_relay-app")
        }))
        .unwrap();
        let app = Application::open(temp.path().join("db"), config.clone()).unwrap();
        Self {
            temp,
            config,
            app,
            _serial: serial,
        }
    }
    fn view(&self) -> Value {
        serde_json::to_value(
            self.app
                .capabilities()
                .unwrap()
                .into_iter()
                .find(|view| view.name == "claude")
                .unwrap(),
        )
        .unwrap()
    }
    fn approval(&self) -> CatalogRefreshRequest {
        CatalogRefreshRequest {
            confirm_startup_effects: true,
            confirmation_token: self.view()["startup_discovery"]["confirmation_token"]
                .as_str()
                .map(str::to_owned),
        }
    }
    fn refresh(&self) -> ProfileCatalog {
        self.app
            .refresh_capabilities_confirmed("claude", &self.approval())
            .unwrap()
            .catalog
            .unwrap()
    }
    fn trace(&self) -> Vec<Value> {
        fs::read_to_string(self.temp.path().join("trace"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn calls(&self) -> Vec<Value> {
        self.trace()
            .into_iter()
            .filter(|entry| entry.get("argv").is_some())
            .collect()
    }
    fn assert_initialize_only(&self) {
        let input: Vec<_> = self
            .trace()
            .into_iter()
            .filter_map(|entry| entry.get("input").cloned())
            .collect();
        assert_eq!(
            input
                .iter()
                .filter(|entry| entry["type"] == "control_request")
                .count(),
            1,
            "{input:?}"
        );
        for entry in input {
            if entry["type"] == "control_response" {
                assert_eq!(entry["response"]["subtype"], "error");
                assert!(!entry.to_string().contains("NEVER_EXPOSE"));
            } else {
                assert_eq!(entry["type"], "control_request");
                assert_eq!(
                    entry["request"],
                    json!({"subtype":"initialize","hooks":null})
                );
            }
        }
        assert!(self.app.list(None).unwrap().is_empty());
    }
    fn assert_reaped(&self, pid: i32) {
        // SAFETY: signal 0 only checks this fixture's PID; no signal is sent.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}
fn assert_private(catalog: &impl serde::Serialize) {
    let serialized = serde_json::to_string(catalog).unwrap();
    for secret in [
        "NEVER_EXPOSE",
        "fixture-subscription-token",
        "ANTHROPIC_AUTH_TOKEN",
    ] {
        assert!(
            !serialized.contains(secret),
            "public catalog leaked {secret}"
        );
    }
}

#[test]
fn confirmed_startup_preserves_native_profile_and_returns_only_advertised_metadata() {
    let f = Fixture::new("success", true);
    let approval = f.approval();
    let initial = f.view();
    let startup = &initial["startup_discovery"];
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let expiry = startup["expires_at_unix_ms"].as_u64().unwrap();
    assert!(expiry > now && expiry <= now + 90_000);
    for risk in ["Claude", "策略", "认证", "网络", "费用", "initialize"] {
        assert!(
            startup["confirmation_text"]
                .as_str()
                .unwrap()
                .contains(risk)
        );
    }
    assert!(f.calls().is_empty());
    let view = f
        .app
        .refresh_capabilities_confirmed("claude", &approval)
        .unwrap();
    let c = view.catalog.as_ref().unwrap();
    assert_eq!(c.model_catalog.state, CapabilityState::Supported);
    assert_eq!(c.model_catalog.source, SOURCE);
    assert_eq!(c.models.len(), 2);
    assert_eq!(c.models[0].model, "dynamic-model");
    assert_eq!(c.models[0].display_name, "<img src=x onerror=alert(1)>");
    assert_eq!(c.models[0].resolved_model.as_deref(), Some("wire-model"));
    assert_eq!(c.models[0].supports_effort, Some(true));
    assert_eq!(c.models[0].supports_adaptive_thinking, Some(true));
    assert_eq!(c.models[0].supports_fast_mode, Some(false));
    assert_eq!(c.models[0].supports_auto_mode, Some(false));
    assert_eq!(
        c.models[0].supported_efforts.as_ref().unwrap()[1].effort,
        "future-effort"
    );
    assert_eq!(c.models[1].supports_effort, None);
    assert!(c.models[1].supported_efforts.is_none());
    assert!(c.models.iter().all(|model| model.source == SOURCE));
    assert_eq!(c.authentication.state, CapabilityState::Unknown);
    assert_eq!(c.permission_control.state, CapabilityState::Unknown);
    assert_eq!(c.session_continuity.state, CapabilityState::Unknown);
    assert_eq!(c.startup_context.state, CapabilityState::Supported);
    assert_eq!(c.process_cleanup.state, CapabilityState::Supported);
    assert!(c.selection.effective_model.is_none() && c.selection.effective_effort.is_none());
    assert!(view.task_observation.is_none());
    assert_private(&view);
    let calls = f.calls();
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert_eq!(calls[0]["argv"], json!(["--version"]));
    assert_eq!(calls[1]["argv"], json!(["--help"]));
    let mut ephemeral = f.config.native_agents["claude"].clone();
    ephemeral.session_continuity = false;
    let mut expected = ephemeral.compile(false).unwrap();
    expected
        .args
        .extend(["--input-format".into(), "stream-json".into()]);
    assert_eq!(calls[2]["argv"], json!(expected.args));
    assert_eq!(calls[2]["env"], json!(expected.env));
    assert_eq!(calls[2]["cwd_entries"], json!([]));
    let cwd = std::path::Path::new(calls[2]["cwd"].as_str().unwrap());
    assert!(cwd.starts_with(&f.config.workspace_root));
    assert_ne!(cwd, f.config.repositories["repo"]);
    assert!(!cwd.exists(), "private discovery workspace must be cleaned");
    f.assert_reaped(calls[2]["pid"].as_i64().unwrap() as i32);
    f.assert_initialize_only();
    assert_eq!(
        fs::read_to_string(f.temp.path().join("source/original.txt")).unwrap(),
        "source is untouched"
    );
    assert_eq!(
        fs::read_to_string(f.temp.path().join("native-settings/settings.json")).unwrap(),
        "{}\n"
    );
    for _ in 0..3 {
        let cached = f.view();
        assert_eq!(cached["generation"], view.generation);
        assert_eq!(cached["cache_epoch"], view.cache_epoch);
        assert_eq!(cached["catalog"]["model_catalog"]["source"], SOURCE);
        assert_eq!(cached["stale"], false);
    }
    assert_eq!(f.calls().len(), 3);
}

#[test]
fn restricted_startup_uses_the_same_existing_reviewer_policy() {
    let mut f = Fixture::new("success", true);
    let profile = f.config.native_agents.get_mut("claude").unwrap();
    profile.native_permission = Some(relay_app::providers::NativePermission::ClaudeRestricted);
    let mut ephemeral = profile.clone();
    ephemeral.session_continuity = false;
    let mut compiled = ephemeral.compile(true).unwrap();
    compiled
        .args
        .extend(["--input-format".into(), "stream-json".into()]);
    f.app = Application::open(f.temp.path().join("db"), f.config.clone()).unwrap();
    let catalog = f.refresh();
    assert_eq!(catalog.model_catalog.state, CapabilityState::Supported);
    assert_eq!(f.calls()[2]["argv"], json!(compiled.args));
    assert_eq!(f.calls()[2]["env"], json!(compiled.env));
    f.assert_initialize_only();
}

#[test]
fn missing_wrong_cross_profile_and_replayed_confirmation_never_launch() {
    let f = Fixture::new("success", true);
    let approval = f.approval();
    assert!(f.app.refresh_capabilities("claude").is_err());
    for request in [
        CatalogRefreshRequest {
            confirm_startup_effects: true,
            confirmation_token: None,
        },
        CatalogRefreshRequest {
            confirm_startup_effects: false,
            confirmation_token: approval.confirmation_token.clone(),
        },
        CatalogRefreshRequest {
            confirm_startup_effects: true,
            confirmation_token: Some("wrong-token".into()),
        },
    ] {
        assert!(
            f.app
                .refresh_capabilities_confirmed("claude", &request)
                .is_err()
        );
    }
    assert!(
        f.app
            .refresh_capabilities_confirmed("other", &approval)
            .is_err()
    );
    assert!(
        f.app
            .refresh_capabilities_confirmed("missing", &approval)
            .is_err()
    );
    assert!(f.calls().is_empty());
    assert!(
        f.app
            .refresh_capabilities_confirmed("claude", &approval)
            .unwrap()
            .catalog
            .is_some()
    );
    assert!(
        f.app
            .refresh_capabilities_confirmed("claude", &approval)
            .is_err()
    );
    assert_eq!(f.calls().len(), 3);
    f.assert_initialize_only();
}

#[test]
fn executable_drift_and_previous_application_epoch_reject_before_probing() {
    let mut f = Fixture::new("success", true);
    let old = f.approval();
    let old_epoch = f.view()["cache_epoch"].clone();
    f.app = Application::open(f.temp.path().join("db"), f.config.clone()).unwrap();
    assert_ne!(f.view()["cache_epoch"], old_epoch);
    assert!(
        f.app
            .refresh_capabilities_confirmed("claude", &old)
            .is_err()
    );
    let approval = f.approval();
    let program = &f.config.native_agents["claude"].program;
    fs::write(program, format!("{CLI}\n# executable drift\n")).unwrap();
    assert!(
        f.app
            .refresh_capabilities_confirmed("claude", &approval)
            .is_err()
    );
    assert!(f.calls().is_empty());
    assert!(f.view()["catalog"].is_null());
}

#[test]
fn disabled_and_public_discovery_remain_version_help_only() {
    for enabled in [false, true] {
        let f = Fixture::new("success", enabled);
        let catalog = if enabled {
            let host = Host::new(f.config.clone()).unwrap();
            discover(&host, &f.config.native_agents["claude"])
        } else {
            assert!(f.view()["startup_discovery"].is_null());
            f.app
                .refresh_capabilities("claude")
                .unwrap()
                .catalog
                .unwrap()
        };
        assert_eq!(catalog.model_catalog.state, CapabilityState::Unknown);
        assert!(catalog.models.is_empty());
        let args: Vec<_> = f
            .calls()
            .into_iter()
            .map(|call| call["argv"].clone())
            .collect();
        assert_eq!(args, vec![json!(["--version"]), json!(["--help"])]);
        assert!(f.trace().iter().all(|entry| entry.get("input").is_none()));
    }
}

#[test]
fn old_or_unadvertised_control_protocol_never_starts_initialization() {
    for mode in ["old", "no_input_flag"] {
        let f = Fixture::new(mode, true);
        let c = f.refresh();
        assert_eq!(c.model_catalog.state, CapabilityState::Unknown, "{mode}");
        assert!(c.models.is_empty());
        assert_eq!(c.authentication.state, CapabilityState::Unknown);
        assert_eq!(f.calls().len(), 2);
        assert!(f.trace().iter().all(|entry| entry.get("input").is_none()));
    }
}

#[test]
fn empty_complete_metadata_is_valid_without_invented_models() {
    let f = Fixture::new("empty", true);
    let c = f.refresh();
    assert_eq!(c.model_catalog.state, CapabilityState::Supported);
    assert!(c.models.is_empty());
    assert_eq!(c.selection.status.state, CapabilityState::Unknown);
    f.assert_initialize_only();
}

#[test]
fn rejected_incomplete_and_oversized_protocols_discard_all_metadata() {
    for mode in [
        "error",
        "wrong_id",
        "request",
        "missing_pending",
        "pending_permission",
        "pending_dialog",
        "requires_action",
        "malformed",
        "malformed_models",
        "partial_models",
        "partial",
        "duplicate",
        "task_frame",
        "line_budget",
        "message_budget",
        "byte_budget",
        "model_budget",
    ] {
        let f = Fixture::new(mode, true);
        let start = Instant::now();
        let c = f.refresh();
        assert_eq!(
            c.model_catalog.state,
            CapabilityState::Unknown,
            "{mode}: {}",
            serde_json::to_string(&c).unwrap()
        );
        assert!(c.models.is_empty(), "{mode}");
        assert_eq!(c.authentication.state, CapabilityState::Unknown, "{mode}");
        assert_eq!(
            c.process_cleanup.state,
            CapabilityState::Supported,
            "{mode}"
        );
        assert!(start.elapsed() < Duration::from_secs(5), "{mode}");
        assert_eq!(f.calls().len(), 3, "{mode}");
        assert_private(&c);
        f.assert_initialize_only();
        f.assert_reaped(f.calls()[2]["pid"].as_i64().unwrap() as i32);
    }
}

#[test]
fn metadata_success_and_timeout_both_reap_escaped_descendants() {
    for mode in ["descendant", "timeout_child"] {
        let f = Fixture::new(mode, true);
        let start = Instant::now();
        let c = f.refresh();
        assert_eq!(
            c.model_catalog.state,
            if mode == "descendant" {
                CapabilityState::Supported
            } else {
                CapabilityState::Unknown
            }
        );
        assert_eq!(c.process_cleanup.state, CapabilityState::Supported);
        assert!(start.elapsed() < Duration::from_secs(15));
        if mode == "descendant" {
            assert!(start.elapsed() < Duration::from_secs(5));
        }
        let child: i32 = fs::read_to_string(f.temp.path().join("child"))
            .unwrap()
            .parse()
            .unwrap();
        f.assert_reaped(child);
        f.assert_reaped(f.calls()[2]["pid"].as_i64().unwrap() as i32);
        f.assert_initialize_only();
        assert_private(&c);
        assert!(
            !f.config
                .workspace_root
                .join(".catalog-discovery-in-flight")
                .exists()
        );
    }
}

#[test]
fn retained_unknown_cleanup_guard_survives_restart_and_new_confirmation() {
    let mut f = Fixture::new("success", true);
    let marker = f.config.workspace_root.join(".catalog-discovery-in-flight");
    fs::write(&marker, "operator must verify stopped\n").unwrap();
    let first = f.refresh();
    assert_eq!(first.process_cleanup.state, CapabilityState::Unknown);
    assert!(first.models.is_empty());
    let previous = f.approval();
    f.app = Application::open(f.temp.path().join("db"), f.config.clone()).unwrap();
    let fresh = f.approval();
    assert_ne!(fresh.confirmation_token, previous.confirmation_token);
    assert!(
        f.app
            .refresh_capabilities_confirmed("claude", &previous)
            .is_err()
    );
    let restarted = f
        .app
        .refresh_capabilities_confirmed("claude", &fresh)
        .unwrap()
        .catalog
        .unwrap();
    assert_eq!(restarted.process_cleanup.state, CapabilityState::Unknown);
    assert!(restarted.models.is_empty());
    assert!(f.calls().is_empty());
    assert!(marker.exists());
    assert!(
        f.app
            .refresh_capabilities_confirmed("claude", &fresh)
            .is_err()
    );
}

fn request(
    method: &str,
    path: &str,
    body: &str,
    bearer: bool,
    cookie: &str,
    origin: &str,
) -> Request<Body> {
    let mut request = Request::builder().method(method).uri(path);
    if !body.is_empty() {
        request = request.header("content-type", "application/json");
    }
    if bearer {
        request = request.header("authorization", format!("Bearer {TOKEN}"));
    }
    if !cookie.is_empty() {
        request = request.header("cookie", cookie);
    }
    if !origin.is_empty() {
        request = request.header("origin", origin);
    }
    request.body(Body::from(body.to_owned())).unwrap()
}
async fn body(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn http_auth_body_validation_confirmation_and_cached_gets() {
    let f = Fixture::new("success", true);
    let router = http::router(f.app.clone(), TOKEN.into()).unwrap();
    let path = "/api/capabilities/claude/refresh";
    for (method, path) in [("GET", "/api/capabilities"), ("POST", path)] {
        let response = router
            .clone()
            .oneshot(request(method, path, "{}", false, "", ""))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let response = router
        .clone()
        .oneshot(request("GET", "/api/capabilities", "", true, "", ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let profiles = body(response).await;
    let token = profiles["profiles"][0]["startup_discovery"]["confirmation_token"]
        .as_str()
        .unwrap();
    let valid = json!({"confirm_startup_effects":true,"confirmation_token":token}).to_string();
    for invalid in [
        "{",
        "null",
        "{\"confirm_startup_effects\":\"true\"}",
        "{\"unexpected\":true}",
    ] {
        let response = router
            .clone()
            .oneshot(request("POST", path, invalid, true, "", ""))
            .await
            .unwrap();
        assert!(
            response.status().is_client_error(),
            "{} for {invalid}",
            response.status()
        );
    }
    let oversized =
        json!({"confirm_startup_effects":true,"confirmation_token":"x".repeat(97 * 1024)})
            .to_string();
    let response = router
        .clone()
        .oneshot(request("POST", path, &oversized, true, "", ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let response = router
        .clone()
        .oneshot(request("POST", path, "", true, "", ""))
        .await
        .unwrap();
    assert!(!response.status().is_success());
    assert!(f.calls().is_empty());
    let response = router
        .clone()
        .oneshot(request("POST", path, &valid, true, "", ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let refreshed = body(response).await;
    assert_eq!(refreshed["catalog"]["model_catalog"]["state"], "supported");
    assert_eq!(refreshed["catalog"]["model_catalog"]["source"], SOURCE);
    assert_private(&refreshed);
    for _ in 0..3 {
        let response = router
            .clone()
            .oneshot(request("GET", "/api/capabilities", "", true, "", ""))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let cached = body(response).await;
        assert_eq!(cached["profiles"][0]["generation"], refreshed["generation"]);
        assert_eq!(cached["profiles"][0]["catalog"], refreshed["catalog"]);
    }
    let replay = router
        .oneshot(request("POST", path, &valid, true, "", ""))
        .await
        .unwrap();
    assert!(!replay.status().is_success());
    assert_eq!(f.calls().len(), 3);
    f.assert_initialize_only();
}

#[tokio::test]
async fn http_legacy_empty_body_still_runs_only_version_help_when_disabled() {
    let f = Fixture::new("success", false);
    let router = http::router(f.app.clone(), TOKEN.into()).unwrap();
    let response = router
        .oneshot(request(
            "POST",
            "/api/capabilities/claude/refresh",
            "",
            true,
            "",
            "",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body(response).await;
    assert_eq!(value["catalog"]["model_catalog"]["state"], "unknown");
    assert!(value["startup_discovery"].is_null());
    assert_eq!(f.calls().len(), 2);
    let router = http::router(f.app.clone(), TOKEN.into()).unwrap();
    let mut empty_json = request("POST", "/api/capabilities/claude/refresh", "", true, "", "");
    empty_json
        .headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    let response = router.oneshot(empty_json).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(f.calls().len(), 4);
    assert!(f.calls().iter().all(|call| {
        call["argv"]
            .as_array()
            .is_some_and(|args| !args.iter().any(|arg| arg == "-p"))
    }));
}

#[tokio::test]
async fn browser_session_rejects_cross_origin_refresh_without_consuming_confirmation() {
    use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
    use rand_core::OsRng;
    let f = Fixture::new("success", true);
    let credentials = f.temp.path().join("credentials.json");
    let hash = Argon2::default()
        .hash_password(b"fixture-password", &SaltString::generate(&mut OsRng))
        .unwrap()
        .to_string();
    fs::write(
        &credentials,
        json!({"username":"operator","password_hash":hash}).to_string(),
    )
    .unwrap();
    fs::set_permissions(&credentials, fs::Permissions::from_mode(0o600)).unwrap();
    let auth = relay_app::auth::Auth::new(
        Some(relay_app::auth::Config {
            mode: relay_app::auth::Mode::Session,
            credentials_file: credentials,
            public_origin: "https://relay.example".into(),
            allow_insecure_loopback: false,
        }),
        None,
    )
    .unwrap();
    let router = http::router_with_auth(f.app.clone(), auth);
    let login = router
        .clone()
        .oneshot(request(
            "POST",
            "/auth/login",
            r#"{"username":"operator","password":"fixture-password"}"#,
            false,
            "",
            "https://relay.example",
        ))
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    let cookie = login.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let response = router
        .clone()
        .oneshot(request("GET", "/api/capabilities", "", false, &cookie, ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body(response).await;
    let valid = json!({"confirm_startup_effects":true,"confirmation_token":value["profiles"][0]["startup_discovery"]["confirmation_token"]}).to_string();
    let path = "/api/capabilities/claude/refresh";
    for origin in ["", "https://evil.example"] {
        let response = router
            .clone()
            .oneshot(request("POST", path, &valid, false, &cookie, origin))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    assert!(f.calls().is_empty());
    let response = router
        .oneshot(request(
            "POST",
            path,
            &valid,
            false,
            &cookie,
            "https://relay.example",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body(response).await["catalog"]["model_catalog"]["state"],
        "supported"
    );
    assert_eq!(f.calls().len(), 3);
    f.assert_initialize_only();
}
