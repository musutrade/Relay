#![cfg(target_os = "linux")]
use relay_app::{
    Application, Submission,
    ci_tracking::CiStartRequest,
    host::HostConfig,
    merge_authorization::{MergeAuthorization, MergeAuthorizeRequest, MergeControlRequest},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::{fd::AsRawFd, unix::fs::PermissionsExt},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tempfile::TempDir;
const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const BASE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const NEW_BASE: &str = "cccccccccccccccccccccccccccccccccccccccc";
const UUID: &str = "11111111-2222-4333-8444-555555555555";
const ADAPTER: &str = r#"import json,os,pathlib,sys,time,fcntl
r=json.load(sys.stdin)
p=pathlib.Path(os.environ['FIXTURE'])
if os.environ.get('RELAY_CI_OBSERVE')=='1':
 print((p/'ci-response').read_text());sys.exit(0)
assert os.environ['RELAY_MERGE_CONTROL']=='1'
phase=r['operation']
assert os.environ.get('RELAY_MERGE_WRITE')==('1' if phase in ('ready','merge') else '0')
with (p/'calls').open('a') as f:f.write(json.dumps(r)+'\n')
(p/'started').write_text(phase)
while (p/('hold-'+phase)).exists(): time.sleep(.01)
if (p/('decline-before-write-'+phase)).exists():
 print((p/('response-'+phase)).read_text());sys.exit(0)
if phase in ('ready','merge'):
 with open(os.environ['RELAY_MERGE_GATE_PATH'],'r+') as gate:
  fcntl.flock(gate,fcntl.LOCK_SH)
  g=json.load(gate)
  if g['revoked']:
   print(json.dumps({'version':1,'operation':phase,'status':'blocked','complete':False,'effect':'none','ci_observation':None,'target':None,'async_request':None,'merge_commit_sha':None,'error_code':'authorization_revoked','detail':None}));sys.exit(0)
  g['write_started']=True
  gate.seek(0);gate.truncate();json.dump(g,gate);gate.flush();os.fsync(gate.fileno())
  (p/'write-started').write_text(phase)
  while (p/('hold-write-'+phase)).exists(): time.sleep(.01)
print((p/('response-'+phase)).read_text())
"#;
fn config(root: &Path) -> HostConfig {
    fs::create_dir_all(root.join("source")).unwrap();
    fs::write(root.join("source/source.txt"), "unchanged").unwrap();
    fs::write(root.join("adapter.py"), ADAPTER).unwrap();
    fs::write(root.join("ci-response"), evidence().to_string()).unwrap();
    serde_json::from_value(json!({"workspace_root":root.join("workspaces"),"repositories":{"repo":root.join("source")},"agents":{"fake":{"program":"/bin/echo","args":["developed"]}},"tests":{"pass":{"program":"/bin/true"}},"supervisor_program":env!("CARGO_BIN_EXE_relay-app"),"max_retained_workspaces":1,"ci_policies":{"checks":{"github_repository":"example/project","base_branch":"main","observer":{"program":"/usr/bin/python3","args":[root.join("adapter.py")],"env":{"FIXTURE":root,"SECRET":"do-not-persist-this"}},"workflow_id":41,"app_id":15368,"required_jobs":["test"]}},"merge_policies":{"safe":{"ci_policy":"checks","adapter":{"program":"/usr/bin/python3","args":[root.join("adapter.py")],"env":{"FIXTURE":root,"SECRET":"do-not-persist-this"}},"merge_method":"squash","allow_ready":true,"target_guard":"preflight_only","authorization_window_seconds":3600,"poll_interval_seconds":60}}})).unwrap()
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
fn evidence() -> Value {
    json!({"version":1,"observation":"ok","complete":true,"repository":{"id":100,"full_name":"example/project"},"pull_request":{"id":200,"number":7,"url":"https://github.com/example/project/pull/7","state":"open","merged":false,"draft":true,"head_sha":SHA,"head_ref":"relay/task-1-g1","head_repository_id":100,"base_ref":"main","base_sha":BASE,"base_repository_id":100,"mergeable":null},"run":{"id":300,"run_number":10,"run_attempt":1,"workflow_id":41,"event":"pull_request","head_sha":SHA,"head_repository_id":100,"check_suite_id":400,"app_id":15368,"status":"completed","conclusion":"success","pull_request_ids":[200],"jobs":[{"id":500,"check_run_id":600,"name":"test","head_sha":SHA,"check_suite_id":400,"app_id":15368,"status":"completed","conclusion":"success"}]},"error_code":null,"detail":null,"remote_merge_eligibility":"not_established"})
}

fn app(root: &Path) -> (Arc<Application>, i64) {
    let c = config(root);
    let id = source(root, publication_result());
    let app = Application::open(root.join("relay.db"), c).unwrap();
    let p = app.ci_preview(id).unwrap();
    let t=app.ci_start(id,serde_json::from_value::<CiStartRequest>(json!({"key":"track","policy":"checks","policy_digest":p["policies"][0]["policy_digest"]})).unwrap()).unwrap();
    assert!(app.ci_work_once().unwrap());
    assert_eq!(app.ci_get(t.id).unwrap().observed_pr_id, Some(200));
    (app, id)
}
fn request(app: &Application, id: i64, key: &str, ready: bool) -> MergeAuthorizeRequest {
    let v = app.merge_preview(id).unwrap();
    let p = &v["policies"][0];
    serde_json::from_value(json!({"key":key,"policy":p["name"],"policy_digest":p["policy_digest"],"ci_track_id":p["ci_track_id"],"scope_digest":p["scope_digest"],"deadline":p["deadline"],"allow_ready":ready,"confirm_merge":true,"accept_non_atomic_target_guard":true,"deadline_semantics":"last_dispatch","accept_existing_automation":true,"risk_disclosure_version":v["risk_disclosure"]["version"],"risk_disclosure_sha256":v["risk_disclosure"]["sha256"]})).unwrap()
}
fn start(app: &Application, id: i64, ready: bool) -> MergeAuthorization {
    app.merge_authorize(id, request(app, id, "authorize", ready))
        .unwrap()
}
fn async_request() -> Value {
    json!({"id":UUID,"options":{"sha":SHA,"merge_method":"squash","merge_action":"direct_merge","bypass_rules":false},"provenance":"relay"})
}
fn response(phase: &str, status: &str, draft: bool, green: bool) -> Value {
    let mut ci = evidence();
    ci["pull_request"]["draft"] = json!(draft);
    ci["pull_request"]["base_sha"] = json!(NEW_BASE);
    if !green {
        ci["run"] = Value::Null;
    }
    json!({"version":1,"operation":phase,"status":status,"complete":true,"effect":match status{"ready_confirmed"=>"ready_confirmed","accepted"=>"merge_request_recorded","merged"=>"merge_confirmed","externally_merged"=>"externally_merged",_=>"none"},"ci_observation":if phase=="reconcile"{Value::Null}else{ci},"target":if phase=="reconcile"{Value::Null}else{json!({"repository_id":100,"pr_id":200,"pr_node_id":"PR_exact","head_sha":SHA,"head_branch":"relay/task-1-g1","base_branch":"main","base_sha":NEW_BASE,"draft":draft,"state":"open","merged":false,"stack_clear":true,"queue_clear":true,"auto_merge_disabled":true,"delete_branch_on_merge":true})},"async_request":if matches!(status,"accepted"|"pending"|"merged"|"failed"|"enqueued"){async_request()}else{Value::Null},"merge_commit_sha":if status=="merged"{json!(NEW_BASE)}else{Value::Null},"error_code":null,"detail":null})
}
fn patch(root: &Path, id: i64, change: impl FnOnce(&mut Value)) {
    let db = rusqlite::Connection::open(root.join("relay.db")).unwrap();
    let raw: String = db
        .query_row(
            "SELECT record FROM app_merge_authorizations WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    let mut v: Value = serde_json::from_str(&raw).unwrap();
    change(&mut v);
    db.execute(
        "UPDATE app_merge_authorizations SET next_poll_at=?2,record=?3 WHERE id=?1",
        rusqlite::params![
            id,
            v["view"]["next_poll_at"].as_u64().unwrap(),
            v.to_string()
        ],
    )
    .unwrap();
}
fn due(root: &Path, id: i64) {
    patch(root, id, |v| v["view"]["next_poll_at"] = json!(0));
}
fn run(root: &Path, app: &Application, id: i64, value: Value) -> MergeAuthorization {
    let phase = value["operation"].as_str().unwrap();
    fs::write(root.join(format!("response-{phase}")), value.to_string()).unwrap();
    due(root, id);
    assert!(app.merge_work_once().unwrap());
    app.merge_get(id).unwrap()
}
fn directory(root: &Path) -> std::path::PathBuf {
    let db = root.join("relay.db").canonicalize().unwrap();
    root.join("workspaces/.ci-tracking").join(format!(
        "{:x}",
        Sha256::digest(db.as_os_str().as_encoded_bytes())
    ))
}
fn calls(root: &Path) -> Vec<Value> {
    fs::read_to_string(root.join("calls"))
        .unwrap_or_default()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}
fn wait_started(root: &Path, phase: &str) {
    let end = Instant::now() + Duration::from_secs(5);
    while fs::read_to_string(root.join("started")).ok().as_deref() != Some(phase) {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn admission_is_default_off_explicit_local_idempotent_and_bounded() {
    let root = TempDir::new().unwrap();
    let (app, id) = app(root.path());
    let preview = app.merge_preview(id).unwrap();
    assert_eq!(preview["eligible"], true);
    assert!(!root.path().join("calls").exists());
    let input = request(&app, id, "authorize", true);
    let auth = app.merge_authorize(id, input.clone()).unwrap();
    assert_eq!(auth.attempt, 0);
    assert!(auth.resolved_pr_node_id.is_none());
    assert_eq!(app.merge_authorize(id, input.clone()).unwrap().id, auth.id);
    assert!(
        app.merge_authorize(id, request(&app, id, "other", true))
            .is_err()
    );
    let mut wrong = input.clone();
    wrong.confirm_merge = false;
    assert!(app.merge_authorize(id, wrong).is_err());
    let mut wrong = input.clone();
    wrong.scope_digest = "0".repeat(64);
    assert!(app.merge_authorize(id, wrong).is_err());
    let raw: String = rusqlite::Connection::open(root.path().join("relay.db"))
        .unwrap()
        .query_row("SELECT record FROM app_merge_authorizations", [], |r| {
            r.get(0)
        })
        .unwrap();
    for hidden in ["do-not-persist-this", "adapter.py", "FIXTURE"] {
        assert!(!raw.contains(hidden));
        assert!(!preview.to_string().contains(hidden));
    }
    let mut c = app.config.clone();
    c.merge_policies.clear();
    let disabled = Application::open(root.path().join("relay.db"), c).unwrap();
    assert_eq!(disabled.merge_preview(id).unwrap()["eligible"], false);
    assert_eq!(disabled.merge_get(auth.id).unwrap().id, auth.id);
    assert!(
        relay::Store::open(root.path().join("relay.db"))
            .unwrap()
            .active_claim()
            .unwrap()
            .is_none()
    );
}
#[test]
fn pending_ready_fresh_ci_async_acceptance_and_late_merge_leave_core_free() {
    let root = TempDir::new().unwrap();
    let (app, id) = app(root.path());
    let auth = start(&app, id, true);
    let p = run(
        root.path(),
        &app,
        auth.id,
        response("preflight", "waiting_ci", true, false),
    );
    assert_eq!(p.status, "watching");
    assert_eq!(p.resolved_pr_node_id.as_deref(), Some("PR_exact"));
    let ready = run(
        root.path(),
        &app,
        auth.id,
        response("ready", "ready_confirmed", true, false),
    );
    assert!(ready.ready_dispatched);
    assert!(!ready.merge_dispatched);
    run(
        root.path(),
        &app,
        auth.id,
        response("preflight", "preflight_ready", false, true),
    );
    let accepted = run(
        root.path(),
        &app,
        auth.id,
        response("merge", "accepted", false, true),
    );
    assert_eq!(accepted.status, "accepted");
    assert_ne!(accepted.remote_merge_eligibility, "merged");
    assert_eq!(accepted.async_request.as_ref().unwrap().id, UUID);
    assert!(
        !directory(root.path())
            .join(".merge-authorization-in-flight")
            .exists()
    );
    let task=app.submit(serde_json::from_value::<Submission>(json!({"key":"parallel","job":{"repository":"repo","requirements":"develop while remote merge pending","agent":"fake","test":"pass","publish":false}})).unwrap()).unwrap();
    assert!(app.work_once().unwrap());
    assert_eq!(
        serde_json::from_str::<Value>(app.get(task.id).unwrap().result.as_ref().unwrap()).unwrap()
            ["outcome"],
        "success"
    );
    let revoked = app
        .merge_revoke(
            auth.id,
            MergeControlRequest {
                expected_revision: accepted.revision,
            },
        )
        .unwrap();
    assert!(revoked.revoked);
    assert_eq!(revoked.status, "accepted");
    patch(root.path(), auth.id, |v| v["view"]["deadline"] = json!(1));
    let pending = run(
        root.path(),
        &app,
        auth.id,
        response("reconcile", "pending", false, true),
    );
    assert_eq!(pending.status, "pending");
    let merged = run(
        root.path(),
        &app,
        auth.id,
        response("reconcile", "merged", false, true),
    );
    assert_eq!(merged.status, "merged");
    assert!(merged.revoked);
    assert_eq!(merged.merge_commit_sha.as_deref(), Some(NEW_BASE));
    assert_eq!(
        calls(root.path())
            .iter()
            .filter(|v| v["operation"] == "merge")
            .count(),
        1
    );
    assert_eq!(
        app.ci_get(auth.ci_track_id)
            .unwrap()
            .observed_base_sha
            .as_deref(),
        Some(BASE)
    );
}
#[test]
fn consent_before_ci_passes_requires_numeric_identity_but_not_node_metadata() {
    let root = TempDir::new().unwrap();
    let c = config(root.path());
    let id = source(root.path(), publication_result());
    let app = Application::open(root.path().join("relay.db"), c).unwrap();
    let p = app.ci_preview(id).unwrap();
    let t=app.ci_start(id,serde_json::from_value(json!({"key":"ci","policy":"checks","policy_digest":p["policies"][0]["policy_digest"]})).unwrap()).unwrap();
    assert_eq!(app.merge_preview(id).unwrap()["eligible"], false);
    let mut pending = evidence();
    pending["run"] = Value::Null;
    fs::write(root.path().join("ci-response"), pending.to_string()).unwrap();
    app.ci_work_once().unwrap();
    assert_eq!(app.ci_get(t.id).unwrap().status, "watching");
    assert_eq!(app.merge_preview(id).unwrap()["eligible"], true);
    let auth = start(&app, id, true);
    assert!(auth.target.pr_node_id.is_none());
}
#[test]
fn stale_green_new_attempt_or_target_drift_never_dispatches_merge() {
    for mode in [
        "pending",
        "new_attempt_failure",
        "head",
        "base_branch",
        "node",
        "stack",
        "queue",
        "auto_merge",
        "source",
    ] {
        let root = TempDir::new().unwrap();
        let (app, id) = app(root.path());
        let auth = start(&app, id, false);
        let mut p = response("preflight", "preflight_ready", false, true);
        match mode {
            "pending" => {
                p["status"] = json!("waiting_ci");
                p["ci_observation"]["run"] = Value::Null;
            }
            "new_attempt_failure" => {
                p["status"] = json!("waiting_ci");
                p["ci_observation"]["run"]["run_attempt"] = json!(2);
                p["ci_observation"]["run"]["jobs"][0]["conclusion"] = json!("failure");
            }
            "head" => p["target"]["head_sha"] = json!(BASE),
            "base_branch" => p["target"]["base_branch"] = json!("changed"),
            "node" => {
                run(
                    root.path(),
                    &app,
                    auth.id,
                    response("preflight", "waiting_ci", false, false),
                );
                p["target"]["pr_node_id"] = json!("PR_different");
            }
            "stack" => p["target"]["stack_clear"] = json!(false),
            "queue" => p["target"]["queue_clear"] = json!(false),
            "auto_merge" => p["target"]["auto_merge_disabled"] = json!(false),
            _ => p["ci_observation"]["run"]["workflow_id"] = json!(42),
        }
        let out = run(root.path(), &app, auth.id, p);
        assert!(!out.merge_dispatched, "{mode}");
        assert!(
            matches!(out.status.as_str(), "watching" | "blocked"),
            "{mode}: {}",
            out.status
        );
    }
}
#[test]
fn malformed_uuid_options_and_missing_receipts_are_never_success_or_retry() {
    for mode in [
        "missing_uuid",
        "options",
        "uuid",
        "truncated",
        "provenance",
        "accepted_merged",
    ] {
        let root = TempDir::new().unwrap();
        let (app, id) = app(root.path());
        let auth = start(&app, id, false);
        run(
            root.path(),
            &app,
            auth.id,
            response("preflight", "preflight_ready", false, true),
        );
        let mut r = response("merge", "accepted", false, true);
        match mode {
            "missing_uuid" => r["async_request"] = Value::Null,
            "options" => r["async_request"]["options"]["bypass_rules"] = json!(true),
            "uuid" => r["async_request"]["id"] = json!("not-a-uuid"),
            "truncated" => r["complete"] = json!(false),
            "provenance" => r["async_request"]["provenance"] = json!("external_unknown"),
            _ => {
                r["status"] = json!("merged");
                r["effect"] = json!("merge_confirmed");
                r["merge_commit_sha"] = json!(BASE);
            }
        }
        let out = run(root.path(), &app, auth.id, r);
        assert_eq!(out.status, "effect_unknown", "{mode}");
        assert!(
            app.merge_authorize(id, request(&app, id, "new-key", false))
                .is_err()
        );
        assert!(!app.merge_work_once().unwrap());
        assert_eq!(
            calls(root.path())
                .iter()
                .filter(|v| v["operation"] == "merge")
                .count(),
            1
        );
    }
}
#[test]
fn expiry_and_revocation_before_dispatch_allow_fresh_linked_consent_only() {
    for revoke in [true, false] {
        let root = TempDir::new().unwrap();
        let (app, id) = app(root.path());
        let input = request(&app, id, "authorize", false);
        let auth = app.merge_authorize(id, input.clone()).unwrap();
        if revoke {
            app.merge_revoke(
                auth.id,
                MergeControlRequest {
                    expected_revision: auth.revision,
                },
            )
            .unwrap();
        } else {
            patch(root.path(), auth.id, |v| v["view"]["deadline"] = json!(1));
            assert!(app.merge_work_once().unwrap());
        }
        assert!(calls(root.path()).is_empty());
        assert_eq!(app.merge_authorize(id, input).unwrap().id, auth.id);
        assert_eq!(app.merge_preview(id).unwrap()["eligible"], true);
        let second = app
            .merge_authorize(id, request(&app, id, "second-consent", false))
            .unwrap();
        assert_ne!(second.id, auth.id);
        assert_eq!(second.previous_authorization_id, Some(auth.id));
        assert_eq!(app.merge_for_task(id).unwrap().len(), 2);
    }
}
#[test]
fn rejected_ready_consent_and_policy_drift_cannot_start_effects() {
    let root = TempDir::new().unwrap();
    let (app, id) = app(root.path());
    let auth = start(&app, id, false);
    let blocked = run(
        root.path(),
        &app,
        auth.id,
        response("preflight", "waiting_ci", true, false),
    );
    assert_eq!(blocked.status, "blocked");
    assert!(!blocked.ready_dispatched);
    let root = TempDir::new().unwrap();
    let (app, id) = self::app(root.path());
    let auth = start(&app, id, true);
    let mut changed = app.config.clone();
    changed
        .merge_policies
        .get_mut("safe")
        .unwrap()
        .adapter
        .env
        .insert("NEW".into(), "value".into());
    let drift = Application::open(root.path().join("relay.db"), changed).unwrap();
    assert!(drift.merge_work_once().unwrap());
    assert_eq!(drift.merge_get(auth.id).unwrap().status, "blocked");
    assert!(calls(root.path()).is_empty());
}
#[test]
fn confirmed_ready_and_confirmed_failed_receipt_can_renew_but_unknown_cannot() {
    for mode in ["ready", "failed", "unknown_ready"] {
        let root = TempDir::new().unwrap();
        let (app, id) = app(root.path());
        let auth = start(&app, id, true);
        if mode == "failed" {
            run(
                root.path(),
                &app,
                auth.id,
                response("preflight", "preflight_ready", false, true),
            );
            run(
                root.path(),
                &app,
                auth.id,
                response("merge", "accepted", false, true),
            );
            let terminal = run(
                root.path(),
                &app,
                auth.id,
                response("reconcile", "failed", false, true),
            );
            assert!(terminal.merge_failed_confirmed);
        } else {
            run(
                root.path(),
                &app,
                auth.id,
                response("preflight", "waiting_ci", true, false),
            );
            let mut ready = response("ready", "ready_confirmed", true, false);
            if mode == "unknown_ready" {
                ready["status"] = json!("effect_unknown");
                ready["complete"] = json!(false);
                ready["effect"] = json!("unknown");
            }
            let out = run(root.path(), &app, auth.id, ready);
            app.merge_revoke(
                auth.id,
                MergeControlRequest {
                    expected_revision: out.revision,
                },
            )
            .unwrap();
        }
        let p = app.merge_preview(id).unwrap();
        assert_eq!(p["eligible"], mode != "unknown_ready", "{mode}");
        if mode != "unknown_ready" {
            let next = app
                .merge_authorize(id, request(&app, id, "fresh-consent", false))
                .unwrap();
            assert_eq!(next.previous_authorization_id, Some(auth.id));
        } else {
            assert!(
                app.merge_authorize(id, request(&app, id, "cannot-bypass", false))
                    .is_err()
            );
        }
    }
}
#[test]
fn readonly_reconciliation_never_restarts_mutation_or_loses_known_no_effect() {
    let root = TempDir::new().unwrap();
    let (app, id) = app(root.path());
    let auth = start(&app, id, false);
    let revoked = app
        .merge_revoke(
            auth.id,
            MergeControlRequest {
                expected_revision: auth.revision,
            },
        )
        .unwrap();
    let observed = app
        .merge_reconcile(
            auth.id,
            MergeControlRequest {
                expected_revision: revoked.revision,
            },
        )
        .unwrap();
    assert_eq!(observed.status, "revoked");
    assert!(!app.merge_work_once().unwrap());
    assert!(calls(root.path()).is_empty());
    let root = TempDir::new().unwrap();
    let (app, id) = self::app(root.path());
    let auth = start(&app, id, false);
    run(
        root.path(),
        &app,
        auth.id,
        response("preflight", "preflight_ready", false, true),
    );
    let mut r = response("merge", "effect_unknown", false, true);
    r["complete"] = json!(false);
    r["effect"] = json!("unknown");
    let unknown = run(root.path(), &app, auth.id, r);
    let replay = app
        .merge_reconcile(
            auth.id,
            MergeControlRequest {
                expected_revision: unknown.revision,
            },
        )
        .unwrap();
    assert!(replay.observation_only);
    let mut r = response("reconcile", "externally_merged", false, true);
    r["merge_commit_sha"] = json!(BASE);
    let merged = run(root.path(), &app, auth.id, r);
    assert_eq!(merged.status, "externally_merged");
    let revoked = app
        .merge_revoke(
            auth.id,
            MergeControlRequest {
                expected_revision: merged.revision,
            },
        )
        .unwrap();
    assert_eq!(revoked.status, "externally_merged");
    assert!(revoked.revoked);
    assert_eq!(
        calls(root.path())
            .iter()
            .filter(|v| v["operation"] == "merge")
            .count(),
        1
    );
}
#[test]
fn failed_result_commit_retains_receipt_uuid_and_local_recovery_never_repeats_write() {
    let root = TempDir::new().unwrap();
    let (app, id) = app(root.path());
    let auth = start(&app, id, false);
    run(
        root.path(),
        &app,
        auth.id,
        response("preflight", "preflight_ready", false, true),
    );
    fs::write(
        root.path().join("response-merge"),
        response("merge", "accepted", false, true).to_string(),
    )
    .unwrap();
    let db = rusqlite::Connection::open(root.path().join("relay.db")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_merge_result BEFORE UPDATE ON app_merge_authorizations WHEN NEW.status='accepted' BEGIN SELECT RAISE(ABORT,'fixture DB commit failure'); END;").unwrap();
    assert!(app.merge_work_once().is_err());
    assert!(
        directory(root.path())
            .join(".merge-authorization-in-flight")
            .exists()
    );
    let until = Instant::now() + Duration::from_secs(3);
    while app.merge_get(auth.id).unwrap().status != "process_unknown" {
        assert!(Instant::now() < until);
        assert!(!app.merge_work_once().unwrap());
        std::thread::sleep(Duration::from_millis(10));
    }
    let unknown = app.merge_get(auth.id).unwrap();
    assert_eq!(unknown.status, "process_unknown");
    assert!(
        app.merge_reconcile(
            auth.id,
            MergeControlRequest {
                expected_revision: unknown.revision
            }
        )
        .is_err()
    );
    assert_eq!(
        calls(root.path())
            .iter()
            .filter(|v| v["operation"] == "merge")
            .count(),
        1
    );
    db.execute_batch("DROP TRIGGER fail_merge_result").unwrap();
    let recovered = app
        .confirm_merge_stopped(auth.id, unknown.attempt, "merge", true)
        .unwrap();
    assert_eq!(recovered.status, "accepted");
    assert_eq!(recovered.async_request.as_ref().unwrap().id, UUID);
    assert!(recovered.observation_only);
    assert!(
        app.merge_authorize(id, request(&app, id, "different", false))
            .is_err()
    );
    let merged = run(
        root.path(),
        &app,
        auth.id,
        response("reconcile", "merged", false, true),
    );
    assert_eq!(merged.status, "merged");
    assert_eq!(
        calls(root.path())
            .iter()
            .filter(|v| v["operation"] == "merge")
            .count(),
        1
    );
}
#[test]
fn local_guard_root_identity_and_wrong_phase_recovery_fail_closed() {
    let root = TempDir::new().unwrap();
    let (app, id) = app(root.path());
    let auth = start(&app, id, false);
    let path = directory(root.path());
    let identity = path.file_name().unwrap().to_str().unwrap();
    fs::write(
        path.join(".merge-authorization-in-flight"),
        json!({"database":identity,"authorization_id":auth.id,"attempt":1,"phase":"preflight"})
            .to_string(),
    )
    .unwrap();
    let guard = fs::File::open(path.join(".merge-authorization-in-flight")).unwrap();
    assert_eq!(
        unsafe { libc::flock(guard.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert!(!app.merge_work_once().unwrap());
    assert!(
        app.confirm_merge_stopped(auth.id, 1, "preflight", true)
            .is_err()
    );
    drop(guard);
    assert!(!app.merge_work_once().unwrap());
    assert_eq!(app.merge_get(auth.id).unwrap().status, "process_unknown");
    assert!(
        app.confirm_merge_stopped(auth.id, 1, "merge", true)
            .is_err()
    );
    let mut c = app.config.clone();
    c.workspace_root = root.path().join("elsewhere");
    let other = Application::open(root.path().join("relay.db"), c).unwrap();
    assert!(other.merge_work_once().is_err());
    assert!(
        other
            .confirm_merge_stopped(auth.id, 1, "preflight", true)
            .is_err()
    );
    assert_eq!(other.merge_get(auth.id).unwrap().status, "process_unknown");
    let restored = app
        .confirm_merge_stopped(auth.id, 1, "preflight", true)
        .unwrap();
    assert_eq!(restored.status, "blocked");
    assert!(!app.merge_work_once().unwrap());
    assert!(calls(root.path()).is_empty());
}
#[test]
fn independent_merge_preflight_does_not_claim_or_block_development() {
    let root = TempDir::new().unwrap();
    let (app, id) = app(root.path());
    let auth = start(&app, id, true);
    fs::write(root.path().join("hold-preflight"), "").unwrap();
    fs::write(
        root.path().join("response-preflight"),
        response("preflight", "waiting_ci", true, false).to_string(),
    )
    .unwrap();
    let worker = Arc::clone(&app);
    let handle = std::thread::spawn(move || worker.merge_work_once());
    wait_started(root.path(), "preflight");
    assert!(
        relay::Store::open(root.path().join("relay.db"))
            .unwrap()
            .active_claim()
            .unwrap()
            .is_none()
    );
    let task=app.submit(serde_json::from_value::<Submission>(json!({"key":"core-parallel","job":{"repository":"repo","requirements":"independent","agent":"fake","test":"pass","publish":false}})).unwrap()).unwrap();
    assert!(app.work_once().unwrap());
    assert_eq!(
        serde_json::from_str::<Value>(app.get(task.id).unwrap().result.as_ref().unwrap()).unwrap()
            ["outcome"],
        "success"
    );
    let current = app.merge_get(auth.id).unwrap();
    let second = Application::open(root.path().join("relay.db"), app.config.clone()).unwrap();
    assert!(
        second
            .confirm_merge_stopped(auth.id, current.attempt, "preflight", true)
            .is_err()
    );
    second
        .merge_revoke(
            auth.id,
            MergeControlRequest {
                expected_revision: current.revision,
            },
        )
        .unwrap();
    assert!(handle.join().unwrap().unwrap());
    assert!(!app.merge_work_once().unwrap());
    assert!(!app.merge_get(auth.id).unwrap().merge_dispatched);
}
#[test]
fn revocation_before_write_gate_wins_and_live_gate_never_freezes_other_local_reads() {
    for write_wins in [false, true] {
        let root = TempDir::new().unwrap();
        let (app, id) = app(root.path());
        let auth = start(&app, id, false);
        run(
            root.path(),
            &app,
            auth.id,
            response("preflight", "preflight_ready", false, true),
        );
        fs::write(
            root.path().join("response-merge"),
            response("merge", "accepted", false, true).to_string(),
        )
        .unwrap();
        let hold = if write_wins {
            "hold-write-merge"
        } else {
            "hold-merge"
        };
        fs::write(root.path().join(hold), "").unwrap();
        let w = Arc::clone(&app);
        let worker = std::thread::spawn(move || w.merge_work_once());
        wait_started(root.path(), "merge");
        if write_wins {
            let until = Instant::now() + Duration::from_secs(3);
            while !root.path().join("write-started").exists() {
                assert!(Instant::now() < until);
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let current = app.merge_get(auth.id).unwrap();
        let other = Application::open(root.path().join("relay.db"), app.config.clone()).unwrap();
        let control = Arc::clone(&other);
        let revoke = std::thread::spawn(move || {
            control.merge_revoke(
                auth.id,
                MergeControlRequest {
                    expected_revision: current.revision,
                },
            )
        });
        if write_wins {
            std::thread::sleep(Duration::from_millis(60));
            let began = Instant::now();
            assert_eq!(app.merge_get(auth.id).unwrap().id, auth.id);
            assert!(began.elapsed() < Duration::from_millis(200));
            let task=app.submit(serde_json::from_value::<Submission>(json!({"key":"core-during-gate","job":{"repository":"repo","requirements":"local operation not blocked by revoke flock wait","agent":"fake","test":"pass","publish":false}})).unwrap()).unwrap();
            assert!(app.work_once().unwrap());
            assert_eq!(app.get(task.id).unwrap().state, relay::State::Finished);
            fs::remove_file(root.path().join(hold)).unwrap();
            let result = revoke.join().unwrap();
            assert!(worker.join().unwrap().unwrap());
            let after = app.merge_get(auth.id).unwrap();
            assert_eq!(after.status, "accepted");
            assert_eq!(after.async_request.as_ref().unwrap().id, UUID);
            if result.is_err() {
                app.merge_revoke(
                    auth.id,
                    MergeControlRequest {
                        expected_revision: after.revision,
                    },
                )
                .unwrap();
            }
            assert!(app.merge_get(auth.id).unwrap().revoked);
        } else {
            let revoked = revoke.join().unwrap().unwrap();
            assert!(revoked.revoked);
            assert!(!root.path().join("write-started").exists());
            assert!(worker.join().unwrap().unwrap());
            assert!(!root.path().join("write-started").exists());
            assert!(app.merge_get(auth.id).unwrap().async_request.is_none());
            let completed = app.merge_get(auth.id).unwrap();
            assert_eq!(completed.status, "revoked");
            assert!(completed.merge_write_not_started);
            assert_eq!(app.merge_preview(id).unwrap()["eligible"], true);
            let fresh = app
                .merge_authorize(id, request(&app, id, "safe-after-revoke", false))
                .unwrap();
            assert_eq!(fresh.previous_authorization_id, Some(auth.id));
        }
    }
}
fn supervisor_response(
    root: &Path,
    app: &Application,
    outcome: &str,
    value: Value,
) -> Arc<Application> {
    let fake = root.join("supervisor-fixture.py");
    fs::write(&fake,"#!/usr/bin/python3\nimport json,pathlib,sys\njson.loads(sys.stdin.readline())\nprint(pathlib.Path(__file__).with_name('supervisor-output').read_text())\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(root.join("supervisor-output"),json!({"outcome":outcome,"exit_code":0,"signal":null,"stdout":value.to_string(),"stderr":"","stdout_truncated":false,"stderr_truncated":false,"duration_ms":0,"supervisor_pid":null,"error":null}).to_string()).unwrap();
    let mut c = app.config.clone();
    c.supervisor_program = Some(fake);
    Application::open(root.join("relay.db"), c).unwrap()
}
#[test]
fn complete_accepted_receipts_survive_cancel_timeout_and_unknown_local_cleanup() {
    for outcome in ["cancelled", "timed_out", "unknown"] {
        let root = TempDir::new().unwrap();
        let (app, id) = app(root.path());
        let auth = start(&app, id, false);
        run(
            root.path(),
            &app,
            auth.id,
            response("preflight", "preflight_ready", false, true),
        );
        let fixture = supervisor_response(
            root.path(),
            &app,
            outcome,
            response("merge", "accepted", false, true),
        );
        assert!(fixture.merge_work_once().unwrap());
        let out = fixture.merge_get(auth.id).unwrap();
        assert_eq!(out.async_request.as_ref().unwrap().id, UUID, "{outcome}");
        assert_eq!(out.observed_remote_status.as_deref(), Some("accepted"));
        if outcome == "unknown" {
            assert_eq!(out.status, "process_unknown");
            let recovered = fixture
                .confirm_merge_stopped(auth.id, out.attempt, "merge", true)
                .unwrap();
            assert_eq!(recovered.status, "accepted");
        } else {
            assert_eq!(out.status, "accepted");
        }
        assert!(
            fixture
                .merge_authorize(id, request(&fixture, id, "retry-forbidden", false))
                .is_err()
        );
    }
}
#[test]
fn confirmed_merge_after_expiry_and_cancel_remains_terminal() {
    let root = TempDir::new().unwrap();
    let (app, id) = app(root.path());
    let auth = start(&app, id, false);
    run(
        root.path(),
        &app,
        auth.id,
        response("preflight", "preflight_ready", false, true),
    );
    run(
        root.path(),
        &app,
        auth.id,
        response("merge", "accepted", false, true),
    );
    patch(root.path(), auth.id, |v| {
        v["view"]["deadline"] = json!(1);
        v["view"]["next_poll_at"] = json!(0);
    });
    let fixture = supervisor_response(
        root.path(),
        &app,
        "cancelled",
        response("reconcile", "merged", false, true),
    );
    assert!(fixture.merge_work_once().unwrap());
    let merged = fixture.merge_get(auth.id).unwrap();
    assert_eq!(merged.status, "merged");
    assert_eq!(merged.observed_remote_status.as_deref(), Some("merged"));
    let revoked = fixture
        .merge_revoke(
            auth.id,
            MergeControlRequest {
                expected_revision: merged.revision,
            },
        )
        .unwrap();
    assert_eq!(revoked.status, "merged");
    assert!(revoked.revoked);
}
#[test]
fn replaced_gate_inode_can_never_prove_no_write_or_enable_reauthorization() {
    let root = TempDir::new().unwrap();
    let (app, id) = app(root.path());
    let auth = start(&app, id, false);
    run(
        root.path(),
        &app,
        auth.id,
        response("preflight", "preflight_ready", false, true),
    );
    let mut lost = response("merge", "effect_unknown", false, true);
    lost["complete"] = json!(false);
    lost["effect"] = json!("unknown");
    fs::write(root.path().join("response-merge"), lost.to_string()).unwrap();
    fs::write(root.path().join("hold-write-merge"), "").unwrap();
    let worker = Arc::clone(&app);
    let handle = std::thread::spawn(move || worker.merge_work_once());
    wait_started(root.path(), "merge");
    let until = Instant::now() + Duration::from_secs(3);
    while !root.path().join("write-started").exists() {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(10));
    }
    let current = app.merge_get(auth.id).unwrap();
    let gate =
        directory(root.path()).join(format!(".merge-write-gate-{}-{}", auth.id, current.attempt));
    let mut bytes: Value = serde_json::from_slice(&fs::read(&gate).unwrap()).unwrap();
    bytes["write_started"] = json!(false);
    fs::rename(&gate, gate.with_extension("original")).unwrap();
    fs::write(&gate, bytes.to_string()).unwrap();
    fs::set_permissions(&gate, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        app.merge_revoke(
            auth.id,
            MergeControlRequest {
                expected_revision: current.revision
            }
        )
        .is_err()
    );
    fs::remove_file(root.path().join("hold-write-merge")).unwrap();
    assert!(handle.join().unwrap().unwrap());
    let unknown = app.merge_get(auth.id).unwrap();
    assert_eq!(unknown.status, "process_unknown");
    assert!(!unknown.merge_write_not_started);
    assert_eq!(app.merge_preview(id).unwrap()["eligible"], false);
    let recovered = app
        .confirm_merge_stopped(auth.id, unknown.attempt, "merge", true)
        .unwrap();
    assert_eq!(recovered.status, "effect_unknown");
    assert!(!recovered.merge_write_not_started);
    let revoked = app
        .merge_revoke(
            auth.id,
            MergeControlRequest {
                expected_revision: recovered.revision,
            },
        )
        .unwrap();
    assert_eq!(revoked.status, "effect_unknown");
    assert!(
        app.merge_authorize(id, request(&app, id, "cannot-replay", false))
            .is_err()
    );
}
#[test]
fn delayed_revoke_of_proven_no_write_ready_or_merge_allows_linked_consent() {
    for phase in ["ready", "merge"] {
        let root = TempDir::new().unwrap();
        let (app, id) = app(root.path());
        let auth = start(&app, id, phase == "ready");
        run(
            root.path(),
            &app,
            auth.id,
            response("preflight", "preflight_ready", phase == "ready", true),
        );
        fs::write(
            root.path().join(format!("decline-before-write-{phase}")),
            "",
        )
        .unwrap();
        let declined = if phase == "merge" {
            response("merge", "waiting_ci", false, false)
        } else {
            let mut r = response("ready", "blocked", true, false);
            r["complete"] = json!(false);
            r["effect"] = json!("none");
            r["error_code"] = json!("remote_state_changed");
            r
        };
        let stopped = run(root.path(), &app, auth.id, declined);
        assert_eq!(stopped.status, "blocked");
        assert!(!stopped.revoked);
        if phase == "ready" {
            assert!(stopped.ready_dispatched && stopped.ready_write_not_started);
        } else {
            assert!(stopped.merge_dispatched && stopped.merge_write_not_started);
        }
        let revoked = app
            .merge_revoke(
                auth.id,
                MergeControlRequest {
                    expected_revision: stopped.revision,
                },
            )
            .unwrap();
        assert_eq!(revoked.status, "revoked");
        assert_eq!(app.merge_preview(id).unwrap()["eligible"], true);
        let replacement = app
            .merge_authorize(id, request(&app, id, "fresh-after-safe-decline", false))
            .unwrap();
        assert_eq!(replacement.previous_authorization_id, Some(auth.id));
        assert!(!root.path().join("write-started").exists());
    }
}
#[test]
fn committed_attempt_rejects_wrong_marker_phase_without_losing_precommit_exception() {
    let root = TempDir::new().unwrap();
    let (app, id) = app(root.path());
    let auth = start(&app, id, false);
    patch(root.path(), auth.id, |r| {
        r["view"]["attempt"] = json!(1);
        r["active"] = json!(true);
        r["phase"] = json!("preflight");
    });
    let dir = directory(root.path());
    let marker = dir.join(".merge-authorization-in-flight");
    let mut value = json!({"database":dir.file_name().unwrap().to_str().unwrap(),"authorization_id":auth.id,"attempt":1,"phase":"merge"});
    fs::write(&marker, value.to_string()).unwrap();
    assert!(app.merge_work_once().is_err());
    assert!(
        app.confirm_merge_stopped(auth.id, 1, "merge", true)
            .is_err()
    );
    assert!(marker.exists());
    assert!(calls(root.path()).is_empty());
    value["phase"] = json!("preflight");
    fs::write(&marker, value.to_string()).unwrap();
    assert_eq!(
        app.confirm_merge_stopped(auth.id, 1, "preflight", true)
            .unwrap()
            .status,
        "blocked"
    );
}
