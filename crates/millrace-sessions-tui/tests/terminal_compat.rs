use millrace_sessions_tui::{TerminalColor, TerminalEmulator};

#[test]
fn terminal_adapter_renders_plain_prompt_and_line_cli() {
    let mut terminal = TerminalEmulator::new(6, 40, 100);

    terminal.process_text("agent> hello\r\nresult: ok\r\nagent> ");
    let snapshot = terminal.snapshot();

    assert!(snapshot.contains_text("agent> hello"));
    assert!(snapshot.contains_text("result: ok"));
    assert!(snapshot.contains_text("agent>"));
}

#[test]
fn terminal_adapter_tracks_cursor_movement() {
    let mut terminal = TerminalEmulator::new(4, 20, 100);

    terminal.process(b"abcd\x1b[2DXY");
    let snapshot = terminal.snapshot();

    assert_eq!(snapshot.plain_lines()[0].trim_end(), "abXY");
    assert_eq!((snapshot.cursor_row, snapshot.cursor_col), (0, 4));
}

#[test]
fn terminal_adapter_tracks_alternate_screen_output() {
    let mut terminal = TerminalEmulator::new(4, 20, 100);

    terminal.process(b"main\x1b[?1049hfull\x1b[2;1Hscreen");
    let snapshot = terminal.snapshot();

    assert!(snapshot.alternate_screen);
    assert!(snapshot.contains_text("full"));
    assert!(snapshot.contains_text("screen"));
}

#[test]
fn terminal_adapter_preserves_basic_color_and_style() {
    let mut terminal = TerminalEmulator::new(3, 20, 100);

    terminal.process(b"\x1b[31;1mRED\x1b[0m");
    let snapshot = terminal.snapshot();
    let cell = &snapshot.cells[0][0];

    assert_eq!(cell.symbol, "R");
    assert_eq!(cell.fg, TerminalColor::Indexed(1));
    assert!(cell.style.bold);
}

#[test]
fn terminal_adapter_resizes_screen_buffer() {
    let mut terminal = TerminalEmulator::new(4, 20, 100);

    terminal.resize(8, 32);
    let snapshot = terminal.snapshot();

    assert_eq!((snapshot.rows, snapshot.cols), (8, 32));
}

#[test]
fn terminal_adapter_handles_millracer_operator_prompt_fixture() {
    let mut terminal = TerminalEmulator::new(8, 60, 100);

    terminal.process_text("Millracer operator ready\r\nworkspace: /tmp/work\r\n> ");
    let snapshot = terminal.snapshot();

    assert!(snapshot.contains_text("Millracer operator ready"));
    assert!(snapshot.contains_text("workspace: /tmp/work"));
}
