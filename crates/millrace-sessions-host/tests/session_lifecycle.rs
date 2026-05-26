use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::Once,
    thread,
    time::{Duration, Instant},
};

use millrace_sessions_core::{
    events::{read_events, SessionEventKind},
    paths::{StatePaths, STATE_DIR_ENV},
    protocol::{
        AttachStreamFrame, ControlErrorCode, ControlResponse, SessionAttachResponse,
        SessionDeleteResponse, SessionEventsResponse, SessionKillResponse, SessionLogsResponse,
        SessionResizeResponse, SessionSendResponse, SessionStartResponse, SessionStopResponse,
    },
    scrollback::{restore_terminal_replay, TerminalSnapshot, TerminalStateBuffer},
    state::{ProcessState, SessionMeta, WorkerMeta},
    storage::{append_raw_pty_log, read_json},
};
use serde_json::{json, Value};

#[test]
fn start_persists_worker_output_and_lifecycle_state() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let response = request_json(
        &paths,
        json!({
            "id": "start-1",
            "method": "session.start",
            "params": {
                "name": "ready",
                "role": "shell",
                "workspace": workspace,
                "cwd": workspace,
                "argv": ["sh", "-c", "printf ready; sleep 0.1"]
            }
        }),
    );
    assert_eq!(response["ok"], true, "{response:#}");
    let start: SessionStartResponse =
        serde_json::from_value(response["result"].clone()).expect("start result");
    assert!(!start.attached_existing);
    assert_eq!(start.session.process_state, ProcessState::Starting);

    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_file_contains(&session_paths.pty_log, "ready");
    wait_for_terminal_or_running_meta(&session_paths.meta_json);
    wait_for_event_kind(&session_paths.events_jsonl, SessionEventKind::Output);

    let raw_log = fs::read_to_string(&session_paths.pty_log).unwrap();
    assert!(raw_log.contains("ready"));
    let worker: WorkerMeta = read_json(&session_paths.worker_json).unwrap();
    assert_eq!(worker.session_id, start.session.session_id);
    assert!(worker.child_pid.is_some());

    let meta: SessionMeta = read_json(&session_paths.meta_json).unwrap();
    assert!(matches!(
        meta.process_state,
        ProcessState::Running | ProcessState::Exited
    ));
    assert_eq!(meta.worker_pid, Some(worker.pid));
    assert!(meta.child_pid.is_some());

    let events = read_events(&session_paths.events_jsonl).unwrap();
    assert!(events
        .iter()
        .any(|event| event.kind == SessionEventKind::SessionCreated));
    assert!(events
        .iter()
        .any(|event| event.kind == SessionEventKind::WorkerStarted));
    assert!(events
        .iter()
        .any(|event| event.kind == SessionEventKind::Output));
    wait_for_file_contains(&session_paths.scrollback_snapshot, "ready");
    let scrollback = fs::read_to_string(&session_paths.scrollback_snapshot).unwrap();
    assert!(scrollback.contains("ready"));
    wait_for_file_contains(&session_paths.terminal_snapshot, "ready");
    assert!(session_paths.raw_replay_ring.exists());
    assert!(fs::read(&session_paths.raw_replay_ring)
        .unwrap()
        .ends_with(b"ready"));

    daemon.kill();
}

#[cfg(unix)]
#[test]
fn session_artifacts_are_private() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_session(
        &paths,
        &workspace,
        "private-artifacts",
        "printf 'ready\\n'; sleep 0.1",
    );
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_file_contains(&session_paths.pty_log, "ready");
    wait_for_file_contains(&session_paths.worker_json, "\"session_id\"");
    wait_for_file_contains(&session_paths.events_jsonl, "\"output\"");
    wait_for_file_contains(&session_paths.scrollback_snapshot, "ready");
    wait_for_file_contains(&session_paths.terminal_snapshot, "ready");

    assert_private_dir(&session_paths.root);
    assert_private_file(&session_paths.meta_json);
    assert_private_file(&session_paths.worker_json);
    assert_private_file(&session_paths.events_jsonl);
    assert_private_file(&session_paths.pty_log);
    assert_private_file(&session_paths.scrollback_snapshot);
    assert_private_file(&session_paths.terminal_snapshot);
    assert_private_file(&session_paths.raw_replay_ring);

    daemon.kill();
}

#[test]
fn start_returns_existing_active_daemon_for_duplicate_workspace_role() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let first = request_json(
        &paths,
        json!({
            "id": "daemon-1",
            "method": "session.start",
            "params": {
                "name": "daemon",
                "role": "millrace_daemon",
                "workspace": workspace,
                "cwd": workspace,
                "argv": ["sh", "-c", "sleep 1"]
            }
        }),
    );
    assert_eq!(first["ok"], true, "{first:#}");
    let first_start: SessionStartResponse =
        serde_json::from_value(first["result"].clone()).expect("first start result");

    let second = request_json(
        &paths,
        json!({
            "id": "daemon-2",
            "method": "session.start",
            "params": {
                "name": "daemon-again",
                "role": "millrace_daemon",
                "workspace": workspace,
                "cwd": workspace,
                "argv": ["sh", "-c", "sleep 1"]
            }
        }),
    );
    assert_eq!(second["ok"], true, "{second:#}");
    let second_start: SessionStartResponse =
        serde_json::from_value(second["result"].clone()).expect("second start result");

    assert!(second_start.attached_existing);
    assert_eq!(
        second_start.session.session_id,
        first_start.session.session_id
    );

    daemon.kill();
}

#[test]
fn daemon_command_resolution_failure_persists_failed_start_summary() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    let empty_bin = temp.path().join("empty-bin");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&empty_bin).unwrap();
    let path_env = std::env::join_paths([empty_bin]).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let response = request_json(
        &paths,
        json!({
            "id": "daemon-missing-command",
            "method": "session.start",
            "params": {
                "name": "daemon-missing-command",
                "role": "millrace_daemon",
                "workspace": workspace,
                "cwd": workspace,
                "argv": ["millrace", "run", "daemon", "--workspace", workspace],
                "env": {"PATH": path_env.to_string_lossy()}
            }
        }),
    );
    assert_eq!(response["ok"], true, "{response:#}");
    let start: SessionStartResponse =
        serde_json::from_value(response["result"].clone()).expect("start result");
    let session_paths = paths.session_paths(start.session.session_id);
    let meta = wait_for_meta_state(&session_paths.meta_json, ProcessState::FailedStart);

    let failure = meta.failure_message.expect("failed_start message");
    assert!(failure.contains("failed to spawn pty child"), "{failure}");

    let listed = request_json(
        &paths,
        json!({
            "id": "list-failed-daemon",
            "method": "session.list",
            "params": {
                "role": "millrace_daemon",
                "workspace": workspace
            }
        }),
    );
    assert_eq!(listed["ok"], true, "{listed:#}");
    assert_eq!(
        listed["result"]["sessions"][0]["process_state"], "failed_start",
        "{listed:#}"
    );
    assert!(
        listed["result"]["sessions"][0]["failure_message"]
            .as_str()
            .is_some_and(|message| message.contains("failed to spawn pty child")),
        "{listed:#}"
    );

    daemon.kill();
}

#[test]
fn millrace_status_probe_redacts_secret_stderr() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let path_env = fake_millrace_path(
        temp.path(),
        r#"if [ "$1" = "status" ]; then
  printf 'MILLRACE_TOKEN=super-secret NORMAL=value\n' >&2
  exit 7
fi
exit 0
"#,
    );
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let response = request_json(
        &paths,
        json!({
            "id": "daemon-status-secret",
            "method": "session.start",
            "params": {
                "name": "daemon-status-secret",
                "role": "millrace_daemon",
                "workspace": workspace,
                "cwd": workspace,
                "argv": ["sh", "-c", "sleep 1"],
                "env": {"PATH": path_env}
            }
        }),
    );
    assert_eq!(response["ok"], true, "{response:#}");
    let start: SessionStartResponse =
        serde_json::from_value(response["result"].clone()).expect("start result");
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_event_kind(
        &session_paths.events_jsonl,
        SessionEventKind::MillraceStatusProbe,
    );

    let events = read_events(&session_paths.events_jsonl).unwrap();
    let stderr = events
        .iter()
        .find(|event| event.kind == SessionEventKind::MillraceStatusProbe)
        .and_then(|event| event.fields.get("stderr"))
        .expect("status probe stderr");
    assert!(stderr.contains("MILLRACE_TOKEN=<redacted>"), "{stderr}");
    assert!(stderr.contains("NORMAL=value"), "{stderr}");
    assert!(!stderr.contains("super-secret"), "{stderr}");

    daemon.kill();
}

#[test]
fn start_rejects_empty_argv_and_missing_cwd() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let empty = request_json(
        &paths,
        json!({
            "id": "empty",
            "method": "session.start",
            "params": {
                "cwd": temp.path(),
                "argv": []
            }
        }),
    );
    assert_error(&empty, "empty", ControlErrorCode::InvalidRequest);

    let missing_cwd = request_json(
        &paths,
        json!({
            "id": "missing-cwd",
            "method": "session.start",
            "params": {
                "cwd": temp.path().join("missing"),
                "argv": ["sh"]
            }
        }),
    );
    assert_error(
        &missing_cwd,
        "missing-cwd",
        ControlErrorCode::WorkspaceNotFound,
    );

    daemon.kill();
}

#[test]
fn send_forwards_input_and_records_input_event() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_session(
        &paths,
        &workspace,
        "send-shell",
        "printf 'ready\\n'; while IFS= read -r line; do printf 'got:%s\\n' \"$line\"; done",
    );
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_worker_socket(&session_paths.worker_sock);
    wait_for_running_meta(&session_paths.meta_json);
    wait_for_file_contains(&session_paths.pty_log, "ready");

    let response = request_json(
        &paths,
        json!({
            "id": "send-1",
            "method": "session.send",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id},
                "text": "hello\n"
            }
        }),
    );
    assert_eq!(response["ok"], true, "{response:#}");
    let send: SessionSendResponse =
        serde_json::from_value(response["result"].clone()).expect("send result");
    assert_eq!(send.bytes_sent, "hello\n".len());
    wait_for_file_contains(&session_paths.pty_log, "got:hello");
    wait_for_event_kind(&session_paths.events_jsonl, SessionEventKind::InputSent);

    daemon.kill();
}

#[test]
fn logs_reads_tail_from_host_artifact() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_session(
        &paths,
        &workspace,
        "logs-shell",
        "printf 'one\\n'; printf 'two\\n'; printf 'three\\n'; sleep 0.2",
    );
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_file_contains(&session_paths.pty_log, "three");

    let response = request_json(
        &paths,
        json!({
            "id": "logs-1",
            "method": "session.logs",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id},
                "tail": 2
            }
        }),
    );
    assert_eq!(response["ok"], true, "{response:#}");
    let logs: SessionLogsResponse =
        serde_json::from_value(response["result"].clone()).expect("logs result");
    let lines = logs
        .lines
        .iter()
        .map(|line| line.line.as_str())
        .collect::<Vec<_>>();
    assert_eq!(lines, ["two", "three"]);

    daemon.kill();
}

#[test]
fn events_reads_structured_session_events() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_session(
        &paths,
        &workspace,
        "events-shell",
        "printf 'event-ready\\n'; sleep 0.2",
    );
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_event_kind(&session_paths.events_jsonl, SessionEventKind::Output);

    let response = request_json(
        &paths,
        json!({
            "id": "events-1",
            "method": "session.events",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id}
            }
        }),
    );
    assert_eq!(response["ok"], true, "{response:#}");
    let events: SessionEventsResponse =
        serde_json::from_value(response["result"].clone()).expect("events result");
    assert!(events
        .events
        .iter()
        .any(|event| event.kind == SessionEventKind::Output));

    daemon.kill();
}

#[test]
fn resize_forwards_dimensions_and_records_event() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_session(
        &paths,
        &workspace,
        "resize-shell",
        "printf 'ready\\n'; sleep 2",
    );
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_worker_socket(&session_paths.worker_sock);
    wait_for_running_meta(&session_paths.meta_json);

    let response = request_json(
        &paths,
        json!({
            "id": "resize-1",
            "method": "session.resize",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id},
                "rows": 31,
                "cols": 99
            }
        }),
    );
    assert_eq!(response["ok"], true, "{response:#}");
    let resize: SessionResizeResponse =
        serde_json::from_value(response["result"].clone()).expect("resize result");
    assert_eq!((resize.rows, resize.cols), (31, 99));
    wait_for_event_kind(&session_paths.events_jsonl, SessionEventKind::Resize);
    wait_for_terminal_snapshot_size(&session_paths.terminal_snapshot, 31, 99);

    daemon.kill();
}

#[test]
fn stop_requests_graceful_shutdown_without_marking_session_killed() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_session(
        &paths,
        &workspace,
        "stop-shell",
        "trap 'printf stopped\\n; exit 0' INT TERM; printf 'ready\\n'; while true; do sleep 1; done",
    );
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_worker_socket(&session_paths.worker_sock);
    wait_for_running_meta(&session_paths.meta_json);
    wait_for_file_contains(&session_paths.pty_log, "ready");

    let response = request_json(
        &paths,
        json!({
            "id": "stop-1",
            "method": "session.stop",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id},
                "grace_seconds": 0
            }
        }),
    );
    assert_eq!(response["ok"], true, "{response:#}");
    let stop: SessionStopResponse =
        serde_json::from_value(response["result"].clone()).expect("stop result");
    assert!(stop.stop_requested);
    assert_ne!(stop.process_state, ProcessState::Killed);
    wait_for_event_kind(&session_paths.events_jsonl, SessionEventKind::StopRequested);

    let meta = wait_for_terminal_meta(&session_paths.meta_json);
    assert_ne!(meta.process_state, ProcessState::Killed);
    let events = read_events(&session_paths.events_jsonl).unwrap();
    assert!(!events
        .iter()
        .any(|event| event.kind == SessionEventKind::KillRequested));

    daemon.kill();
}

#[test]
fn kill_requests_forceful_shutdown_and_persists_killed_state() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_session(
        &paths,
        &workspace,
        "kill-shell",
        "printf 'ready\\n'; while true; do sleep 1; done",
    );
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_worker_socket(&session_paths.worker_sock);
    wait_for_running_meta(&session_paths.meta_json);
    wait_for_file_contains(&session_paths.pty_log, "ready");

    let response = request_json(
        &paths,
        json!({
            "id": "kill-1",
            "method": "session.kill",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id}
            }
        }),
    );
    assert_eq!(response["ok"], true, "{response:#}");
    let kill: SessionKillResponse =
        serde_json::from_value(response["result"].clone()).expect("kill result");
    assert!(kill.kill_requested);
    assert_eq!(kill.process_state, ProcessState::Killed);

    wait_for_event_kind(&session_paths.events_jsonl, SessionEventKind::KillRequested);
    let meta = wait_for_meta_state(&session_paths.meta_json, ProcessState::Killed);
    assert_eq!(meta.process_state, ProcessState::Killed);

    daemon.kill();
}

#[test]
fn delete_refuses_running_archives_stopped_and_purges_explicitly() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let running = start_session(
        &paths,
        &workspace,
        "delete-running",
        "printf 'ready\\n'; while true; do sleep 1; done",
    );
    let running_paths = paths.session_paths(running.session.session_id);
    wait_for_running_meta(&running_paths.meta_json);
    let refused = request_json(
        &paths,
        json!({
            "id": "delete-running",
            "method": "session.delete",
            "params": {
                "selector": {"type": "id", "session_id": running.session.session_id}
            }
        }),
    );
    assert_error(
        &refused,
        "delete-running",
        ControlErrorCode::UnsafeDeleteRunning,
    );
    assert!(running_paths.root.exists());

    let stopped = start_session(
        &paths,
        &workspace,
        "delete-stopped",
        "printf 'done\\n'; sleep 0.1",
    );
    let stopped_paths = paths.session_paths(stopped.session.session_id);
    wait_for_file_contains(&stopped_paths.pty_log, "done");
    let _ = wait_for_terminal_meta(&stopped_paths.meta_json);

    let deleted = request_json(
        &paths,
        json!({
            "id": "delete-stopped",
            "method": "session.delete",
            "params": {
                "selector": {"type": "id", "session_id": stopped.session.session_id}
            }
        }),
    );
    assert_eq!(deleted["ok"], true, "{deleted:#}");
    let delete: SessionDeleteResponse =
        serde_json::from_value(deleted["result"].clone()).expect("delete result");
    assert!(delete.deleted);
    assert!(delete.archived);
    assert!(!delete.purged);
    assert!(!stopped_paths.root.exists());
    let archive_root = paths
        .archive_dir
        .join(stopped.session.session_id.to_string());
    assert!(archive_root.join("meta.json").exists());
    assert!(archive_root.join("events.jsonl").exists());
    assert!(archive_root.join("pty.log").exists());
    assert!(archive_root.join("worker.json").exists());
    assert!(archive_root.join("scrollback.snapshot").exists());
    assert!(archive_root.join("terminal.snapshot.json").exists());
    assert!(archive_root.join("pty.replay").exists());

    let active_list = request_json(
        &paths,
        json!({"id": "list-active", "method": "session.list", "params": {}}),
    );
    assert_eq!(active_list["ok"], true, "{active_list:#}");
    assert!(!active_list["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|session| session["session_id"] == stopped.session.session_id.to_string()));

    let archived_list = request_json(
        &paths,
        json!({"id": "list-archived", "method": "session.list", "params": {"include_archived": true}}),
    );
    assert_eq!(archived_list["ok"], true, "{archived_list:#}");
    assert!(archived_list["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|session| session["session_id"] == stopped.session.session_id.to_string()));

    let purged = request_json(
        &paths,
        json!({
            "id": "purge-archived",
            "method": "session.delete",
            "params": {
                "selector": {"type": "id", "session_id": stopped.session.session_id},
                "purge": true
            }
        }),
    );
    assert_eq!(purged["ok"], true, "{purged:#}");
    let purge: SessionDeleteResponse =
        serde_json::from_value(purged["result"].clone()).expect("purge result");
    assert!(purge.deleted);
    assert!(purge.purged);
    assert!(!archive_root.exists());

    let _ = request_json(
        &paths,
        json!({
            "id": "cleanup-running",
            "method": "session.kill",
            "params": {
                "selector": {"type": "id", "session_id": running.session.session_id}
            }
        }),
    );
    daemon.kill();
}

#[test]
fn attach_streams_scrollback_releases_input_and_leaves_session_running() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_session(
        &paths,
        &workspace,
        "attach-shell",
        "printf 'ready\\n'; sleep 5",
    );
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_worker_socket(&session_paths.worker_sock);
    wait_for_running_meta(&session_paths.meta_json);
    wait_for_file_contains(&session_paths.scrollback_snapshot, "ready");

    let mut stream = UnixStream::connect(&paths.control_sock).expect("connect attach stream");
    stream
        .write_all(
            format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "id": "attach-1",
                    "method": "session.attach",
                    "params": {
                        "selector": {"type": "id", "session_id": start.session.session_id},
                        "replay": "line_scrollback"
                    }
                }))
                .unwrap()
            )
            .as_bytes(),
        )
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut response_line = String::new();
    reader.read_line(&mut response_line).unwrap();
    let response: ControlResponse = serde_json::from_str(response_line.trim_end()).unwrap();
    assert!(response.ok, "{response:#?}");
    let attach: SessionAttachResponse = response.result_as().unwrap();
    assert!(attach.stream.input_owner);

    let conflict = request_json(
        &paths,
        json!({
            "id": "attach-conflict",
            "method": "session.send",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id},
                "text": "blocked\n"
            }
        }),
    );
    assert_error(
        &conflict,
        "attach-conflict",
        ControlErrorCode::InputOwnerConflict,
    );

    let mut frame_line = String::new();
    reader.read_line(&mut frame_line).unwrap();
    let frame = AttachStreamFrame::from_json_line(&frame_line).unwrap();
    assert!(
        matches!(frame, AttachStreamFrame::Scrollback { lines } if lines.iter().any(|line| line.contains("ready")))
    );

    stream
        .write_all(AttachStreamFrame::Close.to_json_line().unwrap().as_bytes())
        .unwrap();
    wait_for_event_kind(&session_paths.events_jsonl, SessionEventKind::AttachClosed);
    wait_for_running_meta(&session_paths.meta_json);

    let after_close = request_json(
        &paths,
        json!({
            "id": "send-after-attach",
            "method": "session.send",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id},
                "text": "after\n"
            }
        }),
    );
    assert_eq!(after_close["ok"], true, "{after_close:#}");

    daemon.kill();
}

#[test]
fn attach_terminal_snapshot_replays_raw_bytes_without_text_conversion() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_session(&paths, &workspace, "raw-attach-shell", "sleep 5");
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_worker_socket(&session_paths.worker_sock);
    wait_for_running_meta(&session_paths.meta_json);
    let raw = b"\x1b[?1049h\xffraw\r\n".to_vec();
    persist_terminal_replay_fixture(&session_paths, &raw, raw.len() as u64);

    let mut stream = UnixStream::connect(&paths.control_sock).expect("connect attach stream");
    stream
        .write_all(
            format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "id": "attach-raw",
                    "method": "session.attach",
                    "params": {
                        "selector": {"type": "id", "session_id": start.session.session_id},
                        "replay": "terminal_snapshot",
                        "requested_terminal_size": {"rows": 24, "cols": 80}
                    }
                }))
                .unwrap()
            )
            .as_bytes(),
        )
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut response_line = String::new();
    reader.read_line(&mut response_line).unwrap();
    let response: ControlResponse = serde_json::from_str(response_line.trim_end()).unwrap();
    assert!(response.ok, "{response:#?}");

    let mut frame_line = String::new();
    reader.read_line(&mut frame_line).unwrap();
    assert!(
        !frame_line.contains('\u{fffd}'),
        "raw replay must not pass through UTF-8 replacement: {frame_line:?}"
    );
    let frame = AttachStreamFrame::from_json_line(&frame_line).unwrap();
    assert!(
        matches!(frame, AttachStreamFrame::RawOutput { data } if data.as_slice() == raw.as_slice())
    );

    stream
        .write_all(AttachStreamFrame::Close.to_json_line().unwrap().as_bytes())
        .unwrap();
    wait_for_event_kind(&session_paths.events_jsonl, SessionEventKind::AttachClosed);
    daemon.kill();
}

#[test]
fn attach_terminal_snapshot_skips_mismatched_size_raw_replay() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_session(&paths, &workspace, "mismatch-raw-attach-shell", "sleep 5");
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_worker_socket(&session_paths.worker_sock);
    wait_for_running_meta(&session_paths.meta_json);
    let raw = b"\x1b[?1049hsize-sensitive raw\r\n".to_vec();
    persist_terminal_replay_fixture(&session_paths, &raw, raw.len() as u64);

    let mut stream = UnixStream::connect(&paths.control_sock).expect("connect attach stream");
    stream
        .write_all(
            format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "id": "attach-size-mismatch",
                    "method": "session.attach",
                    "params": {
                        "selector": {"type": "id", "session_id": start.session.session_id},
                        "replay": "terminal_snapshot",
                        "requested_terminal_size": {"rows": 30, "cols": 100}
                    }
                }))
                .unwrap()
            )
            .as_bytes(),
        )
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut response_line = String::new();
    reader.read_line(&mut response_line).unwrap();
    let response: ControlResponse = serde_json::from_str(response_line.trim_end()).unwrap();
    assert!(response.ok, "{response:#?}");

    stream
        .write_all(AttachStreamFrame::Close.to_json_line().unwrap().as_bytes())
        .unwrap();
    let mut frame_line = String::new();
    reader.read_line(&mut frame_line).unwrap();
    let frame = AttachStreamFrame::from_json_line(&frame_line).unwrap();
    assert_eq!(frame, AttachStreamFrame::Closed);

    daemon.kill();
}

#[test]
fn attach_terminal_snapshot_skips_stale_raw_replay() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_session(&paths, &workspace, "stale-raw-attach-shell", "sleep 5");
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_worker_socket(&session_paths.worker_sock);
    wait_for_running_meta(&session_paths.meta_json);
    let raw = b"\x1b[?1049hstale raw\r\n".to_vec();
    persist_terminal_replay_fixture(&session_paths, &raw, raw.len() as u64);
    append_raw_pty_log(&session_paths.pty_log, b"newer").unwrap();
    assert!(
        restore_terminal_replay(
            &session_paths.terminal_snapshot,
            &session_paths.raw_replay_ring,
            raw.len() as u64 + 5
        )
        .unwrap()
        .is_none(),
        "fixture should be stale before attach"
    );

    let mut stream = UnixStream::connect(&paths.control_sock).expect("connect attach stream");
    stream
        .write_all(
            format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "id": "attach-stale-raw",
                    "method": "session.attach",
                    "params": {
                        "selector": {"type": "id", "session_id": start.session.session_id},
                        "replay": "terminal_snapshot",
                        "requested_terminal_size": {"rows": 24, "cols": 80}
                    }
                }))
                .unwrap()
            )
            .as_bytes(),
        )
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut response_line = String::new();
    reader.read_line(&mut response_line).unwrap();
    let response: ControlResponse = serde_json::from_str(response_line.trim_end()).unwrap();
    assert!(response.ok, "{response:#?}");

    stream
        .write_all(AttachStreamFrame::Close.to_json_line().unwrap().as_bytes())
        .unwrap();
    let mut frame_line = String::new();
    reader.read_line(&mut frame_line).unwrap();
    let frame = AttachStreamFrame::from_json_line(&frame_line).unwrap();
    assert_eq!(frame, AttachStreamFrame::Closed);

    daemon.kill();
}

#[test]
fn attach_drop_without_close_releases_input_and_records_closed() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_session(
        &paths,
        &workspace,
        "attach-drop-shell",
        "printf 'ready\\n'; while IFS= read -r line; do printf 'got:%s\\n' \"$line\"; done",
    );
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_worker_socket(&session_paths.worker_sock);
    wait_for_running_meta(&session_paths.meta_json);
    wait_for_file_contains(&session_paths.scrollback_snapshot, "ready");

    let mut stream = UnixStream::connect(&paths.control_sock).expect("connect attach stream");
    stream
        .write_all(
            format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "id": "attach-drop",
                    "method": "session.attach",
                    "params": {
                        "selector": {"type": "id", "session_id": start.session.session_id},
                        "include_scrollback": true
                    }
                }))
                .unwrap()
            )
            .as_bytes(),
        )
        .unwrap();
    let _ = stream.shutdown(Shutdown::Both);
    drop(stream);

    wait_for_event_kind(&session_paths.events_jsonl, SessionEventKind::AttachOpened);
    wait_for_event_kind(&session_paths.events_jsonl, SessionEventKind::AttachClosed);

    let after_drop = request_json(
        &paths,
        json!({
            "id": "send-after-drop",
            "method": "session.send",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id},
                "text": "after-drop\n"
            }
        }),
    );
    assert_eq!(after_drop["ok"], true, "{after_drop:#}");
    wait_for_file_contains(&session_paths.pty_log, "got:after-drop");

    daemon.kill();
}

#[test]
fn attach_read_only_observer_does_not_steal_input_owner() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_session(
        &paths,
        &workspace,
        "attach-owner-shell",
        "printf 'ready\\n'; while IFS= read -r line; do printf 'got:%s\\n' \"$line\"; done",
    );
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_worker_socket(&session_paths.worker_sock);
    wait_for_running_meta(&session_paths.meta_json);
    wait_for_file_contains(&session_paths.pty_log, "ready");

    let mut owner = open_attach_stream(&paths, start.session.session_id, false);
    let owner_response = read_attach_response(&owner);
    assert!(owner_response.stream.input_owner);
    assert!(!owner_response.stream.read_only);
    let owner_stream_id = owner_response.stream.stream_id.clone();
    let listed_owner = listed_session(&paths, start.session.session_id);
    assert_eq!(listed_owner["attached_clients"], 1, "{listed_owner:#}");
    assert_eq!(
        listed_owner["input_owner"], owner_stream_id,
        "{listed_owner:#}"
    );
    let inspected_owner = inspected_session_summary(&paths, start.session.session_id);
    assert_eq!(
        inspected_owner["attached_clients"], 1,
        "{inspected_owner:#}"
    );
    assert_eq!(
        inspected_owner["input_owner"], owner_stream_id,
        "{inspected_owner:#}"
    );

    let mut observer = open_attach_stream(&paths, start.session.session_id, true);
    let observer_response = read_attach_response(&observer);
    assert!(!observer_response.stream.input_owner);
    assert!(observer_response.stream.read_only);
    let listed_observer = listed_session(&paths, start.session.session_id);
    assert_eq!(
        listed_observer["attached_clients"], 2,
        "{listed_observer:#}"
    );
    assert_eq!(
        listed_observer["input_owner"], owner_stream_id,
        "{listed_observer:#}"
    );

    let conflict = request_json(
        &paths,
        json!({
            "id": "send-while-owned",
            "method": "session.send",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id},
                "text": "blocked\n"
            }
        }),
    );
    assert_error(
        &conflict,
        "send-while-owned",
        ControlErrorCode::InputOwnerConflict,
    );

    observer
        .write_all(AttachStreamFrame::Close.to_json_line().unwrap().as_bytes())
        .unwrap();
    wait_for_attach_closed_count(&session_paths.events_jsonl, 1);
    let listed_after_observer_close = listed_session(&paths, start.session.session_id);
    assert_eq!(
        listed_after_observer_close["attached_clients"], 1,
        "{listed_after_observer_close:#}"
    );
    assert_eq!(
        listed_after_observer_close["input_owner"], owner_stream_id,
        "{listed_after_observer_close:#}"
    );

    owner
        .write_all(AttachStreamFrame::Close.to_json_line().unwrap().as_bytes())
        .unwrap();
    wait_for_attach_closed_count(&session_paths.events_jsonl, 2);
    let listed_after_owner_close = listed_session(&paths, start.session.session_id);
    assert_eq!(
        listed_after_owner_close["attached_clients"], 0,
        "{listed_after_owner_close:#}"
    );
    assert_eq!(
        listed_after_owner_close["input_owner"],
        Value::Null,
        "{listed_after_owner_close:#}"
    );

    let after_close = request_json(
        &paths,
        json!({
            "id": "send-after-owner-close",
            "method": "session.send",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id},
                "text": "after-owner-close\n"
            }
        }),
    );
    assert_eq!(after_close["ok"], true, "{after_close:#}");
    wait_for_file_contains(&session_paths.pty_log, "got:after-owner-close");

    daemon.kill();
}

fn listed_session(paths: &StatePaths, session_id: millrace_sessions_core::ids::SessionId) -> Value {
    let listed = request_json(
        paths,
        json!({
            "id": "list-attach-state",
            "method": "session.list",
            "params": {}
        }),
    );
    assert_eq!(listed["ok"], true, "{listed:#}");
    listed["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["session_id"] == session_id.to_string())
        .unwrap_or_else(|| panic!("missing session {session_id} in {listed:#}"))
        .clone()
}

fn inspected_session_summary(
    paths: &StatePaths,
    session_id: millrace_sessions_core::ids::SessionId,
) -> Value {
    let inspected = request_json(
        paths,
        json!({
            "id": "inspect-attach-state",
            "method": "session.inspect",
            "params": {
                "selector": {"type": "id", "session_id": session_id}
            }
        }),
    );
    assert_eq!(inspected["ok"], true, "{inspected:#}");
    inspected["result"]["session"].clone()
}

fn request_json(paths: &StatePaths, value: Value) -> Value {
    let mut stream = UnixStream::connect(&paths.control_sock).expect("connect to daemon socket");
    stream
        .write_all(format!("{}\n", serde_json::to_string(&value).unwrap()).as_bytes())
        .expect("write request");
    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .expect("read response");
    serde_json::from_str(response.trim_end()).expect("response is json")
}

fn open_attach_stream(
    paths: &StatePaths,
    session_id: millrace_sessions_core::ids::SessionId,
    read_only: bool,
) -> UnixStream {
    let mut stream = UnixStream::connect(&paths.control_sock).expect("connect attach stream");
    stream
        .write_all(
            format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "id": if read_only { "attach-observer" } else { "attach-owner" },
                    "method": "session.attach",
                    "params": {
                        "selector": {"type": "id", "session_id": session_id},
                        "read_only": read_only,
                        "replay": "none"
                    }
                }))
                .unwrap()
            )
            .as_bytes(),
        )
        .unwrap();
    stream
}

fn read_attach_response(stream: &UnixStream) -> SessionAttachResponse {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut response_line = String::new();
    reader.read_line(&mut response_line).unwrap();
    let response: ControlResponse = serde_json::from_str(response_line.trim_end()).unwrap();
    assert!(response.ok, "{response:#?}");
    response.result_as().unwrap()
}

fn start_session(
    paths: &StatePaths,
    workspace: &Path,
    name: &str,
    script: &str,
) -> SessionStartResponse {
    let response = request_json(
        paths,
        json!({
            "id": format!("start-{name}"),
            "method": "session.start",
            "params": {
                "name": name,
                "role": "shell",
                "workspace": workspace,
                "cwd": workspace,
                "argv": ["sh", "-c", script]
            }
        }),
    );
    assert_eq!(response["ok"], true, "{response:#}");
    serde_json::from_value(response["result"].clone()).expect("start result")
}

fn assert_error(response: &Value, id: &str, code: ControlErrorCode) {
    assert_eq!(response["id"], id);
    assert_eq!(response["ok"], false);
    assert_eq!(
        serde_json::from_value::<ControlErrorCode>(response["error"]["code"].clone()).unwrap(),
        code
    );
}

fn wait_for_socket(path: &Path) {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if path.exists() && UnixStream::connect(path).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("daemon socket did not become ready at {}", path.display());
}

fn wait_for_worker_socket(path: &Path) {
    wait_for_socket(path);
}

fn wait_for_file_contains(path: &Path, needle: &str) {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if fs::read_to_string(path)
            .map(|raw| raw.contains(needle))
            .unwrap_or(false)
        {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("{} did not contain {needle:?}", path.display());
}

fn wait_for_terminal_or_running_meta(path: &Path) {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if let Ok(meta) = read_json::<SessionMeta>(path) {
            if matches!(
                meta.process_state,
                ProcessState::Running | ProcessState::Exited | ProcessState::Crashed
            ) {
                return;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("{} did not reach running or terminal state", path.display());
}

fn wait_for_terminal_meta(path: &Path) -> SessionMeta {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if let Ok(meta) = read_json::<SessionMeta>(path) {
            if !matches!(
                meta.process_state,
                ProcessState::Starting | ProcessState::Running
            ) {
                return meta;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("{} did not reach terminal state", path.display());
}

fn wait_for_running_meta(path: &Path) {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if let Ok(meta) = read_json::<SessionMeta>(path) {
            if meta.process_state == ProcessState::Running {
                return;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("{} did not reach running state", path.display());
}

fn wait_for_meta_state(path: &Path, state: ProcessState) -> SessionMeta {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if let Ok(meta) = read_json::<SessionMeta>(path) {
            if meta.process_state == state {
                return meta;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("{} did not reach state {state:?}", path.display());
}

fn wait_for_event_kind(path: &Path, kind: SessionEventKind) {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if read_events(path)
            .map(|events| events.iter().any(|event| event.kind == kind))
            .unwrap_or(false)
        {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("{} did not contain event kind {kind:?}", path.display());
}

fn wait_for_terminal_snapshot_size(path: &Path, rows: u16, cols: u16) {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if read_json::<TerminalSnapshot>(path)
            .map(|snapshot| snapshot.same_size(rows, cols))
            .unwrap_or(false)
        {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "{} did not contain terminal snapshot size rows={rows} cols={cols}",
        path.display()
    );
}

fn wait_for_attach_closed_count(path: &Path, expected: usize) {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if read_events(path)
            .map(|events| {
                events
                    .iter()
                    .filter(|event| event.kind == SessionEventKind::AttachClosed)
                    .count()
                    >= expected
            })
            .unwrap_or(false)
        {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "{} did not contain {expected} attach closed events",
        path.display()
    );
}

fn persist_terminal_replay_fixture(
    session_paths: &millrace_sessions_core::state::SessionPaths,
    raw: &[u8],
    expected_offset: u64,
) {
    append_raw_pty_log(&session_paths.pty_log, raw).unwrap();
    let mut state = TerminalStateBuffer::new(24, 80, 1024, 0);
    state.process_output(raw);
    state
        .persist(
            &session_paths.terminal_snapshot,
            &session_paths.raw_replay_ring,
        )
        .unwrap();
    let snapshot: TerminalSnapshot = read_json(&session_paths.terminal_snapshot).unwrap();
    assert_eq!(snapshot.pty_log_offset, expected_offset);
}

fn fake_millrace_path(root: &Path, script: &str) -> String {
    use std::os::unix::fs::PermissionsExt;

    let bin = root.join("fake-bin");
    fs::create_dir_all(&bin).unwrap();
    let millrace = bin.join("millrace");
    fs::write(&millrace, format!("#!/bin/sh\n{script}\n")).unwrap();
    let mut permissions = fs::metadata(&millrace).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&millrace, permissions).unwrap();
    std::env::join_paths([bin])
        .unwrap()
        .to_string_lossy()
        .to_string()
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

struct DaemonChild {
    child: Child,
}

impl DaemonChild {
    fn spawn(paths: &StatePaths) -> Self {
        let child = Command::new(sessiond_bin())
            .arg("--foreground")
            .env(STATE_DIR_ENV, &paths.root)
            .env("MILLMUX_WORKER_BIN", worker_bin())
            .spawn()
            .expect("spawn millrace-sessiond");
        Self { child }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for DaemonChild {
    fn drop(&mut self) {
        self.kill();
    }
}

fn sessiond_bin() -> PathBuf {
    let path = workspace_root()
        .join("target")
        .join("debug")
        .join("millrace-sessiond");
    ensure_bin(&path, "millrace-sessiond");
    path
}

fn worker_bin() -> PathBuf {
    if let Some(value) = std::env::var_os("MILLMUX_WORKER_BIN") {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            return path;
        }
        return workspace_root().join(path);
    }

    let path = workspace_root()
        .join("target")
        .join("debug")
        .join("millrace-session-worker");
    ensure_bin(&path, "millrace-session-worker");
    path
}

fn ensure_bin(path: &Path, binary_name: &str) {
    static SESSIOND_BUILD: Once = Once::new();
    static WORKER_BUILD: Once = Once::new();

    let build = || {
        let status = Command::new("cargo")
            .args(["build", "-p", "millrace-sessions", "--bin", binary_name])
            .current_dir(workspace_root())
            .status()
            .unwrap_or_else(|error| panic!("build {binary_name}: {error}"));
        assert!(status.success(), "failed to build {binary_name}");
    };
    match binary_name {
        "millrace-sessiond" => SESSIOND_BUILD.call_once(build),
        "millrace-session-worker" => WORKER_BUILD.call_once(build),
        _ => build(),
    }
    assert!(is_executable(path), "{} is not executable", path.display());
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}
