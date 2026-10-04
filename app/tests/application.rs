use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use relay_app::{Application, Submission, host::HostConfig, http, mcp};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};
use tempfile::TempDir;
use tower::ServiceExt;

const TOKEN: &str = "test-only-token-00000000000000000000";

#[test]
fn public_workflow_metadata_contains_selectors_without_execution_configuration() {
    let root = TempDir::new().unwrap();
    let mut config = config(root.path());
    config.native_agents.insert(
        "reviewer".into(),
        serde_json::from_value(json!({
            "provider":"claude_cli", "program":"/bin/echo", "env":{"SECRET":"hidden"}
        }))
        .unwrap(),
    );
    config.workflows.insert("checked".into(), serde_json::from_value(json!({
        "repository":"fixture", "developer":"fake", "reviewer":"reviewer", "test":"pass", "max_repairs":1
    })).unwrap());
    let app = Application::open(root.path().join("relay.db"), config).unwrap();
    let public = app.public_config();
    assert_eq!(
        public["workflows"],
        json!([{
            "name":"checked", "repository":"fixture", "developer":"fake", "reviewer":"reviewer", "test":"pass", "max_repairs":1
        }])
    );
    let serialized = public.to_string();
    for forbidden in [
        "SECRET",
        "hidden",
        "/bin/echo",
        "git_program",
        "workspace_root",
    ] {
        assert!(!serialized.contains(forbidden));
    }
    let schema = mcp::handle(&app, json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).unwrap();
    assert_eq!(
        schema["result"]["tools"][0]["inputSchema"]["properties"]["job"]["properties"]["workflow"]
            ["type"],
        json!(["string", "null"])
    );
}

#[test]
fn public_config_lists_native_agents_without_commands_or_secrets() {
    let root = TempDir::new().unwrap();
    let mut config = config(root.path());
    config.native_agents.insert(
        "native".into(),
        serde_json::from_value(json!({
            "provider":"codex_cli", "program":"/bin/echo", "model":"configured-model",
            "env":{"PRIVATE_TOKEN":"secret-do-not-expose"}
        }))
        .unwrap(),
    );
    let app = Application::open(root.path().join("relay.db"), config).unwrap();
    let public = app.public_config();
    assert!(
        public["agents"]
            .as_array()
            .unwrap()
            .contains(&json!("native"))
    );
    assert_eq!(public["native_agents"][0]["provider"], "codex_cli");
    assert_eq!(public["native_agents"][0]["model"], "configured-model");
    assert_eq!(public["native_agents"][0]["authentication"], "unknown");
    let serialized = public.to_string();
    for forbidden in [
        "PRIVATE_TOKEN",
        "secret-do-not-expose",
        "/bin/echo",
        "program",
        "env",
    ] {
        assert!(!serialized.contains(forbidden));
    }
}

fn config(root: &Path) -> HostConfig {
    let repo = root.join("source");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("README.md"), "fixture\n").unwrap();
    serde_json::from_value(json!({
        "workspace_root": root.join("runs"), "repositories":{"fixture":repo},
        "agents":{"fake":{"program":"/bin/sh","args":["-c","printf 'implemented'; printf 'created' > change.txt"]}},
        "tests":{"pass":{"program":"/bin/sh","args":["-c","test -f change.txt && printf 'tests passed'"]}},
        "draft_pr_adapters":{},"timeout_seconds":3,"output_limit_bytes":1024,
        "supervisor_program":env!("CARGO_BIN_EXE_relay-app")
    })).unwrap()
}
fn submission(key: &str) -> Value {
    json!({"key":key,"job":{"repository":"fixture","requirements":"Implement fixture change","agent":"fake","test":"pass","publish":false}})
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
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!({"text":String::from_utf8_lossy(&bytes)})),
    )
}
#[tokio::test]
async fn authenticated_submit_execute_read_and_idempotency() {
    let root = TempDir::new().unwrap();
    let app = Application::open(root.path().join("relay.db"), config(root.path())).unwrap();
    let router = http::router(app.clone(), TOKEN.into()).unwrap();
    for path in ["/api/tasks", "/api/config", "/api/status", "/api/tasks/1"] {
        assert_eq!(
            request(router.clone(), "GET", path, Value::Null, false)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        request(
            router.clone(),
            "POST",
            "/api/tasks",
            submission("one"),
            false
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, task) = request(
        router.clone(),
        "POST",
        "/api/tasks",
        submission("one"),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(task["state"], "queued");
    assert_eq!(
        request(
            router.clone(),
            "POST",
            "/api/tasks",
            submission("one"),
            true
        )
        .await
        .1["id"],
        task["id"]
    );
    let mut conflict = submission("one");
    conflict["job"]["requirements"] = json!("different");
    assert_eq!(
        request(router.clone(), "POST", "/api/tasks", conflict, true)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert!(app.work_once().unwrap());
    let (_, task) = request(router, "GET", "/api/tasks/1", Value::Null, true).await;
    assert_eq!(task["state"], "finished");
    let result: Value = serde_json::from_str(task["result"].as_str().unwrap()).unwrap();
    assert_eq!(result["outcome"], "success", "{result}");
    assert!(!root.path().join("source/change.txt").exists());
}
#[tokio::test]
async fn validation_limits_and_unknown_restart() {
    let root = TempDir::new().unwrap();
    let db = root.path().join("relay.db");
    let app = Application::open(&db, config(root.path())).unwrap();
    let router = http::router(app.clone(), TOKEN.into()).unwrap();
    let mut invalid = submission("bad");
    invalid["job"]["repository"] = json!("../../etc");
    assert_eq!(
        request(router.clone(), "POST", "/api/tasks", invalid, true)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let mut oversized = submission("large");
    oversized["job"]["requirements"] = json!("x".repeat(100_000));
    assert_eq!(
        request(router.clone(), "POST", "/api/tasks", oversized, true)
            .await
            .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    app.submit(serde_json::from_value(submission("unknown")).unwrap())
        .unwrap();
    let task = relay::Store::open(&db)
        .unwrap()
        .claim_next("crashed-host")
        .unwrap()
        .unwrap();
    drop(app);
    let restarted = Application::open(&db, config(root.path())).unwrap();
    assert_eq!(restarted.status().unwrap()["recovery_required"], true);
    assert!(matches!(
        restarted.cancel(task.id),
        Err(relay_app::Error::RecoveryRequired)
    ));
    assert!(!restarted.work_once().unwrap());
    assert_eq!(restarted.get(task.id).unwrap().state, relay::State::Claimed);
}
#[test]
fn queued_cancel_survives_restart_and_mcp_roundtrip() {
    let root = TempDir::new().unwrap();
    let db = root.path().join("relay.db");
    let app = Application::open(&db, config(root.path())).unwrap();
    let submit = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"relay_submit","arguments":submission("mcp")}});
    let response = mcp::handle(&app, submit).unwrap();
    assert_eq!(response["result"]["isError"], false);
    app.cancel(1).unwrap();
    drop(app);
    let app = Application::open(&db, config(root.path())).unwrap();
    assert!(app.work_once().unwrap());
    let task = app.get(1).unwrap();
    let result: Value = serde_json::from_str(task.result.as_ref().unwrap()).unwrap();
    assert_eq!(result["outcome"], "cancelled");
    let mut output = Vec::new();
    mcp::serve(&app,std::io::Cursor::new(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":{\"name\":\"test\",\"version\":\"1\"}}}\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n"),&mut output).unwrap();
    let text = String::from_utf8(output).unwrap();
    assert_eq!(text.lines().count(), 1);
    assert_eq!(
        serde_json::from_str::<Value>(text.trim()).unwrap()["result"]["protocolVersion"],
        "2024-11-05"
    );
    assert!(
        mcp::serve(
            &app,
            std::io::Cursor::new(vec![b'x'; mcp::MAX_MESSAGE_BYTES + 1]),
            Vec::new()
        )
        .is_err()
    );
}
#[test]
fn cancelling_running_execution_is_bounded_and_releases_serial_slot() {
    let root = TempDir::new().unwrap();
    let mut conf = config(root.path());
    conf.agents.get_mut("fake").unwrap().args = vec!["-c".into(), "sleep 20".into()];
    let app = Application::open(root.path().join("relay.db"), conf).unwrap();
    app.submit(serde_json::from_value::<Submission>(submission("long")).unwrap())
        .unwrap();
    let worker = Arc::clone(&app);
    let join = std::thread::spawn(move || worker.work_once());
    for _ in 0..100 {
        if app.get(1).unwrap().state == relay::State::Claimed {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    app.cancel(1).unwrap();
    assert!(join.join().unwrap().unwrap());
    let task = app.get(1).unwrap();
    assert_eq!(task.state, relay::State::Finished);
    assert_eq!(
        serde_json::from_str::<Value>(task.result.as_ref().unwrap()).unwrap()["outcome"],
        "cancelled"
    );
}

#[test]
fn malformed_mcp_ids_never_enqueue_and_notifications_never_reply() {
    let root = TempDir::new().unwrap();
    let app = Application::open(root.path().join("relay.db"), config(root.path())).unwrap();
    for id in [Value::Null, json!(true), json!([]), json!({}), json!(1.5)] {
        let response=mcp::handle(&app,json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"relay_submit","arguments":submission("invalid")}})).unwrap();
        assert_eq!(response["error"]["code"], -32600);
    }
    for method in ["ping", "unknown", "notifications/initialized", "tools/call"] {
        assert!(mcp::handle(&app,json!({"jsonrpc":"2.0","method":method,"params":{"name":"relay_submit","arguments":submission("notification")}})).is_none());
    }
    assert!(app.list(None).unwrap().is_empty());
    assert_eq!(
        mcp::handle(&app, json!({"jsonrpc":"2.0","id":1,"method":"initialize"})).unwrap()["error"]
            ["code"],
        -32602
    );
}

#[test]
fn cancellation_racing_a_different_application_never_false_acknowledges() {
    for iteration in 0..8 {
        let root = TempDir::new().unwrap();
        let conf = config(root.path());
        let db = root.path().join("relay.db");
        let executor = Application::open(&db, conf.clone()).unwrap();
        let caller = Application::open(&db, conf).unwrap();
        executor
            .submit(serde_json::from_value(submission(&format!("race-{iteration}"))).unwrap())
            .unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let b = barrier.clone();
        let runner = executor.clone();
        let thread = std::thread::spawn(move || {
            b.wait();
            runner.work_once()
        });
        barrier.wait();
        let cancellation = caller.cancel(1);
        assert!(thread.join().unwrap().unwrap());
        let task = executor.get(1).unwrap();
        let result: Value = serde_json::from_str(task.result.as_ref().unwrap()).unwrap();
        match cancellation {
            Ok(value) if value["requested"] == true => assert_eq!(result["outcome"], "cancelled"),
            Ok(value) => assert_eq!(value["finished"], true),
            Err(relay_app::Error::RecoveryRequired) => assert_eq!(result["outcome"], "success"),
            Err(error) => panic!("unexpected cancellation error: {error}"),
        }
    }
}

#[test]
fn stale_local_generation_cannot_cancel_new_claim() {
    let root = TempDir::new().unwrap();
    let db = root.path().join("relay.db");
    let mut conf = config(root.path());
    conf.agents.get_mut("fake").unwrap().args = vec!["-c".into(), "sleep 20".into()];
    let app = Application::open(&db, conf).unwrap();
    app.submit(serde_json::from_value(submission("stale")).unwrap())
        .unwrap();
    let runner = app.clone();
    let thread = std::thread::spawn(move || runner.work_once());
    for _ in 0..100 {
        if app.get(1).unwrap().claim().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let old = app.get(1).unwrap().claim().unwrap();
    // Inject stale local bookkeeping. No second external execution is started.
    let mut store = relay::Store::open(&db).unwrap();
    store.confirm_stopped_and_requeue(&old).unwrap();
    let newer = store.claim_next("new-host").unwrap().unwrap();
    assert!(matches!(
        app.cancel(1),
        Err(relay_app::Error::RecoveryRequired)
    ));
    assert_eq!(app.status().unwrap()["recovery_required"], true);
    app.stop();
    assert!(matches!(
        thread.join().unwrap(),
        Err(relay_app::Error::Core(relay::Error::StaleClaim))
    ));
    assert_eq!(store.get(1).unwrap(), newer);
}

#[test]
fn unknown_host_outcome_preserves_claim_and_diagnostic() {
    let root = TempDir::new().unwrap();
    let conf = config(root.path());
    let app = Application::open(root.path().join("relay.db"), conf).unwrap();
    app.submit(serde_json::from_value(submission("unknown-workspace")).unwrap())
        .unwrap();
    app.submit(serde_json::from_value(submission("blocked-next")).unwrap())
        .unwrap();
    std::fs::create_dir(root.path().join("runs/task-1")).unwrap();
    assert!(matches!(
        app.work_once(),
        Err(relay_app::Error::RecoveryRequired)
    ));
    assert_eq!(app.get(1).unwrap().state, relay::State::Claimed);
    let status = app.status().unwrap();
    assert_eq!(status["recovery_required"], true);
    assert_eq!(
        serde_json::from_str::<Value>(status["diagnostic"].as_str().unwrap()).unwrap()["outcome"],
        "unknown"
    );
    assert!(!app.work_once().unwrap());
    assert_eq!(app.get(2).unwrap().state, relay::State::Queued);
}

#[tokio::test]
async fn browser_session_http_boundary_and_secret_redaction() {
    use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
    use rand_core::OsRng;
    use std::os::unix::fs::PermissionsExt;
    let root = TempDir::new().unwrap();
    let path = root.path().join("credentials.json");
    let hash = Argon2::default()
        .hash_password(b"fixture-password-only", &SaltString::generate(&mut OsRng))
        .unwrap()
        .to_string();
    std::fs::write(
        &path,
        json!({"username":"operator", "password_hash":hash}).to_string(),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let auth = relay_app::auth::Auth::new(
        Some(relay_app::auth::Config {
            mode: relay_app::auth::Mode::Session,
            credentials_file: path,
            public_origin: "https://relay.example".into(),
            allow_insecure_loopback: false,
        }),
        None,
    )
    .unwrap();
    let app = Application::open(root.path().join("relay.db"), config(root.path())).unwrap();
    let router = http::router_with_auth(app, auth);
    let request = |method: &str, uri: &str, cookie: &str, origin: &str, body: &str| {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", cookie)
            .header("origin", origin)
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap()
    };
    assert_eq!(
        router
            .clone()
            .oneshot(request("GET", "/api/config", "", "", ""))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let response = router
        .clone()
        .oneshot(request(
            "POST",
            "/auth/login",
            "",
            "https://evil.example",
            r#"{"username":"operator","password":"fixture-password-only"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = router
        .clone()
        .oneshot(request(
            "POST",
            "/auth/login",
            "",
            "https://relay.example",
            r#"{"username":"operator","password":"wrong"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = router
        .clone()
        .oneshot(request(
            "POST",
            "/auth/login",
            "",
            "https://relay.example",
            r#"{"username":"operator","password":"fixture-password-only"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    for uri in ["/auth/status", "/api/config", "/api/status", "/api/tasks"] {
        let response = router
            .clone()
            .oneshot(request("GET", uri, &cookie, "", ""))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&bytes);
        for secret in [&hash, "fixture-password-only", &cookie] {
            assert!(!text.contains(secret));
        }
    }
    assert_eq!(
        router
            .clone()
            .oneshot(request(
                "POST",
                "/api/tasks/1/cancel",
                &cookie,
                "https://evil.example",
                ""
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        router
            .clone()
            .oneshot(request(
                "POST",
                "/auth/logout",
                &cookie,
                "https://relay.example",
                ""
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        router
            .oneshot(request("GET", "/api/config", &cookie, "", ""))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
}
