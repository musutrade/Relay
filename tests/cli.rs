use serde_json::Value;
use std::{path::Path, process::Command};

fn command(db: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_relay"))
        .arg(db)
        .args(args)
        .output()
        .unwrap()
}
fn success(db: &Path, args: &[&str]) -> Value {
    let out = command(db, args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}
#[test]
fn separate_processes_complete_vertical_slice_and_preserve_unknown_claim() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("relay.sqlite");
    let input = dir.path().join("task.txt");
    let result = dir.path().join("result.txt");
    std::fs::write(&input, "opaque task").unwrap();
    std::fs::write(&result, "manual executor result").unwrap();
    let submitted = success(&db, &["submit", "cli-key", input.to_str().unwrap()]);
    assert_eq!(submitted["id"], 1);
    let claimed = success(&db, &["claim", "manual"]);
    assert_eq!(claimed["state"], "claimed");
    assert!(success(&db, &["claim", "other"]).is_null());
    assert_eq!(success(&db, &["active"]), claimed);
    let wrong = command(
        &db,
        &["finish", "1", "1", "other", result.to_str().unwrap()],
    );
    assert!(!wrong.status.success());
    assert!(serde_json::from_slice::<Value>(&wrong.stderr).unwrap()["error"].is_string());
    let finished = success(
        &db,
        &["finish", "1", "1", "manual", result.to_str().unwrap()],
    );
    assert_eq!(finished["result"], "manual executor result");
    assert_eq!(success(&db, &["get", "1"]), finished);
    assert!(success(&db, &["active"]).is_null());
}

#[test]
fn malformed_and_oversized_inputs_fail_with_json_errors() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("relay.sqlite");
    let file = dir.path().join("large.txt");
    std::fs::write(&file, vec![b'x'; relay::MAX_PAYLOAD_BYTES + 1]).unwrap();
    for args in [
        vec!["unknown"],
        vec!["submit", "x", file.to_str().unwrap()],
        vec!["get", "not-a-number"],
    ] {
        let out = command(&db, &args);
        assert!(!out.status.success());
        assert!(out.stdout.is_empty());
        assert!(serde_json::from_slice::<Value>(&out.stderr).unwrap()["error"].is_string());
    }
    assert!(success(&db, &["claim", "manual"]).is_null());
}
