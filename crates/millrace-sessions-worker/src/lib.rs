use std::{
    fs,
    io::{ErrorKind, Read},
    path::PathBuf,
    process::ExitStatus,
    sync::{
        mpsc::{self, Sender},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use control::{start_control_server, WorkerControlConfig};
use lifecycle::{record_failed_start, record_process_exit, record_running, write_worker_meta};
use millrace_sessions_core::{
    error::{MillmuxError, MillmuxResult},
    ids::SessionId,
    paths::StatePaths,
    protocol::LogStream,
    scrollback::{
        ScrollbackBuffer, TerminalStateBuffer, DEFAULT_RAW_REPLAY_CAPACITY_BYTES,
        DEFAULT_TERMINAL_COLS, DEFAULT_TERMINAL_ROWS,
    },
    state::{SessionMeta, SessionPaths, SpawnMode},
};
use pipe::{spawn_pipe, PipeCommandSpec};
use pty::{spawn_pty, PtyCommandSpec};

pub mod control;
pub mod lifecycle;
pub mod logging;
pub mod pipe;
pub mod pty;

pub fn binary_name() -> &'static str {
    "millrace-session-worker"
}

pub fn run_worker(session_id: SessionId, state_dir: impl Into<PathBuf>) -> MillmuxResult<()> {
    let state_paths = StatePaths::new(state_dir.into());
    let session_paths = state_paths.session_paths(session_id);
    let meta = lifecycle::load_session_meta(&session_paths)?;
    if meta.id != session_id {
        return Err(MillmuxError::InvalidProtocolData(format!(
            "meta.json id {} did not match requested session id {session_id}",
            meta.id
        )));
    }

    write_worker_meta(
        &session_paths,
        lifecycle::WorkerFacts {
            worker_pid: std::process::id(),
            child_pid: None,
            child_pgid: None,
            spawn_mode: meta.spawn_mode,
        },
    )?;

    match meta.spawn_mode {
        SpawnMode::Pty => run_pty_worker(session_id, session_paths, meta),
        SpawnMode::Pipe => run_pipe_worker(session_id, session_paths, meta),
    }
}

fn run_pty_worker(
    session_id: SessionId,
    session_paths: SessionPaths,
    meta: SessionMeta,
) -> MillmuxResult<()> {
    let running = match spawn_pty(PtyCommandSpec {
        argv: meta.argv.clone(),
        cwd: meta.cwd.clone(),
        env: meta.env.clone(),
    }) {
        Ok(running) => running,
        Err(error) => {
            let _ = record_failed_start(&session_paths, error.to_string());
            return Err(error);
        }
    };
    let pty::RunningPty {
        mut reader,
        writer,
        mut child,
        child_pid,
        child_pgid,
        master,
    } = running;

    let current_pty_offset = fs::metadata(&session_paths.pty_log)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let terminal_state = Arc::new(Mutex::new(TerminalStateBuffer::restore_or_new(
        &session_paths.terminal_snapshot,
        &session_paths.raw_replay_ring,
        current_pty_offset,
        DEFAULT_TERMINAL_ROWS,
        DEFAULT_TERMINAL_COLS,
        DEFAULT_RAW_REPLAY_CAPACITY_BYTES,
    )?));

    let control = start_control_server(WorkerControlConfig {
        paths: session_paths.clone(),
        writer: Arc::new(Mutex::new(writer)),
        master: Arc::new(Mutex::new(master)),
        terminal_state: Arc::clone(&terminal_state),
        child_pid,
        child_pgid,
    })?;

    record_running(&session_paths, child_pid, child_pgid)?;

    let mut logger = logging::OutputLogger::new(logging::OutputLoggerConfig {
        session_id,
        pty_log: session_paths.pty_log.clone(),
        events_jsonl: session_paths.events_jsonl.clone(),
        scrollback_snapshot: session_paths.scrollback_snapshot.clone(),
        terminal_snapshot: session_paths.terminal_snapshot.clone(),
        raw_replay_ring: session_paths.raw_replay_ring.clone(),
        terminal_state,
        scrollback_capacity: ScrollbackBuffer::default_capacity(),
    })?;
    let mut buffer = [0_u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                let logged = logger.record_output(&buffer[..count])?;
                control.broadcast_output(&buffer[..count], logged.start_offset, logged.end_offset);
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => break,
            Err(error) if error.raw_os_error() == Some(5) => break,
            Err(error) => return Err(MillmuxError::Io(error)),
        }
    }
    logger.flush()?;

    let status = child.wait()?;
    record_process_exit(&session_paths, status.exit_code() as i32, None)?;
    Ok(())
}

fn run_pipe_worker(
    session_id: SessionId,
    session_paths: SessionPaths,
    meta: SessionMeta,
) -> MillmuxResult<()> {
    let running = match spawn_pipe(PipeCommandSpec {
        argv: meta.argv.clone(),
        cwd: meta.cwd.clone(),
        env: meta.env.clone(),
    }) {
        Ok(running) => running,
        Err(error) => {
            let _ = record_failed_start(&session_paths, error.to_string());
            return Err(error);
        }
    };
    let pipe::RunningPipe {
        stdout,
        stderr,
        mut child,
        child_pid,
        child_pgid,
    } = running;

    let mut stdout_logger = logging::PipeOutputLogger::new(logging::PipeOutputLoggerConfig {
        session_id,
        log: session_paths.stdout_log.clone(),
        events_jsonl: session_paths.events_jsonl.clone(),
        stream: LogStream::Stdout,
    })?;
    let mut stderr_logger = logging::PipeOutputLogger::new(logging::PipeOutputLoggerConfig {
        session_id,
        log: session_paths.stderr_log.clone(),
        events_jsonl: session_paths.events_jsonl.clone(),
        stream: LogStream::Stderr,
    })?;

    let (sender, receiver) = mpsc::channel();
    let stdout_reader = spawn_pipe_reader(LogStream::Stdout, stdout, sender.clone());
    let stderr_reader = spawn_pipe_reader(LogStream::Stderr, stderr, sender.clone());
    drop(sender);

    record_running(&session_paths, child_pid, child_pgid)?;

    let mut sequence = 0_u64;
    for message in receiver {
        match message {
            PipeReadMessage::Chunk { stream, bytes } => {
                sequence += 1;
                match stream {
                    LogStream::Stdout => {
                        stdout_logger.record_chunk(&bytes, sequence)?;
                    }
                    LogStream::Stderr => {
                        stderr_logger.record_chunk(&bytes, sequence)?;
                    }
                    LogStream::Pty => {}
                }
            }
            PipeReadMessage::Done { result } => result?,
        }
    }

    let status = child.wait()?;
    join_pipe_reader(stdout_reader)?;
    join_pipe_reader(stderr_reader)?;
    record_process_exit(
        &session_paths,
        exit_code_for_status(&status),
        exit_signal_for_status(&status),
    )?;
    Ok(())
}

enum PipeReadMessage {
    Chunk { stream: LogStream, bytes: Vec<u8> },
    Done { result: MillmuxResult<()> },
}

fn spawn_pipe_reader<R>(
    stream: LogStream,
    mut reader: R,
    sender: Sender<PipeReadMessage>,
) -> thread::JoinHandle<()>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        let result: MillmuxResult<()> = loop {
            match reader.read(&mut buffer) {
                Ok(0) => break Ok(()),
                Ok(count) => {
                    if sender
                        .send(PipeReadMessage::Chunk {
                            stream,
                            bytes: buffer[..count].to_vec(),
                        })
                        .is_err()
                    {
                        break Ok(());
                    }
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => break Err(MillmuxError::Io(error)),
            }
        };
        let _ = sender.send(PipeReadMessage::Done { result });
    })
}

fn join_pipe_reader(handle: thread::JoinHandle<()>) -> MillmuxResult<()> {
    handle
        .join()
        .map_err(|_| MillmuxError::Internal("pipe reader thread panicked".to_string()))
}

fn exit_code_for_status(status: &ExitStatus) -> i32 {
    status.code().unwrap_or_else(|| {
        exit_signal_number(status)
            .map(|signal| 128 + signal)
            .unwrap_or(-1)
    })
}

fn exit_signal_for_status(status: &ExitStatus) -> Option<String> {
    exit_signal_number(status).map(|signal| signal.to_string())
}

#[cfg(unix)]
fn exit_signal_number(status: &ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal_number(_status: &ExitStatus) -> Option<i32> {
    None
}
