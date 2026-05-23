use std::{collections::BTreeMap, fs, path::Path};

use millrace_sessions_core::{
    events::{read_events, SessionEventKind},
    ids::SessionId,
    paths::StatePaths,
    state::{AttentionState, ProcessState, SessionMeta, SessionRole, WorkerMeta},
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

fn sample_session(cwd: &Path) -> SessionMeta {
    let now = "2026-05-20T18:00:00Z".to_string();
    SessionMeta {
        id: SessionId::new(),
        name: Some("sample".to_string()),
        role: SessionRole::Shell,
        process_state: ProcessState::Starting,
        attention_state: AttentionState::Active,
        workspace: None,
        cwd: cwd.to_path_buf(),
        argv: vec!["sh".to_string(), "-c".to_string(), "echo ready".to_string()],
        env: BTreeMap::new(),
        created_at: now.clone(),
        updated_at: now,
        worker_pid: None,
        child_pid: None,
        child_pgid: None,
        started_at: None,
        ended_at: None,
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
