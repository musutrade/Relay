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
import json, sys
if '--version' in sys.argv:
    print('2.1.259'); sys.exit()
if '--help' in sys.argv:
    print('--output-format --verbose --permission-prompts --restricted --tools --allowedTools --disallowedTools --disable-slash-commands --strict-mcp-config --mcp-config --no-session-persistence'); sys.exit()
sys.stdin.read()
print(json.dumps({'type':'result','subtype':'success','is_error':False,'result':'interrupted review without a verdict','permission_denials':[]}))
"#).unwrap();
    fs::set_permissions(&reviewer, fs::Permissions::from_mode(0o700)).unwrap();
    let config: HostConfig = serde_json::from_value(json!({
        "workspace_root":root.path().join("runs"),"repositories":{"fixture":source},
        "agents":{"developer":{"program":"/bin/sh","args":["-c","printf candidate > candidate.txt"]}},
        "native_agents":{"reviewer":{"provider":"claude_cli","program":reviewer}},
        "tests":{"check":{"program":"/bin/true"}},
        "workflows":{"checked":{"repository":"fixture","developer":"developer","reviewer":"reviewer","test":"check"}},
        "supervisor_program":env!("CARGO_BIN_EXE_relay-app"),"timeout_seconds":10
    })).unwrap();
    let app = Application::open(root.path().join("relay.db"), config).unwrap();
    let submission: Submission = serde_json::from_value(json!({"key":"original","job":{
        "repository":"fixture","requirements":"Preserve the tested candidate","agent":"developer","workflow":"checked"
    }})).unwrap();
    app.submit(submission).unwrap();
    assert!(app.work_once().unwrap());
    let result: Value =
        serde_json::from_str(app.get(1).unwrap().result.as_deref().unwrap()).unwrap();
    assert_eq!(result["outcome"], "failure", "{result}");
    assert_eq!(result["tests"]["outcome"], "success", "{result}");
    assert!(result["workflow"]["rounds"][0]["reviewer"].is_object());
    (root, app)
}
fn request() -> Value {
    json!({"key":"review-retry","confirm_stopped_and_reconciled":true,"revalidate_tests":true,"review_focus":"Check the preserved implementation"})
}
async fn post(router: axum::Router, body: Value, authorized: bool) -> axum::response::Response {
    let mut request = Request::builder()
        .method("POST")
        .uri("/api/tasks/1/continue-review")
        .header("content-type", "application/json");
    if authorized {
        request = request.header("authorization", format!("Bearer {TOKEN}"));
    }
    router
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}
fn call(app: &Application, name: &str, arguments: Value) -> Value {
    mcp::handle(app, json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}})).unwrap()
}
fn content(response: &Value) -> Value {
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn review_endpoint_requires_auth_both_confirmations_and_bounded_focus() {
    let (_root, app) = fixture();
    let router = http::router(app.clone(), TOKEN.into()).unwrap();
    assert_eq!(
        post(router.clone(), request(), false).await.status(),
        StatusCode::UNAUTHORIZED
    );
    for field in ["confirm_stopped_and_reconciled", "revalidate_tests"] {
        let mut body = request();
        body[field] = json!(false);
        assert_eq!(
            post(router.clone(), body, true).await.status(),
            StatusCode::BAD_REQUEST
        );
        let mut body = request();
        body.as_object_mut().unwrap().remove(field);
        assert!(
            post(router.clone(), body, true)
                .await
                .status()
                .is_client_error()
        );
    }
    for focus in ["".to_string(), "   ".to_string(), "界".repeat(2731)] {
        let mut body = request();
        body["review_focus"] = json!(focus);
        assert_eq!(
            post(router.clone(), body, true).await.status(),
            StatusCode::BAD_REQUEST
        );
    }
    let mut body = request();
    body["untrusted_mode"] = json!("skip_tests");
    assert_eq!(
        post(router.clone(), body, true).await.status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(app.list(None).unwrap().len(), 1);
    assert!(app.get_view(1).unwrap().continuation_status.is_none());

    let original = app.get(1).unwrap();
    let mut body = request();
    body["review_focus"] = json!("界".repeat(2730) + "ab");
    let response = post(router.clone(), body.clone(), true).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let child: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(child["id"], 2);
    body["id"] = json!(1);
    body["key"] = json!("mcp-review-different-key");
    body["review_focus"] = json!("another tab cannot overwrite the first reservation");
    let response = call(&app, "relay_continue_review", body);
    assert_eq!(response["result"]["isError"], false, "{response}");
    assert_eq!(content(&response), child);
    let response = call(
        &app,
        "relay_retry",
        json!({"id":1,"key":"ordinary-cross-mode","confirm_stopped_and_reconciled":true}),
    );
    assert_eq!(response["result"]["isError"], false);
    assert_eq!(content(&response), child);
    assert_eq!(app.list(None).unwrap().len(), 2);
    assert_eq!(app.get(1).unwrap(), original);
    assert_eq!(
        app.get_view(1)
            .unwrap()
            .continuation_status
            .unwrap()
            .successor_id,
        Some(2)
    );
}

#[tokio::test]
async fn mcp_review_schema_validation_notifications_and_cross_mode_recovery() {
    let (_root, app) = fixture();
    let schema = mcp::handle(&app, json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).unwrap();
    let tool = schema["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "relay_continue_review")
        .unwrap();
    assert_eq!(
        tool["inputSchema"]["properties"]["revalidate_tests"]["const"],
        true
    );
    assert_eq!(
        tool["inputSchema"]["properties"]["review_focus"]["maxLength"],
        8192
    );
    assert_eq!(tool["inputSchema"]["additionalProperties"], false);
    let mut args = request();
    args["id"] = json!(1);
    assert!(mcp::handle(&app, json!({"jsonrpc":"2.0","method":"tools/call","params":{"name":"relay_continue_review","arguments":args}})).is_none());
    for field in ["confirm_stopped_and_reconciled", "revalidate_tests"] {
        let mut invalid = args.clone();
        invalid[field] = json!(false);
        assert_eq!(
            call(&app, "relay_continue_review", invalid)["result"]["isError"],
            true
        );
        let mut invalid = args.clone();
        invalid.as_object_mut().unwrap().remove(field);
        assert_eq!(
            call(&app, "relay_continue_review", invalid)["result"]["isError"],
            true
        );
    }
    for id in [json!(0), json!(-1), json!(1.5), json!("1")] {
        let mut invalid = args.clone();
        invalid["id"] = id;
        assert_eq!(
            call(&app, "relay_continue_review", invalid)["result"]["isError"],
            true
        );
    }
    for focus in [json!(""), json!("界".repeat(2731)), json!(123)] {
        let mut invalid = args.clone();
        invalid["review_focus"] = focus;
        assert_eq!(
            call(&app, "relay_continue_review", invalid)["result"]["isError"],
            true
        );
    }
    let mut invalid = args.clone();
    invalid["unexpected"] = json!(true);
    assert_eq!(
        call(&app, "relay_continue_review", invalid)["result"]["isError"],
        true
    );
    assert_eq!(app.list(None).unwrap().len(), 1);

    let ordinary = call(
        &app,
        "relay_retry",
        json!({"id":1,"key":"ordinary-first","confirm_stopped_and_reconciled":true}),
    );
    assert_eq!(ordinary["result"]["isError"], false);
    let router = http::router(app.clone(), TOKEN.into()).unwrap();
    let mut body = request();
    body.as_object_mut().unwrap().remove("review_focus");
    let response = post(router, body, true).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let child: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(child, content(&ordinary));
    assert_eq!(app.list(None).unwrap().len(), 2);
}
