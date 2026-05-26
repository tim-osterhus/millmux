use std::{
    collections::BTreeMap,
    env, fs,
    io::{BufRead as StdBufRead, BufReader as StdBufReader, Read, Seek, SeekFrom, Write},
    os::unix::net::UnixStream as StdUnixStream,
    path::Path,
    process::Command,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use millrace_sessions_core::{
    error::{MillmuxError, MillmuxResult},
    events::{append_event, current_timestamp, read_events, SessionEvent, SessionEventKind},
    ids::{SessionId, UiId},
    paths::{state_paths, StatePaths},
    protocol::{
        AttachStreamFrame, ControlErrorBody, ControlErrorCode, ControlResponse, DoctorRequest,
        EventStreamFrame, HostStatusRequest, HostStatusResponse, LogLine, LogStreamFrame,
        SessionAttachRequest, SessionAttachResponse, SessionDeleteRequest, SessionDeleteResponse,
        SessionEventsRequest, SessionEventsResponse, SessionInspectRequest, SessionInspectResponse,
        SessionKillRequest, SessionKillResponse, SessionListRequest, SessionListResponse,
        SessionLogsRequest, SessionLogsResponse, SessionResizeRequest, SessionResizeResponse,
        SessionSendRequest, SessionSendResponse, SessionStartRequest, SessionStartResponse,
        SessionStopRequest, SessionStopResponse, SessionSummary, StreamKind, StreamSetup,
        UiContextCloseRequest, UiContextCloseResponse, UiContextGetRequest, UiContextGetResponse,
        UiContextListEntry, UiContextListRequest, UiContextListResponse, UiContextSetRequest,
        UiContextSetResponse, WorkerAckResponse, WorkerAttachRequest, WorkerAttachResponse,
        WorkerControlMethod, WorkerControlRequest, WorkerControlResponse,
        WorkerReleaseAttachRequest, WorkerResizeRequest, WorkerResizeResponse, WorkerSendRequest,
        WorkerSendResponse, M1_PROTOCOL_VERSION,
    },
    scrollback::ScrollbackBuffer,
    state::{
        AttentionState, HostMeta, MonitorProfile, ProcessState, SessionMeta, SessionPaths,
        SessionRole, UiContext, UiContextPaths, UiEvent, UiEventKind, WorkerMeta,
    },
    storage::{append_json_line, create_private_dir_all, read_json, write_json_atomic},
    workspace::WorkspaceIdentity,
};
use serde_json::{json, Value};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader, Lines},
    net::{unix::OwnedReadHalf, unix::OwnedWriteHalf, UnixListener, UnixStream},
    time::sleep,
};

use crate::{
    bootstrap::{bootstrap_foreground, ForegroundHost, HostBootstrapError},
    doctor,
    registry::{HostRegistry, RegistryError},
    stop,
    worker_launcher::{launch_worker, WorkerLaunchError},
};

static STREAM_COUNTER: AtomicU64 = AtomicU64::new(1);
const WORKER_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const WORKER_CONNECT_POLL: Duration = Duration::from_millis(25);
const FOLLOW_POLL: Duration = Duration::from_millis(50);
const DEFAULT_STOP_GRACE: Duration = Duration::from_secs(2);
const KILL_SETTLE_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug, Error)]
pub enum HostServerError {
    #[error(transparent)]
    Bootstrap(#[from] HostBootstrapError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Core(#[from] MillmuxError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub async fn run_foreground() -> Result<(), HostServerError> {
    let paths = state_paths()?;
    let host = bootstrap_foreground(paths)?;
    serve_until_shutdown(host).await
}

pub async fn serve_until_shutdown(host: ForegroundHost) -> Result<(), HostServerError> {
    let listener = bind_listener(host.paths()).await?;
    let runtime = Arc::new(ServerRuntime { host });

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let runtime = Arc::clone(&runtime);
                tokio::spawn(async move {
                    let _ = handle_connection(stream, runtime).await;
                });
            }
            signal = tokio::signal::ctrl_c() => {
                signal?;
                break;
            }
        }
    }

    Ok(())
}

async fn bind_listener(paths: &StatePaths) -> Result<UnixListener, HostServerError> {
    let listener = UnixListener::bind(&paths.control_sock)?;
    harden_socket_permissions(&paths.control_sock)?;
    Ok(listener)
}

#[cfg(unix)]
fn harden_socket_permissions(path: &std::path::Path) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn harden_socket_permissions(_path: &std::path::Path) -> Result<(), std::io::Error> {
    Ok(())
}

struct ServerRuntime {
    host: ForegroundHost,
}

impl ServerRuntime {
    fn paths(&self) -> &StatePaths {
        self.host.paths()
    }

    fn meta(&self) -> &HostMeta {
        self.host.meta()
    }
}

async fn handle_connection(
    stream: UnixStream,
    runtime: Arc<ServerRuntime>,
) -> Result<(), HostServerError> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = TokioBufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        if should_stream_request(&line) {
            handle_streaming_request(line, lines, writer, runtime).await?;
            return Ok(());
        }

        let response = dispatch_json_line(&line, runtime.paths(), runtime.meta());
        writer
            .write_all(response.to_json_line()?.as_bytes())
            .await?;
        writer.flush().await?;
    }

    Ok(())
}

fn should_stream_request(line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    let Some(method) = value.get("method").and_then(Value::as_str) else {
        return false;
    };
    if method == "session.attach" {
        return true;
    }
    if !matches!(method, "session.logs" | "session.events") {
        return false;
    }
    value
        .get("params")
        .and_then(|params| params.get("follow"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

async fn handle_streaming_request(
    line: String,
    lines: Lines<TokioBufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
    runtime: Arc<ServerRuntime>,
) -> Result<(), HostServerError> {
    let request =
        match serde_json::from_str::<millrace_sessions_core::protocol::ControlRequest>(&line) {
            Ok(request) => request,
            Err(error) => {
                let mut writer = writer;
                let response = error_response(
                    "invalid_request",
                    ControlErrorCode::InvalidRequest,
                    format!("invalid JSON request: {error}"),
                );
                writer
                    .write_all(response.to_json_line()?.as_bytes())
                    .await?;
                writer.flush().await?;
                return Ok(());
            }
        };

    match request.method {
        millrace_sessions_core::protocol::ControlMethod::SessionAttach => {
            handle_attach_stream(request, lines, writer, runtime.paths()).await
        }
        millrace_sessions_core::protocol::ControlMethod::SessionLogs => {
            handle_logs_follow_stream(request, writer, runtime.paths()).await
        }
        millrace_sessions_core::protocol::ControlMethod::SessionEvents => {
            handle_events_follow_stream(request, writer, runtime.paths()).await
        }
        _ => Ok(()),
    }
}

pub fn dispatch_json_line(line: &str, paths: &StatePaths, host: &HostMeta) -> ControlResponse {
    let value = match serde_json::from_str::<Value>(line) {
        Ok(value) => value,
        Err(error) => {
            return error_response(
                "invalid_request",
                ControlErrorCode::InvalidRequest,
                format!("invalid JSON request: {error}"),
            )
        }
    };

    let id = match value.get("id").and_then(Value::as_str) {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => {
            return error_response(
                "invalid_request",
                ControlErrorCode::InvalidRequest,
                "request id must be a non-empty string",
            )
        }
    };

    let method = match value.get("method").and_then(Value::as_str) {
        Some(method) => method,
        None => {
            return error_response(
                id,
                ControlErrorCode::InvalidRequest,
                "request method must be a string",
            )
        }
    };
    let params = value.get("params").cloned().unwrap_or_else(|| json!({}));

    match method {
        "host.status" => dispatch_host_status(id, params, paths, host),
        "host.doctor" => dispatch_host_doctor(id, params, paths, host),
        "session.start" => dispatch_session_start(id, params, paths),
        "session.list" => dispatch_session_list(id, params, paths),
        "session.inspect" => dispatch_session_inspect(id, params, paths),
        "session.logs" => dispatch_session_logs(id, params, paths),
        "session.events" => dispatch_session_events(id, params, paths),
        "session.send" => dispatch_session_send(id, params, paths),
        "session.resize" => dispatch_session_resize(id, params, paths),
        "session.stop" => dispatch_session_stop(id, params, paths),
        "session.kill" => dispatch_session_kill(id, params, paths),
        "session.delete" => dispatch_session_delete(id, params, paths),
        "ui.context.get" => dispatch_ui_context_get(id, params, paths),
        "ui.context.set" => dispatch_ui_context_set(id, params, paths),
        "ui.context.list" => dispatch_ui_context_list(id, params, paths),
        "ui.context.close" => dispatch_ui_context_close(id, params, paths),
        _ => error_response(
            id,
            ControlErrorCode::UnknownMethod,
            format!("unsupported method: {method}"),
        ),
    }
}

fn dispatch_host_doctor(
    id: String,
    params: Value,
    paths: &StatePaths,
    host: &HostMeta,
) -> ControlResponse {
    let request = match serde_json::from_value::<DoctorRequest>(params) {
        Ok(request) => request,
        Err(error) => {
            return error_response(
                id,
                ControlErrorCode::InvalidRequest,
                format!("invalid host.doctor params: {error}"),
            )
        }
    };

    match doctor::run_doctor(paths, Some(host), &request) {
        Ok(result) => success_response(id, &result),
        Err(error) => error_response(id, ControlErrorCode::IoError, error.to_string()),
    }
}

fn dispatch_session_start(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<SessionStartRequest>(params) {
        Ok(request) => request,
        Err(error) => {
            return error_response(
                id,
                ControlErrorCode::InvalidRequest,
                format!("invalid session.start params: {error}"),
            )
        }
    };

    match start_session(paths, request) {
        Ok(result) => success_response(id, &result),
        Err(StartSessionError::Control(code, message)) => error_response(id, code, message),
        Err(StartSessionError::Registry(error)) => server_error_response(id, error),
        Err(StartSessionError::Core(error)) => core_error_response(id, error),
        Err(StartSessionError::Io(error)) => {
            error_response(id, ControlErrorCode::IoError, error.to_string())
        }
        Err(StartSessionError::Worker(error)) => error_response(
            id,
            ControlErrorCode::WorkerUnavailable,
            format!("failed to launch worker: {error}"),
        ),
    }
}

fn start_session(
    paths: &StatePaths,
    request: SessionStartRequest,
) -> Result<SessionStartResponse, StartSessionError> {
    if request.argv.is_empty() {
        return Err(StartSessionError::Control(
            ControlErrorCode::InvalidRequest,
            "session.start argv must contain at least one argument".to_string(),
        ));
    }
    let argv = request.argv;
    let requested_session_id = request.session_id;
    let monitor_profile = request.monitor_profile;
    let env = request.env;

    let cwd = match request.cwd {
        Some(cwd) => cwd,
        None => env::current_dir()?,
    };
    if !cwd.exists() {
        return Err(StartSessionError::Control(
            ControlErrorCode::WorkspaceNotFound,
            format!("cwd does not exist: {}", cwd.display()),
        ));
    }
    if !cwd.is_dir() {
        return Err(StartSessionError::Control(
            ControlErrorCode::InvalidRequest,
            format!("cwd is not a directory: {}", cwd.display()),
        ));
    }
    let cwd = cwd.canonicalize()?;

    let role = request.role.unwrap_or(SessionRole::Shell);
    let workspace = match request.workspace {
        Some(workspace) => Some(WorkspaceIdentity::capture(workspace)?),
        None => None,
    };
    let mut status_probe_issue = None;

    if role == SessionRole::MillraceDaemon {
        let Some(workspace) = &workspace else {
            return Err(StartSessionError::Control(
                ControlErrorCode::InvalidRequest,
                "millrace-daemon sessions require --workspace".to_string(),
            ));
        };

        let registry = HostRegistry::load(paths.clone())?;
        if let Some(record) = registry.find_active_millrace_daemon(workspace) {
            if record.meta.argv == argv {
                return Ok(SessionStartResponse {
                    schema_version: M1_PROTOCOL_VERSION,
                    protocol_version: M1_PROTOCOL_VERSION,
                    session: summary_from_meta(&record.meta),
                    attached_existing: true,
                });
            }
            return Err(StartSessionError::Control(
                ControlErrorCode::DuplicateMillraceDaemon,
                format!(
                    "duplicate millrace-daemon for workspace {} conflicts with active session {}",
                    workspace.canonical_path.display(),
                    record.meta.id
                ),
            ));
        }

        match probe_millrace_status(&workspace.canonical_path) {
            MillraceStatusProbe::Running => {
                return Err(StartSessionError::Control(
                    ControlErrorCode::DuplicateMillraceDaemon,
                    format!(
                        "millrace status reports a running daemon for workspace {}",
                        workspace.canonical_path.display()
                    ),
                ));
            }
            MillraceStatusProbe::NotRunning => {}
            MillraceStatusProbe::Issue(issue) => {
                status_probe_issue = Some(issue);
            }
        }
    }

    let session_id = requested_session_id.unwrap_or_default();
    let session_paths = paths.session_paths(session_id);
    if session_paths.root.exists() {
        return Err(StartSessionError::Control(
            ControlErrorCode::InvalidRequest,
            format!("session id {session_id} already exists"),
        ));
    }
    create_private_dir_all(&session_paths.root)?;
    let now = current_timestamp();
    let mut meta = SessionMeta {
        id: session_id,
        name: request.name,
        role,
        process_state: ProcessState::Starting,
        attention_state: AttentionState::Active,
        workspace,
        cwd,
        argv,
        monitor_profile,
        env,
        worker_pid: None,
        child_pid: None,
        child_pgid: None,
        started_at: None,
        ended_at: None,
        exit_code: None,
        exit_signal: None,
        failure_message: None,
        created_at: now.clone(),
        updated_at: now,
    };
    write_json_atomic(&session_paths.meta_json, &meta)?;
    append_event(
        &session_paths.events_jsonl,
        &SessionEvent::new(session_id, SessionEventKind::SessionCreated),
    )?;
    if let Some(issue) = status_probe_issue {
        append_event(
            &session_paths.events_jsonl,
            &issue.into_event(session_id, meta.workspace.as_ref()),
        )?;
    }

    match launch_worker(paths, session_id) {
        Ok(worker) => {
            meta = read_json(&session_paths.meta_json).unwrap_or(meta);
            meta.worker_pid = Some(worker.pid());
            meta.updated_at = current_timestamp();
            write_json_atomic(&session_paths.meta_json, &meta)?;

            let mut event = SessionEvent::new(session_id, SessionEventKind::WorkerStarted);
            event.process_state = Some(meta.process_state.clone());
            event
                .fields
                .insert("worker_pid".to_string(), worker.pid().to_string());
            append_event(&session_paths.events_jsonl, &event)?;
        }
        Err(error) => {
            meta.process_state = ProcessState::FailedStart;
            meta.failure_message = Some(error.to_string());
            meta.ended_at = Some(current_timestamp());
            meta.updated_at = current_timestamp();
            write_json_atomic(&session_paths.meta_json, &meta)?;
            let mut event = SessionEvent::new(session_id, SessionEventKind::StateChanged);
            event.process_state = Some(ProcessState::FailedStart);
            event.message = Some(error.to_string());
            append_event(&session_paths.events_jsonl, &event)?;
            return Err(StartSessionError::Worker(error));
        }
    }

    Ok(SessionStartResponse {
        schema_version: M1_PROTOCOL_VERSION,
        protocol_version: M1_PROTOCOL_VERSION,
        session: summary_from_meta(&meta),
        attached_existing: false,
    })
}

enum MillraceStatusProbe {
    Running,
    NotRunning,
    Issue(MillraceStatusProbeIssue),
}

struct MillraceStatusProbeIssue {
    outcome: &'static str,
    message: String,
    stderr: Option<String>,
}

impl MillraceStatusProbeIssue {
    fn into_event(
        self,
        session_id: SessionId,
        workspace: Option<&WorkspaceIdentity>,
    ) -> SessionEvent {
        let mut event = SessionEvent::new(session_id, SessionEventKind::MillraceStatusProbe);
        event.message = Some(self.message);
        event
            .fields
            .insert("outcome".to_string(), self.outcome.to_string());
        if let Some(workspace) = workspace {
            event.fields.insert(
                "workspace".to_string(),
                workspace.canonical_path.display().to_string(),
            );
        }
        if let Some(stderr) = self.stderr.filter(|stderr| !stderr.is_empty()) {
            event.fields.insert("stderr".to_string(), stderr);
        }
        event
    }
}

fn probe_millrace_status(workspace: &Path) -> MillraceStatusProbe {
    match Command::new("millrace")
        .arg("status")
        .arg("--format")
        .arg("json")
        .arg("--workspace")
        .arg(workspace)
        .output()
    {
        Ok(output) if output.status.success() => {
            match serde_json::from_slice::<Value>(&output.stdout) {
                Ok(value) if value_process_running(&value) => MillraceStatusProbe::Running,
                Ok(_) => MillraceStatusProbe::NotRunning,
                Err(error) => MillraceStatusProbe::Issue(MillraceStatusProbeIssue {
                    outcome: "unusable_json",
                    message: format!("millrace status returned unusable JSON: {error}"),
                    stderr: Some(String::from_utf8_lossy(&output.stderr).trim().to_string()),
                }),
            }
        }
        Ok(output) => MillraceStatusProbe::Issue(MillraceStatusProbeIssue {
            outcome: "nonzero_exit",
            message: format!(
                "millrace status exited with status {}",
                output
                    .status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "signal".to_string())
            ),
            stderr: Some(String::from_utf8_lossy(&output.stderr).trim().to_string()),
        }),
        Err(error) => MillraceStatusProbe::Issue(MillraceStatusProbeIssue {
            outcome: "unavailable",
            message: format!("failed to run millrace status: {error}"),
            stderr: None,
        }),
    }
}

fn value_process_running(value: &Value) -> bool {
    value
        .get("process_running")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn dispatch_host_status(
    id: String,
    params: Value,
    paths: &StatePaths,
    host: &HostMeta,
) -> ControlResponse {
    if let Err(error) = serde_json::from_value::<HostStatusRequest>(params) {
        return error_response(
            id,
            ControlErrorCode::InvalidRequest,
            format!("invalid host.status params: {error}"),
        );
    }

    let registry = match HostRegistry::load(paths.clone()) {
        Ok(registry) => registry,
        Err(error) => return server_error_response(id, error),
    };
    let result = HostStatusResponse {
        schema_version: M1_PROTOCOL_VERSION,
        protocol_version: M1_PROTOCOL_VERSION,
        host: Some(host.clone()),
        session_count: registry.active_count(),
    };
    success_response(id, &result)
}

fn dispatch_session_list(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<SessionListRequest>(params) {
        Ok(request) => request,
        Err(error) => {
            return error_response(
                id,
                ControlErrorCode::InvalidRequest,
                format!("invalid session.list params: {error}"),
            )
        }
    };

    let registry = match HostRegistry::load(paths.clone()) {
        Ok(registry) => registry,
        Err(error) => return server_error_response(id, error),
    };
    let result = SessionListResponse {
        schema_version: M1_PROTOCOL_VERSION,
        protocol_version: M1_PROTOCOL_VERSION,
        sessions: registry.list(&request),
    };
    success_response(id, &result)
}

fn dispatch_session_inspect(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<SessionInspectRequest>(params) {
        Ok(request) => request,
        Err(error) => {
            return error_response(
                id,
                ControlErrorCode::InvalidRequest,
                format!("invalid session.inspect params: {error}"),
            )
        }
    };

    let registry = match HostRegistry::load(paths.clone()) {
        Ok(registry) => registry,
        Err(error) => return server_error_response(id, error),
    };

    match registry.inspect(&request.selector) {
        Some(result) => success_response(id, &result),
        None => error_response(
            id,
            ControlErrorCode::SessionNotFound,
            "session not found for selector",
        ),
    }
}

fn dispatch_session_logs(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<SessionLogsRequest>(params) {
        Ok(request) => request,
        Err(error) => {
            return error_response(
                id,
                ControlErrorCode::InvalidRequest,
                format!("invalid session.logs params: {error}"),
            )
        }
    };

    match build_logs_response(paths, &request, request.follow) {
        Ok(result) => success_response(id, &result),
        Err(error) => ControlResponse::failure(id, error),
    }
}

fn dispatch_session_events(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<SessionEventsRequest>(params) {
        Ok(request) => request,
        Err(error) => {
            return error_response(
                id,
                ControlErrorCode::InvalidRequest,
                format!("invalid session.events params: {error}"),
            )
        }
    };

    match build_events_response(paths, &request, request.follow) {
        Ok(result) => success_response(id, &result),
        Err(error) => ControlResponse::failure(id, error),
    }
}

fn dispatch_session_send(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<SessionSendRequest>(params) {
        Ok(request) => request,
        Err(error) => {
            return error_response(
                id,
                ControlErrorCode::InvalidRequest,
                format!("invalid session.send params: {error}"),
            )
        }
    };

    match send_to_session(paths, request, None) {
        Ok(result) => success_response(id, &result),
        Err(error) => ControlResponse::failure(id, error),
    }
}

fn dispatch_session_resize(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<SessionResizeRequest>(params) {
        Ok(request) => request,
        Err(error) => {
            return error_response(
                id,
                ControlErrorCode::InvalidRequest,
                format!("invalid session.resize params: {error}"),
            )
        }
    };

    match resize_session(paths, request) {
        Ok(result) => success_response(id, &result),
        Err(error) => ControlResponse::failure(id, error),
    }
}

fn dispatch_session_stop(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<SessionStopRequest>(params) {
        Ok(request) => request,
        Err(error) => {
            return error_response(
                id,
                ControlErrorCode::InvalidRequest,
                format!("invalid session.stop params: {error}"),
            )
        }
    };

    match stop_session(paths, request) {
        Ok(result) => success_response(id, &result),
        Err(error) => ControlResponse::failure(id, error),
    }
}

fn dispatch_session_kill(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<SessionKillRequest>(params) {
        Ok(request) => request,
        Err(error) => {
            return error_response(
                id,
                ControlErrorCode::InvalidRequest,
                format!("invalid session.kill params: {error}"),
            )
        }
    };

    match kill_session(paths, request) {
        Ok(result) => success_response(id, &result),
        Err(error) => ControlResponse::failure(id, error),
    }
}

fn dispatch_session_delete(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<SessionDeleteRequest>(params) {
        Ok(request) => request,
        Err(error) => {
            return error_response(
                id,
                ControlErrorCode::InvalidRequest,
                format!("invalid session.delete params: {error}"),
            )
        }
    };

    match delete_session(paths, request) {
        Ok(result) => success_response(id, &result),
        Err(error) => ControlResponse::failure(id, error),
    }
}

fn dispatch_ui_context_get(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<UiContextGetRequest>(params) {
        Ok(request) => request,
        Err(error) => {
            return error_response(
                id,
                ControlErrorCode::InvalidRequest,
                format!("invalid ui.context.get params: {error}"),
            )
        }
    };

    match resolve_ui_context(paths, request.ui_id) {
        Ok((context, context_paths)) => success_response(
            id,
            &UiContextGetResponse {
                schema_version: M1_PROTOCOL_VERSION,
                protocol_version: M1_PROTOCOL_VERSION,
                context,
                paths: context_paths,
            },
        ),
        Err(error) => ControlResponse::failure(id, error),
    }
}

fn dispatch_ui_context_set(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<UiContextSetRequest>(params) {
        Ok(request) => request,
        Err(error) => {
            return error_response(
                id,
                ControlErrorCode::InvalidRequest,
                format!("invalid ui.context.set params: {error}"),
            )
        }
    };

    match set_ui_context(paths, request) {
        Ok((context, context_paths)) => success_response(
            id,
            &UiContextSetResponse {
                schema_version: M1_PROTOCOL_VERSION,
                protocol_version: M1_PROTOCOL_VERSION,
                context,
                paths: context_paths,
            },
        ),
        Err(error) => ControlResponse::failure(id, error),
    }
}

fn dispatch_ui_context_list(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    if let Err(error) = serde_json::from_value::<UiContextListRequest>(params) {
        return error_response(
            id,
            ControlErrorCode::InvalidRequest,
            format!("invalid ui.context.list params: {error}"),
        );
    }

    match list_ui_contexts(paths) {
        Ok(contexts) => success_response(
            id,
            &UiContextListResponse {
                schema_version: M1_PROTOCOL_VERSION,
                protocol_version: M1_PROTOCOL_VERSION,
                contexts: contexts
                    .into_iter()
                    .map(|(context, paths)| UiContextListEntry { context, paths })
                    .collect(),
            },
        ),
        Err(error) => ControlResponse::failure(id, error),
    }
}

fn dispatch_ui_context_close(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<UiContextCloseRequest>(params) {
        Ok(request) => request,
        Err(error) => {
            return error_response(
                id,
                ControlErrorCode::InvalidRequest,
                format!("invalid ui.context.close params: {error}"),
            )
        }
    };

    match close_ui_context(paths, request.ui_id) {
        Ok(context_paths) => success_response(
            id,
            &UiContextCloseResponse {
                schema_version: M1_PROTOCOL_VERSION,
                protocol_version: M1_PROTOCOL_VERSION,
                ui_id: request.ui_id,
                closed: true,
                paths: context_paths,
            },
        ),
        Err(error) => ControlResponse::failure(id, error),
    }
}

fn build_logs_response(
    paths: &StatePaths,
    request: &SessionLogsRequest,
    include_follow: bool,
) -> Result<SessionLogsResponse, ControlErrorBody> {
    let inspected = resolve_session(paths, &request.selector)?;
    let lines = read_log_lines(&inspected.paths.pty_log, request.tail).map_err(control_io_error)?;
    Ok(SessionLogsResponse {
        schema_version: M1_PROTOCOL_VERSION,
        protocol_version: M1_PROTOCOL_VERSION,
        session_id: inspected.session.session_id,
        lines,
        follow: include_follow.then(|| stream_setup(StreamKind::Logs, true, false)),
    })
}

fn build_events_response(
    paths: &StatePaths,
    request: &SessionEventsRequest,
    include_follow: bool,
) -> Result<SessionEventsResponse, ControlErrorBody> {
    let inspected = resolve_session(paths, &request.selector)?;
    let events = read_event_lines(&inspected.paths.events_jsonl).map_err(control_core_error)?;
    Ok(SessionEventsResponse {
        schema_version: M1_PROTOCOL_VERSION,
        protocol_version: M1_PROTOCOL_VERSION,
        session_id: inspected.session.session_id,
        events,
        follow: include_follow.then(|| stream_setup(StreamKind::Events, true, false)),
    })
}

fn send_to_session(
    paths: &StatePaths,
    request: SessionSendRequest,
    owner: Option<String>,
) -> Result<SessionSendResponse, ControlErrorBody> {
    let inspected = resolve_running_session(paths, &request.selector)?;
    let worker: WorkerSendResponse = worker_request(
        &inspected.paths,
        WorkerControlMethod::Send,
        &WorkerSendRequest {
            text: request.text.clone(),
            owner,
        },
    )?;

    let mut event = SessionEvent::new(inspected.session.session_id, SessionEventKind::InputSent);
    event
        .fields
        .insert("bytes".to_string(), worker.bytes_sent.to_string());
    append_event(&inspected.paths.events_jsonl, &event).map_err(control_core_error)?;

    Ok(SessionSendResponse {
        schema_version: M1_PROTOCOL_VERSION,
        protocol_version: M1_PROTOCOL_VERSION,
        session_id: inspected.session.session_id,
        bytes_sent: worker.bytes_sent,
    })
}

fn resize_session(
    paths: &StatePaths,
    request: SessionResizeRequest,
) -> Result<SessionResizeResponse, ControlErrorBody> {
    if request.rows == 0 || request.cols == 0 {
        return Err(ControlErrorBody::new(
            ControlErrorCode::InvalidRequest,
            "resize rows and cols must be greater than zero",
        ));
    }

    let inspected = resolve_running_session(paths, &request.selector)?;
    let worker: WorkerResizeResponse = worker_request(
        &inspected.paths,
        WorkerControlMethod::Resize,
        &WorkerResizeRequest {
            rows: request.rows,
            cols: request.cols,
        },
    )?;

    let mut event = SessionEvent::new(inspected.session.session_id, SessionEventKind::Resize);
    event
        .fields
        .insert("rows".to_string(), worker.rows.to_string());
    event
        .fields
        .insert("cols".to_string(), worker.cols.to_string());
    append_event(&inspected.paths.events_jsonl, &event).map_err(control_core_error)?;

    Ok(SessionResizeResponse {
        schema_version: M1_PROTOCOL_VERSION,
        protocol_version: M1_PROTOCOL_VERSION,
        session_id: inspected.session.session_id,
        rows: worker.rows,
        cols: worker.cols,
    })
}

fn stop_session(
    paths: &StatePaths,
    request: SessionStopRequest,
) -> Result<SessionStopResponse, ControlErrorBody> {
    let inspected = resolve_running_session(paths, &request.selector)?;
    let grace = request
        .grace_seconds
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_STOP_GRACE);

    let mut event = SessionEvent::new(
        inspected.session.session_id,
        SessionEventKind::StopRequested,
    );
    event.process_state = Some(inspected.session.process_state.clone());
    event
        .fields
        .insert("grace_seconds".to_string(), grace.as_secs().to_string());
    append_event(&inspected.paths.events_jsonl, &event).map_err(control_core_error)?;

    if inspected.session.role == SessionRole::MillraceDaemon {
        let meta =
            read_json::<SessionMeta>(&inspected.paths.meta_json).map_err(control_core_error)?;
        stop::request_millrace_control_stop(&meta, &inspected.paths.events_jsonl)?;
        let state = wait_for_terminal_state(&inspected.paths.meta_json, grace)?;
        if !is_active_process_state(&state) {
            return Ok(SessionStopResponse {
                schema_version: M1_PROTOCOL_VERSION,
                protocol_version: M1_PROTOCOL_VERSION,
                session_id: inspected.session.session_id,
                process_state: state,
                stop_requested: true,
            });
        }
    }

    let interrupt_result = worker_request::<_, WorkerAckResponse>(
        &inspected.paths,
        WorkerControlMethod::PrepareStopInterrupt,
        &json!({}),
    );
    let mut state = wait_for_terminal_state(&inspected.paths.meta_json, grace)?;
    if is_active_process_state(&state) {
        let meta =
            read_json::<SessionMeta>(&inspected.paths.meta_json).map_err(control_core_error)?;
        stop::request_sigterm(&meta).or_else(|error| {
            if interrupt_result.is_err() {
                Err(error)
            } else {
                Ok(false)
            }
        })?;
        state = wait_for_terminal_state(&inspected.paths.meta_json, grace)?;
    }

    Ok(SessionStopResponse {
        schema_version: M1_PROTOCOL_VERSION,
        protocol_version: M1_PROTOCOL_VERSION,
        session_id: inspected.session.session_id,
        process_state: state,
        stop_requested: true,
    })
}

fn kill_session(
    paths: &StatePaths,
    request: SessionKillRequest,
) -> Result<SessionKillResponse, ControlErrorBody> {
    let inspected = resolve_running_session(paths, &request.selector)?;
    request_kill_for_inspected(&inspected)?;
    let state = settle_killed_state(&inspected.paths)?;

    Ok(SessionKillResponse {
        schema_version: M1_PROTOCOL_VERSION,
        protocol_version: M1_PROTOCOL_VERSION,
        session_id: inspected.session.session_id,
        process_state: state,
        kill_requested: true,
    })
}

fn request_kill_for_inspected(inspected: &SessionInspectResponse) -> Result<(), ControlErrorBody> {
    let mut event = SessionEvent::new(
        inspected.session.session_id,
        SessionEventKind::KillRequested,
    );
    event.process_state = Some(inspected.session.process_state.clone());
    event
        .fields
        .insert("kill_requested".to_string(), "true".to_string());
    append_event(&inspected.paths.events_jsonl, &event).map_err(control_core_error)?;

    let worker_result = worker_request::<_, WorkerAckResponse>(
        &inspected.paths,
        WorkerControlMethod::ForwardKill,
        &json!({}),
    );
    if worker_result.is_err() {
        let meta =
            read_json::<SessionMeta>(&inspected.paths.meta_json).map_err(control_core_error)?;
        stop::request_sigkill(&meta)?;
    }

    mark_session_killed(&inspected.paths)?;
    Ok(())
}

fn delete_session(
    paths: &StatePaths,
    request: SessionDeleteRequest,
) -> Result<SessionDeleteResponse, ControlErrorBody> {
    let inspected = resolve_session(paths, &request.selector)?;
    let session_id = inspected.session.session_id;
    let is_active_root = is_active_session_root(paths, &inspected.paths.root);

    if is_active_process_state(&inspected.session.process_state) {
        if !request.kill {
            return Err(ControlErrorBody::new(
                ControlErrorCode::UnsafeDeleteRunning,
                "refusing to delete a running session without --kill",
            )
            .with_details(json!({
                "session_id": session_id,
                "process_state": inspected.session.process_state,
            })));
        }
        request_kill_for_inspected(&inspected)?;
        let _ = settle_killed_state(&inspected.paths)?;
    }

    if request.purge {
        append_delete_event(&inspected.paths, true, false, None)?;
        append_purge_event(&inspected.paths)?;
        remove_worker_socket(&inspected.paths)?;
        remove_session_dir(&inspected.paths.root)?;
        return Ok(SessionDeleteResponse {
            schema_version: M1_PROTOCOL_VERSION,
            protocol_version: M1_PROTOCOL_VERSION,
            session_id,
            deleted: true,
            archived: false,
            purged: true,
            archive_path: None,
        });
    }

    if is_active_root {
        let archive_root = paths.archive_dir.join(session_id.to_string());
        if archive_root.exists() {
            return Err(ControlErrorBody::new(
                ControlErrorCode::InvalidRequest,
                format!("archive already exists for session {session_id}"),
            ));
        }
        append_delete_event(&inspected.paths, false, true, Some(&archive_root))?;
        append_archive_event(&inspected.paths, &archive_root)?;
        create_private_dir_all(&paths.archive_dir).map_err(control_core_error)?;
        fs::rename(&inspected.paths.root, &archive_root).map_err(control_io_error)?;
        remove_worker_socket(&inspected.paths)?;
        return Ok(SessionDeleteResponse {
            schema_version: M1_PROTOCOL_VERSION,
            protocol_version: M1_PROTOCOL_VERSION,
            session_id,
            deleted: true,
            archived: true,
            purged: false,
            archive_path: Some(archive_root),
        });
    }

    append_delete_event(&inspected.paths, false, true, Some(&inspected.paths.root))?;
    Ok(SessionDeleteResponse {
        schema_version: M1_PROTOCOL_VERSION,
        protocol_version: M1_PROTOCOL_VERSION,
        session_id,
        deleted: true,
        archived: true,
        purged: false,
        archive_path: Some(inspected.paths.root),
    })
}

fn resolve_ui_context(
    paths: &StatePaths,
    ui_id: Option<UiId>,
) -> Result<(UiContext, UiContextPaths), ControlErrorBody> {
    if let Some(ui_id) = ui_id {
        return load_ui_context(paths, ui_id);
    }

    let mut contexts = list_ui_contexts(paths)?;
    match contexts.len() {
        0 => Err(ControlErrorBody::new(
            ControlErrorCode::UiContextNotFound,
            "no active UI context found",
        )),
        1 => Ok(contexts.remove(0)),
        count => Err(ControlErrorBody::new(
            ControlErrorCode::AmbiguousUiContext,
            format!("multiple active UI contexts found ({count}); pass --ui or set MILLMUX_UI_ID"),
        )),
    }
}

fn list_ui_contexts(
    paths: &StatePaths,
) -> Result<Vec<(UiContext, UiContextPaths)>, ControlErrorBody> {
    let mut contexts = Vec::new();
    let entries = match fs::read_dir(&paths.views_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(contexts),
        Err(error) => return Err(control_io_error(error)),
    };

    for entry in entries {
        let entry = entry.map_err(control_io_error)?;
        if !entry.file_type().map_err(control_io_error)?.is_dir() {
            continue;
        }
        let file_name = entry.file_name();
        let Some(raw_ui_id) = file_name.to_str() else {
            continue;
        };
        let Ok(ui_id) = raw_ui_id.parse::<UiId>() else {
            continue;
        };
        let context_paths = paths.ui_context_paths(ui_id);
        if !context_paths.context_json.exists() {
            continue;
        }
        let context =
            read_json::<UiContext>(&context_paths.context_json).map_err(control_core_error)?;
        if context.ui_id != ui_id {
            return Err(ControlErrorBody::new(
                ControlErrorCode::InvalidRequest,
                format!(
                    "UI context {} has mismatched ui_id {}",
                    context_paths.context_json.display(),
                    context.ui_id
                ),
            ));
        }
        contexts.push((context, context_paths));
    }

    contexts.sort_by_key(|context| std::cmp::Reverse(context.0.updated_at));
    Ok(contexts)
}

fn load_ui_context(
    paths: &StatePaths,
    ui_id: UiId,
) -> Result<(UiContext, UiContextPaths), ControlErrorBody> {
    let context_paths = paths.ui_context_paths(ui_id);
    if !context_paths.context_json.exists() {
        return Err(ControlErrorBody::new(
            ControlErrorCode::UiContextNotFound,
            format!("UI context not found: {ui_id}"),
        ));
    }
    let context =
        read_json::<UiContext>(&context_paths.context_json).map_err(control_core_error)?;
    if context.ui_id != ui_id {
        return Err(ControlErrorBody::new(
            ControlErrorCode::InvalidRequest,
            format!(
                "UI context {} has mismatched ui_id {}",
                context_paths.context_json.display(),
                context.ui_id
            ),
        ));
    }
    Ok((context, context_paths))
}

fn set_ui_context(
    paths: &StatePaths,
    request: UiContextSetRequest,
) -> Result<(UiContext, UiContextPaths), ControlErrorBody> {
    let mut context = request.context;
    validate_ui_context(&context)?;
    let events = request.events;
    for event in &events {
        if event.ui_id != context.ui_id {
            return Err(ControlErrorBody::new(
                ControlErrorCode::InvalidRequest,
                format!(
                    "UI event for {} cannot be stored under context {}",
                    event.ui_id, context.ui_id
                ),
            ));
        }
    }
    let context_paths = paths.ui_context_paths(context.ui_id);
    let existed = context_paths.context_json.exists();

    create_private_dir_all(&paths.views_dir).map_err(control_core_error)?;
    context.updated_at = OffsetDateTime::now_utc();
    write_json_atomic(&context_paths.context_json, &context).map_err(control_core_error)?;

    if events.is_empty() && !existed {
        append_ui_event(
            &context_paths,
            new_ui_event(context.ui_id, UiEventKind::UiStarted),
        )?;
    } else {
        for mut event in events {
            if event.timestamp.trim().is_empty() {
                event.timestamp = current_timestamp();
            }
            append_ui_event(&context_paths, event)?;
        }
    }

    Ok((context, context_paths))
}

fn validate_ui_context(context: &UiContext) -> Result<(), ControlErrorBody> {
    if context.schema_version != M1_PROTOCOL_VERSION {
        return Err(ControlErrorBody::new(
            ControlErrorCode::InvalidRequest,
            format!(
                "unsupported UI context schema_version {}; expected {}",
                context.schema_version, M1_PROTOCOL_VERSION
            ),
        ));
    }
    Ok(())
}

fn close_ui_context(paths: &StatePaths, ui_id: UiId) -> Result<UiContextPaths, ControlErrorBody> {
    let (_context, context_paths) = load_ui_context(paths, ui_id)?;
    append_ui_event(&context_paths, new_ui_event(ui_id, UiEventKind::UiClosed))?;
    fs::remove_file(&context_paths.context_json).map_err(control_io_error)?;
    Ok(context_paths)
}

fn new_ui_event(ui_id: UiId, kind: UiEventKind) -> UiEvent {
    UiEvent {
        timestamp: current_timestamp(),
        ui_id,
        kind,
        message: None,
        fields: BTreeMap::new(),
    }
}

fn append_ui_event(paths: &UiContextPaths, event: UiEvent) -> Result<(), ControlErrorBody> {
    append_json_line(&paths.events_jsonl, &event).map_err(control_core_error)
}

fn resolve_session(
    paths: &StatePaths,
    selector: &millrace_sessions_core::protocol::SessionSelector,
) -> Result<SessionInspectResponse, ControlErrorBody> {
    let registry = HostRegistry::load(paths.clone()).map_err(|error| {
        ControlErrorBody::new(
            ControlErrorCode::IoError,
            format!("failed to load host registry: {error}"),
        )
    })?;
    registry.inspect(selector).ok_or_else(|| {
        ControlErrorBody::new(
            ControlErrorCode::SessionNotFound,
            "session not found for selector",
        )
    })
}

fn resolve_running_session(
    paths: &StatePaths,
    selector: &millrace_sessions_core::protocol::SessionSelector,
) -> Result<SessionInspectResponse, ControlErrorBody> {
    let inspected = resolve_session(paths, selector)?;
    if inspected.session.process_state != ProcessState::Running {
        return Err(ControlErrorBody::new(
            ControlErrorCode::SessionNotRunning,
            "selected session is not running",
        ));
    }
    Ok(inspected)
}

fn is_active_process_state(state: &ProcessState) -> bool {
    matches!(state, ProcessState::Starting | ProcessState::Running)
}

fn wait_for_terminal_state(
    meta_json: &Path,
    timeout: Duration,
) -> Result<ProcessState, ControlErrorBody> {
    let started = Instant::now();
    loop {
        let meta = read_json::<SessionMeta>(meta_json).map_err(control_core_error)?;
        if !is_active_process_state(&meta.process_state) || started.elapsed() >= timeout {
            return Ok(meta.process_state);
        }
        thread::sleep(FOLLOW_POLL);
    }
}

fn mark_session_killed(paths: &SessionPaths) -> Result<(), ControlErrorBody> {
    update_session_meta(paths, |meta, now| {
        meta.process_state = ProcessState::Killed;
        meta.ended_at.get_or_insert_with(|| now.to_string());
        meta.updated_at = now.to_string();
    })?;
    update_worker_meta(paths, |worker, now| {
        worker.process_state = ProcessState::Killed;
        worker.ended_at.get_or_insert_with(|| now.to_string());
        worker.updated_at = now.to_string();
    })?;

    let meta = read_json::<SessionMeta>(&paths.meta_json).map_err(control_core_error)?;
    let mut event = SessionEvent::new(meta.id, SessionEventKind::StateChanged);
    event.process_state = Some(ProcessState::Killed);
    event
        .fields
        .insert("kill_requested".to_string(), "true".to_string());
    append_event(&paths.events_jsonl, &event).map_err(control_core_error)
}

fn settle_killed_state(paths: &SessionPaths) -> Result<ProcessState, ControlErrorBody> {
    let state = wait_for_terminal_state(&paths.meta_json, KILL_SETTLE_TIMEOUT)?;
    if state != ProcessState::Killed {
        mark_session_killed(paths)?;
    }
    Ok(ProcessState::Killed)
}

fn update_session_meta<F>(paths: &SessionPaths, update: F) -> Result<(), ControlErrorBody>
where
    F: FnOnce(&mut SessionMeta, &str),
{
    let mut meta = read_json::<SessionMeta>(&paths.meta_json).map_err(control_core_error)?;
    let now = current_timestamp();
    update(&mut meta, &now);
    write_json_atomic(&paths.meta_json, &meta).map_err(control_core_error)
}

fn update_worker_meta<F>(paths: &SessionPaths, update: F) -> Result<(), ControlErrorBody>
where
    F: FnOnce(&mut WorkerMeta, &str),
{
    if !paths.worker_json.exists() {
        return Ok(());
    }
    let mut worker = read_json::<WorkerMeta>(&paths.worker_json).map_err(control_core_error)?;
    let now = current_timestamp();
    update(&mut worker, &now);
    write_json_atomic(&paths.worker_json, &worker).map_err(control_core_error)
}

fn append_delete_event(
    paths: &SessionPaths,
    purged: bool,
    archived: bool,
    archive_path: Option<&Path>,
) -> Result<(), ControlErrorBody> {
    let meta = read_json::<SessionMeta>(&paths.meta_json).map_err(control_core_error)?;
    let mut event = SessionEvent::new(meta.id, SessionEventKind::Deleted);
    event.process_state = Some(meta.process_state);
    event
        .fields
        .insert("purged".to_string(), purged.to_string());
    event
        .fields
        .insert("archived".to_string(), archived.to_string());
    if let Some(archive_path) = archive_path {
        event.fields.insert(
            "archive_path".to_string(),
            archive_path.display().to_string(),
        );
    }
    append_event(&paths.events_jsonl, &event).map_err(control_core_error)
}

fn append_archive_event(paths: &SessionPaths, archive_path: &Path) -> Result<(), ControlErrorBody> {
    let meta = read_json::<SessionMeta>(&paths.meta_json).map_err(control_core_error)?;
    let mut event = SessionEvent::new(meta.id, SessionEventKind::Archived);
    event.process_state = Some(meta.process_state);
    event.fields.insert(
        "archive_path".to_string(),
        archive_path.display().to_string(),
    );
    append_event(&paths.events_jsonl, &event).map_err(control_core_error)
}

fn append_purge_event(paths: &SessionPaths) -> Result<(), ControlErrorBody> {
    let meta = read_json::<SessionMeta>(&paths.meta_json).map_err(control_core_error)?;
    let mut event = SessionEvent::new(meta.id, SessionEventKind::Purged);
    event.process_state = Some(meta.process_state);
    event
        .fields
        .insert("purged".to_string(), "true".to_string());
    append_event(&paths.events_jsonl, &event).map_err(control_core_error)
}

fn remove_worker_socket(paths: &SessionPaths) -> Result<(), ControlErrorBody> {
    match fs::remove_file(&paths.worker_sock) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(control_io_error(error)),
    }
}

fn remove_session_dir(path: &Path) -> Result<(), ControlErrorBody> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(control_io_error(error)),
    }
}

fn is_active_session_root(paths: &StatePaths, root: &Path) -> bool {
    root.starts_with(&paths.sessions_dir)
}

fn read_log_lines(path: &std::path::Path, tail: Option<usize>) -> std::io::Result<Vec<LogLine>> {
    let raw = match fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    let text = String::from_utf8_lossy(&raw);
    let mut lines = text
        .lines()
        .map(|line| LogLine {
            timestamp: None,
            line: line.trim_end_matches('\r').to_string(),
        })
        .collect::<Vec<_>>();
    if let Some(tail) = tail {
        if lines.len() > tail {
            lines = lines.split_off(lines.len() - tail);
        }
    }
    Ok(lines)
}

fn read_event_lines(path: &std::path::Path) -> MillmuxResult<Vec<SessionEvent>> {
    match read_events(path) {
        Ok(events) => Ok(events),
        Err(MillmuxError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(Vec::new())
        }
        Err(error) => Err(error),
    }
}

fn stream_setup(kind: StreamKind, read_only: bool, input_owner: bool) -> StreamSetup {
    StreamSetup {
        stream_id: next_stream_id(),
        kind,
        read_only,
        input_owner,
    }
}

fn next_stream_id() -> String {
    let counter = STREAM_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("stream_{}_{}", std::process::id(), counter)
}

fn worker_request<P, R>(
    paths: &SessionPaths,
    method: WorkerControlMethod,
    params: &P,
) -> Result<R, ControlErrorBody>
where
    P: serde::Serialize,
    R: serde::de::DeserializeOwned,
{
    let request_id = next_stream_id();
    let request = WorkerControlRequest::with_params(request_id.clone(), method, params)
        .map_err(control_json_error)?;
    let mut stream = connect_worker(&paths.worker_sock)?;
    stream
        .write_all(
            request
                .to_json_line()
                .map_err(control_json_error)?
                .as_bytes(),
        )
        .map_err(control_io_error)?;
    stream.flush().map_err(control_io_error)?;

    let mut line = String::new();
    StdBufReader::new(stream)
        .read_line(&mut line)
        .map_err(control_io_error)?;
    let response = WorkerControlResponse::from_json_line(&line).map_err(control_json_error)?;
    if response.id != request_id {
        return Err(ControlErrorBody::new(
            ControlErrorCode::WorkerUnavailable,
            format!(
                "worker response id {} did not match request id {request_id}",
                response.id
            ),
        ));
    }
    if !response.ok {
        return Err(response.error.unwrap_or_else(|| {
            ControlErrorBody::new(
                ControlErrorCode::WorkerUnavailable,
                "worker returned an error without an error body",
            )
        }));
    }
    response.result_as::<R>().map_err(control_json_error)
}

fn connect_worker(path: &std::path::Path) -> Result<StdUnixStream, ControlErrorBody> {
    let started = Instant::now();
    let mut last_error = None;
    while started.elapsed() < WORKER_CONNECT_TIMEOUT {
        match StdUnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(error) => {
                last_error = Some(error);
                thread::sleep(WORKER_CONNECT_POLL);
            }
        }
    }

    Err(ControlErrorBody::new(
        ControlErrorCode::WorkerUnavailable,
        format!(
            "worker socket unavailable at {}: {}",
            path.display(),
            last_error
                .map(|error| error.to_string())
                .unwrap_or_else(|| "timed out".to_string())
        ),
    ))
}

fn control_json_error(error: serde_json::Error) -> ControlErrorBody {
    ControlErrorBody::new(ControlErrorCode::InvalidRequest, error.to_string())
}

fn control_io_error(error: std::io::Error) -> ControlErrorBody {
    ControlErrorBody::new(ControlErrorCode::IoError, error.to_string())
}

fn control_core_error(error: MillmuxError) -> ControlErrorBody {
    let code = match &error {
        MillmuxError::InvalidRequest(_) | MillmuxError::InvalidProtocolData(_) => {
            ControlErrorCode::InvalidRequest
        }
        MillmuxError::WorkspaceNotFound(_) => ControlErrorCode::WorkspaceNotFound,
        MillmuxError::Permission(_) => ControlErrorCode::PermissionError,
        MillmuxError::WorkerUnavailable(_) => ControlErrorCode::WorkerUnavailable,
        MillmuxError::Io(_) => ControlErrorCode::IoError,
        _ => ControlErrorCode::InternalError,
    };
    ControlErrorBody::new(code, error.to_string())
}

async fn handle_logs_follow_stream(
    request: millrace_sessions_core::protocol::ControlRequest,
    mut writer: OwnedWriteHalf,
    paths: &StatePaths,
) -> Result<(), HostServerError> {
    let params = match request.params_as::<SessionLogsRequest>() {
        Ok(params) => params,
        Err(error) => {
            let response = error_response(
                request.id,
                ControlErrorCode::InvalidRequest,
                format!("invalid session.logs params: {error}"),
            );
            writer
                .write_all(response.to_json_line()?.as_bytes())
                .await?;
            writer.flush().await?;
            return Ok(());
        }
    };

    let inspected = match resolve_session(paths, &params.selector) {
        Ok(inspected) => inspected,
        Err(error) => {
            writer
                .write_all(
                    ControlResponse::failure(request.id, error)
                        .to_json_line()?
                        .as_bytes(),
                )
                .await?;
            writer.flush().await?;
            return Ok(());
        }
    };
    let response = match build_logs_response(paths, &params, true) {
        Ok(result) => ControlResponse::success(request.id.clone(), &result)?,
        Err(error) => ControlResponse::failure(request.id.clone(), error),
    };
    writer
        .write_all(response.to_json_line()?.as_bytes())
        .await?;
    writer.flush().await?;

    let mut offset = file_len(&inspected.paths.pty_log);
    loop {
        sleep(FOLLOW_POLL).await;
        let next = match read_from_offset(&inspected.paths.pty_log, offset) {
            Ok((bytes, new_offset)) => {
                offset = new_offset;
                bytes
            }
            Err(_) => Vec::new(),
        };
        if next.is_empty() {
            if session_is_terminal(&inspected.paths.meta_json) {
                let frame = LogStreamFrame::Closed;
                let _ = writer.write_all(frame.to_json_line()?.as_bytes()).await;
                let _ = writer.flush().await;
                break;
            }
            continue;
        }

        for line in String::from_utf8_lossy(&next).lines() {
            let frame = LogStreamFrame::Line {
                line: LogLine {
                    timestamp: None,
                    line: line.trim_end_matches('\r').to_string(),
                },
            };
            if writer
                .write_all(frame.to_json_line()?.as_bytes())
                .await
                .is_err()
            {
                break;
            }
            writer.flush().await?;
        }
    }

    Ok(())
}

async fn handle_events_follow_stream(
    request: millrace_sessions_core::protocol::ControlRequest,
    mut writer: OwnedWriteHalf,
    paths: &StatePaths,
) -> Result<(), HostServerError> {
    let params = match request.params_as::<SessionEventsRequest>() {
        Ok(params) => params,
        Err(error) => {
            let response = error_response(
                request.id,
                ControlErrorCode::InvalidRequest,
                format!("invalid session.events params: {error}"),
            );
            writer
                .write_all(response.to_json_line()?.as_bytes())
                .await?;
            writer.flush().await?;
            return Ok(());
        }
    };

    let inspected = match resolve_session(paths, &params.selector) {
        Ok(inspected) => inspected,
        Err(error) => {
            writer
                .write_all(
                    ControlResponse::failure(request.id, error)
                        .to_json_line()?
                        .as_bytes(),
                )
                .await?;
            writer.flush().await?;
            return Ok(());
        }
    };
    let response = match build_events_response(paths, &params, true) {
        Ok(result) => ControlResponse::success(request.id.clone(), &result)?,
        Err(error) => ControlResponse::failure(request.id.clone(), error),
    };
    writer
        .write_all(response.to_json_line()?.as_bytes())
        .await?;
    writer.flush().await?;

    let mut offset = file_len(&inspected.paths.events_jsonl);
    loop {
        sleep(FOLLOW_POLL).await;
        let next = match read_from_offset(&inspected.paths.events_jsonl, offset) {
            Ok((bytes, new_offset)) => {
                offset = new_offset;
                bytes
            }
            Err(_) => Vec::new(),
        };
        if next.is_empty() {
            if session_is_terminal(&inspected.paths.meta_json) {
                let frame = EventStreamFrame::Closed;
                let _ = writer.write_all(frame.to_json_line()?.as_bytes()).await;
                let _ = writer.flush().await;
                break;
            }
            continue;
        }

        for line in String::from_utf8_lossy(&next).lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(event) = serde_json::from_str::<SessionEvent>(line) else {
                continue;
            };
            let frame = EventStreamFrame::Event { event };
            if writer
                .write_all(frame.to_json_line()?.as_bytes())
                .await
                .is_err()
            {
                break;
            }
            writer.flush().await?;
        }
    }

    Ok(())
}

async fn handle_attach_stream(
    request: millrace_sessions_core::protocol::ControlRequest,
    mut lines: Lines<TokioBufReader<OwnedReadHalf>>,
    mut writer: OwnedWriteHalf,
    paths: &StatePaths,
) -> Result<(), HostServerError> {
    let params = match request.params_as::<SessionAttachRequest>() {
        Ok(params) => params,
        Err(error) => {
            let response = error_response(
                request.id,
                ControlErrorCode::InvalidRequest,
                format!("invalid session.attach params: {error}"),
            );
            writer
                .write_all(response.to_json_line()?.as_bytes())
                .await?;
            writer.flush().await?;
            return Ok(());
        }
    };

    let opened = match open_attach(paths, &params) {
        Ok(opened) => opened,
        Err(error) => {
            writer
                .write_all(
                    ControlResponse::failure(request.id, error)
                        .to_json_line()?
                        .as_bytes(),
                )
                .await?;
            writer.flush().await?;
            return Ok(());
        }
    };

    let cleanup = AttachCleanupGuard::new(opened);

    let response = ControlResponse::success(request.id, &cleanup.opened().response)?;
    writer
        .write_all(response.to_json_line()?.as_bytes())
        .await?;
    writer.flush().await?;

    let mut offset = if params.include_scrollback {
        0
    } else {
        file_len(&cleanup.opened().paths.pty_log)
    };
    if params.include_scrollback {
        if let Ok(scrollback) =
            ScrollbackBuffer::restore_snapshot(&cleanup.opened().paths.scrollback_snapshot)
        {
            let frame = AttachStreamFrame::Scrollback {
                lines: scrollback.lines(),
            };
            writer.write_all(frame.to_json_line()?.as_bytes()).await?;
            writer.flush().await?;
            offset = file_len(&cleanup.opened().paths.pty_log);
        }
    }

    loop {
        tokio::select! {
            input = lines.next_line() => {
                match input? {
                    Some(line) => {
                        if handle_attach_input_frame(&line, cleanup.opened(), &mut writer).await? {
                            break;
                        }
                    }
                    None => break,
                }
            }
            _ = sleep(FOLLOW_POLL) => {
                let next = match read_from_offset(&cleanup.opened().paths.pty_log, offset) {
                    Ok((bytes, new_offset)) => {
                        offset = new_offset;
                        bytes
                    }
                    Err(_) => Vec::new(),
                };
                if !next.is_empty() {
                    let frame = AttachStreamFrame::Output {
                        text: String::from_utf8_lossy(&next).to_string(),
                    };
                    if writer.write_all(frame.to_json_line()?.as_bytes()).await.is_err() {
                        break;
                    }
                    writer.flush().await?;
                } else if session_is_terminal(&cleanup.opened().paths.meta_json) {
                    break;
                }
            }
        }
    }

    cleanup.close();
    let frame = AttachStreamFrame::Closed;
    let _ = writer.write_all(frame.to_json_line()?.as_bytes()).await;
    let _ = writer.flush().await;
    Ok(())
}

struct OpenAttach {
    paths: SessionPaths,
    stream_id: String,
    input_owner: bool,
    response: SessionAttachResponse,
}

struct AttachCleanupGuard {
    opened: Option<OpenAttach>,
}

impl AttachCleanupGuard {
    fn new(opened: OpenAttach) -> Self {
        Self {
            opened: Some(opened),
        }
    }

    fn opened(&self) -> &OpenAttach {
        self.opened
            .as_ref()
            .expect("attach cleanup guard missing open attach")
    }

    fn close(mut self) {
        if let Some(opened) = self.opened.take() {
            close_attach(&opened);
        }
    }
}

impl Drop for AttachCleanupGuard {
    fn drop(&mut self) {
        if let Some(opened) = self.opened.take() {
            close_attach(&opened);
        }
    }
}

fn open_attach(
    paths: &StatePaths,
    request: &SessionAttachRequest,
) -> Result<OpenAttach, ControlErrorBody> {
    let inspected = resolve_running_session(paths, &request.selector)?;
    let stream_id = next_stream_id();
    let worker: WorkerAttachResponse = worker_request(
        &inspected.paths,
        WorkerControlMethod::AcquireAttach,
        &WorkerAttachRequest {
            stream_id: stream_id.clone(),
            read_only: request.read_only,
            include_scrollback: request.include_scrollback,
        },
    )?;

    let mut event = SessionEvent::new(inspected.session.session_id, SessionEventKind::AttachOpened);
    event
        .fields
        .insert("stream_id".to_string(), stream_id.clone());
    event
        .fields
        .insert("read_only".to_string(), worker.read_only.to_string());
    event
        .fields
        .insert("input_owner".to_string(), worker.input_owner.to_string());
    append_event(&inspected.paths.events_jsonl, &event).map_err(control_core_error)?;

    let response = SessionAttachResponse {
        schema_version: M1_PROTOCOL_VERSION,
        protocol_version: M1_PROTOCOL_VERSION,
        session_id: inspected.session.session_id,
        stream: StreamSetup {
            stream_id: stream_id.clone(),
            kind: StreamKind::Attach,
            read_only: worker.read_only,
            input_owner: worker.input_owner,
        },
    };

    Ok(OpenAttach {
        paths: inspected.paths,
        stream_id,
        input_owner: worker.input_owner,
        response,
    })
}

async fn handle_attach_input_frame(
    line: &str,
    opened: &OpenAttach,
    writer: &mut OwnedWriteHalf,
) -> Result<bool, HostServerError> {
    let frame = match AttachStreamFrame::from_json_line(line) {
        Ok(frame) => frame,
        Err(error) => {
            let frame = AttachStreamFrame::Error {
                error: ControlErrorBody::new(
                    ControlErrorCode::InvalidRequest,
                    format!("invalid attach stream frame: {error}"),
                ),
            };
            writer.write_all(frame.to_json_line()?.as_bytes()).await?;
            writer.flush().await?;
            return Ok(false);
        }
    };

    match frame {
        AttachStreamFrame::Input { text } => {
            if !opened.input_owner {
                let frame = AttachStreamFrame::Error {
                    error: ControlErrorBody::new(
                        ControlErrorCode::InputOwnerConflict,
                        "attach stream does not own PTY input",
                    ),
                };
                writer.write_all(frame.to_json_line()?.as_bytes()).await?;
                writer.flush().await?;
                return Ok(false);
            }
            match worker_request::<_, WorkerSendResponse>(
                &opened.paths,
                WorkerControlMethod::Send,
                &WorkerSendRequest {
                    text,
                    owner: Some(opened.stream_id.clone()),
                },
            ) {
                Ok(_) => Ok(false),
                Err(error) => {
                    let frame = AttachStreamFrame::Error { error };
                    writer.write_all(frame.to_json_line()?.as_bytes()).await?;
                    writer.flush().await?;
                    Ok(false)
                }
            }
        }
        AttachStreamFrame::Resize { rows, cols } => {
            let _ = worker_request::<_, WorkerResizeResponse>(
                &opened.paths,
                WorkerControlMethod::Resize,
                &WorkerResizeRequest { rows, cols },
            );
            Ok(false)
        }
        AttachStreamFrame::Close => Ok(true),
        _ => Ok(false),
    }
}

fn close_attach(opened: &OpenAttach) {
    let _ = worker_request::<_, WorkerAckResponse>(
        &opened.paths,
        WorkerControlMethod::ReleaseAttach,
        &WorkerReleaseAttachRequest {
            stream_id: opened.stream_id.clone(),
        },
    );
    let mut event = SessionEvent::new(opened.response.session_id, SessionEventKind::AttachClosed);
    event
        .fields
        .insert("stream_id".to_string(), opened.stream_id.clone());
    let _ = append_event(&opened.paths.events_jsonl, &event);
}

fn file_len(path: &std::path::Path) -> u64 {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn read_from_offset(path: &std::path::Path, offset: u64) -> std::io::Result<(Vec<u8>, u64)> {
    let mut file = fs::OpenOptions::new().read(true).open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let new_offset = offset + bytes.len() as u64;
    Ok((bytes, new_offset))
}

fn session_is_terminal(meta_json: &std::path::Path) -> bool {
    read_json::<SessionMeta>(meta_json)
        .map(|meta| {
            !matches!(
                meta.process_state,
                ProcessState::Starting | ProcessState::Running
            )
        })
        .unwrap_or(false)
}

fn success_response<T: serde::Serialize>(id: String, result: &T) -> ControlResponse {
    ControlResponse::success(id.clone(), result).unwrap_or_else(|error| {
        error_response(
            id,
            ControlErrorCode::InternalError,
            format!("failed to serialize response: {error}"),
        )
    })
}

fn server_error_response(id: String, error: RegistryError) -> ControlResponse {
    error_response(
        id,
        ControlErrorCode::IoError,
        format!("failed to load host registry: {error}"),
    )
}

fn core_error_response(id: String, error: MillmuxError) -> ControlResponse {
    let code = match &error {
        MillmuxError::InvalidRequest(_) | MillmuxError::InvalidProtocolData(_) => {
            ControlErrorCode::InvalidRequest
        }
        MillmuxError::WorkspaceNotFound(_) => ControlErrorCode::WorkspaceNotFound,
        MillmuxError::DuplicateDaemon(_) => ControlErrorCode::DuplicateMillraceDaemon,
        MillmuxError::CommandNotFound(_) => ControlErrorCode::CommandNotFound,
        MillmuxError::Permission(_) => ControlErrorCode::PermissionError,
        MillmuxError::WorkerUnavailable(_) => ControlErrorCode::WorkerUnavailable,
        MillmuxError::Io(_) => ControlErrorCode::IoError,
        _ => ControlErrorCode::InternalError,
    };
    error_response(id, code, error.to_string())
}

fn error_response(
    id: impl Into<String>,
    code: ControlErrorCode,
    message: impl Into<String>,
) -> ControlResponse {
    ControlResponse::failure(id, ControlErrorBody::new(code, message))
}

#[derive(Debug, Error)]
enum StartSessionError {
    #[error("{1}")]
    Control(ControlErrorCode, String),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Core(#[from] MillmuxError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Worker(#[from] WorkerLaunchError),
}

fn summary_from_meta(meta: &SessionMeta) -> SessionSummary {
    SessionSummary {
        session_id: meta.id,
        name: meta.name.clone(),
        role: meta.role.clone(),
        process_state: meta.process_state.clone(),
        attention_state: meta.attention_state.clone(),
        workspace: meta.workspace.clone(),
        cwd: meta.cwd.clone(),
        argv: meta.argv.clone(),
        monitor_profile: monitor_profile_from_meta(meta),
        created_at: meta.created_at.clone(),
        updated_at: meta.updated_at.clone(),
        attached_clients: 0,
    }
}

fn monitor_profile_from_meta(meta: &SessionMeta) -> MonitorProfile {
    if !meta.monitor_profile.is_auto() {
        return meta.monitor_profile.clone();
    }
    monitor_profile_from_argv(&meta.argv).unwrap_or_default()
}

fn monitor_profile_from_argv(argv: &[String]) -> Option<MonitorProfile> {
    let mut args = argv.iter();
    while let Some(arg) = args.next() {
        if arg == "--monitor" {
            return args
                .next()
                .and_then(|value| value.parse::<MonitorProfile>().ok());
        }
        if let Some(value) = arg.strip_prefix("--monitor=") {
            return value.parse::<MonitorProfile>().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use millrace_sessions_core::state::HostMeta;

    use super::*;

    #[test]
    fn server_dispatch_returns_invalid_request_for_bad_json() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StatePaths::new(temp.path().join("state"));
        let host = HostMeta {
            pid: 123,
            state_root: paths.root.clone(),
            control_socket: paths.control_sock.clone(),
            started_at: "2026-05-20T18:00:00Z".to_string(),
            updated_at: "2026-05-20T18:00:00Z".to_string(),
        };

        let response = dispatch_json_line("not-json", &paths, &host);

        assert!(!response.ok);
        assert_eq!(
            response.error.expect("error").code,
            ControlErrorCode::InvalidRequest
        );
    }
}
