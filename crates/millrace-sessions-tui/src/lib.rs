//! TUI app foundation for Millmux UI modes.
//!
//! This crate owns UI-local state only. Durable session and process authority
//! stays behind SessionControl in the host crate.

pub mod app;
pub mod keymap;
pub mod pane;
pub mod renderer;
pub mod shell;
pub mod terminal;
pub mod width;

pub use app::{AppModel, HostConnectionState};
pub use keymap::{KeyAction, PrefixKeymap};
pub use pane::{
    AgentCockpitLayout, AgentTerminalPane, CommandOutput, CommandOutputState, CommandPalette,
    ConfirmationPrompt, DaemonConsoleLayout, DaemonSwitcherOverlay, HelpOverlay, LineLogPane, Pane,
    PaneKind, SearchMatch,
};
pub use shell::{ShellExit, TuiShell, UiContextSink};
pub use terminal::{
    TerminalCell, TerminalColor, TerminalEmulator, TerminalSearchDirection, TerminalSearchMatch,
    TerminalSnapshot, TerminalStyle,
};
