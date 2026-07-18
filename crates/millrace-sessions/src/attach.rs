use std::{
    any::Any,
    fs::{File, OpenOptions},
    io::{self, IsTerminal, Read, Write},
    os::fd::{AsFd, AsRawFd, RawFd},
    os::unix::fs::OpenOptionsExt,
    sync::{Arc, Mutex as SyncMutex},
    time::{Duration, Instant},
};

use crossterm::terminal;
use millrace_sessions_core::protocol::{
    AttachFrameType, AttachStreamFrame, SessionAttachRequest, SessionAttachResponse,
    SnapshotUnavailableReason, TerminalDimensions,
};
#[cfg(debug_assertions)]
use nix::unistd::{close, dup, dup2};
use nix::{
    fcntl::OFlag,
    sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg, Termios},
};
use thiserror::Error;
use tokio::{
    io::unix::AsyncFd,
    signal::unix::{Signal, SignalKind},
    sync::{oneshot, Mutex as AsyncMutex},
};

use crate::client::{
    AttachConnection, AttachReader, AttachWriter, ClientError, SessionControlClient,
};

const MANAGED_RAW_DETACH_PREFIX: u8 = 0x1d;
const MANAGED_RAW_DETACH_KEY: u8 = b'd';
const ATTACH_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const ATTACH_STREAM_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn record_managed_raw_test_phase(phase: &str) {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("MILLMUX_TEST_MANAGED_RAW_PHASE_FILE") {
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{phase}");
        }
    }
    let _ = phase;
}

pub async fn run_attach(
    client: &SessionControlClient,
    request: &SessionAttachRequest,
) -> Result<(), AttachError> {
    let request = request_with_terminal_size(request);
    let connection = client.attach(&request).await?;
    let _ = run_attach_connection(connection, &request).await?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManagedRawAttachExit {
    LocalDetach,
    RemoteClosed,
    InputEof,
}

pub(crate) enum ManagedRawAttachEvent<'a> {
    Output { bytes: &'a [u8] },
}

pub(crate) async fn prepare_managed_raw_attach(
    connection: AttachConnection,
    request: &SessionAttachRequest,
) -> Result<ManagedRawAttachOperation, AttachError> {
    let (result, mut reader, mut writer) = connection.split();
    validate_attach_negotiation(request, &result)?;
    if !result.stream.input_owner {
        return Err(AttachError::Compatibility(
            "managed raw attach requires exclusive input ownership".to_string(),
        ));
    }
    let managed_output = match ManagedStdout::open() {
        Ok(output) => Arc::new(output),
        Err(error) => {
            let _ = close_attach_stream_without_output(&mut reader, &mut writer).await;
            return Err(error);
        }
    };
    #[cfg(debug_assertions)]
    let _stdin_redirect = if std::env::var_os("MILLMUX_TEST_MANAGED_RAW_REDIRECT_STDIN").is_some() {
        match TestStdinRedirectGuard::activate() {
            Ok(guard) => Some(guard),
            Err(error) => {
                let _ = close_attach_stream_without_output(&mut reader, &mut writer).await;
                return Err(error.into());
            }
        }
    } else {
        None
    };
    let input = match AsyncTtyInput::open(true) {
        Ok(input) => input.map(Arc::new),
        Err(error) => {
            let _ = close_attach_stream_without_output(&mut reader, &mut writer).await;
            return Err(error);
        }
    };
    let terminal_guard = if let Some(input) = input.as_ref() {
        match TerminalModeGuard::activate(input) {
            Ok(guard) => guard,
            Err(error) => {
                let _ = close_attach_stream_without_output(&mut reader, &mut writer).await;
                return Err(error);
            }
        }
    } else {
        TerminalModeGuard::inactive()
    };
    let (terminal_cleanup_sender, terminal_cleanup_receiver) = oneshot::channel();
    let terminal_restoration = Arc::new(ManagedTerminalRestoration {
        guard: SyncMutex::new(Some(terminal_guard)),
        completion: SyncMutex::new(Some(terminal_cleanup_sender)),
    });
    let resize_signal = Some(tokio::signal::unix::signal(SignalKind::window_change())?);
    let reader = Arc::new(AsyncMutex::new(reader));
    let writer = Arc::new(AsyncMutex::new(writer));
    let (cancel_sender, cancel_receiver) = oneshot::channel();
    let supervisor = ManagedAttachSupervisor {
        reader,
        writer,
        input: input.clone(),
        resize_signal,
        managed_output,
    };
    let control = Arc::new(ManagedAttachControl {
        cancel_sender: SyncMutex::new(Some(cancel_sender)),
        supervisor: AsyncMutex::new(Some(tokio::spawn(supervisor.run(cancel_receiver)))),
    });
    Ok(ManagedRawAttachOperation {
        control,
        terminal_restoration,
        terminal_cleanup_receiver: Some(terminal_cleanup_receiver),
        #[cfg(debug_assertions)]
        _stdin_redirect,
    })
}

pub(crate) struct ManagedRawAttachOperation {
    control: Arc<ManagedAttachControl>,
    terminal_restoration: Arc<ManagedTerminalRestoration>,
    terminal_cleanup_receiver: Option<oneshot::Receiver<()>>,
    #[cfg(debug_assertions)]
    _stdin_redirect: Option<TestStdinRedirectGuard>,
}

impl ManagedRawAttachOperation {
    pub(crate) fn waiter(&self) -> ManagedRawAttachWaiter {
        ManagedRawAttachWaiter {
            control: Arc::clone(&self.control),
            _terminal_restoration: Arc::clone(&self.terminal_restoration),
        }
    }

    pub(crate) fn take_terminal_cleanup_receiver(&mut self) -> oneshot::Receiver<()> {
        self.terminal_cleanup_receiver
            .take()
            .expect("managed terminal cleanup receiver already taken")
    }

    pub(crate) async fn cancel_and_join(&mut self) -> Result<ManagedRawAttachExit, AttachError> {
        self.control
            .cancel(ManagedSupervisorCancel::CallerCancelled);
        self.control.join().await
    }
}

impl Drop for ManagedRawAttachOperation {
    fn drop(&mut self) {
        self.control
            .cancel(ManagedSupervisorCancel::CallerCancelled);
    }
}

pub(crate) struct ManagedRawAttachWaiter {
    control: Arc<ManagedAttachControl>,
    _terminal_restoration: Arc<ManagedTerminalRestoration>,
}

struct ManagedTerminalRestoration {
    guard: SyncMutex<Option<TerminalModeGuard>>,
    completion: SyncMutex<Option<oneshot::Sender<()>>>,
}

impl Drop for ManagedTerminalRestoration {
    fn drop(&mut self) {
        let guard = self
            .guard
            .lock()
            .expect("managed terminal restoration lock poisoned")
            .take();
        drop(guard);
        if let Some(sender) = self
            .completion
            .lock()
            .expect("managed terminal completion lock poisoned")
            .take()
        {
            let _ = sender.send(());
        }
    }
}

impl ManagedRawAttachWaiter {
    pub(crate) async fn run(
        self,
        interrupt: &mut Signal,
    ) -> Result<ManagedRawAttachExit, AttachError> {
        let mut cancellation = ManagedCallerCancellation::new(Arc::clone(&self.control));

        let result = if let Some(delay) = managed_test_cancellation_delay() {
            tokio::select! {
                result = self.control.join() => result,
                () = tokio::time::sleep(delay) => {
                    self.control.cancel(ManagedSupervisorCancel::CallerCancelled);
                    self.control.join().await
                }
                signal = interrupt.recv() => {
                    let reason = if signal.is_some() {
                        ManagedSupervisorCancel::LocalDetach
                    } else {
                        ManagedSupervisorCancel::InterruptClosed
                    };
                    self.control.cancel(reason);
                    self.control.join().await
                }
            }
        } else {
            tokio::select! {
                result = self.control.join() => result,
                signal = interrupt.recv() => {
                    let reason = if signal.is_some() {
                        ManagedSupervisorCancel::LocalDetach
                    } else {
                        ManagedSupervisorCancel::InterruptClosed
                    };
                    self.control.cancel(reason);
                    self.control.join().await
                }
            }
        };
        cancellation.disarm();
        result
    }
}

#[derive(Debug, Clone, Copy)]
enum ManagedSupervisorCancel {
    CallerCancelled,
    LocalDetach,
    InterruptClosed,
}

struct ManagedCallerCancellation {
    control: Arc<ManagedAttachControl>,
    armed: bool,
}

impl ManagedCallerCancellation {
    fn new(control: Arc<ManagedAttachControl>) -> Self {
        Self {
            control,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ManagedCallerCancellation {
    fn drop(&mut self) {
        if self.armed {
            self.control
                .cancel(ManagedSupervisorCancel::CallerCancelled);
        }
    }
}

struct ManagedAttachControl {
    cancel_sender: SyncMutex<Option<oneshot::Sender<ManagedSupervisorCancel>>>,
    supervisor:
        AsyncMutex<Option<tokio::task::JoinHandle<Result<ManagedRawAttachExit, AttachError>>>>,
}

impl ManagedAttachControl {
    fn cancel(&self, reason: ManagedSupervisorCancel) {
        if let Some(sender) = self
            .cancel_sender
            .lock()
            .expect("managed raw cancellation lock poisoned")
            .take()
        {
            let _ = sender.send(reason);
        }
    }

    async fn join(&self) -> Result<ManagedRawAttachExit, AttachError> {
        let mut supervisor = self.supervisor.lock().await;
        let Some(handle) = supervisor.as_mut() else {
            return Err(AttachError::Stream(
                "managed raw supervisor was already joined".to_string(),
            ));
        };
        let joined = handle.await;
        *supervisor = None;
        managed_supervisor_join_result(joined)
    }
}

struct ManagedAttachSupervisor {
    reader: Arc<AsyncMutex<AttachReader>>,
    writer: Arc<AsyncMutex<AttachWriter>>,
    input: Option<Arc<AsyncTtyInput>>,
    resize_signal: Option<Signal>,
    managed_output: Arc<ManagedStdout>,
}

impl ManagedAttachSupervisor {
    async fn run(
        self,
        cancel_receiver: oneshot::Receiver<ManagedSupervisorCancel>,
    ) -> Result<ManagedRawAttachExit, AttachError> {
        let Self {
            reader,
            writer,
            input,
            resize_signal,
            managed_output,
        } = self;
        let mut body = tokio::spawn(run_managed_attach_event_loop(
            Arc::clone(&reader),
            Arc::clone(&writer),
            input.clone(),
            resize_signal,
            managed_output,
        ));

        let outcome = tokio::select! {
            joined = &mut body => managed_body_join_result(joined),
            reason = cancel_receiver => {
                body.abort();
                if matches!(body.await, Err(error) if error.is_cancelled()) {
                    record_managed_raw_test_phase("raw_inner_task_aborted");
                }
                match reason.unwrap_or(ManagedSupervisorCancel::CallerCancelled) {
                    ManagedSupervisorCancel::LocalDetach => Ok(ManagedRawAttachExit::LocalDetach),
                    ManagedSupervisorCancel::CallerCancelled => Err(AttachError::Cancelled),
                    ManagedSupervisorCancel::InterruptClosed => {
                        Err(AttachError::Stream("SIGINT listener closed".to_string()))
                    }
                }
            }
        };

        let mut reader_guard = reader.lock().await;
        let mut writer_guard = writer.lock().await;
        let result = match outcome {
            Ok(exit @ (ManagedRawAttachExit::LocalDetach | ManagedRawAttachExit::InputEof)) => {
                close_attach_stream_without_output(&mut reader_guard, &mut writer_guard).await?;
                Ok(exit)
            }
            Ok(ManagedRawAttachExit::RemoteClosed) => Ok(ManagedRawAttachExit::RemoteClosed),
            Err(error @ AttachError::SnapshotUnavailable(_)) => {
                close_attach_stream_without_output(&mut reader_guard, &mut writer_guard).await?;
                Err(error)
            }
            Err(error) => {
                let _ =
                    close_attach_stream_without_output(&mut reader_guard, &mut writer_guard).await;
                Err(error)
            }
        };
        drop(writer_guard);
        drop(reader_guard);
        record_managed_raw_test_phase("raw_cleanup_complete");
        result
    }
}

fn managed_supervisor_join_result(
    joined: Result<Result<ManagedRawAttachExit, AttachError>, tokio::task::JoinError>,
) -> Result<ManagedRawAttachExit, AttachError> {
    match joined {
        Ok(result) => result,
        Err(error) if error.is_panic() => {
            Err(AttachError::Panic(panic_message(error.into_panic())))
        }
        Err(error) if error.is_cancelled() => Err(AttachError::Cancelled),
        Err(error) => Err(AttachError::Stream(format!(
            "managed raw supervisor failed: {error}"
        ))),
    }
}

fn managed_body_join_result(
    joined: Result<Result<ManagedRawAttachExit, AttachError>, tokio::task::JoinError>,
) -> Result<ManagedRawAttachExit, AttachError> {
    match joined {
        Ok(result) => result,
        Err(error) if error.is_panic() => {
            record_managed_raw_test_phase("raw_inner_task_panicked");
            Err(AttachError::Panic(panic_message(error.into_panic())))
        }
        Err(error) if error.is_cancelled() => Err(AttachError::Cancelled),
        Err(error) => Err(AttachError::Stream(format!(
            "managed raw task failed: {error}"
        ))),
    }
}

async fn run_managed_attach_event_loop(
    reader: Arc<AsyncMutex<AttachReader>>,
    writer: Arc<AsyncMutex<AttachWriter>>,
    input: Option<Arc<AsyncTtyInput>>,
    mut resize_signal: Option<Signal>,
    managed_output: Arc<ManagedStdout>,
) -> Result<ManagedRawAttachExit, AttachError> {
    let test_fault = managed_test_fault();
    let mut detach_scanner = DetachScanner::default();
    record_managed_raw_test_phase("raw_loop_entered");
    loop {
        tokio::select! {
            frame = async {
                let mut reader = reader.lock().await;
                reader.next_frame().await
            } => {
                let frame = frame?;
                if !raw_attach_frame_is_compatible(frame.as_ref()) {
                    return Err(AttachError::Compatibility(format!(
                        "raw attach received incompatible frame: {frame:?}"
                    )));
                }
                if let Some(error) = raw_attach_recovery_error(frame.as_ref()) {
                    return Err(error);
                }
                match frame {
                    Some(AttachStreamFrame::RawOutput { data }) => {
                        if let Some(exit) = apply_managed_test_frame_fault(test_fault.as_deref())? {
                            return Ok(exit);
                        }
                        managed_output.write_all(data.as_slice()).await?;
                    }
                    Some(AttachStreamFrame::Error { error }) => {
                        return Err(AttachError::Stream(error.message));
                    }
                    Some(AttachStreamFrame::Closed) | None => {
                        return Ok(ManagedRawAttachExit::RemoteClosed);
                    }
                    Some(frame) => {
                        return Err(AttachError::Compatibility(format!(
                            "raw attach received incompatible frame: {frame:?}"
                        )));
                    }
                }
            }
            read = read_managed_attach_input(input.as_deref()), if input.is_some() => {
                let bytes = read?;
                if bytes.is_empty() {
                    if let Some(prefix) = detach_scanner.finish() {
                        writer.lock().await
                            .write_frame(&AttachStreamFrame::raw_input(prefix)).await?;
                    }
                    return Ok(ManagedRawAttachExit::InputEof);
                }

                let (bytes, detached) = detach_scanner.scan(&bytes);
                if !bytes.is_empty() {
                    if test_fault.as_deref() == Some("write_error") {
                        return Err(AttachError::Io(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "injected managed raw write failure",
                        )));
                    }
                    writer.lock().await
                        .write_frame(&AttachStreamFrame::raw_input(bytes)).await?;
                }
                if detached {
                    return Ok(ManagedRawAttachExit::LocalDetach);
                }
            }
            resize = next_resize(&mut resize_signal), if resize_signal.is_some() => {
                match resize {
                    Some(size) => {
                        if test_fault.as_deref() == Some("resize_error") {
                            return Err(AttachError::Io(io::Error::other(
                                "injected managed raw resize failure",
                            )));
                        }
                        writer.lock().await.write_frame(&AttachStreamFrame::Resize {
                            rows: size.rows,
                            cols: size.cols,
                        }).await?;
                    }
                    None => resize_signal = None,
                }
            }
        }
    }
}

async fn read_managed_attach_input(input: Option<&AsyncTtyInput>) -> io::Result<Vec<u8>> {
    input
        .expect("enabled managed attach input must exist")
        .read(true)
        .await
}

async fn run_attach_connection(
    connection: AttachConnection,
    request: &SessionAttachRequest,
) -> Result<ManagedRawAttachExit, AttachError> {
    let raw_requested = request.requests_raw_stream();
    let (result, mut reader, mut writer) = connection.split();
    validate_attach_negotiation(request, &result)?;
    let input = if result.stream.input_owner {
        match AsyncTtyInput::open(false) {
            Ok(input) => input,
            Err(error) => {
                let _ = close_attach_stream_without_output(&mut reader, &mut writer).await;
                return Err(error);
            }
        }
    } else {
        None
    };
    let _guard = if let Some(input) = input.as_ref() {
        match TerminalModeGuard::activate(input) {
            Ok(guard) => guard,
            Err(error) => {
                let _ = close_attach_stream_without_output(&mut reader, &mut writer).await;
                return Err(error);
            }
        }
    } else {
        TerminalModeGuard::inactive()
    };
    let mut resize_signal = if raw_requested {
        Some(tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::window_change(),
        )?)
    } else {
        None
    };
    let mut interrupt = tokio::signal::unix::signal(SignalKind::interrupt())?;
    let outcome = run_attach_event_loop(
        &mut reader,
        &mut writer,
        &input,
        &mut resize_signal,
        &mut interrupt,
        raw_requested,
    )
    .await;

    let mut ignore_output: for<'a> fn(ManagedRawAttachEvent<'a>) -> Result<(), AttachError> =
        ignore_managed_raw_output;
    let result = match outcome {
        Ok(exit @ (ManagedRawAttachExit::LocalDetach | ManagedRawAttachExit::InputEof)) => {
            close_attach_stream(&mut reader, &mut writer, &mut ignore_output).await?;
            Ok(exit)
        }
        Ok(ManagedRawAttachExit::RemoteClosed) => Ok(ManagedRawAttachExit::RemoteClosed),
        Err(error @ AttachError::SnapshotUnavailable(_)) => {
            close_attach_stream_without_output(&mut reader, &mut writer).await?;
            Err(error)
        }
        Err(error) => {
            let _ =
                close_attach_stream_after_error(&mut reader, &mut writer, &mut ignore_output).await;
            Err(error)
        }
    };
    result
}

fn ignore_managed_raw_output(_: ManagedRawAttachEvent<'_>) -> Result<(), AttachError> {
    Ok(())
}

async fn run_attach_event_loop(
    reader: &mut AttachReader,
    writer: &mut AttachWriter,
    input: &Option<AsyncTtyInput>,
    resize_signal: &mut Option<Signal>,
    interrupt: &mut Signal,
    raw_requested: bool,
) -> Result<ManagedRawAttachExit, AttachError> {
    loop {
        tokio::select! {
            frame = reader.next_frame() => {
                let frame = frame?;
                if raw_requested {
                    if !raw_attach_frame_is_compatible(frame.as_ref()) {
                        return Err(AttachError::Compatibility(format!(
                            "raw attach received incompatible frame: {frame:?}"
                        )));
                    }
                    if let Some(error) = raw_attach_recovery_error(frame.as_ref()) {
                        return Err(error);
                    }
                }
                match frame {
                    Some(AttachStreamFrame::Scrollback { lines }) if !raw_requested => write_scrollback(&lines)?,
                    Some(AttachStreamFrame::Output { text, .. }) if !raw_requested => write_stdout(text.as_bytes())?,
                    Some(AttachStreamFrame::RawOutput { data }) => {
                        write_stdout(data.as_slice())?;
                    }
                    Some(AttachStreamFrame::Error { error }) => return Err(AttachError::Stream(error.message)),
                    Some(AttachStreamFrame::Closed) | None => {
                        return Ok(ManagedRawAttachExit::RemoteClosed);
                    }
                    Some(frame) if raw_requested => {
                        return Err(AttachError::Compatibility(format!(
                            "raw attach received incompatible frame: {frame:?}"
                        )));
                    }
                    Some(_) => {}
                }
            }
            read = read_attach_input(input, false), if input.is_some() => {
                let bytes = read?;
                if bytes.is_empty() {
                    return Ok(ManagedRawAttachExit::InputEof);
                }

                if !bytes.is_empty() {
                    let frame = if raw_requested {
                        AttachStreamFrame::raw_input(bytes)
                    } else {
                        AttachStreamFrame::Input {
                            text: String::from_utf8_lossy(&bytes).to_string(),
                        }
                    };
                    writer.write_frame(&frame).await?;
                }
            }
            resize = next_resize(resize_signal), if resize_signal.is_some() => {
                match resize {
                    Some(size) => {
                        writer.write_frame(&AttachStreamFrame::Resize {
                            rows: size.rows,
                            cols: size.cols,
                        }).await?;
                    }
                    None => *resize_signal = None,
                }
            }
            signal = interrupt.recv() => {
                if signal.is_none() {
                    return Err(AttachError::Stream("SIGINT listener closed".to_string()));
                }
                return Ok(ManagedRawAttachExit::LocalDetach);
            }
        }
    }
}

fn panic_message(payload: Box<dyn Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_string())
}

fn raw_attach_recovery_error(frame: Option<&AttachStreamFrame>) -> Option<AttachError> {
    match frame {
        Some(AttachStreamFrame::StreamLagged {
            reason, recover, ..
        }) => Some(AttachError::Stream(format!(
            "raw attach stream lagged ({reason:?}); recovery required: {recover}"
        ))),
        Some(AttachStreamFrame::SnapshotUnavailable { reason, .. }) => {
            Some(AttachError::SnapshotUnavailable(*reason))
        }
        _ => None,
    }
}

fn raw_attach_frame_is_compatible(frame: Option<&AttachStreamFrame>) -> bool {
    matches!(
        frame,
        None | Some(
            AttachStreamFrame::RawOutput { .. }
                | AttachStreamFrame::StreamLagged { .. }
                | AttachStreamFrame::SnapshotUnavailable { .. }
                | AttachStreamFrame::Error { .. }
                | AttachStreamFrame::Closed
        )
    )
}

fn managed_test_cancellation_delay() -> Option<Duration> {
    #[cfg(debug_assertions)]
    if let Ok(value) = std::env::var("MILLMUX_TEST_MANAGED_RAW_CANCEL_MS") {
        return value.parse::<u64>().ok().map(Duration::from_millis);
    }
    None
}

fn managed_test_fault() -> Option<String> {
    #[cfg(debug_assertions)]
    return std::env::var("MILLMUX_TEST_MANAGED_RAW_FAULT").ok();
    #[cfg(not(debug_assertions))]
    None
}

fn apply_managed_test_frame_fault(
    fault: Option<&str>,
) -> Result<Option<ManagedRawAttachExit>, AttachError> {
    match fault {
        Some("remote_close") => Ok(Some(ManagedRawAttachExit::RemoteClosed)),
        Some("eof") => Ok(Some(ManagedRawAttachExit::InputEof)),
        Some("protocol_error") => Err(AttachError::Stream(
            "injected managed raw protocol failure".to_string(),
        )),
        Some("read_error") => Err(AttachError::Io(io::Error::other(
            "injected managed raw read failure",
        ))),
        Some("panic") => panic!("injected managed raw panic"),
        _ => Ok(None),
    }
}

fn validate_attach_negotiation(
    request: &SessionAttachRequest,
    response: &SessionAttachResponse,
) -> Result<(), AttachError> {
    if !request.requests_raw_stream() {
        return Ok(());
    }

    let mut required_frames = vec![AttachFrameType::RawOutput, AttachFrameType::StreamLagged];
    if !request.read_only {
        required_frames.push(AttachFrameType::RawInput);
    }
    if response.confirms_raw_stream()
        && response.confirms_raw_input()
        && required_frames
            .iter()
            .all(|frame_type| response.accepted_frame_types.contains(frame_type))
    {
        return Ok(());
    }

    Err(AttachError::Compatibility(format!(
        "raw attach requires host-confirmed v2 raw-byte negotiation; got attach_protocol={:?}, stream_encoding={:?}, accepted_frame_types={:?}, input_owner={}",
        response.negotiated_attach_protocol_version,
        response.negotiated_stream_encoding,
        response.accepted_frame_types,
        response.stream.input_owner
    )))
}

fn request_with_terminal_size(request: &SessionAttachRequest) -> SessionAttachRequest {
    let mut request = request.clone();
    if request.requests_raw_stream() && request.requested_terminal_size.is_none() {
        request.requested_terminal_size = current_terminal_dimensions();
    }
    request
}

fn current_terminal_dimensions() -> Option<TerminalDimensions> {
    terminal::size()
        .ok()
        .map(|(cols, rows)| TerminalDimensions::new(rows, cols))
}

struct AsyncTtyInput {
    file: AsyncFd<File>,
}

#[cfg(debug_assertions)]
struct TestStdinRedirectGuard {
    saved_fd: RawFd,
}

#[cfg(debug_assertions)]
impl TestStdinRedirectGuard {
    fn activate() -> io::Result<Self> {
        let null = File::open("/dev/null")?;
        let saved_fd = dup(0).map_err(errno_io)?;
        if let Err(error) = dup2(null.as_raw_fd(), 0) {
            let _ = close(saved_fd);
            return Err(errno_io(error));
        }
        Ok(Self { saved_fd })
    }
}

#[cfg(debug_assertions)]
impl Drop for TestStdinRedirectGuard {
    fn drop(&mut self) {
        let _ = dup2(self.saved_fd, 0);
        let _ = close(self.saved_fd);
    }
}

#[cfg(debug_assertions)]
fn errno_io(error: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

impl AsyncTtyInput {
    fn open(managed: bool) -> Result<Option<Self>, AttachError> {
        let Some(path) = attach_input_path(managed, io::stdin().is_terminal()) else {
            return Ok(None);
        };
        let mut options = OpenOptions::new();
        options.read(true).custom_flags(OFlag::O_NONBLOCK.bits());
        if managed {
            options.write(true);
        }
        let file = options.open(path)?;
        Ok(Some(Self {
            file: AsyncFd::new(file)?,
        }))
    }

    async fn read(&self, managed: bool) -> io::Result<Vec<u8>> {
        let mut bytes = vec![0; if managed { 1 } else { 512 }];
        loop {
            let mut ready = self.file.readable().await?;
            match ready.try_io(|file| file.get_ref().read(&mut bytes)) {
                Ok(Ok(count)) => {
                    bytes.truncate(count);
                    return Ok(bytes);
                }
                Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
                Ok(Err(error)) => return Err(error),
                Err(_) => continue,
            }
        }
    }
}

fn attach_input_path(managed: bool, stdin_is_terminal: bool) -> Option<&'static str> {
    if managed {
        Some("/dev/tty")
    } else {
        stdin_is_terminal.then_some("/dev/stdin")
    }
}

async fn read_attach_input(input: &Option<AsyncTtyInput>, managed: bool) -> io::Result<Vec<u8>> {
    input
        .as_ref()
        .expect("enabled attach input must exist")
        .read(managed)
        .await
}

async fn next_resize(signal: &mut Option<Signal>) -> Option<TerminalDimensions> {
    signal.as_mut()?.recv().await?;
    current_terminal_dimensions()
}

pub(crate) async fn close_attach_stream<F>(
    reader: &mut AttachReader,
    writer: &mut AttachWriter,
    on_raw_output: &mut F,
) -> Result<(), AttachError>
where
    F: for<'a> FnMut(ManagedRawAttachEvent<'a>) -> Result<(), AttachError>,
{
    close_attach_stream_inner(reader, writer, on_raw_output, true).await
}

async fn close_attach_stream_after_error<F>(
    reader: &mut AttachReader,
    writer: &mut AttachWriter,
    on_raw_output: &mut F,
) -> Result<(), AttachError>
where
    F: for<'a> FnMut(ManagedRawAttachEvent<'a>) -> Result<(), AttachError>,
{
    close_attach_stream_inner(reader, writer, on_raw_output, false).await
}

async fn close_attach_stream_inner<F>(
    reader: &mut AttachReader,
    writer: &mut AttachWriter,
    on_raw_output: &mut F,
    mut apply_output: bool,
) -> Result<(), AttachError>
where
    F: for<'a> FnMut(ManagedRawAttachEvent<'a>) -> Result<(), AttachError>,
{
    let mut first_error = writer
        .write_frame(&AttachStreamFrame::Close)
        .await
        .err()
        .map(AttachError::from);
    let drain = async {
        #[cfg(debug_assertions)]
        if std::env::var_os("MILLMUX_TEST_MANAGED_RAW_PREVIEW_CLOSE_TIMEOUT").is_some() {
            record_managed_raw_test_phase("preview_close_waiting");
            std::future::pending::<()>().await;
        }
        loop {
            let frame = match reader.next_frame().await {
                Ok(frame) => frame,
                Err(error) => return Err(first_error.take().unwrap_or_else(|| error.into())),
            };
            if apply_close_drain_frame(frame, &mut apply_output, &mut first_error, on_raw_output) {
                return first_error.take().map_or(Ok(()), Err);
            }
        }
    };
    let timeout = {
        #[cfg(debug_assertions)]
        if std::env::var_os("MILLMUX_TEST_MANAGED_RAW_PREVIEW_CLOSE_TIMEOUT").is_some() {
            Duration::from_millis(150)
        } else {
            ATTACH_CLOSE_TIMEOUT
        }
        #[cfg(not(debug_assertions))]
        ATTACH_CLOSE_TIMEOUT
    };
    match tokio::time::timeout(timeout, drain).await {
        Ok(result) => result,
        Err(_) => Err(first_error.unwrap_or_else(|| {
            AttachError::Stream(format!(
                "attach close confirmation timed out after {}ms",
                timeout.as_millis()
            ))
        })),
    }
}

fn apply_close_drain_frame<F>(
    frame: Option<AttachStreamFrame>,
    apply_output: &mut bool,
    first_error: &mut Option<AttachError>,
    on_raw_output: &mut F,
) -> bool
where
    F: for<'a> FnMut(ManagedRawAttachEvent<'a>) -> Result<(), AttachError>,
{
    match frame {
        Some(AttachStreamFrame::RawOutput { data }) if *apply_output => {
            if let Err(error) = on_raw_output(ManagedRawAttachEvent::Output {
                bytes: data.as_slice(),
            }) {
                first_error.get_or_insert(error);
                *apply_output = false;
            }
        }
        Some(AttachStreamFrame::Output { text }) if *apply_output => {
            if let Err(error) = on_raw_output(ManagedRawAttachEvent::Output {
                bytes: text.as_bytes(),
            }) {
                first_error.get_or_insert(error);
                *apply_output = false;
            }
        }
        Some(AttachStreamFrame::Error { error }) => {
            first_error.get_or_insert_with(|| AttachError::Stream(error.message));
            *apply_output = false;
        }
        Some(AttachStreamFrame::Closed) | None => return true,
        Some(_) => {}
    }
    false
}

pub(crate) async fn close_attach_stream_without_output(
    reader: &mut AttachReader,
    writer: &mut AttachWriter,
) -> Result<(), AttachError> {
    close_attach_stream_inner(reader, writer, &mut |_| Ok(()), false).await
}

#[derive(Debug, Default)]
struct DetachScanner {
    prefix_pending: bool,
}

impl DetachScanner {
    fn scan(&mut self, bytes: &[u8]) -> (Vec<u8>, bool) {
        let mut forwarded = Vec::with_capacity(bytes.len() + usize::from(self.prefix_pending));
        for byte in bytes {
            if self.prefix_pending {
                self.prefix_pending = false;
                if *byte == MANAGED_RAW_DETACH_KEY {
                    return (forwarded, true);
                }
                forwarded.push(MANAGED_RAW_DETACH_PREFIX);
                forwarded.push(*byte);
            } else if *byte == MANAGED_RAW_DETACH_PREFIX {
                self.prefix_pending = true;
            } else {
                forwarded.push(*byte);
            }
        }
        (forwarded, false)
    }

    fn finish(&mut self) -> Option<Vec<u8>> {
        self.prefix_pending.then(|| {
            self.prefix_pending = false;
            vec![MANAGED_RAW_DETACH_PREFIX]
        })
    }
}

fn write_scrollback(lines: &[String]) -> Result<(), AttachError> {
    if lines.is_empty() {
        return Ok(());
    }
    let mut stdout = io::stdout();
    for line in lines {
        stdout.write_all(line.as_bytes())?;
        stdout.write_all(b"\n")?;
    }
    stdout.flush()?;
    Ok(())
}

fn write_stdout(bytes: &[u8]) -> Result<(), AttachError> {
    let mut stdout = io::stdout();
    stdout.write_all(bytes)?;
    stdout.flush()?;
    Ok(())
}

struct ManagedStdout {
    file: AsyncFd<File>,
}

impl ManagedStdout {
    fn open() -> Result<Self, AttachError> {
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(OFlag::O_NONBLOCK.bits())
            .open("/dev/stdout")?;
        Ok(Self {
            file: AsyncFd::new(file)?,
        })
    }

    async fn write_all(&self, bytes: &[u8]) -> Result<(), AttachError> {
        self.write_all_with_timeout(bytes, managed_output_write_timeout())
            .await
    }

    async fn write_all_with_timeout(
        &self,
        mut bytes: &[u8],
        write_timeout: Duration,
    ) -> Result<(), AttachError> {
        let deadline = Instant::now() + write_timeout;
        while !bytes.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(managed_stdout_timeout(write_timeout));
            }
            let mut writable = tokio::time::timeout(remaining, self.file.writable())
                .await
                .map_err(|_| managed_stdout_timeout(write_timeout))??;
            match writable.try_io(|inner| {
                let mut file = inner.get_ref();
                file.write(bytes)
            }) {
                Ok(Ok(0)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "managed raw stdout closed before output completed",
                    )
                    .into())
                }
                Ok(Ok(written)) => bytes = &bytes[written..],
                Ok(Err(error)) => return Err(error.into()),
                Err(_) => continue,
            }
        }
        Ok(())
    }
}

pub(crate) fn managed_output_write_timeout() -> Duration {
    #[cfg(debug_assertions)]
    if let Ok(value) = std::env::var("MILLMUX_TEST_MANAGED_RAW_STDOUT_TIMEOUT_MS") {
        if let Ok(milliseconds) = value.parse::<u64>() {
            return Duration::from_millis(milliseconds);
        }
    }
    ATTACH_STREAM_WRITE_TIMEOUT
}

fn managed_stdout_timeout(write_timeout: Duration) -> AttachError {
    record_managed_raw_test_phase("raw_stdout_write_timed_out");
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "managed raw stdout write timed out after {}ms",
            write_timeout.as_millis()
        ),
    )
    .into()
}

pub struct TerminalModeGuard {
    file: Option<File>,
    original: Option<Termios>,
}

impl TerminalModeGuard {
    fn activate(input: &AsyncTtyInput) -> Result<Self, AttachError> {
        let tty = input.file.get_ref();
        let restoration_file = tty.try_clone()?;
        let original = tcgetattr(tty.as_fd()).map_err(terminal_error)?;
        let mut raw = original.clone();
        cfmakeraw(&mut raw);
        tcsetattr(tty.as_fd(), SetArg::TCSANOW, &raw).map_err(terminal_error)?;

        Ok(Self {
            file: Some(restoration_file),
            original: Some(original),
        })
    }

    pub fn inactive() -> Self {
        Self {
            file: None,
            original: None,
        }
    }

    #[cfg(test)]
    pub fn is_active(&self) -> bool {
        self.file.is_some()
    }
}

impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        let (Some(file), Some(original)) = (self.file.as_ref(), self.original.as_ref()) else {
            return;
        };
        let _ = tcsetattr(file.as_fd(), SetArg::TCSANOW, original);
    }
}

fn terminal_error(error: nix::errno::Errno) -> AttachError {
    AttachError::Terminal(error.to_string())
}

#[derive(Debug, Error)]
pub enum AttachError {
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("terminal error: {0}")]
    Terminal(String),
    #[error("attach stream error: {0}")]
    Stream(String),
    #[error("attach compatibility error: {0}")]
    Compatibility(String),
    #[error("raw attach replay is unavailable or stale: {0:?}")]
    SnapshotUnavailable(SnapshotUnavailableReason),
    #[error("managed raw attach cancelled")]
    Cancelled,
    #[error("managed raw attach panicked: {0}")]
    Panic(String),
}

impl From<serde_json::Error> for AttachError {
    fn from(error: serde_json::Error) -> Self {
        Self::Stream(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use millrace_sessions_core::{
        ids::SessionId,
        protocol::{
            AttachFrameType, AttachInitialReplay, AttachStreamEncoding, StreamKind, StreamSetup,
            M1_PROTOCOL_VERSION, M2_ATTACH_PROTOCOL_VERSION,
        },
    };

    use super::*;

    #[test]
    fn attach_terminal_guard_inactive_is_noop() {
        let guard = TerminalModeGuard::inactive();
        assert!(!guard.is_active());
    }

    #[test]
    fn attach_stream_frame_round_trips_output() {
        let frame = AttachStreamFrame::Output {
            text: "ready\n".to_string(),
        };
        let line = frame.to_json_line().unwrap();
        assert_eq!(AttachStreamFrame::from_json_line(&line).unwrap(), frame);
    }

    #[test]
    fn managed_raw_detach_scanner_forwards_ctrl_c_and_non_detach_prefixes() {
        let mut scanner = DetachScanner::default();

        assert_eq!(scanner.scan(&[0x03, b'A']), (vec![0x03, b'A'], false));
        assert_eq!(scanner.scan(&[MANAGED_RAW_DETACH_PREFIX]), (vec![], false));
        assert_eq!(
            scanner.scan(b"x"),
            (vec![MANAGED_RAW_DETACH_PREFIX, b'x'], false)
        );
        assert_eq!(scanner.finish(), None);
    }

    #[test]
    fn ordinary_attach_uses_stdin_only_when_it_is_a_terminal() {
        assert_eq!(attach_input_path(false, false), None);
        assert_eq!(attach_input_path(false, true), Some("/dev/stdin"));
        assert_eq!(attach_input_path(true, false), Some("/dev/tty"));
    }

    #[test]
    fn managed_raw_detach_scanner_reserves_only_split_ctrl_bracket_d() {
        let mut scanner = DetachScanner::default();

        assert_eq!(scanner.scan(&[MANAGED_RAW_DETACH_PREFIX]), (vec![], false));
        assert_eq!(scanner.scan(b"d"), (vec![], true));
        assert_eq!(scanner.finish(), None);
    }

    #[test]
    fn managed_raw_detach_scanner_flushes_a_pending_prefix_at_eof() {
        let mut scanner = DetachScanner::default();

        assert_eq!(scanner.scan(&[MANAGED_RAW_DETACH_PREFIX]), (vec![], false));
        assert_eq!(scanner.finish(), Some(vec![MANAGED_RAW_DETACH_PREFIX]));
        assert_eq!(scanner.finish(), None);
    }

    #[test]
    fn raw_attach_negotiation_accepts_confirmed_v2_raw_bytes() {
        let request = raw_attach_request();
        let response = raw_attach_response(
            Some(M2_ATTACH_PROTOCOL_VERSION),
            Some(AttachStreamEncoding::RawBytes),
            vec![
                AttachFrameType::RawOutput,
                AttachFrameType::StreamLagged,
                AttachFrameType::SnapshotUnavailable,
            ],
        );

        validate_attach_negotiation(&request, &response).unwrap();
    }

    #[test]
    fn raw_attach_negotiation_requires_raw_input_for_writable_stream() {
        let mut request = raw_attach_request();
        request.read_only = false;
        request.accepted_frame_types.push(AttachFrameType::RawInput);
        let mut response = raw_attach_response(
            Some(M2_ATTACH_PROTOCOL_VERSION),
            Some(AttachStreamEncoding::RawBytes),
            vec![
                AttachFrameType::RawOutput,
                AttachFrameType::StreamLagged,
                AttachFrameType::SnapshotUnavailable,
            ],
        );
        response.stream.read_only = false;
        response.stream.input_owner = true;

        let error = validate_attach_negotiation(&request, &response).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("raw attach requires host-confirmed v2 raw-byte negotiation"),
            "{error}"
        );

        response
            .accepted_frame_types
            .push(AttachFrameType::RawInput);
        validate_attach_negotiation(&request, &response).unwrap();
    }

    #[test]
    fn raw_attach_negotiation_fails_closed_without_v2_raw_bytes() {
        let request = raw_attach_request();
        for response in [
            raw_attach_response(None, None, Vec::new()),
            raw_attach_response(
                Some(M1_PROTOCOL_VERSION),
                Some(AttachStreamEncoding::RawBytes),
                vec![AttachFrameType::RawOutput],
            ),
            raw_attach_response(
                Some(M2_ATTACH_PROTOCOL_VERSION),
                Some(AttachStreamEncoding::Text),
                vec![AttachFrameType::RawOutput],
            ),
            raw_attach_response(
                Some(M2_ATTACH_PROTOCOL_VERSION),
                Some(AttachStreamEncoding::RawBytes),
                Vec::new(),
            ),
        ] {
            let error = validate_attach_negotiation(&request, &response).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("raw attach requires host-confirmed v2 raw-byte negotiation"),
                "{error}"
            );
        }
    }

    #[test]
    fn raw_attach_recovery_frames_and_legacy_frames_fail_closed() {
        let lagged = AttachStreamFrame::StreamLagged {
            dropped_bytes: 16,
            dropped_from_offset: 4,
            dropped_to_offset: 20,
            current_pty_log_offset: 20,
            reason: millrace_sessions_core::protocol::AttachStreamLagReason::ObserverBackpressure,
            recover: "request_screen_or_reattach_raw_replay".to_string(),
        };
        assert!(raw_attach_recovery_error(Some(&lagged))
            .expect("lagged raw frame must fail")
            .to_string()
            .contains("stream lagged"));

        let unavailable = AttachStreamFrame::SnapshotUnavailable {
            reason: millrace_sessions_core::protocol::SnapshotUnavailableReason::StaleSnapshot,
            details: None,
        };
        assert!(raw_attach_recovery_error(Some(&unavailable))
            .expect("unavailable raw replay must fail")
            .to_string()
            .contains("unavailable or stale"));

        let incompatible = AttachStreamFrame::Output {
            text: "legacy text frame".to_string(),
        };
        assert!(raw_attach_recovery_error(Some(&incompatible)).is_none());
        assert!(!raw_attach_frame_is_compatible(Some(&incompatible)));
    }

    fn raw_attach_request() -> SessionAttachRequest {
        SessionAttachRequest {
            selector: millrace_sessions_core::protocol::SessionSelector::Name {
                name: "shell".to_string(),
            },
            read_only: true,
            replay: millrace_sessions_core::protocol::AttachReplayMode::None,
            requested_terminal_size: None,
            client_protocol_version: Some(M2_ATTACH_PROTOCOL_VERSION),
            accepted_frame_types: vec![
                AttachFrameType::RawOutput,
                AttachFrameType::StreamLagged,
                AttachFrameType::SnapshotUnavailable,
            ],
            stream_encoding: Some(AttachStreamEncoding::RawBytes),
            initial_replay: Some(AttachInitialReplay::RawReplay),
        }
    }

    fn raw_attach_response(
        negotiated_attach_protocol_version: Option<u32>,
        negotiated_stream_encoding: Option<AttachStreamEncoding>,
        accepted_frame_types: Vec<AttachFrameType>,
    ) -> SessionAttachResponse {
        SessionAttachResponse {
            schema_version: M1_PROTOCOL_VERSION,
            protocol_version: M1_PROTOCOL_VERSION,
            session_id: SessionId::new(),
            stream: StreamSetup {
                stream_id: "attach-test".to_string(),
                kind: StreamKind::Attach,
                read_only: true,
                input_owner: false,
            },
            negotiated_attach_protocol_version,
            negotiated_stream_encoding,
            negotiated_initial_replay: Some(AttachInitialReplay::None),
            accepted_frame_types,
        }
    }
}
