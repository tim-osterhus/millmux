//! Authoritative Millmux terminal cell-width facade.

use unicode_segmentation::UnicodeSegmentation as _;

pub use vt100::width::{grapheme_width, tab_advance, terminal_text_width, TERMINAL_TAB_WIDTH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalCellMapEntry {
    pub symbol: String,
    pub width: usize,
    pub continuation: bool,
    pub column: usize,
}

#[must_use]
pub fn normalize_cell_symbol(symbol: &str) -> String {
    if symbol.is_empty() {
        " ".to_string()
    } else {
        symbol.to_string()
    }
}

#[must_use]
pub fn cell_symbol_width(symbol: &str) -> usize {
    if symbol.is_empty() {
        0
    } else {
        terminal_text_width(symbol).min(2)
    }
}

#[must_use]
pub fn decode_lossy_terminal_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[must_use]
pub fn truncate_terminal_text(text: &str, max_width: usize) -> String {
    let text = text.replace('\n', " ");
    if terminal_text_width(&text) <= max_width {
        return text;
    }
    if max_width == 0 {
        return String::new();
    }

    let content_width = max_width - 1;
    let mut width = 0;
    let mut truncated = String::new();
    for grapheme in text.graphemes(true) {
        let grapheme_width = if grapheme == "\t" {
            tab_advance(width)
        } else {
            grapheme_width(grapheme)
        };
        if width.saturating_add(grapheme_width) > content_width {
            break;
        }
        truncated.push_str(grapheme);
        width = width.saturating_add(grapheme_width);
    }
    truncated.push('~');
    truncated
}

#[must_use]
pub fn terminal_cell_map(text: &str) -> Vec<TerminalCellMapEntry> {
    let mut column = 0;
    let mut cells = Vec::new();
    for grapheme in text.graphemes(true) {
        if grapheme == "\t" {
            for _ in 0..tab_advance(column) {
                cells.push(TerminalCellMapEntry {
                    symbol: " ".to_string(),
                    width: 1,
                    continuation: false,
                    column,
                });
                column += 1;
            }
            continue;
        }

        let width = grapheme_width(grapheme).min(2);
        cells.push(TerminalCellMapEntry {
            symbol: grapheme.to_string(),
            width,
            continuation: false,
            column,
        });
        if width == 2 {
            cells.push(TerminalCellMapEntry {
                symbol: String::new(),
                width: 0,
                continuation: true,
                column: column + 1,
            });
        }
        column += width;
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_covers_width_classes_and_tabs() {
        let cases = [
            ("space", " ", 1),
            ("combining", "e\u{301}", 1),
            ("variation selector", "\u{2764}\u{fe0f}", 2),
            ("cjk", "\u{754c}", 2),
            ("emoji", "\u{1f642}", 2),
            ("emoji zwj", "\u{1f469}\u{200d}\u{1f4bb}", 2),
            ("ambiguous", "\u{00b7}", 1),
        ];

        for (name, value, expected) in cases {
            assert_eq!(terminal_text_width(value), expected, "{name}");
        }
        assert_eq!(terminal_text_width("ab\tc"), 9);
    }

    #[test]
    fn invalid_bytes_have_deterministic_replacement_width() {
        let decoded = decode_lossy_terminal_text(&[0xf0, 0x9f]);
        assert_eq!(decoded, "\u{fffd}");
        assert_eq!(terminal_text_width(&decoded), 1);
    }

    #[test]
    fn truncation_respects_grapheme_cell_width() {
        let value = "界e\u{301}\u{1f642}tail";

        assert_eq!(truncate_terminal_text(value, 6), "界e\u{301}\u{1f642}~");
        assert_eq!(terminal_text_width(&truncate_terminal_text(value, 6)), 6);
    }
}
