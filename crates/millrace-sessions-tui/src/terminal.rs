use crate::width::{cell_symbol_width, normalize_cell_symbol};
use millrace_sessions_core::{
    protocol::ScreenSnapshot, scrollback::hydrate_terminal_parser_from_snapshot,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalSearchDirection {
    First,
    Next,
    Previous,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSearchMatch {
    pub physical_row: usize,
    pub scrollback: usize,
    pub row: usize,
    pub occurrence: usize,
    pub start_cell: usize,
    pub end_cell: usize,
    pub query: String,
    pub line: String,
    pub matched_text: String,
}

pub struct TerminalEmulator {
    parser: vt100::Parser,
    scrollback_len: usize,
    output_revision: u64,
    active_search: Option<TerminalSearchState>,
    adopted_cursor_boundary: bool,
}

#[derive(Debug, Clone)]
struct TerminalSearchState {
    query: String,
    revision: u64,
    matches: Vec<TerminalSearchMatch>,
    cursor: usize,
}

struct PhysicalHistoryRow {
    physical_row: usize,
    scrollback: usize,
    viewport_row: usize,
    cells: Vec<TerminalCell>,
}

impl TerminalEmulator {
    pub fn new(rows: u16, cols: u16, scrollback_len: usize) -> Self {
        Self {
            parser: vt100::Parser::new(rows.max(1), cols.max(1), scrollback_len),
            scrollback_len,
            output_revision: 0,
            active_search: None,
            adopted_cursor_boundary: false,
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.parser.process(bytes);
        self.adopted_cursor_boundary = false;
        self.invalidate_output_dependent_state();
    }

    pub fn process_text(&mut self, text: &str) {
        self.process(text.as_bytes());
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let size = (rows.max(1), cols.max(1));
        if self.parser.screen().size() != size {
            let (cursor_row, cursor_col) = self.parser.screen().cursor_position();
            self.parser.screen_mut().set_size(size.0, size.1);
            if self.adopted_cursor_boundary {
                restore_parser_cursor_after_resize(
                    &mut self.parser,
                    cursor_row.min(size.0.saturating_sub(1)),
                    cursor_col.min(size.1),
                );
            }
            self.invalidate_output_dependent_state();
        }
    }

    pub fn finish_input(&mut self) {
        self.adopted_cursor_boundary = false;
        self.parser.finish();
        self.invalidate_output_dependent_state();
    }

    pub fn scroll_up(&mut self, rows: usize) {
        let offset = self.parser.screen().scrollback().saturating_add(rows);
        self.parser
            .screen_viewport_mut()
            .set_scrollback_viewport(offset);
    }

    pub fn scroll_down(&mut self, rows: usize) {
        let offset = self.parser.screen().scrollback().saturating_sub(rows);
        self.parser
            .screen_viewport_mut()
            .set_scrollback_viewport(offset);
    }

    pub fn page_up(&mut self, rows: u16) {
        self.scroll_up(usize::from(rows).max(1));
    }

    pub fn page_down(&mut self, rows: u16) {
        self.scroll_down(usize::from(rows).max(1));
    }

    pub fn jump_top(&mut self) {
        self.parser
            .screen_viewport_mut()
            .set_scrollback_viewport(usize::MAX);
    }

    pub fn jump_bottom(&mut self) {
        self.parser.screen_viewport_mut().set_scrollback_viewport(0);
    }

    pub fn is_scrolled(&self) -> bool {
        self.parser.screen().scrollback() > 0
    }

    pub fn is_following(&self) -> bool {
        !self.is_scrolled()
    }

    pub fn snapshot(&self) -> TerminalSnapshot {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let (cursor_row, cursor_col) = screen.cursor_position();
        let mut cells = Vec::with_capacity(usize::from(rows));

        for row in 0..rows {
            let mut line = Vec::with_capacity(usize::from(cols));
            for col in 0..cols {
                let Some(cell) = screen.cell(row, col) else {
                    line.push(TerminalCell::blank());
                    continue;
                };
                line.push(TerminalCell::from_raw_symbol(
                    cell.contents(),
                    cell.is_occupied(),
                    TerminalColor::from_vt100(cell.fgcolor()),
                    TerminalColor::from_vt100(cell.bgcolor()),
                    TerminalStyle {
                        bold: cell.bold(),
                        dim: cell.dim(),
                        italic: cell.italic(),
                        underline: cell.underline(),
                        inverse: cell.inverse(),
                    },
                ));
            }
            normalize_row_cells(&mut line);
            cells.push(line);
        }

        TerminalSnapshot {
            rows,
            cols,
            cursor_row,
            cursor_col,
            alternate_screen: screen.alternate_screen(),
            cells,
        }
    }

    /// Replaces the active screen from a structured snapshot while retaining
    /// parser-owned physical scrollback for subsequent live output.
    pub fn adopt_screen_snapshot(&mut self, snapshot: &ScreenSnapshot) {
        let legacy_snapshot = snapshot.cells.iter().flatten().any(|cell| {
            !cell.occupied && !cell.continuation && !cell.symbol.is_empty() && cell.symbol != " "
        });
        let normalized_snapshot = legacy_snapshot.then(|| {
            let mut snapshot = snapshot.clone();
            for row in &mut snapshot.cells {
                let occupied_len = row
                    .iter()
                    .rposition(|cell| {
                        cell.continuation || (!cell.symbol.is_empty() && cell.symbol != " ")
                    })
                    .map_or(0, |index| index + 1);
                for cell in row.iter_mut().take(occupied_len) {
                    if !cell.continuation {
                        cell.occupied = true;
                    }
                }
            }
            snapshot
        });
        let snapshot = normalized_snapshot.as_ref().unwrap_or(snapshot);
        let cols = snapshot.cols.max(1);
        hydrate_terminal_parser_from_snapshot(&mut self.parser, snapshot);
        self.adopted_cursor_boundary = snapshot.cursor.col == cols;
        self.invalidate_output_dependent_state();
    }

    pub fn scrollback_len(&self) -> usize {
        self.scrollback_len
    }

    pub fn search_scrollback(
        &mut self,
        query: &str,
        direction: TerminalSearchDirection,
    ) -> Option<TerminalSearchMatch> {
        if query.is_empty() {
            return None;
        }

        let rebuild = match self.active_search.as_ref() {
            Some(search) => search.query != query || search.revision != self.output_revision,
            None => true,
        };
        if rebuild {
            let matches = self.build_search_matches(query);
            if matches.is_empty() {
                self.active_search = None;
                return None;
            }
            let cursor = if direction == TerminalSearchDirection::Previous {
                matches.len() - 1
            } else {
                0
            };
            self.active_search = Some(TerminalSearchState {
                query: query.to_string(),
                revision: self.output_revision,
                matches,
                cursor,
            });
        } else if let Some(search) = &mut self.active_search {
            let len = search.matches.len();
            search.cursor = match direction {
                TerminalSearchDirection::First => 0,
                TerminalSearchDirection::Next => (search.cursor + 1) % len,
                TerminalSearchDirection::Previous => (search.cursor + len - 1) % len,
            };
        }

        let found = self.current_search_match()?;
        self.parser
            .screen_viewport_mut()
            .set_scrollback_viewport(found.scrollback);
        Some(found)
    }

    fn max_scrollback(&mut self) -> usize {
        let original = self.parser.screen().scrollback();
        self.parser
            .screen_viewport_mut()
            .set_scrollback_viewport(usize::MAX);
        let max = self.parser.screen().scrollback();
        self.parser
            .screen_viewport_mut()
            .set_scrollback_viewport(original);
        max
    }

    pub fn current_search_match(&self) -> Option<TerminalSearchMatch> {
        let search = self.active_search.as_ref()?;
        search.matches.get(search.cursor).cloned()
    }

    pub fn clear_search(&mut self) {
        self.active_search = None;
    }

    pub fn search_match_count(&self) -> usize {
        self.active_search
            .as_ref()
            .map_or(0, |search| search.matches.len())
    }

    fn build_search_matches(&mut self, query: &str) -> Vec<TerminalSearchMatch> {
        self.physical_history_rows()
            .into_iter()
            .flat_map(|row| physical_row_matches(row, query))
            .collect()
    }

    fn physical_history_rows(&mut self) -> Vec<PhysicalHistoryRow> {
        let original = self.parser.screen().scrollback();
        let max = self.max_scrollback();
        self.parser
            .screen_viewport_mut()
            .set_scrollback_viewport(max);
        let oldest_view = self.snapshot();
        let viewport_bottom = usize::from(oldest_view.rows).saturating_sub(1);
        let mut physical_rows = oldest_view
            .cells
            .into_iter()
            .enumerate()
            .map(|(row, cells)| PhysicalHistoryRow {
                physical_row: row,
                scrollback: max,
                viewport_row: row,
                cells,
            })
            .collect::<Vec<_>>();

        for scrollback in (0..max).rev() {
            self.parser
                .screen_viewport_mut()
                .set_scrollback_viewport(scrollback);
            let snapshot = self.snapshot();
            if let Some(cells) = snapshot.cells.into_iter().nth(viewport_bottom) {
                physical_rows.push(PhysicalHistoryRow {
                    physical_row: physical_rows.len(),
                    scrollback,
                    viewport_row: viewport_bottom,
                    cells,
                });
            }
        }
        self.parser
            .screen_viewport_mut()
            .set_scrollback_viewport(original);
        physical_rows
    }

    fn invalidate_output_dependent_state(&mut self) {
        self.output_revision = self.output_revision.wrapping_add(1);
        self.active_search = None;
    }
}

impl Default for TerminalEmulator {
    fn default() -> Self {
        Self::new(24, 80, 4000)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSnapshot {
    pub rows: u16,
    pub cols: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub alternate_screen: bool,
    pub cells: Vec<Vec<TerminalCell>>,
}

impl TerminalSnapshot {
    pub fn blank(rows: u16, cols: u16) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Self {
            rows,
            cols,
            cursor_row: 0,
            cursor_col: 0,
            alternate_screen: false,
            cells: vec![vec![TerminalCell::blank(); usize::from(cols)]; usize::from(rows)],
        }
    }

    pub fn plain_lines(&self) -> Vec<String> {
        self.cells
            .iter()
            .map(|row| row_text(row).trim_end().to_string())
            .collect()
    }

    pub fn line_text(&self, row: usize) -> Option<String> {
        self.cells.get(row).map(|row| row_text(row))
    }

    pub fn contains_text(&self, needle: &str) -> bool {
        self.plain_lines().iter().any(|line| line.contains(needle))
    }
}

impl Default for TerminalSnapshot {
    fn default() -> Self {
        Self::blank(24, 80)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalCell {
    pub symbol: String,
    pub width: u8,
    pub continuation: bool,
    pub occupied: bool,
    pub fg: TerminalColor,
    pub bg: TerminalColor,
    pub style: TerminalStyle,
}

impl TerminalCell {
    pub fn new(
        symbol: impl AsRef<str>,
        fg: TerminalColor,
        bg: TerminalColor,
        style: TerminalStyle,
    ) -> Self {
        let raw_symbol = symbol.as_ref();
        let occupied = !raw_symbol.is_empty();
        let symbol = normalize_cell_symbol(raw_symbol);
        Self {
            width: cell_symbol_width(&symbol) as u8,
            continuation: false,
            occupied,
            symbol,
            fg,
            bg,
            style,
        }
    }

    pub fn blank() -> Self {
        Self {
            symbol: " ".to_string(),
            width: 1,
            continuation: false,
            occupied: false,
            fg: TerminalColor::Default,
            bg: TerminalColor::Default,
            style: TerminalStyle::default(),
        }
    }

    pub fn display_symbol(&self) -> &str {
        if self.symbol.is_empty() && !self.continuation {
            " "
        } else {
            self.symbol.as_str()
        }
    }

    fn from_raw_symbol(
        symbol: &str,
        occupied: bool,
        fg: TerminalColor,
        bg: TerminalColor,
        style: TerminalStyle,
    ) -> Self {
        Self {
            symbol: symbol.to_string(),
            width: cell_symbol_width(symbol) as u8,
            continuation: false,
            occupied,
            fg,
            bg,
            style,
        }
    }

    fn mark_continuation(&mut self) {
        self.symbol.clear();
        self.width = 0;
        self.continuation = true;
    }

    fn normalize_blank(&mut self) {
        if self.symbol.is_empty() && !self.continuation {
            self.symbol = " ".to_string();
            self.width = 1;
        }
    }
}

fn normalize_row_cells(row: &mut [TerminalCell]) {
    let len = row.len();
    for index in 0..len {
        let width = usize::from(row[index].width);
        if width <= 1 {
            continue;
        }
        for cell in row
            .iter_mut()
            .take((index + width).min(len))
            .skip(index + 1)
        {
            if cell.symbol.is_empty() {
                cell.mark_continuation();
            }
        }
    }
    for cell in row {
        cell.normalize_blank();
    }
}

fn push_cursor_position(bytes: &mut Vec<u8>, row: usize, col: usize) {
    bytes.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
}

fn restore_parser_cursor_after_resize(parser: &mut vt100::Parser, row: u16, col: u16) {
    let (_, cols) = parser.screen().size();
    let mut bytes = Vec::new();
    if col < cols {
        push_cursor_position(&mut bytes, usize::from(row), usize::from(col));
    } else {
        let last_col = cols.saturating_sub(1);
        let start_col = parser
            .screen()
            .cell(row, last_col)
            .filter(|cell| cell.is_wide_continuation())
            .map_or(last_col, |_| last_col.saturating_sub(1));
        let symbol = parser
            .screen()
            .cell(row, start_col)
            .map(|cell| cell.contents().to_string())
            .filter(|symbol| !symbol.is_empty())
            .unwrap_or_else(|| " ".to_string());
        push_cursor_position(&mut bytes, usize::from(row), usize::from(start_col));
        bytes.extend_from_slice(symbol.as_bytes());
    }
    parser.process(&bytes);
    let mut continuation = parser.continuation();
    continuation.active_grapheme = None;
    parser.restore_continuation(continuation);
}

fn row_text(row: &[TerminalCell]) -> String {
    row.iter()
        .filter(|cell| !cell.continuation)
        .map(TerminalCell::display_symbol)
        .collect()
}

fn physical_row_matches(row: PhysicalHistoryRow, query: &str) -> Vec<TerminalSearchMatch> {
    let (line, boundaries) = row_text_and_boundaries(&row.cells);
    let mut occurrence = 0;
    line.match_indices(query)
        .filter_map(|(start_byte, matched)| {
            let end_byte = start_byte + matched.len();
            let start_cell = boundary_cell(&boundaries, start_byte)?;
            let end_cell = boundary_cell(&boundaries, end_byte)?;
            if row
                .cells
                .iter()
                .take(end_cell)
                .skip(start_cell)
                .any(|cell| !cell.continuation && !cell.occupied)
            {
                return None;
            }
            let found = TerminalSearchMatch {
                physical_row: row.physical_row,
                scrollback: row.scrollback,
                row: row.viewport_row,
                occurrence,
                start_cell,
                end_cell,
                query: query.to_string(),
                line: line.clone(),
                matched_text: line[start_byte..end_byte].to_string(),
            };
            occurrence += 1;
            Some(found)
        })
        .collect()
}

fn row_text_and_boundaries(row: &[TerminalCell]) -> (String, Vec<(usize, usize)>) {
    let mut text = String::new();
    let mut boundaries = vec![(0, 0)];
    let occupied_len = row
        .iter()
        .rposition(|cell| cell.occupied || cell.continuation)
        .map_or(0, |index| index + 1);
    for (column, cell) in row.iter().take(occupied_len).enumerate() {
        if cell.continuation {
            continue;
        }
        text.push_str(cell.display_symbol());
        boundaries.push((text.len(), column + usize::from(cell.width).max(1)));
    }
    (text, boundaries)
}

fn boundary_cell(boundaries: &[(usize, usize)], byte: usize) -> Option<usize> {
    boundaries
        .binary_search_by_key(&byte, |(boundary, _)| *boundary)
        .ok()
        .map(|index| boundaries[index].1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TerminalStyle {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TerminalColor {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl TerminalColor {
    fn from_vt100(color: vt100::Color) -> Self {
        match color {
            vt100::Color::Default => Self::Default,
            vt100::Color::Idx(index) => Self::Indexed(index),
            vt100::Color::Rgb(red, green, blue) => Self::Rgb(red, green, blue),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use millrace_sessions_core::protocol::{
        AttachStreamFrame, ScreenCell, ScreenColor, ScreenCursor, ScreenSnapshotSource, ScreenStyle,
    };
    use millrace_sessions_core::scrollback::TerminalStateBuffer;

    fn structured_snapshot(
        rows: u16,
        cols: u16,
        cursor: (u16, u16),
        alternate_screen: bool,
        lines: &[&str],
    ) -> ScreenSnapshot {
        let mut cells = vec![vec![ScreenCell::blank(); usize::from(cols)]; usize::from(rows)];
        for (row, line) in lines.iter().enumerate().take(usize::from(rows)) {
            for (col, symbol) in line.chars().enumerate().take(usize::from(cols)) {
                cells[row][col] = ScreenCell::default_symbol(symbol.to_string());
            }
        }
        ScreenSnapshot {
            schema_version: 1,
            rows,
            cols,
            cursor: ScreenCursor {
                row: cursor.0,
                col: cursor.1,
                visible: Some(true),
            },
            alternate_screen,
            cells,
            source: ScreenSnapshotSource {
                pty_log_offset: 0,
                raw_replay_start_offset: 0,
                raw_replay_end_offset: 0,
            },
            captured_at: "2026-07-15T00:00:00Z".to_string(),
        }
    }

    fn assert_terminal_matches_structured(actual: &TerminalSnapshot, expected: &ScreenSnapshot) {
        assert_eq!((actual.rows, actual.cols), (expected.rows, expected.cols));
        assert_eq!(
            (actual.cursor_row, actual.cursor_col),
            (expected.cursor.row, expected.cursor.col)
        );
        assert_eq!(actual.alternate_screen, expected.alternate_screen);
        for (actual_row, expected_row) in actual.cells.iter().zip(&expected.cells) {
            for (actual, expected) in actual_row.iter().zip(expected_row) {
                if expected.occupied && !expected.continuation {
                    assert_eq!(actual.symbol, expected.symbol);
                    assert_eq!(actual.width, expected.width);
                }
                assert_eq!(actual.continuation, expected.continuation);
                assert_eq!(actual.occupied, expected.occupied);
            }
        }
    }

    #[test]
    fn structured_snapshot_wrap_pending_cursor_survives_wire_adoption_and_resize() {
        let frame =
            AttachStreamFrame::screen_snapshot(structured_snapshot(2, 4, (0, 4), false, &["abcd"]))
                .expect("wrap-pending snapshot is wire-valid");
        let decoded = AttachStreamFrame::from_json_line(&frame.to_json_line().unwrap()).unwrap();
        let AttachStreamFrame::ScreenSnapshot { snapshot } = decoded else {
            panic!("expected structured snapshot frame");
        };
        let mut terminal = TerminalEmulator::new(2, 4, 8);

        terminal.adopt_screen_snapshot(&snapshot);
        assert_eq!(terminal.snapshot().cursor_col, 4);

        terminal.resize(2, 6);
        assert_eq!(terminal.snapshot().cursor_col, 4);
        terminal.resize(2, 3);
        assert_eq!(terminal.snapshot().cursor_col, 3);
    }

    #[test]
    fn structured_snapshot_hydrates_parser_before_live_output() {
        let snapshot = structured_snapshot(3, 24, (0, 8), false, &["prompt> "]);
        let mut terminal = TerminalEmulator::new(3, 24, 20);

        terminal.adopt_screen_snapshot(&snapshot);
        terminal.process(b"live");

        let current = terminal.snapshot();
        assert_eq!(current.cursor_col, 12);
        assert!(current.line_text(0).unwrap().starts_with("prompt> live"));
    }

    #[test]
    fn structured_snapshot_replay_preserves_split_utf8_and_csi() {
        let encoded = "界".as_bytes();
        let mut authoritative = TerminalStateBuffer::new(2, 8, 128, 0);
        authoritative.process_output(&encoded[..2]);
        let (snapshot, suffix, covered_offset) = authoritative
            .screen_snapshot_replay()
            .expect("split UTF-8 remains covered by raw replay");
        assert_eq!(snapshot.source.pty_log_offset, 0);
        assert_eq!(suffix, encoded[..2]);
        assert_eq!(covered_offset, 2);
        let mut terminal = TerminalEmulator::new(2, 8, 8);
        terminal.adopt_screen_snapshot(&snapshot);
        terminal.process(&suffix);
        authoritative.process_output(&encoded[2..]);
        terminal.process(&encoded[2..]);
        assert_terminal_matches_structured(&terminal.snapshot(), &authoritative.screen_snapshot());

        let mut authoritative = TerminalStateBuffer::new(3, 12, 128, 0);
        authoritative.process_output(b"before\x1b[2;");
        let (snapshot, suffix, _) = authoritative
            .screen_snapshot_replay()
            .expect("split CSI remains covered by raw replay");
        let mut terminal = TerminalEmulator::new(3, 12, 8);
        terminal.adopt_screen_snapshot(&snapshot);
        terminal.process(&suffix);
        authoritative.process_output(b"3Hafter");
        terminal.process(b"3Hafter");
        assert_terminal_matches_structured(&terminal.snapshot(), &authoritative.screen_snapshot());
    }

    #[test]
    fn structured_snapshot_replay_preserves_sgr_saved_cursor_and_scroll_region() {
        let mut authoritative = TerminalStateBuffer::new(6, 20, 256, 0);
        authoritative.process_output(b"\x1b[31m");
        let (snapshot, suffix, _) = authoritative
            .screen_snapshot_replay()
            .expect("SGR state remains replayable");
        let mut terminal = TerminalEmulator::new(6, 20, 8);
        terminal.adopt_screen_snapshot(&snapshot);
        terminal.process(&suffix);
        authoritative.process_output(b"red");
        terminal.process(b"red");
        assert_terminal_matches_structured(&terminal.snapshot(), &authoritative.screen_snapshot());
        assert_eq!(
            terminal.snapshot().cells[0][0].fg,
            TerminalColor::Indexed(1)
        );

        let mut authoritative = TerminalStateBuffer::new(6, 20, 256, 0);
        authoritative.process_output(b"\x1b[2;5r\x1b[4;7H\x1b7\x1b[1;1H");
        let (snapshot, suffix, _) = authoritative
            .screen_snapshot_replay()
            .expect("saved cursor and scroll region remain replayable");
        let mut terminal = TerminalEmulator::new(6, 20, 8);
        terminal.adopt_screen_snapshot(&snapshot);
        terminal.process(&suffix);
        authoritative.process_output(b"\x1b8X");
        terminal.process(b"\x1b8X");
        assert_terminal_matches_structured(&terminal.snapshot(), &authoritative.screen_snapshot());
    }

    #[test]
    fn structured_snapshot_replay_preserves_explicit_grapheme_break() {
        let mut authoritative = TerminalStateBuffer::new(2, 8, 128, 0);
        authoritative.process_output(b"e");
        authoritative.process_output(b"\x1b[5n");
        let (snapshot, suffix, _) = authoritative
            .screen_snapshot_replay()
            .expect("cursor-neutral grapheme break remains covered by raw replay");
        assert_eq!(snapshot.source.pty_log_offset, 1);
        assert_eq!(suffix, b"\x1b[5n");
        let mut terminal = TerminalEmulator::new(2, 8, 8);
        terminal.adopt_screen_snapshot(&snapshot);
        terminal.process(&suffix);

        authoritative.process_output("\u{301}".as_bytes());
        terminal.process("\u{301}".as_bytes());
        assert_terminal_matches_structured(&terminal.snapshot(), &authoritative.screen_snapshot());
        assert_ne!(terminal.snapshot().cells[0][0].symbol, "e\u{301}");
    }

    #[test]
    fn structured_snapshot_replay_restores_grapheme_anchor_for_live_extensions() {
        for (base, suffix, expected, width) in [
            ("e", "\u{301}", "e\u{301}", 1),
            ("\u{2764}", "\u{fe0f}", "\u{2764}\u{fe0f}", 2),
            (
                "\u{1f469}",
                "\u{200d}\u{1f4bb}",
                "\u{1f469}\u{200d}\u{1f4bb}",
                2,
            ),
        ] {
            let mut authoritative = TerminalStateBuffer::new(2, 8, 128, 0);
            authoritative.process_output(base.as_bytes());
            let (snapshot, replay_suffix, _) = authoritative
                .screen_snapshot_replay()
                .expect("active grapheme bytes remain covered by raw replay");
            let mut terminal = TerminalEmulator::new(2, 8, 8);

            terminal.adopt_screen_snapshot(&snapshot);
            terminal.process(&replay_suffix);
            terminal.process(suffix.as_bytes());

            let current = terminal.snapshot();
            assert_eq!(current.cells[0][0].symbol, expected);
            assert_eq!(current.cells[0][0].width, width);
            assert_eq!(current.cursor_col, u16::from(width));
            assert_eq!(current.cells[0][1].continuation, width == 2);
        }
    }

    #[test]
    fn structured_wrap_pending_survives_row_only_resize_before_live_output() {
        let snapshot = structured_snapshot(2, 4, (0, 4), false, &["abcd"]);
        let mut terminal = TerminalEmulator::new(2, 4, 8);
        terminal.adopt_screen_snapshot(&snapshot);

        terminal.resize(3, 4);
        assert_eq!(terminal.snapshot().cursor_col, 4);
        terminal.process(b"e");

        let current = terminal.snapshot();
        assert_eq!(current.line_text(0).as_deref(), Some("abcd"));
        assert_eq!(current.cells[1][0].symbol, "e");
        assert_eq!((current.cursor_row, current.cursor_col), (1, 1));
    }

    #[test]
    fn explicit_grapheme_break_survives_snapshot_and_row_only_resize() {
        let mut authoritative = TerminalStateBuffer::new(2, 4, 128, 0);
        authoritative.process_output(b"abcd");
        authoritative.process_output(b"\x1b[5n");
        let (snapshot, suffix, _) = authoritative
            .screen_snapshot_replay()
            .expect("explicit grapheme break remains replayable");
        let mut terminal = TerminalEmulator::new(2, 4, 8);
        terminal.adopt_screen_snapshot(&snapshot);
        terminal.process(&suffix);

        authoritative.resize(3, 4);
        terminal.resize(3, 4);
        authoritative.process_output("\u{301}".as_bytes());
        terminal.process("\u{301}".as_bytes());

        assert_terminal_matches_structured(&terminal.snapshot(), &authoritative.screen_snapshot());
        assert_ne!(terminal.snapshot().cells[0][3].symbol, "d\u{301}");
    }

    #[test]
    fn structured_snapshot_preserves_styled_erased_cells_without_searchable_spaces() {
        let mut snapshot = structured_snapshot(2, 6, (0, 1), false, &["A C"]);
        snapshot.cells[0][1] = ScreenCell {
            symbol: String::new(),
            occupied: false,
            continuation: false,
            width: 1,
            fg: ScreenColor::Default,
            bg: ScreenColor::Indexed { index: 4 },
            style: ScreenStyle {
                bold: true,
                ..ScreenStyle::default()
            },
        };
        let mut terminal = TerminalEmulator::new(2, 6, 8);

        terminal.adopt_screen_snapshot(&snapshot);

        let restored = terminal.snapshot();
        assert!(!restored.cells[0][1].occupied);
        assert_eq!(restored.cells[0][1].bg, TerminalColor::Indexed(4));
        assert!(restored.cells[0][1].style.bold);
        assert!(terminal
            .search_scrollback(" ", TerminalSearchDirection::First)
            .is_none());
        assert!(terminal
            .search_scrollback("A C", TerminalSearchDirection::First)
            .is_none());
        assert_eq!(
            terminal
                .search_scrollback("C", TerminalSearchDirection::First)
                .expect("occupied text to the right remains searchable")
                .matched_text,
            "C"
        );

        terminal.process(b"B");
        let continued = terminal.snapshot();
        assert!(continued.line_text(0).unwrap().starts_with("ABC"));
        assert_eq!(continued.cursor_col, 2);
        assert_eq!(
            terminal
                .search_scrollback("ABC", TerminalSearchDirection::First)
                .expect("live output makes the formerly erased cell searchable")
                .matched_text,
            "ABC"
        );
    }

    #[test]
    fn default_erased_internal_cell_is_not_searchable_until_live_output_occupies_it() {
        let mut terminal = TerminalEmulator::new(2, 6, 8);
        terminal.process(b"A C\x1b[2D\x1b[X");

        let erased = terminal.snapshot();
        assert!(erased.line_text(0).unwrap().starts_with("A C"));
        assert!(!erased.cells[0][1].occupied);
        assert!(terminal
            .search_scrollback("A C", TerminalSearchDirection::First)
            .is_none());
        assert!(terminal.current_search_match().is_none());

        terminal.process(b"B");
        assert!(terminal.snapshot().line_text(0).unwrap().starts_with("ABC"));
        assert_eq!(
            terminal
                .search_scrollback("ABC", TerminalSearchDirection::First)
                .expect("live output occupies the erased cell")
                .matched_text,
            "ABC"
        );
    }

    #[test]
    fn structured_snapshot_replaces_active_screen_but_preserves_prior_history() {
        let mut terminal = TerminalEmulator::new(3, 24, 20);
        for index in 0..8 {
            terminal.process_text(&format!("history-{index:02}\r\n"));
        }
        terminal.process_text("old-active-only");
        let snapshot = structured_snapshot(3, 24, (0, 18), false, &["returned prompt>  "]);

        terminal.adopt_screen_snapshot(&snapshot);
        terminal.process(b"live");

        assert!(terminal.snapshot().contains_text("returned prompt>  live"));
        assert!(terminal
            .search_scrollback("old-active-only", TerminalSearchDirection::First)
            .is_none());
        let copied = terminal
            .search_scrollback("history-00", TerminalSearchDirection::First)
            .expect("pre-attach history remains searchable and copyable");
        assert_eq!(copied.matched_text, "history-00");
        terminal.page_up(20);
        assert!(terminal.snapshot().contains_text("history-00"));
    }

    #[test]
    fn fresh_structured_hydration_reconstructs_physical_scrollback_from_suffix() {
        let mut durable = TerminalStateBuffer::new(3, 24, 4096, 0);
        for index in 0..8 {
            durable.process_output(format!("history-{index:02}\r\n").as_bytes());
        }
        durable.process_output(b"current prompt> ");
        let (snapshot, suffix, _) = durable
            .screen_snapshot_replay()
            .expect("durable structured replay");

        let mut terminal = TerminalEmulator::new(snapshot.rows, snapshot.cols, 20);
        terminal.adopt_screen_snapshot(&snapshot);
        terminal.process(&suffix);

        let found = terminal
            .search_scrollback("history-00", TerminalSearchDirection::First)
            .expect("fresh hydration retains physical history for search and copy");
        assert_eq!(found.matched_text, "history-00");
        terminal.page_up(20);
        assert!(terminal.snapshot().contains_text("history-00"));
    }

    #[test]
    fn structured_suffix_reconstructs_hidden_main_grid_before_alternate_exit() {
        let mut durable = TerminalStateBuffer::new(4, 32, 4096, 0);
        durable.process_output(b"main prompt> \x1b[?1049h\x1b[2J\x1b[Halternate current");
        let (snapshot, suffix, _) = durable
            .screen_snapshot_replay()
            .expect("alternate structured replay");

        let mut terminal = TerminalEmulator::new(snapshot.rows, snapshot.cols, 20);
        terminal.adopt_screen_snapshot(&snapshot);
        terminal.process(&suffix);
        assert!(terminal.snapshot().contains_text("alternate current"));

        terminal.process(b"\x1b[?1049l");
        assert!(terminal.snapshot().contains_text("main prompt> "));
    }

    #[test]
    fn terminal_fixture_handles_full_screen_agent_protocol_and_resize() {
        let mut terminal = TerminalEmulator::new(8, 48, 200);

        terminal.process(
            concat!(
                "fixture-agent ready\r\n",
                "\x1b[?1049h",
                "\x1b[?2026h",
                "\x1b[2J",
                "\x1b[3J",
                "\x1b[H",
                "question one\r\n",
                "\x1b[4;9Hanswer one complete\r\n",
                "\x1b[2Kanswer two chunk 1",
                "\ranswer two chunk 2",
                "\ranswer two chunk 3\r\n",
                "\x1b[?2026l",
            )
            .as_bytes(),
        );
        let alternate = terminal.snapshot();

        assert!(alternate.alternate_screen);
        assert!(alternate.contains_text("question one"));
        assert!(alternate.contains_text("answer one complete"));
        assert!(alternate.contains_text("answer two chunk 3"));
        assert!(!alternate.contains_text("fixture-agent ready"));

        terminal.resize(12, 64);
        terminal.process(b"resize rows=12 cols=64\r\n\x1b[?1049lanswer two complete\r\n");
        let main = terminal.snapshot();

        assert_eq!((main.rows, main.cols), (12, 64));
        assert!(!main.alternate_screen);
        assert!(main.contains_text("fixture-agent ready"));
        assert!(main.contains_text("answer two complete"));
        assert!(!main.contains_text("answer two chunk 1"));
    }

    #[test]
    fn terminal_internal_scrollback_exposes_prior_visible_rows() {
        let mut terminal = TerminalEmulator::new(4, 24, 20);
        for index in 0..8 {
            terminal.process_text(&format!("line-{index}\r\n"));
        }

        assert!(terminal.snapshot().contains_text("line-7"));
        assert!(!terminal.snapshot().contains_text("line-1"));
        assert!(terminal.is_following());

        terminal.scroll_up(4);
        let scrolled = terminal.snapshot();

        assert!(terminal.is_scrolled());
        assert!(
            scrolled.contains_text("line-1"),
            "{:?}",
            scrolled.plain_lines()
        );
        assert!(
            scrolled.contains_text("line-4"),
            "{:?}",
            scrolled.plain_lines()
        );

        terminal.jump_bottom();

        assert!(terminal.is_following());
        assert!(terminal.snapshot().contains_text("line-7"));
    }

    #[test]
    fn terminal_search_walks_scrollback_history_and_moves_view_to_match() {
        let mut terminal = TerminalEmulator::new(4, 32, 20);
        for index in 0..10 {
            terminal.process_text(&format!("history line {index}\r\n"));
        }

        assert!(!terminal.snapshot().contains_text("history line 2"));
        let found = terminal
            .search_scrollback("history line 2", TerminalSearchDirection::First)
            .expect("history match");

        assert_eq!(found.query, "history line 2");
        assert!(found.scrollback > 0);
        assert!(terminal.is_scrolled());
        assert!(terminal.snapshot().contains_text("history line 2"));
    }

    #[test]
    fn terminal_search_deduplicates_physical_rows_and_visits_every_occurrence() {
        let mut terminal = TerminalEmulator::new(3, 32, 20);
        terminal.process_text("old match match\r\nmid match\r\nnew match match\r\nlast line\r\n");

        let first = terminal
            .search_scrollback("match", TerminalSearchDirection::First)
            .expect("first match");
        assert_eq!(terminal.search_match_count(), 5);

        let mut visited = vec![(first.physical_row, first.occurrence)];
        for _ in 1..5 {
            let found = terminal
                .search_scrollback("match", TerminalSearchDirection::Next)
                .expect("next match");
            visited.push((found.physical_row, found.occurrence));
        }
        assert_eq!(visited.len(), 5);
        assert_eq!(
            visited
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            5
        );
        assert!(visited.windows(2).all(|pair| pair[0] < pair[1]));

        let wrapped = terminal
            .search_scrollback("match", TerminalSearchDirection::Next)
            .expect("circular next");
        assert_eq!((wrapped.physical_row, wrapped.occurrence), visited[0]);
        let previous = terminal
            .search_scrollback("match", TerminalSearchDirection::Previous)
            .expect("circular previous");
        assert_eq!(
            (previous.physical_row, previous.occurrence),
            *visited.last().expect("last match")
        );
    }

    #[test]
    fn terminal_search_is_non_overlapping_and_requires_cell_boundaries() {
        let mut terminal = TerminalEmulator::new(3, 32, 20);
        terminal.process_text("aaaa e\u{301} \u{754c}\r\n");

        terminal
            .search_scrollback("aa", TerminalSearchDirection::First)
            .expect("first non-overlapping match");
        assert_eq!(terminal.search_match_count(), 2);
        assert!(terminal
            .search_scrollback("e", TerminalSearchDirection::First)
            .is_none());
        assert!(terminal
            .search_scrollback("\u{301}", TerminalSearchDirection::First)
            .is_none());
        let grapheme = terminal
            .search_scrollback("e\u{301}", TerminalSearchDirection::First)
            .expect("whole grapheme match");
        assert_eq!(grapheme.matched_text, "e\u{301}");
        assert_eq!(grapheme.end_cell - grapheme.start_cell, 1);
    }

    #[test]
    fn terminal_search_is_literal_and_case_sensitive() {
        let mut terminal = TerminalEmulator::new(3, 32, 20);
        terminal.process_text("Match match MATCH match.\r\n");

        let first = terminal
            .search_scrollback("match", TerminalSearchDirection::First)
            .expect("literal lowercase match");

        assert_eq!(terminal.search_match_count(), 2);
        assert_eq!(first.matched_text, "match");
        assert!(terminal
            .search_scrollback("match ", TerminalSearchDirection::First)
            .is_some());
        assert!(terminal
            .search_scrollback("m.tch", TerminalSearchDirection::First)
            .is_none());
    }

    #[test]
    fn terminal_search_copy_text_preserves_spaces_combining_and_wide_graphemes() {
        let mut terminal = TerminalEmulator::new(3, 32, 20);
        terminal.process_text("prefix  e\u{301} \u{754c}  suffix\r\n");

        let found = terminal
            .search_scrollback("  e\u{301} \u{754c}  ", TerminalSearchDirection::First)
            .expect("exact mixed-width match");

        assert_eq!(found.matched_text, "  e\u{301} \u{754c}  ");
        assert_eq!(found.end_cell - found.start_cell, 8);
    }

    #[test]
    fn terminal_search_ignores_padding_but_preserves_explicit_spaces() {
        let mut terminal = TerminalEmulator::new(3, 8, 20);
        assert!(terminal
            .search_scrollback(" ", TerminalSearchDirection::First)
            .is_none());

        terminal.process_text("A  \r\nB C");
        let snapshot = terminal.snapshot();
        assert!(snapshot.cells[0][1].occupied);
        assert!(snapshot.cells[0][2].occupied);
        assert!(!snapshot.cells[0][3].occupied);

        let trailing = terminal
            .search_scrollback("A  ", TerminalSearchDirection::First)
            .expect("explicit trailing spaces");
        assert_eq!(trailing.line, "A  ");
        assert_eq!(trailing.matched_text, "A  ");
        assert_eq!((trailing.start_cell, trailing.end_cell), (0, 3));

        let internal = terminal
            .search_scrollback("B C", TerminalSearchDirection::First)
            .expect("explicit internal space");
        assert_eq!(internal.matched_text, "B C");
        assert!(terminal
            .search_scrollback("C ", TerminalSearchDirection::First)
            .is_none());
        assert!(terminal
            .search_scrollback("   ", TerminalSearchDirection::First)
            .is_none());
    }

    #[test]
    fn checkpointed_tab_spaces_remain_searchable_while_erased_cells_stay_invisible() {
        let mut source = vt100::Parser::new(3, 12, 20);
        source.process(b"A\tB\r\nA C\x1b[2D\x1b[X");
        let hydration = source.screen().parser_checkpoint_formatted();
        let continuation = source.continuation();
        let mut terminal = TerminalEmulator::new(3, 12, 20);
        terminal.parser.process(&hydration);
        terminal.parser.restore_continuation(continuation);

        let snapshot = terminal.snapshot();
        assert!(snapshot.cells[0][1..8].iter().all(|cell| cell.occupied));
        assert!(!snapshot.cells[1][1].occupied);
        let tabbed = terminal
            .search_scrollback("A       B", TerminalSearchDirection::First)
            .expect("checkpointed tab expansion remains searchable and copyable");
        assert_eq!(tabbed.matched_text, "A       B");
        assert!(terminal
            .search_scrollback("A C", TerminalSearchDirection::First)
            .is_none());
    }

    #[test]
    fn terminal_search_invalidates_and_rebuilds_when_output_changes() {
        let mut terminal = TerminalEmulator::new(3, 32, 20);
        terminal.process_text("first target\r\n");
        terminal
            .search_scrollback("target", TerminalSearchDirection::First)
            .expect("initial match");
        assert!(terminal.current_search_match().is_some());

        terminal.process_text("second target\r\n");
        assert!(terminal.current_search_match().is_none());
        terminal
            .search_scrollback("target", TerminalSearchDirection::Next)
            .expect("rebuilt match");
        assert_eq!(terminal.search_match_count(), 2);

        terminal.resize(4, 40);
        assert!(terminal.current_search_match().is_none());
        terminal
            .search_scrollback("target", TerminalSearchDirection::First)
            .expect("match rebuilt after resize");
    }

    #[test]
    fn terminal_snapshot_preserves_blank_and_internal_space_cells() {
        let mut terminal = TerminalEmulator::new(3, 24, 20);
        terminal.process_text(">Hey can you see");

        let snapshot = terminal.snapshot();
        let line = snapshot.line_text(0).expect("line exists");

        assert!(line.starts_with(">Hey can you see"), "{line:?}");
        assert_eq!(snapshot.cells[0][4].display_symbol(), " ");
        assert_eq!(snapshot.cells[0][8].display_symbol(), " ");
        assert_eq!(snapshot.cells[0][12].display_symbol(), " ");
        assert_eq!(snapshot.cells[1][0].display_symbol(), " ");
        assert_eq!(snapshot.cells[1][0].width, 1);
    }

    #[test]
    fn terminal_snapshot_marks_wide_continuation_cells() {
        let mut terminal = TerminalEmulator::new(2, 12, 20);
        terminal.process_text("A界B");

        let snapshot = terminal.snapshot();
        let row = &snapshot.cells[0];

        assert_eq!(row[0].display_symbol(), "A");
        assert_eq!(row[1].display_symbol(), "界");
        assert_eq!(row[1].width, 2);
        assert!(row[2].continuation);
        assert_eq!(row[3].display_symbol(), "B");
        assert_eq!(snapshot.plain_lines()[0], "A界B");
    }

    #[test]
    fn parser_forms_graphemes_before_authoritative_wrap_and_cursor_advance() {
        let mut terminal = TerminalEmulator::new(3, 4, 20);
        terminal.process_text("abc\u{2764}");
        terminal.process_text("\u{fe0f}");
        let snapshot = terminal.snapshot();

        assert_eq!((snapshot.cursor_row, snapshot.cursor_col), (1, 2));
        assert_eq!(snapshot.cells[1][0].symbol, "\u{2764}\u{fe0f}");
        assert_eq!(snapshot.cells[1][0].width, 2);
        assert!(snapshot.cells[1][1].continuation);
        assert_eq!(snapshot.cells[0][3], TerminalCell::blank());

        terminal.process_text("\u{1f469}");
        terminal.process_text("\u{200d}");
        terminal.process_text("\u{1f4bb}");
        let snapshot = terminal.snapshot();
        assert_eq!(snapshot.cells[1][2].symbol, "\u{1f469}\u{200d}\u{1f4bb}");
        assert_eq!((snapshot.cursor_row, snapshot.cursor_col), (1, 4));
    }

    #[test]
    fn parser_replaces_invalid_and_finalized_incomplete_utf8_deterministically() {
        let mut terminal = TerminalEmulator::new(2, 8, 20);
        terminal.process(&[0xff]);
        terminal.process(&[0xf0, 0x9f]);
        assert_eq!(terminal.snapshot().cells[0][0].symbol, "\u{fffd}");
        assert_eq!(terminal.snapshot().cursor_col, 1);

        terminal.finish_input();
        let snapshot = terminal.snapshot();
        assert_eq!(snapshot.cells[0][1].symbol, "\u{fffd}");
        assert_eq!(snapshot.cursor_col, 2);
    }

    #[test]
    fn neutral_controls_and_viewport_changes_do_not_break_active_graphemes() {
        for control in [
            b"\x1b[?25l".as_slice(),
            b"\x1b[?1000h",
            b"\x1b[?2004h",
            b"\x1b=",
            b"\x1b>",
        ] {
            let mut terminal = TerminalEmulator::new(3, 16, 20);
            terminal.process_text("e");
            terminal.process(control);
            terminal.process_text("\u{301}");
            assert_eq!(terminal.snapshot().cells[0][0].symbol, "e\u{301}");
        }

        let mut terminal = TerminalEmulator::new(2, 16, 20);
        terminal.process_text("history\r\nmore\r\n");
        terminal.process_text("e");
        terminal.scroll_up(1);
        terminal.jump_bottom();
        terminal.process_text("\u{301}");
        assert!(terminal
            .snapshot()
            .cells
            .iter()
            .flatten()
            .any(|cell| cell.symbol == "e\u{301}"));

        let mut positional = TerminalEmulator::new(3, 16, 20);
        positional.process_text("\u{2764}");
        positional.process(b"\x1b[1D");
        positional.process_text("\u{fe0f}");
        assert_eq!(positional.snapshot().cells[0][0].symbol, "\u{2764}");
    }

    #[test]
    fn extending_runs_are_bounded_without_moving_the_cursor() {
        let mut terminal = TerminalEmulator::new(2, 8, 20);
        terminal.process_text("e");
        terminal.process_text(&"\u{301}".repeat(1024));
        let snapshot = terminal.snapshot();

        assert!(snapshot.cells[0][0].symbol.len() <= vt100::width::MAX_GRAPHEME_BYTES);
        assert_eq!((snapshot.cursor_row, snapshot.cursor_col), (0, 1));
    }
}
