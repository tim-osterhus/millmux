use std::{
    collections::{BTreeSet, VecDeque},
    fs,
    io::{self, BufRead, BufReader, Write},
    net::Shutdown,
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender, TrySendError},
        Arc, Mutex, TryLockError,
    },
    thread,
    time::{Duration, Instant},
};

use millrace_sessions_core::{
    error::{MillmuxError, MillmuxResult},
    events::{append_event, current_timestamp, SessionEvent, SessionEventKind},
    protocol::{
        AttachFrameType, AttachInitialReplay, AttachReplayMode, AttachStreamEncoding,
        AttachStreamFrame, AttachStreamLagReason, ControlErrorBody, ControlErrorCode,
        ScreenSnapshot, SnapshotUnavailableReason, TerminalDimensions, WorkerAckResponse,
        WorkerAttachRequest, WorkerAttachResponse, WorkerAttachStateResponse, WorkerControlMethod,
        WorkerControlRequest, WorkerControlResponse, WorkerReleaseAttachRequest,
        WorkerResizeRequest, WorkerResizeResponse, WorkerSendRequest, WorkerSendResponse,
    },
    scrollback::{restore_terminal_replay_bytes, ScrollbackBuffer, TerminalStateBuffer},
    state::{SessionPaths, WorkerMeta},
    storage::{read_json, write_json_atomic},
};
use nix::{
    errno::Errno,
    libc,
    sys::signal::{kill, Signal},
    unistd::Pid,
};
use portable_pty::{MasterPty, PtySize};

#[cfg(test)]
use std::sync::Condvar;

const ATTACH_OBSERVER_QUEUE_CAPACITY: usize = 8;
const ATTACH_INPUT_QUEUE_CAPACITY: usize = 8;
const ATTACH_INPUT_MAX_BYTES: usize = 512;
const ATTACH_INPUT_WRITE_CHUNK: usize = 128;
const ATTACH_INPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const ATTACH_OBSERVER_CLOSE_AFTER_DROPPED_BYTES: u64 = 16 * 1024 * 1024;
const ATTACH_STREAM_POLL: Duration = Duration::from_millis(25);
const ATTACH_STREAM_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

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
            let Some(output) = observer.trim_output(&output) else {
                return true;
            };

            match observer.sender.try_send(output.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    let notice = observer.record_drop(&output);
                    if !observer.snapshot_replay_pending {
                        lag_events.push((observer.stream_id.clone(), notice));
                    }
                    if !observer.snapshot_replay_pending && observer.should_close() {
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
        #[cfg(test)]
        input_write_sync: Arc::new(InputWriteTestSync::default()),
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

enum AttachInputCommand {
    Input(Vec<u8>),
    Resize { rows: u16, cols: u16 },
}

struct AttachObserver {
    stream_id: String,
    sender: SyncSender<AttachOutput>,
    shutdown: UnixStream,
    accepted_frame_types: Vec<AttachFrameType>,
    dropped_bytes: u64,
    dropped_from_offset: Option<u64>,
    dropped_to_offset: u64,
    current_pty_log_offset: u64,
    lag_pending: bool,
    live_start_offset: u64,
    snapshot_replay_pending: bool,
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
        trim_attach_output(output, self.live_start_offset)
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

    fn reconcile_snapshot_frontier(&mut self, covered_offset: u64) -> Option<AttachLagNotice> {
        self.live_start_offset = self.live_start_offset.max(covered_offset);
        let remaining = self
            .lag_pending
            .then(|| self.lag_notice())
            .and_then(|notice| trim_attach_lag_notice(notice, covered_offset));
        match remaining {
            Some(notice) => {
                self.dropped_bytes = notice.dropped_bytes;
                self.dropped_from_offset = Some(notice.dropped_from_offset);
                self.dropped_to_offset = notice.dropped_to_offset;
                self.current_pty_log_offset = notice.current_pty_log_offset;
                Some(notice)
            }
            None => {
                self.clear_lag();
                None
            }
        }
    }

    fn should_close(&self) -> bool {
        self.dropped_bytes >= ATTACH_OBSERVER_CLOSE_AFTER_DROPPED_BYTES
    }

    fn shutdown(&self) {
        let _ = self.shutdown.shutdown(std::net::Shutdown::Both);
    }
}

fn trim_attach_output(output: &AttachOutput, live_start_offset: u64) -> Option<AttachOutput> {
    if output.end_offset <= live_start_offset {
        return None;
    }
    if output.start_offset >= live_start_offset {
        return Some(output.clone());
    }

    let drop_count = live_start_offset.saturating_sub(output.start_offset) as usize;
    let drop_count = drop_count.min(output.bytes.len());
    Some(AttachOutput {
        bytes: output.bytes[drop_count..].to_vec(),
        start_offset: live_start_offset,
        end_offset: output.end_offset,
    })
}

fn trim_attach_lag_notice(
    mut notice: AttachLagNotice,
    live_start_offset: u64,
) -> Option<AttachLagNotice> {
    if notice.dropped_to_offset <= live_start_offset {
        return None;
    }
    if notice.dropped_from_offset < live_start_offset {
        notice.dropped_from_offset = live_start_offset;
        notice.dropped_bytes = notice.dropped_to_offset.saturating_sub(live_start_offset);
    }
    Some(notice)
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
    #[cfg(test)]
    input_write_sync: Arc<InputWriteTestSync>,
}

#[cfg(test)]
#[derive(Default)]
struct InputWriteTestSync {
    state: Mutex<InputWriteTestState>,
    changed: Condvar,
}

#[cfg(test)]
#[derive(Default)]
struct InputWriteTestState {
    waiting: bool,
    active: bool,
    completed: bool,
    snapshot_capture_armed: bool,
    snapshot_capture_waiting: bool,
    snapshot_capture_released: bool,
    snapshot_reconciled_armed: bool,
    snapshot_reconciled_waiting: bool,
    snapshot_reconciled_released: bool,
}

#[cfg(test)]
impl InputWriteTestSync {
    fn mark_waiting(&self) {
        self.state.lock().unwrap().waiting = true;
        self.changed.notify_all();
    }

    fn mark_active(&self) {
        self.state.lock().unwrap().active = true;
        self.changed.notify_all();
    }

    fn mark_completed(&self) {
        self.state.lock().unwrap().completed = true;
        self.changed.notify_all();
    }

    fn wait_for(&self, label: &str, ready: impl Fn(&InputWriteTestState) -> bool) {
        let (state, _) = self
            .changed
            .wait_timeout_while(
                self.state.lock().unwrap(),
                ATTACH_INPUT_DRAIN_TIMEOUT,
                |state| !ready(state),
            )
            .unwrap();
        assert!(ready(&state), "timed out waiting for input writer {label}");
    }

    fn arm_snapshot_capture(&self) {
        self.state.lock().unwrap().snapshot_capture_armed = true;
    }

    fn pause_snapshot_capture(&self) {
        let mut state = self.state.lock().unwrap();
        if !state.snapshot_capture_armed {
            return;
        }
        state.snapshot_capture_waiting = true;
        self.changed.notify_all();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, ATTACH_INPUT_DRAIN_TIMEOUT, |state| {
                !state.snapshot_capture_released
            })
            .unwrap();
        assert!(
            state.snapshot_capture_released,
            "timed out waiting to release snapshot capture"
        );
    }

    fn release_snapshot_capture(&self) {
        self.state.lock().unwrap().snapshot_capture_released = true;
        self.changed.notify_all();
    }

    fn arm_snapshot_reconciled(&self) {
        self.state.lock().unwrap().snapshot_reconciled_armed = true;
    }

    fn pause_snapshot_reconciled(&self) {
        let mut state = self.state.lock().unwrap();
        if !state.snapshot_reconciled_armed {
            return;
        }
        state.snapshot_reconciled_waiting = true;
        self.changed.notify_all();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, ATTACH_INPUT_DRAIN_TIMEOUT, |state| {
                !state.snapshot_reconciled_released
            })
            .unwrap();
        assert!(
            state.snapshot_reconciled_released,
            "timed out waiting to release reconciled snapshot"
        );
    }

    fn release_snapshot_reconciled(&self) {
        self.state.lock().unwrap().snapshot_reconciled_released = true;
        self.changed.notify_all();
    }
}

#[cfg(test)]
struct InputWriteCompletion(Arc<InputWriteTestSync>);

#[cfg(test)]
impl Drop for InputWriteCompletion {
    fn drop(&mut self) {
        self.0.mark_completed();
    }
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
    write_pty_bytes(runtime, text.as_bytes())
}

fn write_pty_bytes(runtime: &ControlRuntime, bytes: &[u8]) -> Result<usize, ControlErrorBody> {
    let mut writer = runtime.writer.lock().expect("pty writer poisoned");
    writer.write_all(bytes).map_err(io_error)?;
    writer.flush().map_err(io_error)?;
    Ok(bytes.len())
}

fn write_pty_bytes_cancellable(
    runtime: &ControlRuntime,
    bytes: &[u8],
    closed: &AtomicBool,
) -> Result<usize, ControlErrorBody> {
    #[cfg(test)]
    let _completion = {
        runtime.input_write_sync.mark_waiting();
        InputWriteCompletion(Arc::clone(&runtime.input_write_sync))
    };
    let deadline = Instant::now() + ATTACH_INPUT_DRAIN_TIMEOUT;
    let _writer = loop {
        if closed.load(Ordering::SeqCst) {
            return Ok(0);
        }
        match runtime.writer.try_lock() {
            Ok(writer) => break writer,
            Err(TryLockError::Poisoned(_)) => panic!("pty writer poisoned"),
            Err(TryLockError::WouldBlock) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    closed.store(true, Ordering::SeqCst);
                    return Err(attach_input_drain_timeout());
                }
                thread::sleep(remaining.min(ATTACH_STREAM_POLL));
            }
        }
    };
    let fd = runtime
        .master
        .lock()
        .expect("pty master poisoned")
        .as_raw_fd()
        .ok_or_else(|| {
            ControlErrorBody::new(
                ControlErrorCode::WorkerUnavailable,
                "PTY master does not expose a raw file descriptor",
            )
        })?;
    let _nonblocking = PtyNonblockingGuard::activate(fd).map_err(io_error)?;
    #[cfg(test)]
    runtime.input_write_sync.mark_active();
    let mut bytes_sent = 0;
    for chunk in bytes.chunks(ATTACH_INPUT_WRITE_CHUNK) {
        let mut pending = chunk;
        while !pending.is_empty() {
            if closed.load(Ordering::SeqCst) {
                return Ok(bytes_sent);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                closed.store(true, Ordering::SeqCst);
                return Err(attach_input_drain_timeout());
            }

            let mut poll_fd = libc::pollfd {
                fd,
                events: libc::POLLOUT,
                revents: 0,
            };
            let poll_timeout = remaining.min(ATTACH_STREAM_POLL).as_millis().max(1) as i32;
            // SAFETY: poll_fd points to one initialized descriptor for this call.
            let ready = unsafe { libc::poll(&mut poll_fd, 1, poll_timeout) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(io_error(error));
            }
            if ready == 0 {
                continue;
            }
            if poll_fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(io_error(io::Error::other(
                    "PTY input descriptor became unavailable",
                )));
            }

            // SAFETY: pending is a valid byte slice and fd remains owned by runtime.
            let written =
                unsafe { libc::write(fd, pending.as_ptr().cast::<libc::c_void>(), pending.len()) };
            if written < 0 {
                let error = io::Error::last_os_error();
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) {
                    continue;
                }
                return Err(io_error(error));
            }
            if written == 0 {
                return Err(io_error(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "PTY input write made no progress",
                )));
            }
            let written = written as usize;
            bytes_sent += written;
            pending = &pending[written..];
        }
    }
    Ok(bytes_sent)
}

fn attach_input_drain_timeout() -> ControlErrorBody {
    ControlErrorBody::new(
        ControlErrorCode::WorkerUnavailable,
        format!(
            "PTY input did not drain within {}ms",
            ATTACH_INPUT_DRAIN_TIMEOUT.as_millis()
        ),
    )
}

struct PtyNonblockingGuard {
    fd: libc::c_int,
    original_flags: libc::c_int,
}

impl PtyNonblockingGuard {
    fn activate(fd: libc::c_int) -> io::Result<Self> {
        // SAFETY: fd is the live PTY master descriptor retained by ControlRuntime.
        let original_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if original_flags < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: F_SETFL updates only the file status flags for the live descriptor.
        if unsafe { libc::fcntl(fd, libc::F_SETFL, original_flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, original_flags })
    }
}

impl Drop for PtyNonblockingGuard {
    fn drop(&mut self) {
        // SAFETY: ControlRuntime retains the master for the guard's full lifetime.
        let _ = unsafe { libc::fcntl(self.fd, libc::F_SETFL, self.original_flags) };
    }
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
        .filter(|size| attach_resize_required(&runtime, *size))
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
            snapshot_replay_pending: params.negotiated_initial_replay()
                == Some(AttachInitialReplay::ScreenSnapshot),
        });
        live_start_offset
    };

    let response =
        WorkerControlResponse::success(request.id, &attach).map_err(MillmuxError::Json)?;
    write_worker_response_locked(&writer, response)?;

    let closed = Arc::new(AtomicBool::new(false));
    let (input_sender, input_receiver) = mpsc::sync_channel(ATTACH_INPUT_QUEUE_CAPACITY);
    let input_shutdown = stream.try_clone()?;
    let input_writer_runtime = Arc::clone(&runtime);
    let input_writer = Arc::clone(&writer);
    let input_writer_closed = Arc::clone(&closed);
    let (input_drained_sender, input_drained_receiver) = mpsc::channel();
    let mut input_writer_handle = Some(thread::spawn(move || {
        write_attach_input_loop(
            input_writer_runtime,
            input_writer,
            input_receiver,
            input_writer_closed,
        );
        let _ = input_drained_sender.send(());
    }));
    let input_runtime = Arc::clone(&runtime);
    let input_writer = Arc::clone(&writer);
    let input_params = params.clone();
    let input_closed = Arc::clone(&closed);
    let (done_sender, done_receiver) = mpsc::channel();
    let input_reader_handle = thread::spawn(move || {
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

    let mut reconciled_outputs = VecDeque::new();
    let (mut first_error, live_start_offset) =
        match write_initial_replay(&writer, &runtime, &params, replay_cutoff, &closed) {
            Ok(snapshot_offset) => {
                if params.negotiated_initial_replay() == Some(AttachInitialReplay::ScreenSnapshot) {
                    let (notice, outputs) = reconcile_snapshot_observer(
                        &runtime,
                        &params.stream_id,
                        snapshot_offset.unwrap_or(replay_cutoff),
                        &receiver,
                    );
                    reconciled_outputs = outputs;
                    if let Some(notice) = notice {
                        persist_attach_lag_event(&runtime.paths, &params.stream_id, &notice);
                    }
                    #[cfg(test)]
                    runtime.input_write_sync.pause_snapshot_reconciled();
                }
                (
                    None,
                    snapshot_offset.unwrap_or(replay_cutoff).max(replay_cutoff),
                )
            }
            Err(error) => (Some(error), replay_cutoff),
        };
    let mut input_finished = false;
    while first_error.is_none() {
        if let Some(notice) =
            take_observer_lag_notice(&runtime.state, &params.stream_id, live_start_offset)
        {
            if let Err(error) = write_lag_notice(&writer, &params, notice) {
                first_error = Some(error);
                break;
            }
        }
        if done_receiver.try_recv().is_ok() {
            input_finished = true;
            break;
        }
        let output = match reconciled_outputs.pop_front() {
            Some(output) => Ok(output),
            None => receiver.recv_timeout(ATTACH_STREAM_POLL),
        };
        match output {
            Ok(output) => {
                let Some(output) = trim_attach_output(&output, live_start_offset) else {
                    continue;
                };
                let frame = attach_live_output_frame(&params, output.bytes);
                if let Err(error) = write_attach_frame_locked(&writer, &frame) {
                    first_error = Some(error);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    if !input_finished {
        closed.store(true, Ordering::SeqCst);
    }
    let _ = input_shutdown.shutdown(Shutdown::Read);
    let _ = input_reader_handle.join();
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
    if let Some(input_writer_handle) = input_writer_handle.take() {
        let _ = input_writer_handle.join();
    }

    cleanup.release();
    let _ = write_attach_frame_locked(&writer, &AttachStreamFrame::Closed);
    if let Some(error) = first_error {
        return Err(error);
    }
    Ok(())
}

fn reconcile_snapshot_observer(
    runtime: &ControlRuntime,
    stream_id: &str,
    covered_offset: u64,
    receiver: &mpsc::Receiver<AttachOutput>,
) -> (Option<AttachLagNotice>, VecDeque<AttachOutput>) {
    let mut state = runtime.state.lock().expect("control state poisoned");
    let Some(observer) = state
        .observers
        .iter_mut()
        .find(|observer| observer.stream_id == stream_id)
    else {
        return (None, VecDeque::new());
    };
    let notice = observer.reconcile_snapshot_frontier(covered_offset);
    let mut outputs = VecDeque::new();
    while let Ok(output) = receiver.try_recv() {
        if let Some(output) = trim_attach_output(&output, covered_offset) {
            outputs.push_back(output);
        }
    }
    observer.snapshot_replay_pending = false;
    (notice, outputs)
}

fn take_observer_lag_notice(
    state: &Arc<Mutex<ControlState>>,
    stream_id: &str,
    live_start_offset: u64,
) -> Option<AttachLagNotice> {
    let mut state = state.lock().expect("control state poisoned");
    let observer = state
        .observers
        .iter_mut()
        .find(|observer| observer.stream_id == stream_id)?;
    if !observer.lag_pending {
        return None;
    }
    let notice = observer.lag_notice();
    observer.clear_lag();
    trim_attach_lag_notice(notice, live_start_offset)
}

fn write_lag_notice(
    writer: &Arc<Mutex<UnixStream>>,
    params: &WorkerAttachRequest,
    notice: AttachLagNotice,
) -> MillmuxResult<()> {
    if !params
        .negotiated_frame_types()
        .contains(&AttachFrameType::StreamLagged)
    {
        return Ok(());
    }
    let frame = AttachStreamFrame::StreamLagged {
        dropped_bytes: notice.dropped_bytes,
        dropped_from_offset: notice.dropped_from_offset,
        dropped_to_offset: notice.dropped_to_offset,
        current_pty_log_offset: notice.current_pty_log_offset,
        reason: AttachStreamLagReason::ObserverBackpressure,
        recover: "request_screen_or_reattach_raw_replay".to_string(),
    };
    write_attach_frame_locked(writer, &frame)
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
    runtime: &ControlRuntime,
    params: &WorkerAttachRequest,
    replay_cutoff: u64,
    closed: &AtomicBool,
) -> MillmuxResult<Option<u64>> {
    if closed.load(Ordering::SeqCst) {
        return Ok(None);
    }
    if params.negotiated_attach_protocol_version().is_some() {
        return write_negotiated_initial_replay(writer, runtime, params, replay_cutoff, closed);
    }
    write_legacy_initial_replay(
        writer,
        runtime,
        params,
        params.replay,
        params.requested_terminal_size,
        replay_cutoff,
        closed,
    )
    .map(|()| None)
}

fn write_negotiated_initial_replay(
    writer: &Arc<Mutex<UnixStream>>,
    runtime: &ControlRuntime,
    params: &WorkerAttachRequest,
    replay_cutoff: u64,
    closed: &AtomicBool,
) -> MillmuxResult<Option<u64>> {
    match params.negotiated_initial_replay() {
        None | Some(AttachInitialReplay::None) => Ok(None),
        Some(AttachInitialReplay::LineScrollback) => {
            write_line_scrollback(writer, &runtime.paths, closed).map(|()| None)
        }
        Some(AttachInitialReplay::RawReplay) => {
            write_raw_initial_replay(
                writer,
                runtime,
                params,
                AttachReplayMode::RawReplay,
                params.requested_terminal_size,
                replay_cutoff,
                closed,
            )?;
            Ok(None)
        }
        Some(AttachInitialReplay::ScreenSnapshot) => {
            if closed.load(Ordering::SeqCst) {
                return Ok(None);
            }
            let (frame, suffix, covered_offset) = screen_snapshot_initial_replay_frame(
                runtime,
                params.requested_terminal_size,
                params
                    .negotiated_frame_types()
                    .contains(&AttachFrameType::RawOutput),
            );
            write_attach_frame_locked(writer, &frame)?;
            if !suffix.is_empty() {
                write_attach_frame_locked(writer, &AttachStreamFrame::raw_output(suffix))?;
            }
            Ok(covered_offset)
        }
    }
}

fn screen_snapshot_initial_replay_frame(
    runtime: &ControlRuntime,
    requested_terminal_size: Option<TerminalDimensions>,
    accepts_raw_output: bool,
) -> (AttachStreamFrame, Vec<u8>, Option<u64>) {
    #[cfg(test)]
    runtime.input_write_sync.pause_snapshot_capture();
    let (snapshot, suffix, covered_offset) = match runtime.terminal_state.lock() {
        Ok(terminal_state) => match terminal_state.screen_snapshot_replay() {
            Some(replay) => replay,
            None => {
                return (
                    AttachStreamFrame::SnapshotUnavailable {
                        reason: SnapshotUnavailableReason::StaleSnapshot,
                        details: Some(serde_json::json!({
                            "message": "structured snapshot parser frontier is outside retained raw replay"
                        })),
                    },
                    Vec::new(),
                    None,
                );
            }
        },
        Err(_) => {
            return (
                AttachStreamFrame::SnapshotUnavailable {
                    reason: SnapshotUnavailableReason::InternalError,
                    details: Some(serde_json::json!({
                        "message": "terminal state lock poisoned"
                    })),
                },
                Vec::new(),
                None,
            );
        }
    };
    if !suffix.is_empty() && !accepts_raw_output {
        return (
            AttachStreamFrame::SnapshotUnavailable {
                reason: SnapshotUnavailableReason::StaleSnapshot,
                details: Some(serde_json::json!({
                    "message": "structured snapshot parser continuation requires raw output replay"
                })),
            },
            Vec::new(),
            None,
        );
    }
    let frame = screen_snapshot_frame(snapshot, requested_terminal_size);
    if matches!(frame, AttachStreamFrame::ScreenSnapshot { .. }) {
        (frame, suffix, Some(covered_offset))
    } else {
        (frame, Vec::new(), None)
    }
}

fn screen_snapshot_frame(
    snapshot: ScreenSnapshot,
    requested_terminal_size: Option<TerminalDimensions>,
) -> AttachStreamFrame {
    if let Some(size) = requested_terminal_size {
        if snapshot.rows != size.rows || snapshot.cols != size.cols {
            return AttachStreamFrame::SnapshotUnavailable {
                reason: SnapshotUnavailableReason::SizeMismatch,
                details: Some(serde_json::json!({
                    "requested_rows": size.rows,
                    "requested_cols": size.cols,
                    "snapshot_rows": snapshot.rows,
                    "snapshot_cols": snapshot.cols
                })),
            };
        }
    }

    match AttachStreamFrame::screen_snapshot(snapshot) {
        Ok(frame) => frame,
        Err(error) => AttachStreamFrame::SnapshotUnavailable {
            reason: error.unavailable_reason(),
            details: error.unavailable_details(),
        },
    }
}

fn write_legacy_initial_replay(
    writer: &Arc<Mutex<UnixStream>>,
    runtime: &ControlRuntime,
    params: &WorkerAttachRequest,
    replay: AttachReplayMode,
    requested_terminal_size: Option<TerminalDimensions>,
    replay_cutoff: u64,
    closed: &AtomicBool,
) -> MillmuxResult<()> {
    match replay {
        AttachReplayMode::LineScrollback => {
            write_line_scrollback(writer, &runtime.paths, closed)?;
        }
        AttachReplayMode::RawReplay | AttachReplayMode::TerminalSnapshot => {
            write_raw_initial_replay(
                writer,
                runtime,
                params,
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
    runtime: &ControlRuntime,
    params: &WorkerAttachRequest,
    replay: AttachReplayMode,
    requested_terminal_size: Option<TerminalDimensions>,
    replay_cutoff: u64,
    closed: &AtomicBool,
) -> MillmuxResult<()> {
    if closed.load(Ordering::SeqCst) {
        return Ok(());
    }
    if replay == AttachReplayMode::TerminalSnapshot {
        return write_persisted_terminal_snapshot_replay(
            writer,
            &runtime.paths,
            requested_terminal_size,
            replay_cutoff,
            closed,
        );
    }
    let replay_bytes = runtime
        .terminal_state
        .lock()
        .ok()
        .and_then(|terminal_state| terminal_state.raw_replay_through(replay_cutoff));
    let Some((bytes, rows, cols)) = replay_bytes else {
        return write_raw_replay_unavailable(
            writer,
            params,
            SnapshotUnavailableReason::StaleSnapshot,
            "live terminal replay does not cover the attach cutoff",
        );
    };
    if !replay_matches_requested_size(replay, requested_terminal_size, rows, cols) {
        return write_raw_replay_unavailable(
            writer,
            params,
            SnapshotUnavailableReason::SizeMismatch,
            "live terminal replay size does not match the requested terminal",
        );
    }
    if closed.load(Ordering::SeqCst) {
        return Ok(());
    }
    if !bytes.is_empty() || params.negotiated_attach_protocol_version().is_some() {
        write_attach_frame_locked(writer, &AttachStreamFrame::raw_output(bytes))?;
    }
    Ok(())
}

fn write_persisted_terminal_snapshot_replay(
    writer: &Arc<Mutex<UnixStream>>,
    paths: &SessionPaths,
    requested_terminal_size: Option<TerminalDimensions>,
    replay_cutoff: u64,
    closed: &AtomicBool,
) -> MillmuxResult<()> {
    let Some(restored) = restore_terminal_replay_bytes(
        &paths.terminal_snapshot,
        &paths.raw_replay_ring,
        replay_cutoff,
    )
    .unwrap_or(None) else {
        return Ok(());
    };
    if !replay_matches_requested_size(
        AttachReplayMode::TerminalSnapshot,
        requested_terminal_size,
        restored.metadata.rows,
        restored.metadata.cols,
    ) || restored.bytes.is_empty()
        || closed.load(Ordering::SeqCst)
    {
        return Ok(());
    }
    write_attach_frame_locked(writer, &AttachStreamFrame::raw_output(restored.bytes))
}

fn write_raw_replay_unavailable(
    writer: &Arc<Mutex<UnixStream>>,
    params: &WorkerAttachRequest,
    reason: SnapshotUnavailableReason,
    message: &str,
) -> MillmuxResult<()> {
    if params.client_accepts_frame_type(AttachFrameType::SnapshotUnavailable) {
        return write_attach_frame_locked(
            writer,
            &AttachStreamFrame::SnapshotUnavailable {
                reason,
                details: Some(serde_json::json!({ "message": message })),
            },
        );
    }
    Err(MillmuxError::Internal(message.to_string()))
}

fn replay_matches_requested_size(
    replay: AttachReplayMode,
    requested_terminal_size: Option<TerminalDimensions>,
    rows: u16,
    cols: u16,
) -> bool {
    match (replay, requested_terminal_size) {
        (AttachReplayMode::RawReplay, None) => true,
        (AttachReplayMode::RawReplay | AttachReplayMode::TerminalSnapshot, Some(size)) => {
            rows == size.rows && cols == size.cols
        }
        (AttachReplayMode::TerminalSnapshot, None) => false,
        _ => true,
    }
}

fn attach_resize_required(runtime: &ControlRuntime, size: TerminalDimensions) -> bool {
    runtime
        .terminal_state
        .lock()
        .map(|terminal_state| !terminal_state.same_size(size.rows, size.cols))
        .unwrap_or(true)
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
        if closed.load(Ordering::SeqCst) {
            break;
        }
        let result = match command {
            AttachInputCommand::Input(bytes) => {
                write_pty_bytes_cancellable(&runtime, &bytes, &closed)
                    .map(|bytes_sent| WorkerSendResponse { bytes_sent })
            }
            AttachInputCommand::Resize { rows, cols } => {
                resize_pty(&runtime, WorkerResizeRequest { rows, cols })
                    .map(|_| WorkerSendResponse { bytes_sent: 0 })
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
) -> bool {
    if closed.load(Ordering::SeqCst) {
        return true;
    }
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
            let bytes = text.into_bytes();
            let allowed = {
                let state = runtime.state.lock().expect("control state poisoned");
                state.send_is_allowed(Some(&params.stream_id))
            };
            if let Err(error) = allowed {
                let _ = write_attach_error_locked(writer, error);
                return false;
            }
            if queue_attach_input_bytes(writer, input_sender, closed, bytes) {
                return true;
            }
            false
        }
        AttachStreamFrame::RawInput { data } => {
            let allowed = {
                let state = runtime.state.lock().expect("control state poisoned");
                validate_raw_input_allowed(&state, params)
            };
            if let Err(error) = allowed {
                let _ = write_attach_error_locked(writer, error);
                return false;
            }
            if queue_attach_input_bytes(writer, input_sender, closed, data.into_vec()) {
                return true;
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
        AttachStreamFrame::Close => true,
        _ => false,
    }
}

fn validate_raw_input_allowed(
    state: &ControlState,
    params: &WorkerAttachRequest,
) -> Result<(), ControlErrorBody> {
    if params.negotiated_attach_protocol_version().is_none()
        || params.negotiated_stream_encoding() != Some(AttachStreamEncoding::RawBytes)
        || !params
            .negotiated_frame_types()
            .contains(&AttachFrameType::RawOutput)
    {
        return Err(ControlErrorBody::new(
            ControlErrorCode::InvalidRequest,
            "raw_input requires a negotiated raw byte attach stream",
        ));
    }
    if params.read_only {
        return Err(ControlErrorBody::new(
            ControlErrorCode::InvalidRequest,
            "raw_input is not allowed on read-only attach streams",
        ));
    }
    if !params
        .negotiated_frame_types()
        .contains(&AttachFrameType::RawInput)
    {
        return Err(ControlErrorBody::new(
            ControlErrorCode::InvalidRequest,
            "raw_input requires negotiated raw_input support",
        ));
    }
    state.send_is_allowed(Some(&params.stream_id))
}

fn queue_attach_input_bytes(
    writer: &Arc<Mutex<UnixStream>>,
    input_sender: &SyncSender<AttachInputCommand>,
    closed: &AtomicBool,
    bytes: Vec<u8>,
) -> bool {
    let input_len = bytes.len();
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
    let deadline = Instant::now() + ATTACH_INPUT_DRAIN_TIMEOUT;
    let mut command = AttachInputCommand::Input(bytes);
    loop {
        match input_sender.try_send(command) {
            Ok(()) => return false,
            Err(TrySendError::Full(pending)) => {
                command = pending;
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    closed.store(true, Ordering::SeqCst);
                    let _ = write_attach_error_locked(
                        writer,
                        ControlErrorBody::new(
                            ControlErrorCode::WorkerUnavailable,
                            format!(
                                "attach input queue did not drain within {}ms",
                                ATTACH_INPUT_DRAIN_TIMEOUT.as_millis()
                            ),
                        ),
                    );
                    return true;
                }
                thread::sleep(remaining.min(ATTACH_STREAM_POLL));
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
    use std::{
        io::{BufRead, BufReader, Read},
        os::fd::AsRawFd,
    };

    use millrace_sessions_core::{
        ids::SessionId,
        paths::StatePaths,
        scrollback::TerminalStateBuffer,
        state::{ProcessState, SpawnMode, WorkerMeta},
        storage::append_raw_pty_log,
    };
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

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
            snapshot_replay_pending: false,
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
    fn broadcast_output_delivers_one_lag_notice_for_one_dropped_interval() {
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
                spawn_mode: SpawnMode::Pty,
                process_state: ProcessState::Running,
                started_at: "2026-05-20T18:00:00Z".to_string(),
                ended_at: None,
                stop_requested_at: None,
                stop_reason: None,
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
            snapshot_replay_pending: false,
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
        let notice = take_observer_lag_notice(&state, "stream-slow", 0)
            .expect("dropped interval has one delivery owner");
        assert!(notice.dropped_bytes > 0);
        assert!(notice.current_pty_log_offset >= notice.dropped_to_offset);
        assert!(take_observer_lag_notice(&state, "stream-slow", 0).is_none());
    }

    #[test]
    fn worker_terminal_snapshot_replay_requires_requested_matching_size() {
        assert!(replay_matches_requested_size(
            AttachReplayMode::TerminalSnapshot,
            Some(TerminalDimensions { rows: 24, cols: 80 }),
            24,
            80
        ));
        assert!(!replay_matches_requested_size(
            AttachReplayMode::TerminalSnapshot,
            Some(TerminalDimensions {
                rows: 30,
                cols: 100
            }),
            24,
            80
        ));
        assert!(!replay_matches_requested_size(
            AttachReplayMode::TerminalSnapshot,
            None,
            24,
            80
        ));
        assert!(replay_matches_requested_size(
            AttachReplayMode::RawReplay,
            None,
            24,
            80
        ));
    }

    #[test]
    fn worker_raw_replay_uses_live_cutoff_and_reports_unavailable_coverage() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = attach_test_runtime(&temp, 3, 20);
        runtime
            .terminal_state
            .lock()
            .unwrap()
            .process_output(b"abc");
        let mut params = raw_worker_attach_request("raw-live-replay", true);
        params.initial_replay = Some(AttachInitialReplay::RawReplay);
        params
            .accepted_frame_types
            .push(AttachFrameType::SnapshotUnavailable);
        let closed = AtomicBool::new(false);
        let (mut client, server) = UnixStream::pair().unwrap();
        let writer = Arc::new(Mutex::new(server));

        write_raw_initial_replay(
            &writer,
            &runtime,
            &params,
            AttachReplayMode::RawReplay,
            None,
            2,
            &closed,
        )
        .unwrap();
        let frame =
            AttachStreamFrame::from_json_line(read_json_line_bytewise(&mut client).trim_end())
                .unwrap();
        assert!(matches!(
            frame,
            AttachStreamFrame::RawOutput { data } if data.as_slice() == b"ab"
        ));

        let (mut client, server) = UnixStream::pair().unwrap();
        let writer = Arc::new(Mutex::new(server));
        write_raw_initial_replay(
            &writer,
            &runtime,
            &params,
            AttachReplayMode::RawReplay,
            None,
            4,
            &closed,
        )
        .unwrap();
        let frame =
            AttachStreamFrame::from_json_line(read_json_line_bytewise(&mut client).trim_end())
                .unwrap();
        assert!(matches!(
            frame,
            AttachStreamFrame::SnapshotUnavailable {
                reason: SnapshotUnavailableReason::StaleSnapshot,
                ..
            }
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
        let runtime = ControlRuntime {
            paths: session_paths,
            writer: Arc::new(Mutex::new(Box::new(Vec::new()))),
            master: Arc::new(Mutex::new(
                native_pty_system()
                    .openpty(PtySize {
                        rows: 24,
                        cols: 80,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .unwrap()
                    .master,
            )),
            terminal_state: Arc::new(Mutex::new(state)),
            child_pid: None,
            child_pgid: None,
            state: Arc::new(Mutex::new(ControlState::default())),
            input_write_sync: Arc::new(InputWriteTestSync::default()),
        };

        assert!(!attach_resize_required(
            &runtime,
            TerminalDimensions { rows: 24, cols: 80 }
        ));
        assert!(attach_resize_required(
            &runtime,
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

    #[test]
    fn worker_observe_attach_raw_replay_none_writes_response_before_frames() {
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
                spawn_mode: SpawnMode::Pty,
                process_state: ProcessState::Running,
                started_at: "2026-07-08T00:00:00Z".to_string(),
                ended_at: None,
                stop_requested_at: None,
                stop_reason: None,
                exit_code: None,
                exit_signal: None,
                attached_clients: 0,
                input_owner: None,
                updated_at: "2026-07-08T00:00:00Z".to_string(),
            },
        )
        .unwrap();

        let runtime = Arc::new(ControlRuntime {
            paths: session_paths,
            writer: Arc::new(Mutex::new(Box::new(Vec::new()))),
            master: Arc::new(Mutex::new(
                native_pty_system()
                    .openpty(PtySize {
                        rows: 24,
                        cols: 80,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .unwrap()
                    .master,
            )),
            terminal_state: Arc::new(Mutex::new(TerminalStateBuffer::new(24, 80, 128, 0))),
            child_pid: None,
            child_pgid: None,
            state: Arc::new(Mutex::new(ControlState::default())),
            input_write_sync: Arc::new(InputWriteTestSync::default()),
        });
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut attach_request = raw_worker_attach_request("raw-observer", true);
        attach_request.requested_terminal_size = Some(TerminalDimensions { rows: 24, cols: 80 });
        attach_request.accepted_frame_types = vec![
            AttachFrameType::RawOutput,
            AttachFrameType::StreamLagged,
            AttachFrameType::SnapshotUnavailable,
            AttachFrameType::ScreenSnapshot,
        ];
        let request = WorkerControlRequest::with_params(
            "observe-raw",
            WorkerControlMethod::ObserveAttach,
            &attach_request,
        )
        .unwrap();

        client
            .write_all(request.to_json_line().unwrap().as_bytes())
            .unwrap();
        let handle = thread::spawn(move || handle_connection(server, runtime));

        let mut reader = BufReader::new(client.try_clone().unwrap());
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("read observe attach response");
        assert!(!line.is_empty(), "worker closed before observe response");
        let response = WorkerControlResponse::from_json_line(line.trim_end()).unwrap();
        assert!(response.ok, "{response:#?}");
        let attach: WorkerAttachResponse = response.result_as().unwrap();
        assert_eq!(
            attach.negotiated_initial_replay,
            Some(AttachInitialReplay::None)
        );
        assert_eq!(
            attach.accepted_frame_types,
            vec![AttachFrameType::RawOutput, AttachFrameType::StreamLagged]
        );

        client
            .write_all(AttachStreamFrame::Close.to_json_line().unwrap().as_bytes())
            .unwrap();
        handle.join().unwrap().unwrap();
    }

    #[test]
    fn worker_screen_snapshot_initial_replay_uses_structured_frame_not_raw_output() {
        let mut terminal_state = TerminalStateBuffer::new(3, 10, 128, 0);
        terminal_state.process_output(b"ready");

        let frame = screen_snapshot_frame(
            terminal_state.screen_snapshot(),
            Some(TerminalDimensions { rows: 3, cols: 10 }),
        );

        assert!(matches!(
            &frame,
            AttachStreamFrame::ScreenSnapshot { snapshot }
                if snapshot.rows == 3
                    && snapshot.cols == 10
                    && snapshot.cells[0][0].symbol == "r"
        ));
        assert!(!matches!(&frame, AttachStreamFrame::RawOutput { .. }));
    }

    #[test]
    fn worker_snapshot_frontier_discards_covered_queue_without_false_lag() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = attach_test_runtime(&temp, 3, 20);
        let covered_offset = ATTACH_OBSERVER_QUEUE_CAPACITY as u64;
        *runtime.terminal_state.lock().unwrap() =
            TerminalStateBuffer::new(3, 20, 128, covered_offset);
        runtime.input_write_sync.arm_snapshot_capture();
        runtime.input_write_sync.arm_snapshot_reconciled();
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut attach_request = raw_worker_attach_request("snapshot-boundary", true);
        attach_request.requested_terminal_size = Some(TerminalDimensions { rows: 3, cols: 20 });
        attach_request.initial_replay = Some(AttachInitialReplay::ScreenSnapshot);
        attach_request.accepted_frame_types.extend([
            AttachFrameType::ScreenSnapshot,
            AttachFrameType::SnapshotUnavailable,
        ]);
        let request = WorkerControlRequest::with_params(
            "observe-snapshot-boundary",
            WorkerControlMethod::ObserveAttach,
            &attach_request,
        )
        .unwrap();
        client
            .write_all(request.to_json_line().unwrap().as_bytes())
            .unwrap();
        let handler_runtime = Arc::clone(&runtime);
        let handle = thread::spawn(move || handle_connection(server, handler_runtime));

        let response = read_json_line_bytewise(&mut client);
        assert!(
            WorkerControlResponse::from_json_line(response.trim_end())
                .unwrap()
                .ok
        );
        runtime
            .input_write_sync
            .wait_for("snapshot capture", |state| state.snapshot_capture_waiting);

        let control = WorkerControlHandle {
            state: Arc::clone(&runtime.state),
            paths: runtime.paths.clone(),
        };
        for index in 0..covered_offset {
            control.broadcast_output(b"x", index, index + 1);
        }
        runtime.input_write_sync.release_snapshot_capture();

        runtime
            .input_write_sync
            .wait_for("snapshot reconciliation", |state| {
                state.snapshot_reconciled_waiting
            });
        runtime
            .terminal_state
            .lock()
            .unwrap()
            .process_output(b"live");
        control.broadcast_output(b"live", covered_offset, covered_offset + 4);
        runtime.input_write_sync.release_snapshot_reconciled();

        let snapshot_frame =
            AttachStreamFrame::from_json_line(read_json_line_bytewise(&mut client).trim_end())
                .unwrap();
        assert!(matches!(
            snapshot_frame,
            AttachStreamFrame::ScreenSnapshot { snapshot }
                if snapshot.source.pty_log_offset == covered_offset
        ));

        let live_frame =
            AttachStreamFrame::from_json_line(read_json_line_bytewise(&mut client).trim_end())
                .unwrap();
        assert!(matches!(
            live_frame,
            AttachStreamFrame::RawOutput { data } if data.as_slice() == b"live"
        ));
        assert!(!fs::read_to_string(&runtime.paths.events_jsonl)
            .unwrap_or_default()
            .contains("attach observer lagged"));

        client
            .write_all(AttachStreamFrame::Close.to_json_line().unwrap().as_bytes())
            .unwrap();
        handle.join().unwrap().unwrap();
    }

    #[test]
    fn worker_snapshot_replay_sends_incomplete_parser_suffix_before_live_output() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = attach_test_runtime(&temp, 3, 20);
        runtime.terminal_state.lock().unwrap().process_output(b"e");
        runtime
            .terminal_state
            .lock()
            .unwrap()
            .process_output(&"界".as_bytes()[..2]);
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut attach_request = raw_worker_attach_request("snapshot-suffix", true);
        attach_request.requested_terminal_size = Some(TerminalDimensions { rows: 3, cols: 20 });
        attach_request.initial_replay = Some(AttachInitialReplay::ScreenSnapshot);
        attach_request.accepted_frame_types.extend([
            AttachFrameType::ScreenSnapshot,
            AttachFrameType::SnapshotUnavailable,
        ]);
        let request = WorkerControlRequest::with_params(
            "observe-snapshot-suffix",
            WorkerControlMethod::ObserveAttach,
            &attach_request,
        )
        .unwrap();
        client
            .write_all(request.to_json_line().unwrap().as_bytes())
            .unwrap();
        let handler_runtime = Arc::clone(&runtime);
        let handle = thread::spawn(move || handle_connection(server, handler_runtime));

        let response = read_json_line_bytewise(&mut client);
        assert!(
            WorkerControlResponse::from_json_line(response.trim_end())
                .unwrap()
                .ok
        );
        let snapshot_frame =
            AttachStreamFrame::from_json_line(read_json_line_bytewise(&mut client).trim_end())
                .unwrap();
        assert!(matches!(
            snapshot_frame,
            AttachStreamFrame::ScreenSnapshot { snapshot }
                if snapshot.source.pty_log_offset == 1
        ));
        let suffix_frame =
            AttachStreamFrame::from_json_line(read_json_line_bytewise(&mut client).trim_end())
                .unwrap();
        assert!(matches!(
            suffix_frame,
            AttachStreamFrame::RawOutput { data }
                if data.as_slice() == &"界".as_bytes()[..2]
        ));

        runtime
            .terminal_state
            .lock()
            .unwrap()
            .process_output(&"界".as_bytes()[2..]);
        WorkerControlHandle {
            state: Arc::clone(&runtime.state),
            paths: runtime.paths.clone(),
        }
        .broadcast_output(&"界".as_bytes()[2..], 3, 4);
        let live_frame =
            AttachStreamFrame::from_json_line(read_json_line_bytewise(&mut client).trim_end())
                .unwrap();
        assert!(matches!(
            live_frame,
            AttachStreamFrame::RawOutput { data }
                if data.as_slice() == &"界".as_bytes()[2..]
        ));

        client
            .write_all(AttachStreamFrame::Close.to_json_line().unwrap().as_bytes())
            .unwrap();
        handle.join().unwrap().unwrap();
    }

    #[test]
    fn worker_screen_snapshot_initial_replay_reports_size_mismatch() {
        let terminal_state = TerminalStateBuffer::new(3, 10, 128, 0);

        let frame = screen_snapshot_frame(
            terminal_state.screen_snapshot(),
            Some(TerminalDimensions { rows: 4, cols: 10 }),
        );

        assert!(matches!(
            &frame,
            AttachStreamFrame::SnapshotUnavailable {
                reason: SnapshotUnavailableReason::SizeMismatch,
                details: Some(_),
            }
        ));
    }

    #[test]
    fn worker_screen_snapshot_initial_replay_reports_payload_too_large() {
        let terminal_state = TerminalStateBuffer::new(1, 1, 128, 0);
        let mut snapshot = terminal_state.screen_snapshot();
        snapshot.cells[0][0].symbol = "x".repeat(5 * 1024 * 1024);

        let frame = screen_snapshot_frame(snapshot, Some(TerminalDimensions { rows: 1, cols: 1 }));

        assert!(matches!(
            &frame,
            AttachStreamFrame::SnapshotUnavailable {
                reason: SnapshotUnavailableReason::PayloadTooLarge,
                details: Some(_),
            }
        ));
    }

    #[test]
    fn worker_raw_input_requires_negotiated_writable_owner_stream() {
        let mut state = ControlState::default();
        let mut request = raw_worker_attach_request("raw-owner", false);

        request.client_protocol_version = None;
        assert_eq!(
            validate_raw_input_allowed(&state, &request)
                .unwrap_err()
                .code,
            ControlErrorCode::InvalidRequest
        );

        request = raw_worker_attach_request("raw-owner", false);
        request.stream_encoding = Some(AttachStreamEncoding::Text);
        assert_eq!(
            validate_raw_input_allowed(&state, &request)
                .unwrap_err()
                .code,
            ControlErrorCode::InvalidRequest
        );

        request = raw_worker_attach_request("raw-owner", true);
        assert_eq!(
            validate_raw_input_allowed(&state, &request)
                .unwrap_err()
                .code,
            ControlErrorCode::InvalidRequest
        );

        request = raw_worker_attach_request("raw-owner", false);
        request.accepted_frame_types = vec![AttachFrameType::RawOutput];
        assert_eq!(
            validate_raw_input_allowed(&state, &request)
                .unwrap_err()
                .code,
            ControlErrorCode::InvalidRequest
        );

        state.acquire_attach("other-owner", false).unwrap();
        request = raw_worker_attach_request("raw-owner", false);
        assert_eq!(
            validate_raw_input_allowed(&state, &request)
                .unwrap_err()
                .code,
            ControlErrorCode::InputOwnerConflict
        );

        state.release_attach("other-owner");
        state.acquire_attach("raw-owner", false).unwrap();
        validate_raw_input_allowed(&state, &request).unwrap();
    }

    #[test]
    fn raw_input_queue_delivers_more_than_one_kibibyte_under_slow_progress() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let writer = Arc::new(Mutex::new(stream));
        let (sender, receiver) = mpsc::sync_channel(1);
        let closed = AtomicBool::new(false);
        let payload = (0..4096)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let expected = payload.clone();
        let consumer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(40));
            let mut received = Vec::new();
            for command in receiver {
                if let AttachInputCommand::Input(bytes) = command {
                    received.extend(bytes);
                }
            }
            received
        });

        for chunk in payload.chunks(ATTACH_INPUT_MAX_BYTES) {
            assert!(!queue_attach_input_bytes(
                &writer,
                &sender,
                &closed,
                chunk.to_vec()
            ));
        }
        drop(sender);

        assert_eq!(consumer.join().unwrap(), expected);
        assert!(!closed.load(Ordering::SeqCst));
    }

    #[test]
    fn cancelled_writer_waiting_for_pty_mutex_allows_replacement_owner() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = attach_test_runtime(&temp, 24, 80);
        runtime
            .state
            .lock()
            .unwrap()
            .acquire_attach("blocked", false)
            .unwrap();
        let pty_writer = runtime.writer.lock().unwrap();
        let closed = Arc::new(AtomicBool::new(false));
        let write_runtime = Arc::clone(&runtime);
        let write_closed = Arc::clone(&closed);
        let (done_sender, done_receiver) = mpsc::channel();
        let writer = thread::spawn(move || {
            let result = write_pty_bytes_cancellable(&write_runtime, b"blocked", &write_closed);
            done_sender.send(result).unwrap();
        });

        runtime
            .input_write_sync
            .wait_for("mutex contention", |state| state.waiting);
        closed.store(true, Ordering::SeqCst);
        assert_eq!(
            done_receiver
                .recv_timeout(Duration::from_millis(250))
                .expect("cancelled mutex waiter exits promptly")
                .unwrap(),
            0
        );
        writer.join().unwrap();
        runtime
            .input_write_sync
            .wait_for("completion", |state| state.completed);
        drop(pty_writer);

        let mut state = runtime.state.lock().unwrap();
        state.release_attach("blocked");
        assert!(
            state
                .acquire_attach("replacement", false)
                .unwrap()
                .input_owner
        );
    }

    #[test]
    fn initial_replay_write_failure_joins_input_before_authority_release() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = attach_test_runtime(&temp, 100, 100);
        let (mut client, server) = UnixStream::pair().unwrap();
        set_socket_send_buffer(&server, 1024);
        let mut attach_request = raw_worker_attach_request("replay-failure", false);
        attach_request.requested_terminal_size = Some(TerminalDimensions {
            rows: 100,
            cols: 100,
        });
        attach_request.initial_replay = Some(AttachInitialReplay::ScreenSnapshot);
        attach_request.accepted_frame_types.extend([
            AttachFrameType::ScreenSnapshot,
            AttachFrameType::SnapshotUnavailable,
        ]);
        let request = WorkerControlRequest::with_params(
            "observe-replay-failure",
            WorkerControlMethod::ObserveAttach,
            &attach_request,
        )
        .unwrap();
        client
            .write_all(request.to_json_line().unwrap().as_bytes())
            .unwrap();
        let handler_runtime = Arc::clone(&runtime);
        let handle = thread::spawn(move || handle_connection(server, handler_runtime));

        let response = read_json_line_bytewise(&mut client);
        assert!(
            WorkerControlResponse::from_json_line(response.trim_end())
                .unwrap()
                .ok
        );
        let pty_writer = runtime.writer.lock().unwrap();
        client
            .write_all(
                AttachStreamFrame::raw_input(b"queued".to_vec())
                    .to_json_line()
                    .unwrap()
                    .as_bytes(),
            )
            .unwrap();
        runtime
            .input_write_sync
            .wait_for("replay-failure mutex contention", |state| state.waiting);
        client.shutdown(Shutdown::Read).unwrap();

        assert!(handle.join().unwrap().is_err());
        runtime
            .input_write_sync
            .wait_for("replay-failure completion", |state| state.completed);
        assert!(runtime.state.lock().unwrap().input_owner.is_none());
        assert!(
            runtime
                .state
                .lock()
                .unwrap()
                .acquire_attach("replacement", false)
                .unwrap()
                .input_owner
        );
        drop(pty_writer);
    }

    #[test]
    fn live_output_write_failure_joins_input_before_authority_release() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = attach_test_runtime(&temp, 24, 80);
        let (mut client, server) = UnixStream::pair().unwrap();
        let attach_request = raw_worker_attach_request("output-failure", false);
        let request = WorkerControlRequest::with_params(
            "observe-output-failure",
            WorkerControlMethod::ObserveAttach,
            &attach_request,
        )
        .unwrap();
        client
            .write_all(request.to_json_line().unwrap().as_bytes())
            .unwrap();
        let handler_runtime = Arc::clone(&runtime);
        let handle = thread::spawn(move || handle_connection(server, handler_runtime));

        let response = read_json_line_bytewise(&mut client);
        assert!(
            WorkerControlResponse::from_json_line(response.trim_end())
                .unwrap()
                .ok
        );
        let pty_writer = runtime.writer.lock().unwrap();
        client
            .write_all(
                AttachStreamFrame::raw_input(b"queued".to_vec())
                    .to_json_line()
                    .unwrap()
                    .as_bytes(),
            )
            .unwrap();
        runtime
            .input_write_sync
            .wait_for("output-failure mutex contention", |state| state.waiting);
        client.shutdown(Shutdown::Read).unwrap();
        WorkerControlHandle {
            state: Arc::clone(&runtime.state),
            paths: runtime.paths.clone(),
        }
        .broadcast_output(b"unread", 0, 6);

        assert!(handle.join().unwrap().is_err());
        runtime
            .input_write_sync
            .wait_for("output-failure completion", |state| state.completed);
        assert!(runtime.state.lock().unwrap().input_owner.is_none());
        assert!(
            runtime
                .state
                .lock()
                .unwrap()
                .acquire_attach("replacement", false)
                .unwrap()
                .input_owner
        );
        drop(pty_writer);
    }

    #[test]
    fn pty_nonblocking_guard_restores_master_flags() {
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let fd = pair.master.as_raw_fd().expect("native PTY master fd");
        // SAFETY: fd is owned by pair.master for the duration of the test.
        let original = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(original >= 0);

        {
            let _guard = PtyNonblockingGuard::activate(fd).unwrap();
            // SAFETY: fd remains owned by pair.master.
            let active = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            assert_ne!(active & libc::O_NONBLOCK, 0);
        }

        // SAFETY: fd remains owned by pair.master.
        let restored = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert_eq!(restored, original);
    }

    #[test]
    fn cancelled_stalled_writer_skips_queued_resize_before_replacement_owner() {
        let temp = tempfile::tempdir().unwrap();
        let gate = temp.path().join("resume");
        let output = temp.path().join("input.bin");
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new("sh");
        command.args([
            "-c",
            "stty raw -echo; while [ ! -e \"$GATE\" ]; do sleep 0.01; done; cat > \"$OUTPUT\"",
        ]);
        command.env("GATE", gate.as_os_str());
        command.env("OUTPUT", output.as_os_str());
        let mut child = pair.slave.spawn_command(command).unwrap();

        let state_paths = StatePaths::new(temp.path().join("state"));
        let session_paths = state_paths.session_paths(SessionId::new());
        fs::create_dir_all(&session_paths.root).unwrap();
        let runtime = Arc::new(ControlRuntime {
            paths: session_paths,
            writer: Arc::new(Mutex::new(Box::new(Vec::new()))),
            master: Arc::new(Mutex::new(pair.master)),
            terminal_state: Arc::new(Mutex::new(TerminalStateBuffer::new(24, 80, 128, 0))),
            child_pid: None,
            child_pgid: None,
            state: Arc::new(Mutex::new(ControlState::default())),
            input_write_sync: Arc::new(InputWriteTestSync::default()),
        });
        runtime
            .state
            .lock()
            .unwrap()
            .acquire_attach("stalled", false)
            .unwrap();

        let closed = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::channel();
        for _ in 0..512 {
            sender
                .send(AttachInputCommand::Input(vec![
                    b'x';
                    ATTACH_INPUT_MAX_BYTES
                ]))
                .unwrap();
        }
        sender
            .send(AttachInputCommand::Resize { rows: 51, cols: 91 })
            .unwrap();
        let writer_runtime = Arc::clone(&runtime);
        let writer_closed = Arc::clone(&closed);
        let (done_sender, done_receiver) = mpsc::channel();
        let writer = thread::spawn(move || {
            write_attach_input_loop(
                writer_runtime,
                Arc::new(Mutex::new(UnixStream::pair().unwrap().0)),
                receiver,
                writer_closed,
            );
            done_sender.send(()).unwrap();
        });

        runtime
            .input_write_sync
            .wait_for("active PTY write", |state| state.active);
        closed.store(true, Ordering::SeqCst);
        drop(sender);
        done_receiver
            .recv_timeout(ATTACH_INPUT_DRAIN_TIMEOUT)
            .expect("cancelled PTY writer exits within its drain bound");
        writer.join().unwrap();
        runtime
            .input_write_sync
            .wait_for("stalled-writer completion", |state| state.completed);
        assert_eq!(runtime.master.lock().unwrap().get_size().unwrap().rows, 24);
        assert_eq!(runtime.master.lock().unwrap().get_size().unwrap().cols, 80);

        {
            let mut state = runtime.state.lock().unwrap();
            state.release_attach("stalled");
            assert!(
                state
                    .acquire_attach("replacement", false)
                    .unwrap()
                    .input_owner
            );
        }
        fs::write(&gate, b"resume").unwrap();
        let replacement_closed = Arc::new(AtomicBool::new(false));
        let (replacement_sender, replacement_receiver) = mpsc::channel();
        replacement_sender
            .send(AttachInputCommand::Input(vec![b'R']))
            .unwrap();
        drop(replacement_sender);
        write_attach_input_loop(
            Arc::clone(&runtime),
            Arc::new(Mutex::new(UnixStream::pair().unwrap().0)),
            replacement_receiver,
            replacement_closed,
        );

        let deadline = Instant::now() + ATTACH_INPUT_DRAIN_TIMEOUT;
        loop {
            if fs::read(&output).is_ok_and(|bytes| bytes.contains(&b'R')) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "replacement byte was not delivered"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    fn raw_worker_attach_request(stream_id: &str, read_only: bool) -> WorkerAttachRequest {
        WorkerAttachRequest {
            stream_id: stream_id.to_string(),
            read_only,
            replay: AttachReplayMode::None,
            requested_terminal_size: None,
            client_protocol_version: Some(
                millrace_sessions_core::protocol::M2_ATTACH_PROTOCOL_VERSION,
            ),
            accepted_frame_types: vec![AttachFrameType::RawOutput, AttachFrameType::RawInput],
            stream_encoding: Some(AttachStreamEncoding::RawBytes),
            initial_replay: Some(AttachInitialReplay::None),
        }
    }

    fn attach_test_runtime(temp: &tempfile::TempDir, rows: u16, cols: u16) -> Arc<ControlRuntime> {
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
                spawn_mode: SpawnMode::Pty,
                process_state: ProcessState::Running,
                started_at: "2026-07-08T00:00:00Z".to_string(),
                ended_at: None,
                stop_requested_at: None,
                stop_reason: None,
                exit_code: None,
                exit_signal: None,
                attached_clients: 0,
                input_owner: None,
                updated_at: "2026-07-08T00:00:00Z".to_string(),
            },
        )
        .unwrap();
        Arc::new(ControlRuntime {
            paths: session_paths,
            writer: Arc::new(Mutex::new(Box::new(Vec::new()))),
            master: Arc::new(Mutex::new(
                native_pty_system()
                    .openpty(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .unwrap()
                    .master,
            )),
            terminal_state: Arc::new(Mutex::new(TerminalStateBuffer::new(rows, cols, 128, 0))),
            child_pid: None,
            child_pgid: None,
            state: Arc::new(Mutex::new(ControlState::default())),
            input_write_sync: Arc::new(InputWriteTestSync::default()),
        })
    }

    fn read_json_line_bytewise(stream: &mut UnixStream) -> String {
        let mut bytes = Vec::new();
        let mut byte = [0_u8; 1];
        loop {
            stream.read_exact(&mut byte).unwrap();
            bytes.push(byte[0]);
            if byte[0] == b'\n' {
                return String::from_utf8(bytes).unwrap();
            }
        }
    }

    fn set_socket_send_buffer(stream: &UnixStream, bytes: libc::c_int) {
        let value = bytes;
        // SAFETY: value points to one initialized integer for this socket option call.
        let result = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&value as *const libc::c_int).cast::<libc::c_void>(),
                std::mem::size_of_val(&value) as libc::socklen_t,
            )
        };
        assert_eq!(result, 0, "failed to constrain worker socket send buffer");
    }
}
