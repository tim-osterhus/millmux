use std::{collections::BTreeMap, path::PathBuf};

use millrace_sessions_core::{
    ids::{PaneId, SessionId, UiId},
    protocol::{
        AttachFrameType, AttachInitialReplay, AttachReplayMode, AttachStreamEncoding,
        AttachStreamFrame, AttachStreamLagReason, ControlErrorBody, ControlErrorCode,
        ControlMethod, ControlRequest, ControlResponse, LogLine, LogStream, ScreenCell,
        ScreenColor, ScreenCursor, ScreenFrame, ScreenSnapshot, ScreenSnapshotSource, ScreenStyle,
        SessionArtifacts, SessionAttachRequest, SessionAttachResponse, SessionCapabilities,
        SessionListRequest, SessionListResponse, SessionScreenResponse, SessionSelector,
        SessionStartRequest, SessionSummary, SnapshotUnavailableReason, StreamKind, StreamSetup,
        TerminalDimensions, UiContextGetRequest, UiContextSetRequest, WorkerAttachRequest,
        WorkerAttachResponse, WorkerAttachStateResponse, WorkerControlMethod, WorkerControlRequest,
        M1_PROTOCOL_VERSION, M2_ATTACH_PROTOCOL_VERSION, MAX_SCREEN_SNAPSHOT_CELLS,
        SCREEN_SNAPSHOT_SCHEMA_VERSION,
    },
    state::{
        AttentionState, MonitorProfile, ProcessState, SessionRole, SpawnMode, UiContext,
        UiDaemonHealth, UiDaemonRecoveryAction, UiMode,
    },
};
use serde_json::json;
use time::macros::datetime;

#[test]
fn session_start_request_matches_m1_jsonl_contract() {
    let params = SessionStartRequest {
        argv: vec![
            "millrace".to_string(),
            "run".to_string(),
            "daemon".to_string(),
        ],
        cwd: Some(PathBuf::from("/tmp/millmux-workspace")),
        workspace: Some(PathBuf::from("/tmp/millmux-workspace")),
        name: Some("daemon".to_string()),
        role: Some(SessionRole::MillraceDaemon),
        spawn_mode: SpawnMode::Pty,
        session_id: None,
        monitor_profile: MonitorProfile::Auto,
        env: BTreeMap::new(),
    };

    let request = ControlRequest::with_params("req_start_1", ControlMethod::SessionStart, &params)
        .expect("params serialize");
    let encoded = request.to_json_line().expect("request serializes");

    assert!(encoded.ends_with('\n'));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(encoded.trim_end()).unwrap(),
        json!({
            "id": "req_start_1",
            "method": "session.start",
            "params": {
                "argv": ["millrace", "run", "daemon"],
                "cwd": "/tmp/millmux-workspace",
                "workspace": "/tmp/millmux-workspace",
                "name": "daemon",
                "role": "millrace_daemon"
            }
        })
    );

    let decoded = ControlRequest::from_json_line(&encoded).expect("request deserializes");
    assert_eq!(decoded.id, "req_start_1");
    assert_eq!(decoded.method, ControlMethod::SessionStart);
    assert_eq!(
        decoded
            .params_as::<SessionStartRequest>()
            .expect("typed params")
            .argv,
        vec!["millrace", "run", "daemon"]
    );
}

#[test]
fn session_start_request_serializes_pipe_spawn_mode_when_requested() {
    let params = SessionStartRequest {
        argv: vec!["sh".to_string(), "-c".to_string(), "echo ready".to_string()],
        cwd: Some(PathBuf::from("/tmp/millmux-workspace")),
        workspace: Some(PathBuf::from("/tmp/millmux-workspace")),
        name: Some("pipe".to_string()),
        role: Some(SessionRole::Shell),
        spawn_mode: SpawnMode::Pipe,
        session_id: None,
        monitor_profile: MonitorProfile::Auto,
        env: BTreeMap::new(),
    };

    let request = ControlRequest::with_params("req_pipe", ControlMethod::SessionStart, &params)
        .expect("params serialize");

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(
            request
                .to_json_line()
                .expect("request serializes")
                .trim_end()
        )
        .unwrap(),
        json!({
            "id": "req_pipe",
            "method": "session.start",
            "params": {
                "argv": ["sh", "-c", "echo ready"],
                "cwd": "/tmp/millmux-workspace",
                "workspace": "/tmp/millmux-workspace",
                "name": "pipe",
                "role": "shell",
                "spawn_mode": "pipe"
            }
        })
    );
}

#[test]
fn session_list_request_and_response_match_m1_jsonl_contract() {
    let request = ControlRequest::with_params(
        "req_list_1",
        ControlMethod::SessionList,
        &SessionListRequest::default(),
    )
    .expect("params serialize");

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(
            request
                .to_json_line()
                .expect("request serializes")
                .trim_end()
        )
        .unwrap(),
        json!({
            "id": "req_list_1",
            "method": "session.list",
            "params": {}
        })
    );

    let session_id: SessionId = "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8".parse().unwrap();
    let result = SessionListResponse {
        schema_version: M1_PROTOCOL_VERSION,
        protocol_version: M1_PROTOCOL_VERSION,
        sessions: vec![SessionSummary {
            session_id,
            name: Some("daemon".to_string()),
            role: SessionRole::MillraceDaemon,
            spawn_mode: SpawnMode::Pty,
            process_state: ProcessState::Running,
            attention_state: AttentionState::MillraceIdle,
            failure_message: None,
            workspace: None,
            cwd: PathBuf::from("/tmp/millmux-workspace"),
            argv: vec![
                "millrace".to_string(),
                "run".to_string(),
                "daemon".to_string(),
            ],
            monitor_profile: MonitorProfile::Basic,
            created_at: "2026-05-20T18:00:00Z".to_string(),
            updated_at: "2026-05-20T18:01:00Z".to_string(),
            stop_requested_at: None,
            stop_reason: None,
            attached_clients: 0,
            input_owner: None,
            capabilities: SessionCapabilities::for_spawn_mode(SpawnMode::Pty),
            artifacts: SessionArtifacts::default(),
            liveness: Default::default(),
        }],
    };

    let response = ControlResponse::success("req_list_1", &result).expect("result serializes");
    let encoded = response.to_json_line().expect("response serializes");

    assert!(encoded.ends_with('\n'));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(encoded.trim_end()).unwrap(),
        json!({
            "id": "req_list_1",
            "ok": true,
            "result": {
                "schema_version": 1,
                "protocol_version": 1,
                "sessions": [{
                    "session_id": "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8",
                    "name": "daemon",
                    "role": "millrace_daemon",
                    "spawn_mode": "pty",
                    "process_state": "running",
                    "attention_state": "millrace_idle",
                    "workspace": null,
                    "cwd": "/tmp/millmux-workspace",
                    "argv": ["millrace", "run", "daemon"],
                    "monitor_profile": "basic",
                    "created_at": "2026-05-20T18:00:00Z",
                    "updated_at": "2026-05-20T18:01:00Z",
                    "attached_clients": 0,
                    "input_owner": null,
                    "capabilities": {
                        "schema_version": 1,
                        "attach": true,
                        "raw_attach": true,
                        "send": true,
                        "resize": true,
                        "screen": true
                    },
                    "artifacts": {
                        "schema_version": 1
                    },
                    "liveness": {
                        "schema_version": 1,
                        "worker": "unknown",
                        "child": "unknown"
                    }
                }]
            }
        })
    );

    let decoded = ControlResponse::from_json_line(&encoded).expect("response deserializes");
    let decoded_result = decoded
        .result_as::<SessionListResponse>()
        .expect("typed result");
    assert_eq!(decoded_result.sessions[0].session_id, session_id);
}

#[test]
fn legacy_log_line_without_stream_defaults_to_pty() {
    let line: LogLine =
        serde_json::from_value(json!({"timestamp": "2026-05-20T18:00:00Z", "line": "ready"}))
            .unwrap();

    assert_eq!(line.stream, LogStream::Pty);
    assert_eq!(line.line, "ready");
}

#[test]
fn session_artifacts_contract_versions_pty_and_pipe_shapes() {
    let paths = millrace_sessions_core::state::SessionPaths {
        root: PathBuf::from("/state/sessions/id"),
        meta_json: PathBuf::from("/state/sessions/id/meta.json"),
        worker_json: PathBuf::from("/state/sessions/id/worker.json"),
        pty_log: PathBuf::from("/state/sessions/id/pty.log"),
        stdout_log: PathBuf::from("/state/sessions/id/stdout.log"),
        stderr_log: PathBuf::from("/state/sessions/id/stderr.log"),
        events_jsonl: PathBuf::from("/state/sessions/id/events.jsonl"),
        scrollback_snapshot: PathBuf::from("/state/sessions/id/scrollback.snapshot"),
        terminal_snapshot: PathBuf::from("/state/sessions/id/terminal.snapshot.json"),
        raw_replay_ring: PathBuf::from("/state/sessions/id/pty.replay"),
        worker_sock: PathBuf::from("/state/sessions/id/worker.sock"),
    };

    assert_eq!(
        serde_json::to_value(SessionArtifacts::for_paths(SpawnMode::Pty, &paths)).unwrap(),
        json!({
            "schema_version": 1,
            "pty": {
                "pty_log": "/state/sessions/id/pty.log",
                "scrollback_snapshot": "/state/sessions/id/scrollback.snapshot",
                "terminal_snapshot": "/state/sessions/id/terminal.snapshot.json",
                "raw_replay_ring": "/state/sessions/id/pty.replay"
            }
        })
    );
    assert_eq!(
        serde_json::to_value(SessionArtifacts::for_paths(SpawnMode::Pipe, &paths)).unwrap(),
        json!({
            "schema_version": 1,
            "pipe": {
                "stdout_log": "/state/sessions/id/stdout.log",
                "stderr_log": "/state/sessions/id/stderr.log"
            }
        })
    );
    assert_eq!(
        serde_json::to_value(SessionCapabilities::for_spawn_mode(SpawnMode::Pipe)).unwrap(),
        json!({
            "schema_version": 1,
            "attach": false,
            "raw_attach": false,
            "send": false,
            "resize": false,
            "screen": false
        })
    );
}

#[test]
fn session_attach_replay_modes_replace_legacy_scrollback_boolean() {
    let session_id: SessionId = "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8".parse().unwrap();
    let params = SessionAttachRequest {
        selector: SessionSelector::Id { session_id },
        read_only: true,
        replay: AttachReplayMode::LineScrollback,
        requested_terminal_size: None,
        client_protocol_version: None,
        accepted_frame_types: Vec::new(),
        stream_encoding: None,
        initial_replay: None,
    };

    let request = ControlRequest::with_params("attach-1", ControlMethod::SessionAttach, &params)
        .expect("params serialize");

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(request.to_json_line().unwrap().trim_end())
            .unwrap(),
        json!({
            "id": "attach-1",
            "method": "session.attach",
            "params": {
                "selector": {"type": "id", "session_id": "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8"},
                "read_only": true,
                "replay": "line_scrollback"
            }
        })
    );

    let legacy: SessionAttachRequest = serde_json::from_value(json!({
        "selector": {"type": "id", "session_id": "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8"},
        "include_scrollback": true
    }))
    .expect("legacy attach params deserialize");
    assert_eq!(legacy.replay, AttachReplayMode::LineScrollback);
    assert_eq!(legacy.requested_terminal_size, None);

    let legacy_no_replay: SessionAttachRequest = serde_json::from_value(json!({
        "selector": {"type": "id", "session_id": "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8"},
        "include_scrollback": false
    }))
    .expect("legacy no-scrollback params deserialize");
    assert_eq!(legacy_no_replay.replay, AttachReplayMode::None);
    assert_eq!(legacy_no_replay.requested_terminal_size, None);

    let default_no_replay: SessionAttachRequest = serde_json::from_value(json!({
        "selector": {"type": "id", "session_id": "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8"}
    }))
    .expect("default attach params deserialize");
    assert_eq!(default_no_replay.replay, AttachReplayMode::None);
    assert_eq!(default_no_replay.requested_terminal_size, None);
}

#[test]
fn session_attach_terminal_snapshot_carries_requested_size() {
    let session_id: SessionId = "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8".parse().unwrap();
    let params = SessionAttachRequest {
        selector: SessionSelector::Id { session_id },
        read_only: false,
        replay: AttachReplayMode::TerminalSnapshot,
        requested_terminal_size: Some(TerminalDimensions { rows: 31, cols: 99 }),
        client_protocol_version: None,
        accepted_frame_types: Vec::new(),
        stream_encoding: None,
        initial_replay: None,
    };

    let request =
        ControlRequest::with_params("attach-sized", ControlMethod::SessionAttach, &params)
            .expect("params serialize");

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(request.to_json_line().unwrap().trim_end())
            .unwrap(),
        json!({
            "id": "attach-sized",
            "method": "session.attach",
            "params": {
                "selector": {"type": "id", "session_id": "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8"},
                "replay": "terminal_snapshot",
                "requested_terminal_size": {"rows": 31, "cols": 99}
            }
        })
    );

    let decoded = request.params_as::<SessionAttachRequest>().unwrap();
    assert_eq!(
        decoded.requested_terminal_size,
        Some(TerminalDimensions { rows: 31, cols: 99 })
    );
}

#[test]
fn session_attach_v2_negotiation_fields_are_additive() {
    let session_id: SessionId = "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8".parse().unwrap();
    let params = SessionAttachRequest {
        selector: SessionSelector::Id { session_id },
        read_only: true,
        replay: AttachReplayMode::TerminalSnapshot,
        requested_terminal_size: Some(TerminalDimensions {
            rows: 40,
            cols: 120,
        }),
        client_protocol_version: Some(M2_ATTACH_PROTOCOL_VERSION),
        accepted_frame_types: vec![
            AttachFrameType::RawOutput,
            AttachFrameType::ScreenSnapshot,
            AttachFrameType::SnapshotUnavailable,
        ],
        stream_encoding: Some(AttachStreamEncoding::RawBytes),
        initial_replay: Some(AttachInitialReplay::ScreenSnapshot),
    };

    let request = ControlRequest::with_params("attach-v2", ControlMethod::SessionAttach, &params)
        .expect("params serialize");

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(request.to_json_line().unwrap().trim_end())
            .unwrap(),
        json!({
            "id": "attach-v2",
            "method": "session.attach",
            "params": {
                "selector": {"type": "id", "session_id": "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8"},
                "read_only": true,
                "replay": "terminal_snapshot",
                "requested_terminal_size": {"rows": 40, "cols": 120},
                "client_protocol_version": 2,
                "accepted_frame_types": ["raw_output", "screen_snapshot", "snapshot_unavailable"],
                "stream_encoding": "raw_bytes",
                "initial_replay": "screen_snapshot"
            }
        })
    );

    let decoded = request.params_as::<SessionAttachRequest>().unwrap();
    assert_eq!(
        decoded.negotiated_attach_protocol_version(),
        Some(M2_ATTACH_PROTOCOL_VERSION)
    );
    assert_eq!(
        decoded.negotiated_stream_encoding(),
        Some(AttachStreamEncoding::RawBytes)
    );
    assert_eq!(
        decoded.negotiated_initial_replay(),
        Some(AttachInitialReplay::ScreenSnapshot)
    );
    assert_eq!(
        decoded.negotiated_frame_types(),
        vec![
            AttachFrameType::RawOutput,
            AttachFrameType::ScreenSnapshot,
            AttachFrameType::SnapshotUnavailable
        ]
    );
    assert!(decoded.accepts_frame_type(AttachFrameType::ScreenSnapshot));
    assert!(decoded.accepts_frame_type(AttachFrameType::SnapshotUnavailable));
    assert!(!decoded.accepts_frame_type(AttachFrameType::StreamLagged));
}

#[test]
fn session_attach_v2_missing_accepted_frame_type_suppresses_that_frame() {
    let session_id: SessionId = "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8".parse().unwrap();
    let request: SessionAttachRequest = serde_json::from_value(json!({
        "selector": {"type": "id", "session_id": session_id},
        "client_protocol_version": 2,
        "accepted_frame_types": ["raw_output"],
        "stream_encoding": "raw_bytes",
        "initial_replay": "none"
    }))
    .expect("v2 attach params deserialize");

    assert!(request.accepts_frame_type(AttachFrameType::RawOutput));
    assert!(!request.accepts_frame_type(AttachFrameType::RawInput));
    assert!(!request.accepts_frame_type(AttachFrameType::StreamLagged));
    assert!(!request.accepts_frame_type(AttachFrameType::SnapshotUnavailable));
    assert!(!request.accepts_frame_type(AttachFrameType::ScreenSnapshot));
}

#[test]
fn session_attach_v2_negotiates_raw_input_only_for_writable_raw_streams() {
    let session_id: SessionId = "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8".parse().unwrap();
    let writable: SessionAttachRequest = serde_json::from_value(json!({
        "selector": {"type": "id", "session_id": session_id},
        "client_protocol_version": 2,
        "accepted_frame_types": ["raw_output", "raw_input"],
        "stream_encoding": "raw_bytes",
        "initial_replay": "none"
    }))
    .expect("writable v2 attach params deserialize");

    assert_eq!(
        writable.negotiated_frame_types(),
        vec![AttachFrameType::RawOutput, AttachFrameType::RawInput]
    );

    let read_only: SessionAttachRequest = serde_json::from_value(json!({
        "selector": {"type": "id", "session_id": session_id},
        "read_only": true,
        "client_protocol_version": 2,
        "accepted_frame_types": ["raw_output", "raw_input"],
        "stream_encoding": "raw_bytes",
        "initial_replay": "none"
    }))
    .expect("read-only v2 attach params deserialize");

    assert_eq!(
        read_only.negotiated_frame_types(),
        vec![AttachFrameType::RawOutput]
    );
}

#[test]
fn session_attach_v2_negotiates_batch1_stream_lagged_frame_type() {
    let session_id: SessionId = "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8".parse().unwrap();
    let request: SessionAttachRequest = serde_json::from_value(json!({
        "selector": {"type": "id", "session_id": session_id},
        "client_protocol_version": 2,
        "accepted_frame_types": [
            "raw_output",
            "stream_lagged",
            "snapshot_unavailable",
            "screen_snapshot"
        ],
        "stream_encoding": "raw_bytes",
        "initial_replay": "screen_snapshot"
    }))
    .expect("v2 attach params deserialize");

    assert!(request.accepts_frame_type(AttachFrameType::StreamLagged));
    assert!(request.accepts_frame_type(AttachFrameType::ScreenSnapshot));
    assert_eq!(
        request.negotiated_frame_types(),
        vec![
            AttachFrameType::RawOutput,
            AttachFrameType::StreamLagged,
            AttachFrameType::ScreenSnapshot,
            AttachFrameType::SnapshotUnavailable
        ]
    );
}

#[test]
fn session_attach_v2_raw_replay_requires_raw_output_frame_acceptance() {
    let session_id: SessionId = "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8".parse().unwrap();
    let request: SessionAttachRequest = serde_json::from_value(json!({
        "selector": {"type": "id", "session_id": session_id},
        "client_protocol_version": 2,
        "accepted_frame_types": [],
        "replay": "terminal_snapshot",
        "initial_replay": "raw_replay"
    }))
    .expect("v2 attach params deserialize");

    assert_eq!(request.negotiated_initial_replay(), None);
    assert!(request.negotiated_frame_types().is_empty());
}

#[test]
fn session_attach_v2_raw_stream_encoding_requires_raw_output_frame_acceptance() {
    let session_id: SessionId = "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8".parse().unwrap();
    let request: SessionAttachRequest = serde_json::from_value(json!({
        "selector": {"type": "id", "session_id": session_id},
        "client_protocol_version": 2,
        "accepted_frame_types": [],
        "stream_encoding": "raw_bytes"
    }))
    .expect("v2 attach params deserialize");

    assert_eq!(
        request.negotiated_stream_encoding(),
        Some(AttachStreamEncoding::Text)
    );
    assert!(request.negotiated_frame_types().is_empty());
}

#[test]
fn session_attach_v2_initial_replay_none_overrides_legacy_replay() {
    let session_id: SessionId = "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8".parse().unwrap();
    let request: SessionAttachRequest = serde_json::from_value(json!({
        "selector": {"type": "id", "session_id": session_id},
        "client_protocol_version": 2,
        "accepted_frame_types": ["raw_output"],
        "replay": "terminal_snapshot",
        "initial_replay": "none"
    }))
    .expect("v2 attach params deserialize");

    assert_eq!(
        request.negotiated_initial_replay(),
        Some(AttachInitialReplay::None)
    );
    assert!(request.negotiated_frame_types().is_empty());
}

#[test]
fn session_attach_response_negotiation_fields_are_omitted_for_v1() {
    let session_id: SessionId = "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8".parse().unwrap();
    let response = SessionAttachResponse {
        schema_version: M1_PROTOCOL_VERSION,
        protocol_version: M1_PROTOCOL_VERSION,
        session_id,
        stream: StreamSetup {
            stream_id: "attach-1".to_string(),
            kind: StreamKind::Attach,
            read_only: false,
            input_owner: true,
        },
        negotiated_attach_protocol_version: None,
        negotiated_stream_encoding: None,
        negotiated_initial_replay: None,
        accepted_frame_types: Vec::new(),
    };

    let value = serde_json::to_value(&response).expect("response serializes");
    assert_eq!(value["protocol_version"], M1_PROTOCOL_VERSION);
    assert!(value.get("negotiated_attach_protocol_version").is_none());
    assert!(value.get("negotiated_stream_encoding").is_none());
    assert!(value.get("negotiated_initial_replay").is_none());
    assert!(value.get("accepted_frame_types").is_none());

    let decoded: SessionAttachResponse = serde_json::from_value(value).unwrap();
    assert_eq!(decoded.negotiated_attach_protocol_version, None);
    assert!(decoded.accepted_frame_types.is_empty());
}

#[test]
fn snapshot_unavailable_frame_has_minimal_v2_envelope() {
    let frame = AttachStreamFrame::SnapshotUnavailable {
        reason: SnapshotUnavailableReason::SizeMismatch,
        details: Some(json!({
            "requested_rows": 40,
            "requested_cols": 120,
            "snapshot_rows": 24,
            "snapshot_cols": 80
        })),
    };

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&frame.to_json_line().unwrap()).unwrap(),
        json!({
            "type": "snapshot_unavailable",
            "reason": "size_mismatch",
            "details": {
                "requested_rows": 40,
                "requested_cols": 120,
                "snapshot_rows": 24,
                "snapshot_cols": 80
            }
        })
    );
}

#[test]
fn snapshot_unavailable_reasons_are_enumerated_contract_values() {
    let reasons = [
        (SnapshotUnavailableReason::NoSnapshot, "no_snapshot"),
        (SnapshotUnavailableReason::StaleSnapshot, "stale_snapshot"),
        (SnapshotUnavailableReason::SizeMismatch, "size_mismatch"),
        (
            SnapshotUnavailableReason::UnsupportedSpawnMode,
            "unsupported_spawn_mode",
        ),
        (
            SnapshotUnavailableReason::PayloadTooLarge,
            "payload_too_large",
        ),
        (
            SnapshotUnavailableReason::TerminalModelUnavailable,
            "terminal_model_unavailable",
        ),
        (
            SnapshotUnavailableReason::PermissionDenied,
            "permission_denied",
        ),
        (SnapshotUnavailableReason::InternalError, "internal_error"),
    ];

    for (reason, wire) in reasons {
        assert_eq!(
            serde_json::to_string(&reason).unwrap(),
            format!("\"{wire}\"")
        );
        assert_eq!(
            serde_json::from_str::<SnapshotUnavailableReason>(&format!("\"{wire}\"")).unwrap(),
            reason
        );
    }
}

#[test]
fn screen_snapshot_attach_frame_uses_flattened_v1_schema() {
    let mut wide = ScreenCell::default_symbol("界");
    wide.width = 2;
    wide.fg = ScreenColor::Indexed { index: 2 };
    wide.bg = ScreenColor::Rgb {
        r: 10,
        g: 20,
        b: 30,
    };
    wide.style = ScreenStyle {
        bold: true,
        dim: false,
        italic: true,
        underline: true,
        inverse: false,
    };
    let mut continuation = ScreenCell::blank();
    continuation.continuation = true;

    let frame = AttachStreamFrame::screen_snapshot(ScreenSnapshot {
        schema_version: SCREEN_SNAPSHOT_SCHEMA_VERSION,
        rows: 1,
        cols: 3,
        cursor: ScreenCursor {
            row: 0,
            col: 2,
            visible: Some(true),
        },
        alternate_screen: true,
        cells: vec![vec![wide, continuation, ScreenCell::blank()]],
        source: ScreenSnapshotSource {
            pty_log_offset: 123456,
            raw_replay_start_offset: 122000,
            raw_replay_end_offset: 123456,
        },
        captured_at: "2026-07-08T00:00:00Z".to_string(),
    })
    .expect("snapshot is within limits");

    let frame_line = frame.to_json_line().unwrap();
    assert_eq!(
        AttachStreamFrame::from_json_line(&frame_line).unwrap(),
        frame
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&frame_line).unwrap(),
        json!({
            "type": "screen_snapshot",
            "schema_version": 1,
            "rows": 1,
            "cols": 3,
            "cursor": {"row": 0, "col": 2, "visible": true},
            "alternate_screen": true,
            "cells": [[
                {
                    "symbol": "界",
                    "width": 2,
                    "fg": {"type": "indexed", "index": 2},
                    "bg": {"type": "rgb", "r": 10, "g": 20, "b": 30},
                    "style": {
                        "bold": true,
                        "italic": true,
                        "underline": true
                    }
                },
                {
                    "symbol": " ",
                    "continuation": true
                },
                {
                    "symbol": " "
                }
            ]],
            "source": {
                "pty_log_offset": 123456,
                "raw_replay_start_offset": 122000,
                "raw_replay_end_offset": 123456
            },
            "captured_at": "2026-07-08T00:00:00Z"
        })
    );
}

#[test]
fn screen_snapshot_validation_enforces_cell_and_payload_bounds() {
    let too_many_cells = ScreenSnapshot {
        schema_version: SCREEN_SNAPSHOT_SCHEMA_VERSION,
        rows: 400,
        cols: 400,
        cursor: ScreenCursor {
            row: 0,
            col: 0,
            visible: Some(true),
        },
        alternate_screen: false,
        cells: Vec::new(),
        source: ScreenSnapshotSource {
            pty_log_offset: 0,
            raw_replay_start_offset: 0,
            raw_replay_end_offset: 0,
        },
        captured_at: "2026-07-08T00:00:00Z".to_string(),
    };
    let error = too_many_cells.validate_for_wire().unwrap_err();
    assert_eq!(
        error.unavailable_reason(),
        SnapshotUnavailableReason::PayloadTooLarge
    );
    assert_eq!(
        error.unavailable_details().unwrap()["max_cells"],
        MAX_SCREEN_SNAPSHOT_CELLS
    );

    let mut huge = ScreenCell::blank();
    huge.symbol = "x".repeat(5 * 1024 * 1024);
    let huge_snapshot = ScreenSnapshot {
        schema_version: SCREEN_SNAPSHOT_SCHEMA_VERSION,
        rows: 1,
        cols: 1,
        cursor: ScreenCursor {
            row: 0,
            col: 0,
            visible: Some(true),
        },
        alternate_screen: false,
        cells: vec![vec![huge]],
        source: ScreenSnapshotSource {
            pty_log_offset: 0,
            raw_replay_start_offset: 0,
            raw_replay_end_offset: 0,
        },
        captured_at: "2026-07-08T00:00:00Z".to_string(),
    };
    let frame = AttachStreamFrame::screen_snapshot(huge_snapshot).unwrap_err();
    assert_eq!(
        frame.unavailable_reason(),
        SnapshotUnavailableReason::PayloadTooLarge
    );
    assert!(
        frame.unavailable_details().unwrap()["serialized_bytes"]
            .as_u64()
            .unwrap()
            > 4 * 1024 * 1024
    );
}

#[test]
fn session_screen_response_uses_structured_screen_snapshot_frame() {
    let session_id: SessionId = "818b61b1-a620-4a57-8e72-4d439d03840f".parse().unwrap();
    let response = SessionScreenResponse {
        protocol_version: M1_PROTOCOL_VERSION,
        session_id,
        frame: ScreenFrame::ScreenSnapshot {
            snapshot: ScreenSnapshot {
                schema_version: SCREEN_SNAPSHOT_SCHEMA_VERSION,
                rows: 1,
                cols: 2,
                cursor: ScreenCursor {
                    row: 0,
                    col: 1,
                    visible: Some(true),
                },
                alternate_screen: false,
                cells: vec![vec![
                    ScreenCell::default_symbol("o"),
                    ScreenCell::default_symbol("k"),
                ]],
                source: ScreenSnapshotSource {
                    pty_log_offset: 7,
                    raw_replay_start_offset: 0,
                    raw_replay_end_offset: 7,
                },
                captured_at: "2026-07-08T00:00:00Z".to_string(),
            },
        },
    };

    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(value["type"], "screen_snapshot");
    assert_eq!(value["cells"][0][0]["symbol"], "o");
    assert!(
        value["cells"][0][0].get("fg").is_none(),
        "default foreground is omitted from compact screen cells"
    );
    assert_eq!(value["source"]["pty_log_offset"], 7);
    assert!(
        value.get("data").is_none(),
        "screen is not a raw_output frame"
    );
}

#[test]
fn stream_lagged_frame_has_batch1_recovery_envelope() {
    let frame = AttachStreamFrame::StreamLagged {
        dropped_bytes: 4096,
        dropped_from_offset: 128,
        dropped_to_offset: 4224,
        current_pty_log_offset: 8192,
        reason: AttachStreamLagReason::ObserverBackpressure,
        recover: "request_screen_or_reattach_raw_replay".to_string(),
    };

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&frame.to_json_line().unwrap()).unwrap(),
        json!({
            "type": "stream_lagged",
            "dropped_bytes": 4096,
            "dropped_from_offset": 128,
            "dropped_to_offset": 4224,
            "current_pty_log_offset": 8192,
            "reason": "observer_backpressure",
            "recover": "request_screen_or_reattach_raw_replay"
        })
    );
}

#[test]
fn worker_attach_replay_modes_decode_legacy_scrollback_boolean() {
    let request = WorkerControlRequest::with_params(
        "worker-attach-1",
        WorkerControlMethod::AcquireAttach,
        &WorkerAttachRequest {
            stream_id: "stream-1".to_string(),
            read_only: false,
            replay: AttachReplayMode::TerminalSnapshot,
            requested_terminal_size: Some(TerminalDimensions { rows: 31, cols: 99 }),
            client_protocol_version: Some(M2_ATTACH_PROTOCOL_VERSION),
            accepted_frame_types: vec![AttachFrameType::RawOutput, AttachFrameType::StreamLagged],
            stream_encoding: Some(AttachStreamEncoding::RawBytes),
            initial_replay: Some(AttachInitialReplay::RawReplay),
        },
    )
    .expect("worker request serializes");

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(request.to_json_line().unwrap().trim_end())
            .unwrap(),
        json!({
            "id": "worker-attach-1",
            "method": "acquire_attach",
            "params": {
                "stream_id": "stream-1",
                "replay": "terminal_snapshot",
                "requested_terminal_size": {"rows": 31, "cols": 99},
                "client_protocol_version": 2,
                "accepted_frame_types": ["raw_output", "stream_lagged"],
                "stream_encoding": "raw_bytes",
                "initial_replay": "raw_replay"
            }
        })
    );
    let decoded = request.params_as::<WorkerAttachRequest>().unwrap();
    assert_eq!(
        decoded.requested_terminal_size,
        Some(TerminalDimensions { rows: 31, cols: 99 })
    );
    assert_eq!(
        decoded.negotiated_frame_types(),
        vec![AttachFrameType::RawOutput, AttachFrameType::StreamLagged]
    );

    let legacy: WorkerAttachRequest = serde_json::from_value(json!({
        "stream_id": "stream-legacy",
        "include_scrollback": true
    }))
    .expect("legacy worker attach params deserialize");
    assert_eq!(legacy.replay, AttachReplayMode::LineScrollback);
    assert_eq!(legacy.requested_terminal_size, None);
}

#[test]
fn worker_attach_response_echoes_optional_negotiated_axes() {
    let response = WorkerAttachResponse {
        stream_id: "stream-1".to_string(),
        read_only: false,
        input_owner: true,
        negotiated_attach_protocol_version: Some(M2_ATTACH_PROTOCOL_VERSION),
        negotiated_stream_encoding: Some(AttachStreamEncoding::RawBytes),
        negotiated_initial_replay: Some(AttachInitialReplay::RawReplay),
        accepted_frame_types: vec![AttachFrameType::RawOutput, AttachFrameType::StreamLagged],
    };

    assert_eq!(
        serde_json::to_value(&response).unwrap(),
        json!({
            "stream_id": "stream-1",
            "read_only": false,
            "input_owner": true,
            "negotiated_attach_protocol_version": 2,
            "negotiated_stream_encoding": "raw_bytes",
            "negotiated_initial_replay": "raw_replay",
            "accepted_frame_types": ["raw_output", "stream_lagged"]
        })
    );

    let legacy: WorkerAttachResponse = serde_json::from_value(json!({
        "stream_id": "stream-old",
        "read_only": true,
        "input_owner": false
    }))
    .expect("legacy worker attach response deserializes");
    assert_eq!(legacy.negotiated_attach_protocol_version, None);
    assert_eq!(legacy.negotiated_stream_encoding, None);
    assert_eq!(legacy.negotiated_initial_replay, None);
    assert!(legacy.accepted_frame_types.is_empty());
}

#[test]
fn worker_attach_state_exposes_active_clients_and_input_owner() {
    let request = WorkerControlRequest::with_params(
        "worker-attach-state-1",
        WorkerControlMethod::AttachState,
        &json!({}),
    )
    .expect("worker request serializes");

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(request.to_json_line().unwrap().trim_end())
            .unwrap(),
        json!({
            "id": "worker-attach-state-1",
            "method": "attach_state",
            "params": {}
        })
    );

    let result = WorkerAttachStateResponse {
        attached_clients: 2,
        input_owner: Some("stream-owner".to_string()),
    };
    let encoded = serde_json::to_value(&result).expect("state result serializes");
    assert_eq!(
        encoded,
        json!({
            "attached_clients": 2,
            "input_owner": "stream-owner"
        })
    );
}

#[test]
fn raw_attach_output_frame_serializes_bytes_as_base64() {
    let frame = AttachStreamFrame::raw_output(vec![0x00, 0xff, b'A']);
    let encoded = frame.to_json_line().expect("frame serializes");

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(encoded.trim_end()).unwrap(),
        json!({
            "type": "raw_output",
            "data": "AP9B"
        })
    );
    assert!(matches!(
        AttachStreamFrame::from_json_line(&encoded).expect("frame deserializes"),
        AttachStreamFrame::RawOutput { data } if data.as_slice() == [0x00, 0xff, b'A']
    ));
}

#[test]
fn raw_attach_input_frame_serializes_bytes_as_base64() {
    let frame = AttachStreamFrame::raw_input(vec![0xff, 0x00, 0x1b, b'[', b'A']);
    let encoded = frame.to_json_line().expect("frame serializes");

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(encoded.trim_end()).unwrap(),
        json!({
            "type": "raw_input",
            "data": "/wAbW0E="
        })
    );
    assert!(matches!(
        AttachStreamFrame::from_json_line(&encoded).expect("frame deserializes"),
        AttachStreamFrame::RawInput { data } if data.as_slice() == [0xff, 0x00, 0x1b, b'[', b'A']
    ));
}

#[test]
fn duplicate_daemon_error_matches_m1_jsonl_contract() {
    let error = ControlErrorBody::new(
        ControlErrorCode::DuplicateMillraceDaemon,
        "a millrace-daemon session already exists for this workspace",
    )
    .with_details(json!({
        "workspace": "/tmp/millmux-workspace",
        "session_id": "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8"
    }));

    let response = ControlResponse::failure("req_start_2", error);
    let encoded = response.to_json_line().expect("response serializes");

    assert!(encoded.ends_with('\n'));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(encoded.trim_end()).unwrap(),
        json!({
            "id": "req_start_2",
            "ok": false,
            "error": {
                "code": "duplicate_millrace_daemon",
                "message": "a millrace-daemon session already exists for this workspace",
                "details": {
                    "workspace": "/tmp/millmux-workspace",
                    "session_id": "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8"
                }
            }
        })
    );

    let decoded = ControlResponse::from_json_line(&encoded).expect("response deserializes");
    let decoded_error = decoded.error.expect("error body");
    assert_eq!(
        decoded_error.code,
        ControlErrorCode::DuplicateMillraceDaemon
    );
}

#[test]
fn ui_context_matches_m2a_jsonl_contract() {
    let ui_id: UiId = "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8".parse().unwrap();
    let pane_id: PaneId = "2d14ac17-d5c9-43aa-a6f2-9414b3c16285".parse().unwrap();
    let daemon_id: SessionId = "818b61b1-a620-4a57-8e72-4d439d03840f".parse().unwrap();
    let context = UiContext {
        schema_version: M1_PROTOCOL_VERSION,
        ui_id,
        mode: UiMode::DaemonConsole,
        active_pane_id: Some(pane_id),
        selected_session_id: Some(daemon_id),
        focused_session_id: Some(daemon_id),
        focused_pane_kind: Some("daemon_monitor".to_string()),
        active_daemon_session_id: Some(daemon_id),
        active_workspace: None,
        agent_session_id: None,
        managed_session_ids: vec![daemon_id],
        managed_daemon_session_ids: vec![daemon_id],
        monitor_profile: MonitorProfile::Basic,
        daemon_health: vec![UiDaemonHealth {
            session_id: daemon_id,
            process_state: ProcessState::FailedStart,
            attention_state: AttentionState::NeedsAttention,
            failure_message: Some("failed to spawn pty child: not found".to_string()),
            recovery_actions: vec![
                UiDaemonRecoveryAction::Inspect,
                UiDaemonRecoveryAction::Logs,
                UiDaemonRecoveryAction::Doctor,
                UiDaemonRecoveryAction::Delete,
            ],
        }],
        updated_at: datetime!(2026-05-26 04:00:00 UTC),
    };

    let request = ControlRequest::with_params(
        "ui-set-1",
        ControlMethod::UiContextSet,
        &UiContextSetRequest {
            context,
            events: Vec::new(),
        },
    )
    .expect("params serialize");

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(
            request
                .to_json_line()
                .expect("request serializes")
                .trim_end()
        )
        .unwrap(),
        json!({
            "id": "ui-set-1",
            "method": "ui.context.set",
            "params": {
                "context": {
                    "schema_version": 1,
                    "ui_id": "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8",
                    "mode": "daemon_console",
                    "active_pane_id": "2d14ac17-d5c9-43aa-a6f2-9414b3c16285",
                    "selected_session_id": "818b61b1-a620-4a57-8e72-4d439d03840f",
                    "focused_session_id": "818b61b1-a620-4a57-8e72-4d439d03840f",
                    "focused_pane_kind": "daemon_monitor",
                    "active_daemon_session_id": "818b61b1-a620-4a57-8e72-4d439d03840f",
                    "active_workspace": null,
                    "agent_session_id": null,
                    "managed_session_ids": [
                        "818b61b1-a620-4a57-8e72-4d439d03840f"
                    ],
                    "managed_daemon_session_ids": [
                        "818b61b1-a620-4a57-8e72-4d439d03840f"
                    ],
                    "monitor_profile": "basic",
                    "daemon_health": [{
                        "session_id": "818b61b1-a620-4a57-8e72-4d439d03840f",
                        "process_state": "failed_start",
                        "attention_state": "needs_attention",
                        "failure_message": "failed to spawn pty child: not found",
                        "recovery_actions": [
                            "inspect",
                            "logs",
                            "doctor",
                            "delete"
                        ]
                    }],
                    "updated_at": "2026-05-26T04:00:00Z"
                }
            }
        })
    );

    let get = ControlRequest::with_params(
        "ui-get-1",
        ControlMethod::UiContextGet,
        &UiContextGetRequest { ui_id: Some(ui_id) },
    )
    .expect("params serialize");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(get.to_json_line().unwrap().trim_end()).unwrap(),
        json!({
            "id": "ui-get-1",
            "method": "ui.context.get",
            "params": {
                "ui_id": "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8"
            }
        })
    );
}
