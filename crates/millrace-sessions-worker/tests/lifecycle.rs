use std::{collections::BTreeMap, fs, path::Path};

use millrace_sessions_core::{
    events::{read_events, SessionEventKind},
    ids::SessionId,
    paths::StatePaths,
    state::{AttentionState, ProcessState, SessionMeta, SessionRole, SpawnMode, WorkerMeta},
    storage::{read_json, write_json_atomic},
};
use millrace_sessions_worker::{
    lifecycle::{
        record_failed_start, record_process_exit, record_running, write_worker_meta, WorkerFacts,
    },
    run_worker,
};

#[test]
fn lifecycle_writes_worker_facts_and_state_transitions() {
    let temp = tempfile::tempdir().unwrap();
    let state_paths = StatePaths::new(temp.path().join("state"));
    let session = sample_session(temp.path());
    let paths = state_paths.session_paths(session.id);
    fs::create_dir_all(&paths.root).unwrap();
    write_json_atomic(&paths.meta_json, &session).unwrap();

    write_worker_meta(
        &paths,
        WorkerFacts {
            worker_pid: 42,
            child_pid: Some(99),
            child_pgid: Some(99),
            spawn_mode: SpawnMode::Pty,
        },
    )
    .expect("write worker meta");
    record_running(&paths, Some(99), Some(99)).expect("running transition");
    record_process_exit(&paths, 7, None).expect("exit transition");

    let worker: WorkerMeta = read_json(&paths.worker_json).unwrap();
    assert_eq!(worker.pid, 42);
    assert_eq!(worker.child_pid, Some(99));
    assert_eq!(worker.child_pgid, Some(99));

    let meta: SessionMeta = read_json(&paths.meta_json).unwrap();
    assert_eq!(meta.process_state, ProcessState::Exited);
    assert_eq!(meta.exit_code, Some(7));
    assert_eq!(meta.exit_signal, None);
}

#[test]
fn lifecycle_clears_attach_state_on_process_exit() {
    let temp = tempfile::tempdir().unwrap();
    let state_paths = StatePaths::new(temp.path().join("state"));
    let session = sample_session(temp.path());
    let paths = state_paths.session_paths(session.id);
    fs::create_dir_all(&paths.root).unwrap();
    write_json_atomic(&paths.meta_json, &session).unwrap();

    write_worker_meta(
        &paths,
        WorkerFacts {
            worker_pid: 42,
            child_pid: Some(99),
            child_pgid: Some(99),
            spawn_mode: SpawnMode::Pty,
        },
    )
    .expect("write worker meta");
    record_running(&paths, Some(99), Some(99)).expect("running transition");
    let mut worker: WorkerMeta = read_json(&paths.worker_json).unwrap();
    worker.attached_clients = 3;
    worker.input_owner = Some("stale-owner".to_string());
    write_json_atomic(&paths.worker_json, &worker).unwrap();

    record_process_exit(&paths, 0, None).expect("exit transition");

    let worker: WorkerMeta = read_json(&paths.worker_json).unwrap();
    assert_eq!(worker.process_state, ProcessState::Exited);
    assert_eq!(worker.attached_clients, 0);
    assert_eq!(worker.input_owner, None);
}

#[cfg(unix)]
#[test]
fn worker_lifecycle_writes_private_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let state_paths = StatePaths::new(temp.path().join("state"));
    let session = sample_session(temp.path());
    let paths = state_paths.session_paths(session.id);
    fs::create_dir_all(&paths.root).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(&paths.root, fs::Permissions::from_mode(0o777)).unwrap();
    }
    write_json_atomic(&paths.meta_json, &session).unwrap();

    write_worker_meta(
        &paths,
        WorkerFacts {
            worker_pid: 42,
            child_pid: Some(99),
            child_pgid: Some(99),
            spawn_mode: SpawnMode::Pty,
        },
    )
    .expect("write worker meta");
    record_running(&paths, Some(99), Some(99)).expect("running transition");

    assert_private_dir(&paths.root);
    assert_private_file(&paths.meta_json);
    assert_private_file(&paths.worker_json);
    assert_private_file(&paths.events_jsonl);
}

#[test]
fn lifecycle_records_failed_start_with_message() {
    let temp = tempfile::tempdir().unwrap();
    let state_paths = StatePaths::new(temp.path().join("state"));
    let session = sample_session(temp.path());
    let paths = state_paths.session_paths(session.id);
    fs::create_dir_all(&paths.root).unwrap();
    write_json_atomic(&paths.meta_json, &session).unwrap();

    record_failed_start(&paths, "command not found").expect("failed start");

    let meta: SessionMeta = read_json(&paths.meta_json).unwrap();
    assert_eq!(meta.process_state, ProcessState::FailedStart);
    assert_eq!(meta.failure_message.as_deref(), Some("command not found"));
}

#[test]
fn lifecycle_run_worker_persists_worker_meta_when_child_spawn_fails() {
    let temp = tempfile::tempdir().unwrap();
    let state_paths = StatePaths::new(temp.path().join("state"));
    let mut session = sample_session(temp.path());
    session.argv = vec!["definitely-missing-millmux-command".to_string()];
    let paths = state_paths.session_paths(session.id);
    fs::create_dir_all(&paths.root).unwrap();
    write_json_atomic(&paths.meta_json, &session).unwrap();

    let result = run_worker(session.id, state_paths.root.clone());

    assert!(result.is_err());
    let worker: WorkerMeta = read_json(&paths.worker_json).unwrap();
    assert_eq!(worker.pid, std::process::id());
    assert_eq!(worker.child_pid, None);
    assert_eq!(worker.child_pgid, None);
    assert_eq!(worker.process_state, ProcessState::FailedStart);

    let meta: SessionMeta = read_json(&paths.meta_json).unwrap();
    assert_eq!(meta.worker_pid, Some(std::process::id()));
    assert_eq!(meta.child_pid, None);
    assert_eq!(meta.child_pgid, None);
    assert_eq!(meta.process_state, ProcessState::FailedStart);
    assert!(meta.failure_message.is_some());

    let events = read_events(&paths.events_jsonl).unwrap();
    let event = events
        .iter()
        .find(|event| event.kind == SessionEventKind::StateChanged)
        .expect("failed-start state change event");
    assert_eq!(event.process_state, Some(ProcessState::FailedStart));
    assert!(event.message.is_some());
}

#[cfg(unix)]
#[test]
fn pipe_worker_captures_separate_streams_without_pty_artifacts() {
    let temp = tempfile::tempdir().unwrap();
    let state_paths = StatePaths::new(temp.path().join("state"));
    let mut session = sample_session(temp.path());
    session.spawn_mode = SpawnMode::Pipe;
    session.argv = vec![
        "sh".to_string(),
        "-c".to_string(),
        "printf 'pipe-out\\n'; printf 'pipe-err\\n' >&2; exit 7".to_string(),
    ];
    let paths = state_paths.session_paths(session.id);
    fs::create_dir_all(&paths.root).unwrap();
    write_json_atomic(&paths.meta_json, &session).unwrap();

    run_worker(session.id, state_paths.root.clone()).expect("pipe worker exits cleanly");

    assert_eq!(fs::read_to_string(&paths.stdout_log).unwrap(), "pipe-out\n");
    assert_eq!(fs::read_to_string(&paths.stderr_log).unwrap(), "pipe-err\n");
    assert!(
        !paths.pty_log.exists(),
        "pipe sessions must not fake pty.log"
    );
    assert!(!paths.scrollback_snapshot.exists());
    assert!(!paths.terminal_snapshot.exists());
    assert!(!paths.raw_replay_ring.exists());

    let worker: WorkerMeta = read_json(&paths.worker_json).unwrap();
    assert_eq!(worker.spawn_mode, SpawnMode::Pipe);
    assert_eq!(worker.process_state, ProcessState::Exited);
    assert_eq!(worker.exit_code, Some(7));

    let meta: SessionMeta = read_json(&paths.meta_json).unwrap();
    assert_eq!(meta.spawn_mode, SpawnMode::Pipe);
    assert_eq!(meta.process_state, ProcessState::Exited);
    assert_eq!(meta.exit_code, Some(7));

    let events = read_events(&paths.events_jsonl).unwrap();
    assert!(events.iter().any(|event| {
        event.kind == SessionEventKind::Output
            && event.message.as_deref() == Some("pipe-out\n")
            && event.fields.get("stream").map(String::as_str) == Some("stdout")
            && event.fields.get("record_kind").map(String::as_str) == Some("chunk")
    }));
    assert!(events.iter().any(|event| {
        event.kind == SessionEventKind::Output
            && event.message.as_deref() == Some("pipe-err\n")
            && event.fields.get("stream").map(String::as_str) == Some("stderr")
            && event.fields.get("record_kind").map(String::as_str) == Some("chunk")
    }));
}

fn sample_session(cwd: &Path) -> SessionMeta {
    let now = "2026-05-20T18:00:00Z".to_string();
    SessionMeta {
        id: SessionId::new(),
        name: Some("sample".to_string()),
        role: SessionRole::Shell,
        process_state: ProcessState::Starting,
        attention_state: AttentionState::Active,
        attention_items: Vec::new(),
        status_summary: None,
        workspace: None,
        cwd: cwd.to_path_buf(),
        argv: vec!["sh".to_string(), "-c".to_string(), "echo ready".to_string()],
        spawn_mode: SpawnMode::Pty,
        monitor_profile: millrace_sessions_core::state::MonitorProfile::Auto,
        env: BTreeMap::new(),
        created_at: now.clone(),
        updated_at: now,
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
    }
}

#[cfg(unix)]
fn assert_private_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o700,
        "{} should be 0700",
        path.display()
    );
}

#[cfg(unix)]
fn assert_private_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600,
        "{} should be 0600",
        path.display()
    );
}
