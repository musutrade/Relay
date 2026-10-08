#![cfg(target_os = "linux")]
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use relay_app::{Application, ci_tracking::CiStartRequest, host::HostConfig, http, mcp};
use serde_json::{Value, json};
use std::{fs, sync::Arc};
use tempfile::TempDir;
use tower::ServiceExt;

const TOKEN: &str = "test-only-token-00000000000000000000";
const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const BASE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
fn fixture() -> (TempDir, Arc<Application>) {
    let root = TempDir::new().unwrap();
    fs::create_dir(root.path().join("source")).unwrap();
    fs::write(root.path().join("source/file"), "fixture").unwrap();
    let observation = json!({"version":1,"observation":"ok","complete":true,"repository":{"id":100,"full_name":"example/project"},"pull_request":{"id":200,"number":7,"url":"https://github.com/example/project/pull/7","state":"open","merged":false,"draft":true,"head_sha":SHA,"head_ref":"relay/task-1-g1","head_repository_id":100,"base_ref":"main","base_sha":BASE,"base_repository_id":100,"mergeable":null},"run":null,"error_code":null,"detail":null,"remote_merge_eligibility":"not_established"});
    fs::write(root.path().join("response.json"), observation.to_string()).unwrap();
    fs::write(root.path().join("observer.py"), "import json,os,pathlib,sys\njson.load(sys.stdin)\nroot=pathlib.Path(os.environ['FIXTURE'])\n(root/'observed').write_text('once')\nprint((root/'response.json').read_text())\n").unwrap();
    let config: HostConfig = serde_json::from_value(json!({
        "workspace_root":root.path().join("runs"),"repositories":{"repo":root.path().join("source")},
        "agents":{"fake":{"program":"/bin/echo"}},"timeout_seconds":10,
        "supervisor_program":env!("CARGO_BIN_EXE_relay-app"),
        "ci_policies":{"checks":{"github_repository":"example/project","base_branch":"main","workflow_id":41,"app_id":15368,"required_jobs":["test"],"observer":{"program":"/usr/bin/python3","args":[root.path().join("observer.py")],"env":{"FIXTURE":root.path()}}}},
        "merge_policies":{"exact-head":{"ci_policy":"checks","merge_method":"squash","allow_ready":true,"adapter":{"program":"/bin/sh","args":["-c",format!("touch {}",root.path().join("merge-called").display())],"env":{"PRIVATE_MERGE_FIXTURE":"not-public"}}}}
    })).unwrap();
    let mut core = relay::Store::open(root.path().join("relay.db")).unwrap();
    core.submit("published", &json!({"repository":"repo","requirements":"fixture","agent":"fake","test":null,"publish":true}).to_string()).unwrap();
    let task = core.claim_next("fixture-host").unwrap().unwrap();
    core.finish(&task.claim().unwrap(), &json!({"outcome":"success","workspace":null,"agent":null,"tests":null,"draft_pr":null,"error":null,"workflow":{"name":"checked","base_sha":BASE,"candidate_sha":SHA,"reviewed_sha":SHA,"rounds":[],"reconciliation_required":false,"publication":{"base_branch":"main","dry_run":false,"draft":true,"repository":"example/project","branch":"relay/task-1-g1","candidate_sha":SHA,"url":"https://github.com/example/project/pull/7"}}}).to_string()).unwrap();
    let app = Application::open(root.path().join("relay.db"), config).unwrap();
    assert_eq!(app.merge_preview(1).unwrap()["eligible"], false);
    let ci = app.ci_preview(1).unwrap();
    app.ci_start(1, serde_json::from_value::<CiStartRequest>(json!({"key":"ci-once","policy":"checks","policy_digest":ci["policies"][0]["policy_digest"]})).unwrap()).unwrap();
    assert_eq!(
        app.merge_preview(1).unwrap()["eligible"],
        false,
        "CI must first establish numeric identity"
    );
    assert!(app.ci_work_once().unwrap());
    assert_eq!(
        app.ci_get(1).unwrap().status,
        "watching",
        "consent can be saved while CI is pending"
    );
    (root, app)
}
fn request(app: &Application) -> Value {
    let preview = app.merge_preview(1).unwrap();
    assert_eq!(preview["eligible"], true, "{preview}");
    let p = &preview["policies"][0];
    json!({"key":"merge-once","confirm_merge":true,"policy":p["name"],"policy_digest":p["policy_digest"],"ci_track_id":p["ci_track_id"],"scope_digest":p["scope_digest"],"deadline":p["deadline"],"allow_ready":false,"accept_non_atomic_target_guard":true,"deadline_semantics":"last_dispatch","accept_existing_automation":true,"risk_disclosure_version":preview["risk_disclosure"]["version"],"risk_disclosure_sha256":preview["risk_disclosure"]["sha256"]})
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
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
fn call(app: &Application, name: &str, arguments: Value) -> Value {
    mcp::handle(app, json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}})).unwrap()
}
fn content(value: &Value) -> Value {
    serde_json::from_str(value["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn merge_transports_require_explicit_exact_consent_and_keep_reads_local() {
    let (root, app) = fixture();
    let router = http::router(app.clone(), TOKEN.into()).unwrap();
    let body = request(&app);
    let original = app.get(1).unwrap();
    for (path, method, input) in [
        ("/api/tasks/1/merge-preview", "GET", None),
        ("/api/tasks/1/merge-authorizations", "GET", None),
        ("/api/tasks/1/authorize-merge", "POST", Some(body.clone())),
        ("/api/merge-authorizations/1", "GET", None),
        (
            "/api/merge-authorizations/1/revoke",
            "POST",
            Some(json!({"expected_revision":1})),
        ),
        (
            "/api/merge-authorizations/1/reconcile",
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
        "relay_merge_preview",
        "relay_merge_list",
        "relay_merge_get",
        "relay_authorize_merge",
        "relay_merge_revoke",
        "relay_merge_reconcile",
    ] {
        let tool = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap();
        assert_eq!(tool["inputSchema"]["additionalProperties"], false, "{name}");
        for id in [json!(0), json!(-1), json!("1")] {
            assert_eq!(
                call(&app, name, json!({"id":id}))["result"]["isError"],
                true,
                "{name}"
            );
        }
        assert_eq!(
            call(&app, name, json!({"id":1,"unexpected":true}))["result"]["isError"],
            true
        );
    }
    for field in body.as_object().unwrap().keys() {
        let mut invalid = body.clone();
        invalid.as_object_mut().unwrap().remove(field);
        assert!(
            http_call(
                router.clone(),
                "/api/tasks/1/authorize-merge",
                "POST",
                Some(invalid.clone()),
                true
            )
            .await
            .0
            .is_client_error(),
            "{field}"
        );
        invalid["id"] = json!(1);
        assert_eq!(
            call(&app, "relay_authorize_merge", invalid)["result"]["isError"],
            true,
            "{field}"
        );
    }
    for (field, value) in [
        ("confirm_merge", json!(false)),
        ("accept_non_atomic_target_guard", json!(false)),
        ("accept_existing_automation", json!(false)),
        ("risk_disclosure_version", json!(2)),
        ("risk_disclosure_sha256", json!("0".repeat(64))),
        ("deadline_semantics", json!("completion")),
        ("ci_track_id", json!("1")),
        ("scope_digest", json!("f".repeat(64))),
        ("policy_digest", json!("f".repeat(64))),
        ("deadline", json!(1)),
        ("allow_ready", json!("false")),
        ("head_sha", json!(SHA)),
        ("bypass_rules", json!(true)),
    ] {
        let mut invalid = body.clone();
        invalid[field] = value;
        assert!(
            http_call(
                router.clone(),
                "/api/tasks/1/authorize-merge",
                "POST",
                Some(invalid.clone()),
                true
            )
            .await
            .0
            .is_client_error(),
            "{field}"
        );
        invalid["id"] = json!(1);
        assert_eq!(
            call(&app, "relay_authorize_merge", invalid)["result"]["isError"],
            true,
            "{field}"
        );
    }
    for path in [
        "/api/tasks/0/merge-preview",
        "/api/tasks/-1/merge-authorizations",
        "/api/merge-authorizations/0",
    ] {
        assert_eq!(
            http_call(router.clone(), path, "GET", None, true).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    let preview = http_call(
        router.clone(),
        "/api/tasks/1/merge-preview",
        "GET",
        None,
        true,
    )
    .await
    .1;
    assert_eq!(preview["policies"][0]["target"]["repository_id"], 100);
    assert_eq!(preview["policies"][0]["target"]["pr_id"], 200);
    for private in [
        "not-public",
        "merge-called",
        "PRIVATE_MERGE_FIXTURE",
        "observer.py",
    ] {
        assert!(!preview.to_string().contains(private), "{private}");
    }
    let (status, first) = http_call(
        router.clone(),
        "/api/tasks/1/authorize-merge",
        "POST",
        Some(body.clone()),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    assert_eq!(first["status"], "watching");
    assert_eq!(first["attempt"], 0);
    assert!(first["async_request"].is_null());
    let mut args = body.clone();
    args["id"] = json!(1);
    assert_eq!(content(&call(&app, "relay_authorize_merge", args)), first);
    assert_eq!(
        http_call(
            router.clone(),
            "/api/tasks/1/authorize-merge",
            "POST",
            Some(body.clone()),
            true
        )
        .await
        .1,
        first
    );
    let mut changed = body.clone();
    changed["allow_ready"] = json!(true);
    assert_eq!(
        http_call(
            router.clone(),
            "/api/tasks/1/authorize-merge",
            "POST",
            Some(changed),
            true
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let mut changed = body;
    changed["key"] = json!("second-key");
    assert_eq!(
        http_call(
            router.clone(),
            "/api/tasks/1/authorize-merge",
            "POST",
            Some(changed),
            true
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        content(&call(&app, "relay_merge_get", json!({"id":1}))),
        first
    );
    assert_eq!(
        content(&call(&app, "relay_merge_list", json!({"id":1}))),
        json!([first])
    );
    assert_eq!(
        http_call(
            router.clone(),
            "/api/tasks/1/merge-authorizations",
            "GET",
            None,
            true
        )
        .await
        .1,
        json!([first])
    );
    assert_eq!(
        http_call(
            router.clone(),
            "/api/merge-authorizations/1",
            "GET",
            None,
            true
        )
        .await
        .1,
        first
    );
    for suffix in ["revoke", "reconcile"] {
        for invalid in [
            json!({}),
            json!({"expected_revision":1,"confirm":true}),
            json!({"expected_revision":-1}),
            json!({"expected_revision":"1"}),
        ] {
            assert!(
                http_call(
                    router.clone(),
                    &format!("/api/merge-authorizations/1/{suffix}"),
                    "POST",
                    Some(invalid.clone()),
                    true
                )
                .await
                .0
                .is_client_error()
            );
            let mut args = invalid;
            args["id"] = json!(1);
            assert_eq!(
                call(&app, &format!("relay_merge_{suffix}"), args)["result"]["isError"],
                true
            );
        }
    }
    let revoked = http_call(
        router.clone(),
        "/api/merge-authorizations/1/revoke",
        "POST",
        Some(json!({"expected_revision":1})),
        true,
    )
    .await
    .1;
    assert_eq!(revoked["revoked"], true);
    assert_eq!(revoked["status"], "revoked");
    assert_eq!(
        http_call(
            router.clone(),
            "/api/merge-authorizations/1/reconcile",
            "POST",
            Some(json!({"expected_revision":1})),
            true
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let reconciled = content(&call(
        &app,
        "relay_merge_reconcile",
        json!({"id":1,"expected_revision":revoked["revision"]}),
    ));
    assert_eq!(reconciled["revoked"], true);
    assert_eq!(reconciled["merge_dispatched"], false);
    assert_eq!(
        content(&call(
            &app,
            "relay_merge_reconcile",
            json!({"id":1,"expected_revision":revoked["revision"]})
        )),
        reconciled
    );
    assert!(
        !root.path().join("merge-called").exists(),
        "GET/authorize/revoke/reconcile only change local records, never invoke adapter inline"
    );
    assert_eq!(app.get(1).unwrap(), original);
    assert_eq!(app.status().unwrap()["active"], Value::Null);
    assert_eq!(app.list_views(None).unwrap().len(), 1);
}

#[tokio::test]
async fn merge_http_rejects_non_json_malformed_and_oversized_mutations() {
    let (_root, app) = fixture();
    let router = http::router(app, TOKEN.into()).unwrap();
    for (content_type, body) in [
        ("text/plain", "{}".to_owned()),
        ("application/json", "{".to_owned()),
        ("application/json", " ".repeat(96 * 1024 + 1)),
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/tasks/1/authorize-merge")
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .header("content-type", content_type)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.status().is_client_error());
    }
}

#[tokio::test]
async fn merge_session_posts_require_same_origin_and_logout_revokes_access() {
    use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
    use rand_core::OsRng;
    use std::os::unix::fs::PermissionsExt;
    let (root, app) = fixture();
    let body = request(&app);
    let path = root.path().join("credentials.json");
    let hash = Argon2::default()
        .hash_password(b"fixture-password", &SaltString::generate(&mut OsRng))
        .unwrap()
        .to_string();
    fs::write(
        &path,
        json!({"username":"operator","password_hash":hash}).to_string(),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let auth = relay_app::auth::Auth::new(
        Some(relay_app::auth::Config {
            mode: relay_app::auth::Mode::Session,
            credentials_file: path,
            public_origin: "http://127.0.0.1:8787".into(),
            allow_insecure_loopback: true,
        }),
        None,
    )
    .unwrap();
    let router = http::router_with_auth(app, auth);
    let login = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("origin", "http://127.0.0.1:8787")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"username":"operator","password":"fixture-password"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    let cookie = login.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    for origin in [None, Some("https://attacker.invalid")] {
        for path in [
            "/api/tasks/1/authorize-merge",
            "/api/merge-authorizations/1/revoke",
            "/api/merge-authorizations/1/reconcile",
        ] {
            let mut request = Request::builder()
                .method("POST")
                .uri(path)
                .header("cookie", &cookie)
                .header("content-type", "application/json");
            if let Some(origin) = origin {
                request = request.header("origin", origin);
            }
            let response = router
                .clone()
                .oneshot(request.body(Body::from(body.to_string())).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        }
    }
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/tasks/1/authorize-merge")
                .header("cookie", &cookie)
                .header("origin", "http://127.0.0.1:8787")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/merge-authorizations/1")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/logout")
                .header("cookie", &cookie)
                .header("origin", "http://127.0.0.1:8787")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = router
        .oneshot(
            Request::builder()
                .uri("/api/merge-authorizations/1")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
