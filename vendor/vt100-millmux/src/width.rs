//! Millmux's pinned, locale-independent terminal width policy.

use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

/// Number of columns between deterministic terminal tab stops.
pub const TERMINAL_TAB_WIDTH: usize = 8;

/// Maximum UTF-8 bytes retained for one terminal cell grapheme.
///
/// Extending code points beyond this cap are ignored until a new base
/// grapheme arrives. This bounds both storage and incremental segmentation
/// work for malformed or adversarial extending runs.
pub const MAX_GRAPHEME_BYTES: usize = 256;

/// Returns the pinned width of one extended grapheme cluster.
#[must_use]
pub fn grapheme_width(grapheme: &str) -> usize {
    grapheme.width().min(2)
}

/// Returns the pinned terminal width of text, including deterministic tabs.
#[must_use]
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

/// Returns the number of cells to the next deterministic tab stop.
#[must_use]
pub fn tab_advance(column: usize) -> usize {
    TERMINAL_TAB_WIDTH - (column % TERMINAL_TAB_WIDTH)
}

/// Returns whether appending `next` keeps `current` in one grapheme cluster.
#[must_use]
pub(crate) fn extends_grapheme(current: &str, next: char) -> bool {
    if current.len().saturating_add(next.len_utf8()) > MAX_GRAPHEME_BYTES {
        return false;
    }
    let mut candidate = String::with_capacity(current.len() + next.len_utf8());
    candidate.push_str(current);
    candidate.push(next);
    candidate.graphemes(true).count() == 1
}
