//! Pinned terminal cell-width policy for Millmux-rendered cells.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub const TERMINAL_TAB_WIDTH: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalCellMapEntry {
    pub symbol: String,
    pub width: usize,
    pub continuation: bool,
    pub column: usize,
}

pub fn normalize_cell_symbol(symbol: &str) -> String {
    if symbol.is_empty() {
        " ".to_string()
    } else {
        symbol.to_string()
    }
}

pub fn cell_symbol_width(symbol: &str) -> usize {
    if symbol.is_empty() {
        return 0;
    }
    terminal_text_width(symbol).clamp(0, 2)
}

pub fn terminal_text_width(text: &str) -> usize {
    let mut column = 0;
    for grapheme in text.graphemes(true) {
        if grapheme == "\t" {
            column += tab_advance(column);
        } else {
            column += grapheme_width(grapheme);
        }
    }
    column
}

pub fn tab_advance(column: usize) -> usize {
    let remainder = column % TERMINAL_TAB_WIDTH;
    if remainder == 0 {
        TERMINAL_TAB_WIDTH
    } else {
        TERMINAL_TAB_WIDTH - remainder
    }
}

pub fn decode_lossy_terminal_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

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

        let width = grapheme_width(grapheme).clamp(0, 2);
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

fn grapheme_width(grapheme: &str) -> usize {
    if grapheme.is_empty() || grapheme.chars().all(is_zero_width_terminal_mark) {
        return 0;
    }
    if is_emoji_grapheme(grapheme) {
        return 2;
    }
    UnicodeWidthStr::width(grapheme)
}

fn is_zero_width_terminal_mark(value: char) -> bool {
    matches!(
        value,
        '\u{0300}'..='\u{036f}'
            | '\u{1ab0}'..='\u{1aff}'
            | '\u{1dc0}'..='\u{1dff}'
            | '\u{20d0}'..='\u{20ff}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{e0100}'..='\u{e01ef}'
            | '\u{200d}'
    )
}

fn is_emoji_grapheme(grapheme: &str) -> bool {
    grapheme.contains('\u{200d}')
        || grapheme.contains('\u{fe0f}')
        || grapheme.chars().any(|value| {
            matches!(
                value,
                '\u{1f000}'..='\u{1faff}' | '\u{2600}'..='\u{27bf}'
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_width_policy_covers_batch_zero_golden_cases() {
        let cases = [
            ("space", " ", 1),
            ("ascii", "abc", 3),
            ("combining", "\u{0301}", 0),
            ("latin combining", "e\u{0301}", 1),
            ("variation selector", "\u{fe0f}", 0),
            ("cjk", "界", 2),
            ("emoji", "🙂", 2),
            ("emoji zwj", "👩\u{200d}💻", 2),
            ("ambiguous", "·", 1),
        ];

        for (name, value, expected) in cases {
            assert_eq!(terminal_text_width(value), expected, "{name}");
        }
    }

    #[test]
    fn pinned_width_policy_expands_tabs_and_invalid_bytes_deterministically() {
        assert_eq!(tab_advance(0), 8);
        assert_eq!(tab_advance(6), 2);
        assert_eq!(terminal_text_width("ab\tc"), 9);

        let decoded = decode_lossy_terminal_text(&[0xf0, 0x9f]);
        assert_eq!(decoded, "\u{fffd}");
        assert_eq!(terminal_text_width(&decoded), 1);
    }

    #[test]
    fn golden_cell_maps_cover_batch_zero_width_cases() {
        let fixtures = [
            (
                "spaces",
                "  a  ",
                vec![
                    (" ", 1, false, 0),
                    (" ", 1, false, 1),
                    ("a", 1, false, 2),
                    (" ", 1, false, 3),
                    (" ", 1, false, 4),
                ],
            ),
            (
                "cjk",
                "A界",
                vec![("A", 1, false, 0), ("界", 2, false, 1), ("", 0, true, 2)],
            ),
            (
                "emoji zwj",
                "👩\u{200d}💻",
                vec![("👩\u{200d}💻", 2, false, 0), ("", 0, true, 1)],
            ),
            ("combining", "e\u{0301}", vec![("e\u{0301}", 1, false, 0)]),
            (
                "variation selector",
                "\u{fe0f}",
                vec![("\u{fe0f}", 0, false, 0)],
            ),
            (
                "tab",
                "a\tb",
                vec![
                    ("a", 1, false, 0),
                    (" ", 1, false, 1),
                    (" ", 1, false, 2),
                    (" ", 1, false, 3),
                    (" ", 1, false, 4),
                    (" ", 1, false, 5),
                    (" ", 1, false, 6),
                    (" ", 1, false, 7),
                    ("b", 1, false, 8),
                ],
            ),
            ("ambiguous", "·", vec![("·", 1, false, 0)]),
        ];

        for (name, text, expected) in fixtures {
            let actual = terminal_cell_map(text)
                .into_iter()
                .map(|cell| (cell.symbol, cell.width, cell.continuation, cell.column))
                .collect::<Vec<_>>();
            let expected = expected
                .into_iter()
                .map(|(symbol, width, continuation, column)| {
                    (symbol.to_string(), width, continuation, column)
                })
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "{name}");
        }

        let invalid = terminal_cell_map(&decode_lossy_terminal_text(&[0xf0, 0x9f]));
        assert_eq!(
            invalid,
            vec![TerminalCellMapEntry {
                symbol: "\u{fffd}".to_string(),
                width: 1,
                continuation: false,
                column: 0,
            }]
        );
    }
}
