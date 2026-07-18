use std::{collections::BTreeMap, path::PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use millrace_sessions_core::{
    ids::{SessionId, UiId},
    protocol::{ScreenSnapshot, SessionSummary},
    scrollback::TerminalStateBuffer,
    state::{MonitorProfile, ProcessState, SessionRole, SpawnMode},
    width::terminal_text_width as core_text_width,
    workspace::WorkspaceIdentity,
};
use millrace_sessions_tui::{
    renderer::render_terminal_snapshot, AgentCockpitLayout, AgentTerminalPane, AppModel, KeyAction,
    TerminalEmulator, TerminalSearchDirection, TerminalSnapshot,
};
use ratatui::{backend::TestBackend, layout::Rect, Terminal};
use serde_json::json;
use unicode_width_renderer::UnicodeWidthStr as _;

type WrapFixture<'a> = (&'a str, &'a [&'a [u8]], (u16, u16));

fn process_both(
    durable: &mut TerminalStateBuffer,
    cockpit: &mut TerminalEmulator,
    chunks: &[&[u8]],
) {
    for chunk in chunks {
        durable.process_output(chunk);
        cockpit.process(chunk);
    }
}

fn assert_parser_and_renderer_parity(durable: &TerminalStateBuffer, cockpit: &TerminalSnapshot) {
    let core = durable.screen_snapshot();
    assert_eq!((core.rows, core.cols), (cockpit.rows, cockpit.cols));
    assert_eq!(
        (core.cursor.row, core.cursor.col),
        (cockpit.cursor_row, cockpit.cursor_col)
    );
    assert_eq!(core.alternate_screen, cockpit.alternate_screen);

    let backend = TestBackend::new(cockpit.cols, cockpit.rows);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render_terminal_snapshot(frame, Rect::new(0, 0, cockpit.cols, cockpit.rows), cockpit);
        })
        .expect("render terminal snapshot");
    let rendered = terminal.backend().buffer();

    for row in 0..usize::from(cockpit.rows) {
        for col in 0..usize::from(cockpit.cols) {
            let durable_cell = &core.cells[row][col];
            let cockpit_cell = &cockpit.cells[row][col];
            assert_eq!(
                durable_cell.continuation, cockpit_cell.continuation,
                "continuation mismatch at {row},{col}"
            );
            if !cockpit_cell.continuation {
                assert_eq!(
                    durable_cell.symbol,
                    cockpit_cell.display_symbol(),
                    "symbol mismatch at {row},{col}"
                );
                assert_eq!(
                    durable_cell.width, cockpit_cell.width,
                    "width mismatch at {row},{col}"
                );
            }
            let rendered_symbol = rendered
                .cell((u16::try_from(col).unwrap(), u16::try_from(row).unwrap()))
                .expect("rendered cell")
                .symbol();
            let fits = col + usize::from(cockpit_cell.width).max(1) <= usize::from(cockpit.cols);
            let expected = if cockpit_cell.continuation || !fits {
                " "
            } else {
                cockpit_cell.display_symbol()
            };
            assert_eq!(rendered_symbol, expected, "buffer mismatch at {row},{col}");
        }
    }
}

#[test]
fn raw_bytes_have_durable_cockpit_and_ratatui_width_parity() {
    let mut durable = TerminalStateBuffer::new(4, 48, 4096, 0);
    let mut cockpit = TerminalEmulator::new(4, 48, 40);
    let chunks: &[&[u8]] = &[
        b">A  ",
        "e".as_bytes(),
        "\u{301}".as_bytes(),
        " \u{754c} \u{2764}".as_bytes(),
        "\u{fe0f}".as_bytes(),
        " \u{1f469}".as_bytes(),
        "\u{200d}".as_bytes(),
        "\u{1f4bb} \u{00b7}\tX ".as_bytes(),
        &[0xff],
        &[0xf0, 0x9f],
    ];
    process_both(&mut durable, &mut cockpit, chunks);
    durable.finish_input();
    cockpit.finish_input();

    let snapshot = cockpit.snapshot();
    assert_parser_and_renderer_parity(&durable, &snapshot);
    assert!(
        snapshot.line_text(0).expect("prompt row").starts_with(
            ">A  e\u{301} \u{754c} \u{2764}\u{fe0f} \u{1f469}\u{200d}\u{1f4bb} \u{00b7}"
        ),
        "{:?}",
        snapshot.plain_lines()
    );
    assert_eq!(snapshot.cells[0][2].display_symbol(), " ");
    assert_eq!(snapshot.cells[0][3].display_symbol(), " ");
    assert_eq!(snapshot.cells[0][25].display_symbol(), " ");
    assert_eq!(
        snapshot
            .line_text(0)
            .expect("raw byte row")
            .matches('\u{fffd}')
            .count(),
        2
    );

    let exact_match =
        "  e\u{301} \u{754c} \u{2764}\u{fe0f} \u{1f469}\u{200d}\u{1f4bb} \u{00b7}        X ";
    let found = cockpit
        .search_scrollback(exact_match, TerminalSearchDirection::First)
        .expect("mixed-width cell-boundary selection");
    assert_eq!(found.matched_text, exact_match);
    assert_eq!(found.end_cell - found.start_cell, 24);

    let mut app = cockpit_app_with_snapshot(cockpit.snapshot());
    app.begin_search_mode();
    app.search_query = exact_match.to_string();
    app.set_agent_search_match("search", &found);
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 4),
        KeyAction::CopySearchMatch
    );
    assert_eq!(app.copy_buffer_text(), Some(exact_match));
}

#[test]
fn adjacent_mixed_graphemes_fill_one_prompt_row_and_wrap_identically() {
    let mut durable = TerminalStateBuffer::new(3, 8, 256, 0);
    let mut cockpit = TerminalEmulator::new(3, 8, 20);
    let chunks: &[&[u8]] = &[
        b">e",
        "\u{301}".as_bytes(),
        "\u{754c}".as_bytes(),
        "\u{2764}".as_bytes(),
        "\u{fe0f}".as_bytes(),
        "\u{1f469}".as_bytes(),
        "\u{200d}".as_bytes(),
        "\u{1f4bb}".as_bytes(),
        b"Z",
    ];
    process_both(&mut durable, &mut cockpit, chunks);
    durable.finish_input();
    cockpit.finish_input();

    let snapshot = cockpit.snapshot();
    assert_parser_and_renderer_parity(&durable, &snapshot);
    assert_eq!(
        snapshot.line_text(0).as_deref(),
        Some(">e\u{301}\u{754c}\u{2764}\u{fe0f}\u{1f469}\u{200d}\u{1f4bb}")
    );
    assert_eq!(snapshot.line_text(1).as_deref(), Some("Z       "));
    assert_eq!((snapshot.cursor_row, snapshot.cursor_col), (1, 1));
}

#[test]
fn cursor_neutral_controls_preserve_split_graphemes_across_the_full_pipeline() {
    let mut durable = TerminalStateBuffer::new(3, 16, 256, 0);
    let mut cockpit = TerminalEmulator::new(3, 16, 20);
    let chunks: &[&[u8]] = &[
        b"e",
        b"\x1b[31m",
        "\u{301}".as_bytes(),
        b" ",
        "\u{2764}".as_bytes(),
        b"\x1b]2;grapheme-test\x07",
        "\u{fe0f}".as_bytes(),
        b" ",
        "\u{1f469}".as_bytes(),
        b"\x07",
        "\u{200d}".as_bytes(),
        b"\x1b[1m",
        "\u{1f4bb}".as_bytes(),
    ];
    process_both(&mut durable, &mut cockpit, chunks);
    durable.finish_input();
    cockpit.finish_input();

    let snapshot = cockpit.snapshot();
    assert_parser_and_renderer_parity(&durable, &snapshot);
    assert_eq!(snapshot.cells[0][0].symbol, "e\u{301}");
    assert_eq!(snapshot.cells[0][2].symbol, "\u{2764}\u{fe0f}");
    assert!(snapshot.cells[0][3].continuation);
    assert_eq!(snapshot.cells[0][5].symbol, "\u{1f469}\u{200d}\u{1f4bb}");
    assert!(snapshot.cells[0][6].continuation);
    assert_eq!((snapshot.cursor_row, snapshot.cursor_col), (0, 7));
}

#[test]
fn synchronized_output_controls_are_grapheme_neutral_until_a_position_break() {
    let mut durable = TerminalStateBuffer::new(3, 16, 256, 0);
    let mut cockpit = TerminalEmulator::new(3, 16, 20);
    let chunks: &[&[u8]] = &[
        b"e",
        b"\x1b[?2026h",
        "\u{301}".as_bytes(),
        b"\x1b[?2026l ",
        "\u{2764}".as_bytes(),
        b"\x1b[?2026h",
        "\u{fe0f}".as_bytes(),
        b"\x1b[?2026l ",
        "\u{1f469}".as_bytes(),
        b"\x1b[?2026h",
        "\u{200d}".as_bytes(),
        b"\x1b[?2026l",
        "\u{1f4bb}".as_bytes(),
        b"\x1b[2;1H",
        b"X",
        b"\x1b[?2026h",
        "\u{301}".as_bytes(),
        b"\x1b[?2026l",
    ];
    process_both(&mut durable, &mut cockpit, chunks);
    durable.finish_input();
    cockpit.finish_input();

    let snapshot = cockpit.snapshot();
    assert_parser_and_renderer_parity(&durable, &snapshot);
    assert_eq!(snapshot.cells[0][0].symbol, "e\u{301}");
    assert_eq!(snapshot.cells[0][2].symbol, "\u{2764}\u{fe0f}");
    assert_eq!(snapshot.cells[0][5].symbol, "\u{1f469}\u{200d}\u{1f4bb}");
    assert_eq!(snapshot.cells[1][0].symbol, "X\u{301}");
    assert_eq!((snapshot.cursor_row, snapshot.cursor_col), (1, 1));
}

#[test]
fn adopted_snapshot_preserves_intentional_trailing_spaces_for_search_and_copy() {
    let mut durable = TerminalStateBuffer::new(1, 8, 64, 0);
    durable.process_output(b"label  ");
    let screen = durable.screen_snapshot();

    assert!(screen.cells[0][5].occupied);
    assert!(screen.cells[0][6].occupied);
    assert!(!screen.cells[0][7].occupied);

    let mut cockpit = TerminalEmulator::new(1, 8, 20);
    cockpit.adopt_screen_snapshot(&screen);
    let found = cockpit
        .search_scrollback("  ", TerminalSearchDirection::First)
        .expect("intentional trailing-space match");
    assert_eq!(found.matched_text, "  ");
    assert_eq!(found.start_cell, 5);
    assert_eq!(found.end_cell, 7);

    let mut app = cockpit_app_with_snapshot(cockpit.snapshot());
    app.begin_search_mode();
    app.search_query = "  ".to_string();
    app.set_agent_search_match("search", &found);
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 1),
        KeyAction::CopySearchMatch
    );
    assert_eq!(app.copy_buffer_text(), Some("  "));
}

#[test]
fn adopted_legacy_snapshot_trims_padding_but_keeps_nonblank_occupancy() {
    let screen: ScreenSnapshot = serde_json::from_value(json!({
        "schema_version": 1,
        "rows": 1,
        "cols": 8,
        "cursor": {"row": 0, "col": 3, "visible": true},
        "alternate_screen": false,
        "cells": [[
            {"symbol": "A"},
            {"symbol": " "},
            {"symbol": "B"},
            {"symbol": " "},
            {"symbol": " "},
            {"symbol": " "},
            {"symbol": " "},
            {"symbol": " "}
        ]],
        "source": {
            "pty_log_offset": 3,
            "raw_replay_start_offset": 0,
            "raw_replay_end_offset": 3
        },
        "captured_at": "2026-07-15T00:00:00Z"
    }))
    .expect("legacy v1 snapshot");
    assert!(screen.cells[0].iter().all(|cell| !cell.occupied));

    let mut cockpit = TerminalEmulator::new(1, 8, 20);
    cockpit.adopt_screen_snapshot(&screen);
    let found = cockpit
        .search_scrollback("A B", TerminalSearchDirection::First)
        .expect("legacy nonblank cells remain searchable");

    assert_eq!(found.line, "A B");
    assert_eq!(found.matched_text, "A B");
    let mut app = cockpit_app_with_snapshot(cockpit.snapshot());
    app.begin_search_mode();
    app.search_query = "A B".to_string();
    app.set_agent_search_match("search", &found);
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 1),
        KeyAction::CopySearchMatch
    );
    assert_eq!(app.copy_buffer_text(), Some("A B"));
    assert!(cockpit
        .search_scrollback("B ", TerminalSearchDirection::First)
        .is_none());
}

#[test]
fn right_margin_width_expansion_preserves_soft_wrap_and_scrollback() {
    let chunks: &[&[u8]] = &[b"abc", "\u{2764}".as_bytes(), "\u{fe0f}".as_bytes()];
    let mut durable = TerminalStateBuffer::new(2, 4, 256, 0);
    let mut cockpit = TerminalEmulator::new(2, 4, 10);
    process_both(&mut durable, &mut cockpit, chunks);
    durable.finish_input();
    cockpit.finish_input();

    let snapshot = cockpit.snapshot();
    assert_parser_and_renderer_parity(&durable, &snapshot);
    assert_eq!(snapshot.cells[0][0].symbol, "a");
    assert_eq!(snapshot.cells[0][1].symbol, "b");
    assert_eq!(snapshot.cells[0][2].symbol, "c");
    assert!(!snapshot.cells[0][3].occupied);
    assert_eq!(snapshot.cells[1][0].symbol, "\u{2764}\u{fe0f}");
    assert!(snapshot.cells[1][1].continuation);
    assert_eq!((snapshot.cursor_row, snapshot.cursor_col), (1, 2));

    let mut parser = vt100::Parser::new(2, 4, 10);
    for chunk in chunks {
        parser.process(chunk);
    }
    parser.finish();
    assert!(parser.screen().row_wrapped(0));
    assert_eq!(parser.screen().contents(), "abc\u{2764}\u{fe0f}");

    let formatted = parser.screen().contents_formatted();
    let mut replay = vt100::Parser::new(2, 4, 10);
    replay.process(&formatted);
    assert!(replay.screen().row_wrapped(0));
    assert_eq!(
        replay.screen().cell(1, 0).unwrap().contents(),
        "\u{2764}\u{fe0f}"
    );
    assert_eq!(replay.screen().cursor_position(), (1, 2));

    parser.process(b"XYZ");
    parser.screen_mut().set_scrollback(usize::MAX);
    assert_eq!(parser.screen().scrollback(), 1);
    assert!(parser.screen().row_wrapped(0));
    assert_eq!(parser.screen().cell(0, 0).unwrap().contents(), "a");
    assert_eq!(parser.screen().cell(0, 2).unwrap().contents(), "c");
    assert_eq!(
        parser.screen().cell(1, 0).unwrap().contents(),
        "\u{2764}\u{fe0f}"
    );
}

#[test]
fn every_width_class_wraps_at_the_same_authoritative_boundary() {
    let fixtures: &[WrapFixture<'_>] = &[
        ("space", &[b"abc "], (0, 4)),
        ("combining", &[b"abce", "\u{301}".as_bytes()], (0, 4)),
        ("ambiguous", &[b"abc", "\u{00b7}".as_bytes()], (0, 4)),
        ("cjk", &[b"abc", "\u{754c}".as_bytes()], (1, 2)),
        (
            "variation selector",
            &[b"abc", "\u{2764}".as_bytes(), "\u{fe0f}".as_bytes()],
            (1, 2),
        ),
        (
            "emoji zwj",
            &[
                b"abc",
                "\u{1f469}".as_bytes(),
                "\u{200d}".as_bytes(),
                "\u{1f4bb}".as_bytes(),
            ],
            (1, 2),
        ),
        ("invalid", &[b"abc", &[0xff]], (0, 4)),
    ];

    for (name, chunks, expected_cursor) in fixtures {
        let mut durable = TerminalStateBuffer::new(3, 4, 128, 0);
        let mut cockpit = TerminalEmulator::new(3, 4, 10);
        process_both(&mut durable, &mut cockpit, chunks);
        durable.finish_input();
        cockpit.finish_input();
        let snapshot = cockpit.snapshot();
        assert_eq!(
            (snapshot.cursor_row, snapshot.cursor_col),
            *expected_cursor,
            "{name}"
        );
        assert_parser_and_renderer_parity(&durable, &snapshot);
    }
}

#[test]
fn parser_adapter_and_ratatui_compat_widths_cannot_drift_silently() {
    let cases = [
        (" ", 1),
        ("e\u{301}", 1),
        ("\u{754c}", 2),
        ("\u{2764}\u{fe0f}", 2),
        ("\u{1f469}\u{200d}\u{1f4bb}", 2),
        ("\u{00b7}", 1),
        ("\u{fffd}", 1),
    ];

    for (text, expected) in cases {
        assert_eq!(vt100::width::terminal_text_width(text), expected, "parser");
        assert_eq!(core_text_width(text), expected, "core adapter");
        assert_eq!(text.width(), expected, "Ratatui compatibility width");
    }
}

#[test]
fn sequential_shrinks_sanitize_wide_cells_and_never_render_past_the_rect() {
    let mut durable = TerminalStateBuffer::new(2, 4, 64, 0);
    let mut cockpit = TerminalEmulator::new(2, 4, 20);
    process_both(
        &mut durable,
        &mut cockpit,
        &[b"A", "\u{754c}".as_bytes(), b"B"],
    );
    durable.resize(2, 2);
    cockpit.resize(2, 2);
    durable.resize(1, 1);
    cockpit.resize(1, 1);

    let snapshot = cockpit.snapshot();
    assert_parser_and_renderer_parity(&durable, &snapshot);
    assert!(snapshot.cells[0]
        .iter()
        .enumerate()
        .all(|(index, cell)| !cell.continuation
            || (index > 0 && snapshot.cells[0][index - 1].width == 2)));

    let backend = TestBackend::new(2, 1);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| render_terminal_snapshot(frame, Rect::new(0, 0, 2, 1), &snapshot))
        .expect("initial draw");
    terminal.backend_mut().resize(1, 1);
    terminal
        .resize(Rect::new(0, 0, 1, 1))
        .expect("resize terminal");
    terminal
        .draw(|frame| render_terminal_snapshot(frame, Rect::new(0, 0, 1, 1), &snapshot))
        .expect("narrow draw");
    assert_ne!(
        terminal
            .backend()
            .buffer()
            .cell((0, 0))
            .expect("cell")
            .symbol(),
        "\u{754c}"
    );
}

#[test]
fn scrolled_wide_edge_is_sanitized_before_narrow_snapshot_search_and_render() {
    let mut durable = TerminalStateBuffer::new(2, 3, 64, 0);
    let mut cockpit = TerminalEmulator::new(2, 3, 20);
    process_both(
        &mut durable,
        &mut cockpit,
        &[b"A", "\u{754c}".as_bytes(), b"\r\nX\r\nY"],
    );

    durable.resize(2, 2);
    cockpit.resize(2, 2);

    let durable_snapshot = durable.screen_snapshot();
    assert_eq!((durable_snapshot.rows, durable_snapshot.cols), (2, 2));
    assert!(durable_snapshot.cells.iter().all(|row| {
        row.iter().enumerate().all(|(col, cell)| {
            cell.width != 2 || (col + 1 < row.len() && row[col + 1].continuation)
        })
    }));
    assert!(cockpit
        .search_scrollback("\u{754c}", TerminalSearchDirection::First)
        .is_none());
    cockpit
        .search_scrollback("A", TerminalSearchDirection::First)
        .expect("sanitized history keeps the bounded lead cell");

    let snapshot = cockpit.snapshot();
    assert_eq!((snapshot.rows, snapshot.cols), (2, 2));
    assert!(snapshot.cells.iter().all(|row| {
        row.iter().enumerate().all(|(col, cell)| {
            cell.width != 2 || (col + 1 < row.len() && row[col + 1].continuation)
        })
    }));
    assert!(snapshot.plain_lines().iter().any(|line| line.contains('A')));
    assert!(snapshot
        .plain_lines()
        .iter()
        .all(|line| !line.contains('\u{754c}')));

    let backend = TestBackend::new(2, 2);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| render_terminal_snapshot(frame, Rect::new(0, 0, 2, 2), &snapshot))
        .expect("render sanitized scrollback snapshot");
    assert!(terminal.backend().buffer().cell((1, 1)).is_some());
}

#[test]
fn one_column_wide_graphemes_keep_policy_width_without_rendering_past_the_rect() {
    let mut durable = TerminalStateBuffer::new(1, 1, 64, 0);
    let mut cockpit = TerminalEmulator::new(1, 1, 20);
    process_both(&mut durable, &mut cockpit, &["\u{754c}".as_bytes()]);
    durable.finish_input();
    cockpit.finish_input();

    let snapshot = cockpit.snapshot();
    assert_eq!(durable.screen_snapshot().cells[0][0].width, 2);
    assert_eq!(snapshot.cells[0][0].symbol, "\u{754c}");
    assert_eq!(snapshot.cells[0][0].width, 2);
    assert!(!snapshot.cells[0][0].continuation);
    assert_parser_and_renderer_parity(&durable, &snapshot);
}

#[test]
fn sequential_wide_to_narrow_keeps_history_metadata_and_clips_test_backend_output() {
    let mut cockpit = TerminalEmulator::new(1, 2, 20);
    cockpit.process("\u{754c}".as_bytes());
    cockpit.resize(1, 1);
    assert_eq!(cockpit.snapshot().cells[0][0].width, 2);

    cockpit.process_text("\r\nX");
    cockpit.scroll_up(1);
    let history = cockpit.snapshot();
    assert_eq!(history.cells[0][0].symbol, "\u{754c}");
    assert_eq!(history.cells[0][0].width, 2);

    let backend = TestBackend::new(1, 1);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| render_terminal_snapshot(frame, Rect::new(0, 0, 1, 1), &history))
        .expect("narrow history draw");
    assert_eq!(
        terminal
            .backend()
            .buffer()
            .cell((0, 0))
            .expect("clipped cell")
            .symbol(),
        " "
    );
}

fn cockpit_app_with_snapshot(snapshot: TerminalSnapshot) -> AppModel {
    let agent = session_summary("width-agent", SessionRole::Agent);
    let daemon = session_summary("width-daemon", SessionRole::MillraceDaemon);
    let daemon_id = daemon.session_id;
    AppModel::agent_cockpit(
        UiId::new(),
        agent,
        vec![daemon],
        Some(daemon_id),
        BTreeMap::new(),
        AgentTerminalPane::with_snapshot(snapshot, true, false),
        AgentCockpitLayout::Right,
        MonitorProfile::Basic,
    )
}

fn session_summary(name: &str, role: SessionRole) -> SessionSummary {
    let cwd = PathBuf::from(format!("/tmp/{name}"));
    SessionSummary {
        session_id: SessionId::new(),
        name: Some(name.to_string()),
        role,
        spawn_mode: SpawnMode::Pty,
        process_state: ProcessState::Running,
        attention_state: millrace_sessions_core::state::AttentionState::Idle,
        attention: Default::default(),
        status_summary: Default::default(),
        failure_message: None,
        workspace: Some(WorkspaceIdentity {
            canonical_path: cwd.clone(),
            unix_device: None,
            unix_inode: None,
        }),
        cwd,
        argv: vec![name.to_string()],
        monitor_profile: MonitorProfile::Auto,
        created_at: "2026-07-10T00:00:00Z".to_string(),
        updated_at: "2026-07-10T00:00:01Z".to_string(),
        stop_requested_at: None,
        stop_reason: None,
        attached_clients: 0,
        input_owner: None,
        capabilities: millrace_sessions_core::protocol::SessionCapabilities::for_spawn_mode(
            SpawnMode::Pty,
        ),
        artifacts: millrace_sessions_core::protocol::SessionArtifacts::default(),
        liveness: Default::default(),
    }
}
