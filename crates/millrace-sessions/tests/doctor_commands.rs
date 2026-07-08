use std::{
    collections::BTreeMap,
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::Path,
};

use assert_cmd::prelude::*;
use millrace_sessions_core::{
    ids::SessionId,
    paths::StatePaths,
    state::{AttentionState, ProcessState, SessionMeta, SessionRole, SpawnMode, WorkerMeta},
    storage::write_json_atomic,
};
use serde_json::Value;

#[test]
fn doctor_json_reports_machine_readable_issues_without_starting_host() {
    let temp = tempfile::tempdir().unwrap();
    let paths = prepared_state(temp.path());
    fs::set_permissions(&paths.root, fs::Permissions::from_mode(0o755)).unwrap();

    let listener = UnixListener::bind(&paths.control_sock).unwrap();
    drop(listener);

    let corrupt_root = paths.sessions_dir.join("corrupt-session");
    fs::create_dir_all(&corrupt_root).unwrap();
    fs::write(corrupt_root.join("meta.json"), b"{not json").unwrap();

    let output = millmux_command(&paths)
        .args(["doctor", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value: Value = serde_json::from_slice(&output).expect("doctor json");
    assert_issue(&value, "bad_state_dir_permissions", "critical");
    assert_issue(&value, "stale_host_socket", "critical");
    assert_issue(&value, "corrupted_meta_json", "critical");
    assert!(value["repairs"].as_array().unwrap().is_empty());
    assert!(!paths.host_json.exists(), "doctor should not start a host");
}

#[test]
fn doctor_archive_stale_repair_is_explicit_json() {
    let temp = tempfile::tempdir().unwrap();
    let paths = prepared_state(temp.path());
    let stale = sample_meta(ProcessState::Lost, temp.path());
    write_session(&paths, &stale);

    let output = millmux_command(&paths)
        .args(["doctor", "--repair", "ARCHIVE_STALE", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value: Value = serde_json::from_slice(&output).expect("doctor repair json");
    let repairs = value["repairs"].as_array().expect("repairs array");
    assert!(repairs.iter().any(|repair| {
        repair["mode"] == "ARCHIVE_STALE"
            && repair["status"] == "applied"
            && repair["session_id"] == stale.id.to_string()
    }));
    assert!(!paths.session_paths(stale.id).root.exists());
    assert!(paths.archive_dir.join(stale.id.to_string()).exists());
}

#[test]
fn doctor_json_reports_worker_child_liveness_issue_without_starting_host() {
    let temp = tempfile::tempdir().unwrap();
    let paths = prepared_state(temp.path());
    let dead_worker_pid = dead_process_pid();
    let live_child_pid = std::process::id();
    let mut session = sample_meta(ProcessState::Running, temp.path());
    session.worker_pid = Some(dead_worker_pid);
    session.child_pid = Some(live_child_pid);
    session.child_pgid = Some(live_child_pid);
    let mut worker = sample_worker(session.id, dead_worker_pid);
    worker.child_pid = Some(live_child_pid);
    worker.child_pgid = Some(live_child_pid);
    write_session(&paths, &session);
    write_json_atomic(&paths.session_paths(session.id).worker_json, &worker).unwrap();

    let output = millmux_command(&paths)
        .args(["doctor", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value: Value = serde_json::from_slice(&output).expect("doctor json");
    assert_issue(&value, "orphaned_child_process", "critical");
    assert_issue(&value, "worker_socket_missing", "warning");
    let issues = value["issues"].as_array().expect("issues array");
    let issue = issues
        .iter()
        .find(|issue| issue["code"] == "orphaned_child_process")
        .expect("worker/child liveness issue");
    assert_eq!(issue["details"]["worker_liveness"], "dead");
    assert_eq!(issue["details"]["child_liveness"], "alive");
    assert!(issue["details"]["recovery_actions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|action| action == "signal_child"));
    assert!(!paths.host_json.exists(), "doctor should not start a host");
}

fn prepared_state(root: &Path) -> StatePaths {
    let paths = StatePaths::new(root.join("state"));
    fs::create_dir_all(&paths.sessions_dir).unwrap();
    fs::create_dir_all(&paths.archive_dir).unwrap();
    fs::create_dir_all(paths.root.join("w")).unwrap();
    fs::set_permissions(&paths.root, fs::Permissions::from_mode(0o700)).unwrap();
    paths
}

fn write_session(paths: &StatePaths, meta: &SessionMeta) {
    let session_paths = paths.session_paths(meta.id);
    fs::create_dir_all(&session_paths.root).unwrap();
    write_json_atomic(&session_paths.meta_json, meta).unwrap();
    fs::write(&session_paths.pty_log, b"ready\n").unwrap();
    millrace_sessions_core::events::append_event(
        &session_paths.events_jsonl,
        &millrace_sessions_core::events::SessionEvent::new(
            meta.id,
            millrace_sessions_core::events::SessionEventKind::SessionCreated,
        ),
    )
    .unwrap();
}

fn sample_meta(state: ProcessState, root: &Path) -> SessionMeta {
    SessionMeta {
        id: SessionId::new(),
        name: None,
        role: SessionRole::Shell,
        process_state: state,
        attention_state: AttentionState::Active,
        workspace: None,
        cwd: root.to_path_buf(),
        argv: vec!["sh".to_string()],
        spawn_mode: SpawnMode::Pty,
        monitor_profile: millrace_sessions_core::state::MonitorProfile::Auto,
        env: BTreeMap::new(),
        worker_pid: None,
        child_pid: None,
        child_pgid: None,
        started_at: None,
        ended_at: None,
        stop_requested_at: None,
        stop_reason: None,
        exit_code: None,
        exit_signal: None,
        failure_message: None,
        created_at: "2026-05-20T18:00:00Z".to_string(),
        updated_at: "2026-05-20T18:01:00Z".to_string(),
    }
}

fn sample_worker(session_id: SessionId, pid: u32) -> WorkerMeta {
    WorkerMeta {
        session_id,
        pid,
        child_pid: None,
        child_pgid: None,
        spawn_mode: SpawnMode::Pty,
        process_state: ProcessState::Running,
        started_at: "2026-05-20T18:00:00Z".to_string(),
        ended_at: None,
        stop_requested_at: None,
        stop_reason: None,
        exit_code: None,
        exit_signal: None,
        attached_clients: 0,
        input_owner: None,
        updated_at: "2026-05-20T18:01:00Z".to_string(),
    }
}

fn millmux_command(paths: &StatePaths) -> std::process::Command {
    let mut command = std::process::Command::cargo_bin("millmux").expect("millmux binary");
    command.env("MILLMUX_STATE_DIR", &paths.root);
    command
}

fn dead_process_pid() -> u32 {
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg("exit 0")
        .spawn()
        .unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

fn assert_issue(value: &Value, code: &str, severity: &str) {
    let issues = value["issues"].as_array().expect("issues array");
    let issue = issues
        .iter()
        .find(|issue| issue["code"] == code)
        .unwrap_or_else(|| panic!("missing issue {code}; got {issues:#?}"));
    assert_eq!(issue["severity"], severity);
}
