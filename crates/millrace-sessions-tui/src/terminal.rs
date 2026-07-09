use crate::width::{cell_symbol_width, normalize_cell_symbol};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalSearchDirection {
    First,
    Next,
    Previous,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSearchMatch {
    pub scrollback: usize,
    pub row: usize,
    pub query: String,
    pub line: String,
}

pub struct TerminalEmulator {
    parser: vt100::Parser,
    scrollback_len: usize,
}

impl TerminalEmulator {
    pub fn new(rows: u16, cols: u16, scrollback_len: usize) -> Self {
        Self {
            parser: vt100::Parser::new(rows.max(1), cols.max(1), scrollback_len),
            scrollback_len,
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    pub fn process_text(&mut self, text: &str) {
        self.process(text.as_bytes());
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows.max(1), cols.max(1));
    }

    pub fn scroll_up(&mut self, rows: usize) {
        let offset = self.parser.screen().scrollback().saturating_add(rows);
        self.parser.screen_mut().set_scrollback(offset);
    }

    pub fn scroll_down(&mut self, rows: usize) {
        let offset = self.parser.screen().scrollback().saturating_sub(rows);
        self.parser.screen_mut().set_scrollback(offset);
    }

    pub fn page_up(&mut self, rows: u16) {
        self.scroll_up(usize::from(rows).max(1));
    }

    pub fn page_down(&mut self, rows: u16) {
        self.scroll_down(usize::from(rows).max(1));
    }

    pub fn jump_top(&mut self) {
        self.parser.screen_mut().set_scrollback(usize::MAX);
    }

    pub fn jump_bottom(&mut self) {
        self.parser.screen_mut().set_scrollback(0);
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

        let original = self.parser.screen().scrollback();
        let max = self.max_scrollback();
        let offsets = search_offsets(original, max, direction);
        for offset in offsets {
            self.parser.screen_mut().set_scrollback(offset);
            if let Some(found) = self.search_visible_rows(query, offset) {
                return Some(found);
            }
        }

        self.parser.screen_mut().set_scrollback(original);
        None
    }

    fn max_scrollback(&mut self) -> usize {
        let original = self.parser.screen().scrollback();
        self.parser.screen_mut().set_scrollback(usize::MAX);
        let max = self.parser.screen().scrollback();
        self.parser.screen_mut().set_scrollback(original);
        max
    }

    fn search_visible_rows(&self, query: &str, scrollback: usize) -> Option<TerminalSearchMatch> {
        let snapshot = self.snapshot();
        snapshot
            .plain_lines()
            .into_iter()
            .enumerate()
            .find_map(|(row, line)| {
                line.contains(query).then(|| TerminalSearchMatch {
                    scrollback,
                    row,
                    query: query.to_string(),
                    line: snapshot.line_text(row).unwrap_or_default(),
                })
            })
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
        let symbol = normalize_cell_symbol(symbol.as_ref());
        Self {
            width: cell_symbol_width(&symbol) as u8,
            continuation: false,
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
        fg: TerminalColor,
        bg: TerminalColor,
        style: TerminalStyle,
    ) -> Self {
        Self {
            symbol: symbol.to_string(),
            width: cell_symbol_width(symbol) as u8,
            continuation: false,
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

fn row_text(row: &[TerminalCell]) -> String {
    row.iter()
        .filter(|cell| !cell.continuation)
        .map(TerminalCell::display_symbol)
        .collect()
}

fn search_offsets(current: usize, max: usize, direction: TerminalSearchDirection) -> Vec<usize> {
    match direction {
        TerminalSearchDirection::First => (0..=max).collect(),
        TerminalSearchDirection::Next => {
            let mut offsets = Vec::new();
            if current < max {
                offsets.extend(current + 1..=max);
            }
            offsets.extend(0..=current.min(max));
            offsets
        }
        TerminalSearchDirection::Previous => {
            let mut offsets = (0..current.min(max)).rev().collect::<Vec<_>>();
            offsets.extend((current.min(max)..=max).rev());
            offsets
        }
    }
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
}
