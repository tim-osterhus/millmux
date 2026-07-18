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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parser_checkpoint: Option<TerminalParserCheckpoint>,
    pub screen: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalParserCheckpoint {
    pub replay_offset: u64,
    pub hydration: String,
    #[serde(default)]
    pub continuation: TerminalParserContinuation,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalParserContinuation {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_utf8: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_grapheme: Option<TerminalActiveGrapheme>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen: Option<TerminalScreenContinuation>,
    #[serde(default)]
    pub vte_resume: TerminalVteResumeState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalActiveGrapheme {
    pub row: u16,
    pub col: u16,
    pub text: String,
    pub width: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalScreenContinuation {
    pub main_grid: TerminalGridContinuation,
    pub alternate_grid: TerminalGridContinuation,
    pub attrs: TerminalAttrsContinuation,
    pub saved_attrs: TerminalAttrsContinuation,
    pub modes: u8,
    pub mouse_protocol_mode: u8,
    pub mouse_protocol_encoding: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalGridContinuation {
    pub row: u16,
    pub col: u16,
    pub saved_row: u16,
    pub saved_col: u16,
    pub scroll_top: u16,
    pub scroll_bottom: u16,
    pub origin_mode: bool,
    pub saved_origin_mode: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalAttrsContinuation {
    pub fg: ScreenColor,
    pub bg: ScreenColor,
    pub mode: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalVteResumeState {
    #[serde(default = "terminal_vte_resume_version")]
    pub version: u8,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bytes: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overflow: Option<TerminalVteResumeOverflow>,
}

impl Default for TerminalVteResumeState {
    fn default() -> Self {
        Self {
            version: terminal_vte_resume_version(),
            bytes: Vec::new(),
            overflow: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalVteResumeOverflow {
    Osc,
    Dcs,
    Csi,
    SosPmApc,
}

const fn terminal_vte_resume_version() -> u8 {
    vt100::VTE_RESUME_VERSION
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
    structured_snapshot_frontier: Option<ScreenSnapshot>,
}

impl TerminalStateBuffer {
    pub fn new(rows: u16, cols: u16, raw_replay_capacity: usize, pty_log_offset: u64) -> Self {
        let mut state = Self {
            parser: vt100::Parser::new(rows.max(1), cols.max(1), DEFAULT_SCROLLBACK_CAPACITY),
            raw_replay: RawReplayBuffer::empty(raw_replay_capacity, pty_log_offset),
            pty_log_offset,
            structured_snapshot_frontier: None,
        };
        state.refresh_structured_snapshot_frontier();
        state
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
            state.structured_snapshot_frontier =
                replay
                    .snapshot
                    .structured_screen
                    .clone()
                    .filter(|snapshot| {
                        snapshot.source.pty_log_offset >= state.raw_replay.start_offset()
                            && snapshot.source.pty_log_offset <= current_pty_offset
                    });
            let restored_from_checkpoint = replay
                .snapshot
                .parser_checkpoint
                .as_ref()
                .filter(|checkpoint| {
                    checkpoint.replay_offset >= replay.snapshot.raw_replay_start_offset
                        && checkpoint.replay_offset <= replay.snapshot.raw_replay_end_offset
                })
                .map(|checkpoint| {
                    state.parser.process(checkpoint.hydration.as_bytes());
                    state
                        .parser
                        .restore_continuation(parser_continuation_from_checkpoint(
                            &checkpoint.continuation,
                        ));
                    let suffix_start = usize::try_from(
                        checkpoint
                            .replay_offset
                            .saturating_sub(replay.snapshot.raw_replay_start_offset),
                    )
                    .unwrap_or(usize::MAX)
                    .min(state.raw_replay.bytes().len());
                    state
                        .parser
                        .process(&state.raw_replay.bytes()[suffix_start..]);
                })
                .is_some();
            if !restored_from_checkpoint {
                state.parser.process(state.raw_replay.bytes());
            }
            if parser_state_is_structured_snapshot_safe(&state.parser, &state.screen_snapshot()) {
                state.refresh_structured_snapshot_frontier();
            } else if state
                .structured_snapshot_frontier
                .as_ref()
                .is_some_and(|snapshot| snapshot.source.pty_log_offset == current_pty_offset)
            {
                state.structured_snapshot_frontier = None;
            }
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
        self.refresh_structured_snapshot_frontier();
    }

    pub fn finish_input(&mut self) {
        self.parser.finish();
        self.refresh_structured_snapshot_frontier();
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let resized_frontier = self
            .structured_snapshot_frontier
            .as_ref()
            .map(|snapshot| self.resize_structured_snapshot_frontier(snapshot, rows, cols));
        self.parser.screen_mut().set_size(rows, cols);
        self.structured_snapshot_frontier = resized_frontier;
        self.refresh_structured_snapshot_frontier();
    }

    pub fn same_size(&self, rows: u16, cols: u16) -> bool {
        let (current_rows, current_cols) = self.parser.screen().size();
        current_rows == rows && current_cols == cols
    }

    pub fn pty_offset(&self) -> u64 {
        self.pty_log_offset
    }

    pub fn snapshot(&self) -> TerminalSnapshot {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let (cursor_row, cursor_col) = screen.cursor_position();
        let structured_screen = self.structured_snapshot_frontier.clone();
        let screen_lines = self.screen_snapshot().plain_lines();

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
            structured_screen,
            parser_checkpoint: Some(TerminalParserCheckpoint {
                replay_offset: self.pty_log_offset,
                hydration: String::from_utf8(screen.parser_checkpoint_formatted())
                    .expect("formatted terminal checkpoint is valid UTF-8"),
                continuation: parser_continuation_to_checkpoint(self.parser.continuation()),
            }),
            screen: screen_lines,
        }
    }

    pub fn screen_snapshot(&self) -> ScreenSnapshot {
        screen_snapshot_from_parser(
            &self.parser,
            ScreenSnapshotSource {
                pty_log_offset: self.pty_log_offset,
                raw_replay_start_offset: self.raw_replay.start_offset(),
                raw_replay_end_offset: self.raw_replay.end_offset(),
            },
        )
    }

    fn resize_structured_snapshot_frontier(
        &self,
        snapshot: &ScreenSnapshot,
        rows: u16,
        cols: u16,
    ) -> ScreenSnapshot {
        let mut parser = vt100::Parser::new(snapshot.rows.max(1), snapshot.cols.max(1), 0);
        hydrate_terminal_parser_from_snapshot(&mut parser, snapshot);
        parser.screen_mut().set_size(rows, cols);
        screen_snapshot_from_parser(
            &parser,
            ScreenSnapshotSource {
                pty_log_offset: snapshot.source.pty_log_offset,
                raw_replay_start_offset: self.raw_replay.start_offset(),
                raw_replay_end_offset: self.raw_replay.end_offset(),
            },
        )
    }

    pub fn screen_snapshot_replay(&self) -> Option<(ScreenSnapshot, Vec<u8>, u64)> {
        let mut snapshot = self.structured_snapshot_frontier.clone()?;
        let snapshot_offset = snapshot.source.pty_log_offset;
        if snapshot_offset < self.raw_replay.start_offset()
            || snapshot_offset > self.pty_log_offset
            || self.raw_replay.end_offset() != self.pty_log_offset
        {
            return None;
        }
        let suffix_start =
            usize::try_from(snapshot_offset - self.raw_replay.start_offset()).ok()?;
        snapshot.source.raw_replay_start_offset = self.raw_replay.start_offset();
        snapshot.source.raw_replay_end_offset = self.pty_log_offset;
        Some((
            snapshot,
            self.raw_replay.bytes().get(suffix_start..)?.to_vec(),
            self.pty_log_offset,
        ))
    }

    pub fn raw_replay_through(&self, cutoff: u64) -> Option<(Vec<u8>, u16, u16)> {
        if cutoff < self.raw_replay.start_offset() || cutoff > self.raw_replay.end_offset() {
            return None;
        }
        let end = usize::try_from(cutoff - self.raw_replay.start_offset()).ok()?;
        let (rows, cols) = self.parser.screen().size();
        Some((self.raw_replay.bytes().get(..end)?.to_vec(), rows, cols))
    }

    fn refresh_structured_snapshot_frontier(&mut self) {
        let snapshot = self.screen_snapshot();
        if parser_state_is_structured_snapshot_safe(&self.parser, &snapshot) {
            self.structured_snapshot_frontier = Some(snapshot);
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

fn screen_snapshot_from_parser(
    parser: &vt100::Parser,
    source: ScreenSnapshotSource,
) -> ScreenSnapshot {
    let screen = parser.screen();
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
        source,
        captured_at: now_rfc3339(),
    }
}

fn parser_state_is_structured_snapshot_safe(
    parser: &vt100::Parser,
    snapshot: &ScreenSnapshot,
) -> bool {
    let continuation = parser.continuation();
    if !continuation.pending_utf8.is_empty()
        || !continuation.vte_resume.bytes.is_empty()
        || continuation.vte_resume.overflow.is_some()
    {
        return false;
    }

    let mut represented = vt100::Parser::new(snapshot.rows.max(1), snapshot.cols.max(1), 0);
    hydrate_terminal_parser_from_snapshot(&mut represented, snapshot);
    let represented = represented.continuation();
    let (Some(actual_screen), Some(represented_screen)) =
        (continuation.screen.as_ref(), represented.screen.as_ref())
    else {
        return false;
    };
    let actual_grid = if snapshot.alternate_screen {
        actual_screen.alternate_grid
    } else {
        actual_screen.main_grid
    };
    let represented_grid = if snapshot.alternate_screen {
        represented_screen.alternate_grid
    } else {
        represented_screen.main_grid
    };

    continuation.active_grapheme == represented.active_grapheme
        && actual_grid == represented_grid
        && actual_screen.attrs == represented_screen.attrs
        && actual_screen.saved_attrs == represented_screen.saved_attrs
        && actual_screen.modes == represented_screen.modes
        && actual_screen.mouse_protocol_mode == represented_screen.mouse_protocol_mode
        && actual_screen.mouse_protocol_encoding == represented_screen.mouse_protocol_encoding
}

pub fn hydrate_terminal_parser_from_snapshot(
    parser: &mut vt100::Parser,
    snapshot: &ScreenSnapshot,
) {
    let rows = snapshot.rows.max(1);
    let cols = snapshot.cols.max(1);
    if parser.screen().size() != (rows, cols) {
        parser.screen_mut().set_size(rows, cols);
    }
    let hydration = structured_snapshot_hydration(snapshot, parser.screen().alternate_screen());
    parser.hydrate_screen(&hydration);
    for (row, cells) in snapshot.cells.iter().take(usize::from(rows)).enumerate() {
        for (col, cell) in cells.iter().take(usize::from(cols)).enumerate() {
            if !cell.occupied
                && !cell.continuation
                && (cell.symbol.is_empty() || cell.symbol == " ")
            {
                parser
                    .screen_mut()
                    .clear_cell_contents(row as u16, col as u16);
            }
        }
    }
    let mut continuation = parser.continuation();
    continuation.active_grapheme = structured_snapshot_active_grapheme(snapshot);
    parser.restore_continuation(continuation);
}

fn structured_snapshot_active_grapheme(
    snapshot: &ScreenSnapshot,
) -> Option<vt100::ActiveGraphemeContinuation> {
    let rows = snapshot.rows.max(1);
    let cols = snapshot.cols.max(1);
    let row = snapshot.cursor.row.min(rows.saturating_sub(1));
    let cursor_col = snapshot.cursor.col.min(cols);
    if cursor_col == 0 {
        return None;
    }
    let mut col = cursor_col - 1;
    let cells = snapshot.cells.get(usize::from(row))?;
    if cells.get(usize::from(col))?.continuation {
        col = col.checked_sub(1)?;
    }
    let cell = cells.get(usize::from(col))?;
    if !cell.occupied
        || cell.continuation
        || cell.symbol.is_empty()
        || col.saturating_add(u16::from(cell.width.max(1))) != cursor_col
    {
        return None;
    }
    Some(vt100::ActiveGraphemeContinuation {
        row,
        col,
        text: cell.symbol.clone(),
        width: u16::from(cell.width.max(1)),
    })
}

fn structured_snapshot_hydration(snapshot: &ScreenSnapshot, alternate_screen: bool) -> Vec<u8> {
    let rows = snapshot.rows.max(1);
    let cols = snapshot.cols.max(1);
    let active_cell = structured_snapshot_active_grapheme(snapshot).and_then(|active| {
        snapshot
            .cells
            .get(usize::from(active.row))?
            .get(usize::from(active.col))
    });
    let mut bytes = Vec::new();
    if snapshot.alternate_screen != alternate_screen {
        if let Some(cell) = active_cell {
            push_screen_cell_attrs(&mut bytes, cell);
        }
        bytes.extend_from_slice(if snapshot.alternate_screen {
            b"\x1b[?1049h"
        } else {
            b"\x1b[?1049l"
        });
    }
    bytes.extend_from_slice(b"\x1b[0m\x1b[2J\x1b[H");
    for (row_index, row) in snapshot.cells.iter().take(usize::from(rows)).enumerate() {
        for (col_index, cell) in row.iter().take(usize::from(cols)).enumerate() {
            if cell.continuation || !screen_cell_needs_hydration(cell) {
                continue;
            }
            push_cursor_position(&mut bytes, row_index, col_index);
            push_screen_cell(&mut bytes, cell);
        }
    }
    bytes.extend_from_slice(b"\x1b[0m");
    let cursor_row = usize::from(snapshot.cursor.row.min(rows.saturating_sub(1)));
    let cursor_col = snapshot.cursor.col.min(cols);
    if cursor_col < cols {
        push_cursor_position(&mut bytes, cursor_row, usize::from(cursor_col));
    } else {
        let row = snapshot.cells.get(cursor_row);
        let last_col = usize::from(cols.saturating_sub(1));
        let start_col = row
            .and_then(|row| row.get(last_col))
            .filter(|cell| cell.continuation)
            .map_or(last_col, |_| last_col.saturating_sub(1));
        push_cursor_position(&mut bytes, cursor_row, start_col);
        if let Some(cell) = row.and_then(|row| row.get(start_col)) {
            push_screen_cell(&mut bytes, cell);
        } else {
            bytes.push(b' ');
        }
    }
    if let Some(cell) = active_cell {
        push_screen_cell_attrs(&mut bytes, cell);
    }
    bytes.extend_from_slice(if snapshot.cursor.visible.unwrap_or(true) {
        b"\x1b[?25h"
    } else {
        b"\x1b[?25l"
    });
    bytes
}

fn screen_cell_needs_hydration(cell: &ScreenCell) -> bool {
    cell.occupied
        || (!cell.symbol.is_empty() && cell.symbol != " ")
        || !matches!(cell.fg, ScreenColor::Default)
        || !matches!(cell.bg, ScreenColor::Default)
        || cell.style != ScreenStyle::default()
}

fn push_cursor_position(bytes: &mut Vec<u8>, row: usize, col: usize) {
    bytes.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
}

fn push_screen_cell(bytes: &mut Vec<u8>, cell: &ScreenCell) {
    push_screen_cell_attrs(bytes, cell);
    if cell.symbol.is_empty() {
        bytes.push(b' ');
    } else {
        bytes.extend_from_slice(cell.symbol.as_bytes());
    }
}

fn push_screen_cell_attrs(bytes: &mut Vec<u8>, cell: &ScreenCell) {
    bytes.extend_from_slice(b"\x1b[0m");
    let mut codes = Vec::new();
    if cell.style.bold {
        codes.push("1".to_string());
    }
    if cell.style.dim {
        codes.push("2".to_string());
    }
    if cell.style.italic {
        codes.push("3".to_string());
    }
    if cell.style.underline {
        codes.push("4".to_string());
    }
    if cell.style.inverse {
        codes.push("7".to_string());
    }
    push_screen_color_code(&mut codes, &cell.fg, true);
    push_screen_color_code(&mut codes, &cell.bg, false);
    if !codes.is_empty() {
        bytes.extend_from_slice(format!("\x1b[{}m", codes.join(";")).as_bytes());
    }
}

fn push_screen_color_code(codes: &mut Vec<String>, color: &ScreenColor, foreground: bool) {
    let base = if foreground { 38 } else { 48 };
    match color {
        ScreenColor::Default => {}
        ScreenColor::Indexed { index } => codes.push(format!("{base};5;{index}")),
        ScreenColor::Rgb { r, g, b } => codes.push(format!("{base};2;{r};{g};{b}")),
    }
}

fn parser_continuation_to_checkpoint(
    continuation: vt100::ParserContinuation,
) -> TerminalParserContinuation {
    TerminalParserContinuation {
        pending_utf8: continuation.pending_utf8,
        active_grapheme: continuation
            .active_grapheme
            .map(|grapheme| TerminalActiveGrapheme {
                row: grapheme.row,
                col: grapheme.col,
                text: grapheme.text,
                width: grapheme.width,
            }),
        screen: continuation.screen.map(screen_continuation_to_checkpoint),
        vte_resume: TerminalVteResumeState {
            version: continuation.vte_resume.version,
            bytes: continuation.vte_resume.bytes,
            overflow: continuation
                .vte_resume
                .overflow
                .map(vte_resume_overflow_to_checkpoint),
        },
    }
}

fn parser_continuation_from_checkpoint(
    continuation: &TerminalParserContinuation,
) -> vt100::ParserContinuation {
    vt100::ParserContinuation {
        pending_utf8: continuation.pending_utf8.clone(),
        active_grapheme: continuation.active_grapheme.as_ref().map(|grapheme| {
            vt100::ActiveGraphemeContinuation {
                row: grapheme.row,
                col: grapheme.col,
                text: grapheme.text.clone(),
                width: grapheme.width,
            }
        }),
        screen: continuation
            .screen
            .as_ref()
            .map(screen_continuation_from_checkpoint),
        vte_resume: vt100::VteResumeState {
            version: continuation.vte_resume.version,
            bytes: continuation
                .vte_resume
                .bytes
                .iter()
                .copied()
                .take(vt100::MAX_VTE_RESUME_BYTES)
                .collect(),
            overflow: continuation
                .vte_resume
                .overflow
                .map(vte_resume_overflow_from_checkpoint),
        },
    }
}

fn screen_continuation_to_checkpoint(
    continuation: vt100::ScreenContinuation,
) -> TerminalScreenContinuation {
    TerminalScreenContinuation {
        main_grid: grid_continuation_to_checkpoint(continuation.main_grid),
        alternate_grid: grid_continuation_to_checkpoint(continuation.alternate_grid),
        attrs: attrs_continuation_to_checkpoint(continuation.attrs),
        saved_attrs: attrs_continuation_to_checkpoint(continuation.saved_attrs),
        modes: continuation.modes,
        mouse_protocol_mode: continuation.mouse_protocol_mode,
        mouse_protocol_encoding: continuation.mouse_protocol_encoding,
    }
}

fn screen_continuation_from_checkpoint(
    continuation: &TerminalScreenContinuation,
) -> vt100::ScreenContinuation {
    vt100::ScreenContinuation {
        main_grid: grid_continuation_from_checkpoint(&continuation.main_grid),
        alternate_grid: grid_continuation_from_checkpoint(&continuation.alternate_grid),
        attrs: attrs_continuation_from_checkpoint(&continuation.attrs),
        saved_attrs: attrs_continuation_from_checkpoint(&continuation.saved_attrs),
        modes: continuation.modes,
        mouse_protocol_mode: continuation.mouse_protocol_mode,
        mouse_protocol_encoding: continuation.mouse_protocol_encoding,
    }
}

fn grid_continuation_to_checkpoint(
    continuation: vt100::GridContinuation,
) -> TerminalGridContinuation {
    TerminalGridContinuation {
        row: continuation.pos.row,
        col: continuation.pos.col,
        saved_row: continuation.saved_pos.row,
        saved_col: continuation.saved_pos.col,
        scroll_top: continuation.scroll_top,
        scroll_bottom: continuation.scroll_bottom,
        origin_mode: continuation.origin_mode,
        saved_origin_mode: continuation.saved_origin_mode,
    }
}

fn grid_continuation_from_checkpoint(
    continuation: &TerminalGridContinuation,
) -> vt100::GridContinuation {
    vt100::GridContinuation {
        pos: vt100::Pos {
            row: continuation.row,
            col: continuation.col,
        },
        saved_pos: vt100::Pos {
            row: continuation.saved_row,
            col: continuation.saved_col,
        },
        scroll_top: continuation.scroll_top,
        scroll_bottom: continuation.scroll_bottom,
        origin_mode: continuation.origin_mode,
        saved_origin_mode: continuation.saved_origin_mode,
    }
}

fn attrs_continuation_to_checkpoint(
    continuation: vt100::AttrsContinuation,
) -> TerminalAttrsContinuation {
    TerminalAttrsContinuation {
        fg: screen_color_from_vt100(continuation.fgcolor),
        bg: screen_color_from_vt100(continuation.bgcolor),
        mode: continuation.mode,
    }
}

fn attrs_continuation_from_checkpoint(
    continuation: &TerminalAttrsContinuation,
) -> vt100::AttrsContinuation {
    vt100::AttrsContinuation {
        fgcolor: vt100_color_from_screen(&continuation.fg),
        bgcolor: vt100_color_from_screen(&continuation.bg),
        mode: continuation.mode,
    }
}

fn vte_resume_overflow_to_checkpoint(
    overflow: vt100::VteResumeOverflow,
) -> TerminalVteResumeOverflow {
    match overflow {
        vt100::VteResumeOverflow::Osc => TerminalVteResumeOverflow::Osc,
        vt100::VteResumeOverflow::Dcs => TerminalVteResumeOverflow::Dcs,
        vt100::VteResumeOverflow::Csi => TerminalVteResumeOverflow::Csi,
        vt100::VteResumeOverflow::SosPmApc => TerminalVteResumeOverflow::SosPmApc,
    }
}

fn vte_resume_overflow_from_checkpoint(
    overflow: TerminalVteResumeOverflow,
) -> vt100::VteResumeOverflow {
    match overflow {
        TerminalVteResumeOverflow::Osc => vt100::VteResumeOverflow::Osc,
        TerminalVteResumeOverflow::Dcs => vt100::VteResumeOverflow::Dcs,
        TerminalVteResumeOverflow::Csi => vt100::VteResumeOverflow::Csi,
        TerminalVteResumeOverflow::SosPmApc => vt100::VteResumeOverflow::SosPmApc,
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
        occupied: cell.is_occupied(),
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

fn vt100_color_from_screen(color: &ScreenColor) -> vt100::Color {
    match color {
        ScreenColor::Default => vt100::Color::Default,
        ScreenColor::Indexed { index } => vt100::Color::Idx(*index),
        ScreenColor::Rgb { r, g, b } => vt100::Color::Rgb(*r, *g, *b),
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
    fn retained_replay_over_one_megabyte_restores_from_the_structured_parser_checkpoint() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot_path = temp.path().join("terminal.snapshot.json");
        let replay_path = temp.path().join("pty.replay");
        let mut state = TerminalStateBuffer::new(4, 30, DEFAULT_RAW_REPLAY_CAPACITY_BYTES, 0);

        state.process_output(b"main-checkpoint");
        state.process_output(b"\x1b[?1049h\x1b[2J\x1b[Halt-checkpoint");
        state.process_output(&vec![b'x'; DEFAULT_RAW_REPLAY_CAPACITY_BYTES + 257]);
        state.process_output("\x1b[2J\x1b[Hfinal-width:\u{754c}\x1b[3;7H".as_bytes());
        state.persist(&snapshot_path, &replay_path).unwrap();

        let snapshot: TerminalSnapshot = read_json(&snapshot_path).unwrap();
        assert!(snapshot.alternate_screen);
        assert_eq!(
            fs::metadata(&replay_path).unwrap().len() as usize,
            DEFAULT_RAW_REPLAY_CAPACITY_BYTES
        );
        assert_eq!(
            snapshot.raw_replay_end_offset - snapshot.raw_replay_start_offset,
            DEFAULT_RAW_REPLAY_CAPACITY_BYTES as u64
        );
        assert!(snapshot
            .structured_screen
            .as_ref()
            .expect("structured screen")
            .plain_lines()
            .iter()
            .any(|line| line.contains("final-width:\u{754c}")));

        let mut restored = TerminalStateBuffer::restore_or_new(
            &snapshot_path,
            &replay_path,
            snapshot.pty_log_offset,
            24,
            80,
            DEFAULT_RAW_REPLAY_CAPACITY_BYTES,
        )
        .unwrap();
        let alternate = restored.screen_snapshot();
        assert_eq!((alternate.rows, alternate.cols), (4, 30));
        assert!(alternate.alternate_screen);
        assert_eq!((alternate.cursor.row, alternate.cursor.col), (2, 6));
        assert_eq!(alternate.cells[0][12].symbol, "\u{754c}");
        assert_eq!(alternate.cells[0][12].width, 2);
        assert!(alternate.cells[0][13].continuation);

        restored.process_output(b"\x1b[?1049l");
        let main = restored.screen_snapshot();
        assert!(!main.alternate_screen);
        assert!(main
            .plain_lines()
            .iter()
            .any(|line| line.contains("main-checkpoint")));

        restored.process_output(b"\x1b[?47h\r\nlive-after-restore");
        let continued = restored.screen_snapshot();
        assert!(continued.alternate_screen);
        let continued_text = continued.plain_lines().join("\n");
        assert!(continued_text.contains("final-width:\u{754c}"));
        assert!(continued_text.contains("live-after-restore"));
    }

    #[test]
    fn checkpoint_restart_matches_split_csi_and_osc_continuations() {
        checkpoint_restart_matches(b"before\x1b[2;", b"3Hcsi-complete", Some(b"\x1b[2;"));
        checkpoint_restart_matches(
            b"before\x1b]2;partial-title",
            b"\x07osc-complete",
            Some(b"\x1b]2;partial-title"),
        );
    }

    #[test]
    fn checkpoint_restart_preserves_split_osc_and_dcs_string_terminators() {
        checkpoint_restart_matches(
            b"before\x1b]2;partial-title\x1b",
            b"\\osc-complete",
            Some(b"\x1b"),
        );
        checkpoint_restart_matches(
            b"before\x1bPqpartial-data\x1b",
            b"\\dcs-complete",
            Some(b"\x1b"),
        );
    }

    #[test]
    fn checkpoint_restart_preserves_saved_cursor_scroll_region_origin_and_attrs() {
        checkpoint_restart_matches(
            b"\x1b[2;5r\x1b[?6h\x1b[31m\x1b[2;2Hmain\x1b7\x1b[?1049h\x1b[2;4r\x1b[?6h\x1b[32m\x1b[1;2Halt\x1b7",
            b"\x1b8A\x1b[?1049lM",
            None,
        );
    }

    #[test]
    fn checkpointed_oversize_osc_and_dcs_preserve_split_string_terminators() {
        for (prefix, expected) in [
            (b"\x1b]2;".as_slice(), TerminalVteResumeOverflow::Osc),
            (b"\x1bPq".as_slice(), TerminalVteResumeOverflow::Dcs),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let snapshot_path = temp.path().join("terminal.snapshot.json");
            let replay_path = temp.path().join("pty.replay");
            let mut state = TerminalStateBuffer::new(3, 40, vt100::MAX_VTE_RESUME_BYTES + 64, 0);
            let mut partial = prefix.to_vec();
            partial.extend(std::iter::repeat(b'x').take(vt100::MAX_VTE_RESUME_BYTES + 1));
            state.process_output(&partial);
            state.persist(&snapshot_path, &replay_path).unwrap();

            let checkpoint: TerminalSnapshot = read_json(&snapshot_path).unwrap();
            let resume = &checkpoint
                .parser_checkpoint
                .as_ref()
                .expect("parser checkpoint")
                .continuation
                .vte_resume;
            assert!(resume.bytes.is_empty());
            assert_eq!(resume.overflow, Some(expected));

            let mut restored = TerminalStateBuffer::restore_or_new(
                &snapshot_path,
                &replay_path,
                partial.len() as u64,
                3,
                40,
                vt100::MAX_VTE_RESUME_BYTES + 64,
            )
            .unwrap();
            restored.process_output(b"payload-must-not-render");
            restored.process_output(b"\x1b");
            restored.persist(&snapshot_path, &replay_path).unwrap();

            let split_checkpoint: TerminalSnapshot = read_json(&snapshot_path).unwrap();
            let split_resume = &split_checkpoint
                .parser_checkpoint
                .as_ref()
                .expect("split parser checkpoint")
                .continuation
                .vte_resume;
            assert_eq!(split_resume.bytes.as_slice(), b"\x1b".as_slice());
            assert!(split_resume.overflow.is_none());

            let split_offset = partial.len() + b"payload-must-not-render".len() + 1;
            let mut restored = TerminalStateBuffer::restore_or_new(
                &snapshot_path,
                &replay_path,
                split_offset as u64,
                3,
                40,
                vt100::MAX_VTE_RESUME_BYTES + 64,
            )
            .unwrap();
            restored.process_output(b"\\visible-after-terminator");
            let lines = restored.screen_snapshot().plain_lines().join("\n");
            assert!(!lines.contains("payload-must-not-render"), "{lines:?}");
            assert!(lines.contains("visible-after-terminator"), "{lines:?}");
        }
    }

    #[test]
    fn checkpointed_oversize_ignored_strings_remain_suppressed_until_termination() {
        for prefix in [
            b"\x1bX".as_slice(),
            b"\x1b^".as_slice(),
            b"\x1b_".as_slice(),
        ] {
            let mut partial = b"before".to_vec();
            partial.extend_from_slice(prefix);
            partial.extend(std::iter::repeat(b'x').take(vt100::MAX_VTE_RESUME_BYTES + 1));
            for post in [
                b"payload\x1b\\after-st".as_slice(),
                b"payload\x18after-can".as_slice(),
                b"payload\x1aafter-sub".as_slice(),
            ] {
                checkpoint_restart_matches(&partial, post, None);
            }
        }
    }

    #[test]
    fn checkpointed_oversize_dcs_cancellation_matches_continuous_parser() {
        let mut partial = b"before\x1bP".to_vec();
        partial.extend(std::iter::repeat(b'x').take(vt100::MAX_VTE_RESUME_BYTES + 1));
        for post in [
            b"\x18after-can".as_slice(),
            b"\x1aafter-sub".as_slice(),
            b"\x1b[2Jafter-escape".as_slice(),
        ] {
            checkpoint_restart_matches(&partial, post, None);
        }
    }

    #[test]
    fn structured_frontier_keeps_non_rendered_screen_state_in_raw_suffix() {
        for state_change in [
            b"\x1b[31m".as_slice(),
            b"\x1b[2;5r\x1b[4;7H\x1b7".as_slice(),
        ] {
            let mut state = TerminalStateBuffer::new(6, 20, 256, 0);
            state.process_output(state_change);

            let (snapshot, suffix, covered_offset) = state
                .screen_snapshot_replay()
                .expect("non-rendered state remains replayable");
            assert_eq!(snapshot.source.pty_log_offset, 0);
            assert_eq!(snapshot.source.raw_replay_end_offset, covered_offset);
            assert_eq!(suffix, state_change);
            assert_eq!(covered_offset, state_change.len() as u64);
        }
    }

    #[test]
    fn resize_preserves_unsafe_state_change_as_suffix_at_resized_frontier() {
        let state_change = b"\x1b[2;5r\x1b[4;7H\x1b7";
        let mut state = TerminalStateBuffer::new(6, 20, 256, 0);
        state.process_output(state_change);
        state.resize(37, 121);

        let (snapshot, suffix, covered_offset) = state
            .screen_snapshot_replay()
            .expect("resized safe frontier must retain its raw suffix");
        assert_eq!((snapshot.rows, snapshot.cols), (37, 121));
        assert_eq!(snapshot.source.pty_log_offset, 0);
        assert_eq!(suffix, state_change);
        assert_eq!(covered_offset, suffix.len() as u64);

        let mut replayed = vt100::Parser::new(37, 121, 0);
        hydrate_terminal_parser_from_snapshot(&mut replayed, &snapshot);
        replayed.process(&suffix);
        assert_eq!(
            replayed.screen().contents(),
            state.parser.screen().contents()
        );
        assert_eq!(replayed.continuation(), state.parser.continuation());
    }

    #[test]
    fn resize_does_not_relabel_unsafe_raw_suffix_with_new_dimensions() {
        let output = b"\x1b[2;2H\x1b7\x1b[H12345X";
        let mut state = TerminalStateBuffer::new(3, 5, 256, 0);
        state.process_output(output);
        state.resize(3, 10);

        let (snapshot, suffix, _) = state
            .screen_snapshot_replay()
            .expect("unsafe suffix remains replayable at its original size");
        assert_eq!((snapshot.rows, snapshot.cols), (3, 5));
        assert_eq!(suffix, output);

        let mut replayed = vt100::Parser::new(snapshot.rows, snapshot.cols, 0);
        hydrate_terminal_parser_from_snapshot(&mut replayed, &snapshot);
        replayed.process(&suffix);
        replayed.screen_mut().set_size(3, 10);
        assert_eq!(
            replayed.screen().contents(),
            state.parser.screen().contents()
        );
        assert_eq!(replayed.continuation(), state.parser.continuation());
    }

    #[test]
    fn printable_ending_frontier_advances_beyond_the_raw_replay_capacity() {
        let mut state = TerminalStateBuffer::new(4, 80, 1024, 0);
        let output = vec![b'x'; 2048];
        state.process_output(&output);

        let (snapshot, suffix, covered_offset) = state
            .screen_snapshot_replay()
            .expect("printable frontier must not be stranded outside replay");
        assert_eq!(snapshot.source.pty_log_offset, output.len() as u64);
        assert!(suffix.is_empty());
        assert_eq!(covered_offset, output.len() as u64);
    }

    #[test]
    fn alternate_screen_printable_fixture_is_a_complete_structured_frontier() {
        let output = b"\x1b[31m\x1b[?1049h\x1b[2J\x1b[HALT_READY";
        let mut state = TerminalStateBuffer::new(24, 80, 1024, 0);
        state.process_output(output);

        let (snapshot, suffix, covered_offset) = state
            .screen_snapshot_replay()
            .expect("alternate-screen printable output is fully represented");
        assert_eq!(snapshot.source.pty_log_offset, output.len() as u64);
        assert!(suffix.is_empty());
        assert_eq!(covered_offset, output.len() as u64);
    }

    fn checkpoint_restart_matches(pre: &[u8], post: &[u8], expected_vte_tail: Option<&[u8]>) {
        let temp = tempfile::tempdir().unwrap();
        let snapshot_path = temp.path().join("terminal.snapshot.json");
        let replay_path = temp.path().join("pty.replay");
        let mut continuous = TerminalStateBuffer::new(6, 40, 8192, 0);
        let mut checkpointed = TerminalStateBuffer::new(6, 40, 8192, 0);

        continuous.process_output(pre);
        continuous.process_output(post);
        checkpointed.process_output(pre);
        checkpointed.persist(&snapshot_path, &replay_path).unwrap();

        let checkpoint: TerminalSnapshot = read_json(&snapshot_path).unwrap();
        let continuation = &checkpoint
            .parser_checkpoint
            .as_ref()
            .expect("parser checkpoint")
            .continuation;
        assert!(continuation.screen.is_some());
        if let Some(expected_vte_tail) = expected_vte_tail {
            assert_eq!(continuation.vte_resume.bytes, expected_vte_tail);
        }

        let mut restored = TerminalStateBuffer::restore_or_new(
            &snapshot_path,
            &replay_path,
            pre.len() as u64,
            6,
            40,
            8192,
        )
        .unwrap();
        restored.process_output(post);
        assert_terminal_screens_match(&continuous.screen_snapshot(), &restored.screen_snapshot());
    }

    fn assert_terminal_screens_match(expected: &ScreenSnapshot, actual: &ScreenSnapshot) {
        assert_eq!(actual.rows, expected.rows);
        assert_eq!(actual.cols, expected.cols);
        assert_eq!(actual.cursor, expected.cursor);
        assert_eq!(actual.alternate_screen, expected.alternate_screen);
        assert_eq!(actual.cells, expected.cells);
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
    fn restored_terminal_replay_finalizes_checkpointed_incomplete_utf8_once() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot_path = temp.path().join("terminal.snapshot.json");
        let replay_path = temp.path().join("pty.replay");
        let mut state = TerminalStateBuffer::new(2, 8, 64, 0);

        state.process_output(&[0xf0, 0x9f]);
        state.persist(&snapshot_path, &replay_path).unwrap();

        let snapshot: TerminalSnapshot = read_json(&snapshot_path).unwrap();
        assert_eq!(
            snapshot
                .parser_checkpoint
                .as_ref()
                .expect("parser checkpoint")
                .continuation
                .pending_utf8,
            vec![0xf0, 0x9f]
        );

        let mut restored =
            TerminalStateBuffer::restore_or_new(&snapshot_path, &replay_path, 2, 2, 8, 64).unwrap();
        assert!(restored
            .screen_snapshot()
            .cells
            .iter()
            .flatten()
            .all(|cell| cell.symbol != "\u{fffd}"));
        restored.finish_input();
        let replacement_count = restored
            .screen_snapshot()
            .cells
            .iter()
            .flatten()
            .filter(|cell| cell.symbol == "\u{fffd}")
            .count();
        assert_eq!(replacement_count, 1);
    }

    #[test]
    fn restored_terminal_replay_preserves_grapheme_continuations() {
        let temp = tempfile::tempdir().unwrap();

        let combining_snapshot = temp.path().join("combining.snapshot.json");
        let combining_replay = temp.path().join("combining.replay");
        let mut combining = TerminalStateBuffer::new(2, 8, 64, 0);
        combining.process_output(b"e");
        combining
            .persist(&combining_snapshot, &combining_replay)
            .unwrap();
        let mut combining = TerminalStateBuffer::restore_or_new(
            &combining_snapshot,
            &combining_replay,
            1,
            2,
            8,
            64,
        )
        .unwrap();
        combining.process_output("\u{301}".as_bytes());
        assert_eq!(combining.screen_snapshot().cells[0][0].symbol, "e\u{301}");

        let emoji_snapshot = temp.path().join("emoji.snapshot.json");
        let emoji_replay = temp.path().join("emoji.replay");
        let mut emoji = TerminalStateBuffer::new(2, 8, 64, 0);
        emoji.process_output("\u{2764}".as_bytes());
        emoji.persist(&emoji_snapshot, &emoji_replay).unwrap();
        let mut emoji =
            TerminalStateBuffer::restore_or_new(&emoji_snapshot, &emoji_replay, 3, 2, 8, 64)
                .unwrap();
        emoji.process_output("\u{fe0f}\u{200d}\u{1f525}".as_bytes());
        assert_eq!(
            emoji.screen_snapshot().cells[0][0].symbol,
            "\u{2764}\u{fe0f}\u{200d}\u{1f525}"
        );
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
