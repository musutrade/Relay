#![cfg(target_os = "linux")]
use relay_app::{
    Application, Submission,
    ci_tracking::{CiControlRequest, CiStartRequest, CiTrack},
    host::HostConfig,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::{fd::AsRawFd, unix::fs::PermissionsExt},
    path::Path,
    sync::{Arc, Barrier},
    time::{Duration, Instant},
};
use tempfile::TempDir;

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const BASE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const OBSERVER: &str = r#"import json,os,pathlib,sys,time
request=json.load(sys.stdin)
assert request['version']==1 and os.environ['RELAY_CI_OBSERVE']=='1'
root=pathlib.Path(os.environ['FIXTURE'])
(root/'started').write_text(str(os.getpid()))
if (root/'sleep').exists(): time.sleep(60)
print((root/'response').read_text())
"#;
fn config(root: &Path) -> HostConfig {
    fs::create_dir_all(root.join("source")).unwrap();
    fs::write(root.join("source/source.txt"), "source unchanged").unwrap();
    fs::write(root.join("observer.py"), OBSERVER).unwrap();
    fs::write(root.join("response"), evidence().to_string()).unwrap();
    serde_json::from_value(json!({
        "workspace_root":root.join("workspaces"),"repositories":{"repo":root.join("source")},
        "agents":{"fake":{"program":"/bin/echo","args":["developed"]}},
        "tests":{"pass":{"program":"/bin/true"}},"timeout_seconds":10,
        "supervisor_program":env!("CARGO_BIN_EXE_relay-app"),
        "ci_policies":{"checks":{"github_repository":"example/project","base_branch":"main","observer":{"program":"/usr/bin/python3","args":[root.join("observer.py")],"env":{"FIXTURE":root}},"workflow_id":41,"app_id":15368,"event":"pull_request","required_jobs":["test"],"poll_interval_seconds":60,"observation_window_seconds":86400}}
    })).unwrap()
}
fn publication_result() -> Value {
    json!({
        "outcome":"success","workspace":null,"agent":null,"tests":null,"draft_pr":null,"error":null,
        "workflow":{"name":"checked","base_sha":BASE,"candidate_sha":SHA,"reviewed_sha":SHA,"rounds":[],"reconciliation_required":false,"publication":{"base_branch":"main","dry_run":false,"draft":true,"repository":"example/project","branch":"relay/task-1-g1","candidate_sha":SHA,"url":"https://github.com/example/project/pull/7"}}
    })
}
fn source(root: &Path, value: Value) -> i64 {
    let mut db = relay::Store::open(root.join("relay.db")).unwrap();
    let task=db.submit("published",&json!({"repository":"repo","requirements":"fixture","agent":"fake","test":"pass","publish":true}).to_string()).unwrap();
    let claimed = db.claim_next("fixture-host").unwrap().unwrap();
    assert_eq!(claimed.id, task.id);
    db.finish(&claimed.claim().unwrap(), &value.to_string())
        .unwrap();
    task.id
}
fn app(root: &Path) -> (Arc<Application>, i64) {
    let config = config(root);
    let id = source(root, publication_result());
    (
        Application::open(root.join("relay.db"), config).unwrap(),
        id,
    )
}
fn request(app: &Application, id: i64, key: &str) -> CiStartRequest {
    serde_json::from_value(json!({"key":key,"policy":"checks","policy_digest":app.ci_preview(id).unwrap()["policies"][0]["policy_digest"]})).unwrap()
}
fn start(app: &Application, id: i64) -> CiTrack {
    app.ci_start(id, request(app, id, "track")).unwrap()
}
fn evidence() -> Value {
    json!({"version":1,"observation":"ok","complete":true,"repository":{"id":100,"full_name":"example/project"},"pull_request":{"id":200,"number":7,"url":"https://github.com/example/project/pull/7","state":"open","merged":false,"draft":true,"head_sha":SHA,"head_ref":"relay/task-1-g1","head_repository_id":100,"base_ref":"main","base_sha":BASE,"base_repository_id":100,"mergeable":null},"run":{"id":300,"run_number":10,"run_attempt":1,"workflow_id":41,"event":"pull_request","head_sha":SHA,"head_repository_id":100,"check_suite_id":400,"app_id":15368,"status":"completed","conclusion":"success","pull_request_ids":[200],"jobs":[{"id":500,"check_run_id":600,"name":"test","head_sha":SHA,"check_suite_id":400,"app_id":15368,"status":"completed","conclusion":"success"}]},"error_code":null,"detail":null,"remote_merge_eligibility":"not_established"})
}
fn observe(root: &Path, app: &Application, id: i64, value: Value) -> CiTrack {
    fs::write(root.join("response"), value.to_string()).unwrap();
    assert!(app.ci_work_once().unwrap());
    app.ci_get(id).unwrap()
}
fn due(root: &Path, id: i64) {
    let db = rusqlite::Connection::open(root.join("relay.db")).unwrap();
    let raw: String = db
        .query_row("SELECT record FROM app_ci_tracks WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .unwrap();
    let mut v: Value = serde_json::from_str(&raw).unwrap();
    v["view"]["next_poll_at"] = json!(0);
    db.execute(
        "UPDATE app_ci_tracks SET next_poll_at=0,record=?2 WHERE id=?1",
        rusqlite::params![id, v.to_string()],
    )
    .unwrap();
}
fn ci_dir(root: &Path) -> std::path::PathBuf {
    let db = root.join("relay.db").canonicalize().unwrap();
    let digest = format!("{:x}", Sha256::digest(db.as_os_str().as_encoded_bytes()));
    root.join("workspaces/.ci-tracking").join(digest)
}
fn wait_started(root: &Path) {
    let end = Instant::now() + Duration::from_secs(5);
    while !root.join("started").exists() {
        assert!(Instant::now() < end, "observer failed to start");
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn admission_is_local_idempotent_and_never_exposes_profile_secrets() {
    let root = TempDir::new().unwrap();
    let (app, id) = app(root.path());
    let preview = app.ci_preview(id).unwrap();
    assert_eq!(preview["eligible"], true);
    assert!(!root.path().join("started").exists());
    for hidden in ["FIXTURE", "observer.py", "program"] {
        assert!(!preview.to_string().contains(hidden));
    }
    let first = start(&app, id);
    let replay = start(&app, id);
    assert_eq!(first.id, replay.id);
    assert_eq!(first.attempt, 0);
    assert!(app.ci_start(id, request(&app, id, "other-key")).is_err());
    let mut mismatch = request(&app, id, "track");
    mismatch.policy_digest = "0".repeat(64);
    assert!(app.ci_start(id, mismatch).is_err());
    let core = relay::Store::open(root.path().join("relay.db")).unwrap();
    assert!(core.active_claim().unwrap().is_none());
    let raw: String = rusqlite::Connection::open(root.path().join("relay.db"))
        .unwrap()
        .query_row("SELECT record FROM app_ci_tracks", [], |r| r.get(0))
        .unwrap();
    assert!(!raw.contains("FIXTURE"));
}
#[test]
fn dry_unknown_old_direct_and_inconsistent_publications_are_rejected() {
    for mode in ["dry", "unknown", "old", "candidate", "base"] {
        let root = TempDir::new().unwrap();
        let config = config(root.path());
        let mut result = publication_result();
        match mode {
            "dry" => result["workflow"]["publication"]["dry_run"] = json!(true),
            "unknown" => result["outcome"] = json!("unknown"),
            "old" => {
                result["workflow"]["publication"]
                    .as_object_mut()
                    .unwrap()
                    .remove("base_branch");
            }
            "candidate" => result["workflow"]["reviewed_sha"] = json!(BASE),
            _ => result["workflow"]["publication"]["base_branch"] = json!("other"),
        }
        let id = source(root.path(), result);
        let app = Application::open(root.path().join("relay.db"), config).unwrap();
        assert_eq!(app.ci_preview(id).unwrap()["eligible"], false, "{mode}");
        assert!(
            app.ci_start(
                id,
                CiStartRequest {
                    key: "x".into(),
                    policy: "checks".into(),
                    policy_digest: "0".repeat(64)
                }
            )
            .is_err()
        );
    }
}
#[test]
fn concurrent_connections_reserve_one_exact_tracker() {
    let root = TempDir::new().unwrap();
    let (app, id) = app(root.path());
    let other = Application::open(root.path().join("relay.db"), app.config.clone()).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let first_request = request(&app, id, "concurrent");
    let peer = Arc::clone(&barrier);
    let handle = std::thread::spawn(move || {
        peer.wait();
        other.ci_start(id, first_request).unwrap()
    });
    let request = request(&app, id, "concurrent");
    barrier.wait();
    let a = app.ci_start(id, request).unwrap();
    let b = handle.join().unwrap();
    assert_eq!(a.id, b.id);
    assert_eq!(app.ci_for_task(id).unwrap().len(), 1);
}
#[test]
fn pending_missing_failure_and_exact_source_success() {
    let root = TempDir::new().unwrap();
    let (app, id) = app(root.path());
    let track = start(&app, id);
    let mut missing = evidence();
    missing["run"]["jobs"] = json!([]);
    let pending = observe(root.path(), &app, track.id, missing);
    assert_eq!(pending.status, "watching");
    assert_eq!(
        pending.latest_evidence.unwrap()["missing_jobs"],
        json!(["test"])
    );
    assert_eq!(pending.observed_repository_id, Some(100));
    due(root.path(), track.id);
    let mut failed = evidence();
    failed["run"]["jobs"][0]["conclusion"] = json!("failure");
    let failed = observe(root.path(), &app, track.id, failed);
    assert_eq!(failed.status, "checks_failed");
    assert!(!app.ci_work_once().unwrap());
    let resumed = app
        .ci_resume(
            track.id,
            CiControlRequest {
                expected_revision: failed.revision,
            },
        )
        .unwrap();
    assert_eq!(resumed.window_generation, 2);
    let passed = observe(root.path(), &app, track.id, evidence());
    assert_eq!(passed.status, "configured_checks_passed");
    assert_eq!(passed.remote_merge_eligibility, "not_established");
    assert!(passed.last_observed_at.is_some());
    assert!(
        !ci_dir(root.path())
            .join(".ci-observation-in-flight")
            .exists()
    );
    assert!(!app.ci_work_once().unwrap());
}
#[test]
fn source_and_head_base_drift_cannot_report_green() {
    for field in [
        "workflow",
        "app",
        "event",
        "head",
        "branch",
        "base",
        "suite",
        "job_head",
        "job_app",
        "pr_id",
        "pr_duplicate",
        "pr_other",
        "duplicate",
        "pending_run",
        "oversize",
        "unknown_field",
    ] {
        let root = TempDir::new().unwrap();
        let (app, id) = app(root.path());
        let track = start(&app, id);
        let mut e = evidence();
        match field {
            "workflow" => e["run"]["workflow_id"] = json!(42),
            "app" => e["run"]["app_id"] = json!(1),
            "event" => e["run"]["event"] = json!("push"),
            "head" => e["pull_request"]["head_sha"] = json!(BASE),
            "branch" => e["pull_request"]["head_ref"] = json!("other"),
            "base" => e["pull_request"]["base_ref"] = json!("other"),
            "suite" => e["run"]["jobs"][0]["check_suite_id"] = json!(999),
            "job_head" => e["run"]["jobs"][0]["head_sha"] = json!(BASE),
            "job_app" => e["run"]["jobs"][0]["app_id"] = json!(1),
            "pr_id" => e["run"]["pull_request_ids"] = json!([999]),
            "pr_duplicate" => e["run"]["pull_request_ids"] = json!([200, 200]),
            "pr_other" => e["run"]["pull_request_ids"] = json!([200, 999]),
            "duplicate" => {
                let mut j = e["run"]["jobs"][0].clone();
                j["id"] = json!(501);
                j["check_run_id"] = json!(601);
                e["run"]["jobs"].as_array_mut().unwrap().push(j);
            }
            "pending_run" => {
                e["run"]["status"] = json!("in_progress");
                e["run"]["conclusion"] = Value::Null;
            }
            "oversize" => e["detail"] = json!("x".repeat(70_000)),
            _ => e["unrecognized"] = json!(true),
        };
        let out = observe(root.path(), &app, track.id, e);
        assert_eq!(
            out.status,
            if field == "pending_run" {
                "watching"
            } else {
                "blocked"
            },
            "{field}"
        );
    }
}
#[test]
fn observed_numeric_identity_and_base_are_pinned_across_resumes() {
    for field in ["base_sha", "repository_id", "pr_id"] {
        let root = TempDir::new().unwrap();
        let (app, id) = app(root.path());
        let track = start(&app, id);
        let mut e = evidence();
        e["run"]["jobs"] = json!([]);
        let first = observe(root.path(), &app, track.id, e.clone());
        assert_eq!(first.observed_base_sha.as_deref(), Some(BASE));
        due(root.path(), track.id);
        match field {
            "base_sha" => {
                e["pull_request"]["base_sha"] = json!("cccccccccccccccccccccccccccccccccccccccc")
            }
            "repository_id" => {
                e["repository"]["id"] = json!(101);
                e["pull_request"]["head_repository_id"] = json!(101);
                e["pull_request"]["base_repository_id"] = json!(101);
                e["run"]["head_repository_id"] = json!(101);
            }
            _ => {
                e["pull_request"]["id"] = json!(201);
                e["run"]["pull_request_ids"] = json!([201]);
            }
        }
        let next = observe(root.path(), &app, track.id, e);
        assert_eq!(next.status, "blocked");
        assert_eq!(next.last_observed_at, first.last_observed_at);
        assert_eq!(next.observed_pr_id, Some(200));
    }
}
#[test]
fn ci_waiting_does_not_claim_or_block_development_and_remote_stop_cancels() {
    let root = TempDir::new().unwrap();
    let mut config = config(root.path());
    config.max_retained_workspaces = 1;
    let id = source(root.path(), publication_result());
    let app = Application::open(root.path().join("relay.db"), config).unwrap();
    fs::write(root.path().join("sleep"), "").unwrap();
    let track = start(&app, id);
    let worker = Arc::clone(&app);
    let handle = std::thread::spawn(move || worker.ci_work_once());
    wait_started(root.path());
    assert!(
        relay::Store::open(root.path().join("relay.db"))
            .unwrap()
            .active_claim()
            .unwrap()
            .is_none()
    );
    let task=app.submit(serde_json::from_value::<Submission>(json!({"key":"development","job":{"repository":"repo","requirements":"work while remote CI pending","agent":"fake","test":"pass","publish":false}})).unwrap()).unwrap();
    assert!(app.work_once().unwrap());
    assert_eq!(app.get(task.id).unwrap().state, relay::State::Finished);
    let development: Value =
        serde_json::from_str(app.get(task.id).unwrap().result.as_deref().unwrap()).unwrap();
    assert_eq!(development["outcome"], "success");
    assert_eq!(development["agent"]["stdout"], "developed\n");
    assert_eq!(development["tests"]["outcome"], "success");
    let other = Application::open(root.path().join("relay.db"), app.config.clone()).unwrap();
    let live = other.ci_get(track.id).unwrap();
    assert!(
        other
            .confirm_ci_stopped(track.id, live.attempt, true)
            .is_err()
    );
    let stopping = other
        .ci_stop(
            track.id,
            CiControlRequest {
                expected_revision: live.revision,
            },
        )
        .unwrap();
    assert!(stopping.stop_requested);
    assert!(handle.join().unwrap().unwrap());
    let stopped = app.ci_get(track.id).unwrap();
    assert_eq!(stopped.status, "stopped");
    assert!(stopped.latest_evidence.is_none());
    assert!(
        other
            .ci_resume(
                track.id,
                CiControlRequest {
                    expected_revision: live.revision
                }
            )
            .is_err()
    );
}
#[test]
fn restart_guard_is_unknown_until_exact_local_confirmation_then_explicit_resume() {
    let root = TempDir::new().unwrap();
    let mut config = config(root.path());
    config.max_retained_workspaces = 1;
    let id = source(root.path(), publication_result());
    let app = Application::open(root.path().join("relay.db"), config).unwrap();
    let track = start(&app, id);
    let root_path = ci_dir(root.path());
    let db = root.path().join("relay.db").canonicalize().unwrap();
    let identity = format!("{:x}", Sha256::digest(db.as_os_str().as_encoded_bytes()));
    // Full published guard, before reservation commit. No process is started.
    fs::write(
        root_path.join(".ci-observation-in-flight"),
        json!({"database":identity,"track_id":track.id,"attempt":1}).to_string(),
    )
    .unwrap();
    let restarted = Application::open(&db, app.config.clone()).unwrap();
    assert!(!restarted.ci_work_once().unwrap());
    let unknown = restarted.ci_get(track.id).unwrap();
    assert_eq!(unknown.status, "process_unknown");
    assert_eq!(unknown.attempt, 0);
    let development = restarted.submit(serde_json::from_value::<Submission>(json!({"key":"development-during-unknown-ci","job":{"repository":"repo","requirements":"continue despite independent CI recovery","agent":"fake","test":"pass","publish":false}})).unwrap()).unwrap();
    assert!(restarted.work_once().unwrap());
    assert_eq!(
        serde_json::from_str::<Value>(
            restarted
                .get(development.id)
                .unwrap()
                .result
                .as_deref()
                .unwrap()
        )
        .unwrap()["outcome"],
        "success"
    );
    assert!(
        restarted
            .ci_resume(
                track.id,
                CiControlRequest {
                    expected_revision: unknown.revision
                }
            )
            .is_err()
    );
    assert!(restarted.confirm_ci_stopped(track.id, 2, true).is_err());
    let guard = fs::File::open(root_path.join(".ci-observation-in-flight")).unwrap();
    assert_eq!(
        unsafe { libc::flock(guard.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert!(restarted.confirm_ci_stopped(track.id, 1, true).is_err());
    drop(guard);
    let stopped = restarted.confirm_ci_stopped(track.id, 1, true).unwrap();
    assert_eq!(stopped.status, "stopped");
    assert_eq!(stopped.attempt, 1);
    assert!(!restarted.ci_work_once().unwrap());
    let resumed = restarted
        .ci_resume(
            track.id,
            CiControlRequest {
                expected_revision: stopped.revision,
            },
        )
        .unwrap();
    assert_eq!(resumed.status, "watching");
    let passed = observe(root.path(), &restarted, track.id, evidence());
    assert_eq!(passed.attempt, 2);
    assert_eq!(passed.status, "configured_checks_passed");
}
#[test]
fn unknown_supervisor_retains_guard_and_temp_without_touching_core() {
    let root = TempDir::new().unwrap();
    let mut config = config(root.path());
    let fake = root.path().join("supervisor.py");
    fs::write(&fake,"#!/usr/bin/python3\nimport json,sys\njson.loads(sys.stdin.readline())\nprint(json.dumps({'outcome':'unknown','exit_code':None,'signal':None,'stdout':'','stderr':'','stdout_truncated':False,'stderr_truncated':False,'duration_ms':0,'supervisor_pid':None,'error':'fixture unknown'}))\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    config.supervisor_program = Some(fake);
    let id = source(root.path(), publication_result());
    let app = Application::open(root.path().join("relay.db"), config).unwrap();
    let track = start(&app, id);
    assert!(app.ci_work_once().unwrap());
    let unknown = app.ci_get(track.id).unwrap();
    assert_eq!(unknown.status, "process_unknown");
    assert!(
        ci_dir(root.path())
            .join(".ci-observation-in-flight")
            .exists()
    );
    assert!(
        relay::Store::open(root.path().join("relay.db"))
            .unwrap()
            .active_claim()
            .unwrap()
            .is_none()
    );
    assert!(!app.ci_work_once().unwrap());
}
#[test]
fn config_drift_blocks_without_process_and_explicit_resume_window_replay_is_idempotent() {
    let root = TempDir::new().unwrap();
    let (app, id) = app(root.path());
    let track = start(&app, id);
    let stopped = app
        .ci_stop(
            track.id,
            CiControlRequest {
                expected_revision: track.revision,
            },
        )
        .unwrap();
    let resumed = app
        .ci_resume(
            track.id,
            CiControlRequest {
                expected_revision: stopped.revision,
            },
        )
        .unwrap();
    let replay = app
        .ci_resume(
            track.id,
            CiControlRequest {
                expected_revision: stopped.revision,
            },
        )
        .unwrap();
    assert_eq!(resumed.window_generation, replay.window_generation);
    assert_eq!(resumed.deadline, replay.deadline);
    let mut config = app.config.clone();
    config.ci_policies.get_mut("checks").unwrap().app_id += 1;
    let changed = Application::open(root.path().join("relay.db"), config).unwrap();
    assert!(changed.ci_work_once().unwrap());
    let blocked = changed.ci_get(track.id).unwrap();
    assert_eq!(blocked.status, "blocked");
    assert!(!root.path().join("started").exists());
    assert!(
        changed
            .ci_resume(
                track.id,
                CiControlRequest {
                    expected_revision: blocked.revision
                }
            )
            .is_err()
    );
}
#[test]
fn policy_empty_jobs_wrong_event_and_unbounded_values_rejected() {
    for mode in ["empty", "event", "many", "poll", "window"] {
        let root = TempDir::new().unwrap();
        let mut c = config(root.path());
        let p = c.ci_policies.get_mut("checks").unwrap();
        match mode {
            "empty" => p.required_jobs.clear(),
            "event" => p.event = "push".into(),
            "many" => p.required_jobs = (0..9).map(|i| format!("job-{i}")).collect(),
            "poll" => p.poll_interval_seconds = 0,
            _ => p.observation_window_seconds = 86401,
        };
        assert!(
            Application::open(root.path().join("relay.db"), c).is_err(),
            "{mode}"
        );
    }
}
#[test]
fn pr_lifecycle_and_transient_retry_are_bounded_observations() {
    for mode in ["closed", "merged", "transient"] {
        let root = TempDir::new().unwrap();
        let (app, id) = app(root.path());
        let track = start(&app, id);
        let mut e = evidence();
        match mode {
            "closed" => e["pull_request"]["state"] = json!("closed"),
            "merged" => {
                e["pull_request"]["state"] = json!("closed");
                e["pull_request"]["merged"] = json!(true);
            }
            _ => {
                e = json!({"version":1,"observation":"transient_error","complete":false,"repository":null,"pull_request":null,"run":null,"error_code":"service_unavailable","detail":"Read-only service temporarily unavailable","remote_merge_eligibility":"not_established"})
            }
        };
        let observed = observe(root.path(), &app, track.id, e.clone());
        assert_eq!(
            observed.status,
            match mode {
                "closed" => "pr_closed",
                "merged" => "pr_merged",
                _ => "watching",
            }
        );
        if mode == "transient" {
            for _ in 0..3 {
                due(root.path(), track.id);
                observe(root.path(), &app, track.id, e.clone());
            }
            assert_eq!(app.ci_get(track.id).unwrap().status, "blocked");
        }
    }
}

#[test]
fn failed_result_write_retains_guard_and_never_reexecutes_observer() {
    let root = TempDir::new().unwrap();
    let (app, id) = app(root.path());
    let track = start(&app, id);
    let db = rusqlite::Connection::open(root.path().join("relay.db")).unwrap();
    db.execute_batch("CREATE TRIGGER ci_fail_result BEFORE UPDATE ON app_ci_tracks WHEN NEW.status='configured_checks_passed' BEGIN SELECT RAISE(ABORT,'fixture result persistence failed'); END;").unwrap();
    assert!(app.ci_work_once().is_err());
    assert!(
        ci_dir(root.path())
            .join(".ci-observation-in-flight")
            .exists()
    );
    let old = fs::read_to_string(root.path().join("started")).unwrap();
    assert!(!app.ci_work_once().unwrap());
    let unknown = app.ci_get(track.id).unwrap();
    assert_eq!(unknown.status, "process_unknown");
    assert_eq!(
        fs::read_to_string(root.path().join("started")).unwrap(),
        old
    );
    assert!(unknown.latest_evidence.is_none());
    db.execute_batch("DROP TRIGGER ci_fail_result").unwrap();
    let stopped = app
        .confirm_ci_stopped(track.id, unknown.attempt, true)
        .unwrap();
    assert_eq!(stopped.status, "stopped");
}
#[test]
fn stale_writer_is_fenced_and_guard_retained_after_known_cancellation() {
    let root = TempDir::new().unwrap();
    let (app, id) = app(root.path());
    let track = start(&app, id);
    fs::write(root.path().join("sleep"), "").unwrap();
    let worker = Arc::clone(&app);
    let handle = std::thread::spawn(move || worker.ci_work_once());
    wait_started(root.path());
    let db = rusqlite::Connection::open(root.path().join("relay.db")).unwrap();
    let raw: String = db
        .query_row(
            "SELECT record FROM app_ci_tracks WHERE id=?1",
            [track.id],
            |r| r.get(0),
        )
        .unwrap();
    let mut raw: Value = serde_json::from_str(&raw).unwrap();
    let revision = raw["view"]["revision"].as_u64().unwrap() + 7;
    raw["view"]["revision"] = json!(revision);
    db.execute(
        "UPDATE app_ci_tracks SET revision=?2,record=?3 WHERE id=?1",
        rusqlite::params![track.id, revision, raw.to_string()],
    )
    .unwrap();
    assert!(handle.join().unwrap().is_err());
    assert_eq!(app.ci_get(track.id).unwrap().revision, revision);
    assert!(app.ci_get(track.id).unwrap().latest_evidence.is_none());
    assert!(
        ci_dir(root.path())
            .join(".ci-observation-in-flight")
            .exists()
    );
    let until = Instant::now() + Duration::from_secs(6);
    while app.ci_get(track.id).unwrap().status != "process_unknown" {
        assert!(Instant::now() < until);
        assert!(!app.ci_work_once().unwrap());
        std::thread::sleep(Duration::from_millis(20));
    }
}
#[test]
fn incomplete_staging_orphan_is_inert_and_pending_window_expires_on_deadline() {
    let root = TempDir::new().unwrap();
    let mut config = config(root.path());
    config
        .ci_policies
        .get_mut("checks")
        .unwrap()
        .poll_interval_seconds = 3600;
    config
        .ci_policies
        .get_mut("checks")
        .unwrap()
        .observation_window_seconds = 60;
    let id = source(root.path(), publication_result());
    let app = Application::open(root.path().join("relay.db"), config).unwrap();
    let track = start(&app, id);
    fs::write(ci_dir(root.path()).join(".ci-guard-stage-dead"), "{").unwrap();
    let mut e = evidence();
    e["run"] = Value::Null;
    let pending = observe(root.path(), &app, track.id, e);
    assert_eq!(pending.status, "watching");
    assert_eq!(pending.next_poll_at, pending.deadline);
    assert!(
        !ci_dir(root.path())
            .join(".ci-observation-in-flight")
            .exists()
    );
    let db = rusqlite::Connection::open(root.path().join("relay.db")).unwrap();
    let raw: String = db
        .query_row(
            "SELECT record FROM app_ci_tracks WHERE id=?1",
            [track.id],
            |r| r.get(0),
        )
        .unwrap();
    let mut raw: Value = serde_json::from_str(&raw).unwrap();
    raw["view"]["deadline"] = json!(1);
    raw["view"]["next_poll_at"] = json!(0);
    db.execute(
        "UPDATE app_ci_tracks SET next_poll_at=0,record=?2 WHERE id=?1",
        rusqlite::params![track.id, raw.to_string()],
    )
    .unwrap();
    assert!(app.ci_work_once().unwrap());
    let expired = app.ci_get(track.id).unwrap();
    assert_eq!(expired.status, "expired");
    let resumed = app
        .ci_resume(
            track.id,
            CiControlRequest {
                expected_revision: expired.revision,
            },
        )
        .unwrap();
    assert_eq!(resumed.window_generation, 2);
    assert!(resumed.deadline > expired.deadline);
    assert_eq!(resumed.head_sha, expired.head_sha);
    assert_eq!(resumed.observed_base_sha, expired.observed_base_sha);
    assert_eq!(resumed.last_observed_at, expired.last_observed_at);
}
#[test]
fn service_death_does_not_release_live_supervisor_inherited_guard() {
    use std::process::{Command, Stdio};
    let root = TempDir::new().unwrap();
    let mut config = config(root.path());
    let fake = root.path().join("persistent-supervisor.py");
    fs::write(&fake,"#!/usr/bin/python3\nimport json,pathlib,sys,time\njson.loads(sys.stdin.readline())\npathlib.Path(__file__).with_name('supervisor_ready').write_text('ready')\ntime.sleep(3)\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    config.supervisor_program = Some(fake);
    let id = source(root.path(), publication_result());
    let app = Application::open(root.path().join("relay.db"), config.clone()).unwrap();
    let track = start(&app, id);
    fs::write(
        root.path().join("config.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let mut service = Command::new(env!("CARGO_BIN_EXE_relay-app"))
        .arg("serve")
        .arg(root.path().join("config.json"))
        .arg(root.path().join("relay.db"))
        .arg("127.0.0.1:0")
        .env("RELAY_TOKEN", "ci-test-token-00000000000000000000000")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while !root.path().join("supervisor_ready").exists() {
        if Instant::now() > until {
            let _ = service.kill();
            panic!("supervisor did not start");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    service.kill().unwrap();
    service.wait().unwrap();
    let live = app.ci_get(track.id).unwrap();
    assert_eq!(live.attempt, 1);
    assert!(
        app.confirm_ci_stopped(track.id, 1, true)
            .unwrap_err()
            .to_string()
            .contains("live")
    );
    assert!(!app.ci_work_once().unwrap());
    std::thread::sleep(Duration::from_millis(3200));
    assert!(!app.ci_work_once().unwrap());
    assert_eq!(app.ci_get(track.id).unwrap().status, "process_unknown");
    assert_eq!(
        app.confirm_ci_stopped(track.id, 1, true).unwrap().status,
        "stopped"
    );
}
#[test]
fn legacy_handoff_uses_immutable_request_base_and_preview_survives_missing_observer() {
    let root = TempDir::new().unwrap();
    let mut config = config(root.path());
    let copied = root.path().join("python3");
    fs::copy("/usr/bin/python3", &copied).unwrap();
    config
        .ci_policies
        .get_mut("checks")
        .unwrap()
        .observer
        .program = copied.clone();
    let mut result = publication_result();
    result["workflow"]["publication"]
        .as_object_mut()
        .unwrap()
        .remove("base_branch");
    let payload = json!({"repository":"repo","requirements":"fixture","agent":"fake","publish":true,"continuation":{"workspace_task_id":1,"predecessor_task_id":1,"predecessor_generation":1,"publish_approved":{"request":{"key":"handoff","confirm_publish":true,"accept_prior_test_evidence":true,"candidate_sha":SHA,"github_repository":"example/project","base_branch":"main","draft_pr_adapter":"publisher","publisher_binding":"x"},"base_sha":BASE,"dry_run":false,"predecessor_result":"{}","predecessor_result_sha256":"x","checkpoint_sha256":"x","expires_at_unix_seconds":9999999999_u64}}});
    let mut store = relay::Store::open(root.path().join("relay.db")).unwrap();
    let task = store
        .submit("legacy-handoff", &payload.to_string())
        .unwrap();
    let claim = store.claim_next("fixture").unwrap().unwrap();
    store
        .finish(&claim.claim().unwrap(), &result.to_string())
        .unwrap();
    let app = Application::open(root.path().join("relay.db"), config).unwrap();
    assert_eq!(
        app.ci_preview(task.id).unwrap()["publication"]["base_branch"],
        "main"
    );
    let track = start(&app, task.id);
    fs::remove_file(copied).unwrap();
    let preview = app.ci_preview(task.id).unwrap();
    assert_eq!(preview["eligible"], false);
    assert_eq!(preview["tracks"][0]["id"], track.id);
    assert_eq!(preview["unavailable_policies"].as_array().unwrap().len(), 1);
}

#[test]
fn durable_control_root_refuses_changed_root_and_replaced_inode() {
    let root = TempDir::new().unwrap();
    let (app, id) = app(root.path());
    let track = start(&app, id);
    fs::write(root.path().join("sleep"), "").unwrap();
    let worker = Arc::clone(&app);
    let handle = std::thread::spawn(move || worker.ci_work_once());
    wait_started(root.path());
    let live = app.ci_get(track.id).unwrap();
    let mut changed_config = app.config.clone();
    changed_config.workspace_root = root.path().join("new-workspaces");
    let changed = Application::open(root.path().join("relay.db"), changed_config).unwrap();
    assert_eq!(changed.ci_get(track.id).unwrap().attempt, live.attempt);
    assert_eq!(
        changed.ci_preview(id).unwrap()["lane_diagnostic"]["code"],
        "ci_storage_root_changed"
    );
    assert!(
        changed
            .ci_work_once()
            .unwrap_err()
            .to_string()
            .contains("restore the original workspace_root")
    );
    assert!(
        changed
            .confirm_ci_stopped(track.id, live.attempt, true)
            .unwrap_err()
            .to_string()
            .contains("restore the original workspace_root")
    );
    assert!(
        ci_dir(root.path())
            .join(".ci-observation-in-flight")
            .exists()
    );
    let task=changed.submit(serde_json::from_value::<Submission>(json!({"key":"development-on-current-root","job":{"repository":"repo","requirements":"unrelated development stays available","agent":"fake","test":"pass","publish":false}})).unwrap()).unwrap();
    assert!(changed.work_once().unwrap());
    assert_eq!(
        serde_json::from_str::<Value>(changed.get(task.id).unwrap().result.as_deref().unwrap())
            .unwrap()["outcome"],
        "success"
    );
    app.ci_stop(
        track.id,
        CiControlRequest {
            expected_revision: live.revision,
        },
    )
    .unwrap();
    assert!(handle.join().unwrap().unwrap());
    let storage = ci_dir(root.path());
    fs::rename(&storage, storage.with_extension("preserved")).unwrap();
    fs::create_dir(&storage).unwrap();
    fs::set_permissions(&storage, fs::Permissions::from_mode(0o700)).unwrap();
    let stopped = app.ci_get(track.id).unwrap();
    assert!(
        app.ci_resume(
            track.id,
            CiControlRequest {
                expected_revision: stopped.revision
            }
        )
        .unwrap_err()
        .to_string()
        .contains("replaced")
    );
}
#[test]
fn separate_databases_share_only_validated_namespace_not_process_lane() {
    let first = TempDir::new().unwrap();
    let (one, id) = app(first.path());
    let track = start(&one, id);
    fs::write(first.path().join("sleep"), "").unwrap();
    let worker = Arc::clone(&one);
    let handle = std::thread::spawn(move || worker.ci_work_once());
    wait_started(first.path());
    let second = TempDir::new().unwrap();
    let mut config = config(second.path());
    config.workspace_root = first.path().join("workspaces");
    let second_id = source(second.path(), publication_result());
    let two = Application::open(second.path().join("relay.db"), config).unwrap();
    let second_track = start(&two, second_id);
    assert!(two.ci_work_once().unwrap());
    assert_eq!(
        two.ci_get(second_track.id).unwrap().status,
        "configured_checks_passed"
    );
    let live = one.ci_get(track.id).unwrap();
    one.ci_stop(
        track.id,
        CiControlRequest {
            expected_revision: live.revision,
        },
    )
    .unwrap();
    assert!(handle.join().unwrap().unwrap());
}
#[test]
fn unverified_control_namespace_still_counts_toward_task_retention() {
    let root = TempDir::new().unwrap();
    let mut config = config(root.path());
    config.max_retained_workspaces = 1;
    let app = Application::open(root.path().join("relay.db"), config).unwrap();
    fs::create_dir(root.path().join("workspaces/.ci-tracking")).unwrap();
    fs::set_permissions(
        root.path().join("workspaces/.ci-tracking"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let task=app.submit(serde_json::from_value::<Submission>(json!({"key":"development","job":{"repository":"repo","requirements":"unknown storage remains counted","agent":"fake","test":"pass","publish":false}})).unwrap()).unwrap();
    assert!(app.work_once().unwrap());
    let result = app.get(task.id).unwrap().result.unwrap();
    assert!(result.contains("workspace retention limit reached"));
}
