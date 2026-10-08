#![cfg(target_os = "linux")]
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use relay_app::{Application, Submission, host::HostConfig, http, mcp};
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command, sync::Arc};
use tempfile::TempDir;
use tower::ServiceExt;

const TOKEN: &str = "test-only-token-00000000000000000000";
fn git(path: &Path, args: &[&str]) {
    let output = Command::new("/usr/bin/git")
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
    assert!(output.status.success(), "{output:?}");
}
fn fixture() -> (TempDir, Arc<Application>) {
    let root = TempDir::new().unwrap();
    let source = root.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("base.txt"), "base\n").unwrap();
    git(&source, &["init", "--initial-branch=main"]);
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "base"]);
    let reviewer = root.path().join("reviewer.py");
    fs::write(&reviewer, r#"#!/usr/bin/python3
import json, os, sys
if '--version' in sys.argv:
    print('2.1.259'); sys.exit()
if '--help' in sys.argv:
    print('--output-format --verbose --permission-prompts --restricted --tools --allowedTools --disallowedTools --disable-slash-commands --strict-mcp-config --mcp-config --no-session-persistence'); sys.exit()
sys.stdin.read()
print(json.dumps({'type':'result','subtype':'success','is_error':False,'result':json.dumps({'candidate_sha':os.environ['RELAY_CANDIDATE_SHA'],'verdict':'approved','summary':'Checked exact fixture candidate','findings':[]}),'permission_denials':[]}))
"#).unwrap();
    fs::set_permissions(&reviewer, fs::Permissions::from_mode(0o700)).unwrap();
    let publisher = root.path().join("publisher.py");
    fs::write(&publisher, r#"#!/usr/bin/python3
import json, os
print(json.dumps({'dry_run':False,'draft':True,'repository':'example/project','branch':'relay/task-' + os.environ['RELAY_TASK_ID'] + '-g' + os.environ['RELAY_GENERATION'],'candidate_sha':os.environ['RELAY_CANDIDATE_SHA'],'url':'https://github.com/example/project/pull/19','reconciliation_required':False}))
"#).unwrap();
    fs::set_permissions(&publisher, fs::Permissions::from_mode(0o700)).unwrap();
    let config: HostConfig = serde_json::from_value(json!({
        "workspace_root":root.path().join("runs"),"repositories":{"fixture":source},
        "agents":{"developer":{"program":"/bin/sh","args":["-c","printf candidate > candidate.txt"]}},
        "native_agents":{"reviewer":{"provider":"claude_cli","program":reviewer}},
        "tests":{"check":{"program":"/bin/true"}},
        "draft_pr_adapters":{"publish":{"program":publisher,"env":{"RELAY_GITHUB_EXECUTE":"1"}}},
        "ci_policies":{"required-ci":{"github_repository":"example/project","base_branch":"main","workflow_id":41,"app_id":15368,"required_jobs":["check","browser"],"observer":{"program":"/bin/sh","args":["-c",format!("touch {}",root.path().join("observer-called").display())],"env":{"PRIVATE_CI_FIXTURE":"not-public"}}}},
        "workflows":{"checked":{"repository":"fixture","developer":"developer","reviewer":"reviewer","test":"check","draft_pr_adapter":"publish","github_repository":"example/project"}},
        "supervisor_program":env!("CARGO_BIN_EXE_relay-app"),"timeout_seconds":10
    })).unwrap();
    let app = Application::open(root.path().join("relay.db"), config).unwrap();
    let submission: Submission = serde_json::from_value(json!({"key":"original","job":{
        "repository":"fixture","requirements":"Preserve the tested candidate","agent":"developer","workflow":"checked","publish":true,"draft_pr_adapter":"publish"
    }})).unwrap();
    app.submit(submission).unwrap();
    assert!(app.work_once().unwrap());
    let result: Value =
        serde_json::from_str(app.get(1).unwrap().result.as_deref().unwrap()).unwrap();
    assert_eq!(result["outcome"], "success", "{result}");
    assert_eq!(result["tests"]["outcome"], "success", "{result}");
    assert!(result["workflow"]["rounds"][0]["reviewer"].is_object());
    (root, app)
}
fn request(app: &Application) -> Value {
    let preview = app.ci_preview(1).unwrap();
    assert_eq!(preview["eligible"], true, "{preview}");
    json!({"key":"track-once","policy":"required-ci","policy_digest":preview["policies"][0]["policy_digest"]})
}
async fn http_call(
    router: axum::Router,
    path: &str,
    method: &str,
    body: Option<Value>,
    authorized: bool,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    if authorized {
        request = request.header("authorization", format!("Bearer {TOKEN}"));
    }
    let body = if let Some(body) = body {
        request = request.header("content-type", "application/json");
        Body::from(body.to_string())
    } else {
        Body::empty()
    };
    let response = router.oneshot(request.body(body).unwrap()).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}
fn call(app: &Application, name: &str, arguments: Value) -> Value {
    mcp::handle(app, json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}})).unwrap()
}
fn content(response: &Value) -> Value {
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}
#[tokio::test]
async fn ci_transports_are_authenticated_strict_local_only_and_idempotent() {
    let (root, app) = fixture();
    let router = http::router(app.clone(), TOKEN.into()).unwrap();
    let body = request(&app);
    let original = app.get(1).unwrap();
    for (path, method, input) in [
        ("/api/tasks/1/ci-preview", "GET", None),
        ("/api/tasks/1/ci-tracks", "GET", None),
        ("/api/tasks/1/track-ci", "POST", Some(body.clone())),
        ("/api/ci-tracks/1", "GET", None),
        (
            "/api/ci-tracks/1/stop",
            "POST",
            Some(json!({"expected_revision":1})),
        ),
        (
            "/api/ci-tracks/1/resume",
            "POST",
            Some(json!({"expected_revision":1})),
        ),
    ] {
        assert_eq!(
            http_call(router.clone(), path, method, input, false)
                .await
                .0,
            StatusCode::UNAUTHORIZED,
            "{path}"
        );
    }
    let tools = mcp::handle(&app, json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).unwrap();
    for name in [
        "relay_ci_preview",
        "relay_track_ci",
        "relay_ci_get",
        "relay_ci_stop",
        "relay_ci_resume",
    ] {
        let tool = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap();
        assert_eq!(tool["inputSchema"]["additionalProperties"], false, "{name}");
        assert_eq!(
            call(&app, name, json!({"id":1,"unexpected":true}))["result"]["isError"],
            true
        );
        assert_eq!(call(&app, name, json!({"id":0}))["result"]["isError"], true);
    }
    for field in ["key", "policy", "policy_digest"] {
        let mut invalid = body.clone();
        invalid.as_object_mut().unwrap().remove(field);
        assert!(
            http_call(
                router.clone(),
                "/api/tasks/1/track-ci",
                "POST",
                Some(invalid.clone()),
                true
            )
            .await
            .0
            .is_client_error()
        );
        invalid["id"] = json!(1);
        assert_eq!(
            call(&app, "relay_track_ci", invalid)["result"]["isError"],
            true
        );
    }
    let mut invalid = body.clone();
    invalid["head_sha"] = json!("b".repeat(40));
    assert_eq!(
        http_call(
            router.clone(),
            "/api/tasks/1/track-ci",
            "POST",
            Some(invalid.clone()),
            true
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    invalid["id"] = json!(1);
    assert_eq!(
        call(&app, "relay_track_ci", invalid)["result"]["isError"],
        true
    );
    let (status, preview) =
        http_call(router.clone(), "/api/tasks/1/ci-preview", "GET", None, true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(preview["publication"]["pr_number"], 19);
    assert!(!preview.to_string().contains("not-public"));
    assert!(!preview.to_string().contains("observer-called"));
    let (status, track) = http_call(
        router.clone(),
        "/api/tasks/1/track-ci",
        "POST",
        Some(body.clone()),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{track}");
    assert_eq!(track["status"], "watching");
    assert_eq!(track["attempt"], 0);
    assert_eq!(track["remote_merge_eligibility"], "not_established");
    let id = track["id"].as_i64().unwrap();
    assert_eq!(
        http_call(
            router.clone(),
            "/api/tasks/1/track-ci",
            "POST",
            Some(body.clone()),
            true
        )
        .await
        .1,
        track
    );
    let mut arguments = body.clone();
    arguments["id"] = json!(1);
    assert_eq!(content(&call(&app, "relay_track_ci", arguments)), track);
    let mut conflict = body.clone();
    conflict["policy_digest"] = json!("0".repeat(64));
    let (status, error) = http_call(
        router.clone(),
        "/api/tasks/1/track-ci",
        "POST",
        Some(conflict),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["failure"]["code"], "ci_idempotency_conflict");
    let mut duplicate = body;
    duplicate["key"] = json!("another-key");
    assert_eq!(
        http_call(
            router.clone(),
            "/api/tasks/1/track-ci",
            "POST",
            Some(duplicate),
            true
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    for _ in 0..3 {
        assert_eq!(
            http_call(
                router.clone(),
                &format!("/api/ci-tracks/{id}"),
                "GET",
                None,
                true
            )
            .await
            .1,
            track
        );
        assert_eq!(
            content(&call(&app, "relay_ci_get", json!({"id":id}))),
            track
        );
        assert_eq!(
            content(&call(&app, "relay_ci_preview", json!({"id":1})))["tracks"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            http_call(router.clone(), "/api/tasks/1/ci-tracks", "GET", None, true)
                .await
                .1
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    for suffix in ["stop", "resume"] {
        for invalid in [
            json!({}),
            json!({"expected_revision":1,"confirm":true}),
            json!({"expected_revision":-1}),
            json!({"expected_revision":"1"}),
        ] {
            assert!(
                http_call(
                    router.clone(),
                    &format!("/api/ci-tracks/{id}/{suffix}"),
                    "POST",
                    Some(invalid.clone()),
                    true
                )
                .await
                .0
                .is_client_error()
            );
            let mut arguments = invalid;
            arguments["id"] = json!(id);
            assert_eq!(
                call(&app, &format!("relay_ci_{suffix}"), arguments)["result"]["isError"],
                true
            );
        }
    }
    let (status, stopped) = http_call(
        router.clone(),
        &format!("/api/ci-tracks/{id}/stop"),
        "POST",
        Some(json!({"expected_revision":1})),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stopped["status"], "stopped");
    assert_eq!(stopped["stop_requested"], true);
    assert_eq!(
        call(
            &app,
            "relay_ci_resume",
            json!({"id":id,"expected_revision":1})
        )["result"]["isError"],
        true
    );
    let resumed = content(&call(
        &app,
        "relay_ci_resume",
        json!({"id":id,"expected_revision":stopped["revision"]}),
    ));
    assert_eq!(resumed["status"], "watching");
    assert_eq!(resumed["window_generation"], 2);
    assert_eq!(resumed["stop_requested"], false);
    let replay = content(&call(
        &app,
        "relay_ci_resume",
        json!({"id":id,"expected_revision":stopped["revision"]}),
    ));
    assert_eq!(
        replay, resumed,
        "same resume revision must not open a third window"
    );
    assert_eq!(resumed["head_sha"], track["head_sha"]);
    assert_eq!(resumed["policy_digest"], track["policy_digest"]);
    assert!(
        !root.path().join("observer-called").exists(),
        "GETs/start/stop/resume must not launch observer processes"
    );
    assert_eq!(
        app.get(1).unwrap(),
        original,
        "CI controls cannot rewrite original task/result"
    );
    assert_eq!(
        app.status().unwrap()["active"],
        Value::Null,
        "CI wait must not own a queue claim"
    );
    assert_eq!(
        app.list_views(None).unwrap().len(),
        1,
        "CI controls must not create development successors"
    );
}
