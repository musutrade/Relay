#![cfg(target_os = "linux")]
use relay::{State, Task};
use relay_app::host::{Host, HostConfig, Outcome, RunResult};
use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, atomic::AtomicBool};
use tempfile::TempDir;

const REVIEWER: &str = r#"#!/usr/bin/python3
import json, os, pathlib, subprocess, sys
if '--version' in sys.argv:
    print(os.environ.get('FAKE_VERSION', '2.1.259')); sys.exit()
if '--help' in sys.argv:
    print('--output-format --verbose --permission-prompts --restricted --tools --allowedTools --disallowedTools --disable-slash-commands --strict-mcp-config --mcp-config --no-session-persistence'); sys.exit()
prompt=sys.stdin.read()
assert '--restricted' in sys.argv and '--tools' in sys.argv and 'Read,Glob,Grep' in sys.argv
assert '--strict-mcp-config' in sys.argv and '--mcp-config' in sys.argv
candidate=os.environ['RELAY_CANDIDATE_SHA']
round=os.environ['RELAY_WORKFLOW_ROUND']
pathlib.Path('.git/review-'+round).write_text(candidate)
mode=os.environ.get('REVIEW_MODE','approve')
if mode == 'mutate': pathlib.Path('original.txt').write_text('reviewer changed source')
if mode == 'mutate_hidden':
    subprocess.run(['/usr/bin/git','update-index','--assume-unchanged','original.txt'],check=True)
    pathlib.Path('original.txt').write_text('hidden reviewer source change')
if mode == 'change_head':
    subprocess.run(['/usr/bin/git','-c','user.name=Fake','-c','user.email=fake@example.invalid','commit','--allow-empty','-m','unexpected'],check=True,stdout=sys.stderr)
verdict='changes_requested' if mode=='reject' or (mode=='repair' and round=='0') else 'approved'
summary={'candidate_sha':candidate,'verdict':verdict,'summary':'Checked exact candidate','findings':['Fix the fixture issue'] if verdict=='changes_requested' else []}
if mode=='wrong_sha': summary['candidate_sha']='f'*40
message=json.dumps(summary)
if mode=='malformed': message='looks good to me'
if mode=='truncated': message += ' '*5000
print(json.dumps({'type':'result','subtype':'success','is_error':False,'result':message,'permission_denials':[]}))
"#;
const DEVELOPER: &str = r#"#!/usr/bin/python3
import os, pathlib, sys
prompt=sys.stdin.read()
round=os.environ['RELAY_WORKFLOW_ROUND']
if int(round)>0: assert 'Fix the fixture issue' in prompt or 'failed its configured tests' in prompt
assert pathlib.Path(os.environ['RELAY_REQUIREMENTS_FILE']).read_text()==prompt
pathlib.Path('changed.txt').write_text('candidate '+round+'\n')
pathlib.Path('.git/developer-'+round).write_text(prompt)
print('implemented')
"#;
const TEST: &str = r#"#!/usr/bin/python3
import os, pathlib, subprocess, sys, time
sha=os.environ['RELAY_CANDIDATE_SHA']; round=os.environ['RELAY_WORKFLOW_ROUND']
actual=subprocess.check_output(['/usr/bin/git','rev-parse','HEAD'],text=True).strip()
assert actual==sha
committed=subprocess.check_output(['/usr/bin/git','show',sha+':changed.txt'],text=True)
assert committed==pathlib.Path('changed.txt').read_text()
pathlib.Path('.git/test-'+round).write_text(sha)
mode=os.environ.get('TEST_MODE','pass')
if mode=='mutate': pathlib.Path('original.txt').write_text('test mutation')
if mode=='stage':
    pathlib.Path('original.txt').write_text('staged mutation')
    subprocess.run(['/usr/bin/git','add','original.txt'],check=True)
    pathlib.Path('original.txt').write_text('original\n')
if mode=='new_file': pathlib.Path('uncommitted.txt').write_text('new')
if mode=='hidden':
    subprocess.run(['/usr/bin/git','update-index','--skip-worktree','original.txt'],check=True)
    pathlib.Path('original.txt').write_text('hidden mutation')
if mode=='filter':
    subprocess.run(['/usr/bin/git','config','filter.freeze.clean','/usr/bin/git show HEAD:original.txt'],check=True)
    pathlib.Path('.git/info').mkdir(exist_ok=True)
    pathlib.Path('.git/info/attributes').write_text('original.txt filter=freeze\n')
    pathlib.Path('original.txt').write_text('filter-hidden mutation')
if mode=='sleep': time.sleep(60)
if mode=='fail' or (mode=='repair' and round=='0'): print('fixture assertion failed',file=sys.stderr); sys.exit(7)
print('passed')
"#;
const PUBLISH: &str = r#"#!/usr/bin/python3
import json, os, pathlib
sha=os.environ['RELAY_CANDIDATE_SHA']
assert sha==os.environ['RELAY_REVIEWED_SHA']
assert os.environ['RELAY_REVIEW_VERDICT']=='approved'
assert os.environ['RELAY_TEST_OUTCOME']=='success'
assert os.environ['RELAY_GITHUB_REPOSITORY']=='example/project'
pathlib.Path('.git/published').write_text(sha)
print(json.dumps({'dry_run':True,'draft':True,'repository':'example/project','branch':'relay/task-'+os.environ['RELAY_TASK_ID']+'-g'+os.environ['RELAY_GENERATION'],'candidate_sha':sha,'reconciliation_required':False}))
"#;
struct Fixture {
    _temp: TempDir,
    config: HostConfig,
    source: PathBuf,
}
fn write_executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
fn git(path: &Path, args: &[&str]) -> String {
    let result = Command::new("/usr/bin/git")
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
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().to_owned()
}
impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("original.txt"), "original\n").unwrap();
        fs::write(source.join(".gitignore"), "target/\nignored-secret.txt\n").unwrap();
        git(&source, &["init", "--initial-branch=main"]);
        git(&source, &["add", "."]);
        git(&source, &["commit", "-m", "base"]);
        let reviewer = temp.path().join("reviewer");
        let developer = temp.path().join("developer");
        let test = temp.path().join("test");
        let publish = temp.path().join("publish");
        write_executable(&reviewer, REVIEWER);
        write_executable(&developer, DEVELOPER);
        write_executable(&test, TEST);
        write_executable(&publish, PUBLISH);
        let config=serde_json::from_value(json!({
            "workspace_root":temp.path().join("runs"),"repositories":{"fixture":source},
            "agents":{"developer":{"program":developer}},
            "native_agents":{"reviewer":{"provider":"claude_cli","program":reviewer}},
            "tests":{"check":{"program":test}},"draft_pr_adapters":{"publish":{"program":publish}},
            "workflows":{"checked":{"repository":"fixture","developer":"developer","reviewer":"reviewer","test":"check","draft_pr_adapter":"publish","github_repository":"example/project"}},
            "timeout_seconds":20,"output_limit_bytes":128,
            "supervisor_program":env!("CARGO_BIN_EXE_relay-app")
        })).unwrap();
        Self {
            _temp: temp,
            config,
            source,
        }
    }
    fn task(&self, publish: bool) -> Task {
        Task { id:1,key:"fixture".into(),generation:1,state:State::Claimed,owner:Some("host".into()),result:None,
            payload:json!({"repository":"fixture","agent":"developer","requirements":"Implement the fixture","workflow":"checked","publish":publish,"draft_pr_adapter":if publish {Some("publish")} else {None}}).to_string() }
    }
    fn run(&self, publish: bool) -> RunResult {
        Host::new(self.config.clone())
            .unwrap()
            .execute(&self.task(publish), Arc::new(AtomicBool::new(false)))
    }
    fn repository(&self) -> PathBuf {
        self.config
            .workspace_root
            .join("task-1-generation-1/repository")
    }
    fn review(&mut self, mode: &str) {
        self.config
            .native_agents
            .get_mut("reviewer")
            .unwrap()
            .env
            .insert("REVIEW_MODE".into(), mode.into());
    }
    fn test(&mut self, mode: &str) {
        self.config
            .tests
            .get_mut("check")
            .unwrap()
            .env
            .insert("TEST_MODE".into(), mode.into());
    }
    fn repairs(&mut self, count: u8) {
        self.config
            .workflows
            .get_mut("checked")
            .unwrap()
            .max_repairs = count;
    }
}
#[test]
fn commits_then_tests_reviews_and_publishes_one_exact_candidate() {
    let mut f = Fixture::new();
    f.config.output_limit_bytes = 1;
    fs::write(f.source.join("ignored-secret.txt"), "must not copy").unwrap();
    let base = git(&f.source, &["rev-parse", "HEAD"]);
    let result = f.run(true);
    assert_eq!(result.outcome, Outcome::Success, "{}", result.to_json());
    let workflow = result.workflow.as_ref().unwrap();
    let candidate = workflow.candidate_sha.as_ref().unwrap();
    assert_eq!(workflow.base_sha.as_deref(), Some(base.as_str()));
    assert_eq!(workflow.reviewed_sha.as_ref(), Some(candidate));
    assert_eq!(workflow.rounds.len(), 1);
    assert_eq!(git(&f.repository(), &["rev-parse", "HEAD"]), *candidate);
    for phase in ["test-0", "review-0", "published"] {
        assert_eq!(
            fs::read_to_string(f.repository().join(".git").join(phase)).unwrap(),
            *candidate
        );
    }
    assert!(!f.repository().join("ignored-secret.txt").exists());
    assert!(!f.source.join("changed.txt").exists());
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"]), base);
}
#[test]
fn changes_requested_triggers_a_new_tested_and_reviewed_commit() {
    let mut f = Fixture::new();
    f.review("repair");
    f.repairs(1);
    let result = f.run(true);
    assert_eq!(result.outcome, Outcome::Success, "{}", result.to_json());
    let workflow = result.workflow.unwrap();
    assert_eq!(workflow.rounds.len(), 2);
    assert_ne!(
        workflow.rounds[0].candidate_sha,
        workflow.rounds[1].candidate_sha
    );
    for round in [0, 1] {
        assert_eq!(
            fs::read_to_string(f.repository().join(format!(".git/test-{round}"))).unwrap(),
            workflow.rounds[round].candidate_sha
        );
    }
    assert_eq!(
        workflow.reviewed_sha,
        Some(workflow.rounds[1].candidate_sha.clone())
    );
}
#[test]
fn ordinary_test_failure_is_the_other_bounded_repair_trigger() {
    let mut f = Fixture::new();
    f.test("repair");
    f.repairs(1);
    let result = f.run(false);
    assert_eq!(result.outcome, Outcome::Success, "{}", result.to_json());
    assert_eq!(result.workflow.unwrap().rounds.len(), 2);
    assert!(!f.repository().join(".git/review-0").exists());
    assert!(f.repository().join(".git/review-1").exists());
    assert!(!f.repository().join(".git/published").exists());
}
#[test]
fn exhausted_repairs_never_publish() {
    let mut f = Fixture::new();
    f.review("reject");
    f.repairs(1);
    let result = f.run(true);
    assert_eq!(result.outcome, Outcome::Failure, "{}", result.to_json());
    assert_eq!(result.workflow.unwrap().rounds.len(), 2);
    assert!(!f.repository().join(".git/published").exists());
}
#[test]
fn malformed_wrong_or_truncated_review_is_not_a_repair_request() {
    for mode in ["malformed", "wrong_sha", "truncated"] {
        let mut f = Fixture::new();
        f.review(mode);
        f.repairs(3);
        let result = f.run(true);
        assert_eq!(
            result.outcome,
            Outcome::Failure,
            "{mode}: {}",
            result.to_json()
        );
        assert_eq!(result.workflow.unwrap().rounds.len(), 1);
        assert!(!f.repository().join(".git/developer-1").exists());
        assert!(!f.repository().join(".git/published").exists());
    }
}
#[test]
fn tests_cannot_mutate_files_index_or_hide_changes_using_index_flags() {
    for mode in ["mutate", "stage", "new_file", "hidden", "filter"] {
        let mut f = Fixture::new();
        f.test(mode);
        f.repairs(3);
        let result = f.run(true);
        assert_eq!(
            result.outcome,
            Outcome::Failure,
            "{mode}: {}",
            result.to_json()
        );
        assert!(!f.repository().join(".git/review-0").exists());
        assert!(!f.repository().join(".git/developer-1").exists());
        assert!(!f.repository().join(".git/published").exists());
    }
}
#[test]
fn reviewer_mutations_or_changed_head_never_approve() {
    for mode in ["mutate", "mutate_hidden", "change_head"] {
        let mut f = Fixture::new();
        f.review(mode);
        f.repairs(3);
        let result = f.run(true);
        assert_eq!(
            result.outcome,
            Outcome::Failure,
            "{mode}: {}",
            result.to_json()
        );
        assert_eq!(result.workflow.unwrap().reviewed_sha, None);
        assert!(!f.repository().join(".git/developer-1").exists());
        assert!(!f.repository().join(".git/published").exists());
    }
}
#[test]
fn unsupported_reviewer_fails_before_developer_and_dirty_source_is_rejected() {
    let mut f = Fixture::new();
    f.config
        .native_agents
        .get_mut("reviewer")
        .unwrap()
        .env
        .insert("FAKE_VERSION".into(), "2.1.100".into());
    let result = f.run(false);
    assert_eq!(result.outcome, Outcome::Failure);
    assert!(!f.repository().join(".git/developer-0").exists());
    let f = Fixture::new();
    fs::write(f.source.join("original.txt"), "dirty").unwrap();
    let result = f.run(false);
    assert_eq!(result.outcome, Outcome::Failure);
    assert!(!f.repository().join(".git/developer-0").exists());
}
#[test]
fn workflow_selectors_and_unsafe_reviewers_are_rejected_at_admission() {
    let mut f = Fixture::new();
    f.config.workflows.get_mut("checked").unwrap().max_repairs = 4;
    assert!(Host::new(f.config.clone()).is_err());
    f.repairs(0);
    f.config.workflows.get_mut("checked").unwrap().reviewer = "developer".into();
    assert!(Host::new(f.config.clone()).is_err());
    let f = Fixture::new();
    let mut job: serde_json::Value = serde_json::from_str(&f.task(false).payload).unwrap();
    job["agent"] = "reviewer".into();
    assert!(relay_app::host::Job::from_payload(&job.to_string(), &f.config).is_err());
    job["agent"] = "developer".into();
    job["workflow"] = "unknown".into();
    assert!(relay_app::host::Job::from_payload(&job.to_string(), &f.config).is_err());
}
#[test]
fn missing_publisher_terminal_is_ambiguous_but_stopped_process_is_not_unknown() {
    let mut f = Fixture::new();
    let adapter = f.config.draft_pr_adapters.get_mut("publish").unwrap();
    adapter.program = PathBuf::from("/bin/true");
    let result = f.run(true);
    assert_eq!(result.outcome, Outcome::Failure, "{}", result.to_json());
    assert!(result.workflow.unwrap().reconciliation_required);
}
#[test]
fn shared_deadline_stops_tests_without_repair_or_review() {
    let mut f = Fixture::new();
    f.test("sleep");
    f.repairs(3);
    f.config.timeout_seconds = 3;
    let result = f.run(true);
    assert_eq!(result.outcome, Outcome::TimedOut, "{}", result.to_json());
    assert!(!f.repository().join(".git/review-0").exists());
    assert!(!f.repository().join(".git/developer-1").exists());
}
#[test]
fn git_metadata_and_control_files_are_inside_the_running_workspace_budget() {
    let mut f = Fixture::new();
    f.config.max_snapshot_bytes = 1024;
    let result = f.run(false);
    assert_eq!(result.outcome, Outcome::Failure, "{}", result.to_json());
    assert!(!f.repository().join(".git/developer-0").exists());
}

#[test]
fn approved_empty_candidate_is_local_success_but_never_published() {
    for publish in [false, true] {
        let mut f = Fixture::new();
        f.config.agents.get_mut("developer").unwrap().program = PathBuf::from("/bin/true");
        f.config.tests.get_mut("check").unwrap().program = PathBuf::from("/bin/true");
        let result = f.run(publish);
        assert_eq!(
            result.outcome,
            if publish {
                Outcome::Failure
            } else {
                Outcome::Success
            },
            "{}",
            result.to_json()
        );
        assert!(result.workflow.unwrap().reviewed_sha.is_some());
        assert!(!f.repository().join(".git/published").exists());
        if publish {
            assert!(result.error.unwrap().contains("no changes to publish"));
        }
    }
}
#[test]
fn persisted_evidence_shrinks_but_retains_candidate_and_publication_metadata() {
    let f = Fixture::new();
    let mut result = f.run(true);
    assert_eq!(result.outcome, Outcome::Success, "{}", result.to_json());
    let workflow = result.workflow.as_mut().unwrap();
    workflow.rounds = vec![workflow.rounds[0].clone(); 4];
    for round in &mut workflow.rounds {
        round.developer.summary = "\u{1}".repeat(512);
        round.tests.as_mut().unwrap().summary = "\u{2}".repeat(512);
        round.reviewer.as_mut().unwrap().summary = "\u{3}".repeat(512);
        round.review.as_mut().unwrap().summary = "\u{4}".repeat(512);
    }
    let sha = workflow.candidate_sha.clone();
    let publication = workflow.publication.clone().unwrap();
    let serialized = result.to_json();
    assert!(serialized.len() <= relay::MAX_RESULT_BYTES);
    let restored: RunResult = serde_json::from_str(&serialized).unwrap();
    let workflow = restored.workflow.unwrap();
    assert!(workflow.evidence_truncated);
    assert_eq!(workflow.candidate_sha, sha);
    assert_eq!(
        workflow.publication.unwrap().candidate_sha,
        publication.candidate_sha
    );
    assert_eq!(workflow.rounds.len(), 4);
}
#[test]
fn absent_workflow_preserves_legacy_job_canonical_payload() {
    let f = Fixture::new();
    let input = json!({"repository":"fixture","requirements":"Change","agent":"developer","test":null,"publish":false,"draft_pr_adapter":null});
    let job = relay_app::host::Job::from_payload(&input.to_string(), &f.config).unwrap();
    assert_eq!(serde_json::to_value(job).unwrap(), input);
}

const TOKEN: &str = "workflow-test-token-000000000000000";
fn real_adapter_fixture() -> Fixture {
    let mut f = Fixture::new();
    f.review("repair");
    f.repairs(1);
    let adapter = f.config.draft_pr_adapters.get_mut("publish").unwrap();
    adapter.program = PathBuf::from("/usr/bin/python3");
    adapter.args = vec![
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../examples/github-draft-pr.py")
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    ];
    adapter
        .env
        .insert("RELAY_GITHUB_EXECUTE".into(), "0".into());
    f
}
async fn http_call(
    router: axum::Router,
    method: &str,
    path: &str,
    value: serde_json::Value,
    authenticated: bool,
) -> (axum::http::StatusCode, serde_json::Value) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if authenticated {
        builder = builder.header("authorization", format!("Bearer {TOKEN}"));
    }
    let response = router
        .oneshot(
            builder
                .body(axum::body::Body::from(value.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
fn mcp_call(
    app: &relay_app::Application,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let request = format!(
        "{}\n",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}})
    );
    let mut output = Vec::new();
    relay_app::mcp::serve(app, std::io::Cursor::new(request), &mut output).unwrap();
    let response: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(response["result"]["isError"], false, "{response}");
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}
fn assert_transport_result(task: &serde_json::Value) {
    assert_eq!(task["state"], "finished");
    let result: RunResult = serde_json::from_str(task["result"].as_str().unwrap()).unwrap();
    assert_eq!(result.outcome, Outcome::Success, "{}", result.to_json());
    let workflow = result.workflow.unwrap();
    assert_eq!(workflow.rounds.len(), 2);
    assert_ne!(
        workflow.rounds[0].candidate_sha,
        workflow.rounds[1].candidate_sha
    );
    assert_eq!(workflow.reviewed_sha, workflow.candidate_sha);
    let publication = workflow.publication.unwrap();
    assert!(publication.dry_run && publication.draft);
    assert_eq!(Some(publication.candidate_sha), workflow.candidate_sha);
    assert_eq!(publication.url, None);
}
#[tokio::test]
async fn authenticated_http_submission_runs_repair_and_real_adapter_dry_run_then_mcp_reads() {
    let f = real_adapter_fixture();
    let app =
        relay_app::Application::open(f._temp.path().join("relay.db"), f.config.clone()).unwrap();
    let router = relay_app::http::router(app.clone(), TOKEN.into()).unwrap();
    let job: serde_json::Value = serde_json::from_str(&f.task(true).payload).unwrap();
    let request = json!({"key":"http-workflow","job":job});
    assert_eq!(
        http_call(router.clone(), "POST", "/api/tasks", request.clone(), false)
            .await
            .0,
        axum::http::StatusCode::UNAUTHORIZED
    );
    let (status, _) = http_call(router, "POST", "/api/tasks", request, true).await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    assert!(app.work_once().unwrap());
    assert_transport_result(&mcp_call(&app, "relay_get", json!({"id":1})));
}
#[tokio::test]
async fn mcp_submission_runs_repair_and_real_adapter_dry_run_then_authenticated_http_reads() {
    let f = real_adapter_fixture();
    let app =
        relay_app::Application::open(f._temp.path().join("relay.db"), f.config.clone()).unwrap();
    let router = relay_app::http::router(app.clone(), TOKEN.into()).unwrap();
    let job: serde_json::Value = serde_json::from_str(&f.task(true).payload).unwrap();
    let queued = mcp_call(
        &app,
        "relay_submit",
        json!({"key":"mcp-workflow","job":job}),
    );
    assert_eq!(queued["state"], "queued");
    assert!(app.work_once().unwrap());
    let (status, task) =
        http_call(router, "GET", "/api/tasks/1", serde_json::Value::Null, true).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_transport_result(&task);
}

#[test]
fn developer_added_embedded_repository_is_rejected_before_tests() {
    let mut f = Fixture::new();
    let developer = f.config.agents.get_mut("developer").unwrap();
    developer.program = PathBuf::from("/bin/sh");
    developer.args = vec!["-c".into(), "mkdir nested && /usr/bin/git init --initial-branch=main nested >/dev/null && printf hidden > nested/file.txt && /usr/bin/git -C nested add . && /usr/bin/git -C nested -c user.name=Fixture -c user.email=fixture@example.invalid commit -m nested >/dev/null".into()];
    let result = f.run(true);
    assert_eq!(result.outcome, Outcome::Failure, "{}", result.to_json());
    assert!(result.error.unwrap().contains("submodules are unsupported"));
    assert!(!f.repository().join(".git/test-0").exists());
    assert!(!f.repository().join(".git/published").exists());
}
#[test]
fn legacy_store_payload_retries_through_new_application_without_key_conflict() {
    let f = Fixture::new();
    let database = f._temp.path().join("legacy.db");
    // Exact pre-workflow canonical field order, including existing optional fields.
    let payload = "{\"repository\":\"fixture\",\"requirements\":\"Change\",\"agent\":\"developer\",\"test\":null,\"publish\":false,\"draft_pr_adapter\":null}";
    let task = relay::Store::open(&database)
        .unwrap()
        .submit("old-key", payload)
        .unwrap();
    let application = relay_app::Application::open(&database, f.config).unwrap();
    let retried = application.submit(serde_json::from_value(json!({"key":"old-key","job":serde_json::from_str::<serde_json::Value>(payload).unwrap()})).unwrap()).unwrap();
    assert_eq!(retried.id, task.id);
    assert_eq!(retried.payload, payload);
}
#[test]
fn publisher_success_requires_exact_sha_mode_draft_and_bounded_verified_url() {
    for mutation in ["wrong_sha", "wrong_mode", "not_draft", "giant_url"] {
        let mut f = Fixture::new();
        let script = f.config.draft_pr_adapters["publish"].program.clone();
        let body = match mutation {
            "wrong_sha" => PUBLISH.replace("'candidate_sha':sha", "'candidate_sha':'f'*40"),
            "wrong_mode" => PUBLISH.replace("'dry_run':True", "'dry_run':False"),
            "not_draft" => PUBLISH.replace("'draft':True", "'draft':False"),
            "giant_url" => {
                f.config
                    .draft_pr_adapters
                    .get_mut("publish")
                    .unwrap()
                    .env
                    .insert("RELAY_GITHUB_EXECUTE".into(), "1".into());
                PUBLISH.replace("'dry_run':True", "'dry_run':False").replace("'reconciliation_required':False", "'reconciliation_required':False,'url':'https://github.com/example/project/pull/'+'1'*10000")
            }
            _ => unreachable!(),
        };
        write_executable(&script, &body);
        let result = f.run(true);
        assert_eq!(
            result.outcome,
            Outcome::Failure,
            "{mutation}: {}",
            result.to_json()
        );
        let workflow = result.workflow.unwrap();
        assert!(workflow.reconciliation_required);
        assert!(workflow.publication.is_none());
    }
}
#[test]
fn executable_permission_check_uses_owner_execute_bit() {
    let mut f = Fixture::new();
    fs::write(f.source.join("executable.sh"), "#!/bin/sh\ntrue\n").unwrap();
    fs::set_permissions(
        f.source.join("executable.sh"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    git(&f.source, &["add", "."]);
    git(&f.source, &["commit", "-m", "executable fixture"]);
    let test = f.config.tests.get_mut("check").unwrap();
    test.program = PathBuf::from("/bin/sh");
    test.args = vec!["-c".into(), "chmod 645 executable.sh".into()];
    let result = f.run(true);
    assert_eq!(result.outcome, Outcome::Failure, "{}", result.to_json());
    assert!(result.error.unwrap().contains("executable mode changed"));
    assert!(!f.repository().join(".git/review-0").exists());
}

#[test]
fn developer_cannot_redirect_git_common_directory_or_object_store_into_source() {
    for redirect in [
        "commondir",
        "objects/info/alternates",
        "objects/info/http-alternates",
    ] {
        let mut f = Fixture::new();
        let base = git(&f.source, &["rev-parse", "HEAD"]);
        let developer = f.config.agents.get_mut("developer").unwrap();
        developer.program = PathBuf::from("/usr/bin/python3");
        developer.env.insert(
            "FIXTURE_SOURCE".into(),
            f.source.to_string_lossy().into_owned(),
        );
        developer.env.insert("REDIRECT".into(), redirect.into());
        developer.args = vec!["-c".into(), "import os,pathlib; p=pathlib.Path('.git')/os.environ['REDIRECT']; p.parent.mkdir(parents=True,exist_ok=True); p.write_text(os.environ['FIXTURE_SOURCE']+'/.git\\n'); pathlib.Path('.git/HEAD').write_text('ref: refs/heads/main\\n'); pathlib.Path('changed.txt').write_text('must not commit to source')".into()];
        let result = f.run(true);
        assert_eq!(
            result.outcome,
            Outcome::Failure,
            "{redirect}: {}",
            result.to_json()
        );
        assert!(result.error.unwrap().contains("redirect"));
        assert_eq!(git(&f.source, &["rev-parse", "HEAD"]), base);
        assert!(!f.source.join("changed.txt").exists());
        assert!(!f.repository().join(".git/test-0").exists());
    }
}
#[test]
fn cancellation_during_workflow_tests_never_repairs_reviews_or_publishes() {
    let mut f = Fixture::new();
    f.test("sleep");
    f.repairs(3);
    let host = Host::new(f.config.clone()).unwrap();
    let task = f.task(true);
    let cancellation = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancellation);
    let thread = std::thread::spawn(move || host.execute(&task, flag));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !f.repository().join(".git/test-0").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "test phase did not start"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    cancellation.store(true, std::sync::atomic::Ordering::Release);
    let result = thread.join().unwrap();
    assert_eq!(result.outcome, Outcome::Cancelled, "{}", result.to_json());
    assert!(!f.repository().join(".git/developer-1").exists());
    assert!(!f.repository().join(".git/review-0").exists());
    assert!(!f.repository().join(".git/published").exists());
}
#[test]
fn unknown_supervisor_stops_workflow_without_retry() {
    let mut f = Fixture::new();
    f.repairs(3);
    f.config.supervisor_program = Some(PathBuf::from("/bin/true"));
    let result = f.run(true);
    assert_eq!(result.outcome, Outcome::Unknown, "{}", result.to_json());
    assert!(result.workflow.unwrap().rounds.is_empty());
    assert!(!f.repository().join(".git/developer-0").exists());
}

#[test]
fn developer_cannot_replace_repository_root_with_source_symlink() {
    let mut f = Fixture::new();
    let base = git(&f.source, &["rev-parse", "HEAD"]);
    let developer = f.config.agents.get_mut("developer").unwrap();
    developer.program = PathBuf::from("/usr/bin/python3");
    developer.env.insert(
        "FIXTURE_SOURCE".into(),
        f.source.to_string_lossy().into_owned(),
    );
    developer.args = vec!["-c".into(), "import os,pathlib; root=pathlib.Path(os.environ['RELAY_WORKSPACE']); root.rename(root.with_name('original-repository')); root.symlink_to(os.environ['FIXTURE_SOURCE'],target_is_directory=True)".into()];
    let result = f.run(true);
    assert_eq!(result.outcome, Outcome::Failure, "{}", result.to_json());
    assert!(result.error.unwrap().contains("redirected"));
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"]), base);
    assert!(!f.source.join("changed.txt").exists());
}
