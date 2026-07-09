use millrace_sessions_core::{
    protocol::{
        ApiCapabilitiesResponse, ApiIdentifyResponse, AttentionListResponse,
        AttentionMutationResponse, DoctorResponse, EventStreamFrame, EventSubscribeResponse,
        HostStatusResponse, InputSendResponse, LogLine, LogStream, LogStreamFrame, ScreenFrame,
        SessionDeleteResponse, SessionEventsResponse, SessionInspectResponse, SessionKillResponse,
        SessionListResponse, SessionLogsResponse, SessionResizeResponse, SessionScreenResponse,
        SessionSendResponse, SessionStartResponse, SessionStopResponse, SessionSummary,
        UiContextGetResponse, UiContextListResponse,
    },
    state::{AttentionItem, AttentionState, ProcessState, SessionRole},
};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

pub fn render_json<T: Serialize>(result: &T) -> Result<String, OutputError> {
    let mut output = serde_json::to_string(result)?;
    output.push('\n');
    Ok(output)
}

pub fn render_list(result: &SessionListResponse) -> String {
    if result.sessions.is_empty() {
        return "no sessions\n".to_string();
    }

    let mut lines = String::new();
    for session in &result.sessions {
        lines.push_str(&format!(
            "{} {} role={} spawn={} name={} monitor={} clients={} owner={} cwd={}\n",
            session.session_id,
            process_state(&session.process_state),
            role(&session.role),
            session.spawn_mode,
            session.name.as_deref().unwrap_or("-"),
            session.monitor_profile,
            session.attached_clients,
            input_owner(session),
            session.cwd.display()
        ));
    }
    lines
}

pub fn render_host_status(result: &HostStatusResponse) -> String {
    match &result.host {
        Some(host) => format!(
            "host running pid={} sessions={} socket={}\n",
            host.pid,
            result.session_count,
            host.control_socket.display()
        ),
        None => format!("host unavailable sessions={}\n", result.session_count),
    }
}

pub fn render_doctor(result: &DoctorResponse) -> String {
    if result.issues.is_empty() {
        return format!("doctor status={:?}\n", result.status).to_ascii_lowercase();
    }

    let mut output = String::new();
    output.push_str(&format!("doctor status={:?}\n", result.status).to_ascii_lowercase());
    for issue in &result.issues {
        output.push_str(&format!(
            "{} {} {}\n",
            json_string(&issue.severity),
            issue.code,
            issue.message
        ));
    }
    for repair in &result.repairs {
        output.push_str(&format!(
            "repair {} {} {}\n",
            json_string(&repair.mode),
            json_string(&repair.status),
            repair.message.as_deref().unwrap_or("")
        ));
    }
    output
}

pub fn render_start(result: &SessionStartResponse) -> String {
    let session = &result.session;
    format!(
        "session {} {} role={} spawn={} name={} monitor={} attached_existing={}\n",
        session.session_id,
        process_state(&session.process_state),
        role(&session.role),
        session.spawn_mode,
        session.name.as_deref().unwrap_or("-"),
        session.monitor_profile,
        result.attached_existing
    )
}

pub fn render_session_status(result: &SessionInspectResponse) -> String {
    let session = &result.session;
    format!(
        "session {} {} role={} spawn={} name={} monitor={} clients={} owner={}\n",
        session.session_id,
        process_state(&session.process_state),
        role(&session.role),
        session.spawn_mode,
        session.name.as_deref().unwrap_or("-"),
        session.monitor_profile,
        session.attached_clients,
        input_owner(session)
    )
}

pub fn render_logs(result: &SessionLogsResponse) -> String {
    let mut output = String::new();
    for line in &result.lines {
        output.push_str(&render_log_line_text(line));
        output.push('\n');
    }
    output
}

pub fn render_events(result: &SessionEventsResponse) -> String {
    let mut output = String::new();
    for event in &result.events {
        push_event_line(&mut output, event);
    }
    output
}

pub fn render_event_subscribe(result: &EventSubscribeResponse) -> String {
    format!(
        "events subscribed session={} cursor={} replay_limit={} queue_limit={} heartbeat_ms={}\n",
        result.session_id,
        result.cursor,
        result.replay_limit,
        result.subscriber_queue_limit,
        result.heartbeat_ms
    )
}

pub fn render_screen(result: &SessionScreenResponse) -> String {
    match &result.frame {
        ScreenFrame::ScreenSnapshot { snapshot } => {
            let mut output = String::new();
            for line in snapshot.plain_lines() {
                output.push_str(&line);
                output.push('\n');
            }
            output
        }
        ScreenFrame::SnapshotUnavailable { reason, details } => {
            let snapshot_state = details
                .as_ref()
                .and_then(|details| details.get("snapshot_state"))
                .and_then(Value::as_str);
            match snapshot_state {
                Some(snapshot_state) => format!(
                    "screen unavailable reason={} snapshot_state={}\n",
                    json_string(reason),
                    snapshot_state
                ),
                None => format!("screen unavailable reason={}\n", json_string(reason)),
            }
        }
    }
}

pub fn render_log_stream_frame(frame: &LogStreamFrame) -> String {
    match frame {
        LogStreamFrame::Line { line } => format!("{}\n", render_log_line_text(line)),
        LogStreamFrame::Closed => String::new(),
    }
}

pub fn render_log_line_text(line: &LogLine) -> String {
    match line.stream {
        LogStream::Pty => line.line.clone(),
        LogStream::Stdout => format!("[stdout] {}", line.line),
        LogStream::Stderr => format!("[stderr] {}", line.line),
    }
}

pub fn render_event_stream_frame(frame: &EventStreamFrame) -> String {
    let mut output = String::new();
    match frame {
        EventStreamFrame::Event { event, .. } => push_event_line(&mut output, event),
        EventStreamFrame::Ack { cursor, .. } => {
            output.push_str(&format!("events subscribed cursor={cursor}\n"));
        }
        EventStreamFrame::Heartbeat { cursor } => {
            output.push_str(&format!("events heartbeat cursor={cursor}\n"));
        }
        EventStreamFrame::StreamLagged {
            dropped_events,
            cursor,
            ..
        } => {
            output.push_str(&format!(
                "events lagged dropped_events={dropped_events} cursor={cursor}\n"
            ));
        }
        EventStreamFrame::Error { error } => {
            output.push_str(&format!("events error {}\n", error));
        }
        EventStreamFrame::Closed => {}
    }
    output
}

pub fn render_send(result: &SessionSendResponse) -> String {
    format!(
        "sent {} bytes to {}\n",
        result.bytes_sent, result.session_id
    )
}

pub fn render_input_send(result: &InputSendResponse) -> String {
    format!(
        "sent {} bytes to {} routed_via_focus={}\n",
        result.bytes_sent, result.session_id, result.routed_via_focus
    )
}

pub fn render_resize(result: &SessionResizeResponse) -> String {
    format!(
        "resized {} to {}x{}\n",
        result.session_id, result.rows, result.cols
    )
}

pub fn render_stop(result: &SessionStopResponse) -> String {
    format!(
        "stop requested for {} state={}\n",
        result.session_id,
        process_state(&result.process_state)
    )
}

pub fn render_kill(result: &SessionKillResponse) -> String {
    format!(
        "kill requested for {} state={}\n",
        result.session_id,
        process_state(&result.process_state)
    )
}

pub fn render_delete(result: &SessionDeleteResponse) -> String {
    format!(
        "deleted {} archived={} purged={}\n",
        result.session_id, result.archived, result.purged
    )
}

pub fn render_attention_list(result: &AttentionListResponse) -> String {
    let mut output = format!(
        "attention {} open={} unread={}\n",
        result.session_id, result.attention.open_count, result.attention.unread_count
    );
    push_attention_items(&mut output, &result.attention_items);
    output
}

pub fn render_attention_mutation(action: &str, result: &AttentionMutationResponse) -> String {
    let mut output = format!(
        "attention {} {}={} open={} unread={}\n",
        result.session_id,
        action,
        result.mutated_count,
        result.attention.open_count,
        result.attention.unread_count
    );
    push_attention_items(&mut output, &result.attention_items);
    output
}

pub fn render_context(result: &UiContextGetResponse) -> String {
    let context = &result.context;
    let mut lines = String::new();
    lines.push_str(&format!("ui {}\n", context.ui_id));
    push_field(&mut lines, "mode", &json_string(&context.mode));
    push_field(
        &mut lines,
        "monitor",
        &json_string(&context.monitor_profile),
    );
    push_field(
        &mut lines,
        "active_pane",
        &context
            .active_pane_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "-".to_string()),
    );
    push_field(
        &mut lines,
        "active_daemon",
        &context
            .active_daemon_session_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "-".to_string()),
    );
    if let Some(workspace) = &context.active_workspace {
        push_field(
            &mut lines,
            "workspace",
            &workspace.canonical_path.display().to_string(),
        );
    }
    push_field(
        &mut lines,
        "context",
        &result.paths.context_json.display().to_string(),
    );
    lines
}

pub fn render_context_list(result: &UiContextListResponse) -> String {
    if result.contexts.is_empty() {
        return "no UI contexts\n".to_string();
    }

    let mut lines = String::new();
    for entry in &result.contexts {
        lines.push_str(&format!(
            "{} mode={} monitor={} updated_at={}\n",
            entry.context.ui_id,
            json_string(&entry.context.mode),
            json_string(&entry.context.monitor_profile),
            entry.context.updated_at
        ));
    }
    lines
}

pub fn render_context_export(result: &Value) -> String {
    let ui_id = result
        .pointer("/ui/ui_id")
        .and_then(Value::as_str)
        .unwrap_or("-");
    let mode = result
        .pointer("/ui/mode")
        .and_then(Value::as_str)
        .unwrap_or("-");
    let workspace = result
        .pointer("/workspace/path")
        .and_then(Value::as_str)
        .unwrap_or("-");
    let session_count = result
        .get("sessions")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or_default();
    let attention_count = result
        .get("open_attention_items")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or_default();
    let mut output = format!(
        "context export ui={} mode={} workspace={} sessions={} open_attention={}\n",
        ui_id, mode, workspace, session_count, attention_count
    );
    if let Some(context_path) = result.pointer("/ui/context_path").and_then(Value::as_str) {
        push_field(&mut output, "context", context_path);
    }
    if let Some(source_session) = result
        .pointer("/handoff/source_session_id")
        .and_then(Value::as_str)
    {
        push_field(&mut output, "source_session", source_session);
    }
    output
}

pub fn render_pane_list(result: &UiContextGetResponse) -> String {
    let mut output = String::new();
    for pane in &result.context.panes {
        output.push_str(&format!(
            "{} title={} kind={} session={} focused={} stale={}\n",
            pane.id,
            pane.title,
            json_string(&pane.view.kind),
            pane.view
                .session_id
                .map(|session_id| session_id.to_string())
                .unwrap_or_else(|| "-".to_string()),
            pane.focused,
            pane.stale
        ));
    }
    if output.is_empty() {
        output.push_str("no panes\n");
    }
    output
}

pub fn render_api_capabilities(result: &ApiCapabilitiesResponse) -> String {
    let mut output = format!("millmux api {} stable capabilities\n", result.api_version);
    for capability in &result.stable {
        output.push_str(&format!(
            "{} available={} methods={}\n",
            capability.name,
            capability.available,
            capability.methods.join(",")
        ));
    }
    for capability in &result.experimental {
        output.push_str(&format!(
            "{} experimental available={} reason={}\n",
            capability.name,
            capability.available,
            capability.unavailable_reason.as_deref().unwrap_or("-")
        ));
    }
    output
}

pub fn render_api_identify(result: &ApiIdentifyResponse) -> String {
    format!(
        "millmux api {} schema={} exposure={}\n",
        result.api_version, result.schema, result.network_exposure
    )
}

pub fn render_inspect(result: &SessionInspectResponse) -> String {
    let session = &result.session;
    let mut lines = String::new();
    lines.push_str(&format!("session {}\n", session.session_id));
    push_field(&mut lines, "name", session.name.as_deref().unwrap_or("-"));
    push_field(&mut lines, "role", &role(&session.role));
    push_field(&mut lines, "spawn", &session.spawn_mode.to_string());
    push_field(
        &mut lines,
        "process",
        &process_state(&session.process_state),
    );
    push_field(
        &mut lines,
        "attention",
        &attention_state(&session.attention_state),
    );
    push_field(
        &mut lines,
        "attention_rollup",
        &format!(
            "open={} unread={} severity={}",
            session.attention.open_count,
            session.attention.unread_count,
            session
                .attention
                .highest_severity
                .map(|severity| severity.to_string())
                .unwrap_or_else(|| "none".to_string())
        ),
    );
    push_field(
        &mut lines,
        "status_summary",
        &format!(
            "{}:{}",
            session.status_summary.source, session.status_summary.label
        ),
    );
    push_field(&mut lines, "cwd", &session.cwd.display().to_string());
    push_field(&mut lines, "monitor", &session.monitor_profile.to_string());
    push_field(
        &mut lines,
        "attached_clients",
        &session.attached_clients.to_string(),
    );
    push_field(&mut lines, "input_owner", input_owner(session));
    if let Some(workspace) = &session.workspace {
        push_field(
            &mut lines,
            "workspace",
            &workspace.canonical_path.display().to_string(),
        );
    }
    push_field(&mut lines, "argv", &argv(session));
    push_field(&mut lines, "root", &result.paths.root.display().to_string());
    if let Some(worker) = &result.worker {
        push_field(
            &mut lines,
            "worker",
            &format!(
                "pid={} state={}",
                worker.pid,
                process_state(&worker.process_state)
            ),
        );
    }
    if !result.attention_items.is_empty() {
        lines.push_str("attention_items:\n");
        push_attention_items(&mut lines, &result.attention_items);
    }
    lines
}

fn push_field(lines: &mut String, label: &str, value: &str) {
    lines.push_str(label);
    lines.push_str(": ");
    lines.push_str(value);
    lines.push('\n');
}

fn push_event_line(output: &mut String, event: &millrace_sessions_core::events::SessionEvent) {
    output.push_str(&format!(
        "{} {} {}\n",
        event.timestamp,
        json_string(&event.kind),
        event.message.as_deref().unwrap_or("")
    ));
}

fn push_attention_items(output: &mut String, items: &[AttentionItem]) {
    for item in items {
        output.push_str(&format!(
            "  {} target={}:{} kind={} severity={} source={} read={} cleared={} message={}\n",
            item.id,
            item.target_type,
            item.target_id,
            item.kind,
            item.severity,
            item.source,
            item.read_at.as_deref().unwrap_or("-"),
            item.cleared_at.as_deref().unwrap_or("-"),
            item.message
        ));
    }
}

fn argv(session: &SessionSummary) -> String {
    if session.argv.is_empty() {
        "-".to_string()
    } else {
        session.argv.join(" ")
    }
}

fn input_owner(session: &SessionSummary) -> &str {
    session.input_owner.as_deref().unwrap_or("-")
}

fn role(value: &SessionRole) -> String {
    value.as_wire_value().to_string()
}

fn process_state(value: &ProcessState) -> String {
    json_string(value)
}

fn attention_state(value: &AttentionState) -> String {
    json_string(value)
}

fn json_string<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| match value {
            Value::String(value) => Some(value),
            _ => None,
        })
        .unwrap_or_else(|| "unknown".to_string())
}

#[derive(Debug, Error)]
pub enum OutputError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use millrace_sessions_core::{
        ids::SessionId,
        protocol::{
            SessionArtifacts, SessionCapabilities, SessionInspectResponse, M1_PROTOCOL_VERSION,
        },
        state::{AttentionState, ProcessState, SessionPaths, SpawnMode},
    };
    use serde_json::Value;

    use super::*;

    fn summary() -> SessionSummary {
        SessionSummary {
            session_id: SessionId::new(),
            name: Some("shell".to_string()),
            role: SessionRole::Shell,
            spawn_mode: SpawnMode::Pty,
            process_state: ProcessState::Running,
            attention_state: AttentionState::Active,
            attention: Default::default(),
            status_summary: Default::default(),
            failure_message: None,
            workspace: None,
            cwd: PathBuf::from("/tmp"),
            argv: vec!["sh".to_string()],
            monitor_profile: millrace_sessions_core::state::MonitorProfile::Auto,
            created_at: "2026-05-20T18:00:00Z".to_string(),
            updated_at: "2026-05-20T18:01:00Z".to_string(),
            stop_requested_at: None,
            stop_reason: None,
            attached_clients: 0,
            input_owner: None,
            capabilities: SessionCapabilities::for_spawn_mode(SpawnMode::Pty),
            artifacts: SessionArtifacts::default(),
            liveness: Default::default(),
        }
    }

    #[test]
    fn output_renders_raw_json_without_protocol_envelope() {
        let result = SessionListResponse {
            schema_version: M1_PROTOCOL_VERSION,
            protocol_version: M1_PROTOCOL_VERSION,
            sessions: Vec::new(),
        };

        let output = render_json(&result).unwrap();
        let value: Value = serde_json::from_str(&output).unwrap();

        assert!(value.get("sessions").is_some());
        assert!(value.get("id").is_none());
        assert!(value.get("ok").is_none());
    }

    #[test]
    fn output_renders_empty_list_compactly() {
        let result = SessionListResponse {
            schema_version: M1_PROTOCOL_VERSION,
            protocol_version: M1_PROTOCOL_VERSION,
            sessions: Vec::new(),
        };

        assert_eq!(render_list(&result), "no sessions\n");
    }

    #[test]
    fn output_renders_inspect_without_optional_worker() {
        let session = summary();
        let result = SessionInspectResponse {
            schema_version: M1_PROTOCOL_VERSION,
            protocol_version: M1_PROTOCOL_VERSION,
            paths: SessionPaths {
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
            },
            session,
            attention_items: Vec::new(),
            worker: None,
        };

        let output = render_inspect(&result);

        assert!(output.contains("role: shell\n"));
        assert!(output.contains("process: running\n"));
    }
}
