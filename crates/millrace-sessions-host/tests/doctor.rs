use std::{
    collections::BTreeMap,
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::Path,
    process::Command,
};

use millrace_sessions_core::{
    events::{read_events, SessionEvent, SessionEventKind},
    ids::{SessionId, UiId},
    paths::StatePaths,
    protocol::{
        DoctorRepairMode, DoctorRepairStatus, DoctorRequest, DoctorResponse, DoctorSeverity,
    },
    scrollback::{legacy_line_scrollback_contains_tui_sequences, ScrollbackBuffer},
    state::{
        AttentionState, HostMeta, MonitorProfile, ProcessState, SessionMeta, SessionRole,
        SpawnMode, UiEvent, WorkerMeta,
    },
    storage::{
        append_raw_pty_log, create_private_dir_all, read_json, read_json_lines, write_json_atomic,
    },
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
fn doctor_reports_orphaned_child_process_with_recovery_actions() {
    let temp = tempfile::tempdir().unwrap();
    let paths = prepared_state(temp.path());

    let dead_worker_pid = dead_process_pid();
    let live_child_pid = std::process::id();
    let mut orphan = sample_meta(ProcessState::Running, temp.path());
    orphan.role = SessionRole::MillraceDaemon;
    orphan.worker_pid = Some(dead_worker_pid);
    orphan.child_pid = Some(live_child_pid);
    orphan.child_pgid = Some(live_child_pid);
    let mut worker = sample_worker(orphan.id, dead_worker_pid, ProcessState::Running);
    worker.child_pid = Some(live_child_pid);
    worker.child_pgid = Some(live_child_pid);
    worker.attached_clients = 1;
    worker.input_owner = Some("stale-owner".to_string());
    write_session(&paths, &orphan, Some(worker), true);

    let result = run_doctor(&paths, None, &DoctorRequest::default()).unwrap();

    let orphan_issue = issue_by_code(&result, "orphaned_child_process");
    assert_eq!(orphan_issue.severity, DoctorSeverity::Critical);
    let details = orphan_issue.details.as_ref().expect("orphan details");
    assert_eq!(details["worker_liveness"], "dead");
    assert_eq!(details["child_liveness"], "alive");
    let actions = details["recovery_actions"]
        .as_array()
        .expect("recovery actions");
    assert!(actions.iter().any(|action| action == "native_stop"));
    assert!(actions.iter().any(|action| action == "signal_child"));
    assert!(actions
        .iter()
        .any(|action| action == "archive_after_stopped"));
    assert!(orphan_issue
        .suggested_action
        .as_deref()
        .unwrap_or_default()
        .contains("worker is gone while the child remains alive"));

    assert_issue(&result, "worker_socket_missing", DoctorSeverity::Warning);
    assert_issue(
        &result,
        "attach_state_inconsistent",
        DoctorSeverity::Warning,
    );
}

#[test]
fn doctor_reports_stale_attach_state_after_terminal_session_end() {
    let temp = tempfile::tempdir().unwrap();
    let paths = prepared_state(temp.path());

    let terminal = sample_meta(ProcessState::Exited, temp.path());
    let mut worker = sample_worker(terminal.id, dead_process_pid(), ProcessState::Exited);
    worker.attached_clients = 2;
    worker.input_owner = Some("ended-owner".to_string());
    write_session(&paths, &terminal, Some(worker), true);

    let result = run_doctor(&paths, None, &DoctorRequest::default()).unwrap();

    let issue = issue_by_code(&result, "attach_state_inconsistent");
    assert_eq!(issue.severity, DoctorSeverity::Warning);
    let details = issue.details.as_ref().expect("attach issue details");
    assert_eq!(details["attached_clients"], 2);
    assert_eq!(details["input_owner"], "ended-owner");
    assert_eq!(details["session_process_state"], "exited");
    assert_eq!(details["worker_process_state"], "exited");
}

#[test]
fn doctor_checks_pipe_artifacts_without_requiring_pty_log() {
    let temp = tempfile::tempdir().unwrap();
    let paths = prepared_state(temp.path());
    let mut pipe_session = sample_meta(ProcessState::Exited, temp.path());
    pipe_session.spawn_mode = SpawnMode::Pipe;
    write_session(&paths, &pipe_session, None, false);
    let session_paths = paths.session_paths(pipe_session.id);
    append_raw_pty_log(&session_paths.pty_log, b"stray pty\n").unwrap();

    let result = run_doctor(&paths, None, &DoctorRequest::default()).unwrap();

    assert_issue(&result, "missing_stdout_log", DoctorSeverity::Warning);
    assert_issue(&result, "missing_stderr_log", DoctorSeverity::Warning);
    assert_issue(
        &result,
        "unexpected_pty_log_for_pipe",
        DoctorSeverity::Warning,
    );
    assert!(
        result
            .issues
            .iter()
            .all(|issue| issue.code != "missing_pty_log"),
        "{:#?}",
        result.issues
    );
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
fn doctor_screen_guidance_preserves_unsafe_agent_terminal_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let paths = prepared_state(temp.path());
    let mut stale = sample_meta(ProcessState::Lost, temp.path());
    stale.role = SessionRole::Agent;
    stale.argv = vec!["millrace-cli".to_string()];
    let session_paths = paths.session_paths(stale.id);

    create_private_dir_all(&session_paths.root).unwrap();
    write_json_atomic(&session_paths.meta_json, &stale).unwrap();
    append_raw_pty_log(&session_paths.pty_log, full_screen_agent_fixture()).unwrap();
    let mut output = SessionEvent::new(stale.id, SessionEventKind::Output);
    output.message = Some("\x1b[?2026hstreaming answer\x1b[?2026l".to_string());
    millrace_sessions_core::events::append_event(&session_paths.events_jsonl, &output).unwrap();
    let mut scrollback = ScrollbackBuffer::new(10);
    scrollback.push_line("\x1b[?1049h\x1b[2Jlegacy alternate frame");
    scrollback.push_line("\x1b[3J\x1b[Hstale answer");
    scrollback
        .persist_snapshot(&session_paths.scrollback_snapshot)
        .unwrap();

    let report = run_doctor(&paths, None, &DoctorRequest::default()).unwrap();
    let issue = issue_by_code(&report, "unsafe_legacy_line_scrollback");
    assert_eq!(issue.severity, DoctorSeverity::Warning);
    assert_eq!(issue.session_id, Some(stale.id));
    assert_eq!(
        issue.path.as_ref(),
        Some(&session_paths.scrollback_snapshot)
    );
    assert!(issue.repairable);
    let suggested_action = issue.suggested_action.as_deref().unwrap_or_default();
    assert!(suggested_action.contains("millmux screen"));
    assert!(suggested_action.contains("ARCHIVE_STALE"));
    assert!(suggested_action.contains("preserve pty.log"));
    assert!(suggested_action.contains("events.jsonl"));
    assert!(suggested_action.contains("terminal.snapshot.json"));
    assert!(suggested_action.contains("pty.replay"));
    assert!(session_paths.pty_log.exists());
    assert!(session_paths.events_jsonl.exists());

    run_doctor(
        &paths,
        None,
        &DoctorRequest {
            repair: Some(DoctorRepairMode::ArchiveStale),
        },
    )
    .unwrap();

    let archive_root = paths.archive_dir.join(stale.id.to_string());
    let archived_pty = fs::read(archive_root.join("pty.log")).unwrap();
    assert!(archived_pty.windows(4).any(|window| window == b"\x1b[2J"));
    assert!(archived_pty.windows(4).any(|window| window == b"\x1b[3J"));
    let archived_events = read_events(archive_root.join("events.jsonl")).unwrap();
    assert!(archived_events.iter().any(|event| event
        .message
        .as_deref()
        .is_some_and(|message| message.contains("\x1b[?2026h"))));
    let archived_scrollback =
        ScrollbackBuffer::restore_snapshot(archive_root.join("scrollback.snapshot")).unwrap();
    assert!(
        legacy_line_scrollback_contains_tui_sequences(&archived_scrollback.lines()),
        "unsafe legacy line scrollback remains detectable after archive repair"
    );
}

#[test]
fn doctor_screen_guidance_reports_attach_stream_lagged_events() {
    let temp = tempfile::tempdir().unwrap();
    let paths = prepared_state(temp.path());
    let session = sample_meta(ProcessState::Exited, temp.path());
    write_session(&paths, &session, None, true);
    let session_paths = paths.session_paths(session.id);

    let mut lagged = SessionEvent::new(session.id, SessionEventKind::AttachStreamLagged);
    lagged
        .fields
        .insert("stream_id".to_string(), "stream-slow".to_string());
    lagged
        .fields
        .insert("dropped_bytes".to_string(), "4096".to_string());
    lagged
        .fields
        .insert("dropped_from_offset".to_string(), "128".to_string());
    lagged
        .fields
        .insert("dropped_to_offset".to_string(), "4224".to_string());
    lagged
        .fields
        .insert("current_pty_log_offset".to_string(), "8192".to_string());
    lagged
        .fields
        .insert("reason".to_string(), "observer_backpressure".to_string());
    millrace_sessions_core::events::append_event(&session_paths.events_jsonl, &lagged).unwrap();

    let report = run_doctor(&paths, None, &DoctorRequest::default()).unwrap();
    let issue = issue_by_code(&report, "attach_stream_lagged");
    assert_eq!(issue.severity, DoctorSeverity::Warning);
    assert_eq!(issue.session_id, Some(session.id));
    assert_eq!(issue.path.as_ref(), Some(&session_paths.events_jsonl));
    assert!(issue
        .suggested_action
        .as_deref()
        .unwrap_or_default()
        .contains("millmux screen"));
    assert!(issue
        .suggested_action
        .as_deref()
        .unwrap_or_default()
        .contains("raw replay"));
    assert_eq!(
        issue
            .details
            .as_ref()
            .and_then(|details| details.get("lag_event_count")),
        Some(&json!(1))
    );
    assert_eq!(
        issue
            .details
            .as_ref()
            .and_then(|details| details.get("last_dropped_from_offset")),
        Some(&json!("128"))
    );
    assert_eq!(
        issue
            .details
            .as_ref()
            .and_then(|details| details.get("last_dropped_to_offset")),
        Some(&json!("4224"))
    );
    assert_eq!(
        issue
            .details
            .as_ref()
            .and_then(|details| details.get("reason")),
        Some(&json!("observer_backpressure"))
    );
}

#[test]
fn doctor_close_stale_ui_contexts_preserves_live_contexts_and_session_artifacts() {
    let temp = tempfile::tempdir().unwrap();
    let paths = prepared_state(temp.path());

    let terminal = sample_meta(ProcessState::Exited, temp.path());
    write_session(&paths, &terminal, None, true);

    let mut live = sample_meta(ProcessState::Running, temp.path());
    live.worker_pid = Some(std::process::id());
    let live_worker = sample_worker(live.id, std::process::id(), ProcessState::Running);
    write_session(&paths, &live, Some(live_worker), true);

    let stale_ui_id = UiId::new();
    seed_ui_context(
        &paths,
        stale_ui_id,
        Some(terminal.id),
        None,
        vec![terminal.id],
        "2000-01-01T00:00:00Z",
    );
    let live_ui_id = UiId::new();
    seed_ui_context(
        &paths,
        live_ui_id,
        Some(live.id),
        None,
        vec![live.id],
        "2000-01-01T00:00:00Z",
    );

    let report = run_doctor(&paths, None, &DoctorRequest::default()).unwrap();
    assert_issue(&report, "stale_ui_context", DoctorSeverity::Warning);
    assert_issue(
        &report,
        "ui_context_has_live_session_refs",
        DoctorSeverity::Info,
    );

    let result = run_doctor(
        &paths,
        None,
        &DoctorRequest {
            repair: Some(DoctorRepairMode::CloseStaleUiContexts),
        },
    )
    .unwrap();

    let stale_paths = paths.ui_context_paths(stale_ui_id);
    let live_paths = paths.ui_context_paths(live_ui_id);
    assert!(!stale_paths.context_json.exists());
    assert!(stale_paths.events_jsonl.exists());
    assert!(live_paths.context_json.exists());
    assert!(paths.session_paths(terminal.id).root.exists());
    assert!(paths.session_paths(live.id).root.exists());

    let repair = result
        .repairs
        .iter()
        .find(|repair| {
            repair.status == DoctorRepairStatus::Applied
                && repair
                    .details
                    .as_ref()
                    .and_then(|details| details.get("ui_id"))
                    .and_then(|value| value.as_str())
                    == Some(stale_ui_id.to_string().as_str())
        })
        .expect("stale UI context repair summary");
    assert_eq!(repair.mode, DoctorRepairMode::CloseStaleUiContexts);

    let events: Vec<UiEvent> = read_json_lines(&stale_paths.events_jsonl).unwrap();
    assert!(events
        .iter()
        .any(|event| event.message.as_deref() == Some("doctor closed stale UI context")));
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

#[test]
fn startup_reconciliation_splits_worker_and_child_liveness() {
    let temp = tempfile::tempdir().unwrap();
    let paths = prepared_state(temp.path());
    let live_pid = std::process::id();

    let mut both_alive = sample_meta(ProcessState::Running, temp.path());
    both_alive.worker_pid = Some(live_pid);
    both_alive.child_pid = Some(live_pid);
    let mut both_alive_worker = sample_worker(both_alive.id, live_pid, ProcessState::Running);
    both_alive_worker.child_pid = Some(live_pid);
    write_session(&paths, &both_alive, Some(both_alive_worker), true);

    let mut worker_dead_child_alive = sample_meta(ProcessState::Running, temp.path());
    worker_dead_child_alive.worker_pid = Some(dead_process_pid());
    worker_dead_child_alive.child_pid = Some(live_pid);
    let mut orphan_worker = sample_worker(
        worker_dead_child_alive.id,
        worker_dead_child_alive.worker_pid.unwrap(),
        ProcessState::Running,
    );
    orphan_worker.child_pid = Some(live_pid);
    orphan_worker.attached_clients = 1;
    orphan_worker.input_owner = Some("stale-owner".to_string());
    write_session(&paths, &worker_dead_child_alive, Some(orphan_worker), true);

    let mut worker_alive_child_dead = sample_meta(ProcessState::Running, temp.path());
    worker_alive_child_dead.worker_pid = Some(live_pid);
    worker_alive_child_dead.child_pid = Some(dead_process_pid());
    let mut stale_child_worker =
        sample_worker(worker_alive_child_dead.id, live_pid, ProcessState::Running);
    stale_child_worker.child_pid = worker_alive_child_dead.child_pid;
    write_session(
        &paths,
        &worker_alive_child_dead,
        Some(stale_child_worker),
        true,
    );

    let mut both_dead = sample_meta(ProcessState::Running, temp.path());
    both_dead.worker_pid = Some(dead_process_pid());
    both_dead.child_pid = Some(dead_process_pid());
    let mut both_dead_worker = sample_worker(
        both_dead.id,
        both_dead.worker_pid.unwrap(),
        ProcessState::Running,
    );
    both_dead_worker.child_pid = both_dead.child_pid;
    write_session(&paths, &both_dead, Some(both_dead_worker), true);

    let summary = reconcile_startup(&paths).unwrap();

    assert_eq!(summary.scanned, 4);
    assert_eq!(summary.preserved, 1);
    assert_eq!(summary.marked_terminal, 3);

    let both_alive_after: SessionMeta =
        read_json(&paths.session_paths(both_alive.id).meta_json).unwrap();
    let orphan_after: SessionMeta =
        read_json(&paths.session_paths(worker_dead_child_alive.id).meta_json).unwrap();
    let stale_child_after: SessionMeta =
        read_json(&paths.session_paths(worker_alive_child_dead.id).meta_json).unwrap();
    let both_dead_after: SessionMeta =
        read_json(&paths.session_paths(both_dead.id).meta_json).unwrap();

    assert_eq!(both_alive_after.process_state, ProcessState::Running);
    assert_eq!(orphan_after.process_state, ProcessState::Orphaned);
    assert_eq!(
        orphan_after.failure_message.as_deref(),
        Some("startup reconciliation found a live child without a live worker")
    );
    assert_eq!(stale_child_after.process_state, ProcessState::Stale);
    assert_eq!(both_dead_after.process_state, ProcessState::Lost);

    let orphan_worker_after: WorkerMeta =
        read_json(&paths.session_paths(worker_dead_child_alive.id).worker_json).unwrap();
    assert_eq!(orphan_worker_after.attached_clients, 0);
    assert_eq!(orphan_worker_after.input_owner, None);

    let events = read_events(paths.session_paths(worker_dead_child_alive.id).events_jsonl).unwrap();
    assert!(events.iter().any(|event| {
        event.kind == SessionEventKind::StateChanged
            && event.process_state == Some(ProcessState::Orphaned)
            && event.fields.get("reason").map(String::as_str) == Some("startup_reconcile")
            && event.fields.get("liveness_reason").map(String::as_str) == Some("orphaned_child")
            && event.fields.get("worker_liveness").map(String::as_str) == Some("dead")
            && event.fields.get("child_liveness").map(String::as_str) == Some("alive")
    }));
}

#[test]
fn doctor_reports_reconciled_orphaned_child_process() {
    let temp = tempfile::tempdir().unwrap();
    let paths = prepared_state(temp.path());
    let live_child_pid = std::process::id();

    let mut orphan = sample_meta(ProcessState::Running, temp.path());
    orphan.role = SessionRole::MillraceDaemon;
    orphan.worker_pid = Some(dead_process_pid());
    orphan.child_pid = Some(live_child_pid);
    orphan.child_pgid = Some(live_child_pid);
    let mut worker = sample_worker(orphan.id, orphan.worker_pid.unwrap(), ProcessState::Running);
    worker.child_pid = Some(live_child_pid);
    worker.child_pgid = Some(live_child_pid);
    write_session(&paths, &orphan, Some(worker), true);

    let summary = reconcile_startup(&paths).unwrap();
    assert_eq!(summary.marked_terminal, 1);
    let reconciled: SessionMeta = read_json(&paths.session_paths(orphan.id).meta_json).unwrap();
    assert_eq!(reconciled.process_state, ProcessState::Orphaned);

    let result = run_doctor(&paths, None, &DoctorRequest::default()).unwrap();
    let orphan_issue = issue_by_code(&result, "orphaned_child_process");
    assert_eq!(orphan_issue.severity, DoctorSeverity::Critical);
    let details = orphan_issue.details.as_ref().expect("orphan details");
    assert_eq!(details["worker_liveness"], "dead");
    assert_eq!(details["child_liveness"], "alive");
    let actions = details["recovery_actions"]
        .as_array()
        .expect("recovery actions");
    assert!(actions.iter().any(|action| action == "native_stop"));
    assert!(actions.iter().any(|action| action == "signal_child"));
    assert!(actions
        .iter()
        .any(|action| action == "archive_after_stopped"));

    let repaired = run_doctor(
        &paths,
        None,
        &DoctorRequest {
            repair: Some(DoctorRepairMode::ArchiveStale),
        },
    )
    .unwrap();
    assert!(paths.session_paths(orphan.id).root.exists());
    assert!(!paths.archive_dir.join(orphan.id.to_string()).exists());
    assert!(!repaired.repairs.iter().any(|repair| {
        repair.session_id == Some(orphan.id) && repair.status == DoctorRepairStatus::Applied
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

fn full_screen_agent_fixture() -> &'static [u8] {
    concat!(
        "fixture-agent ready\r\n",
        "\x1b[?1049h",
        "\x1b[?2026h",
        "\x1b[2J",
        "\x1b[3J",
        "\x1b[H",
        "question one\r\n",
        "\x1b[4;9Hanswer one complete\r\n",
        "\x1b[2Kanswer two chunk 3\r\n",
        "\x1b[?2026l",
        "\x1b[?1049l",
    )
    .as_bytes()
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

fn seed_ui_context(
    paths: &StatePaths,
    ui_id: UiId,
    active_daemon_session_id: Option<SessionId>,
    agent_session_id: Option<SessionId>,
    managed_daemon_session_ids: Vec<SessionId>,
    updated_at: &str,
) {
    let ui_paths = paths.ui_context_paths(ui_id);
    write_json_atomic(
        &ui_paths.context_json,
        &serde_json::json!({
            "schema_version": 1,
            "ui_id": ui_id,
            "mode": "daemon_console",
            "active_pane_id": null,
            "active_daemon_session_id": active_daemon_session_id,
            "active_workspace": null,
            "agent_session_id": agent_session_id,
            "managed_daemon_session_ids": managed_daemon_session_ids,
            "monitor_profile": "auto",
            "updated_at": updated_at
        }),
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
        attention_items: Vec::new(),
        status_summary: None,
        workspace: None,
        cwd: root.to_path_buf(),
        argv: vec!["sh".to_string()],
        spawn_mode: SpawnMode::Pty,
        monitor_profile: MonitorProfile::Auto,
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

fn sample_worker(session_id: SessionId, pid: u32, state: ProcessState) -> WorkerMeta {
    WorkerMeta {
        session_id,
        pid,
        child_pid: None,
        child_pgid: None,
        spawn_mode: SpawnMode::Pty,
        process_state: state,
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
    let issue = issue_by_code(result, code);
    assert_eq!(issue.severity, severity);
}

fn issue_by_code<'a>(
    result: &'a DoctorResponse,
    code: &str,
) -> &'a millrace_sessions_core::protocol::DoctorIssue {
    result
        .issues
        .iter()
        .find(|issue| issue.code == code)
        .unwrap_or_else(|| panic!("missing issue {code}; got {:#?}", result.issues))
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
