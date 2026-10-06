#![cfg(target_os = "linux")]
use relay_app::capabilities::{CapabilityState, discover};
use relay_app::host::{Host, HostConfig};
use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const CLI: &str = r#"#!/usr/bin/python3
import json, os, pathlib, sys, time
trace=pathlib.Path(os.environ['TRACE'])
def record(v):
    with trace.open('a') as f: f.write(json.dumps(v)+'\n')
record({'argv':sys.argv[1:]})
if '--version' in sys.argv: print('codex-cli 0.160.0'); sys.exit()
if '--help' in sys.argv:
    print('codex app-server --json --sandbox --skip-git-repo-check --ephemeral --model --config')
    sys.exit()
assert sys.argv[1:]==['app-server']
mode=os.environ.get('MODE','success')
def recv():
    v=json.loads(sys.stdin.readline());record(v);return v
def send(v): print(json.dumps(v),flush=True)
a=recv();assert a['method']=='initialize';send({'id':a['id'],'result':{}})
assert recv()['method']=='initialized'
a=recv();assert a['method']=='model/list' and a['params']['includeHidden'] is True
if mode=='request':
    send({'id':'approval','method':'item/commandExecution/requestApproval','params':{'token':'NEVER_EXPOSE_SECRET'}})
    a=recv();assert a['error']['code']==-32601
    time.sleep(60)
if mode=='malformed': print('not json',flush=True);time.sleep(60)
if mode=='timeout': time.sleep(60)
if mode=='delay': time.sleep(1)
if mode=='partial':
    send({'id':a['id'],'result':{'data':[{'id':'partial','model':'partial'}],'nextCursor':'more'}})
    sys.exit()
if mode=='descendant':
    pid=os.fork()
    if pid==0:
        os.setsid()
        pathlib.Path(os.environ['CHILD']).write_text(str(os.getpid()))
        time.sleep(60);sys.exit()
    limit=time.monotonic()+2
    while not pathlib.Path(os.environ['CHILD']).exists() and time.monotonic()<limit:time.sleep(.01)
send({'method':'account/updated','params':{'token':'NEVER_EXPOSE_SECRET'}})
send({'id':a['id'],'result':{'data':[{'id':'fixture-a','model':'fixture-a','displayName':'Fixture A','supportedReasoningEfforts':[{'reasoningEffort':'high','description':'Fixture effort'}],'defaultReasoningEffort':'high','isDefault':True}],'nextCursor':'page-two'}})
a=recv();assert a['method']=='model/list' and a['params']['cursor']=='page-two'
send({'id':a['id'],'result':{'data':[{'id':'fixture-b','model':'fixture-b','hidden':True}],'nextCursor':None}})
time.sleep(60)
"#;
const CLAUDE_CLI: &str = r#"#!/usr/bin/python3
import json, os, pathlib, sys
with pathlib.Path(os.environ['TRACE']).open('a') as f: f.write(json.dumps(sys.argv[1:])+'\n')
if sys.argv[1:]==['--version']: print('2.1.281 (Claude Code)');sys.exit()
if sys.argv[1:]==['--help']:
    print('--output-format --verbose --permission-prompts --no-session-persistence --model --effort --restricted --tools --allowedTools --disallowedTools --disable-slash-commands --strict-mcp-config --mcp-config');sys.exit()
raise AssertionError('Claude discovery must not launch unverified managed startup')
"#;
struct Fixture {
    temp: TempDir,
    host: Host,
}
impl Fixture {
    fn new(mode: &str, provider: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let program = temp.path().join("fake.py");
        fs::write(
            &program,
            if provider == "claude_cli" {
                CLAUDE_CLI
            } else {
                CLI
            },
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let config:HostConfig=serde_json::from_value(json!({
            "workspace_root":temp.path().join("workspaces"),"repositories":{"source":source},
            "native_agents":{"fixture":{"provider":provider,"program":program,"model":"fixture-a","effort":"high","env":{"MODE":mode,"TRACE":temp.path().join("trace"),"CHILD":temp.path().join("child"),"PRIVATE":"NEVER_EXPOSE_SECRET"}}},
            "tests":{"test":{"program":"/bin/true"}},"supervisor_program":env!("CARGO_BIN_EXE_relay-app")
        })).unwrap();
        Self {
            temp,
            host: Host::new(config).unwrap(),
        }
    }
    fn run(&self) -> relay_app::capabilities::ProfileCatalog {
        discover(&self.host, &self.host.config().native_agents["fixture"])
    }
}
#[test]
fn fake_codex_catalog_is_complete_read_only_and_credential_free() {
    for provider in ["codex_app_server", "codex_cli"] {
        let f = Fixture::new("success", provider);
        let before = Instant::now();
        let c = f.run();
        assert_eq!(
            c.model_catalog.state,
            CapabilityState::Supported,
            "{}",
            serde_json::to_string(&c).unwrap()
        );
        assert_eq!(c.models.len(), 2);
        assert_eq!(c.cli_version.as_deref(), Some("0.160.0"));
        assert_eq!(c.authentication.state, CapabilityState::Unknown);
        assert_eq!(c.reviewer_isolation.state, CapabilityState::Unsupported);
        assert!(c.selection.effective_model.is_none());
        assert!(c.selection.effective_effort.is_none());
        assert_eq!(c.selection.status.state, CapabilityState::Supported);
        assert!(
            !serde_json::to_string(&c)
                .unwrap()
                .contains("NEVER_EXPOSE_SECRET")
        );
        assert!(before.elapsed() < Duration::from_secs(5));
        let trace = fs::read_to_string(f.temp.path().join("trace")).unwrap();
        assert!(
            !trace.contains("thread/start")
                && !trace.contains("turn/start")
                && !trace.contains("login")
        );
        assert_eq!(
            fs::read_dir(f.host.config().workspace_root.clone())
                .unwrap()
                .count(),
            0
        );
    }
}
#[test]
fn failures_discard_partial_models_and_do_not_hang() {
    for mode in ["request", "malformed", "partial"] {
        let f = Fixture::new(mode, "codex_app_server");
        let before = Instant::now();
        let c = f.run();
        assert_eq!(c.model_catalog.state, CapabilityState::Unknown);
        assert!(c.models.is_empty());
        assert_eq!(c.process_cleanup.state, CapabilityState::Supported);
        assert!(before.elapsed() < Duration::from_secs(5));
        assert!(
            !serde_json::to_string(&c)
                .unwrap()
                .contains("NEVER_EXPOSE_SECRET")
        );
    }
}
#[test]
fn successful_catalog_reaps_escaped_descendants_that_hold_pipes() {
    let f = Fixture::new("descendant", "codex_app_server");
    let before = Instant::now();
    let c = f.run();
    assert_eq!(c.model_catalog.state, CapabilityState::Supported);
    assert!(before.elapsed() < Duration::from_secs(5));
    let pid: i32 = fs::read_to_string(f.temp.path().join("child"))
        .unwrap()
        .parse()
        .unwrap();
    // SAFETY: signal 0 only observes the fixture child; it never signals a process.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}
#[test]
fn persistent_unknown_cleanup_marker_blocks_all_subprocesses() {
    let f = Fixture::new("success", "codex_app_server");
    fs::write(
        f.host
            .config()
            .workspace_root
            .join(".catalog-discovery-in-flight"),
        "inspect",
    )
    .unwrap();
    let c = f.run();
    assert_eq!(c.process_cleanup.state, CapabilityState::Unknown);
    assert!(c.models.is_empty());
    assert!(!f.temp.path().join("trace").exists());
}
#[test]
fn timeout_is_bounded_and_does_not_report_model_access() {
    let f = Fixture::new("timeout", "codex_app_server");
    let before = Instant::now();
    let c = f.run();
    assert_eq!(c.model_catalog.state, CapabilityState::Unknown);
    assert!(c.models.is_empty());
    assert_eq!(c.authentication.state, CapabilityState::Unknown);
    assert_eq!(c.process_cleanup.state, CapabilityState::Supported);
    assert!(before.elapsed() < Duration::from_secs(15));
}

#[test]
fn active_guard_cannot_be_reconciled_and_other_refresh_is_only_busy() {
    let fixture = std::sync::Arc::new(Fixture::new("delay", "codex_app_server"));
    let running = std::sync::Arc::clone(&fixture);
    let worker = std::thread::spawn(move || running.run());
    let deadline = Instant::now() + Duration::from_secs(3);
    while !fixture.temp.path().join("trace").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let busy = fixture.run();
    assert_eq!(busy.process_cleanup.state, CapabilityState::Supported);
    assert_eq!(busy.model_catalog.state, CapabilityState::Unknown);
    assert!(busy.model_catalog.reason.contains("active"));
    assert!(relay_app::capabilities::confirm_discovery_stopped(&fixture.host).is_err());
    assert_eq!(
        worker.join().unwrap().model_catalog.state,
        CapabilityState::Supported
    );
    assert!(!relay_app::capabilities::confirm_discovery_stopped(&fixture.host).unwrap());
}
#[test]
fn explicit_reconciliation_recovers_unknown_guard_without_deleting_diagnostics() {
    let fixture = Fixture::new("success", "codex_app_server");
    let mut config = fixture.host.config().clone();
    config.supervisor_program = Some("/bin/true".into());
    let broken = Host::new(config).unwrap();
    let c = discover(&broken, &broken.config().native_agents["fixture"]);
    assert_eq!(c.process_cleanup.state, CapabilityState::Unknown);
    let blocked = fixture.run();
    assert_eq!(blocked.process_cleanup.state, CapabilityState::Unknown);
    assert!(!fixture.temp.path().join("trace").exists());
    assert!(relay_app::capabilities::confirm_discovery_stopped(&fixture.host).unwrap());
    assert_eq!(
        fixture.run().model_catalog.state,
        CapabilityState::Supported
    );
    // Reconciliation clears only the guard, preserving retained PID diagnostics.
    assert!(
        fs::read_dir(&fixture.host.config().workspace_root)
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".catalog-"))
    );
}
#[test]
fn reconciliation_refuses_symlinks_without_touching_target() {
    let fixture = Fixture::new("success", "codex_app_server");
    let target = fixture.temp.path().join("unrelated");
    fs::write(&target, "keep").unwrap();
    std::os::unix::fs::symlink(
        &target,
        fixture
            .host
            .config()
            .workspace_root
            .join(".catalog-discovery-in-flight"),
    )
    .unwrap();
    assert!(relay_app::capabilities::confirm_discovery_stopped(&fixture.host).is_err());
    assert_eq!(fs::read_to_string(target).unwrap(), "keep");
}

#[test]
fn claude_without_opt_in_and_confirmation_keeps_manual_discovery() {
    let fixture = Fixture::new("success", "claude_cli");
    let c = fixture.run();
    assert_eq!(c.compatibility.state, CapabilityState::Supported);
    assert_eq!(c.cli_version.as_deref(), Some("2.1.281"));
    assert_eq!(c.model_catalog.state, CapabilityState::Unknown);
    assert!(c.model_catalog.reason.contains("initialization"));
    assert!(c.startup_context.reason.contains("freshly confirmed"));
    assert!(c.models.is_empty());
    assert_eq!(c.authentication.state, CapabilityState::Unknown);
    let trace = fs::read_to_string(fixture.temp.path().join("trace")).unwrap();
    let calls: Vec<serde_json::Value> = trace
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(calls, vec![json!(["--version"]), json!(["--help"])]);
}

#[test]
fn explicit_reviewer_profile_can_probe_without_implying_claude_catalog_safety() {
    let fixture = Fixture::new("success", "claude_cli");
    let mut config = fixture.host.config().clone();
    config
        .native_agents
        .get_mut("fixture")
        .unwrap()
        .native_permission = Some(relay_app::providers::NativePermission::ClaudeRestricted);
    let host = Host::new(config.clone()).unwrap();
    let catalog = discover(&host, &config.native_agents["fixture"]);
    assert_eq!(catalog.compatibility.state, CapabilityState::Supported);
    assert_eq!(catalog.reviewer_isolation.state, CapabilityState::Supported);
    assert_eq!(catalog.model_catalog.state, CapabilityState::Unknown);
    assert_eq!(catalog.permission_control.state, CapabilityState::Unknown);
    let config_path = fixture.temp.path().join("doctor.json");
    fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_relay-app"))
        .args(["doctor", config_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["profiles"][0]["compatible"], true, "{result}");
}
