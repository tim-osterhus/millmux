use std::{collections::BTreeMap, path::PathBuf};

use millrace_sessions_core::{
    ids::{PaneId, SessionId, UiId},
    protocol::{
        AttachFrameType, AttachInitialReplay, AttachReplayMode, AttachStreamEncoding,
        AttachStreamFrame, ControlErrorBody, ControlErrorCode, ControlMethod, ControlRequest,
        ControlResponse, SessionAttachRequest, SessionAttachResponse, SessionListRequest,
        SessionListResponse, SessionSelector, SessionStartRequest, SessionSummary,
        SnapshotUnavailableReason, StreamKind, StreamSetup, TerminalDimensions,
        UiContextGetRequest, UiContextSetRequest, WorkerAttachRequest, WorkerAttachStateResponse,
        WorkerControlMethod, WorkerControlRequest, M1_PROTOCOL_VERSION, M2_ATTACH_PROTOCOL_VERSION,
    },
    state::{
        AttentionState, MonitorProfile, ProcessState, SessionRole, UiContext, UiDaemonHealth,
        UiDaemonRecoveryAction, UiMode,
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
            attached_clients: 0,
            input_owner: None,
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
                    "process_state": "running",
                    "attention_state": "millrace_idle",
                    "workspace": null,
                    "cwd": "/tmp/millmux-workspace",
                    "argv": ["millrace", "run", "daemon"],
                    "monitor_profile": "basic",
                    "created_at": "2026-05-20T18:00:00Z",
                    "updated_at": "2026-05-20T18:01:00Z",
                    "attached_clients": 0,
                    "input_owner": null
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
                "accepted_frame_types": ["raw_output", "snapshot_unavailable"],
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
            AttachFrameType::SnapshotUnavailable
        ]
    );
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
    assert!(!request.accepts_frame_type(AttachFrameType::StreamLagged));
    assert!(!request.accepts_frame_type(AttachFrameType::SnapshotUnavailable));
    assert!(!request.accepts_frame_type(AttachFrameType::ScreenSnapshot));
}

#[test]
fn session_attach_v2_negotiates_only_batch0_implemented_frame_types() {
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
fn worker_attach_replay_modes_decode_legacy_scrollback_boolean() {
    let request = WorkerControlRequest::with_params(
        "worker-attach-1",
        WorkerControlMethod::AcquireAttach,
        &WorkerAttachRequest {
            stream_id: "stream-1".to_string(),
            read_only: false,
            replay: AttachReplayMode::TerminalSnapshot,
            requested_terminal_size: Some(TerminalDimensions { rows: 31, cols: 99 }),
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
                "requested_terminal_size": {"rows": 31, "cols": 99}
            }
        })
    );
    let decoded = request.params_as::<WorkerAttachRequest>().unwrap();
    assert_eq!(
        decoded.requested_terminal_size,
        Some(TerminalDimensions { rows: 31, cols: 99 })
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
        active_daemon_session_id: Some(daemon_id),
        active_workspace: None,
        agent_session_id: None,
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
                    "active_daemon_session_id": "818b61b1-a620-4a57-8e72-4d439d03840f",
                    "active_workspace": null,
                    "agent_session_id": null,
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
