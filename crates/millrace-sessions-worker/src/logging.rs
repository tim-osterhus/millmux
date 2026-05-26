use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use millrace_sessions_core::{
    error::{MillmuxError, MillmuxResult},
    events::{append_event, SessionEvent, SessionEventKind},
    ids::SessionId,
    scrollback::{ScrollbackBuffer, TerminalStateBuffer},
    storage::append_raw_pty_log,
};

pub type SharedTerminalState = Arc<Mutex<TerminalStateBuffer>>;

#[derive(Clone)]
pub struct OutputLoggerConfig {
    pub session_id: SessionId,
    pub pty_log: PathBuf,
    pub events_jsonl: PathBuf,
    pub scrollback_snapshot: PathBuf,
    pub terminal_snapshot: PathBuf,
    pub raw_replay_ring: PathBuf,
    pub terminal_state: SharedTerminalState,
    pub scrollback_capacity: usize,
}

pub struct OutputLogger {
    config: OutputLoggerConfig,
    scrollback: ScrollbackBuffer,
    pending_line: Vec<u8>,
}

impl OutputLogger {
    pub fn new(config: OutputLoggerConfig) -> MillmuxResult<Self> {
        let scrollback = if config.scrollback_snapshot.exists() {
            ScrollbackBuffer::restore_snapshot(&config.scrollback_snapshot)?
        } else {
            ScrollbackBuffer::new(config.scrollback_capacity)
        };

        Ok(Self {
            config,
            scrollback,
            pending_line: Vec::new(),
        })
    }

    pub fn record_output(&mut self, bytes: &[u8]) -> MillmuxResult<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        append_raw_pty_log(&self.config.pty_log, bytes)?;
        self.record_terminal_output(bytes)?;
        self.pending_line.extend_from_slice(bytes);

        while let Some(index) = self.pending_line.iter().position(|byte| *byte == b'\n') {
            let mut line = self.pending_line.drain(..=index).collect::<Vec<_>>();
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.record_structured_line(&line)?;
        }

        Ok(())
    }

    pub fn flush(&mut self) -> MillmuxResult<()> {
        if !self.pending_line.is_empty() {
            let line = std::mem::take(&mut self.pending_line);
            self.record_structured_line(&line)?;
        } else if !self.config.scrollback_snapshot.exists() {
            self.scrollback
                .persist_snapshot(&self.config.scrollback_snapshot)?;
            self.persist_terminal_state()?;
        }
        Ok(())
    }

    pub fn record_resize(&mut self, rows: u16, cols: u16) -> MillmuxResult<()> {
        let mut terminal_state = self
            .config
            .terminal_state
            .lock()
            .map_err(|_| MillmuxError::Internal("terminal state lock poisoned".to_string()))?;
        terminal_state.resize(rows, cols);
        terminal_state.persist(&self.config.terminal_snapshot, &self.config.raw_replay_ring)
    }

    fn record_structured_line(&mut self, line: &[u8]) -> MillmuxResult<()> {
        let message = String::from_utf8_lossy(line).to_string();
        let mut event = SessionEvent::new(self.config.session_id, SessionEventKind::Output);
        event.message = Some(message.clone());
        event.fields = BTreeMap::from([("stream".to_string(), "pty".to_string())]);
        append_event(&self.config.events_jsonl, &event)?;
        self.scrollback.push_line(message);
        self.scrollback
            .persist_snapshot(&self.config.scrollback_snapshot)?;
        Ok(())
    }

    fn record_terminal_output(&self, bytes: &[u8]) -> MillmuxResult<()> {
        let mut terminal_state = self
            .config
            .terminal_state
            .lock()
            .map_err(|_| MillmuxError::Internal("terminal state lock poisoned".to_string()))?;
        terminal_state.process_output(bytes);
        terminal_state.persist(&self.config.terminal_snapshot, &self.config.raw_replay_ring)
    }

    fn persist_terminal_state(&self) -> MillmuxResult<()> {
        let terminal_state = self
            .config
            .terminal_state
            .lock()
            .map_err(|_| MillmuxError::Internal("terminal state lock poisoned".to_string()))?;
        terminal_state.persist(&self.config.terminal_snapshot, &self.config.raw_replay_ring)
    }
}

pub fn pty_log_contains(path: impl Into<PathBuf>, needle: &str) -> bool {
    fs::read_to_string(path.into())
        .map(|contents| contents.contains(needle))
        .unwrap_or(false)
}
