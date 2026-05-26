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
