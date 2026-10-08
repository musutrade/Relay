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
    let config: HostConfig = serde_json::from_value(json!({
        "workspace_root":root.path().join("runs"),"repositories":{"fixture":source},
        "agents":{"developer":{"program":"/bin/sh","args":["-c","printf candidate > candidate.txt"]}},
        "native_agents":{"reviewer":{"provider":"claude_cli","program":reviewer}},
        "tests":{"check":{"program":"/bin/true"}},
        "draft_pr_adapters":{"publish":{"program":"/bin/true"}},
        "workflows":{"checked":{"repository":"fixture","developer":"developer","reviewer":"reviewer","test":"check","draft_pr_adapter":"publish","github_repository":"example/project"}},
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
    assert_eq!(result["outcome"], "success", "{result}");
    assert_eq!(result["tests"]["outcome"], "success", "{result}");
    assert!(result["workflow"]["rounds"][0]["reviewer"].is_object());
    (root, app)
}
fn request(app: &Application) -> Value {
    let operator = app.operator(1).unwrap();
    let action = operator["recovery"]["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|action| action["id"] == "publish_approved")
        .unwrap();
    let mut request =
        json!({"key":"publication-key","confirm_publish":true,"accept_prior_test_evidence":true});
    for field in [
        "candidate_sha",
        "github_repository",
        "base_branch",
        "draft_pr_adapter",
        "publisher_binding",
    ] {
        request[field] = action[field].clone();
    }
    request
}
async fn post(router: axum::Router, body: Value, authorized: bool) -> axum::response::Response {
    let mut request = Request::builder()
        .method("POST")
        .uri("/api/tasks/1/publish-approved")
        .header("content-type", "application/json");
    if authorized {
        request = request.header("authorization", format!("Bearer {TOKEN}"));
    }
    router
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}
fn call(app: &Application, arguments: Value) -> Value {
    mcp::handle(app, json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"relay_publish_approved","arguments":arguments}})).unwrap()
}
fn content(response: &Value) -> Value {
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn publication_transports_require_exact_scope_confirmations_and_preserve_one_successor() {
    let (_root, app) = fixture();
    let router = http::router(app.clone(), TOKEN.into()).unwrap();
    let body = request(&app);
    let original = app.get(1).unwrap();
    assert_eq!(
        post(router.clone(), body.clone(), false).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let schema = mcp::handle(&app, json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).unwrap();
    let tool = schema["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "relay_publish_approved")
        .unwrap();
    assert_eq!(tool["inputSchema"]["additionalProperties"], false);
    for field in ["confirm_publish", "accept_prior_test_evidence"] {
        assert_eq!(tool["inputSchema"]["properties"][field]["const"], true);
        for value in [Some(json!(false)), None] {
            let mut invalid = body.clone();
            if let Some(value) = value {
                invalid[field] = value;
            } else {
                invalid.as_object_mut().unwrap().remove(field);
            }
            assert!(
                post(router.clone(), invalid.clone(), true)
                    .await
                    .status()
                    .is_client_error()
            );
            invalid["id"] = json!(1);
            assert_eq!(call(&app, invalid)["result"]["isError"], true);
        }
    }
    for field in [
        "candidate_sha",
        "github_repository",
        "base_branch",
        "draft_pr_adapter",
        "publisher_binding",
    ] {
        assert!(
            tool["inputSchema"]["required"]
                .as_array()
                .unwrap()
                .contains(&json!(field))
        );
        for value in [Some(json!("stale-or-unapproved")), None] {
            let mut invalid = body.clone();
            if let Some(value) = value {
                invalid[field] = value;
            } else {
                invalid.as_object_mut().unwrap().remove(field);
            }
            assert!(
                post(router.clone(), invalid.clone(), true)
                    .await
                    .status()
                    .is_client_error(),
                "{field}"
            );
            invalid["id"] = json!(1);
            assert_eq!(call(&app, invalid)["result"]["isError"], true, "{field}");
        }
    }
    let mut invalid = body.clone();
    invalid["publish"] = json!(true);
    assert_eq!(
        post(router.clone(), invalid.clone(), true).await.status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    invalid["id"] = json!(1);
    assert_eq!(call(&app, invalid)["result"]["isError"], true);
    let mut invalid = body.clone();
    invalid["key"] = json!("界".repeat(43));
    assert_eq!(
        post(router.clone(), invalid, true).await.status(),
        StatusCode::BAD_REQUEST
    );
    let mut oversized = body.clone();
    oversized["key"] = json!("x".repeat(96 * 1024));
    assert_eq!(
        post(router.clone(), oversized, true).await.status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    for id in [json!(0), json!(-1), json!(1.5), json!("1")] {
        let mut invalid = body.clone();
        invalid["id"] = id;
        assert_eq!(call(&app, invalid)["result"]["isError"], true);
    }
    let mut args = body.clone();
    args["id"] = json!(1);
    assert!(mcp::handle(&app, json!({"jsonrpc":"2.0","method":"tools/call","params":{"name":"relay_publish_approved","arguments":args}})).is_none());
    assert_eq!(app.list(None).unwrap().len(), 1);
    assert!(app.get_view(1).unwrap().continuation_status.is_none());

    let mut duplicate_key = body.clone();
    duplicate_key["key"] = json!(original.key);
    let response = post(router.clone(), duplicate_key, true).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let failure: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(failure["failure"]["code"], "idempotency_conflict");
    assert!(app.get_view(1).unwrap().continuation_status.is_none());

    let response = post(router.clone(), body.clone(), true).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let child: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(child["id"], 2);
    let payload: Value = serde_json::from_str(child["payload"].as_str().unwrap()).unwrap();
    assert_eq!(payload["publish"], false);
    assert_eq!(payload["continuation"]["publish_approved"]["request"], body);
    let response = call(&app, args.clone());
    assert_eq!(response["result"]["isError"], false, "{response}");
    assert_eq!(content(&response), child);
    let response = post(router.clone(), body.clone(), true).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let mut changed = body.clone();
    changed["key"] = json!("another-tab");
    assert!(
        post(router.clone(), changed.clone(), true)
            .await
            .status()
            .is_client_error()
    );
    changed["id"] = json!(1);
    assert_eq!(call(&app, changed)["result"]["isError"], true);
    let mut changed = args;
    changed["base_branch"] = json!("other");
    assert_eq!(call(&app, changed)["result"]["isError"], true);
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
