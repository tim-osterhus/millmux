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
    protocol::LogStream,
    scrollback::{ScrollbackBuffer, TerminalStateBuffer},
    storage::{append_raw_log, append_raw_pty_log},
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoggedOutput {
    pub start_offset: u64,
    pub end_offset: u64,
}

#[derive(Clone)]
pub struct PipeOutputLoggerConfig {
    pub session_id: SessionId,
    pub log: PathBuf,
    pub events_jsonl: PathBuf,
    pub stream: LogStream,
}

pub struct PipeOutputLogger {
    config: PipeOutputLoggerConfig,
}

impl PipeOutputLogger {
    pub fn new(config: PipeOutputLoggerConfig) -> MillmuxResult<Self> {
        append_raw_log(&config.log, b"")?;
        Ok(Self { config })
    }

    pub fn record_chunk(&mut self, bytes: &[u8], sequence: u64) -> MillmuxResult<LoggedOutput> {
        let start_offset = fs::metadata(&self.config.log)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if bytes.is_empty() {
            return Ok(LoggedOutput {
                start_offset,
                end_offset: start_offset,
            });
        }
        let end_offset = start_offset + bytes.len() as u64;
        append_raw_log(&self.config.log, bytes)?;
        let message = String::from_utf8_lossy(bytes).to_string();
        let mut event = SessionEvent::new(self.config.session_id, SessionEventKind::Output);
        event.message = Some(message);
        event.fields = BTreeMap::from([
            (
                "stream".to_string(),
                self.config.stream.as_wire_value().to_string(),
            ),
            ("record_kind".to_string(), "chunk".to_string()),
            ("pipe_sequence".to_string(), sequence.to_string()),
            ("byte_count".to_string(), bytes.len().to_string()),
            ("stream_start_offset".to_string(), start_offset.to_string()),
            ("stream_end_offset".to_string(), end_offset.to_string()),
            ("content_encoding".to_string(), "utf8_lossy".to_string()),
        ]);
        append_event(&self.config.events_jsonl, &event)?;
        Ok(LoggedOutput {
            start_offset,
            end_offset,
        })
    }
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

    pub fn record_output(&mut self, bytes: &[u8]) -> MillmuxResult<LoggedOutput> {
        let start_offset = fs::metadata(&self.config.pty_log)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if bytes.is_empty() {
            return Ok(LoggedOutput {
                start_offset,
                end_offset: start_offset,
            });
        }
        let end_offset = start_offset + bytes.len() as u64;
        self.record_terminal_output(bytes)?;
        append_raw_pty_log(&self.config.pty_log, bytes)?;
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

        Ok(LoggedOutput {
            start_offset,
            end_offset,
        })
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
