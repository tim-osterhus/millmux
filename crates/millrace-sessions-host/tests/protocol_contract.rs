use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

use millrace_sessions_core::{
    events::{read_events, SessionEventKind},
    ids::{PaneId, SessionId, UiId},
    paths::{StatePaths, STATE_DIR_ENV},
    protocol::{
        AttachFrameType, AttachInitialReplay, AttachStreamEncoding, ControlErrorCode, ScreenCell,
        ScreenCursor, ScreenSnapshot, ScreenSnapshotSource, SessionAttachRequest,
        SessionAttachResponse, SessionInspectResponse, SessionListResponse, SessionScreenResponse,
        SessionSelector, StreamKind, StreamSetup, TerminalDimensions, M1_PROTOCOL_VERSION,
        M2_ATTACH_PROTOCOL_VERSION, SCREEN_SNAPSHOT_SCHEMA_VERSION,
    },
    scrollback::TerminalSnapshot,
    state::{
        AttentionState, HostMeta, MonitorProfile, ProcessState, SessionMeta, SessionRole, SpawnMode,
    },
    storage::{read_json, read_json_lines, write_json_atomic},
    workspace::WorkspaceIdentity,
};
use serde_json::{json, Value};

use millrace_sessions_host::server::dispatch_json_line;

#[test]
fn protocol_contract_v1_attach_response_omits_negotiation_fields() {
    let session_id: SessionId = "818b61b1-a620-4a57-8e72-4d439d03840f".parse().unwrap();
    let response = SessionAttachResponse {
        schema_version: M1_PROTOCOL_VERSION,
        protocol_version: M1_PROTOCOL_VERSION,
        session_id,
        stream: StreamSetup {
            stream_id: "attach-v1".to_string(),
            kind: StreamKind::Attach,
            read_only: false,
            input_owner: true,
        },
        negotiated_attach_protocol_version: None,
        negotiated_stream_encoding: None,
        negotiated_initial_replay: None,
        accepted_frame_types: Vec::new(),
    };

    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(
        value,
        json!({
            "schema_version": 1,
            "protocol_version": 1,
            "session_id": "818b61b1-a620-4a57-8e72-4d439d03840f",
            "stream": {
                "stream_id": "attach-v1",
                "kind": "attach",
                "read_only": false,
                "input_owner": true
            }
        })
    );
}

#[test]
fn protocol_contract_v2_attach_response_confirms_negotiated_axes() {
    let session_id: SessionId = "818b61b1-a620-4a57-8e72-4d439d03840f".parse().unwrap();
    let response = SessionAttachResponse {
        schema_version: M1_PROTOCOL_VERSION,
        protocol_version: M1_PROTOCOL_VERSION,
        session_id,
        stream: StreamSetup {
            stream_id: "attach-v2".to_string(),
            kind: StreamKind::Attach,
            read_only: true,
            input_owner: false,
        },
        negotiated_attach_protocol_version: Some(M2_ATTACH_PROTOCOL_VERSION),
        negotiated_stream_encoding: Some(AttachStreamEncoding::RawBytes),
        negotiated_initial_replay: Some(AttachInitialReplay::None),
        accepted_frame_types: vec![AttachFrameType::RawOutput],
    };

    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(
        value,
        json!({
            "schema_version": 1,
            "protocol_version": 1,
            "session_id": "818b61b1-a620-4a57-8e72-4d439d03840f",
            "stream": {
                "stream_id": "attach-v2",
                "kind": "attach",
                "read_only": true,
                "input_owner": false
            },
            "negotiated_attach_protocol_version": 2,
            "negotiated_stream_encoding": "raw_bytes",
            "negotiated_initial_replay": "none",
            "accepted_frame_types": ["raw_output"]
        })
    );
    assert!(!response
        .accepted_frame_types
        .contains(&AttachFrameType::StreamLagged));
}

#[test]
fn protocol_contract_raw_attach_request_keeps_stream_and_replay_axes_separate() {
    let session_id: SessionId = "818b61b1-a620-4a57-8e72-4d439d03840f".parse().unwrap();
    let request = SessionAttachRequest {
        selector: SessionSelector::Id { session_id },
        read_only: true,
        replay: millrace_sessions_core::protocol::AttachReplayMode::None,
        requested_terminal_size: None,
        client_protocol_version: Some(M2_ATTACH_PROTOCOL_VERSION),
        accepted_frame_types: vec![
            AttachFrameType::RawOutput,
            AttachFrameType::StreamLagged,
            AttachFrameType::SnapshotUnavailable,
            AttachFrameType::ScreenSnapshot,
        ],
        stream_encoding: Some(AttachStreamEncoding::RawBytes),
        initial_replay: Some(AttachInitialReplay::None),
    };

    assert!(request.requests_raw_stream());
    assert_eq!(
        request.negotiated_stream_encoding(),
        Some(AttachStreamEncoding::RawBytes)
    );
    assert_eq!(
        request.negotiated_initial_replay(),
        Some(AttachInitialReplay::None)
    );
    assert_eq!(
        request.negotiated_frame_types(),
        vec![AttachFrameType::RawOutput, AttachFrameType::StreamLagged]
    );
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        json!({
            "selector": {
                "type": "id",
                "session_id": "818b61b1-a620-4a57-8e72-4d439d03840f"
            },
            "read_only": true,
            "replay": "none",
            "client_protocol_version": 2,
            "accepted_frame_types": [
                "raw_output",
                "stream_lagged",
                "snapshot_unavailable",
                "screen_snapshot"
            ],
            "stream_encoding": "raw_bytes",
            "initial_replay": "none"
        })
    );
}

#[test]
fn protocol_contract_raw_attach_response_confirms_fail_closed_fields() {
    let session_id: SessionId = "818b61b1-a620-4a57-8e72-4d439d03840f".parse().unwrap();
    let raw_response = SessionAttachResponse {
        schema_version: M1_PROTOCOL_VERSION,
        protocol_version: M1_PROTOCOL_VERSION,
        session_id,
        stream: StreamSetup {
            stream_id: "attach-raw".to_string(),
            kind: StreamKind::Attach,
            read_only: true,
            input_owner: false,
        },
        negotiated_attach_protocol_version: Some(M2_ATTACH_PROTOCOL_VERSION),
        negotiated_stream_encoding: Some(AttachStreamEncoding::RawBytes),
        negotiated_initial_replay: Some(AttachInitialReplay::None),
        accepted_frame_types: vec![AttachFrameType::RawOutput, AttachFrameType::StreamLagged],
    };
    assert!(raw_response.confirms_raw_stream());

    let mut downgraded = raw_response.clone();
    downgraded.negotiated_attach_protocol_version = None;
    assert!(!downgraded.confirms_raw_stream());

    downgraded.negotiated_attach_protocol_version = Some(M2_ATTACH_PROTOCOL_VERSION);
    downgraded.negotiated_stream_encoding = Some(AttachStreamEncoding::Text);
    assert!(!downgraded.confirms_raw_stream());
}

#[test]
fn protocol_contract_foreground_daemon_serves_read_only_jsonl_contract() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    fs::create_dir_all(&paths.sessions_dir).unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let session = sample_session(&workspace);
    write_session_meta(&paths, &session);

    let mut child = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let status = request_json(
        &paths,
        json!({"id": "status-1", "method": "host.status", "params": {}}),
    );
    assert_eq!(status["id"], "status-1");
    assert_eq!(status["ok"], true);
    assert_eq!(status["result"]["session_count"], 1);
    let host: HostMeta = serde_json::from_value(status["result"]["host"].clone()).unwrap();
    assert_eq!(host.state_root, paths.root);

    let list = request_json(
        &paths,
        json!({"id": "list-1", "method": "session.list", "params": {}}),
    );
    assert_eq!(list["ok"], true);
    let list_result: SessionListResponse =
        serde_json::from_value(list["result"].clone()).expect("list result shape");
    assert_eq!(list_result.sessions.len(), 1);
    assert_eq!(list_result.sessions[0].session_id, session.id);

    let inspect = request_json(
        &paths,
        json!({
            "id": "inspect-1",
            "method": "session.inspect",
            "params": {
                "selector": {
                    "type": "id",
                    "session_id": session.id
                }
            }
        }),
    );
    assert_eq!(inspect["ok"], true);
    let inspect_result: SessionInspectResponse =
        serde_json::from_value(inspect["result"].clone()).expect("inspect result shape");
    assert_eq!(inspect_result.session.session_id, session.id);
    assert_eq!(
        inspect_result.paths.meta_json,
        paths.session_paths(session.id).meta_json
    );

    let invalid = request_line(&paths, "not-json\n");
    assert_error(
        &invalid,
        "invalid_request",
        ControlErrorCode::InvalidRequest,
    );

    let unsupported = request_json(
        &paths,
        json!({"id": "start-1", "method": "session.start", "params": {}}),
    );
    assert_error(&unsupported, "start-1", ControlErrorCode::InvalidRequest);

    let missing: SessionId = "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8".parse().unwrap();
    let missing_response = request_json(
        &paths,
        json!({
            "id": "inspect-missing",
            "method": "session.inspect",
            "params": {
                "selector": {
                    "type": "id",
                    "session_id": missing
                }
            }
        }),
    );
    assert_error(
        &missing_response,
        "inspect-missing",
        ControlErrorCode::SessionNotFound,
    );

    child.kill();
}

#[test]
fn protocol_contract_foreground_daemon_persists_ui_context_contract() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let mut child = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let ui_id = UiId::new();
    let pane_id = PaneId::new();
    let daemon_id = SessionId::new();
    let set = request_json(
        &paths,
        json!({
            "id": "ui-set-1",
            "method": "ui.context.set",
            "params": {
                "context": {
                    "schema_version": 1,
                    "ui_id": ui_id,
                    "mode": "daemon_console",
                    "active_pane_id": pane_id,
                    "active_daemon_session_id": daemon_id,
                    "active_workspace": null,
                    "agent_session_id": null,
                    "managed_daemon_session_ids": [daemon_id],
                    "monitor_profile": "basic",
                    "updated_at": "2026-05-26T04:00:00Z"
                },
                "events": [{
                    "timestamp": "",
                    "ui_id": ui_id,
                    "kind": "active_daemon_changed",
                    "message": "daemon selected",
                    "fields": {}
                }]
            }
        }),
    );
    assert_eq!(set["ok"], true);
    assert_eq!(set["result"]["context"]["ui_id"], ui_id.to_string());
    assert_eq!(set["result"]["context"]["mode"], "daemon_console");

    let ui_paths = paths.ui_context_paths(ui_id);
    assert!(ui_paths.context_json.exists());
    assert!(ui_paths.events_jsonl.exists());
    assert_private_file(&ui_paths.context_json);
    assert_private_file(&ui_paths.events_jsonl);

    let stored: millrace_sessions_core::state::UiContext =
        read_json(&ui_paths.context_json).expect("context persists");
    assert_eq!(stored.ui_id, ui_id);
    assert_eq!(stored.monitor_profile, MonitorProfile::Basic);
    let events: Vec<millrace_sessions_core::state::UiEvent> =
        read_json_lines(&ui_paths.events_jsonl).expect("events persist");
    assert_eq!(events.len(), 1);
    assert_eq!(
        serde_json::to_value(&events[0].kind).unwrap(),
        json!("active_daemon_changed")
    );

    let get_by_id = request_json(
        &paths,
        json!({
            "id": "ui-get-1",
            "method": "ui.context.get",
            "params": { "ui_id": ui_id }
        }),
    );
    assert_eq!(get_by_id["ok"], true);
    assert_eq!(get_by_id["result"]["context"]["ui_id"], ui_id.to_string());

    let get_unambiguous = request_json(
        &paths,
        json!({"id": "ui-get-2", "method": "ui.context.get", "params": {}}),
    );
    assert_eq!(get_unambiguous["ok"], true);
    assert_eq!(
        get_unambiguous["result"]["context"]["ui_id"],
        ui_id.to_string()
    );

    let second_ui_id = UiId::new();
    let second_set = request_json(
        &paths,
        json!({
            "id": "ui-set-2",
            "method": "ui.context.set",
            "params": {
                "context": {
                    "schema_version": 1,
                    "ui_id": second_ui_id,
                    "mode": "agent_cockpit",
                    "active_pane_id": null,
                    "active_daemon_session_id": null,
                    "active_workspace": null,
                    "agent_session_id": null,
                    "managed_daemon_session_ids": [],
                    "monitor_profile": "auto",
                    "updated_at": "2026-05-26T04:01:00Z"
                }
            }
        }),
    );
    assert_eq!(second_set["ok"], true);

    let ambiguous = request_json(
        &paths,
        json!({"id": "ui-get-ambiguous", "method": "ui.context.get", "params": {}}),
    );
    assert_error(
        &ambiguous,
        "ui-get-ambiguous",
        ControlErrorCode::AmbiguousUiContext,
    );

    let list = request_json(
        &paths,
        json!({"id": "ui-list-1", "method": "ui.context.list", "params": {}}),
    );
    assert_eq!(list["ok"], true);
    assert_eq!(list["result"]["contexts"].as_array().unwrap().len(), 2);

    let close_second = request_json(
        &paths,
        json!({
            "id": "ui-close-2",
            "method": "ui.context.close",
            "params": { "ui_id": second_ui_id }
        }),
    );
    assert_eq!(close_second["ok"], true);
    assert_eq!(close_second["result"]["closed"], true);
    assert!(!paths.ui_context_paths(second_ui_id).context_json.exists());

    let close_first = request_json(
        &paths,
        json!({
            "id": "ui-close-1",
            "method": "ui.context.close",
            "params": { "ui_id": ui_id }
        }),
    );
    assert_eq!(close_first["ok"], true);
    assert!(!ui_paths.context_json.exists());
    let first_events: Vec<millrace_sessions_core::state::UiEvent> =
        read_json_lines(&ui_paths.events_jsonl).expect("close event persists");
    assert_eq!(
        serde_json::to_value(&first_events.last().unwrap().kind).unwrap(),
        json!("ui_closed")
    );

    let missing = request_json(
        &paths,
        json!({
            "id": "ui-get-missing",
            "method": "ui.context.get",
            "params": { "ui_id": ui_id }
        }),
    );
    assert_error(
        &missing,
        "ui-get-missing",
        ControlErrorCode::UiContextNotFound,
    );

    child.kill();
}

#[test]
fn v04_api_dispatch_wraps_success_and_invalid_role_errors() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let host = host_meta(&paths);

    let capabilities = dispatch_json_line(
        &format!(
            "{}\n",
            serde_json::to_string(&json!({
                "id": "api-cap-1",
                "version": "0.4",
                "method": "api.capabilities",
                "params": {}
            }))
            .unwrap()
        ),
        &paths,
        &host,
    );
    let value = serde_json::to_value(capabilities).unwrap();
    assert_eq!(value["id"], "api-cap-1");
    assert_eq!(value["ok"], true);
    assert_eq!(value["schema"], "millmux.api.v0.4");
    assert_eq!(value["method"], "api.capabilities");
    assert!(value["result"]["stable"]
        .as_array()
        .unwrap()
        .iter()
        .any(|capability| capability["name"] == "logs"));
    let session_capability = value["result"]["stable"]
        .as_array()
        .unwrap()
        .iter()
        .find(|capability| capability["name"] == "session")
        .expect("session capability");
    for method in [
        "session.start",
        "session.attach",
        "session.list",
        "session.status",
        "session.inspect",
        "session.screen",
        "session.logs",
        "session.events",
        "session.send",
        "session.resize",
        "session.stop",
        "session.kill",
        "session.delete",
    ] {
        assert!(
            session_capability["methods"]
                .as_array()
                .unwrap()
                .iter()
                .any(|candidate| candidate.as_str() == Some(method)),
            "missing {method}: {session_capability:#?}"
        );
    }

    let invalid_role = dispatch_json_line(
        &format!(
            "{}\n",
            serde_json::to_string(&json!({
                "id": "role-bad-1",
                "version": "0.4",
                "method": "session.list",
                "params": {"role": "worker"}
            }))
            .unwrap()
        ),
        &paths,
        &host,
    );
    let value = serde_json::to_value(invalid_role).unwrap();
    assert_eq!(value["ok"], false);
    assert_eq!(value["schema"], "millmux.api.v0.4");
    assert_eq!(value["method"], "session.list");
    assert_eq!(value["error"]["code"], "invalid_role");
    assert_eq!(value["error"]["retryable"], false);
    assert_eq!(value["error"]["details"], json!({}));

    let invalid_selector = json!({
        "type": "workspace_role",
        "workspace": temp.path().join("workspace").display().to_string(),
        "role": "worker"
    });
    for (method, params) in [
        (
            "session.inspect",
            json!({"selector": invalid_selector.clone()}),
        ),
        (
            "session.screen",
            json!({"selector": invalid_selector.clone()}),
        ),
        (
            "session.logs",
            json!({"selector": invalid_selector.clone()}),
        ),
        (
            "session.events",
            json!({"selector": invalid_selector.clone()}),
        ),
        (
            "session.send",
            json!({"selector": invalid_selector.clone(), "text": "x"}),
        ),
        (
            "input.send",
            json!({
                "target": {"type": "session", "selector": invalid_selector.clone()},
                "text": "x"
            }),
        ),
        (
            "session.resize",
            json!({"selector": invalid_selector.clone(), "rows": 24, "cols": 80}),
        ),
        (
            "session.stop",
            json!({"selector": invalid_selector.clone()}),
        ),
        (
            "session.kill",
            json!({"selector": invalid_selector.clone()}),
        ),
        (
            "session.delete",
            json!({"selector": invalid_selector.clone()}),
        ),
        (
            "attention.list",
            json!({"selector": invalid_selector.clone()}),
        ),
        (
            "attention.mark",
            json!({
                "selector": invalid_selector.clone(),
                "kind": "needs_input",
                "severity": "warning",
                "source": "api",
                "message": "waiting"
            }),
        ),
        (
            "attention.read",
            json!({"selector": invalid_selector.clone()}),
        ),
        (
            "attention.clear",
            json!({"selector": invalid_selector.clone()}),
        ),
    ] {
        let response = dispatch_json_line(
            &format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "id": format!("{method}-bad-role"),
                    "version": "0.4",
                    "method": method,
                    "params": params
                }))
                .unwrap()
            ),
            &paths,
            &host,
        );
        let value = serde_json::to_value(response).unwrap();
        assert_eq!(value["ok"], false, "{method}: {value:#?}");
        assert_eq!(value["method"], method, "{method}: {value:#?}");
        assert_eq!(
            value["error"]["code"], "invalid_role",
            "{method}: {value:#?}"
        );
    }

    let unknown = dispatch_json_line(
        &format!(
            "{}\n",
            serde_json::to_string(&json!({
                "id": "unknown-v04-1",
                "version": "0.4",
                "method": "future.command",
                "params": {}
            }))
            .unwrap()
        ),
        &paths,
        &host,
    );
    let value = serde_json::to_value(unknown).unwrap();
    assert_eq!(value["ok"], false);
    assert_eq!(value["schema"], "millmux.api.v0.4");
    assert_eq!(value["method"], "future.command");
    assert_eq!(value["error"]["code"], "unknown_method");
    assert_eq!(value["error"]["retryable"], false);
    assert_eq!(value["error"]["details"], json!({}));
}

#[test]
fn input_send_pane_target_requires_focus_before_worker_send() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let session = sample_session(&workspace);
    write_session_meta(&paths, &session);

    let ui_id = UiId::new();
    let pane_id = PaneId::new();
    let ui_paths = paths.ui_context_paths(ui_id);
    write_json_atomic(
        &ui_paths.context_json,
        &json!({
            "schema_version": 1,
            "ui_id": ui_id,
            "mode": "agent_cockpit",
            "active_pane_id": pane_id,
            "panes": [{
                "id": pane_id,
                "title": "Agent",
                "view": {
                    "kind": "session_terminal",
                    "session_id": session.id
                },
                "focused": false,
                "stale": false
            }],
            "selected_session_id": session.id,
            "focused_session_id": session.id,
            "focused_pane_kind": "agent_terminal",
            "active_daemon_session_id": null,
            "active_workspace": null,
            "agent_session_id": session.id,
            "managed_session_ids": [session.id],
            "managed_daemon_session_ids": [],
            "monitor_profile": "basic",
            "daemon_health": [],
            "updated_at": "2026-07-09T00:00:00Z"
        }),
    )
    .unwrap();

    let response = dispatch_json_line(
        &format!(
            "{}\n",
            serde_json::to_string(&json!({
                "id": "input-pane-1",
                "version": "0.4",
                "method": "input.send",
                "params": {
                    "target": {
                        "type": "pane",
                        "ui_id": ui_id,
                        "pane_id": pane_id
                    },
                    "text": "hello",
                    "require_focus": true
                }
            }))
            .unwrap()
        ),
        &paths,
        &host_meta(&paths),
    );

    let value = serde_json::to_value(response).unwrap();
    assert_eq!(value["ok"], false);
    assert_eq!(value["method"], "input.send");
    assert_eq!(value["error"]["code"], "input_owner_conflict");
    assert_eq!(value["error"]["details"]["require_focus"], true);

    for (case, pane_patch, expected_code) in [
        (
            "scrollback",
            json!({
                "view": {
                    "kind": "session_terminal",
                    "session_id": session.id,
                    "view_mode": "scrollback"
                },
                "focused": true,
                "stale": false,
                "read_only": false,
                "overlay_active": false
            }),
            "invalid_request",
        ),
        (
            "read-only",
            json!({
                "view": {
                    "kind": "session_terminal",
                    "session_id": session.id
                },
                "focused": true,
                "stale": false,
                "read_only": true,
                "overlay_active": false
            }),
            "input_owner_conflict",
        ),
        (
            "overlay",
            json!({
                "view": {
                    "kind": "session_terminal",
                    "session_id": session.id
                },
                "focused": true,
                "stale": false,
                "read_only": false,
                "overlay_active": true
            }),
            "input_owner_conflict",
        ),
    ] {
        write_json_atomic(
            &ui_paths.context_json,
            &json!({
                "schema_version": 1,
                "ui_id": ui_id,
                "mode": "agent_cockpit",
                "active_pane_id": pane_id,
                "panes": [{
                    "id": pane_id,
                    "title": "Agent",
                    "view": pane_patch["view"].clone(),
                    "focused": pane_patch["focused"].clone(),
                    "stale": pane_patch["stale"].clone(),
                    "read_only": pane_patch["read_only"].clone(),
                    "overlay_active": pane_patch["overlay_active"].clone()
                }],
                "selected_session_id": session.id,
                "focused_session_id": session.id,
                "focused_pane_kind": "agent_terminal",
                "active_daemon_session_id": null,
                "active_workspace": null,
                "agent_session_id": session.id,
                "managed_session_ids": [session.id],
                "managed_daemon_session_ids": [],
                "monitor_profile": "basic",
                "daemon_health": [],
                "updated_at": "2026-07-09T00:00:00Z"
            }),
        )
        .unwrap();

        let response = dispatch_json_line(
            &format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "id": format!("input-pane-{case}"),
                    "version": "0.4",
                    "method": "input.send",
                    "params": {
                        "target": {
                            "type": "pane",
                            "ui_id": ui_id,
                            "pane_id": pane_id
                        },
                        "text": "hello",
                        "require_focus": true
                    }
                }))
                .unwrap()
            ),
            &paths,
            &host_meta(&paths),
        );
        let value = serde_json::to_value(response).unwrap();
        assert_eq!(value["ok"], false, "{case}: {value:#?}");
        assert_eq!(value["method"], "input.send");
        assert_eq!(value["error"]["code"], expected_code, "{case}: {value:#?}");
    }
}

#[test]
fn session_screen_returns_structured_snapshot_from_terminal_checkpoint() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    fs::create_dir_all(&paths.sessions_dir).unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let session = sample_session(&workspace);
    write_session_meta(&paths, &session);
    seed_pty_log(&paths, session.id, b"ready");
    seed_terminal_snapshot(&paths, session.id, 1, 8, 5, vec!["ready".to_string()]);

    let response = dispatch_json_line(
        &screen_request("screen-1", session.id, None),
        &paths,
        &host_meta(&paths),
    );

    assert!(response.ok, "{response:#?}");
    let result = response.result_as::<SessionScreenResponse>().unwrap();
    let value = serde_json::to_value(&result).unwrap();
    assert_eq!(value["type"], "screen_snapshot");
    assert_eq!(value["rows"], 1);
    assert_eq!(value["cols"], 8);
    assert_eq!(value["cells"][0][0]["symbol"], "r");
    assert!(value.get("data").is_none());
}

#[test]
fn session_screen_reports_pipe_sessions_as_structured_unavailable() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    fs::create_dir_all(&paths.sessions_dir).unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut session = sample_session(&workspace);
    session.spawn_mode = SpawnMode::Pipe;
    write_session_meta(&paths, &session);

    let result = dispatch_screen(&paths, session.id, None);

    assert_eq!(result["type"], "snapshot_unavailable");
    assert_eq!(result["reason"], "unsupported_spawn_mode");
    assert_eq!(result["details"]["capability"], "screen");
}

#[test]
fn session_screen_distinguishes_missing_empty_stale_and_size_mismatch() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    fs::create_dir_all(&paths.sessions_dir).unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    let missing = sample_session(&workspace);
    write_session_meta(&paths, &missing);
    seed_pty_log(&paths, missing.id, b"ready");
    let missing_result = dispatch_screen(&paths, missing.id, None);
    assert_eq!(missing_result["reason"], "no_snapshot");
    assert_eq!(missing_result["details"]["snapshot_state"], "missing_file");

    let empty = sample_session(&workspace);
    write_session_meta(&paths, &empty);
    seed_pty_log(&paths, empty.id, b"ready");
    fs::write(&paths.session_paths(empty.id).terminal_snapshot, b"").unwrap();
    let empty_result = dispatch_screen(&paths, empty.id, None);
    assert_eq!(empty_result["reason"], "no_snapshot");
    assert_eq!(empty_result["details"]["snapshot_state"], "empty_file");

    let stale = sample_session(&workspace);
    write_session_meta(&paths, &stale);
    seed_pty_log(&paths, stale.id, b"ready-new");
    seed_terminal_snapshot(&paths, stale.id, 1, 8, 5, vec!["ready".to_string()]);
    let stale_result = dispatch_screen(&paths, stale.id, None);
    assert_eq!(stale_result["reason"], "stale_snapshot");
    assert_eq!(
        stale_result["details"]["snapshot_state"],
        "stale_pty_log_offset"
    );

    let mismatch = sample_session(&workspace);
    write_session_meta(&paths, &mismatch);
    seed_pty_log(&paths, mismatch.id, b"ready");
    seed_terminal_snapshot(&paths, mismatch.id, 1, 8, 5, vec!["ready".to_string()]);
    let mismatch_result = dispatch_screen(
        &paths,
        mismatch.id,
        Some(TerminalDimensions { rows: 2, cols: 8 }),
    );
    assert_eq!(mismatch_result["reason"], "size_mismatch");
    assert_eq!(
        mismatch_result["details"]["snapshot_state"],
        "size_mismatch"
    );
}

fn request_json(paths: &StatePaths, value: Value) -> Value {
    request_line(
        paths,
        &format!("{}\n", serde_json::to_string(&value).unwrap()),
    )
}

fn request_line(paths: &StatePaths, line: &str) -> Value {
    let mut stream = UnixStream::connect(&paths.control_sock).expect("connect to daemon socket");
    stream.write_all(line.as_bytes()).expect("write request");
    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .expect("read response");
    serde_json::from_str(response.trim_end()).expect("response is json")
}

fn assert_error(response: &Value, id: &str, code: ControlErrorCode) {
    assert_eq!(response["id"], id);
    assert_eq!(response["ok"], false);
    assert_eq!(
        serde_json::from_value::<ControlErrorCode>(response["error"]["code"].clone()).unwrap(),
        code
    );
}

#[cfg(unix)]
fn assert_private_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[cfg(not(unix))]
fn assert_private_file(_path: &Path) {}

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

struct DaemonChild {
    child: Child,
}

impl DaemonChild {
    fn spawn(paths: &StatePaths) -> Self {
        let child = Command::new(sessiond_bin())
            .arg("--foreground")
            .env(STATE_DIR_ENV, &paths.root)
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
    ensure_sessiond_bin(&path);
    path
}

fn ensure_sessiond_bin(path: &Path) {
    if is_executable(path) {
        return;
    }

    let status = Command::new("cargo")
        .args([
            "build",
            "-p",
            "millrace-sessions",
            "--bin",
            "millrace-sessiond",
        ])
        .current_dir(workspace_root())
        .status()
        .expect("build millrace-sessiond");
    assert!(status.success(), "failed to build millrace-sessiond");
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

fn write_session_meta(paths: &StatePaths, meta: &SessionMeta) {
    let session_paths = paths.session_paths(meta.id);
    fs::create_dir_all(&session_paths.root).unwrap();
    write_json_atomic(&session_paths.meta_json, meta).unwrap();
}

#[test]
fn attention_dispatch_persists_dedupe_read_clear_and_events() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let session = sample_session(&workspace);
    let session_id = session.id;
    write_session_meta(&paths, &session);
    let host = host_meta(&paths);

    let mark = attention_request(
        "attention-mark-1",
        "attention.mark",
        json!({
            "selector": {"type": "id", "session_id": session_id},
            "kind": "unread",
            "severity": "warning",
            "source": "cli",
            "message": "new terminal output",
            "dedupe_key": "terminal-output"
        }),
    );
    let response = dispatch_json_line(&mark, &paths, &host);
    assert!(response.ok, "{response:#?}");
    let value = response.result.expect("mark result");
    assert_eq!(value["mutated_count"], 1);
    assert_eq!(value["attention"]["open_count"], 1);
    assert_eq!(value["attention"]["unread_count"], 1);

    let dedupe = attention_request(
        "attention-mark-2",
        "attention.mark",
        json!({
            "selector": {"type": "id", "session_id": session_id},
            "kind": "unread",
            "severity": "error",
            "source": "cli",
            "message": "updated terminal output",
            "dedupe_key": "terminal-output"
        }),
    );
    let response = dispatch_json_line(&dedupe, &paths, &host);
    assert!(response.ok, "{response:#?}");
    let value = response.result.expect("dedupe result");
    assert_eq!(value["attention_items"].as_array().unwrap().len(), 1);
    assert_eq!(
        value["attention_items"][0]["message"],
        "updated terminal output"
    );
    assert_eq!(value["attention"]["highest_severity"], "error");

    let block = attention_request(
        "attention-mark-3",
        "attention.mark",
        json!({
            "selector": {"type": "id", "session_id": session_id},
            "kind": "blocked",
            "severity": "critical",
            "source": "agent",
            "message": "waiting on operator"
        }),
    );
    assert!(dispatch_json_line(&block, &paths, &host).ok);

    let read_unread = attention_request(
        "attention-read-1",
        "attention.read",
        json!({
            "selector": {"type": "id", "session_id": session_id}
        }),
    );
    let response = dispatch_json_line(&read_unread, &paths, &host);
    assert!(response.ok, "{response:#?}");
    let value = response.result.expect("read result");
    assert_eq!(value["mutated_count"], 1);
    assert_eq!(value["attention"]["open_count"], 2);
    assert_eq!(value["attention"]["unread_count"], 0);

    let list = attention_request(
        "attention-list-1",
        "attention.list",
        json!({
            "selector": {"type": "id", "session_id": session_id},
            "include_read": true
        }),
    );
    let response = dispatch_json_line(&list, &paths, &host);
    assert!(response.ok, "{response:#?}");
    let value = response.result.expect("list result");
    let items = value["attention_items"].as_array().unwrap();
    let blocked = items
        .iter()
        .find(|item| item["kind"] == "blocked")
        .expect("blocked item remains");
    assert!(blocked.get("read_at").is_none(), "{blocked:#?}");

    let clear_unread = attention_request(
        "attention-clear-1",
        "attention.clear",
        json!({
            "selector": {"type": "id", "session_id": session_id},
            "kinds": ["unread"]
        }),
    );
    let response = dispatch_json_line(&clear_unread, &paths, &host);
    assert!(response.ok, "{response:#?}");
    let value = response.result.expect("clear result");
    assert_eq!(value["mutated_count"], 1);
    assert_eq!(value["attention"]["open_count"], 1);
    assert_eq!(value["attention"]["kinds"], json!(["blocked"]));

    let events = read_events(paths.session_paths(session_id).events_jsonl).unwrap();
    assert!(events
        .iter()
        .any(|event| event.kind == SessionEventKind::AttentionMarked));
    assert!(events
        .iter()
        .any(|event| event.kind == SessionEventKind::AttentionRead));
    assert!(events
        .iter()
        .any(|event| event.kind == SessionEventKind::AttentionCleared));
}

fn attention_request(id: &str, method: &str, params: Value) -> String {
    format!(
        "{}\n",
        serde_json::to_string(&json!({
            "id": id,
            "method": method,
            "params": params
        }))
        .unwrap()
    )
}

fn dispatch_screen(
    paths: &StatePaths,
    session_id: SessionId,
    requested_terminal_size: Option<TerminalDimensions>,
) -> Value {
    let response = dispatch_json_line(
        &screen_request("screen-fixture", session_id, requested_terminal_size),
        paths,
        &host_meta(paths),
    );
    assert!(response.ok, "{response:#?}");
    response.result.unwrap()
}

fn screen_request(
    id: &str,
    session_id: SessionId,
    requested_terminal_size: Option<TerminalDimensions>,
) -> String {
    let mut params = json!({
        "selector": {
            "type": "id",
            "session_id": session_id
        }
    });
    if let Some(size) = requested_terminal_size {
        params["requested_terminal_size"] = json!({
            "rows": size.rows,
            "cols": size.cols
        });
    }
    format!(
        "{}\n",
        serde_json::to_string(&json!({
            "id": id,
            "method": "session.screen",
            "params": params
        }))
        .unwrap()
    )
}

fn seed_pty_log(paths: &StatePaths, session_id: SessionId, bytes: &[u8]) {
    fs::write(&paths.session_paths(session_id).pty_log, bytes).unwrap();
}

fn seed_terminal_snapshot(
    paths: &StatePaths,
    session_id: SessionId,
    rows: u16,
    cols: u16,
    offset: u64,
    screen: Vec<String>,
) {
    let snapshot = TerminalSnapshot {
        schema_version: 1,
        rows,
        cols,
        cursor_row: 0,
        cursor_col: 0,
        alternate_screen: false,
        pty_log_offset: offset,
        raw_replay_start_offset: 0,
        raw_replay_end_offset: offset,
        captured_at: "2026-07-08T00:00:00Z".to_string(),
        structured_screen: Some(structured_screen_fixture(rows, cols, offset, &screen)),
        screen,
    };
    let session_paths = paths.session_paths(session_id);
    write_json_atomic(&session_paths.terminal_snapshot, &snapshot).unwrap();
    fs::write(&session_paths.raw_replay_ring, b"ready").unwrap();
}

fn structured_screen_fixture(
    rows: u16,
    cols: u16,
    offset: u64,
    lines: &[String],
) -> ScreenSnapshot {
    let mut cells = Vec::with_capacity(usize::from(rows));
    for row_index in 0..usize::from(rows) {
        let line = lines.get(row_index).map(String::as_str).unwrap_or("");
        let mut row = line
            .chars()
            .take(usize::from(cols))
            .map(|ch| ScreenCell::default_symbol(ch.to_string()))
            .collect::<Vec<_>>();
        row.resize_with(usize::from(cols), ScreenCell::blank);
        cells.push(row);
    }
    ScreenSnapshot {
        schema_version: SCREEN_SNAPSHOT_SCHEMA_VERSION,
        rows,
        cols,
        cursor: ScreenCursor {
            row: 0,
            col: 0,
            visible: Some(true),
        },
        alternate_screen: false,
        cells,
        source: ScreenSnapshotSource {
            pty_log_offset: offset,
            raw_replay_start_offset: 0,
            raw_replay_end_offset: offset,
        },
        captured_at: "2026-07-08T00:00:00Z".to_string(),
    }
}

fn host_meta(paths: &StatePaths) -> HostMeta {
    HostMeta {
        pid: std::process::id(),
        state_root: paths.root.clone(),
        control_socket: paths.control_sock.clone(),
        started_at: "2026-07-08T00:00:00Z".to_string(),
        updated_at: "2026-07-08T00:00:00Z".to_string(),
    }
}

fn sample_session(workspace: impl AsRef<Path>) -> SessionMeta {
    let workspace = workspace.as_ref();
    SessionMeta {
        id: SessionId::new(),
        name: Some("daemon".to_string()),
        role: SessionRole::MillraceDaemon,
        process_state: ProcessState::Running,
        attention_state: AttentionState::MillraceIdle,
        attention_items: Vec::new(),
        status_summary: None,
        workspace: Some(WorkspaceIdentity::capture(workspace).unwrap()),
        cwd: workspace.to_path_buf(),
        argv: vec![
            "millrace".to_string(),
            "run".to_string(),
            "daemon".to_string(),
        ],
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
