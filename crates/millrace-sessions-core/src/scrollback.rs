use std::{collections::VecDeque, fs, path::Path};

use serde::{Deserialize, Serialize};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::{
    error::MillmuxResult,
    protocol::{
        ScreenCell, ScreenColor, ScreenCursor, ScreenSnapshot, ScreenSnapshotSource, ScreenStyle,
        SCREEN_SNAPSHOT_SCHEMA_VERSION,
    },
    storage::{read_json, write_json_atomic, write_private_bytes_atomic},
};

pub const DEFAULT_SCROLLBACK_CAPACITY: usize = 4000;
pub const DEFAULT_RAW_REPLAY_CAPACITY_BYTES: usize = 1024 * 1024;
pub const DEFAULT_TERMINAL_ROWS: u16 = 24;
pub const DEFAULT_TERMINAL_COLS: u16 = 80;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrollbackSnapshot {
    pub capacity: usize,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollbackBuffer {
    capacity: usize,
    lines: VecDeque<String>,
}

impl ScrollbackBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            lines: VecDeque::with_capacity(capacity),
        }
    }

    pub fn default_capacity() -> usize {
        DEFAULT_SCROLLBACK_CAPACITY
    }

    pub fn push_line(&mut self, line: impl Into<String>) {
        if self.capacity == 0 {
            return;
        }
        while self.lines.len() >= self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(line.into());
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn lines(&self) -> Vec<String> {
        self.lines.iter().cloned().collect()
    }

    pub fn snapshot(&self) -> ScrollbackSnapshot {
        ScrollbackSnapshot {
            capacity: self.capacity,
            lines: self.lines(),
        }
    }

    pub fn from_snapshot(snapshot: ScrollbackSnapshot) -> Self {
        let mut buffer = Self::new(snapshot.capacity);
        for line in snapshot.lines {
            buffer.push_line(line);
        }
        buffer
    }

    pub fn persist_snapshot(&self, path: impl AsRef<Path>) -> MillmuxResult<()> {
        write_json_atomic(path, &self.snapshot())
    }

    pub fn restore_snapshot(path: impl AsRef<Path>) -> MillmuxResult<Self> {
        let snapshot: ScrollbackSnapshot = read_json(path)?;
        Ok(Self::from_snapshot(snapshot))
    }
}

impl Default for ScrollbackBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_SCROLLBACK_CAPACITY)
    }
}

pub fn legacy_line_scrollback_contains_tui_sequences(lines: &[String]) -> bool {
    legacy_line_scrollback_tui_sequence_name(lines).is_some()
}

pub fn legacy_line_scrollback_tui_sequence_name(lines: &[String]) -> Option<&'static str> {
    lines.iter().find_map(|line| tui_sequence_name(line))
}

fn tui_sequence_name(line: &str) -> Option<&'static str> {
    let bytes = line.as_bytes();
    let mut index = 0;

    while index + 2 <= bytes.len() {
        let offset = bytes[index..]
            .windows(2)
            .position(|window| window == b"\x1b[")?;
        let start = index + offset;
        let mut cursor = start + 2;

        while cursor < bytes.len() && (0x30..=0x3f).contains(&bytes[cursor]) {
            cursor += 1;
        }
        while cursor < bytes.len() && (0x20..=0x2f).contains(&bytes[cursor]) {
            cursor += 1;
        }
        if cursor >= bytes.len() {
            return None;
        }

        let params = std::str::from_utf8(&bytes[start + 2..cursor]).unwrap_or("");
        let final_byte = bytes[cursor];
        if let Some(name) = classify_tui_csi(params, final_byte) {
            return Some(name);
        }
        index = cursor + 1;
    }

    None
}

fn classify_tui_csi(params: &str, final_byte: u8) -> Option<&'static str> {
    match final_byte {
        b'h' | b'l' if params == "?1049" => Some("alternate_screen"),
        b'h' | b'l' if params == "?2026" => Some("synchronized_output"),
        b'J' if matches!(params, "" | "2" | "3") => Some("screen_clear"),
        b'K' if matches!(params, "" | "2") => Some("line_clear"),
        b'H' | b'f' if is_cursor_position_params(params) => Some("cursor_position"),
        _ => None,
    }
}

fn is_cursor_position_params(params: &str) -> bool {
    params.is_empty()
        || params
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b';')
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSnapshot {
    pub schema_version: u32,
    pub rows: u16,
    pub cols: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub alternate_screen: bool,
    pub pty_log_offset: u64,
    pub raw_replay_start_offset: u64,
    pub raw_replay_end_offset: u64,
    pub captured_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_screen: Option<ScreenSnapshot>,
    pub screen: Vec<String>,
}

pub type TerminalReplayCheckpoint = TerminalSnapshot;

impl TerminalSnapshot {
    pub fn replay_is_fresh(&self, current_pty_offset: u64, raw_replay_len: usize) -> bool {
        self.pty_log_offset == current_pty_offset
            && self.raw_replay_end_offset == current_pty_offset
            && self
                .raw_replay_end_offset
                .saturating_sub(self.raw_replay_start_offset)
                == raw_replay_len as u64
    }

    pub fn same_size(&self, rows: u16, cols: u16) -> bool {
        self.rows == rows && self.cols == cols
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalReplay {
    pub snapshot: TerminalSnapshot,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TerminalReplayMetadata {
    pub rows: u16,
    pub cols: u16,
    pub pty_log_offset: u64,
    pub raw_replay_start_offset: u64,
    pub raw_replay_end_offset: u64,
}

impl TerminalReplayMetadata {
    pub fn replay_is_fresh(&self, current_pty_offset: u64, raw_replay_len: usize) -> bool {
        self.pty_log_offset == current_pty_offset
            && self.raw_replay_end_offset == current_pty_offset
            && self
                .raw_replay_end_offset
                .saturating_sub(self.raw_replay_start_offset)
                == raw_replay_len as u64
    }

    pub fn same_size(&self, rows: u16, cols: u16) -> bool {
        self.rows == rows && self.cols == cols
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalReplayBytes {
    pub metadata: TerminalReplayMetadata,
    pub bytes: Vec<u8>,
}

pub fn restore_terminal_replay(
    snapshot_path: impl AsRef<Path>,
    raw_replay_path: impl AsRef<Path>,
    current_pty_offset: u64,
) -> MillmuxResult<Option<TerminalReplay>> {
    let snapshot_path = snapshot_path.as_ref();
    let raw_replay_path = raw_replay_path.as_ref();
    if !snapshot_path.exists() || !raw_replay_path.exists() {
        return Ok(None);
    }

    let snapshot: TerminalSnapshot = read_json(snapshot_path)?;
    let bytes = fs::read(raw_replay_path)?;
    if !snapshot.replay_is_fresh(current_pty_offset, bytes.len()) {
        return Ok(None);
    }

    Ok(Some(TerminalReplay { snapshot, bytes }))
}

pub fn restore_terminal_replay_bytes(
    snapshot_path: impl AsRef<Path>,
    raw_replay_path: impl AsRef<Path>,
    current_pty_offset: u64,
) -> MillmuxResult<Option<TerminalReplayBytes>> {
    let snapshot_path = snapshot_path.as_ref();
    let raw_replay_path = raw_replay_path.as_ref();
    if !snapshot_path.exists() || !raw_replay_path.exists() {
        return Ok(None);
    }

    let metadata: TerminalReplayMetadata = read_json(snapshot_path)?;
    let bytes = fs::read(raw_replay_path)?;
    if !metadata.replay_is_fresh(current_pty_offset, bytes.len()) {
        return Ok(None);
    }

    Ok(Some(TerminalReplayBytes { metadata, bytes }))
}

pub struct TerminalStateBuffer {
    parser: vt100::Parser,
    raw_replay: RawReplayBuffer,
    pty_log_offset: u64,
}

impl TerminalStateBuffer {
    pub fn new(rows: u16, cols: u16, raw_replay_capacity: usize, pty_log_offset: u64) -> Self {
        Self {
            parser: vt100::Parser::new(rows.max(1), cols.max(1), DEFAULT_SCROLLBACK_CAPACITY),
            raw_replay: RawReplayBuffer::empty(raw_replay_capacity, pty_log_offset),
            pty_log_offset,
        }
    }

    pub fn restore_or_new(
        snapshot_path: impl AsRef<Path>,
        raw_replay_path: impl AsRef<Path>,
        current_pty_offset: u64,
        rows: u16,
        cols: u16,
        raw_replay_capacity: usize,
    ) -> MillmuxResult<Self> {
        if let Some(replay) =
            restore_terminal_replay(snapshot_path, raw_replay_path, current_pty_offset)?
        {
            let mut state = Self::new(
                replay.snapshot.rows,
                replay.snapshot.cols,
                raw_replay_capacity,
                current_pty_offset,
            );
            state.raw_replay = RawReplayBuffer::from_fresh_bytes(
                raw_replay_capacity,
                replay.snapshot.raw_replay_start_offset,
                replay.snapshot.raw_replay_end_offset,
                replay.bytes,
            );
            state.parser.process(state.raw_replay.bytes());
            return Ok(state);
        }

        Ok(Self::new(
            rows,
            cols,
            raw_replay_capacity,
            current_pty_offset,
        ))
    }

    pub fn process_output(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }

        let start_offset = self.pty_log_offset;
        self.parser.process(bytes);
        self.pty_log_offset = self.pty_log_offset.saturating_add(bytes.len() as u64);
        self.raw_replay.record(start_offset, bytes);
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows.max(1), cols.max(1));
    }

    pub fn same_size(&self, rows: u16, cols: u16) -> bool {
        let (current_rows, current_cols) = self.parser.screen().size();
        current_rows == rows && current_cols == cols
    }

    pub fn snapshot(&self) -> TerminalSnapshot {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let (cursor_row, cursor_col) = screen.cursor_position();
        let structured_screen = self.screen_snapshot();
        let screen_lines = structured_screen.plain_lines();

        TerminalSnapshot {
            schema_version: 1,
            rows,
            cols,
            cursor_row,
            cursor_col,
            alternate_screen: screen.alternate_screen(),
            pty_log_offset: self.pty_log_offset,
            raw_replay_start_offset: self.raw_replay.start_offset(),
            raw_replay_end_offset: self.raw_replay.end_offset(),
            captured_at: now_rfc3339(),
            structured_screen: Some(structured_screen),
            screen: screen_lines,
        }
    }

    pub fn screen_snapshot(&self) -> ScreenSnapshot {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let (cursor_row, cursor_col) = screen.cursor_position();
        let mut cells = Vec::with_capacity(usize::from(rows));

        for row in 0..rows {
            let mut snapshot_row = Vec::with_capacity(usize::from(cols));
            for col in 0..cols {
                snapshot_row.push(
                    screen
                        .cell(row, col)
                        .map(screen_cell_from_vt100)
                        .unwrap_or_else(ScreenCell::blank),
                );
            }
            cells.push(snapshot_row);
        }

        ScreenSnapshot {
            schema_version: SCREEN_SNAPSHOT_SCHEMA_VERSION,
            rows,
            cols,
            cursor: ScreenCursor {
                row: cursor_row,
                col: cursor_col,
                visible: Some(!screen.hide_cursor()),
            },
            alternate_screen: screen.alternate_screen(),
            cells,
            source: ScreenSnapshotSource {
                pty_log_offset: self.pty_log_offset,
                raw_replay_start_offset: self.raw_replay.start_offset(),
                raw_replay_end_offset: self.raw_replay.end_offset(),
            },
            captured_at: now_rfc3339(),
        }
    }

    pub fn persist(
        &self,
        snapshot_path: impl AsRef<Path>,
        raw_replay_path: impl AsRef<Path>,
    ) -> MillmuxResult<()> {
        write_private_bytes_atomic(raw_replay_path, self.raw_replay.bytes())?;
        write_json_atomic(snapshot_path, &self.snapshot())
    }
}

fn screen_cell_from_vt100(cell: &vt100::Cell) -> ScreenCell {
    let continuation = cell.is_wide_continuation();
    ScreenCell {
        symbol: if continuation || !cell.has_contents() {
            " ".to_string()
        } else {
            cell.contents().to_string()
        },
        width: if cell.is_wide() { 2 } else { 1 },
        fg: screen_color_from_vt100(cell.fgcolor()),
        bg: screen_color_from_vt100(cell.bgcolor()),
        style: ScreenStyle {
            bold: cell.bold(),
            dim: cell.dim(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        },
        continuation,
    }
}

fn screen_color_from_vt100(color: vt100::Color) -> ScreenColor {
    match color {
        vt100::Color::Default => ScreenColor::Default,
        vt100::Color::Idx(index) => ScreenColor::Indexed { index },
        vt100::Color::Rgb(r, g, b) => ScreenColor::Rgb { r, g, b },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawReplayBuffer {
    capacity: usize,
    start_offset: u64,
    end_offset: u64,
    bytes: Vec<u8>,
}

impl RawReplayBuffer {
    fn empty(capacity: usize, offset: u64) -> Self {
        Self {
            capacity,
            start_offset: offset,
            end_offset: offset,
            bytes: Vec::new(),
        }
    }

    fn from_fresh_bytes(
        capacity: usize,
        start_offset: u64,
        end_offset: u64,
        bytes: Vec<u8>,
    ) -> Self {
        let mut buffer = Self::empty(capacity, end_offset);
        if end_offset.saturating_sub(start_offset) == bytes.len() as u64 {
            buffer.start_offset = start_offset;
            buffer.end_offset = end_offset;
            buffer.bytes = bytes;
            buffer.trim_to_capacity();
        }
        buffer
    }

    fn record(&mut self, start_offset: u64, bytes: &[u8]) {
        let end_offset = start_offset.saturating_add(bytes.len() as u64);
        if self.capacity == 0 {
            self.bytes.clear();
            self.start_offset = end_offset;
            self.end_offset = end_offset;
            return;
        }

        if self.end_offset != start_offset {
            self.bytes.clear();
            self.start_offset = start_offset;
        }

        self.bytes.extend_from_slice(bytes);
        self.end_offset = end_offset;
        self.trim_to_capacity();
    }

    fn trim_to_capacity(&mut self) {
        if self.bytes.len() <= self.capacity {
            return;
        }

        let drop_count = self.bytes.len() - self.capacity;
        self.bytes.drain(..drop_count);
        self.start_offset = self.start_offset.saturating_add(drop_count as u64);
    }

    fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn start_offset(&self) -> u64 {
        self.start_offset
    }

    fn end_offset(&self) -> u64 {
        self.end_offset
    }
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrollback_defaults_to_4000_lines() {
        assert_eq!(
            ScrollbackBuffer::default_capacity(),
            DEFAULT_SCROLLBACK_CAPACITY
        );
    }

    #[test]
    fn scrollback_drops_oldest_lines() {
        let mut buffer = ScrollbackBuffer::new(3);
        buffer.push_line("one");
        buffer.push_line("two");
        buffer.push_line("three");
        buffer.push_line("four");
        assert_eq!(buffer.lines(), vec!["two", "three", "four"]);
    }

    #[test]
    fn scrollback_persists_and_restores_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("scrollback.snapshot");
        let mut buffer = ScrollbackBuffer::new(2);
        buffer.push_line("a");
        buffer.push_line("b");
        buffer.persist_snapshot(&path).unwrap();
        let restored = ScrollbackBuffer::restore_snapshot(&path).unwrap();
        assert_eq!(restored.lines(), vec!["a", "b"]);
        assert_eq!(restored.snapshot().capacity, 2);
    }

    #[test]
    fn legacy_line_scrollback_detects_likely_tui_sequences() {
        let lines = vec![
            "plain \x1b[32mcolor\x1b[0m is not enough".to_string(),
            "\x1b[?1049h\x1b[2J\x1b[4;9Hagent frame".to_string(),
        ];

        assert!(legacy_line_scrollback_contains_tui_sequences(&lines));
        assert_eq!(
            legacy_line_scrollback_tui_sequence_name(&lines),
            Some("alternate_screen")
        );
    }

    #[test]
    fn legacy_line_scrollback_ignores_plain_ansi_color() {
        let lines = vec!["status \x1b[32mok\x1b[0m".to_string()];

        assert!(!legacy_line_scrollback_contains_tui_sequences(&lines));
        assert_eq!(legacy_line_scrollback_tui_sequence_name(&lines), None);
    }

    #[test]
    fn raw_terminal_replay_is_bounded_and_tracks_offsets() {
        let mut state = TerminalStateBuffer::new(4, 12, 8, 0);

        state.process_output(b"hello ");
        state.process_output(&[0x00, 0xff, b'a', b'b']);
        let snapshot = state.snapshot();

        assert_eq!(snapshot.pty_log_offset, 10);
        assert_eq!(snapshot.raw_replay_start_offset, 2);
        assert_eq!(snapshot.raw_replay_end_offset, 10);
        assert_eq!(
            state.raw_replay.bytes(),
            &[b'l', b'l', b'o', b' ', 0x00, 0xff, b'a', b'b']
        );
    }

    #[test]
    fn terminal_snapshot_persists_replay_metadata_and_screen_state() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot_path = temp.path().join("terminal.snapshot.json");
        let replay_path = temp.path().join("pty.replay");
        let mut state = TerminalStateBuffer::new(5, 20, 64, 0);

        state.process_output(b"ready\r\n\x1b[?1049h\x1b[2J\x1b[Hagent screen");
        state.persist(&snapshot_path, &replay_path).unwrap();

        let snapshot: TerminalSnapshot = read_json(&snapshot_path).unwrap();
        assert_eq!((snapshot.rows, snapshot.cols), (5, 20));
        assert!(snapshot.alternate_screen);
        assert!(snapshot
            .screen
            .iter()
            .any(|line| line.contains("agent screen")));
        assert_eq!(
            fs::read(&replay_path).unwrap().len() as u64,
            snapshot.pty_log_offset
        );

        let restored =
            restore_terminal_replay(&snapshot_path, &replay_path, snapshot.pty_log_offset)
                .unwrap()
                .expect("fresh replay");
        assert_eq!(restored.snapshot, snapshot);
        assert_eq!(restored.bytes, fs::read(&replay_path).unwrap());

        let lightweight =
            restore_terminal_replay_bytes(&snapshot_path, &replay_path, snapshot.pty_log_offset)
                .unwrap()
                .expect("fresh lightweight replay");
        assert_eq!(
            (lightweight.metadata.rows, lightweight.metadata.cols),
            (5, 20)
        );
        assert_eq!(lightweight.metadata.pty_log_offset, snapshot.pty_log_offset);
        assert_eq!(
            lightweight.metadata.raw_replay_start_offset,
            snapshot.raw_replay_start_offset
        );
        assert_eq!(
            lightweight.metadata.raw_replay_end_offset,
            snapshot.raw_replay_end_offset
        );
        assert_eq!(lightweight.bytes, fs::read(&replay_path).unwrap());
    }

    #[test]
    fn terminal_replay_is_unavailable_when_offset_is_stale() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot_path = temp.path().join("terminal.snapshot.json");
        let replay_path = temp.path().join("pty.replay");
        let mut state = TerminalStateBuffer::new(5, 20, 64, 0);

        state.process_output(b"ready");
        state.persist(&snapshot_path, &replay_path).unwrap();

        assert!(
            restore_terminal_replay(&snapshot_path, &replay_path, 6)
                .unwrap()
                .is_none(),
            "stale replay must fail closed"
        );
    }

    #[test]
    fn terminal_snapshot_records_resize_metadata() {
        let mut state = TerminalStateBuffer::new(5, 20, 64, 0);

        state.resize(9, 30);
        state.process_output(b"resized");
        let snapshot = state.snapshot();

        assert_eq!((snapshot.rows, snapshot.cols), (9, 30));
        assert!(snapshot.same_size(9, 30));
        assert!(!snapshot.same_size(5, 20));
        assert!(snapshot.screen.iter().any(|line| line.contains("resized")));
    }

    #[test]
    fn screen_snapshot_records_wide_and_default_cells() {
        let mut state = TerminalStateBuffer::new(3, 6, 64, 0);

        state.process_output("A\u{754c}".as_bytes());
        let snapshot = state.screen_snapshot();

        assert_eq!((snapshot.rows, snapshot.cols), (3, 6));
        assert_eq!(snapshot.cursor.row, 0);
        assert_eq!(snapshot.cursor.col, 3);
        assert_eq!(snapshot.cursor.visible, Some(true));
        assert_eq!(snapshot.source.pty_log_offset, 4);

        let first = &snapshot.cells[0][0];
        assert_eq!(first.symbol, "A");
        assert_eq!(first.width, 1);
        assert!(!first.continuation);

        let wide = &snapshot.cells[0][1];
        assert_eq!(wide.symbol, "\u{754c}");
        assert_eq!(wide.width, 2);
        assert!(!wide.continuation);

        let continuation = &snapshot.cells[0][2];
        assert_eq!(continuation.symbol, " ");
        assert_eq!(continuation.width, 1);
        assert!(continuation.continuation);

        let blank = &snapshot.cells[0][3];
        assert_eq!(blank, &ScreenCell::blank());
    }
}
