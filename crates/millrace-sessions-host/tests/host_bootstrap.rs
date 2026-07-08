use std::{collections::BTreeMap, fs, os::unix::net::UnixListener, path::Path};

use millrace_sessions_core::{
    events::{append_event, read_events, SessionEvent, SessionEventKind},
    ids::SessionId,
    paths::StatePaths,
    state::{
        AttentionState, HostMeta, ProcessState, SessionMeta, SessionRole, SpawnMode, WorkerMeta,
    },
    storage::{read_json, write_json_atomic},
    workspace::WorkspaceIdentity,
};
use millrace_sessions_host::{
    bootstrap::{bootstrap_foreground, HostBootstrapError},
    registry::HostRegistry,
};

#[test]
fn bootstrap_creates_private_state_layout_and_host_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));

    let host = bootstrap_foreground(paths.clone()).expect("bootstrap succeeds");

    assert!(paths.root.is_dir());
    assert!(paths.sessions_dir.is_dir());
    assert!(paths.archive_dir.is_dir());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            fs::metadata(&paths.root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&paths.sessions_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&paths.archive_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&paths.host_json).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    let meta: HostMeta = read_json(&paths.host_json).expect("host metadata is written");
    assert_eq!(meta.pid, std::process::id());
    assert_eq!(meta.state_root, paths.root);
    assert_eq!(meta.control_socket, paths.control_sock);
    assert_eq!(host.meta(), &meta);
}

#[test]
fn second_bootstrap_fails_without_deleting_live_socket_or_host_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let _first = bootstrap_foreground(paths.clone()).expect("first lock succeeds");
    let original_host_json = fs::read_to_string(&paths.host_json).expect("host metadata exists");
    let listener = UnixListener::bind(&paths.control_sock).expect("live socket binds");

    let second = bootstrap_foreground(paths.clone()).expect_err("second lock fails");

    assert!(matches!(second, HostBootstrapError::AlreadyRunning { .. }));
    assert!(paths.control_sock.exists(), "live socket remains in place");
    assert_eq!(
        fs::read_to_string(&paths.host_json).expect("host metadata remains readable"),
        original_host_json
    );
    drop(listener);
}

#[test]
fn registry_loads_valid_metadata_and_retains_corrupt_load_issues() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    fs::create_dir_all(&paths.sessions_dir).unwrap();

    let valid = sample_session(paths.root.join("workspace"), SessionRole::Shell);
    write_session_meta(&paths, &valid);

    let corrupt_dir = paths.sessions_dir.join("corrupt-session");
    fs::create_dir_all(&corrupt_dir).unwrap();
    let corrupt_meta = corrupt_dir.join("meta.json");
    fs::write(&corrupt_meta, "{not valid json").unwrap();

    let registry = HostRegistry::load(paths.clone()).expect("registry loads");

    assert_eq!(registry.sessions().len(), 1);
    assert_eq!(registry.load_issues().len(), 1);
    assert_eq!(registry.load_issues()[0].path, corrupt_meta);
    assert!(!registry.load_issues()[0].error.is_empty());
    assert!(
        registry.load_issues()[0].path.exists(),
        "corrupt metadata is not deleted"
    );
}

#[test]
fn registry_detects_duplicate_millrace_daemon_through_symlink_alias() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    fs::create_dir_all(&paths.sessions_dir).unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let alias = temp.path().join("workspace-alias");
    std::os::unix::fs::symlink(&workspace, &alias).unwrap();

    let daemon = sample_session(&workspace, SessionRole::MillraceDaemon);
    write_session_meta(&paths, &daemon);

    let registry = HostRegistry::load(paths).expect("registry loads");
    let duplicate = registry
        .find_duplicate_millrace_daemon(&alias)
        .expect("lookup succeeds")
        .expect("duplicate daemon is found");

    assert_eq!(duplicate.session_id, daemon.id);
    assert_eq!(duplicate.role, SessionRole::MillraceDaemon);
}

#[test]
fn bootstrap_reconcile_preserves_raw_evidence_when_marking_stale_active_session() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    fs::create_dir_all(&paths.sessions_dir).unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    let meta = sample_session(&workspace, SessionRole::Shell);
    let session_paths = paths.session_paths(meta.id);
    fs::create_dir_all(&session_paths.root).unwrap();
    write_json_atomic(&session_paths.meta_json, &meta).unwrap();
    write_json_atomic(
        &session_paths.worker_json,
        &WorkerMeta {
            session_id: meta.id,
            pid: 0,
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
            attached_clients: 1,
            input_owner: Some("lost-client".to_string()),
            updated_at: "2026-05-20T18:01:00Z".to_string(),
        },
    )
    .unwrap();
    fs::write(&session_paths.pty_log, b"raw pty evidence\n").unwrap();
    fs::write(&session_paths.raw_replay_ring, b"raw replay evidence").unwrap();
    append_event(
        &session_paths.events_jsonl,
        &SessionEvent::new(meta.id, SessionEventKind::Output),
    )
    .unwrap();

    let _host = bootstrap_foreground(paths.clone()).expect("bootstrap succeeds");

    assert_eq!(
        fs::read_to_string(&session_paths.pty_log).unwrap(),
        "raw pty evidence\n"
    );
    assert_eq!(
        fs::read(&session_paths.raw_replay_ring).unwrap(),
        b"raw replay evidence"
    );
    let reconciled: SessionMeta = read_json(&session_paths.meta_json).unwrap();
    assert_eq!(reconciled.process_state, ProcessState::Lost);
    let worker: WorkerMeta = read_json(&session_paths.worker_json).unwrap();
    assert_eq!(worker.process_state, ProcessState::Lost);
    assert_eq!(worker.attached_clients, 0);
    assert_eq!(worker.input_owner, None);

    let events = read_events(&session_paths.events_jsonl).unwrap();
    assert!(events
        .iter()
        .any(|event| event.kind == SessionEventKind::Output));
    assert!(events.iter().any(|event| {
        event.kind == SessionEventKind::StateChanged
            && event.fields.get("reason").map(String::as_str) == Some("startup_reconcile")
            && event.fields.get("liveness_reason").map(String::as_str)
                == Some("all_recorded_processes_dead")
    }));
}

fn write_session_meta(paths: &StatePaths, meta: &SessionMeta) {
    let session_paths = paths.session_paths(meta.id);
    fs::create_dir_all(&session_paths.root).unwrap();
    write_json_atomic(&session_paths.meta_json, meta).unwrap();
}

fn sample_session(workspace: impl AsRef<Path>, role: SessionRole) -> SessionMeta {
    let workspace = workspace.as_ref();
    fs::create_dir_all(workspace).unwrap();
    SessionMeta {
        id: SessionId::new(),
        name: Some("daemon".to_string()),
        role,
        process_state: ProcessState::Running,
        attention_state: AttentionState::MillraceIdle,
        workspace: Some(WorkspaceIdentity::capture(workspace).unwrap()),
        cwd: workspace.to_path_buf(),
        argv: vec![
            "millrace".to_string(),
            "run".to_string(),
            "daemon".to_string(),
        ],
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
