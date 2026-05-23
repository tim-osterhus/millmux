use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    sync::{mpsc, Arc, Mutex},
    thread,
};

use millrace_sessions_core::{
    error::{MillmuxError, MillmuxResult},
    protocol::{
        AttachStreamFrame, ControlErrorBody, ControlErrorCode, WorkerAckResponse,
        WorkerAttachRequest, WorkerAttachResponse, WorkerControlMethod, WorkerControlRequest,
        WorkerControlResponse, WorkerReleaseAttachRequest, WorkerResizeRequest,
        WorkerResizeResponse, WorkerSendRequest, WorkerSendResponse,
    },
    scrollback::ScrollbackBuffer,
    state::SessionPaths,
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
        let text = String::from_utf8_lossy(bytes).to_string();
        let mut state = self.state.lock().expect("control state poisoned");
        state
            .observers
            .retain(|observer| observer.send(text.clone()).is_ok());
    }
}

#[derive(Clone)]
pub struct WorkerControlConfig {
    pub paths: SessionPaths,
    pub writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pub master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
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
    child_pid: Option<u32>,
    child_pgid: Option<u32>,
    state: Arc<Mutex<ControlState>>,
}

#[derive(Default)]
struct ControlState {
    input_owner: Option<String>,
    observers: Vec<mpsc::Sender<String>>,
}

impl ControlState {
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

    fn release_input(&mut self, stream_id: &str) {
        if self.input_owner.as_deref() == Some(stream_id) {
            self.input_owner = None;
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
            let input_owner = runtime
                .state
                .lock()
                .expect("control state poisoned")
                .acquire_input(&params.stream_id, params.read_only)?;
            let result = WorkerAttachResponse {
                stream_id: params.stream_id,
                read_only: params.read_only,
                input_owner,
            };
            WorkerControlResponse::success(request.id, &result).map_err(internal_error)
        }
        WorkerControlMethod::ReleaseAttach => {
            let params = request
                .params_as::<WorkerReleaseAttachRequest>()
                .map_err(invalid_params)?;
            runtime
                .state
                .lock()
                .expect("control state poisoned")
                .release_input(&params.stream_id);
            WorkerControlResponse::success(request.id, &WorkerAckResponse { accepted: true })
                .map_err(internal_error)
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

    let input_owner = match runtime
        .state
        .lock()
        .expect("control state poisoned")
        .acquire_input(&params.stream_id, params.read_only)
    {
        Ok(input_owner) => input_owner,
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
        WorkerControlResponse::success(
            request.id,
            &WorkerAttachResponse {
                stream_id: params.stream_id.clone(),
                read_only: params.read_only,
                input_owner,
            },
        )
        .map_err(MillmuxError::Json)?,
    )?;

    if params.include_scrollback {
        for frame in scrollback_frames(&runtime.paths) {
            stream.write_all(frame.to_json_line()?.as_bytes())?;
            stream.flush()?;
        }
    }

    for text in receiver {
        let frame = AttachStreamFrame::Output { text };
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
            self.runtime
                .state
                .lock()
                .expect("control state poisoned")
                .release_input(&stream_id);
        }
    }
}

impl Drop for InputReleaseGuard {
    fn drop(&mut self) {
        if let Some(stream_id) = self.stream_id.take() {
            self.runtime
                .state
                .lock()
                .expect("control state poisoned")
                .release_input(&stream_id);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_state_allows_one_read_write_owner() {
        let mut state = ControlState::default();

        assert!(state.acquire_input("a", false).unwrap());
        let error = state.acquire_input("b", false).unwrap_err();

        assert_eq!(error.code, ControlErrorCode::InputOwnerConflict);
        state.release_input("a");
        assert!(state.acquire_input("b", false).unwrap());
    }

    #[test]
    fn control_state_allows_read_only_observers_without_ownership() {
        let mut state = ControlState::default();

        assert!(!state.acquire_input("observer", true).unwrap());
        assert!(state.input_owner.is_none());
        assert!(state.send_is_allowed(None).is_ok());
    }

    #[test]
    fn control_state_rejects_one_shot_send_while_owned() {
        let mut state = ControlState::default();
        state.acquire_input("attach", false).unwrap();

        assert_eq!(
            state.send_is_allowed(None).unwrap_err().code,
            ControlErrorCode::InputOwnerConflict
        );
        assert!(state.send_is_allowed(Some("attach")).is_ok());
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
}
