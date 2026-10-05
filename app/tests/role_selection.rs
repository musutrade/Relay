#![cfg(target_os = "linux")]
use relay_app::{
    Application, Error, Submission,
    host::{HostConfig, Job},
    providers::NativePermission,
};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::Command,
    sync::{Mutex, MutexGuard},
};

// Parallel forks can briefly retain another fixture's CLOEXEC workspace lease
// before exec. Isolate complete fixture lifetimes, matching the session/review
// suites; production nonblocking ownership checks and their failures stay intact.
static FIXTURES: Mutex<()> = Mutex::new(());
struct Fixture {
    temp: tempfile::TempDir,
    _serial: MutexGuard<'static, ()>,
}
impl Fixture {
    fn new() -> Self {
        let serial = FIXTURES.lock().unwrap_or_else(|error| error.into_inner());
        Self {
            temp: tempfile::tempdir().unwrap(),
            _serial: serial,
        }
    }
    fn path(&self) -> &Path {
        self.temp.path()
    }
}

fn config(root: &Path) -> HostConfig {
    let source = root.join("source");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("original.txt"), "original\n").unwrap();
    serde_json::from_value(json!({
        "workspace_root":root.join("runs"),"repositories":{"repo":source},
        "native_agents":{
            "dev":{"provider":"codex_cli","program":"/bin/true"},
            "other":{"provider":"codex_app_server","program":"/bin/true"},
            "review":{"provider":"claude_cli","program":"/bin/true"},
            "isolated":{"provider":"claude_cli","program":"/bin/true","session_continuity":true}
        },"agents":{"generic":{"program":"/bin/true"}},
        "tests":{"test":{"program":"/bin/true"}},
        "workflows":{"checked":{"repository":"repo","developer":"dev","reviewer":"review","test":"test"}},
        "supervisor_program":env!("CARGO_BIN_EXE_relay-app")
    })).unwrap()
}
fn job() -> Value {
    json!({"repository":"repo","requirements":"Do work","agent":"dev","workflow":"checked"})
}
fn validate(value: Value, config: &HostConfig) -> Result<(), String> {
    serde_json::from_value::<Job>(value)
        .map_err(|e| e.to_string())?
        .validate(config)
        .map_err(|e| e.to_string())
}
fn submit(app: &Application, key: &str, job: Value) -> Result<relay::Task, Error> {
    app.submit(serde_json::from_value::<Submission>(json!({"key":key,"job":job})).unwrap())
}
#[test]
fn initial_role_policy_rejects_unapproved_profiles_paths_and_reviewer_expansion() {
    let temp = Fixture::new();
    let mut config = config(temp.path());
    assert!(validate(job(), &config).is_ok());
    let mut selected = job();
    selected["agent"] = json!("other");
    selected["role_selections"] = json!({"developer":{"profile":"other"}});
    assert!(
        validate(selected.clone(), &config)
            .unwrap_err()
            .contains("allowlisted")
    );
    config
        .workflows
        .get_mut("checked")
        .unwrap()
        .selectable_developers = Some(vec!["other".into()]);
    assert!(validate(selected.clone(), &config).is_ok());
    selected["agent"] = json!("dev");
    assert!(
        validate(selected, &config)
            .unwrap_err()
            .contains("job.agent")
    );
    for rejected in [
        json!({"reviewer":{"profile":"other"}}),
        json!({"reviewer":{"profile":"generic"}}),
        json!({"reviewer":{"profile":"review","native_permission":"claude_auto","confirm_permission_expansion":true}}),
        json!({"reviewer":{"profile":"review","native_permission":"claude_bypass_permissions","confirm_permission_expansion":true}}),
        json!({"developer":{"profile":"dev","program":"/bin/sh"}}),
        json!({"developer":{"profile":"dev","env":{"PATH":"evil"}}}),
        json!({"developer":{"profile":"../dev"}}),
        json!({}),
    ] {
        let mut value = job();
        value["role_selections"] = rejected;
        assert!(validate(value, &config).is_err());
    }
    let mut standalone = job();
    standalone.as_object_mut().unwrap().remove("workflow");
    standalone["role_selections"] = json!({"reviewer":{"profile":"review"}});
    assert!(
        validate(standalone, &config)
            .unwrap_err()
            .contains("requires")
    );
}
#[test]
fn native_permission_requires_host_policy_and_explicit_expansion_confirmation() {
    let temp = Fixture::new();
    let mut config = config(temp.path());
    let mut selected = job();
    selected["role_selections"] = json!({"developer":{"profile":"dev","native_permission":"codex_full_access","confirm_permission_expansion":true}});
    assert!(
        validate(selected.clone(), &config)
            .unwrap_err()
            .contains("host policy")
    );
    config
        .native_agents
        .get_mut("dev")
        .unwrap()
        .allowed_permission_modes = vec![NativePermission::CodexFullAccess];
    selected["role_selections"]["developer"]
        .as_object_mut()
        .unwrap()
        .remove("confirm_permission_expansion");
    let error = validate(selected.clone(), &config).unwrap_err();
    assert!(error.contains("filesystem AND network"), "{error}");
    selected["role_selections"]["developer"]["confirm_permission_expansion"] = json!(true);
    assert!(validate(selected, &config).is_ok());
    let mut safe = job();
    safe["role_selections"] = json!({"developer":{"profile":"dev","native_permission":"codex_workspace_write"},"reviewer":{"profile":"review","native_permission":"claude_restricted"}});
    assert!(validate(safe, &config).is_ok());
    config
        .native_agents
        .get_mut("review")
        .unwrap()
        .native_permission = Some(NativePermission::ClaudeBypassPermissions);
    let mut unsafe_reviewer = job();
    unsafe_reviewer["role_selections"] =
        json!({"reviewer":{"profile":"review","native_permission":"claude_restricted"}});
    assert!(validate(unsafe_reviewer, &config).is_err());
}
#[test]
fn manual_model_is_bounded_and_unknown_effort_never_becomes_a_choice() {
    let temp = Fixture::new();
    let config = config(temp.path());
    let mut selected = job();
    selected["role_selections"] = json!({"developer":{"profile":"dev","model":{"value":"unverified-new-model","source":"manual"}}});
    assert!(validate(selected.clone(), &config).is_ok());
    selected["role_selections"]["developer"]["effort"] = json!("future-effort");
    assert!(
        validate(selected.clone(), &config)
            .unwrap_err()
            .contains("catalog")
    );
    selected["role_selections"]["developer"]
        .as_object_mut()
        .unwrap()
        .remove("effort");
    for model in [
        "-bad".to_owned(),
        "control\n".into(),
        "x".repeat(257),
        "".into(),
    ] {
        selected["role_selections"]["developer"]["model"]["value"] = json!(model);
        assert!(validate(selected.clone(), &config).is_err());
    }
}
#[test]
fn legacy_canonical_payload_and_profile_bytes_remain_identical() {
    let value: Job =
        serde_json::from_value(json!({"repository":"repo","requirements":"Do work","agent":"dev"}))
            .unwrap();
    assert_eq!(
        serde_json::to_string(&value).unwrap(),
        r#"{"repository":"repo","requirements":"Do work","agent":"dev","test":null,"publish":false,"draft_pr_adapter":null}"#
    );
    let profile: relay_app::providers::NativeProfile =
        serde_json::from_value(json!({"provider":"codex_cli","program":"/bin/true"})).unwrap();
    assert_eq!(
        serde_json::to_string(&profile).unwrap(),
        r#"{"provider":"codex_cli","program":"/bin/true","env":{},"model":null,"effort":null,"max_turns":null,"max_budget_usd":null,"session_continuity":false}"#
    );
    let workflow: relay_app::workflow::WorkflowConfig = serde_json::from_value(
        json!({"repository":"repo","developer":"dev","reviewer":"review","test":"test"}),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_string(&workflow).unwrap(),
        r#"{"repository":"repo","developer":"dev","reviewer":"review","test":"test","draft_pr_adapter":null,"git_program":"/usr/bin/git","max_repairs":0,"base_branch":"main","github_repository":null}"#
    );
}
#[test]
fn selected_reviewer_resource_estimate_uses_its_checkout_requirement_and_policy() {
    let temp = Fixture::new();
    let mut config = config(temp.path());
    config
        .workflows
        .get_mut("checked")
        .unwrap()
        .selectable_reviewers = Some(vec!["isolated".into()]);
    let app = Application::open(temp.path().join("queue.db"), config).unwrap();
    let base = app.resource_estimate("repo", Some("checked")).unwrap();
    let changed = app
        .resource_estimate_with_reviewer("repo", Some("checked"), Some("isolated"))
        .unwrap();
    assert_eq!(base.initial_estimate.reviewer_copy_bytes, Some(0));
    assert_eq!(
        changed.initial_estimate.reviewer_copy_bytes,
        changed.initial_estimate.snapshot_bytes
    );
    for bad in ["dev", "../review", "unknown"] {
        assert!(
            app.resource_estimate_with_reviewer("repo", Some("checked"), Some(bad))
                .is_err()
        );
    }
    assert!(
        app.resource_estimate_with_reviewer("repo", None, Some("review"))
            .is_err()
    );
    let public = app.public_config();
    let dev = public["native_agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "dev")
        .unwrap();
    let full = dev["permission_modes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "codex_full_access")
        .unwrap();
    assert_eq!(full["host_allowed"], false);
    assert!(
        full["confirmation_text"]
            .as_str()
            .unwrap()
            .contains("filesystem AND network")
    );
}
const CATALOG: &str = r#"#!/usr/bin/python3
import sys,json,time
if '--version' in sys.argv: print('codex-cli 0.160.0');sys.exit()
if '--help' in sys.argv: print('codex app-server --json --sandbox --skip-git-repo-check --ephemeral --model --config');sys.exit()
def recv():return json.loads(sys.stdin.readline())
def send(v):print(json.dumps(v),flush=True)
a=recv();assert a['method']=='initialize';send({'id':a['id'],'result':{}})
assert recv()['method']=='initialized'
a=recv();assert a['method']=='model/list'
send({'id':a['id'],'result':{'data':[{'id':'new-id','model':'new-model','supportedReasoningEfforts':[{'reasoningEffort':'future"effort\\safe'}]},{'id':'missing','model':'no-effort-metadata'}],'nextCursor':None}})
time.sleep(60)
"#;
fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
#[test]
fn dynamic_catalog_selection_is_model_specific_and_replays_survive_restart() {
    let temp = Fixture::new();
    let mut config = config(temp.path());
    let path = temp.path().join("catalog");
    executable(&path, CATALOG);
    config.native_agents.get_mut("dev").unwrap().program = path;
    let db = temp.path().join("queue.db");
    let app = Application::open(&db, config.clone()).unwrap();
    let catalog = serde_json::to_value(app.refresh_capabilities("dev").unwrap()).unwrap();
    let reference =
        json!({"cache_epoch":catalog["cache_epoch"],"generation":catalog["generation"]});
    let mut selected = job();
    selected["role_selections"] = json!({"developer":{"profile":"dev","model":{"value":"new-model","source":"catalog","catalog":reference},"effort":"future\"effort\\safe"}});
    let task = submit(&app, "catalog-choice", selected.clone()).unwrap();
    let mut unknown = selected.clone();
    unknown["role_selections"]["developer"]["model"]["value"] = json!("no-effort-metadata");
    assert!(
        submit(&app, "no-effort", unknown)
            .unwrap_err()
            .to_string()
            .contains("no effort metadata")
    );
    let mut invalid = selected.clone();
    invalid["role_selections"]["developer"]["effort"] = json!("provider-wide-made-up");
    assert!(submit(&app, "not-supported", invalid).is_err());
    app.refresh_capabilities("dev").unwrap();
    assert_eq!(
        submit(&app, "catalog-choice", selected.clone()).unwrap().id,
        task.id
    );
    assert!(submit(&app, "stale", selected.clone()).is_err());
    drop(app);
    let app = Application::open(&db, config).unwrap();
    assert_eq!(
        submit(&app, "catalog-choice", selected.clone()).unwrap().id,
        task.id
    );
    assert!(submit(&app, "new-after-restart", selected.clone()).is_err());
    selected["requirements"] = json!("changed");
    assert!(matches!(
        submit(&app, "catalog-choice", selected),
        Err(Error::Core(relay::Error::IdempotencyConflict))
    ));
}
const ROLE_CLI: &str = r#"#!/usr/bin/python3
import json,os,pathlib,sys
if '--version' in sys.argv:print('2.1.259');sys.exit()
if '--help' in sys.argv:print('--output-format --verbose --permission-prompts --restricted --tools --allowedTools --disallowedTools --disable-slash-commands --strict-mcp-config --mcp-config --no-session-persistence --model --effort --session-id --resume --permission-mode');sys.exit()
prompt=sys.stdin.read();review='--restricted' in sys.argv
model=sys.argv[sys.argv.index('--model')+1]
assert model==('review-model' if review else 'developer-model')
session=sys.argv[sys.argv.index('--session-id')+1] if '--session-id' in sys.argv else sys.argv[sys.argv.index('--resume')+1]
with open(os.environ['TRACE'],'a') as f:f.write(json.dumps({'review':review,'model':model,'session':session,'argv':sys.argv})+'\n')
print(json.dumps({'type':'system','subtype':'init','model':model,'session_id':session,'permissionMode':'default'}))
print(json.dumps({'type':'assistant','parent_tool_use_id':None,'message':{'model':model,'content':[]}}))
if review:
  answer=json.dumps({'candidate_sha':os.environ['RELAY_CANDIDATE_SHA'],'verdict':'approved','summary':'Reviewed candidate','findings':[]})
  marker=os.environ.get('FAIL_REVIEW_ONCE')
  if marker and not pathlib.Path(marker).exists():pathlib.Path(marker).write_text('failed');answer='malformed review'
else:
  pathlib.Path('changed.txt').write_text('implemented\n');answer='Implemented'
print(json.dumps({'type':'result','subtype':'success','is_error':False,'result':answer,'session_id':session,'permission_denials':[]}))
"#;
fn git(path: &Path, args: &[&str]) {
    let status = Command::new("/usr/bin/git")
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
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
}
#[test]
fn same_profile_resolves_independent_role_models_sessions_and_result_evidence() {
    let temp = Fixture::new();
    let mut config = config(temp.path());
    let source = config.repositories["repo"].clone();
    git(&source, &["init", "--initial-branch=main"]);
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "base"]);
    let path = temp.path().join("roles");
    executable(&path, ROLE_CLI);
    let profile=serde_json::from_value(json!({"provider":"claude_cli","program":path,"session_continuity":true,"env":{"TRACE":temp.path().join("trace")}})).unwrap();
    config.native_agents.insert("shared".into(), profile);
    let workflow = config.workflows.get_mut("checked").unwrap();
    workflow.selectable_developers = Some(vec!["shared".into()]);
    workflow.selectable_reviewers = Some(vec!["shared".into()]);
    let app = Application::open(temp.path().join("queue.db"), config).unwrap();
    let mut selected = job();
    selected["agent"] = json!("shared");
    selected["role_selections"] = json!({"developer":{"profile":"shared","model":{"value":"developer-model","source":"manual"}},"reviewer":{"profile":"shared","model":{"value":"review-model","source":"manual"},"native_permission":"claude_restricted"}});
    let task = submit(&app, "two-roles", selected).unwrap();
    assert!(app.work_once().unwrap());
    let result: Value = serde_json::from_str(
        app.get_view(task.id)
            .unwrap()
            .task
            .result
            .as_deref()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["outcome"], "success", "{result}");
    assert_eq!(
        result["workflow"]["rounds"][0]["developer"]["selection"]["requested"]["model"],
        "developer-model"
    );
    assert_eq!(
        result["workflow"]["rounds"][0]["reviewer"]["selection"]["requested"]["model"],
        "review-model"
    );
    assert_eq!(
        result["workflow"]["rounds"][0]["reviewer"]["selection"]["observed"]["model"],
        "review-model"
    );
    let trace: Vec<Value> = fs::read_to_string(temp.path().join("trace"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(trace.len(), 2);
    assert_ne!(trace[0]["session"], trace[1]["session"]);
    assert_eq!(trace[0]["review"], false);
    assert_eq!(trace[1]["review"], true);
}

#[test]
fn selected_reviewer_continues_same_candidate_and_session_without_redevelopment() {
    let temp = Fixture::new();
    let mut config = config(temp.path());
    let source = config.repositories["repo"].clone();
    git(&source, &["init", "--initial-branch=main"]);
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "base"]);
    let path = temp.path().join("roles");
    executable(&path, ROLE_CLI);
    let profile=serde_json::from_value(json!({"provider":"claude_cli","program":path,"session_continuity":true,"env":{"TRACE":temp.path().join("trace"),"FAIL_REVIEW_ONCE":temp.path().join("failed-once")}})).unwrap();
    config.native_agents.insert("shared".into(), profile);
    let workflow = config.workflows.get_mut("checked").unwrap();
    workflow.selectable_developers = Some(vec!["shared".into()]);
    workflow.selectable_reviewers = Some(vec!["shared".into()]);
    let app = Application::open(temp.path().join("queue.db"), config).unwrap();
    let mut selected = job();
    selected["agent"] = json!("shared");
    selected["role_selections"] = json!({"developer":{"profile":"shared","model":{"value":"developer-model","source":"manual"}},"reviewer":{"profile":"shared","model":{"value":"review-model","source":"manual"},"native_permission":"claude_restricted"}});
    let task = submit(&app, "review-fails", selected.clone()).unwrap();
    assert!(app.work_once().unwrap());
    let before = app.get_view(task.id).unwrap().task;
    let failed: Value = serde_json::from_str(before.result.as_deref().unwrap()).unwrap();
    assert_eq!(failed["outcome"], "failure", "{failed}");
    let input = || {
        serde_json::from_value(json!({"key":"resume-review","confirm_stopped_and_reconciled":true,"revalidate_tests":true})).unwrap()
    };
    let successor = app.continue_review(task.id, input()).unwrap();
    assert_eq!(
        app.continue_review(task.id, input()).unwrap().id,
        successor.id
    );
    let inherited: Value = serde_json::from_str(&successor.payload).unwrap();
    assert_eq!(inherited["role_selections"], selected["role_selections"]);
    assert!(app.work_once().unwrap());
    let result: Value = serde_json::from_str(
        app.get_view(successor.id)
            .unwrap()
            .task
            .result
            .as_deref()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["outcome"], "success", "{result}");
    assert_eq!(
        result["workflow"]["candidate_sha"],
        failed["workflow"]["candidate_sha"]
    );
    assert_eq!(app.get_view(task.id).unwrap().task.result, before.result);
    let trace: Vec<Value> = fs::read_to_string(temp.path().join("trace"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(trace.len(), 3);
    assert_eq!(trace[0]["review"], false);
    assert_eq!(trace[1]["review"], true);
    assert_eq!(trace[2]["review"], true);
    assert_eq!(trace[1]["session"], trace[2]["session"]);
    assert!(
        trace[2]["argv"]
            .as_array()
            .unwrap()
            .contains(&json!("--resume"))
    );
}

#[tokio::test]
async fn http_and_mcp_share_role_admission_and_selected_reviewer_estimates() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let temp = Fixture::new();
    let mut config = config(temp.path());
    config
        .workflows
        .get_mut("checked")
        .unwrap()
        .selectable_reviewers = Some(vec!["isolated".into()]);
    let app = Application::open(temp.path().join("queue.db"), config).unwrap();
    let token = "role-transport-token-000000000000000000";
    let router = relay_app::http::router(app.clone(), token.into()).unwrap();
    let mut selected = job();
    selected["role_selections"] = json!({"developer":{"profile":"dev","model":{"value":"manual-model","source":"manual"}},"reviewer":{"profile":"isolated"}});
    let request = Request::builder()
        .method("POST")
        .uri("/api/tasks")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"key":"transport-choice","job":selected}).to_string(),
        ))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let task: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let response=relay_app::mcp::handle(&app,json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"relay_submit","arguments":{"key":"transport-choice","job":selected}}})).unwrap();
    assert_eq!(response["result"]["isError"], false);
    let replay: Value =
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(replay["id"], task["id"]);
    let request = Request::builder()
        .uri("/api/resources?repository=repo&workflow=checked&reviewer_profile=isolated")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let estimate: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        estimate["initial_estimate"]["reviewer_copy_bytes"],
        estimate["initial_estimate"]["snapshot_bytes"]
    );
    let response=relay_app::mcp::handle(&app,json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"relay_resources","arguments":{"repository":"repo","workflow":"checked","reviewer_profile":"dev"}}})).unwrap();
    assert_eq!(response["result"]["isError"], true);
    let schema =
        relay_app::mcp::handle(&app, json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}))
            .unwrap();
    assert_eq!(
        schema["result"]["tools"][0]["inputSchema"]["properties"]["job"]["properties"]["role_selections"]
            ["properties"]["reviewer"]["properties"]["profile"]["type"],
        "string"
    );
    assert_eq!(app.list(None).unwrap().len(), 1);
}

fn expanded_job() -> Value {
    let mut selected = job();
    selected["role_selections"] = json!({"developer":{"profile":"dev","model":{"value":"manual-model","source":"manual"},"native_permission":"codex_full_access"}});
    selected
}
fn expanded_config(root: &Path) -> HostConfig {
    let mut config = config(root);
    let profile = config.native_agents.get_mut("dev").unwrap();
    profile.allowed_permission_modes = vec![NativePermission::CodexFullAccess];
    profile.env.insert(
        "PRIVATE_CREDENTIAL".into(),
        "never-persist-this-credential-value".into(),
    );
    config
}
fn challenge(app: &Application, selected: Value) -> Value {
    app.permission_challenge(serde_json::from_value(selected).unwrap())
        .unwrap()
}
fn confirmed_submit(
    app: &Application,
    key: &str,
    mut selected: Value,
    challenge: &Value,
) -> Result<relay::Task, Error> {
    selected["role_selections"]["developer"]["confirm_permission_expansion"] = json!(true);
    app.submit(
        serde_json::from_value(
            json!({"key":key,"job":selected,"permission_challenge":challenge["challenge"]}),
        )
        .unwrap(),
    )
}
#[test]
fn permission_challenge_is_exact_scoped_one_use_and_still_requires_attestation() {
    let temp = Fixture::new();
    let app =
        Application::open(temp.path().join("queue.db"), expanded_config(temp.path())).unwrap();
    let selected = expanded_job();
    let mut preview = selected.clone();
    preview["requirements"] = json!("");
    let issued = challenge(&app, preview);
    assert!(
        issued["confirmation_text"]
            .as_str()
            .unwrap()
            .contains("filesystem AND network")
    );
    assert_eq!(issued["scope"]["developer"]["model"], "manual-model");
    assert!(app.list(None).unwrap().is_empty());
    assert!(
        !issued
            .to_string()
            .contains("never-persist-this-credential-value")
    );
    let no_attestation=app.submit(serde_json::from_value(json!({"key":"missing-attestation","job":selected,"permission_challenge":issued["challenge"]})).unwrap());
    assert!(
        no_attestation
            .unwrap_err()
            .to_string()
            .contains("explicit confirmation")
    );
    let mut no_challenge = selected.clone();
    no_challenge["role_selections"]["developer"]["confirm_permission_expansion"] = json!(true);
    assert!(
        submit(&app, "missing-challenge", no_challenge)
            .unwrap_err()
            .to_string()
            .contains("server-issued challenge")
    );
    let mut changed = selected.clone();
    changed["role_selections"]["developer"]["model"]["value"] = json!("different-model");
    assert!(
        confirmed_submit(&app, "changed", changed, &issued)
            .unwrap_err()
            .to_string()
            .contains("no longer matches")
    );
    let task = confirmed_submit(&app, "accepted", selected.clone(), &issued).unwrap();
    assert!(!task.payload.contains("never-persist-this-credential-value"));
    let accepted: Value = serde_json::from_str(&task.payload).unwrap();
    assert_eq!(
        accepted["role_binding"]["developer"]["model"],
        "manual-model"
    );
    assert_eq!(
        accepted["role_binding"]["developer"]["native_permission"],
        "codex_full_access"
    );
    assert!(confirmed_submit(&app, "second-key", selected.clone(), &issued).is_err());
    assert_eq!(
        confirmed_submit(&app, "accepted", selected, &issued)
            .unwrap()
            .id,
        task.id
    );
}
#[test]
fn accepted_replay_survives_restart_and_policy_drift_but_launch_is_rejected() {
    let temp = Fixture::new();
    let db = temp.path().join("queue.db");
    let config = expanded_config(temp.path());
    let app = Application::open(&db, config.clone()).unwrap();
    let selected = expanded_job();
    let issued = challenge(&app, selected.clone());
    let task = confirmed_submit(&app, "accepted", selected.clone(), &issued).unwrap();
    drop(app);
    let mut changed = config;
    changed.native_agents.get_mut("dev").unwrap().model = Some("changed-profile-default".into());
    let app = Application::open(&db, changed).unwrap();
    assert_eq!(
        confirmed_submit(&app, "accepted", selected.clone(), &issued)
            .unwrap()
            .id,
        task.id
    );
    assert!(confirmed_submit(&app, "new-after-restart", selected, &issued).is_err());
    assert!(app.work_once().unwrap());
    let finished = app.get_view(task.id).unwrap().task;
    let result: Value = serde_json::from_str(finished.result.as_deref().unwrap()).unwrap();
    assert_eq!(result["outcome"], "failure");
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("changed after acceptance"),
        "{result}"
    );
    assert!(result["workspace"].is_null());
    assert!(!temp.path().join("runs/task-1").exists());
}
#[test]
fn selected_jobs_bind_inherited_defaults_and_executable_identity_without_expansion() {
    let temp = Fixture::new();
    let db = temp.path().join("queue.db");
    let mut config = config(temp.path());
    let cli = temp.path().join("fake-native");
    executable(&cli, "#!/bin/sh\nexit 91\n");
    config.native_agents.get_mut("dev").unwrap().program = cli.clone();
    config.native_agents.get_mut("dev").unwrap().model = Some("original-default".into());
    config.native_agents.get_mut("dev").unwrap().effort = Some("host-effort".into());
    let app = Application::open(&db, config).unwrap();
    let mut selected = job();
    selected["role_selections"] = json!({"developer":{"profile":"dev"}});
    let task = submit(&app, "frozen", selected).unwrap();
    let payload: Value = serde_json::from_str(&task.payload).unwrap();
    assert_eq!(
        payload["role_binding"]["developer"]["model"],
        "original-default"
    );
    assert_eq!(
        payload["role_binding"]["developer"]["effort"],
        "host-effort"
    );
    assert!(
        submit(&app, "injected-binding", payload)
            .unwrap_err()
            .to_string()
            .contains("server-owned")
    );
    executable(&cli, "#!/bin/sh\n# binary was replaced\nexit 91\n");
    assert!(app.work_once().unwrap());
    let result: Value = serde_json::from_str(
        app.get_view(task.id)
            .unwrap()
            .task
            .result
            .as_deref()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["outcome"], "failure");
    assert!(result["workspace"].is_null());
}
#[tokio::test]
async fn permission_challenge_transport_requires_auth_and_mcp_returns_same_scoped_contract() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let temp = Fixture::new();
    let app =
        Application::open(temp.path().join("queue.db"), expanded_config(temp.path())).unwrap();
    let token = "role-challenge-token-00000000000000000";
    let router = relay_app::http::router(app.clone(), token.into()).unwrap();
    let mut selected = expanded_job();
    selected["requirements"] = json!("");
    for authenticated in [false, true] {
        let mut request = Request::builder()
            .method("POST")
            .uri("/api/permission-challenge")
            .header("content-type", "application/json");
        if authenticated {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = router
            .clone()
            .oneshot(
                request
                    .body(Body::from(json!({"job":selected}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if authenticated {
                StatusCode::OK
            } else {
                StatusCode::UNAUTHORIZED
            }
        );
        if authenticated {
            let result: Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(
                result["scope"]["developer"]["native_permission"],
                "codex_full_access"
            );
        }
    }
    let response=relay_app::mcp::handle(&app,json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"relay_permission_challenge","arguments":{"job":selected}}})).unwrap();
    assert_eq!(response["result"]["isError"], false);
    assert!(app.list(None).unwrap().is_empty());
}

#[test]
fn database_failure_does_not_consume_challenge_and_new_acknowledgement_conflicts() {
    let temp = Fixture::new();
    let db = temp.path().join("queue.db");
    let app = Application::open(&db, expanded_config(temp.path())).unwrap();
    let selected = expanded_job();
    let issued = challenge(&app, selected.clone());
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection.execute_batch("CREATE TRIGGER test_reject_submission BEFORE INSERT ON tasks BEGIN SELECT RAISE(ABORT, 'fixture transient failure'); END;").unwrap();
    assert!(confirmed_submit(&app, "same-key", selected.clone(), &issued).is_err());
    assert!(app.list(None).unwrap().is_empty());
    connection
        .execute_batch("DROP TRIGGER test_reject_submission;")
        .unwrap();
    let accepted = confirmed_submit(&app, "same-key", selected.clone(), &issued).unwrap();
    let unknown = json!({"challenge":"0".repeat(64)});
    assert!(matches!(
        confirmed_submit(&app, "same-key", selected.clone(), &unknown),
        Err(Error::Core(relay::Error::IdempotencyConflict))
    ));
    let mut no_token = selected.clone();
    no_token["role_selections"]["developer"]["confirm_permission_expansion"] = json!(true);
    assert!(matches!(
        submit(&app, "same-key", no_token),
        Err(Error::Core(relay::Error::IdempotencyConflict))
    ));
    let new_acknowledgement = challenge(&app, selected.clone());
    assert!(matches!(
        confirmed_submit(&app, "same-key", selected.clone(), &new_acknowledgement),
        Err(Error::Core(relay::Error::IdempotencyConflict))
    ));
    assert_eq!(
        confirmed_submit(&app, "same-key", selected, &issued)
            .unwrap()
            .id,
        accepted.id
    );
    assert_eq!(app.list(None).unwrap().len(), 1);
}

#[test]
fn changed_inherited_defaults_require_a_new_key_even_with_same_raw_role_request() {
    let temp = Fixture::new();
    let db = temp.path().join("queue.db");
    let mut config = expanded_config(temp.path());
    config.native_agents.get_mut("dev").unwrap().model = Some("original-default".into());
    let app = Application::open(&db, config.clone()).unwrap();
    let mut selected = expanded_job();
    selected["role_selections"]["developer"]
        .as_object_mut()
        .unwrap()
        .remove("model");
    let issued = challenge(&app, selected.clone());
    let accepted = confirmed_submit(&app, "same-key", selected.clone(), &issued).unwrap();
    drop(app);
    config.native_agents.get_mut("dev").unwrap().model = Some("new-default".into());
    let app = Application::open(&db, config).unwrap();
    let new_acknowledgement = challenge(&app, selected.clone());
    assert_eq!(
        new_acknowledgement["scope"]["developer"]["model"],
        "new-default"
    );
    assert!(matches!(
        confirmed_submit(&app, "same-key", selected.clone(), &new_acknowledgement),
        Err(Error::Core(relay::Error::IdempotencyConflict))
    ));
    assert_eq!(
        confirmed_submit(&app, "same-key", selected, &issued)
            .unwrap()
            .id,
        accepted.id
    );
    assert_eq!(app.list(None).unwrap().len(), 1);
}
