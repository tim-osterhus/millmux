use std::fs;

use millrace_sessions_core::{events::read_events, scrollback::ScrollbackBuffer};
use millrace_sessions_worker::logging::{OutputLogger, OutputLoggerConfig};

#[test]
fn logging_appends_raw_bytes_events_and_scrollback() {
    let temp = tempfile::tempdir().unwrap();
    let paths = OutputLoggerConfig {
        session_id: millrace_sessions_core::ids::SessionId::new(),
        pty_log: temp.path().join("pty.log"),
        events_jsonl: temp.path().join("events.jsonl"),
        scrollback_snapshot: temp.path().join("scrollback.snapshot"),
        scrollback_capacity: 3,
    };
    fs::write(&paths.pty_log, b"previous\n").unwrap();
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
        session_id: millrace_sessions_core::ids::SessionId::new(),
        pty_log: temp.path().join("pty.log"),
        events_jsonl: temp.path().join("events.jsonl"),
        scrollback_snapshot: temp.path().join("scrollback.snapshot"),
        scrollback_capacity: 10,
    })
    .expect("logger");

    logger.record_output(b"ready\n").expect("output");
    logger.flush().expect("flush logger");

    assert_private_file(&temp.path().join("pty.log"));
    assert_private_file(&temp.path().join("events.jsonl"));
    assert_private_file(&temp.path().join("scrollback.snapshot"));
}

#[test]
fn logging_keeps_partial_utf8_out_of_structured_lines_until_complete() {
    let temp = tempfile::tempdir().unwrap();
    let mut logger = OutputLogger::new(OutputLoggerConfig {
        session_id: millrace_sessions_core::ids::SessionId::new(),
        pty_log: temp.path().join("pty.log"),
        events_jsonl: temp.path().join("events.jsonl"),
        scrollback_snapshot: temp.path().join("scrollback.snapshot"),
        scrollback_capacity: 10,
    })
    .expect("logger");

    logger.record_output(&[0xE2, 0x82]).expect("partial utf8");
    logger.record_output(&[0xAC, b'\n']).expect("complete utf8");

    let events = read_events(temp.path().join("events.jsonl")).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].message.as_deref(), Some("€"));
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
