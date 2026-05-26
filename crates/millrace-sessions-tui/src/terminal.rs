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
                line.push(TerminalCell {
                    symbol: cell.contents().to_string(),
                    fg: TerminalColor::from_vt100(cell.fgcolor()),
                    bg: TerminalColor::from_vt100(cell.bgcolor()),
                    style: TerminalStyle {
                        bold: cell.bold(),
                        dim: cell.dim(),
                        italic: cell.italic(),
                        underline: cell.underline(),
                        inverse: cell.inverse(),
                    },
                });
            }
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
            .map(|row| {
                row.iter()
                    .map(|cell| cell.symbol.as_str())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
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
    pub fg: TerminalColor,
    pub bg: TerminalColor,
    pub style: TerminalStyle,
}

impl TerminalCell {
    pub fn blank() -> Self {
        Self {
            symbol: " ".to_string(),
            fg: TerminalColor::Default,
            bg: TerminalColor::Default,
            style: TerminalStyle::default(),
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
}
