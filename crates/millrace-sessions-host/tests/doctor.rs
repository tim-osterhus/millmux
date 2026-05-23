use std::{
    collections::BTreeMap,
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::Path,
    process::Command,
};

use millrace_sessions_core::{
    events::{read_events, SessionEvent, SessionEventKind},
    ids::SessionId,
    paths::StatePaths,
    protocol::{
        DoctorRepairMode, DoctorRepairStatus, DoctorRequest, DoctorResponse, DoctorSeverity,
    },
    scrollback::ScrollbackBuffer,
    state::{AttentionState, HostMeta, ProcessState, SessionMeta, SessionRole, WorkerMeta},
    storage::{append_raw_pty_log, create_private_dir_all, read_json, write_json_atomic},
};
use millrace_sessions_host::{
    doctor::run_doctor, reconcile::reconcile_startup, server::dispatch_json_line,
};
use serde_json::json;

#[test]
fn doctor_reports_structured_state_socket_and_session_issues() {
    let temp = tempfile::tempdir().unwrap();
    let paths = prepared_state(temp.path());
    fs::set_permissions(&paths.root, fs::Permissions::from_mode(0o755)).unwrap();

    let listener = UnixListener::bind(&paths.control_sock).unwrap();
    drop(listener);
    fs::set_permissions(&paths.control_sock, fs::Permissions::from_mode(0o666)).unwrap();

    let corrupt_root = paths.sessions_dir.join("corrupt-session");
    fs::create_dir_all(&corrupt_root).unwrap();
    fs::write(corrupt_root.join("meta.json"), b"{not json").unwrap();

    let missing_pid = sample_meta(ProcessState::Running, temp.path());
    write_session(&paths, &missing_pid, None, true);

    let dead_pid = dead_process_pid();
    let mut stale_worker = sample_meta(ProcessState::Running, temp.path());
    stale_worker.worker_pid = Some(dead_pid);
    let worker = sample_worker(stale_worker.id, dead_pid, ProcessState::Running);
    write_session(&paths, &stale_worker, Some(worker), true);

    let missing_log = sample_meta(ProcessState::Exited, temp.path());
    write_session(&paths, &missing_log, None, false);

    let result = run_doctor(&paths, None, &DoctorRequest::default()).unwrap();

    assert_issue(
        &result,
        "bad_state_dir_permissions",
        DoctorSeverity::Critical,
    );
    assert_issue(&result, "bad_socket_permissions", DoctorSeverity::Critical);
    assert_issue(&result, "stale_host_socket", DoctorSeverity::Critical);
    assert_issue(&result, "corrupted_meta_json", DoctorSeverity::Critical);
    assert_issue(&result, "missing_pid", DoctorSeverity::Critical);
    assert_issue(&result, "stale_worker_record", DoctorSeverity::Warning);
    assert_issue(&result, "missing_pty_log", DoctorSeverity::Warning);
}

#[test]
fn doctor_reports_responsive_host_socket_as_info() {
    let temp = tempfile::tempdir().unwrap();
    let paths = prepared_state(temp.path());
    let _listener = UnixListener::bind(&paths.control_sock).unwrap();
    fs::set_permissions(&paths.control_sock, fs::Permissions::from_mode(0o600)).unwrap();

    let result = run_doctor(&paths, Some(&host_meta(&paths)), &DoctorRequest::default()).unwrap();

    assert_issue(&result, "host_socket_responsive", DoctorSeverity::Info);
}

#[test]
fn host_doctor_dispatches_session_control_method() {
    let temp = tempfile::tempdir().unwrap();
    let paths = prepared_state(temp.path());
    let host = host_meta(&paths);

    let line = format!(
        "{}\n",
        serde_json::to_string(&json!({
            "id": "doctor-1",
            "method": "host.doctor",
            "params": {}
        }))
        .unwrap()
    );
    let response = dispatch_json_line(&line, &paths, &host);

    assert!(response.ok, "{response:#?}");
    let result = response.result_as::<DoctorResponse>().unwrap();
    assert_eq!(result.schema_version, 1);
    assert!(result
        .issues
        .iter()
        .any(|issue| issue.code == "state_dir_permissions_ok"));
}

#[test]
fn doctor_archive_stale_repair_archives_only_proven_stale_records() {
    let temp = tempfile::tempdir().unwrap();
    let paths = prepared_state(temp.path());

    let stale = sample_meta(ProcessState::Lost, temp.path());
    write_session(&paths, &stale, None, true);

    let mut live = sample_meta(ProcessState::Running, temp.path());
    live.worker_pid = Some(std::process::id());
    let live_worker = sample_worker(live.id, std::process::id(), ProcessState::Running);
    write_session(&paths, &live, Some(live_worker), true);

    let corrupt_root = paths.sessions_dir.join("corrupt-session");
    fs::create_dir_all(&corrupt_root).unwrap();
    fs::write(corrupt_root.join("meta.json"), b"{not json").unwrap();

    let result = run_doctor(
        &paths,
        None,
        &DoctorRequest {
            repair: Some(DoctorRepairMode::ArchiveStale),
        },
    )
    .unwrap();

    let stale_archive = paths.archive_dir.join(stale.id.to_string());
    assert!(!paths.session_paths(stale.id).root.exists());
    assert!(stale_archive.exists());
    assert!(paths.session_paths(live.id).root.exists());
    assert!(corrupt_root.exists());

    let repair = result
        .repairs
        .iter()
        .find(|repair| repair.session_id == Some(stale.id))
        .expect("stale session repair summary");
    assert_eq!(repair.status, DoctorRepairStatus::Applied);
    assert_eq!(repair.archive_path.as_ref(), Some(&stale_archive));

    let events = read_events(stale_archive.join("events.jsonl")).unwrap();
    assert!(events
        .iter()
        .any(|event| event.kind == SessionEventKind::DoctorRepair));
}

#[cfg(unix)]
#[test]
fn doctor_archive_preserves_private_session_artifacts() {
    let temp = tempfile::tempdir().unwrap();
    let paths = prepared_state(temp.path());
    let stale = sample_meta(ProcessState::Lost, temp.path());
    let worker = sample_worker(stale.id, dead_process_pid(), ProcessState::Lost);
    let session_paths = paths.session_paths(stale.id);

    create_private_dir_all(&session_paths.root).unwrap();
    write_json_atomic(&session_paths.meta_json, &stale).unwrap();
    write_json_atomic(&session_paths.worker_json, &worker).unwrap();
    append_raw_pty_log(&session_paths.pty_log, b"ready\n").unwrap();
    millrace_sessions_core::events::append_event(
        &session_paths.events_jsonl,
        &SessionEvent::new(stale.id, SessionEventKind::SessionCreated),
    )
    .unwrap();
    let mut scrollback = ScrollbackBuffer::new(10);
    scrollback.push_line("ready");
    scrollback
        .persist_snapshot(&session_paths.scrollback_snapshot)
        .unwrap();

    run_doctor(
        &paths,
        None,
        &DoctorRequest {
            repair: Some(DoctorRepairMode::ArchiveStale),
        },
    )
    .unwrap();

    let archive_root = paths.archive_dir.join(stale.id.to_string());
    assert_private_dir(&archive_root);
    assert_private_file(&archive_root.join("meta.json"));
    assert_private_file(&archive_root.join("worker.json"));
    assert_private_file(&archive_root.join("events.jsonl"));
    assert_private_file(&archive_root.join("pty.log"));
    assert_private_file(&archive_root.join("scrollback.snapshot"));
}

#[test]
fn startup_reconciliation_preserves_live_and_marks_dead_without_deleting() {
    let temp = tempfile::tempdir().unwrap();
    let paths = prepared_state(temp.path());

    let mut live = sample_meta(ProcessState::Running, temp.path());
    live.worker_pid = Some(std::process::id());
    write_session(&paths, &live, None, true);

    let mut dead = sample_meta(ProcessState::Running, temp.path());
    dead.worker_pid = Some(dead_process_pid());
    write_session(&paths, &dead, None, true);

    let terminal = sample_meta(ProcessState::Exited, temp.path());
    write_session(&paths, &terminal, None, true);

    let summary = reconcile_startup(&paths).unwrap();

    assert_eq!(summary.preserved, 1);
    assert_eq!(summary.marked_terminal, 1);
    let live_after: SessionMeta = read_json(&paths.session_paths(live.id).meta_json).unwrap();
    let dead_after: SessionMeta = read_json(&paths.session_paths(dead.id).meta_json).unwrap();
    let terminal_after: SessionMeta =
        read_json(&paths.session_paths(terminal.id).meta_json).unwrap();

    assert_eq!(live_after.process_state, ProcessState::Running);
    assert_eq!(dead_after.process_state, ProcessState::Lost);
    assert_eq!(terminal_after.process_state, ProcessState::Exited);
    assert!(paths.session_paths(dead.id).root.exists());

    let events = read_events(paths.session_paths(dead.id).events_jsonl).unwrap();
    assert!(events.iter().any(|event| {
        event.kind == SessionEventKind::StateChanged
            && event.process_state == Some(ProcessState::Lost)
            && event.fields.get("reason").map(String::as_str) == Some("startup_reconcile")
    }));
}

fn prepared_state(root: &Path) -> StatePaths {
    let paths = StatePaths::new(root.join("state"));
    create_private_dir_all(&paths.sessions_dir).unwrap();
    create_private_dir_all(&paths.archive_dir).unwrap();
    fs::create_dir_all(paths.root.join("w")).unwrap();
    fs::set_permissions(&paths.root, fs::Permissions::from_mode(0o700)).unwrap();
    paths
}

fn write_session(
    paths: &StatePaths,
    meta: &SessionMeta,
    worker: Option<WorkerMeta>,
    write_pty_log: bool,
) {
    let session_paths = paths.session_paths(meta.id);
    create_private_dir_all(&session_paths.root).unwrap();
    write_json_atomic(&session_paths.meta_json, meta).unwrap();
    if let Some(worker) = worker {
        write_json_atomic(&session_paths.worker_json, &worker).unwrap();
    }
    if write_pty_log {
        append_raw_pty_log(&session_paths.pty_log, b"ready\n").unwrap();
    }
    millrace_sessions_core::events::append_event(
        &session_paths.events_jsonl,
        &SessionEvent::new(meta.id, SessionEventKind::SessionCreated),
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

fn sample_worker(session_id: SessionId, pid: u32, state: ProcessState) -> WorkerMeta {
    WorkerMeta {
        session_id,
        pid,
        child_pid: None,
        child_pgid: None,
        process_state: state,
        started_at: "2026-05-20T18:00:00Z".to_string(),
        ended_at: None,
        exit_code: None,
        exit_signal: None,
        updated_at: "2026-05-20T18:01:00Z".to_string(),
    }
}

fn host_meta(paths: &StatePaths) -> HostMeta {
    HostMeta {
        pid: std::process::id(),
        state_root: paths.root.clone(),
        control_socket: paths.control_sock.clone(),
        started_at: "2026-05-20T18:00:00Z".to_string(),
        updated_at: "2026-05-20T18:01:00Z".to_string(),
    }
}

fn assert_issue(result: &DoctorResponse, code: &str, severity: DoctorSeverity) {
    let issue = result
        .issues
        .iter()
        .find(|issue| issue.code == code)
        .unwrap_or_else(|| panic!("missing issue {code}; got {:#?}", result.issues));
    assert_eq!(issue.severity, severity);
}

#[cfg(unix)]
fn assert_private_dir(path: &Path) {
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o700,
        "{} should be 0700",
        path.display()
    );
}

#[cfg(unix)]
fn assert_private_file(path: &Path) {
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600,
        "{} should be 0600",
        path.display()
    );
}

fn dead_process_pid() -> u32 {
    let mut child = Command::new("sh").arg("-c").arg("exit 0").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}
