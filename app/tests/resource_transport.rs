//! Read-only operator transport and backward-compatible quota admission.
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use relay_app::{Application, host::HostConfig, http};
use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

const TOKEN: &str = "resource-transport-fixture-token-00000";
const CAP: u64 = 128 * 1024;
fn fixture() -> (TempDir, Arc<Application>, axum::Router) {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("README.md"), "fixture\n").unwrap();
    let config: HostConfig = serde_json::from_value(json!({
        "workspace_root":root.path().join("runs"),
        "repositories":{"fixture":source},
        "agents":{"fake":{"program":"/bin/sh","args":["-c","printf invoked >> \"$CALL_LOG\""],
            "env":{"CALL_LOG":root.path().join("agent-calls"),"PRIVATE_SECRET":"never-expose-resource-secret"}}},
        "max_snapshot_bytes":65536,"max_workspace_bytes":CAP,
        "supervisor_program":env!("CARGO_BIN_EXE_relay-app")
    })).unwrap();
    let app = Application::open(root.path().join("queue.db"), config).unwrap();
    let router = http::router(app.clone(), TOKEN.into()).unwrap();
    (root, app, router)
}
async fn request(
    router: &axum::Router,
    method: &str,
    path: &str,
    body: Value,
    authenticated: bool,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if authenticated {
        request = request.header("authorization", format!("Bearer {TOKEN}"));
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!({"text":String::from_utf8_lossy(&bytes)}));
    (status, value)
}
fn submission(key: &str) -> Value {
    json!({"key":key,"job":{"repository":"fixture","requirements":"Implement fixture", "agent":"fake","test":null,"publish":false}})
}

#[tokio::test]
async fn resource_reads_are_authenticated_allowlisted_and_never_invoke_agents() {
    let (root, app, router) = fixture();
    assert_eq!(
        request(
            &router,
            "GET",
            "/api/resources?repository=fixture",
            Value::Null,
            false
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, estimate) = request(
        &router,
        "GET",
        "/api/resources?repository=fixture",
        Value::Null,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{estimate}");
    assert_eq!(estimate["host_policy_cap_bytes"], CAP);
    assert_eq!(estimate["default_quota_bytes"], CAP);
    assert_eq!(estimate["initial_estimate"]["source"], "host_inventory");
    assert_eq!(estimate["build_growth"], "unknown");
    assert_eq!(estimate["enforcement"], "logical_bytes_best_effort");
    assert_eq!(estimate["os_hard_quota"], false);
    assert_eq!(estimate["disk_reserved"], false);
    assert!(
        !estimate
            .to_string()
            .contains("never-expose-resource-secret")
    );
    for query in [
        "repository=missing",
        "repository=%2Fetc%2Fpasswd",
        "repository=fixture&workflow=unknown",
        "repository=fixture&path=%2Fetc",
    ] {
        let (status, _) = request(
            &router,
            "GET",
            &format!("/api/resources?{query}"),
            Value::Null,
            true,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "query {query}");
    }
    assert!(app.list(None).unwrap().is_empty());
    assert!(!root.path().join("agent-calls").exists());
}

#[tokio::test]
async fn absent_and_null_quotas_preserve_original_idempotency_and_queued_operator_is_read_only() {
    let (root, app, router) = fixture();
    let original = submission("legacy");
    let (status, first) = request(&router, "POST", "/api/tasks", original.clone(), true).await;
    assert_eq!(status, StatusCode::CREATED);
    let mut null_quota = original;
    null_quota["job"]["workspace_quota_bytes"] = Value::Null;
    let (status, repeated) = request(&router, "POST", "/api/tasks", null_quota, true).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(first["id"], repeated["id"]);
    assert!(
        !first["payload"]
            .as_str()
            .unwrap()
            .contains("workspace_quota_bytes")
    );
    let path = format!("/api/tasks/{}/operator", first["id"].as_i64().unwrap());
    assert_eq!(
        request(&router, "GET", &path, Value::Null, false).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, operator) = request(&router, "GET", &path, Value::Null, true).await;
    assert_eq!(status, StatusCode::OK, "{operator}");
    assert_eq!(operator["task_id"], first["id"]);
    assert_eq!(operator["resources"]["quota_bytes"], CAP);
    assert_eq!(operator["resources"]["host_policy_cap_bytes"], CAP);
    assert!(
        operator["recovery"]["actions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(operator["workspace_retained"], false);
    assert_eq!(operator["retained_result"]["available"], false);
    assert_eq!(
        app.get(first["id"].as_i64().unwrap()).unwrap().state,
        relay::State::Queued
    );
    assert!(!root.path().join("agent-calls").exists());
}

#[tokio::test]
async fn invalid_quotas_and_client_supplied_lineage_never_enter_queue() {
    let (root, app, router) = fixture();
    for quota in [
        json!(0),
        json!(CAP + 1),
        json!(-1),
        json!(1.5),
        json!("1024"),
    ] {
        let mut input = submission("invalid");
        input["job"]["workspace_quota_bytes"] = quota;
        let (status, _) = request(&router, "POST", "/api/tasks", input, true).await;
        assert!(status.is_client_error());
    }
    let mut input = submission("forged");
    input["job"]["workspace_quota_bytes"] = json!(2);
    input["job"]["continuation"] = json!({"workspace_task_id":1,"predecessor_task_id":1,"predecessor_generation":1,
        "quota_increase":{"previous_bytes":1,"new_bytes":2}});
    assert_eq!(
        request(&router, "POST", "/api/tasks", input, true).await.0,
        StatusCode::BAD_REQUEST
    );
    assert!(app.list(None).unwrap().is_empty());
    assert!(!root.path().join("agent-calls").exists());
}
