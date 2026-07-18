/// Version of the serialized VTE resume representation.
pub const VTE_RESUME_VERSION: u8 = 1;

/// Maximum bytes retained for an incomplete VTE control sequence.
pub const MAX_VTE_RESUME_BYTES: usize = 4096;

/// An incomplete VTE control type whose payload was too large to checkpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VteResumeOverflow {
    /// An operating-system command.
    Osc,
    /// A device-control string.
    Dcs,
    /// A control-sequence introducer command.
    Csi,
    /// An ignored SOS, PM, or APC control string.
    SosPmApc,
}

impl VteResumeOverflow {
    fn prefix(self) -> &'static [u8] {
        match self {
            Self::Osc => b"\x1b]",
            Self::Dcs => b"\x1bP",
            Self::Csi => b"\x1b[",
            Self::SosPmApc => b"\x1bX",
        }
    }
}

/// Bounded VTE parser state not represented by formatted screen hydration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VteResumeState {
    /// Wire format version for this typed representation.
    pub version: u8,
    /// Bytes since the last completed VTE dispatch.
    pub bytes: Vec<u8>,
    /// A fail-closed control sequence whose payload was not retained.
    pub overflow: Option<VteResumeOverflow>,
}

impl Default for VteResumeState {
    fn default() -> Self {
        Self {
            version: VTE_RESUME_VERSION,
            bytes: Vec::new(),
            overflow: None,
        }
    }
}

/// A parser continuation that is not represented by formatted screen hydration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParserContinuation {
    /// Incomplete UTF-8 bytes retained until more input or finalization arrives.
    pub pending_utf8: Vec<u8>,
    /// The current grapheme anchor, when later input may extend it.
    pub active_grapheme: Option<ActiveGraphemeContinuation>,
    /// Directly captured non-rendered screen state.
    pub screen: Option<crate::ScreenContinuation>,
    /// Bounded VTE state required to complete a split control sequence.
    pub vte_resume: VteResumeState,
}

/// The location and contents of an active terminal grapheme.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveGraphemeContinuation {
    /// Zero-based row of the grapheme's leading cell.
    pub row: u16,
    /// Zero-based column of the grapheme's leading cell.
    pub col: u16,
    /// Complete grapheme text at the checkpoint boundary.
    pub text: String,
    /// Terminal-cell width assigned to the grapheme.
    pub width: u16,
}

/// A parser for terminal output which produces an in-memory representation of
/// the terminal contents.
pub struct Parser<CB: crate::callbacks::Callbacks = ()> {
    parser: vte::Parser,
    screen: crate::perform::WrappedScreen<CB>,
    pending_utf8: Vec<u8>,
    vte_tail: Vec<u8>,
    vte_overflow: Option<VteResumeOverflow>,
    suppressing_vte: Option<VteResumeOverflow>,
}

impl Parser {
    /// Creates a new terminal parser of the given size and with the given
    /// amount of scrollback.
    #[must_use]
    pub fn new(rows: u16, cols: u16, scrollback_len: usize) -> Self {
        Self {
            parser: vte::Parser::new(),
            screen: crate::perform::WrappedScreen::new(rows, cols, scrollback_len),
            pending_utf8: Vec::with_capacity(4),
            vte_tail: Vec::new(),
            vte_overflow: None,
            suppressing_vte: None,
        }
    }
}

impl<CB: crate::callbacks::Callbacks> Parser<CB> {
    /// Creates a new terminal parser of the given size and with the given
    /// amount of scrollback. Terminal events will be reported via method
    /// calls on the provided [`Callbacks`](crate::callbacks::Callbacks)
    /// implementation.
    pub fn new_with_callbacks(rows: u16, cols: u16, scrollback_len: usize, callbacks: CB) -> Self {
        Self {
            parser: vte::Parser::new(),
            screen: crate::perform::WrappedScreen::new_with_callbacks(
                rows,
                cols,
                scrollback_len,
                callbacks,
            ),
            pending_utf8: Vec::with_capacity(4),
            vte_tail: Vec::new(),
            vte_overflow: None,
            suppressing_vte: None,
        }
    }

    /// Processes the contents of the given byte string, and updates the
    /// in-memory terminal state.
    pub fn process(&mut self, bytes: &[u8]) {
        self.pending_utf8.extend_from_slice(bytes);
        let input = std::mem::take(&mut self.pending_utf8);
        let mut remaining = input.as_slice();

        while !remaining.is_empty() {
            match std::str::from_utf8(remaining) {
                Ok(_) => {
                    self.advance_vte(remaining);
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        self.advance_vte(&remaining[..valid]);
                    }
                    if let Some(invalid) = error.error_len() {
                        self.advance_vte("\u{fffd}".as_bytes());
                        remaining = &remaining[valid + invalid..];
                    } else {
                        self.pending_utf8.extend_from_slice(&remaining[valid..]);
                        break;
                    }
                }
            }
        }
    }

    /// Finalizes an input stream, replacing any incomplete UTF-8 suffix.
    pub fn finish(&mut self) {
        if !self.pending_utf8.is_empty() {
            self.pending_utf8.clear();
            self.advance_vte("\u{fffd}".as_bytes());
        }
    }

    /// Returns the parser state that formatted screen hydration does not retain.
    #[must_use]
    pub fn continuation(&self) -> ParserContinuation {
        ParserContinuation {
            pending_utf8: self.pending_utf8.clone(),
            active_grapheme: self.screen.screen.active_grapheme_continuation(),
            screen: Some(self.screen.screen.continuation()),
            vte_resume: VteResumeState {
                version: VTE_RESUME_VERSION,
                bytes: self.vte_tail.clone(),
                overflow: self.vte_overflow,
            },
        }
    }

    /// Restores parser state after the screen has been hydrated from a checkpoint.
    pub fn restore_continuation(&mut self, continuation: ParserContinuation) {
        self.pending_utf8 = continuation.pending_utf8;
        if let Some(screen) = continuation.screen {
            self.screen.screen.restore_continuation(&screen);
        }
        self.screen
            .screen
            .restore_active_grapheme_continuation(continuation.active_grapheme);
        self.restore_vte_resume(continuation.vte_resume);
    }

    /// Hydrates a visible screen at a new authoritative parser boundary.
    pub fn hydrate_screen(&mut self, hydration: &[u8]) {
        self.pending_utf8.clear();
        self.parser = vte::Parser::new();
        self.vte_tail.clear();
        self.vte_overflow = None;
        self.suppressing_vte = None;
        self.screen.screen.break_grapheme();
        self.process(hydration);
        debug_assert!(self.pending_utf8.is_empty());
    }

    /// Returns a reference to a [`Screen`](crate::Screen) object containing
    /// the terminal state.
    #[must_use]
    pub fn screen(&self) -> &crate::Screen {
        &self.screen.screen
    }

    /// Returns a mutable reference to a [`Screen`](crate::Screen) object
    /// containing the terminal state.
    #[must_use]
    pub fn screen_mut(&mut self) -> &mut crate::Screen {
        self.screen.screen.break_grapheme();
        &mut self.screen.screen
    }

    /// Returns the screen for a viewport-only operation.
    ///
    /// Unlike [`Self::screen_mut`], this preserves an active grapheme. Callers
    /// must only use APIs that do not move the drawing cursor or mutate
    /// terminal content, such as [`crate::Screen::set_scrollback_viewport`].
    #[must_use]
    pub fn screen_viewport_mut(&mut self) -> &mut crate::Screen {
        &mut self.screen.screen
    }

    /// Returns a reference to the [`Callbacks`](crate::callbacks::Callbacks)
    /// state object passed into the constructor.
    pub fn callbacks(&self) -> &CB {
        &self.screen.callbacks
    }

    /// Returns a mutable reference to the
    /// [`Callbacks`](crate::callbacks::Callbacks) state object passed into
    /// the constructor.
    pub fn callbacks_mut(&mut self) -> &mut CB {
        &mut self.screen.callbacks
    }

    fn advance_vte(&mut self, bytes: &[u8]) {
        for byte in bytes {
            if self.suppressing_vte.is_some() {
                let cancellation_without_dispatch =
                    matches!(
                        self.suppressing_vte,
                        Some(
                            VteResumeOverflow::Csi
                                | VteResumeOverflow::Dcs
                                | VteResumeOverflow::SosPmApc
                        )
                    ) && matches!(*byte, b'\x1b' | b'\x18' | b'\x1a');
                let mut discard = ResumePerformer::default();
                self.parser
                    .advance(&mut discard, std::slice::from_ref(byte));
                if discard.completed_dispatch || cancellation_without_dispatch {
                    self.suppressing_vte = None;
                    self.vte_tail.clear();
                    self.vte_overflow = None;
                    if *byte == b'\x1b' {
                        self.vte_tail.push(*byte);
                    }
                }
                continue;
            }

            let ignored_string_cancelled = self.vte_overflow == Some(VteResumeOverflow::SosPmApc)
                && matches!(*byte, b'\x1b' | b'\x18' | b'\x1a');
            let dispatches = self.screen.completed_dispatches();
            self.parser
                .advance(&mut self.screen, std::slice::from_ref(byte));
            if self.screen.completed_dispatches() != dispatches || ignored_string_cancelled {
                self.vte_tail.clear();
                self.vte_overflow = None;
                if *byte == b'\x1b' {
                    self.vte_tail.push(*byte);
                }
            } else if self.vte_overflow.is_none() && (!self.vte_tail.is_empty() || *byte == b'\x1b')
            {
                self.vte_tail.push(*byte);
                if self.vte_tail.len() > MAX_VTE_RESUME_BYTES {
                    self.vte_overflow = vte_overflow_kind(&self.vte_tail);
                    self.vte_tail.clear();
                }
            }
        }
    }

    fn restore_vte_resume(&mut self, resume: VteResumeState) {
        self.vte_tail.clear();
        self.vte_overflow = None;
        self.suppressing_vte = None;
        if resume.version != VTE_RESUME_VERSION {
            return;
        }

        if let Some(overflow) = resume.overflow {
            let mut discard = ResumePerformer::default();
            self.parser.advance(&mut discard, overflow.prefix());
            self.suppressing_vte = Some(overflow);
            self.vte_overflow = Some(overflow);
            return;
        }

        let bytes = resume
            .bytes
            .into_iter()
            .take(MAX_VTE_RESUME_BYTES)
            .collect::<Vec<_>>();
        let mut discard = ResumePerformer::default();
        self.parser.advance(&mut discard, &bytes);
        self.vte_tail = bytes;
    }
}

#[derive(Default)]
struct ResumePerformer {
    completed_dispatch: bool,
}

impl vte::Perform for ResumePerformer {
    fn print(&mut self, _c: char) {
        self.completed_dispatch = true;
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {
        self.completed_dispatch = true;
    }

    fn csi_dispatch(
        &mut self,
        _params: &vte::Params,
        _intermediates: &[u8],
        _ignore: bool,
        _c: char,
    ) {
        self.completed_dispatch = true;
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bel_terminated: bool) {
        self.completed_dispatch = true;
    }

    fn unhook(&mut self) {
        self.completed_dispatch = true;
    }
}

fn vte_overflow_kind(bytes: &[u8]) -> Option<VteResumeOverflow> {
    match bytes.get(..2) {
        Some(b"\x1b]") => Some(VteResumeOverflow::Osc),
        Some(b"\x1bP") => Some(VteResumeOverflow::Dcs),
        Some(b"\x1b[") => Some(VteResumeOverflow::Csi),
        Some(b"\x1bX" | b"\x1b^" | b"\x1b_") => Some(VteResumeOverflow::SosPmApc),
        _ => None,
    }
}

impl Default for Parser {
    /// Returns a parser with dimensions 80x24 and no scrollback.
    fn default() -> Self {
        Self::new(24, 80, 0)
    }
}

impl std::io::Write for Parser {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.process(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.finish();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Parser, VteResumeOverflow, MAX_VTE_RESUME_BYTES};

    #[test]
    fn restored_oversized_csi_preserves_escape_cancellation_prefix() {
        checkpointed_oversized_csi_matches(b"\x1b[2Jafter");
    }

    #[test]
    fn restored_oversized_csi_cancellation_does_not_consume_next_printable() {
        checkpointed_oversized_csi_matches(b"\x18after-can");
        checkpointed_oversized_csi_matches(b"\x1aafter-sub");
    }

    fn checkpointed_oversized_csi_matches(post: &[u8]) {
        let mut continuous = Parser::new(3, 20, 10);
        continuous.process(b"dirty-screen");
        let mut oversized = b"\x1b[".to_vec();
        oversized.extend(std::iter::repeat(b'1').take(MAX_VTE_RESUME_BYTES));
        continuous.process(&oversized);

        let hydration = continuous.screen().parser_checkpoint_formatted();
        let continuation = continuous.continuation();
        assert_eq!(
            continuation.vte_resume.overflow,
            Some(VteResumeOverflow::Csi)
        );

        let mut restored = Parser::new(3, 20, 10);
        restored.process(&hydration);
        restored.restore_continuation(continuation);

        continuous.process(post);
        restored.process(post);

        assert_eq!(restored.screen().contents(), continuous.screen().contents());
        assert_eq!(
            restored.screen().cursor_position(),
            continuous.screen().cursor_position()
        );
    }

    #[test]
    fn restored_oversized_ignored_strings_suppress_payload_until_termination() {
        for prefix in [
            b"\x1bX".as_slice(),
            b"\x1b^".as_slice(),
            b"\x1b_".as_slice(),
        ] {
            for post in [
                b"payload\x1b\\after-st".as_slice(),
                b"payload\x18after-can".as_slice(),
                b"payload\x1aafter-sub".as_slice(),
            ] {
                checkpointed_oversized_ignored_string_matches(prefix, post);
            }
        }
    }

    #[test]
    fn restored_oversized_dcs_cancellation_matches_continuous_parser() {
        checkpointed_oversized_dcs_matches(b"\x18after-can");
        checkpointed_oversized_dcs_matches(b"\x1aafter-sub");
        checkpointed_oversized_dcs_matches(b"\x1b[2Jafter-escape");
    }

    fn checkpointed_oversized_dcs_matches(post: &[u8]) {
        let mut continuous = Parser::new(3, 40, 10);
        continuous.process(b"before");
        let mut oversized = b"\x1bP".to_vec();
        oversized.extend(std::iter::repeat(b'x').take(MAX_VTE_RESUME_BYTES));
        continuous.process(&oversized);

        let hydration = continuous.screen().parser_checkpoint_formatted();
        let continuation = continuous.continuation();
        assert_eq!(
            continuation.vte_resume.overflow,
            Some(VteResumeOverflow::Dcs)
        );

        let mut restored = Parser::new(3, 40, 10);
        restored.process(&hydration);
        restored.restore_continuation(continuation);
        continuous.process(post);
        restored.process(post);

        assert_eq!(restored.screen().contents(), continuous.screen().contents());
        assert_eq!(
            restored.screen().cursor_position(),
            continuous.screen().cursor_position()
        );
    }

    fn checkpointed_oversized_ignored_string_matches(prefix: &[u8], post: &[u8]) {
        let mut continuous = Parser::new(3, 40, 10);
        continuous.process(b"before");
        let mut oversized = prefix.to_vec();
        oversized.extend(std::iter::repeat(b'x').take(MAX_VTE_RESUME_BYTES));
        continuous.process(&oversized);

        let hydration = continuous.screen().parser_checkpoint_formatted();
        let continuation = continuous.continuation();
        assert_eq!(
            continuation.vte_resume.overflow,
            Some(VteResumeOverflow::SosPmApc)
        );

        let mut restored = Parser::new(3, 40, 10);
        restored.process(&hydration);
        restored.restore_continuation(continuation);
        continuous.process(post);
        restored.process(post);

        assert_eq!(restored.screen().contents(), continuous.screen().contents());
        assert_eq!(
            restored.screen().cursor_position(),
            continuous.screen().cursor_position()
        );
    }

    #[test]
    fn checkpoint_preserves_tab_occupancy_without_materializing_erased_cells() {
        let mut continuous = Parser::new(3, 12, 10);
        continuous.process(b"A\tB\r\nA C\x1b[2D\x1b[X");
        let hydration = continuous.screen().parser_checkpoint_formatted();
        let continuation = continuous.continuation();

        let mut restored = Parser::new(3, 12, 10);
        restored.process(&hydration);
        restored.restore_continuation(continuation);

        for col in 1..8 {
            let continuous_cell = continuous.screen().cell(0, col).unwrap();
            let restored_cell = restored.screen().cell(0, col).unwrap();
            assert!(continuous_cell.is_occupied());
            assert!(!continuous_cell.has_contents());
            assert!(restored_cell.is_occupied());
            assert_eq!(restored_cell.contents(), " ");
        }
        assert!(!continuous.screen().cell(1, 1).unwrap().is_occupied());
        assert!(!restored.screen().cell(1, 1).unwrap().is_occupied());
        assert_eq!(restored.screen().contents(), continuous.screen().contents());
    }
}
