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
    state::{AttentionState, ProcessState, SessionMeta, SessionRole},
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
        monitor_profile: millrace_sessions_core::state::MonitorProfile::Auto,
        env: BTreeMap::new(),
        worker_pid: None,
        child_pid: None,
        child_pgid: None,
        started_at: None,
        ended_at: None,
        exit_code: None,
        exit_signal: None,
        failure_message: None,
        created_at: "2026-05-20T18:00:00Z".to_string(),
        updated_at: "2026-05-20T18:01:00Z".to_string(),
    }
}

fn millmux_command(paths: &StatePaths) -> std::process::Command {
    let mut command = std::process::Command::cargo_bin("millmux").expect("millmux binary");
    command.env("MILLMUX_STATE_DIR", &paths.root);
    command
}

fn assert_issue(value: &Value, code: &str, severity: &str) {
    let issues = value["issues"].as_array().expect("issues array");
    let issue = issues
        .iter()
        .find(|issue| issue["code"] == code)
        .unwrap_or_else(|| panic!("missing issue {code}; got {issues:#?}"));
    assert_eq!(issue["severity"], severity);
}
