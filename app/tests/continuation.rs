#![cfg(target_os = "linux")]
use relay_app::{
    Application, RetryRequest, Submission,
    host::{HostConfig, Outcome, RunResult},
};
use serde_json::json;
use std::{fs, path::Path, sync::Arc};
use tempfile::TempDir;

fn config(root: &Path, script: &str) -> HostConfig {
    let source = root.join("source");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("original.txt"), "source").unwrap();
    serde_json::from_value(json!({"workspace_root":root.join("runs"),"repositories":{"repo":source},
        "agents":{"agent":{"program":"/bin/sh","args":["-c",script]}},
        "supervisor_program":env!("CARGO_BIN_EXE_relay-app"),"timeout_seconds":10,"max_retained_workspaces":1})).unwrap()
}
fn input() -> Submission {
    serde_json::from_value(json!({"key":"first","job":{"repository":"repo","requirements":"preserve my work","agent":"agent"}})).unwrap()
}
fn retry(key: &str) -> RetryRequest {
    RetryRequest {
        key: key.into(),
        confirm_stopped_and_reconciled: true,
    }
}
fn result(app: &Application, id: i64) -> RunResult {
    serde_json::from_str(app.get(id).unwrap().result.as_deref().unwrap()).unwrap()
}
const FAIL_ONCE: &str = "if test ! -f .git/attempted; then touch .git/attempted; for n in $(seq 1 17); do printf 'preserved-%s' \"$n\" > file-$n; done; exit 7; fi; for n in $(seq 1 17); do test \"$(cat file-$n)\" = \"preserved-$n\" || exit 9; done; printf continued > finished.txt";
#[test]
fn explicit_retry_preserves_files_reserves_one_successor_and_survives_restart() {
    let tmp = TempDir::new().unwrap();
    let config = config(tmp.path(), FAIL_ONCE);
    let db = tmp.path().join("db");
    let app = Application::open(&db, config.clone()).unwrap();
    app.submit(input()).unwrap();
    assert!(app.work_once().unwrap());
    let original_result = app.get(1).unwrap().result.clone();
    let old = result(&app, 1);
    assert_eq!(old.outcome, Outcome::Failure);
    let workspace = old.workspace.unwrap();
    assert!(
        app.retry(
            1,
            RetryRequest {
                key: "retry".into(),
                confirm_stopped_and_reconciled: false
            }
        )
        .is_err()
    );
    let first = app.retry(1, retry("retry")).unwrap();
    assert_eq!(first.id, 2);
    assert_eq!(app.retry(1, retry("different-click-key")).unwrap(), first);
    drop(app);
    let app = Application::open(&db, config).unwrap();
    assert_eq!(app.retry(1, retry("after-restart")).unwrap(), first);
    assert!(app.work_once().unwrap());
    let next = result(&app, 2);
    assert_eq!(next.outcome, Outcome::Success, "{next:?}");
    assert_eq!(next.workspace.as_ref(), Some(&workspace));
    assert_eq!(fs::read_dir(tmp.path().join("runs")).unwrap().count(), 1);
    assert_eq!(
        fs::read_to_string(workspace.join("repository/finished.txt")).unwrap(),
        "continued"
    );
    assert_eq!(app.get(1).unwrap().result, original_result);
}
#[test]
fn racing_retry_requests_across_application_connections_have_one_child() {
    let tmp = TempDir::new().unwrap();
    let config = config(tmp.path(), FAIL_ONCE);
    let db = tmp.path().join("db");
    let app = Application::open(&db, config.clone()).unwrap();
    app.submit(input()).unwrap();
    app.work_once().unwrap();
    let other = Application::open(&db, config).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let a = {
        let app = app.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            app.retry(1, retry("one")).unwrap()
        })
    };
    let b = std::thread::spawn(move || {
        barrier.wait();
        other.retry(1, retry("two")).unwrap()
    });
    assert_eq!(a.join().unwrap(), b.join().unwrap());
    assert_eq!(app.list(None).unwrap().len(), 2);
}
#[test]
fn direct_metadata_injection_profile_drift_and_publication_are_blocked() {
    let tmp = TempDir::new().unwrap();
    let mut config = config(tmp.path(), FAIL_ONCE);
    let db = tmp.path().join("db");
    let app = Application::open(&db, config.clone()).unwrap();
    let mut injected = input();
    injected.job.continuation = serde_json::from_value(
        json!({"workspace_task_id":1,"predecessor_task_id":1,"predecessor_generation":1}),
    )
    .ok();
    assert!(app.submit(injected).is_err());
    app.submit(input()).unwrap();
    assert!(app.retry(1, retry("queued")).is_err());
    app.work_once().unwrap();
    let workspace = result(&app, 1).workspace.unwrap();
    config
        .agents
        .get_mut("agent")
        .unwrap()
        .env
        .insert("MODEL_SETTING".into(), "changed".into());
    assert!(
        Application::open(&db, config)
            .unwrap()
            .retry(1, retry("changed"))
            .unwrap_err()
            .to_string()
            .contains("binding changed")
    );
    fs::write(workspace.join("publication-attempt.json"), "{}").unwrap();
    assert!(
        app.retry(1, retry("published"))
            .unwrap_err()
            .to_string()
            .contains("publication was attempted")
    );
    assert_eq!(
        fs::read_to_string(workspace.join("repository/file-17")).unwrap(),
        "preserved-17"
    );
}
#[test]
fn missing_or_stale_workspace_never_triggers_a_fresh_snapshot() {
    let tmp = TempDir::new().unwrap();
    let config = config(tmp.path(), FAIL_ONCE);
    let app = Application::open(tmp.path().join("db"), config).unwrap();
    app.submit(input()).unwrap();
    app.work_once().unwrap();
    let child = app.retry(1, retry("retry")).unwrap();
    let root = result(&app, 1).workspace.unwrap();
    let marker = root.join("claim.json");
    let mut record: serde_json::Value =
        serde_json::from_slice(&fs::read(&marker).unwrap()).unwrap();
    record["task_id"] = json!(999);
    fs::write(marker, serde_json::to_vec(&record).unwrap()).unwrap();
    assert!(app.work_once().unwrap());
    assert_eq!(result(&app, child.id).outcome, Outcome::Failure);
    assert_eq!(
        fs::read_to_string(root.join("repository/file-17")).unwrap(),
        "preserved-17"
    );
    assert_eq!(
        fs::read_dir(app.host.config().workspace_root.clone())
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn successful_workspace_ttl_is_opt_in_and_never_deletes_failed_work() {
    let tmp = TempDir::new().unwrap();
    let mut config = config(tmp.path(), FAIL_ONCE);
    config.successful_workspace_retention_seconds = Some(60);
    let app = Application::open(tmp.path().join("db"), config).unwrap();
    app.submit(input()).unwrap();
    app.work_once().unwrap();
    let path = result(&app, 1).workspace.unwrap();
    let task = app.get(1).unwrap();
    fs::write(
        path.join("finished.json"),
        json!({"task_id":1,"generation":1,"owner":task.owner,"finished_at":1}).to_string(),
    )
    .unwrap();
    assert_eq!(app.cleanup_completed().unwrap(), 0);
    assert!(path.join("repository/file-17").is_file());
    app.retry(1, retry("continue")).unwrap();
    app.work_once().unwrap();
    assert_eq!(app.cleanup_completed().unwrap(), 0);
    let marker = path.join("finished.json");
    let mut finished: serde_json::Value =
        serde_json::from_slice(&fs::read(&marker).unwrap()).unwrap();
    finished["finished_at"] = json!(1);
    fs::write(marker, finished.to_string()).unwrap();
    assert_eq!(app.cleanup_completed().unwrap(), 1);
    assert!(!path.exists());
    assert_eq!(app.get(2).unwrap().state, relay::State::Finished);
    assert!(app.get(1).unwrap().result.is_some());
    assert!(app.get(2).unwrap().result.is_some());
}

#[test]
fn lease_child_process() {
    let Some(root) = std::env::var_os("RELAY_TEST_LEASE_CHILD") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let config = HostConfig::load(root.join("config.json")).unwrap();
    let value: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("task.json")).unwrap()).unwrap();
    let task = relay::Task {
        id: value["id"].as_i64().unwrap(),
        key: value["key"].as_str().unwrap().into(),
        payload: value["payload"].as_str().unwrap().into(),
        state: relay::State::Claimed,
        generation: 1,
        owner: Some("old-owner".into()),
        result: None,
    };
    relay_app::host::Host::new(config)
        .unwrap()
        .execute(&task, Arc::new(std::sync::atomic::AtomicBool::new(false)));
}

#[test]
fn supervisor_retains_workspace_ownership_after_parent_process_dies() {
    use std::os::{fd::AsRawFd, unix::fs::PermissionsExt};
    let tmp = TempDir::new().unwrap();
    let mut config = config(tmp.path(), "true");
    let wrapper = tmp.path().join("slow-supervisor.py");
    let marker = tmp.path().join("supervisor-started");
    fs::write(&wrapper,format!("#!/usr/bin/python3\nimport os,pathlib,time\npathlib.Path({}).write_text(str(os.getpid()))\ntime.sleep(1)\nos.execv({}, [{}, '__relay_host_supervisor'])\n",json!(marker),json!(env!("CARGO_BIN_EXE_relay-app")),json!(env!("CARGO_BIN_EXE_relay-app")))).unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    config.supervisor_program = Some(wrapper);
    let task = relay::Task {
        id: 1,
        key: "lease".into(),
        payload: serde_json::to_string(&input().job).unwrap(),
        state: relay::State::Claimed,
        generation: 1,
        owner: Some("old-owner".into()),
        result: None,
    };
    fs::write(
        tmp.path().join("config.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    fs::write(
        tmp.path().join("task.json"),
        serde_json::to_vec(&task).unwrap(),
    )
    .unwrap();
    let mut parent = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lease_child_process", "--nocapture"])
        .env("RELAY_TEST_LEASE_CHILD", tmp.path())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !marker.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(marker.exists());
    parent.kill().unwrap();
    parent.wait().unwrap();
    let mut next = task.clone();
    next.generation = 2;
    next.owner = Some("new-owner".into());
    let result = relay_app::host::Host::new(config.clone())
        .unwrap()
        .execute(&next, Arc::new(std::sync::atomic::AtomicBool::new(false)));
    assert_eq!(result.outcome, Outcome::Unknown, "{result:?}");
    assert!(result.error.unwrap().contains("exclusively stopped"));
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(config.workspace_root.join("task-1/owner.lock"))
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        // SAFETY: try to lock only this test-owned descriptor; no process signaling.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "supervisor did not release stopped workspace"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[tokio::test]
async fn authenticated_http_and_mcp_share_the_same_explicit_continuation() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let tmp = TempDir::new().unwrap();
    let app = Application::open(tmp.path().join("db"), config(tmp.path(), FAIL_ONCE)).unwrap();
    app.submit(input()).unwrap();
    app.work_once().unwrap();
    let token = "test-only-token-00000000000000000000";
    let router = relay_app::http::router(app.clone(), token.into()).unwrap();
    let body = json!({"key":"http-retry","confirm_stopped_and_reconciled":true}).to_string();
    let unauthorized = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/tasks/1/retry")
                .header("content-type", "application/json")
                .body(Body::from(body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/tasks/1/retry")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let task: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(task["id"], 2);
    let response=relay_app::mcp::handle(&app,json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"relay_retry","arguments":{"id":1,"key":"mcp-retry","confirm_stopped_and_reconciled":true}}})).unwrap();
    assert_eq!(response["result"]["isError"], false);
    assert!(response.to_string().contains("http-retry"));
    assert_eq!(app.list(None).unwrap().len(), 2);
}
