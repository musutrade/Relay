#![cfg(target_os = "linux")]
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use relay::{Store, Task};
use relay_app::{Application, Submission, host::HostConfig, http};
use serde_json::{Value, json};
use std::{
    fs,
    os::{fd::AsRawFd, unix::fs::symlink},
    path::{Path, PathBuf},
    sync::Arc,
};
use tempfile::TempDir;
use tower::ServiceExt;

const TOKEN: &str = "workspace-inventory-fixture-token-0000";
struct Fixture {
    root: TempDir,
    config: HostConfig,
    app: Arc<Application>,
}
impl Fixture {
    fn new(ttl: Option<u64>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("readme"), "source").unwrap();
        let config: HostConfig = serde_json::from_value(json!({
            "workspace_root":root.path().join("runs"),"repositories":{"repo":source},
            "agents":{"fake":{"program":"/bin/false","env":{"PRIVATE_SECRET":"never-expose-this"}}},
            "successful_workspace_retention_seconds":ttl,
            "supervisor_program":env!("CARGO_BIN_EXE_relay-app")
        }))
        .unwrap();
        let app = Application::open(root.path().join("queue.db"), config.clone()).unwrap();
        Self { root, config, app }
    }
    fn db(&self) -> PathBuf {
        self.root.path().join("queue.db")
    }
    fn first(&self, outcome: Option<&str>) -> Task {
        let input: Submission = serde_json::from_value(json!({"key":"first","job":{"repository":"repo","requirements":"fixture","agent":"fake"}})).unwrap();
        self.app.submit(input).unwrap();
        let mut store = Store::open(self.db()).unwrap();
        let task = store.claim_next("fixture-owner").unwrap().unwrap();
        self.claim_files(&task, 1);
        outcome.map_or(task.clone(), |outcome| self.finish(task, 1, outcome))
    }
    fn claim_files(&self, task: &Task, root: i64) {
        let path = self.path(root);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("owner.lock"), "").unwrap();
        let mut job: Value = serde_json::from_str(&task.payload).unwrap();
        job.as_object_mut().unwrap().remove("continuation");
        fs::write(path.join("claim.json"),json!({"version":1,"workspace_task_id":root,"task_id":task.id,"generation":task.generation,"owner":task.owner,"attempt":1,"config_binding":"fixture","job":job}).to_string()).unwrap();
    }
    fn finish(&self, task: Task, root: i64, outcome: &str) -> Task {
        let path = self.path(root);
        let result = json!({"outcome":outcome,"workspace":path,"agent":null,"tests":null,"draft_pr":null,"error":null}).to_string();
        let task = Store::open(self.db())
            .unwrap()
            .finish(&task.claim().unwrap(), &result)
            .unwrap();
        fs::write(path.join("finished.json"),json!({"task_id":task.id,"generation":task.generation,"owner":task.owner,"finished_at":1}).to_string()).unwrap();
        task
    }
    fn next(&self, predecessor: &Task, outcome: Option<&str>) -> Task {
        let mut job: Value = serde_json::from_str(&predecessor.payload).unwrap();
        job["continuation"] = json!({"workspace_task_id":1,"predecessor_task_id":predecessor.id,"predecessor_generation":predecessor.generation});
        let payload = job.to_string();
        let key = format!("child-{}", predecessor.id);
        let mut store = Store::open(self.db()).unwrap();
        let task = store.submit(&key, &payload).unwrap();
        rusqlite::Connection::open(self.db()).unwrap().execute("INSERT INTO app_continuations(predecessor_id,key,payload,task_id) VALUES(?1,?2,?3,?4)",rusqlite::params![predecessor.id,key,payload,task.id]).unwrap();
        if let Some(outcome) = outcome {
            let claimed = store.claim_next("successor-owner").unwrap().unwrap();
            self.claim_files(&claimed, 1);
            self.finish(claimed, 1, outcome)
        } else {
            task
        }
    }
    fn path(&self, id: i64) -> PathBuf {
        self.config.workspace_root.join(format!("task-{id}"))
    }
    fn entry(&self) -> Value {
        self.app.workspace_inventory(None).unwrap()["workspaces"][0].clone()
    }
}
fn change(path: &Path, field: &str, value: Value) {
    let mut object: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    object[field] = value;
    fs::write(path, object.to_string()).unwrap();
}

#[test]
fn preview_preserves_files_tasks_and_disabled_policy() {
    let fixture = Fixture::new(None);
    let task = fixture.first(Some("success"));
    let path = fixture.path(1);
    let claim = fs::read(path.join("claim.json")).unwrap();
    let marker = fs::read(path.join("finished.json")).unwrap();
    fs::write(path.join("retained-build"), vec![1; 16384]).unwrap();
    let preview = fixture.app.workspace_inventory(None).unwrap();
    assert_eq!(preview["policy"]["automatic_cleanup_enabled"], false);
    assert_eq!(preview["workspaces"][0]["retention"]["status"], "disabled");
    assert!(
        preview["workspaces"][0]["allocated_usage"]["allocated_bytes"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(preview["workspaces"][0]["references_complete"], true);
    assert_eq!(fs::read(path.join("claim.json")).unwrap(), claim);
    assert_eq!(fs::read(path.join("finished.json")).unwrap(), marker);
    assert_eq!(fixture.app.get(1).unwrap(), task);
    assert!(path.join("retained-build").exists());
    assert!(!preview.to_string().contains("never-expose-this"));
}
#[test]
fn shared_history_current_success_and_exact_ttl_are_distinct() {
    let fixture = Fixture::new(Some(60));
    let first = fixture.first(Some("failure"));
    assert_eq!(fixture.entry()["retention"]["status"], "protected");
    let second = fixture.next(&first, Some("failure"));
    let third = fixture.next(&second, Some("failure"));
    let fourth = fixture.next(&third, Some("success"));
    let entry = fixture.entry();
    assert_eq!(entry["references"].as_array().unwrap().len(), 4);
    assert_eq!(entry["current_owner"]["task_id"], fourth.id);
    assert_eq!(entry["retention"]["status"], "eligible");
    assert_eq!(entry["retention"]["eligible_at"], 61);
    assert_eq!(fixture.app.get(first.id).unwrap().result, first.result);
    let future = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 600;
    change(
        &fixture.path(1).join("finished.json"),
        "finished_at",
        json!(future),
    );
    let entry = fixture.entry();
    assert_eq!(entry["retention"]["status"], "waiting");
    assert_eq!(entry["retention"]["eligible_at"], future + 60);
}
#[test]
fn queued_active_and_unsubmitted_successors_remain_protected() {
    let fixture = Fixture::new(Some(60));
    let first = fixture.first(Some("failure"));
    let queued = fixture.next(&first, None);
    let entry = fixture.entry();
    assert_eq!(entry["retention"]["status"], "protected");
    assert_eq!(entry["successor_reserved"], true);
    assert_eq!(entry["references"][1]["state"], "queued");
    let claimed = Store::open(fixture.db())
        .unwrap()
        .claim_next("next")
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, queued.id);
    fixture.claim_files(&claimed, 1);
    assert_eq!(fixture.entry()["retention"]["status"], "protected");
    let stopped = fixture.finish(claimed, 1, "failure");
    rusqlite::Connection::open(fixture.db()).unwrap().execute("INSERT INTO app_continuations(predecessor_id,key,payload) VALUES(?1,'not-yet-submitted','{}')",[stopped.id]).unwrap();
    let entry = fixture.entry();
    assert_eq!(entry["successor_reserved"], true);
    assert_eq!(entry["retention"]["status"], "protected");
}
#[test]
fn missing_or_changed_identity_marker_and_locks_never_become_eligible() {
    let fixture = Fixture::new(Some(60));
    fixture.first(Some("success"));
    let path = fixture.path(1);
    let lock = fs::File::open(path.join("owner.lock")).unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert_eq!(fixture.entry()["retention"]["status"], "protected");
    drop(lock);
    change(&path.join("finished.json"), "owner", json!("other-owner"));
    assert_eq!(fixture.entry()["retention"]["status"], "unknown");
    change(&path.join("claim.json"), "generation", json!(9));
    assert_eq!(fixture.entry()["retention"]["status"], "unknown");
    assert_eq!(fixture.entry()["current_owner"], Value::Null);
    assert!(path.join("finished.json").exists());
}
#[test]
fn canonical_owned_roots_only_and_bounded_paging() {
    let fixture = Fixture::new(None);
    fixture.first(Some("success"));
    let outside = fixture.root.path().join("old-offline-preflight");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("sentinel"), "outside").unwrap();
    symlink(&outside, fixture.path(2)).unwrap();
    fs::create_dir(fixture.config.workspace_root.join("task-03")).unwrap();
    fs::create_dir(fixture.config.workspace_root.join("task-3-generation-1")).unwrap();
    let preview = fixture.app.workspace_inventory(None).unwrap();
    assert_eq!(preview["workspaces"].as_array().unwrap().len(), 2);
    assert_eq!(
        preview["workspaces"][0]["allocated_usage"]["allocated_bytes"],
        Value::Null
    );
    assert_eq!(preview["workspaces"][0]["retention"]["status"], "unknown");
    assert!(!preview.to_string().contains("old-offline-preflight"));
    assert_eq!(
        fs::read_to_string(outside.join("sentinel")).unwrap(),
        "outside"
    );
    for id in 3..=18 {
        fs::create_dir(fixture.path(id)).unwrap();
    }
    let page = fixture.app.workspace_inventory(None).unwrap();
    assert_eq!(page["workspaces"].as_array().unwrap().len(), 16);
    assert_eq!(page["next_before"], 3);
    let next = fixture.app.workspace_inventory(Some(3)).unwrap();
    assert_eq!(next["workspaces"].as_array().unwrap().len(), 2);
    assert_eq!(next["next_before"], Value::Null);
}
#[test]
fn completion_fifo_is_unknown_without_blocking_and_history_limit_is_visible() {
    let fixture = Fixture::new(Some(60));
    let mut task = fixture.first(Some("failure"));
    for _ in 0..100 {
        task = fixture.next(&task, Some("success"));
    }
    let entry = fixture.entry();
    assert_eq!(entry["references_complete"], false);
    assert_eq!(entry["retention"]["status"], "unknown");
    let fixture = Fixture::new(Some(60));
    fixture.first(Some("success"));
    let marker = fixture.path(1).join("finished.json");
    fs::remove_file(&marker).unwrap();
    let cpath = std::ffi::CString::new(marker.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
    assert_eq!(fixture.entry()["retention"]["status"], "unknown");
}
#[tokio::test]
async fn endpoint_is_authenticated_strict_read_only_and_never_launches_commands() {
    let fixture = Fixture::new(None);
    fixture.first(Some("success"));
    let router = http::router(fixture.app.clone(), TOKEN.into()).unwrap();
    for (method, uri, authorized, status) in [
        ("GET", "/api/workspaces", false, StatusCode::UNAUTHORIZED),
        ("GET", "/api/workspaces", true, StatusCode::OK),
        (
            "GET",
            "/api/workspaces?path=/tmp",
            true,
            StatusCode::BAD_REQUEST,
        ),
        (
            "GET",
            "/api/workspaces?before=-1",
            true,
            StatusCode::BAD_REQUEST,
        ),
        ("GET", "/api/workspaces?before=1", true, StatusCode::OK),
        (
            "POST",
            "/api/workspaces",
            true,
            StatusCode::METHOD_NOT_ALLOWED,
        ),
    ] {
        let mut request = Request::builder().method(method).uri(uri);
        if authorized {
            request = request.header("authorization", format!("Bearer {TOKEN}"));
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{method} {uri}");
        assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
        if status == StatusCode::OK {
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["policy"]["automatic_cleanup_enabled"], false);
        }
    }
    assert_eq!(fixture.app.list(None).unwrap().len(), 1);
    assert!(fixture.path(1).exists());
}

#[test]
fn active_failed_and_unmarked_success_reads_never_open_the_host_lease() {
    use std::os::fd::FromRawFd;
    for outcome in [None, Some("failure"), Some("success")] {
        let fixture = Fixture::new(Some(60));
        fixture.first(outcome);
        if outcome == Some("success") {
            fs::remove_file(fixture.path(1).join("finished.json")).unwrap();
        }
        // Watch only the fixture's lease: even briefly obtaining a shared or
        // exclusive flock would race the host's nonblocking prepare path.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(fd >= 0);
        let watch = unsafe { fs::File::from_raw_fd(fd) };
        let path = std::ffi::CString::new(
            fixture
                .path(1)
                .join("owner.lock")
                .as_os_str()
                .as_encoded_bytes(),
        )
        .unwrap();
        assert!(unsafe { libc::inotify_add_watch(fd, path.as_ptr(), libc::IN_OPEN) } >= 0);
        let entry = fixture.entry();
        assert_ne!(entry["retention"]["status"], "eligible");
        let mut events = [0u8; 256];
        let read =
            unsafe { libc::read(watch.as_raw_fd(), events.as_mut_ptr().cast(), events.len()) };
        assert_eq!(read, -1, "inventory opened a lease for {outcome:?}");
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
