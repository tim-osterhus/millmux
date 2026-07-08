use std::{
    collections::BTreeSet,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender, TrySendError},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use millrace_sessions_core::{
    error::{MillmuxError, MillmuxResult},
    events::{append_event, current_timestamp, SessionEvent, SessionEventKind},
    protocol::{
        AttachFrameType, AttachInitialReplay, AttachReplayMode, AttachStreamEncoding,
        AttachStreamFrame, AttachStreamLagReason, ControlErrorBody, ControlErrorCode,
        SnapshotUnavailableReason, TerminalDimensions, WorkerAckResponse, WorkerAttachRequest,
        WorkerAttachResponse, WorkerAttachStateResponse, WorkerControlMethod, WorkerControlRequest,
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

const ATTACH_OBSERVER_QUEUE_CAPACITY: usize = 64;
const ATTACH_INPUT_QUEUE_CAPACITY: usize = 8;
const ATTACH_INPUT_MAX_BYTES: usize = 512;
const ATTACH_INPUT_MAX_TOTAL_BYTES: usize = 1024;
const ATTACH_INPUT_WRITE_CHUNK: usize = 128;
const ATTACH_INPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const ATTACH_OBSERVER_CLOSE_AFTER_DROPPED_BYTES: u64 = 16 * 1024 * 1024;
const ATTACH_STREAM_POLL: Duration = Duration::from_millis(25);
const ATTACH_STREAM_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct WorkerControlHandle {
    state: Arc<Mutex<ControlState>>,
    paths: SessionPaths,
}

impl WorkerControlHandle {
    pub fn broadcast_output(&self, bytes: &[u8], start_offset: u64, end_offset: u64) {
        if bytes.is_empty() {
            return;
        }
        let output = AttachOutput {
            bytes: bytes.to_vec(),
            start_offset,
            end_offset,
        };
        let mut lag_events = Vec::new();
        let mut state = self.state.lock().expect("control state poisoned");
        state.observers.retain_mut(|observer| {
            if observer.lag_pending && observer.accepts_frame_type(AttachFrameType::StreamLagged) {
                let notice = observer.lag_notice();
                match observer
                    .sender
                    .try_send(AttachObserverMessage::Lag(notice.clone()))
                {
                    Ok(()) => observer.clear_lag(),
                    Err(TrySendError::Full(_)) => {
                        lag_events
                            .push((observer.stream_id.clone(), observer.record_drop(&output)));
                        if observer.should_close() {
                            observer.shutdown();
                            return false;
                        }
                        return true;
                    }
                    Err(TrySendError::Disconnected(_)) => return false,
                }
            }

            let Some(output) = observer.trim_output(&output) else {
                return true;
            };

            match observer
                .sender
                .try_send(AttachObserverMessage::Output(output.clone()))
            {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    let notice = observer.record_drop(&output);
                    lag_events.push((observer.stream_id.clone(), notice));
                    if observer.should_close() {
                        observer.shutdown();
                        return false;
                    }
                    true
                }
                Err(TrySendError::Disconnected(_)) => false,
            }
        });
        drop(state);

        for (stream_id, notice) in lag_events {
            persist_attach_lag_event(&self.paths, &stream_id, &notice);
        }
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
        paths: config.paths.clone(),
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

#[derive(Debug, Clone)]
struct AttachOutput {
    bytes: Vec<u8>,
    start_offset: u64,
    end_offset: u64,
}

#[derive(Debug, Clone)]
struct AttachLagNotice {
    dropped_bytes: u64,
    dropped_from_offset: u64,
    dropped_to_offset: u64,
    current_pty_log_offset: u64,
}

#[derive(Debug, Clone)]
enum AttachObserverMessage {
    Output(AttachOutput),
    Lag(AttachLagNotice),
}

enum AttachInputCommand {
    Input(String),
    Resize { rows: u16, cols: u16 },
    Close,
}

struct AttachObserver {
    stream_id: String,
    sender: SyncSender<AttachObserverMessage>,
    shutdown: UnixStream,
    accepted_frame_types: Vec<AttachFrameType>,
    dropped_bytes: u64,
    dropped_from_offset: Option<u64>,
    dropped_to_offset: u64,
    current_pty_log_offset: u64,
    lag_pending: bool,
    live_start_offset: u64,
}

impl AttachObserver {
    fn accepts_frame_type(&self, frame_type: AttachFrameType) -> bool {
        self.accepted_frame_types.contains(&frame_type)
    }

    fn record_drop(&mut self, output: &AttachOutput) -> AttachLagNotice {
        if self.dropped_from_offset.is_none() {
            self.dropped_from_offset = Some(output.start_offset);
        }
        self.dropped_to_offset = output.end_offset;
        self.current_pty_log_offset = output.end_offset;
        self.dropped_bytes = self
            .dropped_bytes
            .saturating_add(output.end_offset.saturating_sub(output.start_offset));
        self.lag_pending = self.accepts_frame_type(AttachFrameType::StreamLagged);
        self.lag_notice()
    }

    fn trim_output(&self, output: &AttachOutput) -> Option<AttachOutput> {
        if output.end_offset <= self.live_start_offset {
            return None;
        }
        if output.start_offset >= self.live_start_offset {
            return Some(output.clone());
        }

        let drop_count = self.live_start_offset.saturating_sub(output.start_offset) as usize;
        let drop_count = drop_count.min(output.bytes.len());
        Some(AttachOutput {
            bytes: output.bytes[drop_count..].to_vec(),
            start_offset: self.live_start_offset,
            end_offset: output.end_offset,
        })
    }

    fn lag_notice(&self) -> AttachLagNotice {
        AttachLagNotice {
            dropped_bytes: self.dropped_bytes,
            dropped_from_offset: self.dropped_from_offset.unwrap_or(self.dropped_to_offset),
            dropped_to_offset: self.dropped_to_offset,
            current_pty_log_offset: self.current_pty_log_offset,
        }
    }

    fn clear_lag(&mut self) {
        self.dropped_bytes = 0;
        self.dropped_from_offset = None;
        self.dropped_to_offset = self.current_pty_log_offset;
        self.lag_pending = false;
    }

    fn should_close(&self) -> bool {
        self.dropped_bytes >= ATTACH_OBSERVER_CLOSE_AFTER_DROPPED_BYTES
    }

    fn shutdown(&self) {
        let _ = self.shutdown.shutdown(std::net::Shutdown::Both);
    }
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
    observers: Vec<AttachObserver>,
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
            negotiated_attach_protocol_version: None,
            negotiated_stream_encoding: None,
            negotiated_initial_replay: None,
            accepted_frame_types: Vec::new(),
        })
    }

    fn release_attach(&mut self, stream_id: &str) {
        if self.input_owner.as_deref() == Some(stream_id) {
            self.input_owner = None;
        }
        self.attaches.remove(stream_id);
        self.observers
            .retain(|observer| observer.stream_id != stream_id);
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
    let (mut result, state) = {
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
    result.negotiated_attach_protocol_version = params.negotiated_attach_protocol_version();
    result.negotiated_stream_encoding = params.negotiated_stream_encoding();
    result.negotiated_initial_replay = params.negotiated_initial_replay();
    result.accepted_frame_types = params.negotiated_frame_types();
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

fn persist_attach_lag_event(paths: &SessionPaths, stream_id: &str, notice: &AttachLagNotice) {
    let Ok(worker) = read_json::<WorkerMeta>(&paths.worker_json) else {
        return;
    };
    let mut event = SessionEvent::new(worker.session_id, SessionEventKind::AttachStreamLagged);
    event.message = Some("attach observer lagged behind PTY output".to_string());
    event
        .fields
        .insert("stream_id".to_string(), stream_id.to_string());
    event.fields.insert(
        "dropped_bytes".to_string(),
        notice.dropped_bytes.to_string(),
    );
    event.fields.insert(
        "dropped_from_offset".to_string(),
        notice.dropped_from_offset.to_string(),
    );
    event.fields.insert(
        "dropped_to_offset".to_string(),
        notice.dropped_to_offset.to_string(),
    );
    event.fields.insert(
        "current_pty_log_offset".to_string(),
        notice.current_pty_log_offset.to_string(),
    );
    event
        .fields
        .insert("reason".to_string(), "observer_backpressure".to_string());
    let _ = append_event(&paths.events_jsonl, &event);
}

fn send_text(
    runtime: &ControlRuntime,
    params: WorkerSendRequest,
) -> Result<WorkerSendResponse, ControlErrorBody> {
    {
        let state = runtime.state.lock().expect("control state poisoned");
        state.send_is_allowed(params.owner.as_deref())?;
    }

    let bytes_sent = write_pty_text(runtime, &params.text)?;
    Ok(WorkerSendResponse { bytes_sent })
}

fn write_pty_text(runtime: &ControlRuntime, text: &str) -> Result<usize, ControlErrorBody> {
    let mut writer = runtime.writer.lock().expect("pty writer poisoned");
    writer.write_all(text.as_bytes()).map_err(io_error)?;
    writer.flush().map_err(io_error)?;
    Ok(text.len())
}

fn write_pty_text_cancellable(
    runtime: &ControlRuntime,
    text: &str,
    closed: &AtomicBool,
) -> Result<usize, ControlErrorBody> {
    let mut bytes_sent = 0;
    for chunk in text.as_bytes().chunks(ATTACH_INPUT_WRITE_CHUNK) {
        if closed.load(Ordering::SeqCst) {
            break;
        }
        {
            let mut writer = runtime.writer.lock().expect("pty writer poisoned");
            writer.write_all(chunk).map_err(io_error)?;
            writer.flush().map_err(io_error)?;
        }
        bytes_sent += chunk.len();
    }
    Ok(bytes_sent)
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
    stream: UnixStream,
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
    if let Some(size) = params
        .requested_terminal_size
        .filter(|size| attach_resize_required(&runtime.paths, *size))
    {
        if let Err(error) = resize_pty(
            &runtime,
            WorkerResizeRequest {
                rows: size.rows,
                cols: size.cols,
            },
        ) {
            let _ = release_attach(&runtime, &params.stream_id);
            return write_worker_response(
                stream,
                WorkerControlResponse::failure(request.id, error),
            );
        }
    }

    let cleanup = InputReleaseGuard::new(Arc::clone(&runtime), params.stream_id.clone());
    let writer_stream = stream.try_clone()?;
    writer_stream.set_write_timeout(Some(ATTACH_STREAM_WRITE_TIMEOUT))?;
    let writer = Arc::new(Mutex::new(writer_stream));
    let (sender, receiver) = mpsc::sync_channel(ATTACH_OBSERVER_QUEUE_CAPACITY);
    let replay_cutoff = {
        let mut state = runtime.state.lock().expect("control state poisoned");
        let live_start_offset = file_len(&runtime.paths.pty_log);
        state.observers.push(AttachObserver {
            stream_id: params.stream_id.clone(),
            sender,
            shutdown: stream.try_clone()?,
            accepted_frame_types: params.negotiated_frame_types(),
            dropped_bytes: 0,
            dropped_from_offset: None,
            dropped_to_offset: live_start_offset,
            current_pty_log_offset: live_start_offset,
            lag_pending: false,
            live_start_offset,
        });
        live_start_offset
    };

    write_worker_response_locked(
        &writer,
        WorkerControlResponse::success(request.id, &attach).map_err(MillmuxError::Json)?,
    )?;

    let closed = Arc::new(AtomicBool::new(false));
    let (input_sender, input_receiver) = mpsc::sync_channel(ATTACH_INPUT_QUEUE_CAPACITY);
    let input_writer_runtime = Arc::clone(&runtime);
    let input_writer = Arc::clone(&writer);
    let input_writer_closed = Arc::clone(&closed);
    let (input_drained_sender, input_drained_receiver) = mpsc::channel();
    thread::spawn(move || {
        write_attach_input_loop(
            input_writer_runtime,
            input_writer,
            input_receiver,
            input_writer_closed,
        );
        let _ = input_drained_sender.send(());
    });
    let input_runtime = Arc::clone(&runtime);
    let input_writer = Arc::clone(&writer);
    let input_params = params.clone();
    let input_closed = Arc::clone(&closed);
    let (done_sender, done_receiver) = mpsc::channel();
    thread::spawn(move || {
        read_attach_input_loop(
            stream,
            input_runtime,
            input_writer,
            input_params,
            input_sender,
            input_closed,
        );
        let _ = done_sender.send(());
    });

    write_initial_replay(&writer, &runtime.paths, &params, replay_cutoff, &closed)?;

    loop {
        if done_receiver.try_recv().is_ok() {
            match input_drained_receiver.recv_timeout(ATTACH_INPUT_DRAIN_TIMEOUT) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    closed.store(true, Ordering::SeqCst);
                    let _ = write_attach_error_locked(
                        &writer,
                        ControlErrorBody::new(
                            ControlErrorCode::WorkerUnavailable,
                            format!(
                                "attach input writer did not drain within {}ms; closing attach stream",
                                ATTACH_INPUT_DRAIN_TIMEOUT.as_millis()
                            ),
                        ),
                    );
                }
            }
            break;
        }
        match receiver.recv_timeout(ATTACH_STREAM_POLL) {
            Ok(AttachObserverMessage::Output(output)) => {
                let frame = attach_live_output_frame(&params, output.bytes);
                if write_attach_frame_locked(&writer, &frame).is_err() {
                    break;
                }
            }
            Ok(AttachObserverMessage::Lag(notice)) => {
                if params
                    .negotiated_frame_types()
                    .contains(&AttachFrameType::StreamLagged)
                {
                    let frame = AttachStreamFrame::StreamLagged {
                        dropped_bytes: notice.dropped_bytes,
                        dropped_from_offset: notice.dropped_from_offset,
                        dropped_to_offset: notice.dropped_to_offset,
                        current_pty_log_offset: notice.current_pty_log_offset,
                        reason: AttachStreamLagReason::ObserverBackpressure,
                        recover: "request_screen_or_reattach_raw_replay".to_string(),
                    };
                    if write_attach_frame_locked(&writer, &frame).is_err() {
                        break;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    cleanup.release();
    let _ = write_attach_frame_locked(&writer, &AttachStreamFrame::Closed);
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
    writer: &Arc<Mutex<UnixStream>>,
    paths: &SessionPaths,
    params: &WorkerAttachRequest,
    replay_cutoff: u64,
    closed: &AtomicBool,
) -> MillmuxResult<()> {
    if closed.load(Ordering::SeqCst) {
        return Ok(());
    }
    if params.negotiated_attach_protocol_version().is_some() {
        return write_negotiated_initial_replay(writer, paths, params, replay_cutoff, closed);
    }
    write_legacy_initial_replay(
        writer,
        paths,
        params.replay,
        params.requested_terminal_size,
        replay_cutoff,
        closed,
    )
}

fn write_negotiated_initial_replay(
    writer: &Arc<Mutex<UnixStream>>,
    paths: &SessionPaths,
    params: &WorkerAttachRequest,
    replay_cutoff: u64,
    closed: &AtomicBool,
) -> MillmuxResult<()> {
    match params.negotiated_initial_replay() {
        None | Some(AttachInitialReplay::None) => Ok(()),
        Some(AttachInitialReplay::LineScrollback) => write_line_scrollback(writer, paths, closed),
        Some(AttachInitialReplay::RawReplay) => write_raw_initial_replay(
            writer,
            paths,
            AttachReplayMode::RawReplay,
            params.requested_terminal_size,
            replay_cutoff,
            closed,
        ),
        Some(AttachInitialReplay::ScreenSnapshot) => {
            if closed.load(Ordering::SeqCst) {
                return Ok(());
            }
            if params
                .negotiated_frame_types()
                .contains(&AttachFrameType::SnapshotUnavailable)
            {
                let frame = AttachStreamFrame::SnapshotUnavailable {
                    reason: SnapshotUnavailableReason::TerminalModelUnavailable,
                    details: None,
                };
                write_attach_frame_locked(writer, &frame)?;
            }
            Ok(())
        }
    }
}

fn write_legacy_initial_replay(
    writer: &Arc<Mutex<UnixStream>>,
    paths: &SessionPaths,
    replay: AttachReplayMode,
    requested_terminal_size: Option<TerminalDimensions>,
    replay_cutoff: u64,
    closed: &AtomicBool,
) -> MillmuxResult<()> {
    match replay {
        AttachReplayMode::LineScrollback => {
            write_line_scrollback(writer, paths, closed)?;
        }
        AttachReplayMode::RawReplay | AttachReplayMode::TerminalSnapshot => {
            write_raw_initial_replay(
                writer,
                paths,
                replay,
                requested_terminal_size,
                replay_cutoff,
                closed,
            )?;
        }
        AttachReplayMode::None => {}
    }
    Ok(())
}

fn write_line_scrollback(
    writer: &Arc<Mutex<UnixStream>>,
    paths: &SessionPaths,
    closed: &AtomicBool,
) -> MillmuxResult<()> {
    for frame in scrollback_frames(paths) {
        if closed.load(Ordering::SeqCst) {
            break;
        }
        write_attach_frame_locked(writer, &frame)?;
    }
    Ok(())
}

fn write_raw_initial_replay(
    writer: &Arc<Mutex<UnixStream>>,
    paths: &SessionPaths,
    replay: AttachReplayMode,
    requested_terminal_size: Option<TerminalDimensions>,
    replay_cutoff: u64,
    closed: &AtomicBool,
) -> MillmuxResult<()> {
    if closed.load(Ordering::SeqCst) {
        return Ok(());
    }
    if let Some(restored) = restore_terminal_replay(
        &paths.terminal_snapshot,
        &paths.raw_replay_ring,
        replay_cutoff,
    )
    .unwrap_or(None)
    {
        if !replay_matches_requested_size(replay, requested_terminal_size, &restored.snapshot)
            || restored.bytes.is_empty()
        {
            return Ok(());
        }
        if closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        let frame = AttachStreamFrame::raw_output(restored.bytes);
        write_attach_frame_locked(writer, &frame)?;
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

fn attach_resize_required(paths: &SessionPaths, size: TerminalDimensions) -> bool {
    let current_offset = file_len(&paths.pty_log);
    match restore_terminal_replay(
        &paths.terminal_snapshot,
        &paths.raw_replay_ring,
        current_offset,
    ) {
        Ok(Some(replay)) => !replay.snapshot.same_size(size.rows, size.cols),
        _ => true,
    }
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn attach_live_output_frame(params: &WorkerAttachRequest, bytes: Vec<u8>) -> AttachStreamFrame {
    if params.negotiated_stream_encoding() == Some(AttachStreamEncoding::RawBytes)
        && params
            .negotiated_frame_types()
            .contains(&AttachFrameType::RawOutput)
    {
        AttachStreamFrame::raw_output(bytes)
    } else {
        AttachStreamFrame::Output {
            text: String::from_utf8_lossy(&bytes).to_string(),
        }
    }
}

fn read_attach_input_loop(
    stream: UnixStream,
    runtime: Arc<ControlRuntime>,
    writer: Arc<Mutex<UnixStream>>,
    params: WorkerAttachRequest,
    input_sender: SyncSender<AttachInputCommand>,
    closed: Arc<AtomicBool>,
) {
    let mut reader = BufReader::new(stream);
    let mut accepted_input_bytes = 0_usize;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                closed.store(true, Ordering::SeqCst);
                break;
            }
            Ok(_) => {
                if handle_attach_input_line(
                    line.trim_end(),
                    &runtime,
                    &writer,
                    &params,
                    &input_sender,
                    &closed,
                    &mut accepted_input_bytes,
                ) {
                    break;
                }
            }
            Err(_) => {
                closed.store(true, Ordering::SeqCst);
                break;
            }
        }
    }
}

fn write_attach_input_loop(
    runtime: Arc<ControlRuntime>,
    writer: Arc<Mutex<UnixStream>>,
    input_receiver: mpsc::Receiver<AttachInputCommand>,
    closed: Arc<AtomicBool>,
) {
    for command in input_receiver {
        let result = match command {
            AttachInputCommand::Input(text) => write_pty_text_cancellable(&runtime, &text, &closed)
                .map(|bytes_sent| WorkerSendResponse { bytes_sent }),
            AttachInputCommand::Resize { rows, cols } => {
                resize_pty(&runtime, WorkerResizeRequest { rows, cols })
                    .map(|_| WorkerSendResponse { bytes_sent: 0 })
            }
            AttachInputCommand::Close => {
                closed.store(true, Ordering::SeqCst);
                break;
            }
        };
        if let Err(error) = result {
            let _ = write_attach_error_locked(&writer, error);
        }
    }
}

fn handle_attach_input_line(
    line: &str,
    runtime: &ControlRuntime,
    writer: &Arc<Mutex<UnixStream>>,
    params: &WorkerAttachRequest,
    input_sender: &SyncSender<AttachInputCommand>,
    closed: &AtomicBool,
    accepted_input_bytes: &mut usize,
) -> bool {
    let frame = match AttachStreamFrame::from_json_line(line) {
        Ok(frame) => frame,
        Err(error) => {
            let _ = write_attach_error_locked(
                writer,
                ControlErrorBody::new(
                    ControlErrorCode::InvalidRequest,
                    format!("invalid attach stream frame: {error}"),
                ),
            );
            return false;
        }
    };

    match frame {
        AttachStreamFrame::Input { text } => {
            let input_len = text.len();
            if input_len > ATTACH_INPUT_MAX_BYTES {
                let _ = write_attach_error_locked(
                    writer,
                    ControlErrorBody::new(
                        ControlErrorCode::InvalidRequest,
                        format!(
                            "attach input frame is {} bytes; maximum is {}",
                            input_len, ATTACH_INPUT_MAX_BYTES
                        ),
                    ),
                );
                return false;
            }
            if accepted_input_bytes.saturating_add(input_len) > ATTACH_INPUT_MAX_TOTAL_BYTES {
                let _ = write_attach_error_locked(
                    writer,
                    ControlErrorBody::new(
                        ControlErrorCode::InvalidRequest,
                        format!(
                            "attach input stream would exceed {} accepted bytes",
                            ATTACH_INPUT_MAX_TOTAL_BYTES
                        ),
                    ),
                );
                return false;
            }
            let allowed = {
                let state = runtime.state.lock().expect("control state poisoned");
                state.send_is_allowed(Some(&params.stream_id))
            };
            if let Err(error) = allowed {
                let _ = write_attach_error_locked(writer, error);
                return false;
            }
            match input_sender.try_send(AttachInputCommand::Input(text)) {
                Ok(()) => {
                    *accepted_input_bytes += input_len;
                }
                Err(TrySendError::Full(_)) => {
                    let _ = write_attach_error_locked(
                        writer,
                        ControlErrorBody::new(
                            ControlErrorCode::WorkerUnavailable,
                            "attach input queue is full",
                        ),
                    );
                }
                Err(TrySendError::Disconnected(_)) => {
                    let _ = write_attach_error_locked(
                        writer,
                        ControlErrorBody::new(
                            ControlErrorCode::WorkerUnavailable,
                            "attach input writer is unavailable",
                        ),
                    );
                    return true;
                }
            }
            false
        }
        AttachStreamFrame::Resize { rows, cols } => {
            match input_sender.try_send(AttachInputCommand::Resize { rows, cols }) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    let _ = write_attach_error_locked(
                        writer,
                        ControlErrorBody::new(
                            ControlErrorCode::WorkerUnavailable,
                            "attach input queue is full",
                        ),
                    );
                }
                Err(TrySendError::Disconnected(_)) => {
                    let _ = write_attach_error_locked(
                        writer,
                        ControlErrorBody::new(
                            ControlErrorCode::WorkerUnavailable,
                            "attach input writer is unavailable",
                        ),
                    );
                    return true;
                }
            }
            false
        }
        AttachStreamFrame::Close => {
            match input_sender.try_send(AttachInputCommand::Close) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                    closed.store(true, Ordering::SeqCst);
                }
            }
            true
        }
        _ => false,
    }
}

fn write_worker_response_locked(
    writer: &Arc<Mutex<UnixStream>>,
    response: WorkerControlResponse,
) -> MillmuxResult<()> {
    let mut writer = writer.lock().expect("attach stream writer poisoned");
    writer.write_all(response.to_json_line()?.as_bytes())?;
    writer.flush()?;
    Ok(())
}

fn write_attach_frame_locked(
    writer: &Arc<Mutex<UnixStream>>,
    frame: &AttachStreamFrame,
) -> MillmuxResult<()> {
    let mut writer = writer.lock().expect("attach stream writer poisoned");
    writer.write_all(frame.to_json_line()?.as_bytes())?;
    writer.flush()?;
    Ok(())
}

fn write_attach_error_locked(
    writer: &Arc<Mutex<UnixStream>>,
    error: ControlErrorBody,
) -> MillmuxResult<()> {
    write_attach_frame_locked(writer, &AttachStreamFrame::Error { error })
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
    use millrace_sessions_core::{
        ids::SessionId,
        paths::StatePaths,
        scrollback::{TerminalSnapshot, TerminalStateBuffer},
        state::ProcessState,
        storage::append_raw_pty_log,
    };

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
    fn control_state_release_removes_quiet_attach_observer() {
        let mut state = ControlState::default();
        let (shutdown, _peer) = UnixStream::pair().unwrap();
        let (sender, _receiver) = mpsc::sync_channel(ATTACH_OBSERVER_QUEUE_CAPACITY);

        state.acquire_attach("quiet", false).unwrap();
        state.observers.push(AttachObserver {
            stream_id: "quiet".to_string(),
            sender,
            shutdown,
            accepted_frame_types: Vec::new(),
            dropped_bytes: 0,
            dropped_from_offset: None,
            dropped_to_offset: 0,
            current_pty_log_offset: 0,
            lag_pending: false,
            live_start_offset: 0,
        });

        state.release_attach("quiet");

        assert_eq!(state.attach_state().attached_clients, 0);
        assert!(state.observers.is_empty());
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
    fn broadcast_output_records_lag_and_best_effort_notice_for_slow_observer() {
        let temp = tempfile::tempdir().unwrap();
        let state_paths = StatePaths::new(temp.path().join("state"));
        let session_id = SessionId::new();
        let session_paths = state_paths.session_paths(session_id);
        fs::create_dir_all(&session_paths.root).unwrap();
        write_json_atomic(
            &session_paths.worker_json,
            &WorkerMeta {
                session_id,
                pid: std::process::id(),
                child_pid: None,
                child_pgid: None,
                process_state: ProcessState::Running,
                started_at: "2026-05-20T18:00:00Z".to_string(),
                ended_at: None,
                exit_code: None,
                exit_signal: None,
                attached_clients: 1,
                input_owner: Some("stream-slow".to_string()),
                updated_at: "2026-05-20T18:00:00Z".to_string(),
            },
        )
        .unwrap();

        let (shutdown, _peer) = UnixStream::pair().unwrap();
        let (sender, receiver) = mpsc::sync_channel(ATTACH_OBSERVER_QUEUE_CAPACITY);
        let state = Arc::new(Mutex::new(ControlState::default()));
        state.lock().unwrap().observers.push(AttachObserver {
            stream_id: "stream-slow".to_string(),
            sender,
            shutdown,
            accepted_frame_types: vec![AttachFrameType::StreamLagged],
            dropped_bytes: 0,
            dropped_from_offset: None,
            dropped_to_offset: 0,
            current_pty_log_offset: 0,
            lag_pending: false,
            live_start_offset: 0,
        });
        let handle = WorkerControlHandle {
            state: Arc::clone(&state),
            paths: session_paths.clone(),
        };

        for offset in 0..ATTACH_OBSERVER_QUEUE_CAPACITY as u64 {
            handle.broadcast_output(b"x", offset, offset + 1);
        }
        handle.broadcast_output(b"y", 64, 65);

        let events = millrace_sessions_core::events::read_events(&session_paths.events_jsonl)
            .expect("lag event persists");
        assert!(events.iter().any(|event| {
            event.kind == SessionEventKind::AttachStreamLagged
                && event.fields.get("stream_id").map(String::as_str) == Some("stream-slow")
        }));

        let _ = receiver.try_recv().expect("slow observer queue is full");
        handle.broadcast_output(b"z", 65, 66);
        let messages = receiver.try_iter().collect::<Vec<_>>();
        assert!(messages.iter().any(|message| {
            matches!(
                message,
                AttachObserverMessage::Lag(notice)
                    if notice.dropped_bytes > 0
                        && notice.current_pty_log_offset >= notice.dropped_to_offset
            )
        }));
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

    #[test]
    fn attach_resize_is_skipped_when_fresh_replay_already_matches_requested_size() {
        let temp = tempfile::tempdir().unwrap();
        let state_paths = StatePaths::new(temp.path().join("state"));
        let session_paths = state_paths.session_paths(SessionId::new());
        fs::create_dir_all(&session_paths.root).unwrap();
        let raw = b"\x1b[?1049hraw\r\n";
        append_raw_pty_log(&session_paths.pty_log, raw).unwrap();
        let mut state = TerminalStateBuffer::new(24, 80, 1024, 0);
        state.process_output(raw);
        state
            .persist(
                &session_paths.terminal_snapshot,
                &session_paths.raw_replay_ring,
            )
            .unwrap();

        assert!(!attach_resize_required(
            &session_paths,
            TerminalDimensions { rows: 24, cols: 80 }
        ));
        assert!(attach_resize_required(
            &session_paths,
            TerminalDimensions {
                rows: 30,
                cols: 100
            }
        ));
    }

    #[test]
    fn worker_attach_live_output_requires_v2_raw_bytes_and_raw_output_acceptance() {
        let mut request = WorkerAttachRequest {
            stream_id: "stream-raw".to_string(),
            read_only: false,
            replay: AttachReplayMode::RawReplay,
            requested_terminal_size: None,
            client_protocol_version: None,
            accepted_frame_types: Vec::new(),
            stream_encoding: None,
            initial_replay: None,
        };

        assert!(matches!(
            attach_live_output_frame(&request, b"\xfflive".to_vec()),
            AttachStreamFrame::Output { .. }
        ));

        request.client_protocol_version = Some(2);
        request.stream_encoding = Some(AttachStreamEncoding::RawBytes);
        assert!(matches!(
            attach_live_output_frame(&request, b"\xfflive".to_vec()),
            AttachStreamFrame::Output { .. }
        ));

        request.accepted_frame_types = vec![AttachFrameType::RawOutput];
        assert!(matches!(
            attach_live_output_frame(&request, b"\xfflive".to_vec()),
            AttachStreamFrame::RawOutput { data } if data.as_slice() == b"\xfflive"
        ));
    }
}
