#![cfg(target_os = "linux")]
use relay::{MAX_RESULT_BYTES, State, Task};
use relay_app::host::{Host, HostConfig, Job, Outcome, RunResult};
use serde_json::json;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Fixture {
    temp: TempDir,
    config: HostConfig,
}
impl Fixture {
    fn new(script: &str) -> Self {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("original.txt"), "original\n").unwrap();
        fs::create_dir(source.join(".git")).unwrap();
        fs::write(source.join(".git/config"), "source git configuration").unwrap();
        let config = serde_json::from_value(json!({
            "workspace_root":temp.path().join("runs"),
            "repositories":{"fixture":source},
            "agents":{"fake":{"program":"/bin/sh","args":["-c",script,"relay-fake","{requirements}"]}},
            "tests":{"check":{"program":"/bin/sh","args":["-c","test -f changed.txt && printf 'tests passed'"]}},
            "draft_pr_adapters":{"fake-pr":{"program":"/bin/sh","args":["-c","test \"$RELAY_DRAFT_PR\" = true && printf 'https://github.example.invalid/fixture/pull/1 (draft)' && touch published.txt"]}},
            "timeout_seconds":3,"output_limit_bytes":1024,
            "supervisor_program":env!("CARGO_BIN_EXE_relay-app")
        })).unwrap();
        Self { temp, config }
    }
    fn task(&self, id: i64) -> Task {
        Task {
            id,
            key: format!("fixture-{id}"),
            payload: serde_json::to_string(&self.job()).unwrap(),
            state: State::Claimed,
            generation: 1,
            owner: Some("fixture-host".into()),
            result: None,
        }
    }
    fn job(&self) -> Job {
        Job {
            repository: "fixture".into(),
            requirements: "Implement the fixture change".into(),
            agent: "fake".into(),
            test: None,
            publish: false,
            draft_pr_adapter: None,
        }
    }
    fn workspace(&self, id: i64) -> PathBuf {
        self.config
            .workspace_root
            .join(format!("task-{id}-generation-1/repository"))
    }
    fn run(&self, task: &Task) -> RunResult {
        Host::new(self.config.clone())
            .unwrap()
            .execute(task, Arc::new(AtomicBool::new(false)))
    }
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.is_file() {
        assert!(
            Instant::now() < deadline,
            "file did not appear: {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}
fn assert_pid_reaped(path: &Path) {
    let pid: u32 = fs::read_to_string(path).unwrap().trim().parse().unwrap();
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "process {pid} remains after command cleanup"
    );
}

#[test]
fn fake_agent_tests_and_draft_adapter_use_private_snapshot_and_literal_input() {
    let fixture = Fixture::new(
        "cat > stdin.txt; printf '%s' \"$1\" > argument.txt; printf changed > changed.txt; printf 'agent done'",
    );
    let injection_target = fixture.temp.path().join("must-not-exist");
    let mut task = fixture.task(1);
    let mut job = fixture.job();
    job.requirements = format!(
        "Implement text; $(touch {}) \" ' 🚀",
        injection_target.display()
    );
    job.test = Some("check".into());
    job.publish = true;
    job.draft_pr_adapter = Some("fake-pr".into());
    task.payload = serde_json::to_string(&job).unwrap();
    let result = fixture.run(&task);
    assert_eq!(result.outcome, Outcome::Success, "{}", result.to_json());
    assert_eq!(result.agent.as_ref().unwrap().stdout, "agent done");
    assert_eq!(result.tests.as_ref().unwrap().stdout, "tests passed");
    assert!(result.draft_pr.as_ref().unwrap().stdout.contains("(draft)"));
    let workspace = fixture.workspace(1);
    assert_eq!(
        fs::read_to_string(workspace.join("stdin.txt")).unwrap(),
        job.requirements
    );
    assert_eq!(
        fs::read_to_string(workspace.join("argument.txt")).unwrap(),
        job.requirements
    );
    assert_eq!(
        fs::read_to_string(workspace.join("original.txt")).unwrap(),
        "original\n"
    );
    assert!(!workspace.join(".git").exists());
    assert!(workspace.join("published.txt").exists());
    assert!(!fixture.temp.path().join("source/changed.txt").exists());
    assert!(!injection_target.exists());
    assert_eq!(
        fs::metadata(workspace.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

#[test]
fn failure_stops_later_stages() {
    let mut fixture = Fixture::new("printf failed >&2; exit 7");
    let mut job = fixture.job();
    job.test = Some("check".into());
    job.publish = true;
    job.draft_pr_adapter = Some("fake-pr".into());
    let mut task = fixture.task(1);
    task.payload = serde_json::to_string(&job).unwrap();
    let result = fixture.run(&task);
    assert_eq!(result.outcome, Outcome::Failure);
    assert_eq!(result.agent.unwrap().exit_code, Some(7));
    assert!(result.tests.is_none());
    assert!(result.draft_pr.is_none());
    fixture.config.agents.get_mut("fake").unwrap().args[1] = "printf done".into();
    task.id = 2;
    let result = fixture.run(&task);
    assert_eq!(result.outcome, Outcome::Failure);
    assert_eq!(result.agent.unwrap().outcome, Outcome::Success);
    assert_eq!(result.tests.unwrap().outcome, Outcome::Failure);
    assert!(result.draft_pr.is_none());
}

#[test]
fn command_fields_and_unknown_profiles_are_rejected() {
    let fixture = Fixture::new("exit 0");
    let mut job = fixture.job();
    job.repository = "../../etc".into();
    assert!(job.validate(&fixture.config).is_err());
    job = fixture.job();
    job.agent = "/bin/sh".into();
    assert!(job.validate(&fixture.config).is_err());
    job = fixture.job();
    job.test = Some("arbitrary".into());
    assert!(job.validate(&fixture.config).is_err());
    job = fixture.job();
    job.publish = true;
    assert!(job.validate(&fixture.config).is_err());
    job = fixture.job();
    job.draft_pr_adapter = Some("fake-pr".into());
    assert!(job.validate(&fixture.config).is_err());
    job = fixture.job();
    job.requirements = " ".into();
    assert!(job.validate(&fixture.config).is_err());
    job.requirements = "x".repeat(32769);
    assert!(job.validate(&fixture.config).is_err());
    let mut value = serde_json::to_value(fixture.job()).unwrap();
    value["program"] = json!("/bin/sh");
    assert!(Job::from_payload(&value.to_string(), &fixture.config).is_err());
    let mut task = fixture.task(1);
    task.state = State::Queued;
    assert_eq!(fixture.run(&task).outcome, Outcome::Failure);
}

#[test]
fn symlink_escape_is_rejected_before_running_agent() {
    let fixture = Fixture::new("printf changed > escape");
    let outside = fixture.temp.path().join("outside.txt");
    fs::write(&outside, "safe").unwrap();
    symlink(&outside, fixture.temp.path().join("source/escape")).unwrap();
    let result = fixture.run(&fixture.task(1));
    assert_eq!(result.outcome, Outcome::Failure);
    assert!(result.error.unwrap().contains("symlink"));
    assert!(result.agent.is_none());
    assert_eq!(fs::read_to_string(outside).unwrap(), "safe");
}

#[test]
fn snapshot_limits_and_workspace_overlap_are_rejected() {
    let mut fixture = Fixture::new("exit 0");
    fixture.config.max_snapshot_bytes = 4;
    let result = fixture.run(&fixture.task(1));
    assert_eq!(result.outcome, Outcome::Failure);
    assert!(result.error.unwrap().contains("byte limit"));
    fixture.config.max_snapshot_bytes = 1024;
    fixture.config.max_snapshot_entries = 1;
    fs::write(fixture.temp.path().join("source/second.txt"), "second").unwrap();
    let result = fixture.run(&fixture.task(2));
    assert_eq!(result.outcome, Outcome::Failure);
    assert!(result.error.unwrap().contains("entry limit"));
    fixture.config.workspace_root = fixture.temp.path().join("source/runs");
    assert!(Host::new(fixture.config).is_err());
    assert!(!fixture.temp.path().join("source/runs").exists());
}

#[test]
fn same_generation_is_never_executed_twice() {
    let fixture = Fixture::new("printf 'once' >> executions.txt");
    let task = fixture.task(1);
    assert_eq!(fixture.run(&task).outcome, Outcome::Success);
    let result = fixture.run(&task);
    assert_eq!(result.outcome, Outcome::Unknown);
    assert_eq!(
        fs::read_to_string(fixture.workspace(1).join("executions.txt")).unwrap(),
        "once"
    );
}

const TREE_SCRIPT: &str = "echo $$ > leader.pid; sleep 60 & echo $! > child.pid; /bin/sh -c 'sleep 60 & echo $! > grandchild.pid; wait' & echo $! > branch.pid; wait";

#[test]
fn timeout_stops_and_reaps_group_and_grandchildren() {
    let mut fixture = Fixture::new(TREE_SCRIPT);
    fixture.config.timeout_seconds = 1;
    let started = Instant::now();
    let result = fixture.run(&fixture.task(1));
    assert_eq!(result.outcome, Outcome::TimedOut, "{}", result.to_json());
    assert!(started.elapsed() < Duration::from_secs(5));
    for file in ["leader.pid", "child.pid", "branch.pid", "grandchild.pid"] {
        assert_pid_reaped(&fixture.workspace(1).join(file));
    }
}

#[test]
fn cancellation_stops_and_reaps_process_tree_before_return() {
    let fixture = Fixture::new(TREE_SCRIPT);
    let host = Host::new(fixture.config.clone()).unwrap();
    let task = fixture.task(1);
    let cancellation = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancellation);
    let worker = thread::spawn(move || host.execute(&task, flag));
    wait_for_file(&fixture.workspace(1).join("grandchild.pid"));
    cancellation.store(true, Ordering::Release);
    let result = worker.join().unwrap();
    assert_eq!(result.outcome, Outcome::Cancelled, "{}", result.to_json());
    for file in ["leader.pid", "child.pid", "branch.pid", "grandchild.pid"] {
        assert_pid_reaped(&fixture.workspace(1).join(file));
    }
}

#[test]
fn successful_leader_does_not_leave_background_child() {
    let fixture = Fixture::new("sleep 60 & echo $! > child.pid; exit 0");
    let result = fixture.run(&fixture.task(1));
    assert_eq!(result.outcome, Outcome::Success, "{}", result.to_json());
    assert_pid_reaped(&fixture.workspace(1).join("child.pid"));
}

#[test]
fn adopted_session_descendant_is_stopped_and_reaped() {
    let mut fixture = Fixture::new(
        "/usr/bin/setsid /bin/sh -c 'echo $$ > escaped.pid; sleep 60 & echo $! > escaped-child.pid; wait' & wait",
    );
    fixture.config.timeout_seconds = 1;
    let result = fixture.run(&fixture.task(1));
    assert_eq!(result.outcome, Outcome::TimedOut, "{}", result.to_json());
    assert_pid_reaped(&fixture.workspace(1).join("escaped.pid"));
    assert_pid_reaped(&fixture.workspace(1).join("escaped-child.pid"));
}

#[test]
fn noisy_output_is_drained_and_json_result_is_bounded() {
    let mut fixture = Fixture::new(
        "head -c 200000 /dev/zero; head -c 200000 /dev/zero >&2; printf changed > changed.txt",
    );
    fixture.config.output_limit_bytes = 8192;
    let mut job = fixture.job();
    job.test = Some("check".into());
    let mut task = fixture.task(1);
    task.payload = serde_json::to_string(&job).unwrap();
    let result = fixture.run(&task);
    assert_eq!(result.outcome, Outcome::Success, "{}", result.to_json());
    assert!(result.agent.as_ref().unwrap().stdout_truncated);
    assert!(result.agent.as_ref().unwrap().stderr_truncated);
    assert!(result.to_json().len() <= MAX_RESULT_BYTES);
    let bounded: RunResult = serde_json::from_str(&result.to_json()).unwrap();
    assert_eq!(bounded.outcome, Outcome::Success);
    assert!(bounded.agent.unwrap().stdout.len() < 8192);
}

#[test]
fn one_wall_clock_budget_covers_all_phases() {
    let mut fixture = Fixture::new("sleep 0.6; touch changed.txt");
    fixture.config.timeout_seconds = 1;
    fixture.config.tests.get_mut("check").unwrap().args[1] = "sleep 0.6".into();
    let mut job = fixture.job();
    job.test = Some("check".into());
    let mut task = fixture.task(1);
    task.payload = serde_json::to_string(&job).unwrap();
    let result = fixture.run(&task);
    assert_eq!(result.outcome, Outcome::TimedOut, "{}", result.to_json());
    assert_eq!(result.agent.unwrap().outcome, Outcome::Success);
    assert_eq!(result.tests.unwrap().outcome, Outcome::TimedOut);
}

#[test]
fn pre_cancelled_job_creates_no_workspace() {
    let fixture = Fixture::new("exit 0");
    let result = Host::new(fixture.config.clone())
        .unwrap()
        .execute(&fixture.task(1), Arc::new(AtomicBool::new(true)));
    assert_eq!(result.outcome, Outcome::Cancelled);
    assert!(result.workspace.is_none());
    assert!(!fixture.workspace(1).exists());
}

#[test]
fn workspace_growth_and_retention_limits_fail_closed() {
    let mut fixture = Fixture::new("head -c 65536 /dev/zero > large.bin; sleep 60");
    fixture.config.max_snapshot_bytes = 1024;
    let result = fixture.run(&fixture.task(1));
    assert_eq!(result.outcome, Outcome::Failure, "{}", result.to_json());
    assert!(
        result
            .agent
            .unwrap()
            .error
            .unwrap()
            .contains("workspace byte limit")
    );
    fixture.config.max_retained_workspaces = 1;
    let result = fixture.run(&fixture.task(2));
    assert_eq!(result.outcome, Outcome::Failure);
    assert!(result.error.unwrap().contains("retention limit"));
}

#[test]
fn failed_supervisor_is_unknown_not_an_execution_success() {
    let mut fixture = Fixture::new("touch should-not-run");
    fixture.config.supervisor_program = Some(PathBuf::from("/bin/false"));
    let result = fixture.run(&fixture.task(1));
    assert_eq!(result.outcome, Outcome::Unknown);
    assert!(!fixture.workspace(1).join("should-not-run").exists());
}

#[test]
fn independent_groups_do_not_cancel_each_other() {
    let mut fixture = Fixture::new("echo $$ > leader.pid; sleep 60");
    let first = Host::new(fixture.config.clone()).unwrap();
    fixture.config.agents.get_mut("fake").unwrap().args[1] = "sleep 0.4; printf completed".into();
    let second = Host::new(fixture.config.clone()).unwrap();
    let flag = Arc::new(AtomicBool::new(false));
    let first_flag = Arc::clone(&flag);
    let task1 = fixture.task(1);
    let task2 = fixture.task(2);
    let run1 = thread::spawn(move || first.execute(&task1, first_flag));
    let run2 = thread::spawn(move || second.execute(&task2, Arc::new(AtomicBool::new(false))));
    wait_for_file(&fixture.workspace(1).join("leader.pid"));
    flag.store(true, Ordering::Release);
    assert_eq!(run1.join().unwrap().outcome, Outcome::Cancelled);
    let result2 = run2.join().unwrap();
    assert_eq!(result2.outcome, Outcome::Success, "{}", result2.to_json());
    assert_eq!(result2.agent.unwrap().stdout, "completed");
}

#[test]
fn host_api_token_is_not_passed_to_commands() {
    let mut fixture = Fixture::new("test -z \"${RELAY_TOKEN+x}\" && printf safe");
    fixture
        .config
        .agents
        .get_mut("fake")
        .unwrap()
        .env
        .insert("RELAY_TOKEN".into(), "must-not-leak".into());
    let result = fixture.run(&fixture.task(1));
    assert_eq!(result.outcome, Outcome::Success, "{}", result.to_json());
    assert_eq!(result.agent.unwrap().stdout, "safe");
}

#[test]
fn existing_generation_stays_unknown_even_with_cancel_or_retention_limit() {
    let mut fixture = Fixture::new("exit 0");
    assert_eq!(fixture.run(&fixture.task(1)).outcome, Outcome::Success);
    fixture.config.max_retained_workspaces = 1;
    let result = Host::new(fixture.config.clone())
        .unwrap()
        .execute(&fixture.task(1), Arc::new(AtomicBool::new(true)));
    assert_eq!(result.outcome, Outcome::Unknown);
}

#[test]
fn fast_workspace_growth_is_checked_before_success() {
    let mut fixture = Fixture::new("head -c 65536 /dev/zero > large.bin");
    fixture.config.max_snapshot_bytes = 1024;
    let result = fixture.run(&fixture.task(1));
    assert_eq!(result.outcome, Outcome::Failure, "{}", result.to_json());
}
