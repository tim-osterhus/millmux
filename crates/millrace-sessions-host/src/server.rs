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
        ApiCapabilitiesRequest, ApiCapabilitiesResponse, ApiCapability, ApiEnvelopeDescription,
        ApiIdentifyRequest, ApiIdentifyResponse, ApiStability, AttachStreamFrame,
        AttentionClearRequest, AttentionListRequest, AttentionListResponse, AttentionMarkRequest,
        AttentionMutationResponse, AttentionReadRequest, ControlErrorBody, ControlErrorCode,
        ControlMethod, ControlResponse, DoctorRequest, EventStreamFrame, EventStreamLagReason,
        EventSubscribeRequest, EventSubscribeResponse, EventSubscriptionContract,
        HostStatusRequest, HostStatusResponse, InputSendRequest, InputSendResponse, InputTarget,
        LogLine, LogStream, LogStreamFrame, ScreenFrame, SessionArtifacts, SessionAttachRequest,
        SessionAttachResponse, SessionCapabilities, SessionDeleteRequest, SessionDeleteResponse,
        SessionEventsRequest, SessionEventsResponse, SessionInspectRequest, SessionInspectResponse,
        SessionKillRequest, SessionKillResponse, SessionListRequest, SessionListResponse,
        SessionLogsRequest, SessionLogsResponse, SessionResizeRequest, SessionResizeResponse,
        SessionScreenRequest, SessionScreenResponse, SessionSelector, SessionSendRequest,
        SessionSendResponse, SessionStartRequest, SessionStartResponse, SessionStopRequest,
        SessionStopResponse, SessionSummary, SnapshotUnavailableReason, StreamKind, StreamSetup,
        UiContextCloseRequest, UiContextCloseResponse, UiContextGetRequest, UiContextGetResponse,
        UiContextListEntry, UiContextListRequest, UiContextListResponse, UiContextSetRequest,
        UiContextSetResponse, WorkerAckResponse, WorkerAttachRequest, WorkerAttachResponse,
        WorkerControlMethod, WorkerControlRequest, WorkerControlResponse, WorkerResizeRequest,
        WorkerResizeResponse, WorkerSendRequest, WorkerSendResponse, M1_PROTOCOL_VERSION,
        V04_API_SCHEMA, V04_API_VERSION,
    },
    scrollback::TerminalSnapshot,
    state::{
        AttentionItem, AttentionKind, AttentionRollup, AttentionState, AttentionTargetType,
        HostMeta, MonitorProfile, ProcessState, SessionLiveness, SessionMeta, SessionPaths,
        SessionRole, StatusSummary, UiContext, UiContextPaths, UiEvent, UiEventKind,
        UiPaneViewKind, UiPaneViewMode, WorkerMeta,
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
const DEFAULT_STOP_REASON: &str = "session_stop";
const SIGTERM_STOP_REASON: &str = "sigterm_stop";
const SIGTERM_FALLBACK_STOP_REASON: &str = "sigterm_fallback";
const TAIL_READ_CHUNK_BYTES: u64 = 8192;
const MAX_TAIL_READ_BYTES: u64 = 1024 * 1024;

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
    if method == "events.subscribe" {
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
        ControlMethod::SessionAttach => {
            handle_attach_stream(request, lines, writer, runtime.paths()).await
        }
        ControlMethod::SessionLogs => {
            handle_logs_follow_stream(request, writer, runtime.paths()).await
        }
        ControlMethod::SessionEvents => {
            handle_events_follow_stream(request, writer, runtime.paths()).await
        }
        ControlMethod::EventsSubscribe => {
            handle_events_subscribe_stream(request, writer, runtime.paths()).await
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
    let version = value.get("version").and_then(Value::as_str);
    let params = value.get("params").cloned().unwrap_or_else(|| json!({}));

    let response = match method {
        "host.status" => dispatch_host_status(id, params, paths, host),
        "host.doctor" => dispatch_host_doctor(id, params, paths, host),
        "session.start" => dispatch_session_start(id, params, paths),
        "session.list" => dispatch_session_list(id, params, paths),
        "session.status" => dispatch_session_inspect(id, params, paths),
        "session.inspect" => dispatch_session_inspect(id, params, paths),
        "session.screen" => dispatch_session_screen(id, params, paths),
        "session.logs" => dispatch_session_logs(id, params, paths),
        "session.events" => dispatch_session_events(id, params, paths),
        "session.send" => dispatch_session_send(id, params, paths),
        "session.resize" => dispatch_session_resize(id, params, paths),
        "session.stop" => dispatch_session_stop(id, params, paths),
        "session.kill" => dispatch_session_kill(id, params, paths),
        "session.delete" => dispatch_session_delete(id, params, paths),
        "input.send" => dispatch_input_send(id, params, paths),
        "events.subscribe" => dispatch_events_subscribe(id, params, paths),
        "attention.list" => dispatch_attention_list(id, params, paths),
        "attention.mark" => dispatch_attention_mark(id, params, paths),
        "attention.read" => dispatch_attention_read(id, params, paths),
        "attention.clear" => dispatch_attention_clear(id, params, paths),
        "ui.context.get" => dispatch_ui_context_get(id, params, paths),
        "ui.context.set" => dispatch_ui_context_set(id, params, paths),
        "ui.context.list" => dispatch_ui_context_list(id, params, paths),
        "ui.context.close" => dispatch_ui_context_close(id, params, paths),
        "api.capabilities" => dispatch_api_capabilities(id, params),
        "api.identify" => dispatch_api_identify(id, params),
        _ => error_response(
            id,
            ControlErrorCode::UnknownMethod,
            format!("unsupported method: {method}"),
        ),
    };

    if version == Some(V04_API_VERSION) {
        response.into_v04_method(method)
    } else {
        response
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
        Err(error) => return invalid_params_response(id, "session.start", error),
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
    let spawn_mode = request.spawn_mode;
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
            if record.meta.argv == argv && record.meta.spawn_mode == spawn_mode {
                return Ok(SessionStartResponse {
                    schema_version: M1_PROTOCOL_VERSION,
                    protocol_version: M1_PROTOCOL_VERSION,
                    session: summary_from_meta(&record.meta, record.worker.as_ref(), &record.paths),
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

        match probe_millrace_status(&workspace.canonical_path, &env) {
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
        attention_items: Vec::new(),
        status_summary: None,
        workspace,
        cwd,
        argv,
        spawn_mode,
        monitor_profile,
        env,
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
        session: summary_from_meta(&meta, None, &session_paths),
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

fn probe_millrace_status(
    workspace: &Path,
    env_overrides: &BTreeMap<String, String>,
) -> MillraceStatusProbe {
    let mut command = Command::new("millrace");
    command
        .arg("status")
        .arg("--format")
        .arg("json")
        .arg("--workspace")
        .arg(workspace)
        .envs(env_overrides);
    match command.output() {
        Ok(output) if output.status.success() => {
            match serde_json::from_slice::<Value>(&output.stdout) {
                Ok(value) if value_process_running(&value) => MillraceStatusProbe::Running,
                Ok(_) => MillraceStatusProbe::NotRunning,
                Err(error) => MillraceStatusProbe::Issue(MillraceStatusProbeIssue {
                    outcome: "unusable_json",
                    message: format!("millrace status returned unusable JSON: {error}"),
                    stderr: redacted_stderr(&output.stderr),
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
            stderr: redacted_stderr(&output.stderr),
        }),
        Err(error) => MillraceStatusProbe::Issue(MillraceStatusProbeIssue {
            outcome: "unavailable",
            message: format!("failed to run millrace status: {error}"),
            stderr: None,
        }),
    }
}

fn redacted_stderr(stderr: &[u8]) -> Option<String> {
    let stderr = String::from_utf8_lossy(stderr).trim().to_string();
    if stderr.is_empty() {
        None
    } else {
        Some(redact_diagnostic(&stderr))
    }
}

fn redact_diagnostic(value: &str) -> String {
    value
        .split_whitespace()
        .map(redact_diagnostic_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn redact_diagnostic_token(token: &str) -> String {
    let Some((key, _value)) = token.split_once('=') else {
        return token.to_string();
    };
    if is_secret_env_key(key) {
        format!("{key}=<redacted>")
    } else {
        token.to_string()
    }
}

fn is_secret_env_key(key: &str) -> bool {
    let upper = key
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .to_ascii_uppercase();
    upper.contains("TOKEN")
        || upper.contains("SECRET")
        || upper.contains("PASSWORD")
        || upper.ends_with("_KEY")
        || upper == "KEY"
        || upper.contains("PRIVATE_KEY")
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

fn dispatch_api_capabilities(id: String, params: Value) -> ControlResponse {
    if let Err(error) = serde_json::from_value::<ApiCapabilitiesRequest>(params) {
        return invalid_params_response(id, "api.capabilities", error);
    }

    success_response(id, &api_capabilities_response())
}

fn dispatch_api_identify(id: String, params: Value) -> ControlResponse {
    if let Err(error) = serde_json::from_value::<ApiIdentifyRequest>(params) {
        return invalid_params_response(id, "api.identify", error);
    }

    success_response(id, &api_identify_response())
}

fn dispatch_session_list(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<SessionListRequest>(params) {
        Ok(request) => request,
        Err(error) => return invalid_params_response(id, "session.list", error),
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
        Err(error) => return invalid_params_response(id, "session.inspect", error),
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

fn dispatch_session_screen(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<SessionScreenRequest>(params) {
        Ok(request) => request,
        Err(error) => return invalid_params_response(id, "session.screen", error),
    };

    match build_screen_response(paths, &request) {
        Ok(result) => success_response(id, &result),
        Err(error) => ControlResponse::failure(id, error),
    }
}

fn dispatch_session_logs(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<SessionLogsRequest>(params) {
        Ok(request) => request,
        Err(error) => return invalid_params_response(id, "session.logs", error),
    };

    match build_logs_response(paths, &request, request.follow) {
        Ok(result) => success_response(id, &result),
        Err(error) => ControlResponse::failure(id, error),
    }
}

fn dispatch_session_events(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<SessionEventsRequest>(params) {
        Ok(request) => request,
        Err(error) => return invalid_params_response(id, "session.events", error),
    };

    match build_events_response(paths, &request, request.follow) {
        Ok(result) => success_response(id, &result),
        Err(error) => ControlResponse::failure(id, error),
    }
}

fn dispatch_events_subscribe(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<EventSubscribeRequest>(params) {
        Ok(request) => request,
        Err(error) => return invalid_params_response(id, "events.subscribe", error),
    };

    match build_event_subscribe_response(paths, &request) {
        Ok(result) => success_response(id, &result),
        Err(error) => ControlResponse::failure(id, error),
    }
}

fn dispatch_session_send(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<SessionSendRequest>(params) {
        Ok(request) => request,
        Err(error) => return invalid_params_response(id, "session.send", error),
    };

    match send_to_session(paths, request, None) {
        Ok(result) => success_response(id, &result),
        Err(error) => ControlResponse::failure(id, error),
    }
}

fn dispatch_input_send(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<InputSendRequest>(params) {
        Ok(request) => request,
        Err(error) => return invalid_params_response(id, "input.send", error),
    };

    match input_send(paths, request) {
        Ok(result) => success_response(id, &result),
        Err(error) => ControlResponse::failure(id, error),
    }
}

fn dispatch_session_resize(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<SessionResizeRequest>(params) {
        Ok(request) => request,
        Err(error) => return invalid_params_response(id, "session.resize", error),
    };

    match resize_session(paths, request) {
        Ok(result) => success_response(id, &result),
        Err(error) => ControlResponse::failure(id, error),
    }
}

fn dispatch_session_stop(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<SessionStopRequest>(params) {
        Ok(request) => request,
        Err(error) => return invalid_params_response(id, "session.stop", error),
    };

    match stop_session(paths, request) {
        Ok(result) => success_response(id, &result),
        Err(error) => ControlResponse::failure(id, error),
    }
}

fn dispatch_session_kill(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<SessionKillRequest>(params) {
        Ok(request) => request,
        Err(error) => return invalid_params_response(id, "session.kill", error),
    };

    match kill_session(paths, request) {
        Ok(result) => success_response(id, &result),
        Err(error) => ControlResponse::failure(id, error),
    }
}

fn dispatch_session_delete(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<SessionDeleteRequest>(params) {
        Ok(request) => request,
        Err(error) => return invalid_params_response(id, "session.delete", error),
    };

    match delete_session(paths, request) {
        Ok(result) => success_response(id, &result),
        Err(error) => ControlResponse::failure(id, error),
    }
}

fn dispatch_attention_list(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<AttentionListRequest>(params) {
        Ok(request) => request,
        Err(error) => return invalid_params_response(id, "attention.list", error),
    };

    match list_attention(paths, request) {
        Ok(result) => success_response(id, &result),
        Err(error) => ControlResponse::failure(id, error),
    }
}

fn dispatch_attention_mark(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<AttentionMarkRequest>(params) {
        Ok(request) => request,
        Err(error) => return invalid_params_response(id, "attention.mark", error),
    };

    match mark_attention(paths, request) {
        Ok(result) => success_response(id, &result),
        Err(error) => ControlResponse::failure(id, error),
    }
}

fn dispatch_attention_read(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<AttentionReadRequest>(params) {
        Ok(request) => request,
        Err(error) => return invalid_params_response(id, "attention.read", error),
    };

    match read_attention(paths, request) {
        Ok(result) => success_response(id, &result),
        Err(error) => ControlResponse::failure(id, error),
    }
}

fn dispatch_attention_clear(id: String, params: Value, paths: &StatePaths) -> ControlResponse {
    let request = match serde_json::from_value::<AttentionClearRequest>(params) {
        Ok(request) => request,
        Err(error) => return invalid_params_response(id, "attention.clear", error),
    };

    match clear_attention(paths, request) {
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
    let lines = read_session_log_lines(&inspected, request.tail)?;
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
    let events = read_event_lines(&inspected.paths.events_jsonl, request.tail)
        .map_err(control_core_error)?;
    Ok(SessionEventsResponse {
        schema_version: M1_PROTOCOL_VERSION,
        protocol_version: M1_PROTOCOL_VERSION,
        session_id: inspected.session.session_id,
        events,
        follow: include_follow.then(|| stream_setup(StreamKind::Events, true, false)),
    })
}

fn build_event_subscribe_response(
    paths: &StatePaths,
    request: &EventSubscribeRequest,
) -> Result<EventSubscribeResponse, ControlErrorBody> {
    let inspected = resolve_session(paths, &request.selector)?;
    let cursor = match request.cursor.as_deref() {
        Some(cursor) => cursor.to_string(),
        None => event_cursor(inspected.session.session_id, 0),
    };
    Ok(EventSubscribeResponse {
        schema_version: M1_PROTOCOL_VERSION,
        protocol_version: M1_PROTOCOL_VERSION,
        session_id: inspected.session.session_id,
        stream: stream_setup(StreamKind::Events, true, false),
        cursor,
        replay_limit: normalized_replay_limit(request),
        subscriber_queue_limit: normalized_subscriber_queue_limit(request),
        heartbeat_ms: normalized_heartbeat_ms(request),
        contract: EventSubscriptionContract::default(),
    })
}

fn build_screen_response(
    paths: &StatePaths,
    request: &SessionScreenRequest,
) -> Result<SessionScreenResponse, ControlErrorBody> {
    let inspected = resolve_session(paths, &request.selector)?;
    let frame = read_session_screen_frame(&inspected, request);
    Ok(SessionScreenResponse {
        protocol_version: M1_PROTOCOL_VERSION,
        session_id: inspected.session.session_id,
        frame,
    })
}

fn read_session_screen_frame(
    inspected: &SessionInspectResponse,
    request: &SessionScreenRequest,
) -> ScreenFrame {
    if !inspected.session.capabilities.screen || !inspected.session.spawn_mode.is_pty() {
        return screen_unavailable(
            SnapshotUnavailableReason::UnsupportedSpawnMode,
            json!({
                "session_id": inspected.session.session_id,
                "spawn_mode": inspected.session.spawn_mode,
                "capability": "screen",
            }),
        );
    }

    let snapshot_path = &inspected.paths.terminal_snapshot;
    let snapshot_metadata = match fs::metadata(snapshot_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return screen_unavailable(
                SnapshotUnavailableReason::NoSnapshot,
                json!({
                    "snapshot_state": "missing_file",
                    "terminal_snapshot": snapshot_path,
                }),
            )
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return screen_unavailable(
                SnapshotUnavailableReason::PermissionDenied,
                json!({
                    "snapshot_state": "permission_denied",
                    "terminal_snapshot": snapshot_path,
                    "error": error.to_string(),
                }),
            )
        }
        Err(error) => {
            return screen_unavailable(
                SnapshotUnavailableReason::InternalError,
                json!({
                    "snapshot_state": "metadata_error",
                    "terminal_snapshot": snapshot_path,
                    "error": error.to_string(),
                }),
            )
        }
    };

    if snapshot_metadata.len() == 0 {
        return screen_unavailable(
            SnapshotUnavailableReason::NoSnapshot,
            json!({
                "snapshot_state": "empty_file",
                "terminal_snapshot": snapshot_path,
            }),
        );
    }

    let snapshot = match read_json::<TerminalSnapshot>(snapshot_path) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return screen_unavailable(
                SnapshotUnavailableReason::InternalError,
                json!({
                    "snapshot_state": "invalid_json",
                    "terminal_snapshot": snapshot_path,
                    "error": error.to_string(),
                }),
            )
        }
    };

    let Some(screen_snapshot) = snapshot.structured_screen.clone() else {
        return screen_unavailable(
            SnapshotUnavailableReason::TerminalModelUnavailable,
            json!({
                "snapshot_state": "structured_screen_missing",
                "terminal_snapshot": snapshot_path,
            }),
        );
    };

    if screen_snapshot.rows == 0 || screen_snapshot.cols == 0 || screen_snapshot.cells.is_empty() {
        return screen_unavailable(
            SnapshotUnavailableReason::NoSnapshot,
            json!({
                "snapshot_state": "empty_snapshot",
                "terminal_snapshot": snapshot_path,
                "rows": screen_snapshot.rows,
                "cols": screen_snapshot.cols,
            }),
        );
    }

    if let Some(size) = request.requested_terminal_size {
        if screen_snapshot.rows != size.rows || screen_snapshot.cols != size.cols {
            return screen_unavailable(
                SnapshotUnavailableReason::SizeMismatch,
                json!({
                    "snapshot_state": "size_mismatch",
                    "terminal_snapshot": snapshot_path,
                    "requested_rows": size.rows,
                    "requested_cols": size.cols,
                    "snapshot_rows": screen_snapshot.rows,
                    "snapshot_cols": screen_snapshot.cols,
                }),
            );
        }
    }

    let current_pty_offset = inspected
        .paths
        .pty_log
        .exists()
        .then(|| file_len(&inspected.paths.pty_log));
    if current_pty_offset.is_some_and(|offset| snapshot.pty_log_offset != offset) {
        return screen_unavailable(
            SnapshotUnavailableReason::StaleSnapshot,
            json!({
                "snapshot_state": "stale_pty_log_offset",
                "terminal_snapshot": snapshot_path,
                "pty_log": inspected.paths.pty_log,
                "snapshot_pty_log_offset": snapshot.pty_log_offset,
                "current_pty_log_offset": current_pty_offset,
            }),
        );
    }

    if inspected.paths.raw_replay_ring.exists()
        && snapshot.raw_replay_end_offset != snapshot.pty_log_offset
    {
        return screen_unavailable(
            SnapshotUnavailableReason::StaleSnapshot,
            json!({
                "snapshot_state": "stale_raw_replay_offset",
                "terminal_snapshot": snapshot_path,
                "raw_replay_ring": inspected.paths.raw_replay_ring,
                "raw_replay_end_offset": snapshot.raw_replay_end_offset,
                "snapshot_pty_log_offset": snapshot.pty_log_offset,
            }),
        );
    }

    if let Err(error) = screen_snapshot.validate_for_wire() {
        return ScreenFrame::SnapshotUnavailable {
            reason: error.unavailable_reason(),
            details: error.unavailable_details(),
        };
    }

    ScreenFrame::ScreenSnapshot {
        snapshot: screen_snapshot,
    }
}

fn screen_unavailable(reason: SnapshotUnavailableReason, details: Value) -> ScreenFrame {
    ScreenFrame::SnapshotUnavailable {
        reason,
        details: Some(details),
    }
}

fn ensure_capability(
    inspected: &SessionInspectResponse,
    supported: bool,
    method: &'static str,
    capability: &'static str,
) -> Result<(), ControlErrorBody> {
    if supported {
        return Ok(());
    }
    Err(ControlErrorBody::new(
        ControlErrorCode::UnsupportedSpawnMode,
        format!(
            "{method} is unsupported for spawn_mode={}",
            inspected.session.spawn_mode
        ),
    )
    .with_details(json!({
        "session_id": inspected.session.session_id,
        "spawn_mode": inspected.session.spawn_mode,
        "capability": capability,
    })))
}

fn send_to_session(
    paths: &StatePaths,
    request: SessionSendRequest,
    owner: Option<String>,
) -> Result<SessionSendResponse, ControlErrorBody> {
    let inspected = resolve_running_session(paths, &request.selector)?;
    ensure_capability(
        &inspected,
        inspected.session.capabilities.send,
        "session.send",
        "send",
    )?;
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

fn input_send(
    paths: &StatePaths,
    request: InputSendRequest,
) -> Result<InputSendResponse, ControlErrorBody> {
    let InputSendRequest {
        target,
        text,
        require_focus,
        owner,
    } = request;
    let (selector, routed_via_focus, response_target) =
        input_target_selector(paths, target, require_focus)?;
    let response = send_to_session(paths, SessionSendRequest { selector, text }, owner)?;
    Ok(InputSendResponse {
        schema_version: response.schema_version,
        protocol_version: response.protocol_version,
        session_id: response.session_id,
        target: response_target,
        bytes_sent: response.bytes_sent,
        routed_via_focus,
    })
}

fn input_target_selector(
    paths: &StatePaths,
    target: InputTarget,
    require_focus: bool,
) -> Result<(SessionSelector, bool, InputTarget), ControlErrorBody> {
    match target {
        InputTarget::Session { selector } => {
            let response_target = InputTarget::Session {
                selector: selector.clone(),
            };
            Ok((selector, false, response_target))
        }
        InputTarget::Pane { ui_id, pane_id } => {
            let ui_paths = paths.ui_context_paths(ui_id);
            let context = read_json::<UiContext>(&ui_paths.context_json).map_err(|error| {
                ControlErrorBody::new(
                    ControlErrorCode::UiContextNotFound,
                    format!("UI context {ui_id} not found: {error}"),
                )
            })?;
            let pane = context
                .panes
                .iter()
                .find(|pane| pane.id == pane_id)
                .ok_or_else(|| {
                    ControlErrorBody::new(
                        ControlErrorCode::InvalidRequest,
                        format!("pane {pane_id} not found in UI context {ui_id}"),
                    )
                })?;
            if pane.stale {
                return Err(ControlErrorBody::new(
                    ControlErrorCode::InvalidRequest,
                    format!("pane {pane_id} is stale"),
                ));
            }
            if pane.view.kind != UiPaneViewKind::SessionTerminal {
                return Err(ControlErrorBody::new(
                    ControlErrorCode::InvalidRequest,
                    format!("pane {pane_id} is not a live session terminal"),
                )
                .with_details(json!({
                    "ui_id": ui_id,
                    "pane_id": pane_id,
                    "view_kind": pane.view.kind,
                })));
            }
            if pane.view.view_mode != UiPaneViewMode::Live {
                return Err(ControlErrorBody::new(
                    ControlErrorCode::InvalidRequest,
                    format!("pane {pane_id} is in scrollback mode"),
                )
                .with_details(json!({
                    "ui_id": ui_id,
                    "pane_id": pane_id,
                    "view_mode": pane.view.view_mode,
                })));
            }
            if pane.read_only {
                return Err(ControlErrorBody::new(
                    ControlErrorCode::InputOwnerConflict,
                    format!("pane {pane_id} is read-only"),
                )
                .with_details(json!({
                    "ui_id": ui_id,
                    "pane_id": pane_id,
                    "read_only": true,
                })));
            }
            if pane.overlay_active {
                return Err(ControlErrorBody::new(
                    ControlErrorCode::InputOwnerConflict,
                    format!("pane {pane_id} has an active overlay"),
                )
                .with_details(json!({
                    "ui_id": ui_id,
                    "pane_id": pane_id,
                    "overlay_active": true,
                })));
            }
            if require_focus && !pane.focused {
                return Err(ControlErrorBody::new(
                    ControlErrorCode::InputOwnerConflict,
                    format!("pane {pane_id} is not focused"),
                )
                .with_details(json!({
                    "ui_id": ui_id,
                    "pane_id": pane_id,
                    "require_focus": true
                })));
            }
            let session_id = pane.view.session_id.ok_or_else(|| {
                ControlErrorBody::new(
                    ControlErrorCode::InvalidRequest,
                    format!("pane {pane_id} is not backed by a session"),
                )
            })?;
            Ok((
                SessionSelector::Id { session_id },
                true,
                InputTarget::Pane { ui_id, pane_id },
            ))
        }
    }
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
    ensure_capability(
        &inspected,
        inspected.session.capabilities.resize,
        "session.resize",
        "resize",
    )?;
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
    let stop_reason = normalize_stop_reason(request.reason.as_deref());
    let stop_requested_at = current_timestamp();
    persist_stop_request_metadata(&inspected.paths, &stop_requested_at, &stop_reason)?;

    let mut event = SessionEvent::new(
        inspected.session.session_id,
        SessionEventKind::StopRequested,
    );
    event.timestamp = stop_requested_at.clone();
    event.process_state = Some(inspected.session.process_state.clone());
    event
        .fields
        .insert("grace_seconds".to_string(), grace.as_secs().to_string());
    event
        .fields
        .insert("reason".to_string(), stop_reason.clone());
    event
        .fields
        .insert("stop_requested_at".to_string(), stop_requested_at.clone());
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
                stop_requested_at: Some(stop_requested_at),
                stop_reason: Some(stop_reason),
            });
        }
    }

    if !inspected.session.spawn_mode.is_pty() {
        let meta =
            read_json::<SessionMeta>(&inspected.paths.meta_json).map_err(control_core_error)?;
        append_signal_stop_event(
            &inspected.paths,
            inspected.session.session_id,
            &meta.process_state,
            SIGTERM_STOP_REASON,
        )?;
        stop::request_sigterm(&meta)?;
        let state = wait_for_terminal_state(&inspected.paths.meta_json, grace)?;
        return Ok(SessionStopResponse {
            schema_version: M1_PROTOCOL_VERSION,
            protocol_version: M1_PROTOCOL_VERSION,
            session_id: inspected.session.session_id,
            process_state: state,
            stop_requested: true,
            stop_requested_at: Some(stop_requested_at),
            stop_reason: Some(stop_reason),
        });
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
        append_signal_stop_event(
            &inspected.paths,
            inspected.session.session_id,
            &meta.process_state,
            SIGTERM_FALLBACK_STOP_REASON,
        )?;
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
        stop_requested_at: Some(stop_requested_at),
        stop_reason: Some(stop_reason),
    })
}

fn normalize_stop_reason(reason: Option<&str>) -> String {
    reason
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .unwrap_or(DEFAULT_STOP_REASON)
        .to_string()
}

fn persist_stop_request_metadata(
    paths: &SessionPaths,
    requested_at: &str,
    reason: &str,
) -> Result<(), ControlErrorBody> {
    update_session_meta(paths, |meta, now| {
        meta.stop_requested_at = Some(requested_at.to_string());
        meta.stop_reason = Some(reason.to_string());
        meta.updated_at = now.to_string();
    })?;
    update_worker_meta(paths, |worker, now| {
        worker.stop_requested_at = Some(requested_at.to_string());
        worker.stop_reason = Some(reason.to_string());
        worker.updated_at = now.to_string();
    })
}

fn append_signal_stop_event(
    paths: &SessionPaths,
    session_id: SessionId,
    process_state: &ProcessState,
    reason: &str,
) -> Result<(), ControlErrorBody> {
    let mut event = SessionEvent::new(session_id, SessionEventKind::StopRequested);
    event.process_state = Some(process_state.clone());
    event
        .fields
        .insert("reason".to_string(), reason.to_string());
    event
        .fields
        .insert("signal".to_string(), "SIGTERM".to_string());
    event
        .fields
        .insert("stop_requested_at".to_string(), event.timestamp.clone());
    append_event(&paths.events_jsonl, &event).map_err(control_core_error)
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

    if delete_requires_kill(&inspected.session.process_state) {
        if !request.kill {
            return Err(ControlErrorBody::new(
                ControlErrorCode::UnsafeDeleteRunning,
                "refusing to delete a running or orphaned session without --kill",
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

fn list_attention(
    paths: &StatePaths,
    request: AttentionListRequest,
) -> Result<AttentionListResponse, ControlErrorBody> {
    let inspected = resolve_session(paths, &request.selector)?;
    let meta = read_json::<SessionMeta>(&inspected.paths.meta_json).map_err(control_core_error)?;
    let items = filtered_attention_items(
        &meta.attention_items,
        request.include_read,
        request.include_cleared,
    );
    Ok(AttentionListResponse {
        schema_version: M1_PROTOCOL_VERSION,
        protocol_version: M1_PROTOCOL_VERSION,
        session_id: meta.id,
        attention: AttentionRollup::from_items(&meta.attention_items),
        attention_items: items,
    })
}

fn mark_attention(
    paths: &StatePaths,
    request: AttentionMarkRequest,
) -> Result<AttentionMutationResponse, ControlErrorBody> {
    if request.message.trim().is_empty() {
        return Err(ControlErrorBody::new(
            ControlErrorCode::InvalidRequest,
            "attention.mark message must not be empty",
        ));
    }

    let inspected = resolve_session(paths, &request.selector)?;
    let now = current_timestamp();
    let mut meta =
        read_json::<SessionMeta>(&inspected.paths.meta_json).map_err(control_core_error)?;
    let target_id = request
        .target_id
        .clone()
        .unwrap_or_else(|| default_attention_target_id(&request.target_type, &meta));
    let mut item = AttentionItem::new(
        request.target_type,
        target_id,
        request.kind,
        request.severity,
        request.source,
        request.message.trim().to_string(),
        now.clone(),
    );
    item.dedupe_key = normalize_optional(request.dedupe_key);
    item.status_label = normalize_optional(request.status_label);
    item.status_detail = normalize_optional(request.status_detail);

    let item_id = if let Some(dedupe_key) = item.dedupe_key.as_deref() {
        if let Some(existing) = meta.attention_items.iter_mut().find(|candidate| {
            candidate.is_open() && candidate.dedupe_key.as_deref() == Some(dedupe_key)
        }) {
            existing.target_type = item.target_type;
            existing.target_id = item.target_id;
            existing.kind = item.kind;
            existing.severity = item.severity;
            existing.source = item.source;
            existing.message = item.message;
            existing.read_at = None;
            existing.status_label = item.status_label;
            existing.status_detail = item.status_detail;
            existing.id.clone()
        } else {
            let id = item.id.clone();
            meta.attention_items.push(item);
            id
        }
    } else {
        let id = item.id.clone();
        meta.attention_items.push(item);
        id
    };

    meta.updated_at = now.clone();
    write_json_atomic(&inspected.paths.meta_json, &meta).map_err(control_core_error)?;
    append_attention_event(
        &inspected.paths,
        meta.id,
        SessionEventKind::AttentionMarked,
        &item_id,
        1,
        Some(&request.kind),
    )?;
    attention_mutation_response(meta, 1)
}

fn read_attention(
    paths: &StatePaths,
    request: AttentionReadRequest,
) -> Result<AttentionMutationResponse, ControlErrorBody> {
    let kinds = if request.item_id.is_none() && request.kinds.is_empty() {
        vec![AttentionKind::Unread]
    } else {
        request.kinds
    };
    mutate_attention_items(
        paths,
        &request.selector,
        SessionEventKind::AttentionRead,
        request.item_id.as_deref(),
        &kinds,
        |item, now| {
            if item.read_at.is_none() {
                item.read_at = Some(now.to_string());
                true
            } else {
                false
            }
        },
    )
}

fn clear_attention(
    paths: &StatePaths,
    request: AttentionClearRequest,
) -> Result<AttentionMutationResponse, ControlErrorBody> {
    mutate_attention_items(
        paths,
        &request.selector,
        SessionEventKind::AttentionCleared,
        request.item_id.as_deref(),
        &request.kinds,
        |item, now| {
            if item.cleared_at.is_none() {
                item.cleared_at = Some(now.to_string());
                true
            } else {
                false
            }
        },
    )
}

fn mutate_attention_items<F>(
    paths: &StatePaths,
    selector: &millrace_sessions_core::protocol::SessionSelector,
    event_kind: SessionEventKind,
    item_id: Option<&str>,
    kinds: &[AttentionKind],
    mut apply: F,
) -> Result<AttentionMutationResponse, ControlErrorBody>
where
    F: FnMut(&mut AttentionItem, &str) -> bool,
{
    let inspected = resolve_session(paths, selector)?;
    let now = current_timestamp();
    let mut meta =
        read_json::<SessionMeta>(&inspected.paths.meta_json).map_err(control_core_error)?;
    let mut mutated_count = 0_u32;
    let mut first_item_id = None;
    let mut first_kind = None;

    for item in &mut meta.attention_items {
        if !attention_item_matches(item, item_id, kinds) {
            continue;
        }
        if apply(item, &now) {
            mutated_count = mutated_count.saturating_add(1);
            first_item_id.get_or_insert_with(|| item.id.clone());
            first_kind.get_or_insert(item.kind);
        }
    }

    if mutated_count > 0 {
        meta.updated_at = now;
        write_json_atomic(&inspected.paths.meta_json, &meta).map_err(control_core_error)?;
        append_attention_event(
            &inspected.paths,
            meta.id,
            event_kind,
            first_item_id.as_deref().unwrap_or("multiple"),
            mutated_count,
            first_kind.as_ref(),
        )?;
    }

    attention_mutation_response(meta, mutated_count)
}

fn attention_item_matches(
    item: &AttentionItem,
    item_id: Option<&str>,
    kinds: &[AttentionKind],
) -> bool {
    if !item.is_open() {
        return false;
    }
    if let Some(item_id) = item_id {
        return item.id == item_id;
    }
    kinds.is_empty() || kinds.contains(&item.kind)
}

fn attention_mutation_response(
    meta: SessionMeta,
    mutated_count: u32,
) -> Result<AttentionMutationResponse, ControlErrorBody> {
    Ok(AttentionMutationResponse {
        schema_version: M1_PROTOCOL_VERSION,
        protocol_version: M1_PROTOCOL_VERSION,
        session_id: meta.id,
        mutated_count,
        attention: AttentionRollup::from_items(&meta.attention_items),
        attention_items: filtered_attention_items(&meta.attention_items, true, false),
    })
}

fn filtered_attention_items(
    items: &[AttentionItem],
    include_read: bool,
    include_cleared: bool,
) -> Vec<AttentionItem> {
    items
        .iter()
        .filter(|item| include_cleared || item.is_open())
        .filter(|item| include_read || item.read_at.is_none())
        .cloned()
        .collect()
}

fn default_attention_target_id(target_type: &AttentionTargetType, meta: &SessionMeta) -> String {
    match target_type {
        AttentionTargetType::Workspace => meta
            .workspace
            .as_ref()
            .map(|workspace| workspace.canonical_path.display().to_string())
            .unwrap_or_else(|| meta.id.to_string()),
        AttentionTargetType::Session | AttentionTargetType::Pane => meta.id.to_string(),
    }
}

fn normalize_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn append_attention_event(
    paths: &SessionPaths,
    session_id: SessionId,
    kind: SessionEventKind,
    item_id: &str,
    mutated_count: u32,
    attention_kind: Option<&AttentionKind>,
) -> Result<(), ControlErrorBody> {
    let mut event = SessionEvent::new(session_id, kind);
    event
        .fields
        .insert("attention_item_id".to_string(), item_id.to_string());
    event
        .fields
        .insert("mutated_count".to_string(), mutated_count.to_string());
    if let Some(attention_kind) = attention_kind {
        event
            .fields
            .insert("attention_kind".to_string(), attention_kind.to_string());
    }
    append_event(&paths.events_jsonl, &event).map_err(control_core_error)
}

fn delete_requires_kill(state: &ProcessState) -> bool {
    is_active_process_state(state) || *state == ProcessState::Orphaned
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
        worker.attached_clients = 0;
        worker.input_owner = None;
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

fn read_session_log_lines(
    inspected: &SessionInspectResponse,
    tail: Option<usize>,
) -> Result<Vec<LogLine>, ControlErrorBody> {
    match inspected.session.spawn_mode {
        millrace_sessions_core::state::SpawnMode::Pty => {
            read_log_lines(&inspected.paths.pty_log, tail, LogStream::Pty).map_err(control_io_error)
        }
        millrace_sessions_core::state::SpawnMode::Pipe => {
            let event_tail = tail.map(|tail| tail.saturating_mul(8).max(tail));
            let events = read_event_lines(&inspected.paths.events_jsonl, event_tail)
                .map_err(control_core_error)?;
            let mut lines = log_lines_from_output_events(events);
            apply_tail(&mut lines, tail);
            Ok(lines)
        }
    }
}

fn read_log_lines(
    path: &std::path::Path,
    tail: Option<usize>,
    stream: LogStream,
) -> std::io::Result<Vec<LogLine>> {
    let raw_lines = match tail {
        Some(tail) => match read_tail_lines(path, tail) {
            Ok(lines) => lines,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        },
        None => {
            let raw = match fs::read(path) {
                Ok(raw) => raw,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(error) => return Err(error),
            };
            String::from_utf8_lossy(&raw)
                .lines()
                .map(|line| line.trim_end_matches('\r').to_string())
                .collect::<Vec<_>>()
        }
    };
    let mut lines = raw_lines
        .into_iter()
        .map(|line| LogLine {
            stream,
            timestamp: None,
            line,
        })
        .collect::<Vec<_>>();
    apply_tail(&mut lines, tail);
    Ok(lines)
}

fn apply_tail(lines: &mut Vec<LogLine>, tail: Option<usize>) {
    if let Some(tail) = tail {
        if lines.len() > tail {
            *lines = lines.split_off(lines.len() - tail);
        }
    }
}

fn log_lines_from_output_events(events: Vec<SessionEvent>) -> Vec<LogLine> {
    let mut assembler = LogLineAssembler::default();
    let mut lines = Vec::new();
    for event in events {
        lines.extend(assembler.push_event(event));
    }
    lines.extend(assembler.flush());
    lines
}

#[derive(Default)]
struct LogLineAssembler {
    pty: Vec<u8>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl LogLineAssembler {
    fn push_event(&mut self, event: SessionEvent) -> Vec<LogLine> {
        if event.kind != SessionEventKind::Output {
            return Vec::new();
        }
        let Some(stream) = log_stream_from_event(&event) else {
            return Vec::new();
        };
        let message = event.message.unwrap_or_default();
        if event.fields.get("record_kind").map(String::as_str) != Some("chunk") {
            return vec![LogLine {
                stream,
                timestamp: Some(event.timestamp),
                line: message,
            }];
        }

        let mut lines = Vec::new();
        let buffer = self.buffer_mut(stream);
        buffer.extend_from_slice(message.as_bytes());
        while let Some(index) = buffer.iter().position(|byte| *byte == b'\n') {
            let mut line = buffer.drain(..=index).collect::<Vec<_>>();
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            lines.push(LogLine {
                stream,
                timestamp: Some(event.timestamp.clone()),
                line: String::from_utf8_lossy(&line).to_string(),
            });
        }
        lines
    }

    fn flush(&mut self) -> Vec<LogLine> {
        let mut lines = Vec::new();
        for stream in [LogStream::Pty, LogStream::Stdout, LogStream::Stderr] {
            let buffer = self.buffer_mut(stream);
            if buffer.is_empty() {
                continue;
            }
            let line = std::mem::take(buffer);
            lines.push(LogLine {
                stream,
                timestamp: None,
                line: String::from_utf8_lossy(&line).to_string(),
            });
        }
        lines
    }

    fn buffer_mut(&mut self, stream: LogStream) -> &mut Vec<u8> {
        match stream {
            LogStream::Pty => &mut self.pty,
            LogStream::Stdout => &mut self.stdout,
            LogStream::Stderr => &mut self.stderr,
        }
    }
}

fn log_stream_from_event(event: &SessionEvent) -> Option<LogStream> {
    match event.fields.get("stream").map(String::as_str) {
        Some("stdout") => Some(LogStream::Stdout),
        Some("stderr") => Some(LogStream::Stderr),
        Some("pty") | None => Some(LogStream::Pty),
        Some(_) => None,
    }
}

fn read_event_lines(
    path: &std::path::Path,
    tail: Option<usize>,
) -> MillmuxResult<Vec<SessionEvent>> {
    match tail {
        Some(tail) => {
            let lines = match read_tail_lines(path, tail) {
                Ok(lines) => lines,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(error) => return Err(MillmuxError::Io(error)),
            };
            lines
                .into_iter()
                .filter(|line| !line.trim().is_empty())
                .map(|line| serde_json::from_str::<SessionEvent>(&line).map_err(MillmuxError::Json))
                .collect()
        }
        None => match read_events(path) {
            Ok(events) => Ok(events),
            Err(MillmuxError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(Vec::new())
            }
            Err(error) => Err(error),
        },
    }
}

fn read_tail_lines(path: &std::path::Path, tail: usize) -> std::io::Result<Vec<String>> {
    if tail == 0 {
        return Ok(Vec::new());
    }

    let mut file = fs::File::open(path)?;
    let mut offset = file.metadata()?.len();
    let mut bytes_read = 0_u64;
    let mut chunks = Vec::new();
    let mut newline_count = 0_usize;

    while offset > 0 && newline_count <= tail && bytes_read < MAX_TAIL_READ_BYTES {
        let remaining_budget = MAX_TAIL_READ_BYTES - bytes_read;
        let chunk_size = TAIL_READ_CHUNK_BYTES.min(offset).min(remaining_budget);
        if chunk_size == 0 {
            break;
        }
        offset -= chunk_size;
        file.seek(SeekFrom::Start(offset))?;
        let mut chunk = vec![0; chunk_size as usize];
        file.read_exact(&mut chunk)?;
        newline_count += chunk.iter().filter(|byte| **byte == b'\n').count();
        chunks.push(chunk);
        bytes_read += chunk_size;
    }

    let truncated_start = offset > 0;
    let mut raw = Vec::with_capacity(bytes_read as usize);
    for chunk in chunks.into_iter().rev() {
        raw.extend(chunk);
    }
    if truncated_start {
        if let Some(first_newline) = raw.iter().position(|byte| *byte == b'\n') {
            raw.drain(..=first_newline);
        } else {
            raw.clear();
        }
    }
    let mut lines = String::from_utf8_lossy(&raw)
        .lines()
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect::<Vec<_>>();
    if lines.len() > tail {
        lines = lines.split_off(lines.len() - tail);
    }
    Ok(lines)
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
    let is_v04 = request.is_v04();
    let params = match request.params_as::<SessionLogsRequest>() {
        Ok(params) => params,
        Err(error) => {
            let response = invalid_params_response(request.id, "session.logs", error);
            let response = if is_v04 {
                response.into_v04(ControlMethod::SessionLogs)
            } else {
                response
            };
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
            let response = if is_v04 {
                ControlResponse::failure_v04(request.id, ControlMethod::SessionLogs, error)
            } else {
                ControlResponse::failure(request.id, error)
            };
            writer
                .write_all(response.to_json_line()?.as_bytes())
                .await?;
            writer.flush().await?;
            return Ok(());
        }
    };
    let mut response = match build_logs_response(paths, &params, true) {
        Ok(result) => ControlResponse::success(request.id.clone(), &result)?,
        Err(error) => ControlResponse::failure(request.id.clone(), error),
    };
    if is_v04 {
        response = response.into_v04(ControlMethod::SessionLogs);
    }
    writer
        .write_all(response.to_json_line()?.as_bytes())
        .await?;
    writer.flush().await?;

    match inspected.session.spawn_mode {
        millrace_sessions_core::state::SpawnMode::Pty => {
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
                            stream: LogStream::Pty,
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
        }
        millrace_sessions_core::state::SpawnMode::Pipe => {
            let mut offset = file_len(&inspected.paths.events_jsonl);
            let mut assembler = LogLineAssembler::default();
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
                        for line in assembler.flush() {
                            let frame = LogStreamFrame::Line { line };
                            let _ = writer.write_all(frame.to_json_line()?.as_bytes()).await;
                            let _ = writer.flush().await;
                        }
                        let frame = LogStreamFrame::Closed;
                        let _ = writer.write_all(frame.to_json_line()?.as_bytes()).await;
                        let _ = writer.flush().await;
                        break;
                    }
                    continue;
                }

                for line in String::from_utf8_lossy(&next).lines() {
                    let Ok(event) = serde_json::from_str::<SessionEvent>(line) else {
                        continue;
                    };
                    for line in assembler.push_event(event) {
                        let frame = LogStreamFrame::Line { line };
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
            }
        }
    }

    Ok(())
}

async fn handle_events_follow_stream(
    request: millrace_sessions_core::protocol::ControlRequest,
    mut writer: OwnedWriteHalf,
    paths: &StatePaths,
) -> Result<(), HostServerError> {
    let is_v04 = request.is_v04();
    let params = match request.params_as::<SessionEventsRequest>() {
        Ok(params) => params,
        Err(error) => {
            let response = invalid_params_response(request.id, "session.events", error);
            let response = if is_v04 {
                response.into_v04(ControlMethod::SessionEvents)
            } else {
                response
            };
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
            let response = if is_v04 {
                ControlResponse::failure_v04(request.id, ControlMethod::SessionEvents, error)
            } else {
                ControlResponse::failure(request.id, error)
            };
            writer
                .write_all(response.to_json_line()?.as_bytes())
                .await?;
            writer.flush().await?;
            return Ok(());
        }
    };
    let mut response = match build_events_response(paths, &params, true) {
        Ok(result) => ControlResponse::success(request.id.clone(), &result)?,
        Err(error) => ControlResponse::failure(request.id.clone(), error),
    };
    if is_v04 {
        response = response.into_v04(ControlMethod::SessionEvents);
    }
    writer
        .write_all(response.to_json_line()?.as_bytes())
        .await?;
    writer.flush().await?;

    let mut offset = file_len(&inspected.paths.events_jsonl);
    loop {
        sleep(FOLLOW_POLL).await;
        let records = read_event_records_from_offset(&inspected.paths.events_jsonl, offset)
            .unwrap_or_default();
        if records.is_empty() {
            if session_is_terminal(&inspected.paths.meta_json) {
                let frame = EventStreamFrame::Closed;
                let _ = writer.write_all(frame.to_json_line()?.as_bytes()).await;
                let _ = writer.flush().await;
                break;
            }
            continue;
        }

        for (event, cursor_offset) in records {
            offset = cursor_offset;
            let frame = EventStreamFrame::Event {
                cursor: event_cursor(inspected.session.session_id, cursor_offset),
                event,
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

async fn handle_events_subscribe_stream(
    request: millrace_sessions_core::protocol::ControlRequest,
    mut writer: OwnedWriteHalf,
    paths: &StatePaths,
) -> Result<(), HostServerError> {
    let params = match request.params_as::<EventSubscribeRequest>() {
        Ok(params) => params,
        Err(error) => {
            let response = invalid_params_response(request.id, "events.subscribe", error)
                .into_v04(ControlMethod::EventsSubscribe);
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
                    ControlResponse::failure_v04(request.id, ControlMethod::EventsSubscribe, error)
                        .to_json_line()?
                        .as_bytes(),
                )
                .await?;
            writer.flush().await?;
            return Ok(());
        }
    };

    let response = match build_event_subscribe_response(paths, &params) {
        Ok(result) => ControlResponse::success_v04(
            request.id.clone(),
            ControlMethod::EventsSubscribe,
            &result,
        )?,
        Err(error) => {
            ControlResponse::failure_v04(request.id.clone(), ControlMethod::EventsSubscribe, error)
        }
    };
    writer
        .write_all(response.to_json_line()?.as_bytes())
        .await?;
    writer.flush().await?;

    let replay_limit = normalized_replay_limit(&params);
    let queue_limit = normalized_subscriber_queue_limit(&params);
    let heartbeat_ms = normalized_heartbeat_ms(&params);
    let current_len = file_len(&inspected.paths.events_jsonl);
    let start_offset = match params.cursor.as_deref() {
        Some(cursor) => match parse_event_cursor(cursor, inspected.session.session_id) {
            Ok(offset) if offset <= current_len => offset,
            _ => {
                write_event_stream_frame(
                    &mut writer,
                    &EventStreamFrame::Error {
                        error: ControlErrorBody::new(
                            ControlErrorCode::EventCursorExpired,
                            "event cursor is invalid or no longer readable",
                        )
                        .with_retryable(false)
                        .with_details(json!({
                            "cursor": cursor,
                            "session_id": inspected.session.session_id,
                        })),
                    },
                )
                .await?;
                write_event_stream_frame(&mut writer, &EventStreamFrame::Closed).await?;
                return Ok(());
            }
        },
        None => 0,
    };

    let all_replay = read_event_records_from_offset(&inspected.paths.events_jsonl, start_offset)
        .unwrap_or_default();
    let dropped_replay = all_replay.len().saturating_sub(replay_limit);
    let replay = all_replay
        .into_iter()
        .skip(dropped_replay)
        .collect::<Vec<_>>();
    let mut offset = replay
        .last()
        .map(|(_, cursor_offset)| *cursor_offset)
        .unwrap_or(start_offset);

    write_event_stream_frame(
        &mut writer,
        &EventStreamFrame::Ack {
            cursor: event_cursor(inspected.session.session_id, start_offset),
            replayed: replay.len(),
            subscriber_queue_limit: queue_limit,
            heartbeat_ms,
        },
    )
    .await?;
    if dropped_replay > 0 {
        write_event_stream_frame(
            &mut writer,
            &EventStreamFrame::StreamLagged {
                dropped_events: dropped_replay as u64,
                cursor: event_cursor(inspected.session.session_id, offset),
                reason: EventStreamLagReason::SubscriberBackpressure,
                recover: "resubscribe_with_newer_cursor_or_lower_replay_limit".to_string(),
            },
        )
        .await?;
    }
    for (event, cursor_offset) in replay {
        offset = cursor_offset;
        write_event_stream_frame(
            &mut writer,
            &EventStreamFrame::Event {
                cursor: event_cursor(inspected.session.session_id, cursor_offset),
                event,
            },
        )
        .await?;
    }

    let heartbeat = Duration::from_millis(heartbeat_ms);
    let mut last_heartbeat = Instant::now();
    loop {
        sleep(FOLLOW_POLL).await;
        let records = read_event_records_from_offset(&inspected.paths.events_jsonl, offset)
            .unwrap_or_default();
        if records.is_empty() {
            if session_is_terminal(&inspected.paths.meta_json) {
                write_event_stream_frame(&mut writer, &EventStreamFrame::Closed).await?;
                break;
            }
            if last_heartbeat.elapsed() >= heartbeat {
                write_event_stream_frame(
                    &mut writer,
                    &EventStreamFrame::Heartbeat {
                        cursor: event_cursor(inspected.session.session_id, offset),
                    },
                )
                .await?;
                last_heartbeat = Instant::now();
            }
            continue;
        }

        let dropped = records.len().saturating_sub(queue_limit);
        let records = records.into_iter().skip(dropped).collect::<Vec<_>>();
        if dropped > 0 {
            write_event_stream_frame(
                &mut writer,
                &EventStreamFrame::StreamLagged {
                    dropped_events: dropped as u64,
                    cursor: event_cursor(inspected.session.session_id, offset),
                    reason: EventStreamLagReason::SubscriberBackpressure,
                    recover: "resubscribe_with_latest_cursor".to_string(),
                },
            )
            .await?;
        }
        for (event, cursor_offset) in records {
            offset = cursor_offset;
            write_event_stream_frame(
                &mut writer,
                &EventStreamFrame::Event {
                    cursor: event_cursor(inspected.session.session_id, cursor_offset),
                    event,
                },
            )
            .await?;
        }
        last_heartbeat = Instant::now();
    }

    Ok(())
}

async fn handle_attach_stream(
    request: millrace_sessions_core::protocol::ControlRequest,
    lines: Lines<TokioBufReader<OwnedReadHalf>>,
    mut writer: OwnedWriteHalf,
    paths: &StatePaths,
) -> Result<(), HostServerError> {
    let is_v04 = request.is_v04();
    let params = match request.params_as::<SessionAttachRequest>() {
        Ok(params) => params,
        Err(error) => {
            let response = invalid_params_response(request.id, "session.attach", error);
            let response = if is_v04 {
                response.into_v04(ControlMethod::SessionAttach)
            } else {
                response
            };
            writer
                .write_all(response.to_json_line()?.as_bytes())
                .await?;
            writer.flush().await?;
            return Ok(());
        }
    };

    let worker_stream = match open_worker_attach(paths, &params).await {
        Ok(worker_stream) => worker_stream,
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

    let WorkerAttachStream {
        opened,
        lines: mut worker_lines,
        writer: mut worker_writer,
    } = worker_stream;
    let cleanup = AttachCleanupGuard::new(opened);

    let response = ControlResponse::success(request.id, &cleanup.opened().response)?;
    let response_line = response.to_json_line()?;
    if writer.write_all(response_line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
        let _ = worker_writer.shutdown().await;
        if drain_worker_attach_close(&mut worker_lines).await? {
            cleanup.close();
        }
        return Ok(());
    }

    if proxy_attach_stream(lines, writer, worker_lines, worker_writer).await? {
        cleanup.close();
    }
    Ok(())
}

struct OpenAttach {
    paths: SessionPaths,
    stream_id: String,
    response: SessionAttachResponse,
}

struct WorkerAttachStream {
    opened: OpenAttach,
    lines: Lines<TokioBufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
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
            record_attach_closed(&opened);
        }
    }
}

impl Drop for AttachCleanupGuard {
    fn drop(&mut self) {}
}

async fn open_worker_attach(
    paths: &StatePaths,
    request: &SessionAttachRequest,
) -> Result<WorkerAttachStream, ControlErrorBody> {
    let inspected = resolve_running_session(paths, &request.selector)?;
    ensure_capability(
        &inspected,
        inspected.session.capabilities.attach,
        "session.attach",
        "attach",
    )?;
    let stream_id = next_stream_id();
    let worker_params = WorkerAttachRequest {
        stream_id: stream_id.clone(),
        read_only: request.read_only,
        replay: request.replay,
        requested_terminal_size: request.requested_terminal_size,
        client_protocol_version: request.client_protocol_version,
        accepted_frame_types: request.accepted_frame_types.clone(),
        stream_encoding: request.stream_encoding,
        initial_replay: request.initial_replay,
    };
    let worker_request_id = next_stream_id();
    let worker_request = WorkerControlRequest::with_params(
        worker_request_id.clone(),
        WorkerControlMethod::ObserveAttach,
        &worker_params,
    )
    .map_err(control_json_error)?;
    let mut worker_stream = connect_worker_stream(&inspected.paths.worker_sock).await?;
    worker_stream
        .write_all(
            worker_request
                .to_json_line()
                .map_err(control_json_error)?
                .as_bytes(),
        )
        .await
        .map_err(control_io_error)?;
    worker_stream.flush().await.map_err(control_io_error)?;

    let (worker_reader, worker_writer) = worker_stream.into_split();
    let mut worker_lines = TokioBufReader::new(worker_reader).lines();
    let response_line = worker_lines
        .next_line()
        .await
        .map_err(control_io_error)?
        .ok_or_else(|| {
            ControlErrorBody::new(
                ControlErrorCode::WorkerUnavailable,
                "worker closed attach stream before responding",
            )
        })?;
    let worker_response =
        WorkerControlResponse::from_json_line(&response_line).map_err(control_json_error)?;
    if worker_response.id != worker_request_id {
        return Err(ControlErrorBody::new(
            ControlErrorCode::WorkerUnavailable,
            format!(
                "worker response id {} did not match request id {worker_request_id}",
                worker_response.id
            ),
        ));
    }
    if !worker_response.ok {
        return Err(worker_response.error.unwrap_or_else(|| {
            ControlErrorBody::new(
                ControlErrorCode::WorkerUnavailable,
                "worker returned an error without an error body",
            )
        }));
    }
    let worker: WorkerAttachResponse = worker_response.result_as().map_err(control_json_error)?;

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
    event
        .fields
        .insert("replay".to_string(), request.replay.to_string());
    if let Some(size) = request.requested_terminal_size {
        event
            .fields
            .insert("requested_rows".to_string(), size.rows.to_string());
        event
            .fields
            .insert("requested_cols".to_string(), size.cols.to_string());
    }
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
        negotiated_attach_protocol_version: worker.negotiated_attach_protocol_version,
        negotiated_stream_encoding: worker.negotiated_stream_encoding,
        negotiated_initial_replay: worker.negotiated_initial_replay,
        accepted_frame_types: worker.accepted_frame_types,
    };

    Ok(WorkerAttachStream {
        opened: OpenAttach {
            paths: inspected.paths,
            stream_id,
            response,
        },
        lines: worker_lines,
        writer: worker_writer,
    })
}

async fn proxy_attach_stream(
    mut client_lines: Lines<TokioBufReader<OwnedReadHalf>>,
    mut client_writer: OwnedWriteHalf,
    mut worker_lines: Lines<TokioBufReader<OwnedReadHalf>>,
    mut worker_writer: OwnedWriteHalf,
) -> Result<bool, HostServerError> {
    loop {
        tokio::select! {
            input = client_lines.next_line() => {
                match input? {
                    Some(line) => {
                        if worker_writer.write_all(line.as_bytes()).await.is_err()
                            || worker_writer.write_all(b"\n").await.is_err()
                            || worker_writer.flush().await.is_err()
                        {
                            return drain_worker_attach_close(&mut worker_lines).await;
                        }
                    }
                    None => {
                        let _ = worker_writer.shutdown().await;
                        return drain_worker_attach_close(&mut worker_lines).await;
                    }
                }
            }
            output = worker_lines.next_line() => {
                match output? {
                    Some(line) => {
                        let is_closed = attach_stream_line_is_closed(&line);
                        if client_writer.write_all(line.as_bytes()).await.is_err()
                            || client_writer.write_all(b"\n").await.is_err()
                            || client_writer.flush().await.is_err()
                        {
                            if is_closed {
                                return Ok(true);
                            }
                            let _ = worker_writer.shutdown().await;
                            return drain_worker_attach_close(&mut worker_lines).await;
                        }
                        if is_closed {
                            return Ok(true);
                        }
                    }
                    None => return Ok(true),
                }
            }
        }
    }
}

async fn drain_worker_attach_close(
    worker_lines: &mut Lines<TokioBufReader<OwnedReadHalf>>,
) -> Result<bool, HostServerError> {
    loop {
        tokio::select! {
            output = worker_lines.next_line() => {
                match output? {
                    Some(line) if attach_stream_line_is_closed(&line) => return Ok(true),
                    Some(_) => continue,
                    None => return Ok(true),
                }
            }
            _ = sleep(DEFAULT_STOP_GRACE) => return Ok(false),
        }
    }
}

fn attach_stream_line_is_closed(line: &str) -> bool {
    matches!(
        AttachStreamFrame::from_json_line(line),
        Ok(AttachStreamFrame::Closed)
    )
}

async fn connect_worker_stream(path: &Path) -> Result<UnixStream, ControlErrorBody> {
    let started = Instant::now();
    let mut last_error = None;
    while started.elapsed() < WORKER_CONNECT_TIMEOUT {
        match UnixStream::connect(path).await {
            Ok(stream) => return Ok(stream),
            Err(error) => {
                last_error = Some(error);
                sleep(WORKER_CONNECT_POLL).await;
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

fn record_attach_closed(opened: &OpenAttach) {
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

fn read_event_records_from_offset(
    path: &std::path::Path,
    offset: u64,
) -> std::io::Result<Vec<(SessionEvent, u64)>> {
    let (bytes, _) = read_from_offset(path, offset)?;
    let mut cursor = offset;
    let mut records = Vec::new();
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        cursor = cursor.saturating_add(line.len() as u64);
        let line = String::from_utf8_lossy(line);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<SessionEvent>(trimmed) {
            records.push((event, cursor));
        }
    }
    Ok(records)
}

fn event_cursor(session_id: SessionId, offset: u64) -> String {
    format!("events:{session_id}:{offset}")
}

fn parse_event_cursor(
    cursor: &str,
    expected_session_id: SessionId,
) -> Result<u64, ControlErrorBody> {
    let mut parts = cursor.split(':');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("events"), Some(session_id), Some(offset), None)
            if session_id == expected_session_id.to_string() =>
        {
            offset
                .parse::<u64>()
                .map_err(|_| event_cursor_expired(cursor))
        }
        _ => Err(event_cursor_expired(cursor)),
    }
}

fn event_cursor_expired(cursor: &str) -> ControlErrorBody {
    ControlErrorBody::new(
        ControlErrorCode::EventCursorExpired,
        "event cursor is invalid or expired",
    )
    .with_details(json!({ "cursor": cursor }))
}

fn normalized_replay_limit(request: &EventSubscribeRequest) -> usize {
    request.replay_limit.unwrap_or(128).min(1_000)
}

fn normalized_subscriber_queue_limit(request: &EventSubscribeRequest) -> usize {
    request
        .subscriber_queue_limit
        .unwrap_or(256)
        .clamp(1, 10_000)
}

fn normalized_heartbeat_ms(request: &EventSubscribeRequest) -> u64 {
    request.heartbeat_ms.unwrap_or(5_000).clamp(50, 60_000)
}

async fn write_event_stream_frame(
    writer: &mut OwnedWriteHalf,
    frame: &EventStreamFrame,
) -> Result<(), HostServerError> {
    writer.write_all(frame.to_json_line()?.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
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

fn invalid_params_response(
    id: String,
    method: &'static str,
    error: serde_json::Error,
) -> ControlResponse {
    let message = format!("invalid {method} params: {error}");
    let code = if message.contains("invalid role") {
        ControlErrorCode::InvalidRole
    } else {
        ControlErrorCode::InvalidRequest
    };
    error_response(id, code, message)
}

fn api_identify_response() -> ApiIdentifyResponse {
    ApiIdentifyResponse {
        schema_version: M1_PROTOCOL_VERSION,
        api_version: V04_API_VERSION.to_string(),
        schema: V04_API_SCHEMA.to_string(),
        socket_envelope: ApiEnvelopeDescription {
            request: r#"{"id":"req_...","version":"0.4","method":"domain.action","params":{}}"#
                .to_string(),
            success_response:
                r#"{"id":"req_...","ok":true,"schema":"millmux.api.v0.4","method":"domain.action","result":{}}"#
                    .to_string(),
            error_response:
                r#"{"id":"req_...","ok":false,"schema":"millmux.api.v0.4","method":"domain.action","error":{"code":"...","message":"...","retryable":false,"details":{}}}"#
                    .to_string(),
        },
        stable_command_groups: stable_command_groups(),
        experimental_command_groups: experimental_command_groups(),
        network_exposure: "local_unix_socket_only".to_string(),
    }
}

fn api_capabilities_response() -> ApiCapabilitiesResponse {
    let stable = [
        ("workspace", vec!["session.list"]),
        (
            "session",
            vec![
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
            ],
        ),
        ("agent", vec!["session.start", "session.list"]),
        ("shell", vec!["session.start", "session.list"]),
        ("daemon", vec!["session.start", "session.list"]),
        ("pane", vec!["ui.context.get", "ui.context.list"]),
        ("input", vec!["input.send"]),
        ("screen", vec!["session.screen"]),
        ("scrollback", vec!["session.screen"]),
        ("logs", vec!["session.logs"]),
        ("events", vec!["session.events", "events.subscribe"]),
        (
            "attention",
            vec![
                "attention.list",
                "attention.mark",
                "attention.read",
                "attention.clear",
            ],
        ),
        ("api", vec!["api.capabilities", "api.identify"]),
        ("identify", vec!["api.identify"]),
        ("context export", vec!["ui.context.get", "ui.context.list"]),
        ("doctor", vec!["host.doctor"]),
        ("cockpit", vec!["ui.context.set", "session.attach"]),
        ("console", vec!["ui.context.set", "session.logs"]),
    ]
    .into_iter()
    .map(|(name, methods)| ApiCapability {
        name: name.to_string(),
        methods: methods.into_iter().map(str::to_string).collect(),
        available: true,
        stability: ApiStability::Stable,
        unavailable_reason: None,
    })
    .collect();

    let experimental = [
        (
            "input leases",
            "owner leases require TTL, token, identity, stale recovery, and events",
        ),
        ("browser adapters", "batch-6 follow-on surface"),
        ("remote sockets", "network exposure is out of v0.4 scope"),
    ]
    .into_iter()
    .map(|(name, reason)| ApiCapability {
        name: name.to_string(),
        methods: Vec::new(),
        available: false,
        stability: ApiStability::Experimental,
        unavailable_reason: Some(reason.to_string()),
    })
    .collect();

    ApiCapabilitiesResponse {
        schema_version: M1_PROTOCOL_VERSION,
        api_version: V04_API_VERSION.to_string(),
        stable,
        experimental,
    }
}

fn stable_command_groups() -> Vec<String> {
    [
        "workspace",
        "session",
        "agent",
        "shell",
        "daemon",
        "pane",
        "input",
        "screen",
        "scrollback",
        "logs",
        "events",
        "attention",
        "api",
        "identify",
        "context export",
        "doctor",
        "cockpit",
        "console",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn experimental_command_groups() -> Vec<String> {
    ["input leases", "browser adapters", "remote sockets"]
        .into_iter()
        .map(str::to_string)
        .collect()
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

fn summary_from_meta(
    meta: &SessionMeta,
    worker: Option<&WorkerMeta>,
    paths: &SessionPaths,
) -> SessionSummary {
    let active = is_active_process_state(&meta.process_state);
    let terminal_capable = meta.spawn_mode.is_pty();
    let attached_clients = if terminal_capable {
        worker
            .filter(|_| active)
            .map_or(0, |worker| worker.attached_clients)
    } else {
        0
    };
    let input_owner = if terminal_capable {
        worker
            .filter(|_| active)
            .and_then(|worker| worker.input_owner.clone())
    } else {
        None
    };

    SessionSummary {
        session_id: meta.id,
        name: meta.name.clone(),
        role: meta.role.clone(),
        spawn_mode: meta.spawn_mode,
        process_state: meta.process_state.clone(),
        attention_state: meta.attention_state.clone(),
        attention: AttentionRollup::from_items(&meta.attention_items),
        status_summary: status_summary_from_meta(meta),
        failure_message: meta.failure_message.clone(),
        workspace: meta.workspace.clone(),
        cwd: meta.cwd.clone(),
        argv: meta.argv.clone(),
        monitor_profile: monitor_profile_from_meta(meta),
        created_at: meta.created_at.clone(),
        updated_at: meta.updated_at.clone(),
        stop_requested_at: meta.stop_requested_at.clone(),
        stop_reason: meta.stop_reason.clone(),
        attached_clients,
        input_owner,
        capabilities: SessionCapabilities::for_spawn_mode(meta.spawn_mode),
        artifacts: SessionArtifacts::for_paths(meta.spawn_mode, paths),
        liveness: SessionLiveness::default(),
    }
}

fn status_summary_from_meta(meta: &SessionMeta) -> StatusSummary {
    meta.status_summary.clone().unwrap_or_else(|| {
        StatusSummary::millmux_session(
            process_state_label(&meta.process_state),
            Some(format!(
                "liveness source=millmux_session session={}",
                meta.id
            )),
        )
    })
}

fn process_state_label(state: &ProcessState) -> &'static str {
    match state {
        ProcessState::Starting => "starting",
        ProcessState::Running => "running",
        ProcessState::Orphaned => "orphaned",
        ProcessState::Exited => "exited",
        ProcessState::Crashed => "crashed",
        ProcessState::Killed => "killed",
        ProcessState::FailedStart => "failed_start",
        ProcessState::Failed => "failed",
        ProcessState::Lost => "lost",
        ProcessState::Stale => "stale",
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
