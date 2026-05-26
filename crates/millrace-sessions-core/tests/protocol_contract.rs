use std::{collections::BTreeMap, path::PathBuf};

use millrace_sessions_core::{
    ids::{PaneId, SessionId, UiId},
    protocol::{
        ControlErrorBody, ControlErrorCode, ControlMethod, ControlRequest, ControlResponse,
        SessionListRequest, SessionListResponse, SessionStartRequest, SessionSummary,
        UiContextGetRequest, UiContextSetRequest, M1_PROTOCOL_VERSION,
    },
    state::{AttentionState, MonitorProfile, ProcessState, SessionRole, UiContext, UiMode},
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
                    "attached_clients": 0
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
