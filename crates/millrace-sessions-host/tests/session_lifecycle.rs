use std::{
    collections::BTreeMap,
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
    events::{append_event, read_events, SessionEvent, SessionEventKind},
    ids::SessionId,
    paths::{StatePaths, STATE_DIR_ENV},
    protocol::{
        AttachFrameType, AttachInitialReplay, AttachStreamFrame, ControlErrorCode, ControlResponse,
        LogStream, SessionAttachResponse, SessionDeleteResponse, SessionEventsResponse,
        SessionInspectResponse, SessionKillResponse, SessionLogsResponse, SessionResizeResponse,
        SessionSelector, SessionSendResponse, SessionStartResponse, SessionStopResponse,
    },
    scrollback::{restore_terminal_replay, TerminalSnapshot, TerminalStateBuffer},
    state::{
        AttentionState, MonitorProfile, ProcessState, SessionMeta, SessionRole, SpawnMode,
        WorkerMeta,
    },
    storage::{append_raw_pty_log, create_private_dir_all, read_json, write_json_atomic},
};
use millrace_sessions_host::{reconcile::reconcile_startup, registry::HostRegistry};
use nix::{
    sys::{
        signal::kill,
        socket::{setsockopt, sockopt::RcvBuf},
    },
    unistd::Pid,
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
fn pipe_session_exposes_stream_logs_capabilities_and_artifacts() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_pipe_session(
        &paths,
        &workspace,
        "pipe-artifacts",
        "printf 'pipe-out\\n'; printf 'pipe-err\\n' >&2; exit 7",
    );
    assert_eq!(start.session.spawn_mode, SpawnMode::Pipe);
    assert!(!start.session.capabilities.attach);
    assert!(!start.session.capabilities.send);
    assert!(!start.session.capabilities.resize);
    assert!(start.session.artifacts.pty.is_none());
    assert!(start.session.artifacts.pipe.is_some());

    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_file_contains(&session_paths.stdout_log, "pipe-out");
    wait_for_file_contains(&session_paths.stderr_log, "pipe-err");
    let meta = wait_for_terminal_meta(&session_paths.meta_json);
    assert_eq!(meta.spawn_mode, SpawnMode::Pipe);
    assert_eq!(meta.exit_code, Some(7));
    assert!(
        !session_paths.pty_log.exists(),
        "pipe sessions must not fake pty.log"
    );

    let listed = listed_session(&paths, start.session.session_id);
    assert_eq!(listed["spawn_mode"], "pipe", "{listed:#}");
    assert_eq!(listed["capabilities"]["attach"], false, "{listed:#}");
    assert!(listed["artifacts"]["pty"].is_null(), "{listed:#}");
    assert!(
        listed["artifacts"]["pipe"]["stdout_log"]
            .as_str()
            .is_some_and(|path| path.ends_with("stdout.log")),
        "{listed:#}"
    );

    let inspected = inspected_session_summary(&paths, start.session.session_id);
    assert_eq!(inspected["spawn_mode"], "pipe", "{inspected:#}");
    assert_eq!(inspected["capabilities"]["send"], false, "{inspected:#}");
    assert!(
        inspected["artifacts"]["pipe"]["stderr_log"]
            .as_str()
            .is_some_and(|path| path.ends_with("stderr.log")),
        "{inspected:#}"
    );

    let response = request_json(
        &paths,
        json!({
            "id": "pipe-logs",
            "method": "session.logs",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id},
                "tail": 10
            }
        }),
    );
    assert_eq!(response["ok"], true, "{response:#}");
    let logs: SessionLogsResponse =
        serde_json::from_value(response["result"].clone()).expect("logs result");
    assert!(logs
        .lines
        .iter()
        .any(|line| line.stream == LogStream::Stdout && line.line == "pipe-out"));
    assert!(logs
        .lines
        .iter()
        .any(|line| line.stream == LogStream::Stderr && line.line == "pipe-err"));

    daemon.kill();
}

#[test]
fn pipe_session_rejects_pty_only_control_methods() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_pipe_session(
        &paths,
        &workspace,
        "pipe-no-pty-controls",
        "printf 'ready\\n'; sleep 5",
    );
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_file_contains(&session_paths.stdout_log, "ready");
    wait_for_running_meta(&session_paths.meta_json);

    let attach = request_json(
        &paths,
        json!({
            "id": "pipe-attach",
            "method": "session.attach",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id},
                "replay": "none"
            }
        }),
    );
    assert_error(
        &attach,
        "pipe-attach",
        ControlErrorCode::UnsupportedSpawnMode,
    );

    let send = request_json(
        &paths,
        json!({
            "id": "pipe-send",
            "method": "session.send",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id},
                "text": "hello\n"
            }
        }),
    );
    assert_error(&send, "pipe-send", ControlErrorCode::UnsupportedSpawnMode);

    let resize = request_json(
        &paths,
        json!({
            "id": "pipe-resize",
            "method": "session.resize",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id},
                "rows": 24,
                "cols": 80
            }
        }),
    );
    assert_error(
        &resize,
        "pipe-resize",
        ControlErrorCode::UnsupportedSpawnMode,
    );

    let stop = request_json(
        &paths,
        json!({
            "id": "pipe-stop",
            "method": "session.stop",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id},
                "grace_seconds": 1
            }
        }),
    );
    assert_eq!(stop["ok"], true, "{stop:#}");
    assert!(matches!(
        wait_for_terminal_meta(&session_paths.meta_json).process_state,
        ProcessState::Exited | ProcessState::Killed | ProcessState::Crashed
    ));

    daemon.kill();
}

#[test]
fn liveness_reconciliation_marks_pty_and_pipe_worker_dead_child_alive_orphaned() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    create_private_dir_all(&paths.sessions_dir).unwrap();
    create_private_dir_all(&paths.archive_dir).unwrap();

    let live_child_pid = std::process::id();
    let pty_session = write_liveness_orphan_session(&paths, SpawnMode::Pty, live_child_pid);
    let pipe_session = write_liveness_orphan_session(&paths, SpawnMode::Pipe, live_child_pid);

    let summary = reconcile_startup(&paths).unwrap();
    assert_eq!(summary.scanned, 2);
    assert_eq!(summary.preserved, 0);
    assert_eq!(summary.marked_terminal, 2);

    let registry = HostRegistry::load(paths.clone()).unwrap();
    for session_id in [pty_session, pipe_session] {
        let inspected = registry
            .inspect(&SessionSelector::Id { session_id })
            .expect("session remains inspectable");
        assert_eq!(inspected.session.process_state, ProcessState::Orphaned);
        assert_eq!(inspected.session.attached_clients, 0);
        assert_eq!(inspected.session.input_owner, None);
        assert_eq!(
            inspected.session.failure_message.as_deref(),
            Some("startup reconciliation found a live child without a live worker")
        );

        let session_paths = paths.session_paths(session_id);
        let meta: SessionMeta = read_json(&session_paths.meta_json).unwrap();
        let worker: WorkerMeta = read_json(&session_paths.worker_json).unwrap();
        assert_eq!(meta.child_pid, Some(live_child_pid));
        assert_eq!(worker.child_pid, Some(live_child_pid));
        assert_eq!(worker.process_state, ProcessState::Orphaned);
        assert_eq!(worker.attached_clients, 0);
        assert_eq!(worker.input_owner, None);

        let events = read_events(&session_paths.events_jsonl).unwrap();
        assert!(events.iter().any(|event| {
            event.kind == SessionEventKind::StateChanged
                && event.process_state == Some(ProcessState::Orphaned)
                && event.fields.get("liveness_reason").map(String::as_str) == Some("orphaned_child")
        }));
    }
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
fn client_loss_does_not_kill_hosted_child() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_session(
        &paths,
        &workspace,
        "client-loss-shell",
        "printf 'ready\\n'; while IFS= read -r line; do printf 'got:%s\\n' \"$line\"; done",
    );
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_worker_socket(&session_paths.worker_sock);
    wait_for_running_meta(&session_paths.meta_json);
    wait_for_file_contains(&session_paths.pty_log, "ready");
    let worker: WorkerMeta = read_json(&session_paths.worker_json).unwrap();
    let child_pid = worker.child_pid.expect("child pid is recorded");

    let stream = open_attach_stream(&paths, start.session.session_id, false);
    let attach = read_attach_response(&stream);
    assert!(attach.stream.input_owner);
    drop(stream);

    wait_for_attach_closed_count(&session_paths.events_jsonl, 1);
    assert_process_alive(worker.pid, "worker after client loss");
    assert_process_alive(child_pid, "child after client loss");

    let after_loss = request_json(
        &paths,
        json!({
            "id": "send-after-client-loss",
            "method": "session.send",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id},
                "text": "after-client-loss\n"
            }
        }),
    );
    assert_eq!(after_loss["ok"], true, "{after_loss:#}");
    wait_for_file_contains(&session_paths.pty_log, "got:after-client-loss");

    let cleanup = request_json(
        &paths,
        json!({
            "id": "kill-after-client-loss",
            "method": "session.kill",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id}
            }
        }),
    );
    assert_eq!(cleanup["ok"], true, "{cleanup:#}");

    daemon.kill();
}

#[test]
fn restart_preserves_pty_session_and_supported_surfaces_work() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_session(
        &paths,
        &workspace,
        "restart-pty-shell",
        "printf 'ready\\n'; while IFS= read -r line; do printf 'got:%s\\n' \"$line\"; done",
    );
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_worker_socket(&session_paths.worker_sock);
    wait_for_running_meta(&session_paths.meta_json);
    wait_for_file_contains(&session_paths.pty_log, "ready");
    let worker_before: WorkerMeta = read_json(&session_paths.worker_json).unwrap();
    let child_pid = worker_before.child_pid.expect("child pid is recorded");

    daemon.kill();
    assert_process_alive(worker_before.pid, "worker after sessiond restart");
    assert_process_alive(child_pid, "child after sessiond restart");

    let mut restarted = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);
    let host_status = request_json(
        &paths,
        json!({"id": "host-status-after-restart", "method": "host.status", "params": {}}),
    );
    assert_eq!(host_status["ok"], true, "{host_status:#}");

    let inspected = request_json(
        &paths,
        json!({
            "id": "inspect-after-restart",
            "method": "session.inspect",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id}
            }
        }),
    );
    assert_eq!(inspected["ok"], true, "{inspected:#}");
    let inspect: SessionInspectResponse =
        serde_json::from_value(inspected["result"].clone()).expect("inspect result");
    assert_eq!(inspect.session.process_state, ProcessState::Running);
    assert_eq!(inspect.session.spawn_mode, SpawnMode::Pty);
    assert_eq!(
        inspect.worker.as_ref().map(|worker| worker.pid),
        Some(worker_before.pid)
    );

    let logs = request_json(
        &paths,
        json!({
            "id": "logs-after-restart",
            "method": "session.logs",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id},
                "tail": 10
            }
        }),
    );
    assert_eq!(logs["ok"], true, "{logs:#}");
    let logs: SessionLogsResponse =
        serde_json::from_value(logs["result"].clone()).expect("logs result");
    assert!(logs.lines.iter().any(|line| line.line.contains("ready")));

    let events = request_json(
        &paths,
        json!({
            "id": "events-after-restart",
            "method": "session.events",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id}
            }
        }),
    );
    assert_eq!(events["ok"], true, "{events:#}");
    let events: SessionEventsResponse =
        serde_json::from_value(events["result"].clone()).expect("events result");
    assert!(events
        .events
        .iter()
        .any(|event| event.kind == SessionEventKind::Output));

    let send = request_json(
        &paths,
        json!({
            "id": "send-after-restart",
            "method": "session.send",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id},
                "text": "after-restart\n"
            }
        }),
    );
    assert_eq!(send["ok"], true, "{send:#}");
    wait_for_file_contains(&session_paths.pty_log, "got:after-restart");

    let mut attach =
        open_attach_stream_with_replay(&paths, start.session.session_id, true, "line_scrollback");
    let attach_response = read_attach_response(&attach);
    assert!(attach_response.stream.read_only);
    let mut reader = BufReader::new(attach.try_clone().unwrap());
    let frame = wait_for_attach_frame(
        &mut reader,
        |frame| matches!(frame, AttachStreamFrame::Scrollback { lines } if lines.iter().any(|line| line.contains("ready"))),
    );
    assert!(matches!(frame, AttachStreamFrame::Scrollback { .. }));
    attach
        .write_all(AttachStreamFrame::Close.to_json_line().unwrap().as_bytes())
        .unwrap();
    wait_for_attach_closed_count(&session_paths.events_jsonl, 1);

    let cleanup = request_json(
        &paths,
        json!({
            "id": "kill-after-pty-restart",
            "method": "session.kill",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id}
            }
        }),
    );
    assert_eq!(cleanup["ok"], true, "{cleanup:#}");

    restarted.kill();
}

#[test]
fn restart_preserves_pipe_session_and_supported_surfaces_work() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_pipe_session(
        &paths,
        &workspace,
        "restart-pipe-shell",
        "trap 'printf pipe-stopped\\n; exit 0' TERM; printf 'ready\\n'; while true; do sleep 1; done",
    );
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_file_contains(&session_paths.stdout_log, "ready");
    wait_for_running_meta(&session_paths.meta_json);
    let worker_before: WorkerMeta = read_json(&session_paths.worker_json).unwrap();
    let child_pid = worker_before.child_pid.expect("child pid is recorded");

    daemon.kill();
    assert_process_alive(worker_before.pid, "pipe worker after sessiond restart");
    assert_process_alive(child_pid, "pipe child after sessiond restart");

    let mut restarted = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);
    let inspected = request_json(
        &paths,
        json!({
            "id": "pipe-inspect-after-restart",
            "method": "session.inspect",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id}
            }
        }),
    );
    assert_eq!(inspected["ok"], true, "{inspected:#}");
    let inspect: SessionInspectResponse =
        serde_json::from_value(inspected["result"].clone()).expect("pipe inspect result");
    assert_eq!(inspect.session.process_state, ProcessState::Running);
    assert_eq!(inspect.session.spawn_mode, SpawnMode::Pipe);
    assert_eq!(
        inspect.worker.as_ref().map(|worker| worker.pid),
        Some(worker_before.pid)
    );

    let logs = request_json(
        &paths,
        json!({
            "id": "pipe-logs-after-restart",
            "method": "session.logs",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id},
                "tail": 10
            }
        }),
    );
    assert_eq!(logs["ok"], true, "{logs:#}");
    let logs: SessionLogsResponse =
        serde_json::from_value(logs["result"].clone()).expect("pipe logs result");
    assert!(logs
        .lines
        .iter()
        .any(|line| line.stream == LogStream::Stdout && line.line == "ready"));

    let events = request_json(
        &paths,
        json!({
            "id": "pipe-events-after-restart",
            "method": "session.events",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id}
            }
        }),
    );
    assert_eq!(events["ok"], true, "{events:#}");
    let events: SessionEventsResponse =
        serde_json::from_value(events["result"].clone()).expect("pipe events result");
    assert!(events.events.iter().any(|event| {
        event.kind == SessionEventKind::Output
            && event.fields.get("stream").map(String::as_str) == Some("stdout")
    }));

    let stop = request_json(
        &paths,
        json!({
            "id": "pipe-stop-after-restart",
            "method": "session.stop",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id},
                "grace_seconds": 2
            }
        }),
    );
    assert_eq!(stop["ok"], true, "{stop:#}");
    let stop: SessionStopResponse =
        serde_json::from_value(stop["result"].clone()).expect("pipe stop result");
    assert!(stop.stop_requested);
    let meta = wait_for_terminal_meta(&session_paths.meta_json);
    assert!(matches!(
        meta.process_state,
        ProcessState::Exited | ProcessState::Crashed | ProcessState::Killed
    ));

    let deleted = request_json(
        &paths,
        json!({
            "id": "pipe-delete-after-restart",
            "method": "session.delete",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id}
            }
        }),
    );
    assert_eq!(deleted["ok"], true, "{deleted:#}");
    let delete: SessionDeleteResponse =
        serde_json::from_value(deleted["result"].clone()).expect("pipe delete result");
    assert!(delete.deleted);
    assert!(delete.archived);
    let archive_root = paths.archive_dir.join(start.session.session_id.to_string());
    assert!(archive_root.join("stdout.log").exists());
    assert!(archive_root.join("stderr.log").exists());
    assert!(archive_root.join("events.jsonl").exists());

    restarted.kill();
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
fn attach_stream_forwards_input_and_resize_on_worker_stream() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_session(
        &paths,
        &workspace,
        "attach-io-shell",
        "printf 'ready\\n'; while IFS= read -r line; do printf 'got:%s\\n' \"$line\"; done",
    );
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_worker_socket(&session_paths.worker_sock);
    wait_for_running_meta(&session_paths.meta_json);
    wait_for_file_contains(&session_paths.pty_log, "ready");

    let mut stream = UnixStream::connect(&paths.control_sock).expect("connect attach stream");
    stream
        .write_all(
            format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "id": "attach-io",
                    "method": "session.attach",
                    "params": {
                        "selector": {"type": "id", "session_id": start.session.session_id},
                        "replay": "none"
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

    stream
        .write_all(
            AttachStreamFrame::Input {
                text: "via-attach\n".to_string(),
            }
            .to_json_line()
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
    wait_for_file_contains(&session_paths.pty_log, "got:via-attach");

    stream
        .write_all(
            AttachStreamFrame::Resize { rows: 33, cols: 77 }
                .to_json_line()
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
    wait_for_terminal_snapshot_size(&session_paths.terminal_snapshot, 33, 77);

    stream
        .write_all(AttachStreamFrame::Close.to_json_line().unwrap().as_bytes())
        .unwrap();
    wait_for_event_kind(&session_paths.events_jsonl, SessionEventKind::AttachClosed);

    daemon.kill();
}

#[test]
fn attach_stream_delivers_queued_input_before_close_release() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_session(
        &paths,
        &workspace,
        "attach-close-order-shell",
        "printf 'ready\\n'; while IFS= read -r line; do printf 'got:%s\\n' \"$line\"; done",
    );
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_worker_socket(&session_paths.worker_sock);
    wait_for_file_contains(&session_paths.pty_log, "ready");

    let mut stream = open_attach_stream(&paths, start.session.session_id, false);
    let attach = read_attach_response(&stream);
    assert!(attach.stream.input_owner);
    stream
        .write_all(
            AttachStreamFrame::Input {
                text: "close-order\n".to_string(),
            }
            .to_json_line()
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
    stream
        .write_all(AttachStreamFrame::Close.to_json_line().unwrap().as_bytes())
        .unwrap();

    wait_for_file_contains(&session_paths.pty_log, "got:close-order");
    wait_for_event_kind(&session_paths.events_jsonl, SessionEventKind::AttachClosed);

    let after_close = request_json(
        &paths,
        json!({
            "id": "send-after-close-order",
            "method": "session.send",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id},
                "text": "after-close-order\n"
            }
        }),
    );
    assert_eq!(after_close["ok"], true, "{after_close:#}");
    wait_for_file_contains(&session_paths.pty_log, "got:after-close-order");

    daemon.kill();
}

#[test]
fn raw_attach_stream_forwards_binary_input_live_output_and_resize() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_session(
        &paths,
        &workspace,
        "raw-attach-io-shell",
        concat!(
            "stty raw -echo; ",
            "printf 'ready\\n'; ",
            "dd bs=1 count=6 2>/dev/null | od -An -tx1 | tr -d ' \\n'; ",
            "printf '\\n'; ",
            "printf '\\377\\000\\033[31mraw-live'; ",
            "while [ ! -f go-resize ]; do sleep 0.05; done; ",
            "stty size; sleep 1"
        ),
    );
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_worker_socket(&session_paths.worker_sock);
    wait_for_running_meta(&session_paths.meta_json);
    wait_for_file_contains(&session_paths.pty_log, "ready");

    let mut stream =
        open_raw_attach_stream(&paths, start.session.session_id, false, Some((26, 88)));
    let attach = read_attach_response(&stream);
    assert!(attach.stream.input_owner);
    assert_eq!(
        attach.accepted_frame_types,
        vec![
            AttachFrameType::RawOutput,
            AttachFrameType::RawInput,
            AttachFrameType::StreamLagged
        ]
    );
    wait_for_terminal_snapshot_size(&session_paths.terminal_snapshot, 26, 88);

    let mut reader = BufReader::new(stream.try_clone().unwrap());
    stream
        .write_all(
            AttachStreamFrame::raw_input(vec![0xff, 0x00, 0x1b, b'[', b'A', 0x03])
                .to_json_line()
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
    wait_for_file_bytes_contains(&session_paths.pty_log, b"ff001b5b4103");
    let seen = wait_for_raw_output_contains(&mut reader, b"\xff\0\x1b[31mraw-live");
    assert!(
        !contains_bytes(&seen, "\u{fffd}".as_bytes()),
        "raw live output passed through UTF-8 replacement: {seen:?}"
    );

    stream
        .write_all(
            AttachStreamFrame::Resize { rows: 34, cols: 90 }
                .to_json_line()
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
    wait_for_terminal_snapshot_size(&session_paths.terminal_snapshot, 34, 90);
    fs::write(workspace.join("go-resize"), b"go").unwrap();
    wait_for_file_bytes_contains(&session_paths.pty_log, b"34 90");

    stream
        .write_all(AttachStreamFrame::Close.to_json_line().unwrap().as_bytes())
        .unwrap();
    wait_for_event_kind(&session_paths.events_jsonl, SessionEventKind::AttachClosed);

    daemon.kill();
}

#[test]
fn raw_attach_rejects_raw_input_without_negotiated_writable_owner() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_session(
        &paths,
        &workspace,
        "raw-attach-reject-shell",
        "printf 'ready\\n'; sleep 5",
    );
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_worker_socket(&session_paths.worker_sock);
    wait_for_running_meta(&session_paths.meta_json);
    wait_for_file_contains(&session_paths.pty_log, "ready");

    let mut legacy = open_attach_stream(&paths, start.session.session_id, false);
    let legacy_attach = read_attach_response(&legacy);
    assert!(legacy_attach.stream.input_owner);
    assert_attach_raw_input_error(&mut legacy, ControlErrorCode::InvalidRequest);
    legacy
        .write_all(AttachStreamFrame::Close.to_json_line().unwrap().as_bytes())
        .unwrap();
    wait_for_attach_closed_count(&session_paths.events_jsonl, 1);

    let mut text_v2 = open_v2_text_attach_stream(&paths, start.session.session_id);
    let text_attach = read_attach_response(&text_v2);
    assert!(text_attach.stream.input_owner);
    assert_attach_raw_input_error(&mut text_v2, ControlErrorCode::InvalidRequest);
    text_v2
        .write_all(AttachStreamFrame::Close.to_json_line().unwrap().as_bytes())
        .unwrap();
    wait_for_attach_closed_count(&session_paths.events_jsonl, 2);

    let mut read_only = open_raw_attach_stream(&paths, start.session.session_id, true, None);
    let read_only_attach = read_attach_response(&read_only);
    assert!(!read_only_attach.stream.input_owner);
    assert_attach_raw_input_error(&mut read_only, ControlErrorCode::InvalidRequest);
    read_only
        .write_all(AttachStreamFrame::Close.to_json_line().unwrap().as_bytes())
        .unwrap();
    wait_for_attach_closed_count(&session_paths.events_jsonl, 3);

    let owner = open_raw_attach_stream(&paths, start.session.session_id, false, None);
    let owner_attach = read_attach_response(&owner);
    assert!(owner_attach.stream.input_owner);
    let non_owner = request_json(
        &paths,
        json!({
            "id": "raw-non-owner",
            "method": "session.attach",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id},
                "client_protocol_version": 2,
                "accepted_frame_types": ["raw_output", "raw_input"],
                "stream_encoding": "raw_bytes",
                "initial_replay": "none"
            }
        }),
    );
    assert_error(
        &non_owner,
        "raw-non-owner",
        ControlErrorCode::InputOwnerConflict,
    );

    daemon.kill();
}

#[test]
fn raw_attach_works_after_sessiond_restart() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_session(
        &paths,
        &workspace,
        "raw-after-restart-shell",
        "printf 'ready\\n'; while [ ! -f go-raw-restart ]; do sleep 0.05; done; printf '\\377raw-after-restart'",
    );
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_worker_socket(&session_paths.worker_sock);
    wait_for_running_meta(&session_paths.meta_json);
    wait_for_file_contains(&session_paths.pty_log, "ready");
    let worker_before: WorkerMeta = read_json(&session_paths.worker_json).unwrap();

    daemon.kill();
    assert_process_alive(worker_before.pid, "worker before raw restart attach");

    let mut restarted = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let stream = open_raw_attach_stream(&paths, start.session.session_id, true, None);
    let attach = read_attach_response(&stream);
    assert!(attach.stream.read_only);
    assert!(!attach.stream.input_owner);
    assert!(attach.confirms_raw_stream());
    let mut reader = BufReader::new(stream.try_clone().unwrap());

    fs::write(workspace.join("go-raw-restart"), b"go").expect("release raw restart fixture");
    wait_for_raw_output_contains(&mut reader, b"\xffraw-after-restart");

    restarted.kill();
}

#[test]
fn attach_stream_reports_lagged_event_to_slow_v2_observer() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_session(
        &paths,
        &workspace,
        "attach-lag-shell",
        concat!(
            "printf 'ready\\n'; ",
            "while IFS= read -r line; do ",
            "case \"$line\" in ",
            "burst) dd if=/dev/zero bs=65536 count=256 2>/dev/null | tr '\\000' x; printf '\\nDONE\\n' ;; ",
            "tick) printf 'tick\\n' ;; ",
            "esac; ",
            "done"
        ),
    );
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_worker_socket(&session_paths.worker_sock);
    wait_for_file_contains(&session_paths.pty_log, "ready");

    let mut slow = UnixStream::connect(&paths.control_sock).expect("connect slow attach stream");
    setsockopt(&slow, RcvBuf, &4096_usize).unwrap();
    slow.write_all(
        format!(
            "{}\n",
            serde_json::to_string(&json!({
                "id": "attach-slow-lag",
                "method": "session.attach",
                "params": {
                    "selector": {"type": "id", "session_id": start.session.session_id},
                    "read_only": true,
                    "replay": "none",
                    "client_protocol_version": 2,
                    "accepted_frame_types": ["stream_lagged"],
                    "initial_replay": "none"
                }
            }))
            .unwrap()
        )
        .as_bytes(),
    )
    .unwrap();
    slow.set_read_timeout(Some(Duration::from_millis(250)))
        .unwrap();
    let mut slow_reader = BufReader::new(slow.try_clone().unwrap());
    let mut response_line = String::new();
    slow_reader.read_line(&mut response_line).unwrap();
    let response: ControlResponse = serde_json::from_str(response_line.trim_end()).unwrap();
    assert!(response.ok, "{response:#?}");
    let attach: SessionAttachResponse = response.result_as().unwrap();
    assert_eq!(
        attach.accepted_frame_types,
        vec![AttachFrameType::StreamLagged]
    );

    let fast_one = open_attach_stream(&paths, start.session.session_id, true);
    let fast_one_attach = read_attach_response(&fast_one);
    assert!(!fast_one_attach.stream.input_owner);
    let mut fast_one_writer = fast_one.try_clone().unwrap();
    let fast_one_reader = spawn_attach_drain(fast_one);

    let fast_two = open_attach_stream(&paths, start.session.session_id, true);
    let fast_two_attach = read_attach_response(&fast_two);
    assert!(!fast_two_attach.stream.input_owner);
    let mut fast_two_writer = fast_two.try_clone().unwrap();
    let fast_two_reader = spawn_attach_drain(fast_two);

    let burst_started = Instant::now();
    let burst = request_json(
        &paths,
        json!({
            "id": "lag-burst",
            "method": "session.send",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id},
                "text": "burst\n"
            }
        }),
    );
    assert_eq!(burst["ok"], true, "{burst:#}");
    wait_for_event_kind_timeout(
        &session_paths.events_jsonl,
        SessionEventKind::AttachStreamLagged,
        Duration::from_secs(10),
    );
    assert!(
        burst_started.elapsed() <= Duration::from_secs(10),
        "16 MiB burst did not produce lag evidence within 10s"
    );

    for _ in 0..128 {
        if read_next_attach_frame(&mut slow_reader).is_none() {
            break;
        }
    }
    for _ in 0..128 {
        let Some(frame) = read_next_attach_frame(&mut slow_reader) else {
            break;
        };
        if let AttachStreamFrame::StreamLagged {
            dropped_from_offset,
            dropped_to_offset,
            current_pty_log_offset,
            ..
        } = frame
        {
            assert!(dropped_from_offset < dropped_to_offset);
            assert!(current_pty_log_offset >= dropped_to_offset);
            break;
        }
    }

    slow.write_all(AttachStreamFrame::Close.to_json_line().unwrap().as_bytes())
        .unwrap();
    fast_one_writer
        .write_all(AttachStreamFrame::Close.to_json_line().unwrap().as_bytes())
        .unwrap();
    fast_two_writer
        .write_all(AttachStreamFrame::Close.to_json_line().unwrap().as_bytes())
        .unwrap();
    assert!(fast_one_reader.join().unwrap() > 0);
    assert!(fast_two_reader.join().unwrap() > 0);
    wait_for_event_kind(&session_paths.events_jsonl, SessionEventKind::AttachClosed);

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
        matches!(frame, AttachStreamFrame::RawOutput { ref data } if data.as_slice() == raw.as_slice()),
        "expected raw replay {raw:?}, got {frame:?}"
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
fn attach_v2_initial_replay_none_suppresses_legacy_terminal_snapshot_replay() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_session(&paths, &workspace, "v2-none-raw-attach-shell", "sleep 5");
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_worker_socket(&session_paths.worker_sock);
    wait_for_running_meta(&session_paths.meta_json);
    let raw = b"\x1b[?1049hv2-none raw\r\n".to_vec();
    persist_terminal_replay_fixture(&session_paths, &raw, raw.len() as u64);

    let mut stream = UnixStream::connect(&paths.control_sock).expect("connect attach stream");
    stream
        .write_all(
            format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "id": "attach-v2-none",
                    "method": "session.attach",
                    "params": {
                        "selector": {"type": "id", "session_id": start.session.session_id},
                        "replay": "terminal_snapshot",
                        "requested_terminal_size": {"rows": 24, "cols": 80},
                        "client_protocol_version": 2,
                        "accepted_frame_types": ["raw_output"],
                        "stream_encoding": "raw_bytes",
                        "initial_replay": "none"
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
    assert_eq!(
        attach.negotiated_initial_replay,
        Some(AttachInitialReplay::None)
    );
    assert_eq!(
        attach.accepted_frame_types,
        vec![AttachFrameType::RawOutput]
    );

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
fn attach_v2_screen_snapshot_initial_replay_uses_structured_frame_or_unavailable() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let start = start_session(&paths, &workspace, "v2-screen-snapshot-shell", "sleep 5");
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_worker_socket(&session_paths.worker_sock);
    wait_for_running_meta(&session_paths.meta_json);

    let mut stream = UnixStream::connect(&paths.control_sock).expect("connect attach stream");
    stream
        .write_all(
            format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "id": "attach-v2-screen",
                    "method": "session.attach",
                    "params": {
                        "selector": {"type": "id", "session_id": start.session.session_id},
                        "client_protocol_version": 2,
                        "accepted_frame_types": [
                            "stream_lagged",
                            "snapshot_unavailable",
                            "screen_snapshot"
                        ],
                        "initial_replay": "screen_snapshot"
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
    assert_eq!(
        attach.negotiated_initial_replay,
        Some(AttachInitialReplay::ScreenSnapshot)
    );
    assert_eq!(
        attach.accepted_frame_types,
        vec![
            AttachFrameType::StreamLagged,
            AttachFrameType::ScreenSnapshot,
            AttachFrameType::SnapshotUnavailable
        ]
    );

    let mut frame_line = String::new();
    reader.read_line(&mut frame_line).unwrap();
    let frame = AttachStreamFrame::from_json_line(&frame_line).unwrap();
    assert!(matches!(
        &frame,
        AttachStreamFrame::ScreenSnapshot { .. } | AttachStreamFrame::SnapshotUnavailable { .. }
    ));
    assert!(!matches!(&frame, AttachStreamFrame::RawOutput { .. }));

    stream
        .write_all(AttachStreamFrame::Close.to_json_line().unwrap().as_bytes())
        .unwrap();
    wait_for_event_kind(&session_paths.events_jsonl, SessionEventKind::AttachClosed);

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

fn write_liveness_orphan_session(
    paths: &StatePaths,
    spawn_mode: SpawnMode,
    live_child_pid: u32,
) -> SessionId {
    let session_id = SessionId::new();
    let dead_worker_pid = dead_process_pid();
    let now = "2026-05-20T18:00:00Z".to_string();
    let session_paths = paths.session_paths(session_id);
    create_private_dir_all(&session_paths.root).unwrap();

    let meta = SessionMeta {
        id: session_id,
        name: Some(format!("liveness-{spawn_mode}")),
        role: SessionRole::Shell,
        process_state: ProcessState::Running,
        attention_state: AttentionState::Active,
        attention_items: Vec::new(),
        status_summary: None,
        workspace: None,
        cwd: paths.root.clone(),
        argv: vec!["sh".to_string(), "-c".to_string(), "sleep 60".to_string()],
        spawn_mode,
        monitor_profile: MonitorProfile::Auto,
        env: BTreeMap::new(),
        worker_pid: Some(dead_worker_pid),
        child_pid: Some(live_child_pid),
        child_pgid: Some(live_child_pid),
        started_at: Some(now.clone()),
        ended_at: None,
        stop_requested_at: None,
        stop_reason: None,
        exit_code: None,
        exit_signal: None,
        failure_message: None,
        created_at: now.clone(),
        updated_at: now.clone(),
    };
    let worker = WorkerMeta {
        session_id,
        pid: dead_worker_pid,
        child_pid: Some(live_child_pid),
        child_pgid: Some(live_child_pid),
        spawn_mode,
        process_state: ProcessState::Running,
        started_at: now.clone(),
        ended_at: None,
        stop_requested_at: None,
        stop_reason: None,
        exit_code: None,
        exit_signal: None,
        attached_clients: if spawn_mode.is_pty() { 1 } else { 0 },
        input_owner: spawn_mode.is_pty().then(|| "stale-owner".to_string()),
        updated_at: now,
    };

    write_json_atomic(&session_paths.meta_json, &meta).unwrap();
    write_json_atomic(&session_paths.worker_json, &worker).unwrap();
    match spawn_mode {
        SpawnMode::Pty => append_raw_pty_log(&session_paths.pty_log, b"orphan pty\n").unwrap(),
        SpawnMode::Pipe => {
            fs::write(&session_paths.stdout_log, b"orphan stdout\n").unwrap();
            fs::write(&session_paths.stderr_log, b"orphan stderr\n").unwrap();
        }
    }
    append_event(
        &session_paths.events_jsonl,
        &SessionEvent::new(session_id, SessionEventKind::SessionCreated),
    )
    .unwrap();
    session_id
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

fn dead_process_pid() -> u32 {
    let mut child = Command::new("sh").arg("-c").arg("exit 0").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

fn open_attach_stream(
    paths: &StatePaths,
    session_id: millrace_sessions_core::ids::SessionId,
    read_only: bool,
) -> UnixStream {
    open_attach_stream_with_replay(paths, session_id, read_only, "none")
}

fn open_attach_stream_with_replay(
    paths: &StatePaths,
    session_id: millrace_sessions_core::ids::SessionId,
    read_only: bool,
    replay: &str,
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
                        "replay": replay
                    }
                }))
                .unwrap()
            )
            .as_bytes(),
        )
        .unwrap();
    stream
}

fn open_raw_attach_stream(
    paths: &StatePaths,
    session_id: millrace_sessions_core::ids::SessionId,
    read_only: bool,
    requested_size: Option<(u16, u16)>,
) -> UnixStream {
    let mut stream = UnixStream::connect(&paths.control_sock).expect("connect raw attach stream");
    let mut params = json!({
        "selector": {"type": "id", "session_id": session_id},
        "read_only": read_only,
        "replay": "none",
        "client_protocol_version": 2,
        "accepted_frame_types": if read_only {
            json!(["raw_output", "stream_lagged"])
        } else {
            json!(["raw_output", "raw_input", "stream_lagged"])
        },
        "stream_encoding": "raw_bytes",
        "initial_replay": "none"
    });
    if let Some((rows, cols)) = requested_size {
        params["requested_terminal_size"] = json!({ "rows": rows, "cols": cols });
    }
    stream
        .write_all(
            format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "id": if read_only { "raw-attach-observer" } else { "raw-attach-owner" },
                    "method": "session.attach",
                    "params": params
                }))
                .unwrap()
            )
            .as_bytes(),
        )
        .unwrap();
    stream
}

fn open_v2_text_attach_stream(
    paths: &StatePaths,
    session_id: millrace_sessions_core::ids::SessionId,
) -> UnixStream {
    let mut stream = UnixStream::connect(&paths.control_sock).expect("connect text attach stream");
    stream
        .write_all(
            format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "id": "v2-text-attach",
                    "method": "session.attach",
                    "params": {
                        "selector": {"type": "id", "session_id": session_id},
                        "client_protocol_version": 2,
                        "accepted_frame_types": ["raw_output", "raw_input"],
                        "stream_encoding": "text",
                        "initial_replay": "none"
                    }
                }))
                .unwrap()
            )
            .as_bytes(),
        )
        .unwrap();
    stream
}

fn assert_attach_raw_input_error(stream: &mut UnixStream, code: ControlErrorCode) {
    stream
        .write_all(
            AttachStreamFrame::raw_input(vec![0xff, 0x00, 0x1b])
                .to_json_line()
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let frame = wait_for_attach_frame(&mut reader, |frame| {
        matches!(frame, AttachStreamFrame::Error { .. })
    });
    assert!(
        matches!(&frame, AttachStreamFrame::Error { error } if error.code == code),
        "expected attach error {code:?}, got {frame:?}"
    );
}

fn assert_process_alive(pid: u32, label: &str) {
    assert!(
        kill(Pid::from_raw(pid as i32), None).is_ok(),
        "{label} pid {pid} should still be alive"
    );
}

fn read_attach_response(stream: &UnixStream) -> SessionAttachResponse {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut response_line = String::new();
    reader.read_line(&mut response_line).unwrap();
    let response: ControlResponse = serde_json::from_str(response_line.trim_end()).unwrap();
    assert!(response.ok, "{response:#?}");
    response.result_as().unwrap()
}

fn read_next_attach_frame(reader: &mut BufReader<UnixStream>) -> Option<AttachStreamFrame> {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => Some(AttachStreamFrame::from_json_line(line.trim_end()).unwrap()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            None
        }
        Err(error) => panic!("failed to read attach frame: {error}"),
    }
}

fn wait_for_attach_frame(
    reader: &mut BufReader<UnixStream>,
    predicate: impl Fn(&AttachStreamFrame) -> bool,
) -> AttachStreamFrame {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(10) {
        if let Some(frame) = read_next_attach_frame(reader) {
            if predicate(&frame) {
                return frame;
            }
        }
    }
    panic!("timed out waiting for matching attach frame");
}

fn wait_for_raw_output_contains(reader: &mut BufReader<UnixStream>, needle: &[u8]) -> Vec<u8> {
    let started = Instant::now();
    let mut seen = Vec::new();
    while started.elapsed() < Duration::from_secs(10) {
        if let Some(frame) = read_next_attach_frame(reader) {
            match frame {
                AttachStreamFrame::RawOutput { data } => seen.extend_from_slice(data.as_slice()),
                AttachStreamFrame::Output { text } => seen.extend_from_slice(text.as_bytes()),
                AttachStreamFrame::Error { error } => panic!("attach stream error: {error:?}"),
                AttachStreamFrame::Closed => break,
                _ => {}
            }
            if contains_bytes(&seen, needle) {
                return seen;
            }
        }
    }
    panic!("timed out waiting for raw output {needle:?}; saw {seen:?}");
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn spawn_attach_drain(stream: UnixStream) -> thread::JoinHandle<usize> {
    thread::spawn(move || {
        stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        let mut reader = BufReader::new(stream);
        let started = Instant::now();
        let mut non_closed_frames = 0;
        while started.elapsed() < Duration::from_secs(20) {
            if let Some(frame) = read_next_attach_frame(&mut reader) {
                if matches!(frame, AttachStreamFrame::Closed) {
                    break;
                }
                non_closed_frames += 1;
            }
        }
        non_closed_frames
    })
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

fn start_pipe_session(
    paths: &StatePaths,
    workspace: &Path,
    name: &str,
    script: &str,
) -> SessionStartResponse {
    let response = request_json(
        paths,
        json!({
            "id": format!("start-pipe-{name}"),
            "method": "session.start",
            "params": {
                "name": name,
                "role": "shell",
                "spawn_mode": "pipe",
                "workspace": workspace,
                "cwd": workspace,
                "argv": ["sh", "-c", script]
            }
        }),
    );
    assert_eq!(response["ok"], true, "{response:#}");
    serde_json::from_value(response["result"].clone()).expect("pipe start result")
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
    wait_for_file_contains_timeout(path, needle, Duration::from_secs(5));
}

fn wait_for_file_bytes_contains(path: &Path, needle: &[u8]) {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if fs::read(path)
            .map(|raw| contains_bytes(&raw, needle))
            .unwrap_or(false)
        {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("{} did not contain bytes {needle:?}", path.display());
}

fn wait_for_file_contains_timeout(path: &Path, needle: &str, timeout: Duration) {
    let started = Instant::now();
    while started.elapsed() < timeout {
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
    wait_for_event_kind_timeout(path, kind, Duration::from_secs(5));
}

fn wait_for_event_kind_timeout(path: &Path, kind: SessionEventKind, timeout: Duration) {
    let started = Instant::now();
    while started.elapsed() < timeout {
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
