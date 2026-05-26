use std::{
    collections::BTreeSet,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    sync::{mpsc, Arc, Mutex},
    thread,
};

use millrace_sessions_core::{
    error::{MillmuxError, MillmuxResult},
    events::current_timestamp,
    protocol::{
        AttachReplayMode, AttachStreamFrame, ControlErrorBody, ControlErrorCode,
        TerminalDimensions, WorkerAckResponse, WorkerAttachRequest, WorkerAttachResponse,
        WorkerAttachStateResponse, WorkerControlMethod, WorkerControlRequest,
        WorkerControlResponse, WorkerReleaseAttachRequest, WorkerResizeRequest,
        WorkerResizeResponse, WorkerSendRequest, WorkerSendResponse,
    },
    scrollback::{restore_terminal_replay, ScrollbackBuffer, TerminalStateBuffer},
    state::{SessionPaths, WorkerMeta},
    storage::{read_json, write_json_atomic},
};
use nix::{
    errno::Errno,
    sys::signal::{kill, Signal},
    unistd::Pid,
};
use portable_pty::{MasterPty, PtySize};

#[derive(Clone)]
pub struct WorkerControlHandle {
    state: Arc<Mutex<ControlState>>,
}

impl WorkerControlHandle {
    pub fn broadcast_output(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let bytes = bytes.to_vec();
        let mut state = self.state.lock().expect("control state poisoned");
        state
            .observers
            .retain(|observer| observer.send(bytes.clone()).is_ok());
    }
}

#[derive(Clone)]
pub struct WorkerControlConfig {
    pub paths: SessionPaths,
    pub writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pub master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    pub terminal_state: Arc<Mutex<TerminalStateBuffer>>,
    pub child_pid: Option<u32>,
    pub child_pgid: Option<u32>,
}

pub fn start_control_server(config: WorkerControlConfig) -> MillmuxResult<WorkerControlHandle> {
    if let Some(parent) = config.paths.worker_sock.parent() {
        fs::create_dir_all(parent)?;
    }
    if config.paths.worker_sock.exists() {
        fs::remove_file(&config.paths.worker_sock)?;
    }
    let listener = UnixListener::bind(&config.paths.worker_sock)?;
    harden_socket_permissions(&config.paths.worker_sock)?;

    let state = Arc::new(Mutex::new(ControlState::default()));
    let handle = WorkerControlHandle {
        state: Arc::clone(&state),
    };
    let runtime = Arc::new(ControlRuntime {
        paths: config.paths,
        writer: config.writer,
        master: config.master,
        terminal_state: config.terminal_state,
        child_pid: config.child_pid,
        child_pgid: config.child_pgid,
        state,
    });

    thread::spawn(move || {
        for accepted in listener.incoming() {
            let Ok(stream) = accepted else {
                continue;
            };
            let runtime = Arc::clone(&runtime);
            thread::spawn(move || {
                let _ = handle_connection(stream, runtime);
            });
        }
    });

    Ok(handle)
}

#[cfg(unix)]
fn harden_socket_permissions(path: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn harden_socket_permissions(_path: &Path) -> Result<(), std::io::Error> {
    Ok(())
}

struct ControlRuntime {
    paths: SessionPaths,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    terminal_state: Arc<Mutex<TerminalStateBuffer>>,
    child_pid: Option<u32>,
    child_pgid: Option<u32>,
    state: Arc<Mutex<ControlState>>,
}

#[derive(Default)]
struct ControlState {
    input_owner: Option<String>,
    attaches: BTreeSet<String>,
    observers: Vec<mpsc::Sender<Vec<u8>>>,
}

impl ControlState {
    fn acquire_attach(
        &mut self,
        stream_id: &str,
        read_only: bool,
    ) -> Result<WorkerAttachResponse, ControlErrorBody> {
        let input_owner = self.acquire_input(stream_id, read_only)?;
        self.attaches.insert(stream_id.to_string());
        Ok(WorkerAttachResponse {
            stream_id: stream_id.to_string(),
            read_only,
            input_owner,
        })
    }

    fn release_attach(&mut self, stream_id: &str) {
        if self.input_owner.as_deref() == Some(stream_id) {
            self.input_owner = None;
        }
        self.attaches.remove(stream_id);
    }

    fn acquire_input(
        &mut self,
        stream_id: &str,
        read_only: bool,
    ) -> Result<bool, ControlErrorBody> {
        if read_only {
            return Ok(false);
        }

        if let Some(owner) = &self.input_owner {
            if owner != stream_id {
                return Err(ControlErrorBody::new(
                    ControlErrorCode::InputOwnerConflict,
                    "another attach stream owns PTY input",
                ));
            }
        }

        self.input_owner = Some(stream_id.to_string());
        Ok(true)
    }

    fn attach_state(&self) -> WorkerAttachStateResponse {
        WorkerAttachStateResponse {
            attached_clients: self.attaches.len().try_into().unwrap_or(u32::MAX),
            input_owner: self.input_owner.clone(),
        }
    }

    fn send_is_allowed(&self, owner: Option<&str>) -> Result<(), ControlErrorBody> {
        match (&self.input_owner, owner) {
            (None, None) => Ok(()),
            (Some(current), Some(owner)) if current == owner => Ok(()),
            _ => Err(ControlErrorBody::new(
                ControlErrorCode::InputOwnerConflict,
                "PTY input is owned by another attach stream",
            )),
        }
    }
}

fn handle_connection(stream: UnixStream, runtime: Arc<ControlRuntime>) -> MillmuxResult<()> {
    let mut line = String::new();
    let mut reader = BufReader::new(stream.try_clone()?);
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }

    let request = match WorkerControlRequest::from_json_line(&line) {
        Ok(request) => request,
        Err(error) => {
            let response = WorkerControlResponse::failure(
                "invalid_request",
                ControlErrorBody::new(
                    ControlErrorCode::InvalidRequest,
                    format!("invalid worker control request: {error}"),
                ),
            );
            write_worker_response(stream, response)?;
            return Ok(());
        }
    };

    if request.method == WorkerControlMethod::ObserveAttach {
        return handle_observe_attach(stream, runtime, request);
    }

    let id = request.id.clone();
    let response = match dispatch_request(&runtime, request) {
        Ok(response) => response,
        Err(error) => WorkerControlResponse::failure(id, error),
    };
    write_worker_response(stream, response)
}

fn dispatch_request(
    runtime: &ControlRuntime,
    request: WorkerControlRequest,
) -> Result<WorkerControlResponse, ControlErrorBody> {
    match request.method {
        WorkerControlMethod::Send => {
            let params = request
                .params_as::<WorkerSendRequest>()
                .map_err(invalid_params)?;
            let result = send_text(runtime, params)?;
            WorkerControlResponse::success(request.id, &result).map_err(internal_error)
        }
        WorkerControlMethod::Resize => {
            let params = request
                .params_as::<WorkerResizeRequest>()
                .map_err(invalid_params)?;
            let result = resize_pty(runtime, params)?;
            WorkerControlResponse::success(request.id, &result).map_err(internal_error)
        }
        WorkerControlMethod::AcquireAttach => {
            let params = request
                .params_as::<WorkerAttachRequest>()
                .map_err(invalid_params)?;
            let result = acquire_attach(runtime, &params)?;
            WorkerControlResponse::success(request.id, &result).map_err(internal_error)
        }
        WorkerControlMethod::ReleaseAttach => {
            let params = request
                .params_as::<WorkerReleaseAttachRequest>()
                .map_err(invalid_params)?;
            release_attach(runtime, &params.stream_id)?;
            WorkerControlResponse::success(request.id, &WorkerAckResponse { accepted: true })
                .map_err(internal_error)
        }
        WorkerControlMethod::AttachState => {
            let result = runtime
                .state
                .lock()
                .expect("control state poisoned")
                .attach_state();
            WorkerControlResponse::success(request.id, &result).map_err(internal_error)
        }
        WorkerControlMethod::PrepareStopInterrupt => {
            let result = prepare_stop_interrupt(runtime)?;
            WorkerControlResponse::success(request.id, &result).map_err(internal_error)
        }
        WorkerControlMethod::ForwardKill => {
            let result = forward_kill(runtime)?;
            WorkerControlResponse::success(request.id, &result).map_err(internal_error)
        }
        WorkerControlMethod::ObserveAttach => unreachable!("handled before one-shot dispatch"),
    }
}

fn acquire_attach(
    runtime: &ControlRuntime,
    params: &WorkerAttachRequest,
) -> Result<WorkerAttachResponse, ControlErrorBody> {
    let (result, state) = {
        let mut state = runtime.state.lock().expect("control state poisoned");
        let result = state.acquire_attach(&params.stream_id, params.read_only)?;
        let attach_state = state.attach_state();
        (result, attach_state)
    };
    if let Err(error) = persist_attach_state(runtime, &state) {
        let rolled_back = {
            let mut state = runtime.state.lock().expect("control state poisoned");
            state.release_attach(&params.stream_id);
            state.attach_state()
        };
        let _ = persist_attach_state(runtime, &rolled_back);
        return Err(error);
    }
    Ok(result)
}

fn release_attach(
    runtime: &ControlRuntime,
    stream_id: &str,
) -> Result<WorkerAttachStateResponse, ControlErrorBody> {
    let state = {
        let mut state = runtime.state.lock().expect("control state poisoned");
        state.release_attach(stream_id);
        state.attach_state()
    };
    persist_attach_state(runtime, &state)?;
    Ok(state)
}

fn persist_attach_state(
    runtime: &ControlRuntime,
    state: &WorkerAttachStateResponse,
) -> Result<(), ControlErrorBody> {
    let mut worker: WorkerMeta = read_json(&runtime.paths.worker_json).map_err(core_error)?;
    worker.attached_clients = state.attached_clients;
    worker.input_owner = state.input_owner.clone();
    worker.updated_at = current_timestamp();
    write_json_atomic(&runtime.paths.worker_json, &worker).map_err(core_error)
}

fn send_text(
    runtime: &ControlRuntime,
    params: WorkerSendRequest,
) -> Result<WorkerSendResponse, ControlErrorBody> {
    let state = runtime.state.lock().expect("control state poisoned");
    state.send_is_allowed(params.owner.as_deref())?;

    let mut writer = runtime.writer.lock().expect("pty writer poisoned");
    writer.write_all(params.text.as_bytes()).map_err(io_error)?;
    writer.flush().map_err(io_error)?;
    Ok(WorkerSendResponse {
        bytes_sent: params.text.len(),
    })
}

fn resize_pty(
    runtime: &ControlRuntime,
    params: WorkerResizeRequest,
) -> Result<WorkerResizeResponse, ControlErrorBody> {
    if params.rows == 0 || params.cols == 0 {
        return Err(ControlErrorBody::new(
            ControlErrorCode::InvalidRequest,
            "resize rows and cols must be greater than zero",
        ));
    }

    runtime
        .master
        .lock()
        .expect("pty master poisoned")
        .resize(PtySize {
            rows: params.rows,
            cols: params.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| {
            ControlErrorBody::new(
                ControlErrorCode::WorkerUnavailable,
                format!("failed to resize pty: {error}"),
            )
        })?;

    let mut terminal_state = runtime.terminal_state.lock().map_err(|_| {
        ControlErrorBody::new(
            ControlErrorCode::InternalError,
            "terminal state lock poisoned",
        )
    })?;
    terminal_state.resize(params.rows, params.cols);
    terminal_state
        .persist(
            &runtime.paths.terminal_snapshot,
            &runtime.paths.raw_replay_ring,
        )
        .map_err(core_error)?;

    Ok(WorkerResizeResponse {
        rows: params.rows,
        cols: params.cols,
    })
}

fn prepare_stop_interrupt(runtime: &ControlRuntime) -> Result<WorkerAckResponse, ControlErrorBody> {
    let mut writer = runtime.writer.lock().expect("pty writer poisoned");
    writer.write_all(&[0x03]).map_err(io_error)?;
    writer.flush().map_err(io_error)?;
    Ok(WorkerAckResponse { accepted: true })
}

fn forward_kill(runtime: &ControlRuntime) -> Result<WorkerAckResponse, ControlErrorBody> {
    signal_child(runtime, Signal::SIGKILL)?;
    Ok(WorkerAckResponse { accepted: true })
}

fn signal_child(runtime: &ControlRuntime, signal: Signal) -> Result<(), ControlErrorBody> {
    if let Some(pgid) = runtime.child_pgid {
        signal_raw_pid(negative_pid(pgid)?, signal)?;
        return Ok(());
    }
    if let Some(pid) = runtime.child_pid {
        signal_raw_pid(raw_pid(pid)?, signal)?;
        return Ok(());
    }

    Err(ControlErrorBody::new(
        ControlErrorCode::WorkerUnavailable,
        "worker does not know the child process id",
    ))
}

fn signal_raw_pid(pid: i32, signal: Signal) -> Result<(), ControlErrorBody> {
    match kill(Pid::from_raw(pid), signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(ControlErrorBody::new(
            ControlErrorCode::WorkerUnavailable,
            format!("failed to signal child process: {error}"),
        )),
    }
}

fn raw_pid(pid: u32) -> Result<i32, ControlErrorBody> {
    i32::try_from(pid).map_err(|_| {
        ControlErrorBody::new(
            ControlErrorCode::InvalidRequest,
            format!("process id {pid} does not fit in a platform pid"),
        )
    })
}

fn negative_pid(pid: u32) -> Result<i32, ControlErrorBody> {
    raw_pid(pid).map(|pid| -pid)
}

fn handle_observe_attach(
    mut stream: UnixStream,
    runtime: Arc<ControlRuntime>,
    request: WorkerControlRequest,
) -> MillmuxResult<()> {
    let params = match request.params_as::<WorkerAttachRequest>() {
        Ok(params) => params,
        Err(error) => {
            return write_worker_response(
                stream,
                WorkerControlResponse::failure(request.id, invalid_params(error)),
            )
        }
    };

    let attach = match acquire_attach(&runtime, &params) {
        Ok(attach) => attach,
        Err(error) => {
            return write_worker_response(stream, WorkerControlResponse::failure(request.id, error))
        }
    };
    let cleanup = InputReleaseGuard::new(Arc::clone(&runtime), params.stream_id.clone());

    let (sender, receiver) = mpsc::channel();
    runtime
        .state
        .lock()
        .expect("control state poisoned")
        .observers
        .push(sender);

    write_worker_response(
        stream.try_clone()?,
        WorkerControlResponse::success(request.id, &attach).map_err(MillmuxError::Json)?,
    )?;

    write_initial_replay(
        &mut stream,
        &runtime.paths,
        params.replay,
        params.requested_terminal_size,
    )?;

    for bytes in receiver {
        let frame = attach_output_frame(params.replay, bytes);
        if stream.write_all(frame.to_json_line()?.as_bytes()).is_err() {
            break;
        }
        if stream.flush().is_err() {
            break;
        }
    }

    cleanup.release();
    Ok(())
}

struct InputReleaseGuard {
    runtime: Arc<ControlRuntime>,
    stream_id: Option<String>,
}

impl InputReleaseGuard {
    fn new(runtime: Arc<ControlRuntime>, stream_id: String) -> Self {
        Self {
            runtime,
            stream_id: Some(stream_id),
        }
    }

    fn release(mut self) {
        if let Some(stream_id) = self.stream_id.take() {
            let _ = release_attach(&self.runtime, &stream_id);
        }
    }
}

impl Drop for InputReleaseGuard {
    fn drop(&mut self) {
        if let Some(stream_id) = self.stream_id.take() {
            let _ = release_attach(&self.runtime, &stream_id);
        }
    }
}

fn scrollback_frames(paths: &SessionPaths) -> Vec<AttachStreamFrame> {
    match ScrollbackBuffer::restore_snapshot(&paths.scrollback_snapshot) {
        Ok(snapshot) if !snapshot.is_empty() => {
            vec![AttachStreamFrame::Scrollback {
                lines: snapshot.lines(),
            }]
        }
        _ => Vec::new(),
    }
}

fn write_initial_replay(
    stream: &mut UnixStream,
    paths: &SessionPaths,
    replay: AttachReplayMode,
    requested_terminal_size: Option<TerminalDimensions>,
) -> MillmuxResult<()> {
    match replay {
        AttachReplayMode::LineScrollback => {
            for frame in scrollback_frames(paths) {
                stream.write_all(frame.to_json_line()?.as_bytes())?;
                stream.flush()?;
            }
        }
        AttachReplayMode::RawReplay | AttachReplayMode::TerminalSnapshot => {
            let current_offset = file_len(&paths.pty_log);
            if let Some(restored) = restore_terminal_replay(
                &paths.terminal_snapshot,
                &paths.raw_replay_ring,
                current_offset,
            )
            .unwrap_or(None)
            {
                if !replay_matches_requested_size(
                    replay,
                    requested_terminal_size,
                    &restored.snapshot,
                ) || restored.bytes.is_empty()
                {
                    return Ok(());
                }
                let frame = AttachStreamFrame::raw_output(restored.bytes);
                stream.write_all(frame.to_json_line()?.as_bytes())?;
                stream.flush()?;
            }
        }
        AttachReplayMode::None => {}
    }
    Ok(())
}

fn replay_matches_requested_size(
    replay: AttachReplayMode,
    requested_terminal_size: Option<TerminalDimensions>,
    snapshot: &millrace_sessions_core::scrollback::TerminalSnapshot,
) -> bool {
    match (replay, requested_terminal_size) {
        (AttachReplayMode::RawReplay, None) => true,
        (AttachReplayMode::RawReplay | AttachReplayMode::TerminalSnapshot, Some(size)) => {
            snapshot.same_size(size.rows, size.cols)
        }
        (AttachReplayMode::TerminalSnapshot, None) => false,
        _ => true,
    }
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn attach_output_frame(replay: AttachReplayMode, bytes: Vec<u8>) -> AttachStreamFrame {
    if replay.uses_raw_payloads() {
        AttachStreamFrame::raw_output(bytes)
    } else {
        AttachStreamFrame::Output {
            text: String::from_utf8_lossy(&bytes).to_string(),
        }
    }
}

fn write_worker_response(
    mut stream: UnixStream,
    response: WorkerControlResponse,
) -> MillmuxResult<()> {
    stream.write_all(response.to_json_line()?.as_bytes())?;
    stream.flush()?;
    Ok(())
}

fn invalid_params(error: serde_json::Error) -> ControlErrorBody {
    ControlErrorBody::new(
        ControlErrorCode::InvalidRequest,
        format!("invalid worker control params: {error}"),
    )
}

fn internal_error(error: serde_json::Error) -> ControlErrorBody {
    ControlErrorBody::new(
        ControlErrorCode::InternalError,
        format!("failed to encode worker control response: {error}"),
    )
}

fn io_error(error: std::io::Error) -> ControlErrorBody {
    ControlErrorBody::new(ControlErrorCode::IoError, error.to_string())
}

fn core_error(error: MillmuxError) -> ControlErrorBody {
    let code = match error {
        MillmuxError::Io(_) => ControlErrorCode::IoError,
        MillmuxError::Permission(_) => ControlErrorCode::PermissionError,
        _ => ControlErrorCode::InternalError,
    };
    ControlErrorBody::new(code, error.to_string())
}

#[cfg(test)]
mod tests {
    use millrace_sessions_core::scrollback::TerminalSnapshot;

    use super::*;

    #[test]
    fn control_state_allows_one_read_write_owner() {
        let mut state = ControlState::default();

        assert!(state.acquire_attach("a", false).unwrap().input_owner);
        let error = state.acquire_attach("b", false).unwrap_err();

        assert_eq!(error.code, ControlErrorCode::InputOwnerConflict);
        state.release_attach("a");
        assert!(state.acquire_attach("b", false).unwrap().input_owner);
    }

    #[test]
    fn control_state_allows_read_only_observers_without_ownership() {
        let mut state = ControlState::default();

        assert!(!state.acquire_attach("observer", true).unwrap().input_owner);
        assert!(state.input_owner.is_none());
        assert!(state.send_is_allowed(None).is_ok());
    }

    #[test]
    fn control_state_rejects_one_shot_send_while_owned() {
        let mut state = ControlState::default();
        state.acquire_attach("attach", false).unwrap();

        assert_eq!(
            state.send_is_allowed(None).unwrap_err().code,
            ControlErrorCode::InputOwnerConflict
        );
        assert!(state.send_is_allowed(Some("attach")).is_ok());
    }

    #[test]
    fn control_state_reports_attached_clients_and_input_owner() {
        let mut state = ControlState::default();

        let owner = state.acquire_attach("owner", false).unwrap();
        let observer = state.acquire_attach("observer", true).unwrap();

        assert!(owner.input_owner);
        assert!(!observer.input_owner);
        assert_eq!(state.attach_state().attached_clients, 2);
        assert_eq!(state.attach_state().input_owner.as_deref(), Some("owner"));

        state.release_attach("observer");
        assert_eq!(state.attach_state().attached_clients, 1);
        assert_eq!(state.attach_state().input_owner.as_deref(), Some("owner"));

        state.release_attach("owner");
        assert_eq!(state.attach_state().attached_clients, 0);
        assert_eq!(state.attach_state().input_owner, None);
    }

    #[test]
    fn control_request_round_trips_worker_send_params() {
        let request = WorkerControlRequest::with_params(
            "req",
            WorkerControlMethod::Send,
            &WorkerSendRequest {
                text: "hello\n".to_string(),
                owner: None,
            },
        )
        .unwrap();
        let line = request.to_json_line().unwrap();
        let decoded = WorkerControlRequest::from_json_line(&line).unwrap();

        assert_eq!(decoded.method, WorkerControlMethod::Send);
        assert_eq!(
            decoded.params_as::<WorkerSendRequest>().unwrap().text,
            "hello\n"
        );
    }

    #[test]
    fn worker_terminal_snapshot_replay_requires_requested_matching_size() {
        let snapshot = TerminalSnapshot {
            schema_version: 1,
            rows: 24,
            cols: 80,
            cursor_row: 0,
            cursor_col: 0,
            alternate_screen: true,
            pty_log_offset: 10,
            raw_replay_start_offset: 0,
            raw_replay_end_offset: 10,
            captured_at: "2026-05-26T00:00:00Z".to_string(),
            screen: vec!["ready".to_string()],
        };

        assert!(replay_matches_requested_size(
            AttachReplayMode::TerminalSnapshot,
            Some(TerminalDimensions { rows: 24, cols: 80 }),
            &snapshot
        ));
        assert!(!replay_matches_requested_size(
            AttachReplayMode::TerminalSnapshot,
            Some(TerminalDimensions {
                rows: 30,
                cols: 100
            }),
            &snapshot
        ));
        assert!(!replay_matches_requested_size(
            AttachReplayMode::TerminalSnapshot,
            None,
            &snapshot
        ));
        assert!(replay_matches_requested_size(
            AttachReplayMode::RawReplay,
            None,
            &snapshot
        ));
    }
}
