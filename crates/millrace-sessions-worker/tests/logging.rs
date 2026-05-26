use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
};

use millrace_sessions_core::{
    events::read_events,
    scrollback::{
        restore_terminal_replay, ScrollbackBuffer, TerminalSnapshot, TerminalStateBuffer,
        DEFAULT_TERMINAL_COLS, DEFAULT_TERMINAL_ROWS,
    },
    storage::read_json,
};
use millrace_sessions_worker::logging::{OutputLogger, OutputLoggerConfig};

#[test]
fn logging_appends_raw_bytes_events_and_scrollback() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("pty.log"), b"previous\n").unwrap();
    let paths = output_logger_config(temp.path(), 3, 1024);
    let mut logger = OutputLogger::new(paths).expect("logger");

    logger.record_output(b"hello").expect("partial output");
    logger
        .record_output(b" world\none\ntwo\nthree\n")
        .expect("more output");
    logger.flush().expect("flush logger");

    assert_eq!(
        fs::read(temp.path().join("pty.log")).unwrap(),
        b"previous\nhello world\none\ntwo\nthree\n"
    );
    let events = read_events(temp.path().join("events.jsonl")).unwrap();
    assert_eq!(events.len(), 4);
    assert_eq!(events[0].message.as_deref(), Some("hello world"));
    assert_eq!(
        events[0].fields.get("stream").map(String::as_str),
        Some("pty")
    );

    let scrollback = ScrollbackBuffer::restore_snapshot(temp.path().join("scrollback.snapshot"))
        .expect("restore scrollback");
    assert_eq!(scrollback.lines(), vec!["one", "two", "three"]);
}

#[cfg(unix)]
#[test]
fn logging_creates_private_log_event_and_scrollback_files() {
    let temp = tempfile::tempdir().unwrap();
    let mut logger = OutputLogger::new(OutputLoggerConfig {
        scrollback_capacity: 10,
        ..output_logger_config(temp.path(), 10, 1024)
    })
    .expect("logger");

    logger.record_output(b"ready\n").expect("output");
    logger.flush().expect("flush logger");

    assert_private_file(&temp.path().join("pty.log"));
    assert_private_file(&temp.path().join("events.jsonl"));
    assert_private_file(&temp.path().join("scrollback.snapshot"));
    assert_private_file(&temp.path().join("terminal.snapshot.json"));
    assert_private_file(&temp.path().join("pty.replay"));
}

#[test]
fn logging_keeps_partial_utf8_out_of_structured_lines_until_complete() {
    let temp = tempfile::tempdir().unwrap();
    let mut logger =
        OutputLogger::new(output_logger_config(temp.path(), 10, 1024)).expect("logger");

    logger.record_output(&[0xE2, 0x82]).expect("partial utf8");
    logger.record_output(&[0xAC, b'\n']).expect("complete utf8");

    let events = read_events(temp.path().join("events.jsonl")).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].message.as_deref(), Some("€"));
}

#[test]
fn logging_fixture_preserves_full_screen_agent_protocol_artifacts() {
    let temp = tempfile::tempdir().unwrap();
    let mut logger =
        OutputLogger::new(output_logger_config(temp.path(), 20, 1024)).expect("logger");
    let output = full_screen_agent_fixture();

    for chunk in output.chunks(13) {
        logger.record_output(chunk).expect("streaming output");
    }
    logger.flush().expect("flush logger");

    assert_eq!(fs::read(temp.path().join("pty.log")).unwrap(), output);
    let events = read_events(temp.path().join("events.jsonl")).unwrap();
    let event_text = events
        .iter()
        .filter_map(|event| event.message.as_deref())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(event_text.contains("\x1b[?1049h"), "{event_text:?}");
    assert!(event_text.contains("\x1b[?1049l"), "{event_text:?}");
    assert!(event_text.contains("\x1b[2J"), "{event_text:?}");
    assert!(event_text.contains("\x1b[3J"), "{event_text:?}");
    assert!(event_text.contains("\x1b[H"), "{event_text:?}");
    assert!(event_text.contains("\x1b[4;9H"), "{event_text:?}");
    assert!(event_text.contains("\x1b[?2026h"), "{event_text:?}");
    assert!(event_text.contains("\x1b[?2026l"), "{event_text:?}");
    assert!(event_text.contains("\x1b[2K"), "{event_text:?}");
    assert!(event_text.contains("answer two chunk 3"), "{event_text:?}");

    let scrollback = ScrollbackBuffer::restore_snapshot(temp.path().join("scrollback.snapshot"))
        .expect("restore scrollback");
    assert!(
        legacy_line_scrollback_contains_terminal_protocol(&scrollback.lines()),
        "legacy line scrollback should be detectable as unsafe for TUI replay"
    );
}

#[test]
fn logging_persists_bounded_raw_replay_and_terminal_snapshot_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let mut logger = OutputLogger::new(output_logger_config(temp.path(), 20, 13)).expect("logger");

    logger
        .record_output(b"first line\r\nsecond line\r\n")
        .expect("output");
    logger.flush().expect("flush logger");

    assert_eq!(
        fs::read(temp.path().join("pty.log")).unwrap(),
        b"first line\r\nsecond line\r\n"
    );
    assert_eq!(
        fs::read(temp.path().join("pty.replay")).unwrap(),
        b"second line\r\n"
    );

    let snapshot: TerminalSnapshot = read_json(temp.path().join("terminal.snapshot.json")).unwrap();
    assert_eq!((snapshot.rows, snapshot.cols), (24, 80));
    assert_eq!(snapshot.pty_log_offset, 25);
    assert_eq!(snapshot.raw_replay_start_offset, 12);
    assert_eq!(snapshot.raw_replay_end_offset, 25);
    assert!(snapshot
        .screen
        .iter()
        .any(|line| line.contains("second line")));

    let replay = restore_terminal_replay(
        temp.path().join("terminal.snapshot.json"),
        temp.path().join("pty.replay"),
        snapshot.pty_log_offset,
    )
    .unwrap()
    .expect("fresh replay");
    assert_eq!(replay.bytes, b"second line\r\n");
}

#[test]
fn logging_marks_stale_terminal_replay_unavailable() {
    let temp = tempfile::tempdir().unwrap();
    let mut logger =
        OutputLogger::new(output_logger_config(temp.path(), 20, 1024)).expect("logger");

    logger.record_output(b"ready\r\n").expect("output");
    logger.flush().expect("flush logger");
    let snapshot: TerminalSnapshot = read_json(temp.path().join("terminal.snapshot.json")).unwrap();

    assert!(
        restore_terminal_replay(
            temp.path().join("terminal.snapshot.json"),
            temp.path().join("pty.replay"),
            snapshot.pty_log_offset + 1,
        )
        .unwrap()
        .is_none(),
        "offset mismatch must not offer raw replay"
    );
}

#[test]
fn logging_resize_updates_terminal_snapshot_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let mut logger =
        OutputLogger::new(output_logger_config(temp.path(), 20, 1024)).expect("logger");

    logger.record_resize(31, 99).expect("resize");
    logger.record_output(b"resized\r\n").expect("output");

    let snapshot: TerminalSnapshot = read_json(temp.path().join("terminal.snapshot.json")).unwrap();
    assert_eq!((snapshot.rows, snapshot.cols), (31, 99));
    assert!(snapshot.same_size(31, 99));
    assert!(snapshot.screen.iter().any(|line| line.contains("resized")));
}

fn full_screen_agent_fixture() -> Vec<u8> {
    concat!(
        "fixture-agent ready\r\n",
        "\x1b[?1049h",
        "\x1b[?2026h",
        "\x1b[2J",
        "\x1b[3J",
        "\x1b[H",
        "question one\r\n",
        "\x1b[4;9Hanswer one complete\r\n",
        "\x1b[2Kstream answer two chunk 1",
        "\rstream answer two chunk 2",
        "\rstream answer two chunk 3\r\n",
        "resize rows=18 cols=72\r\n",
        "\x1b[?2026l",
        "\x1b[?1049l",
        "answer two chunk 3\r\n",
    )
    .as_bytes()
    .to_vec()
}

fn legacy_line_scrollback_contains_terminal_protocol(lines: &[String]) -> bool {
    lines.iter().any(|line| {
        [
            "\x1b[?1049h",
            "\x1b[?1049l",
            "\x1b[2J",
            "\x1b[3J",
            "\x1b[H",
            "\x1b[?2026h",
            "\x1b[?2026l",
            "\x1b[2K",
        ]
        .iter()
        .any(|needle| line.contains(needle))
    })
}

fn output_logger_config(
    root: &Path,
    scrollback_capacity: usize,
    raw_replay_capacity: usize,
) -> OutputLoggerConfig {
    let pty_log = root.join("pty.log");
    let terminal_snapshot = root.join("terminal.snapshot.json");
    let raw_replay_ring = root.join("pty.replay");
    let current_pty_offset = fs::metadata(&pty_log)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let terminal_state = Arc::new(Mutex::new(
        TerminalStateBuffer::restore_or_new(
            &terminal_snapshot,
            &raw_replay_ring,
            current_pty_offset,
            DEFAULT_TERMINAL_ROWS,
            DEFAULT_TERMINAL_COLS,
            raw_replay_capacity,
        )
        .unwrap(),
    ));

    OutputLoggerConfig {
        session_id: millrace_sessions_core::ids::SessionId::new(),
        pty_log,
        events_jsonl: root.join("events.jsonl"),
        scrollback_snapshot: root.join("scrollback.snapshot"),
        terminal_snapshot,
        raw_replay_ring,
        terminal_state,
        scrollback_capacity,
    }
}

#[cfg(unix)]
fn assert_private_file(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600,
        "{} should be 0600",
        path.display()
    );
}
