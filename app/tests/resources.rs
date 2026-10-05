#![cfg(target_os = "linux")]

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use relay_app::{
    Application, RetryRequest, Submission,
    host::{HostConfig, Job, Outcome, RunResult},
    http, mcp,
};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Barrier, Mutex, MutexGuard},
    time::{Duration, Instant},
};
use tempfile::TempDir;
use tower::ServiceExt;

const KIB: u64 = 1024;
const LOW: u64 = 128 * KIB;
const CAP: u64 = 1024 * KIB;
const TOKEN: &str = "resource-fixture-token-00000000000000";
static FIXTURES: Mutex<()> = Mutex::new(());

// Every execution is a local deterministic fixture. Serialize fixture lifetimes
// so unrelated forks cannot transiently inherit another workspace's flock.
struct Fixture {
    root: TempDir,
    config: HostConfig,
    _serial: MutexGuard<'static, ()>,
}
impl Fixture {
    fn new(script: &str) -> Self {
        let serial = FIXTURES.lock().unwrap_or_else(|error| error.into_inner());
        let root = TempDir::new().unwrap();
        let source = root.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("original.txt"), "original\n").unwrap();
        let config = serde_json::from_value(json!({
            "workspace_root":root.path().join("runs"),"repositories":{"repo":source},
            "agents":{"fake":{"program":"/usr/bin/python3","args":["-c",script]}},
            "max_snapshot_bytes":64 * KIB,"max_workspace_bytes":CAP,
            "max_retained_workspaces":1,"timeout_seconds":10,
            "supervisor_program":env!("CARGO_BIN_EXE_relay-app")
        }))
        .unwrap();
        Self {
            root,
            config,
            _serial: serial,
        }
    }
    fn db(&self) -> PathBuf {
        self.root.path().join("db")
    }
    fn open(&self) -> Arc<Application> {
        Application::open(self.db(), self.config.clone()).unwrap()
    }
    fn input(&self, quota: Option<u64>) -> Submission {
        serde_json::from_value(json!({"key":"first","job":{"repository":"repo","requirements":"Preserve all dirty work","agent":"fake","workspace_quota_bytes":quota}})).unwrap()
    }
    fn failed(&self, quota: Option<u64>) -> (Arc<Application>, RunResult) {
        let app = self.open();
        app.submit(self.input(quota)).unwrap();
        assert!(app.work_once().unwrap());
        let result = read_result(&app, 1);
        assert_eq!(result.outcome, Outcome::Failure, "{}", result.to_json());
        (app, result)
    }
}
fn retry(key: &str, quota: Option<u64>) -> RetryRequest {
    RetryRequest {
        key: key.into(),
        confirm_stopped_and_reconciled: true,
        workspace_quota_bytes: quota,
    }
}
fn read_result(app: &Application, id: i64) -> RunResult {
    serde_json::from_str(app.get(id).unwrap().result.as_deref().unwrap()).unwrap()
}
fn read_json(path: impl AsRef<Path>) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
fn write_json(path: impl AsRef<Path>, value: &Value) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}
fn sparse(path: impl AsRef<Path>, bytes: u64) {
    fs::File::create(path).unwrap().set_len(bytes).unwrap();
}
fn assert_no_actions(app: &Application, id: i64) {
    let view = app.operator(id).unwrap();
    assert_eq!(view["recovery"]["actions"], json!([]), "{view}");
    assert!(view["recovery"]["blocked_reason"].is_string(), "{view}");
}
fn quota_code(result: &RunResult, expected: &str) {
    assert_eq!(
        result.failure.as_ref().map(|failure| failure.code.as_str()),
        Some(expected),
        "{}",
        result.to_json()
    );
}
const GROW_ONCE: &str = r#"
import pathlib
root = pathlib.Path('.')
marker = root / '.git/attempted'
if not marker.exists():
    marker.write_text('first')
    (root / 'original.txt').write_text('dirty original\n')
    (root / 'uncommitted.txt').write_text('keep this uncommitted file\n')
    with (root / 'sparse.bin').open('wb') as output: output.truncate(256 * 1024)
else:
    assert (root / 'original.txt').read_text() == 'dirty original\n'
    assert (root / 'uncommitted.txt').read_text() == 'keep this uncommitted file\n'
    assert (root / 'sparse.bin').stat().st_size == 256 * 1024
    (root / 'continued.txt').write_text('continued in place\n')
"#;

#[test]
fn sparse_logical_bytes_fail_low_attempt_then_explicit_increase_reuses_exact_directory() {
    let f = Fixture::new(GROW_ONCE);
    let (app, first) = f.failed(Some(LOW));
    quota_code(&first, "workspace_quota_exceeded");
    let root = first.workspace.unwrap();
    let original_result = app.get(1).unwrap().result;
    let sparse_metadata = fs::metadata(root.join("repository/sparse.bin")).unwrap();
    assert_eq!(sparse_metadata.len(), 256 * KIB);
    assert!(sparse_metadata.blocks() * 512 < sparse_metadata.len());
    let inode = fs::metadata(&root).unwrap().ino();
    let view = app.operator(1).unwrap();
    assert_eq!(view["resources"]["quota_bytes"], LOW);
    assert_eq!(view["resources"]["host_policy_cap_bytes"], CAP);
    assert_eq!(view["resources"]["usage"]["complete"], true);
    assert!(
        view["resources"]["usage"]["logical_bytes"]
            .as_u64()
            .unwrap()
            >= 256 * KIB
    );
    assert_eq!(
        view["recovery"]["actions"][0]["quota_increase_allowed"],
        true
    );
    assert!(app.retry(1, retry("implicit", None)).is_err());
    let next = app.retry(1, retry("explicit", Some(512 * KIB))).unwrap();
    assert!(app.work_once().unwrap());
    let completed = read_result(&app, next.id);
    assert_eq!(
        completed.outcome,
        Outcome::Success,
        "{}",
        completed.to_json()
    );
    assert_eq!(completed.workspace.as_ref(), Some(&root));
    assert_eq!(fs::metadata(&root).unwrap().ino(), inode);
    assert_eq!(
        fs::read_to_string(root.join("repository/uncommitted.txt")).unwrap(),
        "keep this uncommitted file\n"
    );
    assert_eq!(
        fs::read_to_string(f.root.path().join("source/original.txt")).unwrap(),
        "original\n"
    );
    assert_eq!(fs::read_dir(&app.config.workspace_root).unwrap().count(), 1);
    assert_eq!(app.get(1).unwrap().result, original_result);
    assert_eq!(
        app.operator(1).unwrap()["recovery"]["successor_id"],
        next.id
    );
    assert_eq!(completed.resources.unwrap().quota_bytes, Some(512 * KIB));
}

#[test]
fn chained_increases_survive_restart_and_prove_each_immediate_predecessor() {
    let f = Fixture::new(
        r#"
import pathlib
marker = pathlib.Path('.git/attempt-number')
n = int(marker.read_text()) + 1 if marker.exists() else 1
marker.write_text(str(n))
if n == 1: pathlib.Path('dirty.txt').write_text('preserve me')
assert pathlib.Path('dirty.txt').read_text() == 'preserve me'
if n < 3:
    with pathlib.Path('sparse.bin').open('wb') as output: output.truncate(n * 256 * 1024)
else: pathlib.Path('finished.txt').write_text('done')
"#,
    );
    let (app, first) = f.failed(Some(LOW));
    let root = first.workspace.unwrap();
    let original = app.get(1).unwrap().result;
    drop(app);
    let app = f.open();
    let second = app.retry(1, retry("second", Some(384 * KIB))).unwrap();
    let second_payload: Value = serde_json::from_str(&second.payload).unwrap();
    assert_eq!(
        second_payload["continuation"]["quota_increase"],
        json!({"previous_bytes":LOW,"new_bytes":384 * KIB})
    );
    assert!(app.work_once().unwrap());
    quota_code(&read_result(&app, second.id), "workspace_quota_exceeded");
    let second_result = app.get(second.id).unwrap().result;
    drop(app);
    let app = f.open();
    let third = app
        .retry(second.id, retry("third", Some(768 * KIB)))
        .unwrap();
    let third_payload: Value = serde_json::from_str(&third.payload).unwrap();
    assert_eq!(third_payload["continuation"]["workspace_task_id"], 1);
    assert_eq!(
        third_payload["continuation"]["predecessor_task_id"],
        second.id
    );
    assert_eq!(
        third_payload["continuation"]["quota_increase"],
        json!({"previous_bytes":384 * KIB,"new_bytes":768 * KIB})
    );
    assert!(app.work_once().unwrap());
    let completed = read_result(&app, third.id);
    assert_eq!(
        completed.outcome,
        Outcome::Success,
        "{}",
        completed.to_json()
    );
    assert_eq!(completed.workspace.as_ref(), Some(&root));
    assert_eq!(app.get(1).unwrap().result, original);
    assert_eq!(app.get(second.id).unwrap().result, second_result);
    assert_eq!(read_json(root.join("claim.json"))["quota_bytes"], 768 * KIB);
}

#[test]
fn racing_connections_freeze_one_successor_and_its_first_quota() {
    let f = Fixture::new(GROW_ONCE);
    let (app, _) = f.failed(Some(LOW));
    let other = f.open();
    let barrier = Arc::new(Barrier::new(2));
    let left = {
        let app = app.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            app.retry(1, retry("left", Some(512 * KIB))).unwrap()
        })
    };
    let right = std::thread::spawn(move || {
        barrier.wait();
        other.retry(1, retry("right", Some(768 * KIB))).unwrap()
    });
    let winner = left.join().unwrap();
    assert_eq!(winner, right.join().unwrap());
    let payload: Value = serde_json::from_str(&winner.payload).unwrap();
    assert!([512 * KIB, 768 * KIB].contains(&payload["workspace_quota_bytes"].as_u64().unwrap()));
    assert_eq!(app.list(None).unwrap().len(), 2);
    drop(app);
    let app = f.open();
    assert_eq!(
        app.retry(1, retry("third-click", Some(CAP))).unwrap(),
        winner
    );
    assert_eq!(app.get(winner.id).unwrap().payload, winner.payload);
}

#[test]
fn invalid_increases_do_not_reserve_or_change_a_task() {
    let f = Fixture::new(GROW_ONCE);
    let (app, _) = f.failed(Some(LOW));
    let original = app.get(1).unwrap();
    for quota in [0, LOW - 1, LOW, 192 * KIB, CAP + 1] {
        assert!(
            app.retry(1, retry("invalid", Some(quota))).is_err(),
            "accepted {quota}"
        );
        assert_eq!(app.list(None).unwrap().len(), 1);
        assert!(app.get_view(1).unwrap().continuation_status.is_none());
    }
    assert_eq!(app.get(1).unwrap(), original);
    assert!(serde_json::from_str::<Submission>(r#"{"key":"overflow","job":{"repository":"repo","requirements":"test","agent":"fake","workspace_quota_bytes":18446744073709551616}}"#).is_err());
    for quota in [
        json!(0),
        json!(-1),
        json!(1.5),
        json!(CAP + 1),
        json!(u64::MAX),
    ] {
        let mut value = json!({"key":"invalid-submit","job":{"repository":"repo","requirements":"test","agent":"fake"}});
        value["job"]["workspace_quota_bytes"] = quota;
        if let Ok(input) = serde_json::from_value::<Submission>(value) {
            assert!(app.submit(input).is_err());
        }
    }
}

#[test]
fn default_uses_host_ceiling_with_no_hidden_increase_headroom() {
    let f = Fixture::new("raise SystemExit(7)");
    let (app, _) = f.failed(None);
    let view = app.operator(1).unwrap();
    assert_eq!(view["resources"]["quota_bytes"], CAP);
    assert_eq!(view["recovery"]["actions"][0]["id"], "retry");
    assert_eq!(
        view["recovery"]["actions"][0]["quota_increase_allowed"],
        false
    );
    assert!(view["recovery"]["actions"][0]["min_quota_bytes"].is_null());
    assert!(app.retry(1, retry("over-policy", Some(CAP + 1))).is_err());
}

#[test]
fn missing_partial_stale_and_publication_evidence_offer_no_server_actions() {
    for scenario in ["missing", "incomplete", "owner", "publication"] {
        let mut f = Fixture::new(if scenario == "incomplete" {
            GROW_ONCE
        } else {
            "raise SystemExit(7)"
        });
        f.config.max_snapshot_entries = 128;
        let (app, result) = f.failed(Some(LOW));
        let root = result.workspace.unwrap();
        match scenario {
            "missing" => fs::rename(&root, f.root.path().join("saved-work")).unwrap(),
            "incomplete" => {
                for n in 0..160 {
                    fs::write(root.join(format!("evidence-{n}")), "").unwrap();
                }
            }
            "owner" => {
                let path = root.join("claim.json");
                let mut claim = read_json(&path);
                claim["owner"] = json!("stale-owner");
                write_json(path, &claim);
            }
            "publication" => fs::write(root.join("publication-attempt.json"), "{}").unwrap(),
            _ => unreachable!(),
        }
        assert_no_actions(&app, 1);
        assert!(
            app.retry(1, retry(scenario, None)).is_err(),
            "accepted {scenario}"
        );
        assert!(
            app.retry(1, retry(scenario, Some(512 * KIB))).is_err(),
            "accepted bump: {scenario}"
        );
        assert!(app.get_view(1).unwrap().continuation_status.is_none());
    }
}

#[test]
fn queued_unknown_and_successful_attempts_have_no_recovery_action() {
    let f = Fixture::new("pass");
    let app = f.open();
    app.submit(f.input(Some(LOW))).unwrap();
    assert_no_actions(&app, 1);
    let mut store = relay::Store::open(f.db()).unwrap();
    let task = store.claim_next("unverified-owner").unwrap().unwrap();
    assert_no_actions(&app, 1);
    assert!(app.retry(1, retry("unknown", Some(512 * KIB))).is_err());
    store.finish(&task.claim().unwrap(),&json!({"outcome":"success","workspace":null,"agent":null,"tests":null,"draft_pr":null,"error":null}).to_string()).unwrap();
    assert_no_actions(&app, 1);
}

#[test]
fn source_snapshot_failure_is_distinct_and_never_suggests_a_workspace_increase() {
    let f = Fixture::new("raise RuntimeError('agent must not execute')");
    sparse(f.root.path().join("source/large.bin"), 128 * KIB);
    let (app, result) = f.failed(Some(256 * KIB));
    quota_code(&result, "snapshot_limit_exceeded");
    assert!(result.agent.is_none());
    assert_no_actions(&app, 1);
    assert!(
        app.retry(1, retry("wrong-remedy", Some(512 * KIB)))
            .is_err()
    );
}

#[test]
fn absent_quota_keeps_legacy_job_bytes_and_old_workspace_record_reusable() {
    let f = Fixture::new("raise SystemExit(7)");
    let legacy = json!({"repository":"repo","requirements":"old request","agent":"fake","test":null,"publish":false,"draft_pr_adapter":null});
    let job = Job::from_payload(&legacy.to_string(), &f.config).unwrap();
    assert_eq!(serde_json::to_value(&job).unwrap(), legacy);
    let app = f.open();
    app.submit(Submission {
        key: "legacy".into(),
        job,
    })
    .unwrap();
    assert!(app.work_once().unwrap());
    let root = read_result(&app, 1).workspace.unwrap();
    let claim_path = root.join("claim.json");
    let mut claim = read_json(&claim_path);
    claim.as_object_mut().unwrap().remove("quota_bytes");
    claim["job"]
        .as_object_mut()
        .unwrap()
        .remove("workspace_quota_bytes");
    write_json(claim_path, &claim);
    let mut old: Value =
        serde_json::from_str(app.get(1).unwrap().result.as_ref().unwrap()).unwrap();
    old.as_object_mut().unwrap().remove("failure");
    old.as_object_mut().unwrap().remove("resources");
    write_json(root.join("last-result.json"), &old);
    rusqlite::Connection::open(f.db())
        .unwrap()
        .execute("UPDATE tasks SET result=?1 WHERE id=1", [old.to_string()])
        .unwrap();
    drop(app);
    let app = f.open();
    assert_eq!(
        app.operator(1).unwrap()["failure"]["code"],
        "legacy_failure"
    );
    assert!(app.operator(1).unwrap()["resources"]["quota_bytes"].is_null());
    assert_eq!(
        app.operator(1).unwrap()["recovery"]["inherited_quota_bytes"],
        CAP
    );
    let child = app.retry(1, retry("compatible", None)).unwrap();
    assert!(app.work_once().unwrap());
    assert_eq!(read_result(&app, child.id).workspace.as_ref(), Some(&root));
    assert_eq!(
        app.get(1).unwrap().result.as_deref(),
        Some(old.to_string().as_str())
    );
}

fn add_estimate_workflow(config: &mut HostConfig) {
    config.tests.insert(
        "check".into(),
        serde_json::from_value(json!({"program":"/bin/true"})).unwrap(),
    );
    config.native_agents.insert("reviewer".into(),serde_json::from_value(json!({"provider":"claude_cli","program":"/bin/true","session_continuity":true,"max_turns":2})).unwrap());
    config.workflows.insert("checked".into(),serde_json::from_value(json!({"repository":"repo","developer":"fake","reviewer":"reviewer","test":"check","max_repairs":0})).unwrap());
}

#[test]
fn source_estimate_is_bounded_metadata_only_with_explicit_uncertainty() {
    let mut f = Fixture::new("raise RuntimeError('must never execute')");
    let source = f.root.path().join("source");
    for directory in [".git", "target", "node_modules"] {
        fs::create_dir(source.join(directory)).unwrap();
        sparse(source.join(directory).join("bytes"), 4 * KIB);
    }
    sparse(source.join("sparse.bin"), 8 * KIB);
    add_estimate_workflow(&mut f.config);
    // Every configured executable would leave this marker if estimation invoked it.
    use std::os::unix::fs::PermissionsExt;
    let invoked = f.root.path().join("unexpected-process");
    let sentinel = f.root.path().join("must-not-run.py");
    fs::write(
        &sentinel,
        format!(
            "#!/usr/bin/python3\nimport pathlib\npathlib.Path({}).write_text('invoked')\n",
            json!(invoked)
        ),
    )
    .unwrap();
    fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o700)).unwrap();
    f.config.agents.get_mut("fake").unwrap().program = sentinel.clone();
    f.config.native_agents.get_mut("reviewer").unwrap().program = sentinel.clone();
    f.config.workflows.get_mut("checked").unwrap().git_program = sentinel;
    let app = f.open();
    let ordinary = serde_json::to_value(app.resource_estimate("repo", None).unwrap()).unwrap();
    assert_eq!(ordinary["initial_estimate"]["source"], "host_inventory");
    assert_eq!(ordinary["initial_estimate"]["snapshot_bytes"], 9 + 8 * KIB);
    assert_eq!(
        ordinary["initial_estimate"]["git_metadata_reference_bytes"],
        4 * KIB
    );
    assert_eq!(ordinary["initial_estimate"]["reviewer_copy_bytes"], 0);
    assert_eq!(
        ordinary["initial_estimate"]["estimated_initial_bytes"],
        Value::Null
    );
    let workflow =
        serde_json::to_value(app.resource_estimate("repo", Some("checked")).unwrap()).unwrap();
    assert_eq!(workflow["initial_estimate"]["snapshot_bytes"], 9 + 16 * KIB);
    assert_eq!(
        workflow["initial_estimate"]["reviewer_copy_bytes"],
        9 + 16 * KIB
    );
    assert!(
        workflow["initial_estimate"]["estimated_initial_bytes"]
            .as_u64()
            .unwrap()
            > 2 * (9 + 16 * KIB)
    );
    for estimate in [ordinary, workflow] {
        assert_eq!(estimate["host_policy_cap_bytes"], CAP);
        assert_eq!(estimate["default_quota_bytes"], CAP);
        assert_eq!(estimate["snapshot_cap_bytes"], 64 * KIB);
        assert_eq!(estimate["build_growth"], "unknown");
        assert_eq!(estimate["enforcement"], "logical_bytes_best_effort");
        assert_eq!(estimate["os_hard_quota"], false);
        assert_eq!(estimate["disk_reserved"], false);
        assert!(
            !estimate["initial_estimate"]["notes"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    for name in ["../source", "/tmp", "missing"] {
        assert!(app.resource_estimate(name, None).is_err());
    }
    assert!(app.resource_estimate("repo", Some("missing")).is_err());
    assert_eq!(app.list(None).unwrap().len(), 0);
    assert!(!invoked.exists());
    assert_eq!(fs::read_dir(&app.config.workspace_root).unwrap().count(), 0);
}

#[test]
fn current_usage_is_cached_but_action_rechecks_the_filesystem() {
    let f = Fixture::new("raise SystemExit(7)");
    let (app, result) = f.failed(Some(LOW));
    let first = app.operator(1).unwrap();
    sparse(
        result.workspace.unwrap().join("repository/later.bin"),
        768 * KIB,
    );
    let cached = app.operator(1).unwrap();
    assert_eq!(cached["resources"]["usage"], first["resources"]["usage"]);
    assert!(app.retry(1, retry("fresh-check", Some(512 * KIB))).is_err());
    assert!(app.get_view(1).unwrap().continuation_status.is_none());
}

async fn request(
    router: axum::Router,
    method: &str,
    path: &str,
    body: Value,
    auth: bool,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if auth {
        builder = builder.header("authorization", format!("Bearer {TOKEN}"));
    }
    let response = router
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!({"text":String::from_utf8_lossy(&bytes)})),
    )
}
fn mcp_call(app: &Application, name: &str, arguments: Value) -> Value {
    mcp::handle(app,json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}})).unwrap()["result"].clone()
}

#[tokio::test]
async fn http_and_mcp_expose_authenticated_read_contract_and_reject_unknown_input() {
    let f = Fixture::new("raise SystemExit(7)");
    let (app, _) = f.failed(Some(LOW));
    let router = http::router(app.clone(), TOKEN.into()).unwrap();
    for path in ["/api/resources?repository=repo", "/api/tasks/1/operator"] {
        assert_eq!(
            request(router.clone(), "GET", path, Value::Null, false)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request(router.clone(), "GET", path, Value::Null, true)
                .await
                .0,
            StatusCode::OK
        );
    }
    let operator = request(
        router.clone(),
        "GET",
        "/api/tasks/1/operator",
        Value::Null,
        true,
    )
    .await
    .1;
    assert_eq!(operator["task_id"], 1);
    assert_eq!(operator["generation"], 1);
    assert_eq!(
        operator["retained_result"],
        json!({"available":true,"immutable":true})
    );
    assert_eq!(operator["workspace_retained"], true);
    assert!(operator["failure"]["code"].is_string());
    assert!(operator["failure"]["stage"].is_string());
    assert!(operator["failure"]["cause"].is_string());
    assert!(
        operator["resources"]["usage"]["measured_at"]
            .as_u64()
            .unwrap()
            > 0
    );
    for path in [
        "/api/resources?repository=repo&injected=true",
        "/api/resources?repository=%2Ftmp",
        "/api/resources?repository=missing",
        "/api/resources?repository=repo&workflow=missing",
    ] {
        assert_eq!(
            request(router.clone(), "GET", path, Value::Null, true)
                .await
                .0,
            StatusCode::BAD_REQUEST,
            "{path}"
        );
    }
    assert_eq!(
        request(
            router.clone(),
            "GET",
            "/api/tasks/999/operator",
            Value::Null,
            true
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (status, error) = request(
        router.clone(),
        "POST",
        "/api/tasks/1/retry",
        json!({"key":"bad","confirm_stopped_and_reconciled":true,"workspace_quota_bytes":LOW}),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["failure"]["code"], "invalid_workspace_quota_increase");
    assert_eq!(error["failure"]["stage"], "request");
    assert!(error["error"].is_string());
    let (status, error) = request(router.clone(), "POST", "/api/tasks/1/continue-review", json!({"key":"unsupported-review","confirm_stopped_and_reconciled":true,"revalidate_tests":true}), true).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["failure"]["code"], "review_checkpoint_unverified");
    assert_eq!(error["failure"]["stage"], "request");
    let tools = mcp::handle(&app, json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).unwrap();
    for name in [
        "relay_resources",
        "relay_operator",
        "relay_retry",
        "relay_continue_review",
    ] {
        let tool = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap();
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        if ["relay_retry", "relay_continue_review"].contains(&name) {
            assert_eq!(
                tool["inputSchema"]["properties"]["workspace_quota_bytes"]["type"],
                json!(["integer", "null"])
            );
        }
    }
    assert_eq!(
        request(
            router,
            "POST",
            "/api/tasks/1/retry",
            json!({"key":"bad","confirm_stopped_and_reconciled":true,"unknown":true}),
            true
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    for (name, args) in [
        ("relay_resources", json!({"repository":"repo"})),
        ("relay_operator", json!({"id":1})),
    ] {
        let response = mcp_call(&app, name, args.clone());
        assert_eq!(response["isError"], false);
        let body: Value =
            serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap();
        assert!(body.is_object());
        let mut unknown = args;
        unknown["injected"] = json!(true);
        assert_eq!(mcp_call(&app, name, unknown)["isError"], true);
    }
    assert_eq!(
        mcp_call(&app, "relay_operator", json!({"id":0}))["isError"],
        true
    );
    assert_eq!(
        mcp_call(
            &app,
            "relay_retry",
            json!({"id":1,"key":"bad","confirm_stopped_and_reconciled":true,"workspace_quota_bytes":LOW})
        )["isError"],
        true
    );
    assert_eq!(app.list(None).unwrap().len(), 1);
}

#[test]
fn adversarial_failure_metadata_and_evidence_fit_16k_without_looping() {
    const CHILD: &str = "RELAY_RESOURCE_SERIALIZATION_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let noise = "\u{1}".repeat(8192);
        let failure = json!({"code":"workspace_quota_exceeded","stage":"developer","cause":noise});
        let command = json!({"failure":failure,"outcome":"failure","exit_code":1,"signal":null,"stdout":noise,"stderr":noise,"stdout_truncated":false,"stderr_truncated":false,"duration_ms":1,"supervisor_pid":null,"error":noise});
        let result:RunResult=serde_json::from_value(json!({"failure":failure,"resources":{"usage":{"logical_bytes":262144,"complete":true,"measured_at":1,"reason":noise},"quota_bytes":LOW,"host_policy_cap_bytes":CAP,"snapshot_cap_bytes":64*KIB,"enforcement":"logical_bytes_best_effort","os_hard_quota":false,"disk_reserved":false},"outcome":"failure","workspace":"/fixture","agent":command,"tests":command,"draft_pr":command,"error":noise})).unwrap();
        let serialized = result.to_json();
        assert!(serialized.len() <= relay::MAX_RESULT_BYTES);
        let restored: RunResult = serde_json::from_str(&serialized).unwrap();
        assert!(matches!(
            restored
                .failure
                .as_ref()
                .map(|failure| failure.code.as_str()),
            Some("workspace_quota_exceeded" | "result_evidence_truncated")
        ));
        assert_eq!(restored.resources.unwrap().quota_bytes, Some(LOW));
        return;
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "adversarial_failure_metadata_and_evidence_fit_16k_without_looping",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("RunResult serialization did not converge within 5 seconds");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn review_fixture() -> Fixture {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new(
        r#"
import os,pathlib
log=pathlib.Path(os.environ['FIXTURE_AUDIT'])/'developer'
with log.open('a') as output: output.write('developer\n')
pathlib.Path('changed.txt').write_text('exact candidate\n')
"#,
    );
    let source = f.root.path().join("source");
    for args in [
        vec!["init", "--initial-branch=main"],
        vec!["add", "original.txt"],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-m",
            "baseline",
        ],
    ] {
        let output = Command::new("/usr/bin/git")
            .args(args)
            .current_dir(&source)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let audit = f.root.path().join("audit");
    fs::create_dir(&audit).unwrap();
    let reviewer = f.root.path().join("reviewer.py");
    fs::write(&reviewer,r#"#!/usr/bin/python3
import json,os,pathlib,sys
if '--version' in sys.argv:
    print('2.1.281 (Claude Code)');sys.exit()
if '--help' in sys.argv:
    print('--output-format --verbose --permission-prompts --restricted --tools --allowedTools --disallowedTools --disable-slash-commands --strict-mcp-config --mcp-config --no-session-persistence --session-id --resume --max-turns --max-budget-usd');sys.exit()
sys.stdin.read()
log=pathlib.Path(os.environ['FIXTURE_AUDIT'])/'reviewer'
count=len(log.read_text().splitlines()) if log.exists() else 0
with log.open('a') as output: output.write('reviewer\n')
flag='--resume' if count else '--session-id'
sid=sys.argv[sys.argv.index(flag)+1]
print(json.dumps({'type':'system','subtype':'init','session_id':sid,'model':'fixture-model'}))
if count == 0:
    print(json.dumps({'type':'result','subtype':'error_max_turns','is_error':True,'session_id':sid,'result':'Maximum turns reached'}))
else:
    verdict=json.dumps({'candidate_sha':os.environ['RELAY_CANDIDATE_SHA'],'verdict':'approved','summary':'Checked exact retained candidate','findings':[]})
    print(json.dumps({'type':'result','subtype':'success','is_error':False,'session_id':sid,'result':verdict,'permission_denials':[]}))
"#).unwrap();
    fs::set_permissions(&reviewer, fs::Permissions::from_mode(0o700)).unwrap();
    add_estimate_workflow(&mut f.config);
    f.config
        .agents
        .get_mut("fake")
        .unwrap()
        .env
        .insert("FIXTURE_AUDIT".into(), audit.to_str().unwrap().into());
    f.config.native_agents.get_mut("reviewer").unwrap().program = reviewer;
    f.config
        .native_agents
        .get_mut("reviewer")
        .unwrap()
        .env
        .insert("FIXTURE_AUDIT".into(), audit.to_str().unwrap().into());
    f.config.tests.insert("check".into(),serde_json::from_value(json!({"program":"/usr/bin/python3","args":["-c","import os,pathlib; p=pathlib.Path(os.environ['FIXTURE_AUDIT'])/'test'; p.open('a').write('test\\n')"],"env":{"FIXTURE_AUDIT":audit}})).unwrap());
    f
}
fn failed_review(f: &Fixture) -> (Arc<Application>, RunResult) {
    let app = f.open();
    let mut input = f.input(Some(256 * KIB));
    input.job.workflow = Some("checked".into());
    app.submit(input).unwrap();
    assert!(app.work_once().unwrap());
    let result = read_result(&app, 1);
    assert_eq!(result.outcome, Outcome::Failure, "{}", result.to_json());
    assert!(result.workflow.as_ref().unwrap().candidate_sha.is_some());
    (app, result)
}

#[test]
fn review_continuation_increases_quota_without_development_or_candidate_change() {
    let f = review_fixture();
    let (app, first) = failed_review(&f);
    let root = first.workspace.unwrap();
    let candidate = first.workflow.unwrap().candidate_sha;
    let old = app.get(1).unwrap().result;
    sparse(root.join("retained-evidence.bin"), 512 * KIB);
    let view = app.operator(1).unwrap();
    let action = view["recovery"]["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|action| action["id"] == "continue_review")
        .unwrap();
    assert_eq!(action["quota_increase_allowed"], true);
    assert_eq!(action["requires_test_revalidation"], true);
    let request:relay_app::ReviewContinuationRequest=serde_json::from_value(json!({"key":"review-larger","confirm_stopped_and_reconciled":true,"revalidate_tests":true,"review_focus":"Check the exact retained candidate","workspace_quota_bytes":768*KIB})).unwrap();
    let child = app.continue_review(1, request).unwrap();
    assert!(app.work_once().unwrap());
    let completed = read_result(&app, child.id);
    assert_eq!(
        completed.outcome,
        Outcome::Success,
        "{}",
        completed.to_json()
    );
    assert!(completed.agent.is_none());
    assert_eq!(completed.workspace.as_ref(), Some(&root));
    assert_eq!(completed.workflow.unwrap().candidate_sha, candidate);
    assert_eq!(app.get(1).unwrap().result, old);
    for (name, count) in [("developer", 1), ("test", 2), ("reviewer", 2)] {
        assert_eq!(
            fs::read_to_string(f.root.path().join("audit").join(name))
                .unwrap()
                .lines()
                .count(),
            count,
            "{name}"
        );
    }
}

#[test]
fn changed_candidate_checkpoint_blocks_operator_and_all_recovery_actions() {
    let f = review_fixture();
    let (app, result) = failed_review(&f);
    let root = result.workspace.unwrap();
    write_json(root.join("candidate-head.json"), &json!("a".repeat(40)));
    assert_no_actions(&app, 1);
    assert!(app.retry(1, retry("changed", Some(512 * KIB))).is_err());
    let request:relay_app::ReviewContinuationRequest=serde_json::from_value(json!({"key":"changed-review","confirm_stopped_and_reconciled":true,"revalidate_tests":true,"workspace_quota_bytes":512*KIB})).unwrap();
    assert!(app.continue_review(1, request).is_err());
    assert!(app.get_view(1).unwrap().continuation_status.is_none());
}

#[test]
fn fifo_claim_and_nonregular_lock_are_rejected_without_blocking() {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    for name in ["claim.json", "owner.lock"] {
        let f = Fixture::new("raise SystemExit(7)");
        let (app, result) = f.failed(Some(LOW));
        let path = result.workspace.unwrap().join(name);
        fs::remove_file(&path).unwrap();
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: path is a valid NUL-terminated fixture path, owned by this test.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let started = Instant::now();
        assert_no_actions(&app, 1);
        assert!(app.retry(1, retry("fifo", None)).is_err());
        assert!(app.retry(1, retry("fifo-bump", Some(512 * KIB))).is_err());
        assert!(started.elapsed() < Duration::from_secs(1), "{name}");
        assert!(app.get_view(1).unwrap().continuation_status.is_none());
    }
}

#[test]
fn nonresource_partial_lower_bound_allows_only_original_quota_recovery() {
    let mut f = Fixture::new("raise SystemExit(7)");
    f.config.max_snapshot_entries = 128;
    let (app, result) = f.failed(Some(LOW));
    let root = result.workspace.unwrap();
    for n in 0..160 {
        fs::write(root.join(format!("partial-{n}")), "").unwrap();
    }
    let view = app.operator(1).unwrap();
    assert_eq!(view["resources"]["usage"]["complete"], false);
    assert_eq!(view["recovery"]["actions"][0]["id"], "retry");
    assert_eq!(
        view["recovery"]["actions"][0]["quota_increase_allowed"],
        false
    );
    assert!(
        app.retry(1, retry("unknown-increase", Some(512 * KIB)))
            .is_err()
    );
    let child = app.retry(1, retry("same-limit", None)).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&child.payload).unwrap()["workspace_quota_bytes"],
        LOW
    );
}

#[test]
fn reserved_but_unsubmitted_increase_retains_its_original_request() {
    let f = Fixture::new(GROW_ONCE);
    let (app, _) = f.failed(Some(LOW));
    let reserved = app.retry(1, retry("frozen-key", Some(512 * KIB))).unwrap();
    let connection = rusqlite::Connection::open(f.db()).unwrap();
    connection
        .execute(
            "UPDATE app_continuations SET task_id=NULL WHERE predecessor_id=1",
            [],
        )
        .unwrap();
    connection
        .execute("DELETE FROM tasks WHERE id=?1", [reserved.id])
        .unwrap();
    drop(connection);
    drop(app);
    let app = f.open();
    let view = app.operator(1).unwrap();
    assert_eq!(
        view["recovery"]["actions"][0]["quota_increase_required"],
        false
    );
    assert!(view["recovery"]["successor_id"].is_null());
    assert_eq!(
        view["recovery"]["reserved_request"],
        json!({"action_id":"retry","key":"frozen-key","workspace_quota_bytes":512*KIB,"revalidate_tests":false,"review_focus":null})
    );
    let restored = app
        .retry(1, retry("different-key", Some(768 * KIB)))
        .unwrap();
    assert_eq!(restored.key, reserved.key);
    assert_eq!(restored.payload, reserved.payload);
    assert_eq!(app.list(None).unwrap().len(), 2);
}

#[test]
fn mutated_predecessor_quota_proof_blocks_execution_without_rebuilding() {
    let f = Fixture::new(GROW_ONCE);
    let (app, result) = f.failed(Some(LOW));
    let root = result.workspace.unwrap();
    let child = app.retry(1, retry("proved", Some(512 * KIB))).unwrap();
    let path = root.join("last-result.json");
    let mut result = read_json(&path);
    result["resources"]["quota_bytes"] = json!(LOW - 1);
    write_json(path, &result);
    assert!(app.work_once().unwrap());
    let failed = read_result(&app, child.id);
    assert_eq!(failed.outcome, Outcome::Failure, "{}", failed.to_json());
    assert!(!root.join("repository/continued.txt").exists());
    assert_eq!(
        fs::read_to_string(root.join("repository/uncommitted.txt")).unwrap(),
        "keep this uncommitted file\n"
    );
    assert_eq!(
        fs::read_dir(app.config.workspace_root.clone())
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn restart_with_changed_host_cap_preserves_observed_quota_and_legacy_inheritance() {
    for explicit_increase in [false, true] {
        let mut f = Fixture::new(GROW_ONCE);
        f.config.max_workspace_bytes = Some(256 * KIB);
        let (app, first) = f.failed(None);
        quota_code(&first, "workspace_quota_exceeded");
        let root = first.workspace.unwrap();
        let original = app.get(1).unwrap();
        assert!(
            !serde_json::from_str::<Value>(&original.payload)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("workspace_quota_bytes")
        );
        drop(app);
        f.config.max_workspace_bytes = Some(768 * KIB);
        let app = f.open();
        let view = app.operator(1).unwrap();
        assert_eq!(view["resources"]["quota_bytes"], 256 * KIB);
        assert_eq!(view["resources"]["host_policy_cap_bytes"], 768 * KIB);
        assert_eq!(view["recovery"]["inherited_quota_bytes"], 768 * KIB);
        let quota = explicit_increase.then_some(512 * KIB);
        let child = app.retry(1, retry("changed-policy", quota)).unwrap();
        let payload: Value = serde_json::from_str(&child.payload).unwrap();
        if explicit_increase {
            assert_eq!(payload["workspace_quota_bytes"], 512 * KIB);
            assert_eq!(
                payload["continuation"]["quota_increase"],
                json!({"previous_bytes":256*KIB,"new_bytes":512*KIB})
            );
        } else {
            assert!(
                !payload
                    .as_object()
                    .unwrap()
                    .contains_key("workspace_quota_bytes")
            );
            assert!(
                !payload["continuation"]
                    .as_object()
                    .unwrap()
                    .contains_key("quota_increase")
            );
        }
        assert!(app.work_once().unwrap());
        let completed = read_result(&app, child.id);
        assert_eq!(
            completed.outcome,
            Outcome::Success,
            "{}",
            completed.to_json()
        );
        assert_eq!(completed.workspace.as_ref(), Some(&root));
        assert_eq!(
            completed.resources.unwrap().quota_bytes,
            Some(if explicit_increase {
                512 * KIB
            } else {
                768 * KIB
            })
        );
        assert_eq!(app.get(1).unwrap(), original);
        assert_eq!(
            app.operator(1).unwrap()["resources"]["quota_bytes"],
            256 * KIB
        );
    }
}

#[test]
fn invalid_current_predecessor_proof_rejects_increase_before_reservation() {
    let f = Fixture::new(GROW_ONCE);
    let (app, result) = f.failed(Some(LOW));
    let root = result.workspace.unwrap();
    let path = root.join("last-result.json");
    let mut proof = read_json(&path);
    proof["resources"]["quota_bytes"] = json!(LOW - 1);
    write_json(path, &proof);
    assert!(
        app.retry(1, retry("invalid-proof", Some(512 * KIB)))
            .is_err()
    );
    assert!(app.get_view(1).unwrap().continuation_status.is_none());
    assert_eq!(app.list(None).unwrap().len(), 1);
    assert_eq!(
        fs::read_to_string(root.join("repository/uncommitted.txt")).unwrap(),
        "keep this uncommitted file\n"
    );
}

#[tokio::test]
async fn planned_copy_admission_declares_required_increase_despite_low_current_usage() {
    let f = Fixture::new("raise SystemExit(7)");
    let (app, mut result) = f.failed(Some(LOW));
    let required = 384 * KIB;
    let mut failure = relay_app::resources::Failure::new(
        "workspace_quota_exceeded",
        "workspace_admission",
        "known planned reviewer copy exceeds the attempt quota",
    );
    failure.required_bytes = Some(required);
    failure.limit_bytes = Some(LOW);
    result.failure = Some(failure);
    // Durable synthetic host evidence isolates the already-known admission case;
    // it does not pretend a future build size can be predicted.
    let serialized = result.to_json();
    rusqlite::Connection::open(f.db())
        .unwrap()
        .execute("UPDATE tasks SET result=?1 WHERE id=1", [&serialized])
        .unwrap();
    fs::write(
        result.workspace.as_ref().unwrap().join("last-result.json"),
        serialized,
    )
    .unwrap();
    let router = http::router(app.clone(), TOKEN.into()).unwrap();
    let (status, view) = request(
        router.clone(),
        "GET",
        "/api/tasks/1/operator",
        Value::Null,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        view["resources"]["usage"]["logical_bytes"]
            .as_u64()
            .unwrap()
            < LOW
    );
    let action = &view["recovery"]["actions"][0];
    assert_eq!(action["quota_increase_required"], true);
    assert_eq!(action["quota_increase_allowed"], true);
    assert_eq!(action["min_quota_bytes"], required);
    let (status, error) = request(
        router.clone(),
        "POST",
        "/api/tasks/1/retry",
        json!({"key":"blank-is-insufficient","confirm_stopped_and_reconciled":true}),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        error["failure"]["code"],
        "workspace_quota_increase_required"
    );
    assert!(app.get_view(1).unwrap().continuation_status.is_none());
    let (status, child) = request(router, "POST", "/api/tasks/1/retry", json!({"key":"bounded-planned-copy","confirm_stopped_and_reconciled":true,"workspace_quota_bytes":required}), true).await;
    assert_eq!(status, StatusCode::CREATED, "{child}");
    assert_eq!(
        serde_json::from_str::<Value>(child["payload"].as_str().unwrap()).unwrap()["workspace_quota_bytes"],
        required
    );
}
