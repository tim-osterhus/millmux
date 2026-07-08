use std::{
    fs,
    io::{ErrorKind, Read},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use control::{start_control_server, WorkerControlConfig};
use lifecycle::{record_failed_start, record_process_exit, record_running, write_worker_meta};
use millrace_sessions_core::{
    error::{MillmuxError, MillmuxResult},
    ids::SessionId,
    paths::StatePaths,
    scrollback::{
        ScrollbackBuffer, TerminalStateBuffer, DEFAULT_RAW_REPLAY_CAPACITY_BYTES,
        DEFAULT_TERMINAL_COLS, DEFAULT_TERMINAL_ROWS,
    },
};
use pty::{spawn_pty, PtyCommandSpec};

pub mod control;
pub mod lifecycle;
pub mod logging;
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
        },
    )?;

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
