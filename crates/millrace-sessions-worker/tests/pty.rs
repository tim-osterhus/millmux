use std::{collections::BTreeMap, fs, io::Read, path::PathBuf};

use millrace_sessions_core::{protocol::ScreenColor, scrollback::TerminalStateBuffer};
use millrace_sessions_worker::pty::{spawn_pty, PtyCommandSpec};

#[test]
fn pty_spawns_exact_argv_in_stored_cwd() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("marker.txt"), "ready").unwrap();

    let mut child = spawn_pty(PtyCommandSpec {
        argv: vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf '%s:%s:%s' \"$1\" \"$(cat marker.txt)\" \"$MILLMUX_UI_ID\"".to_string(),
            "script-name".to_string(),
            "literal-value".to_string(),
        ],
        cwd: temp.path().to_path_buf(),
        env: BTreeMap::from([("MILLMUX_UI_ID".to_string(), "ui-test".to_string())]),
    })
    .expect("spawn pty command");

    let mut output = String::new();
    child
        .reader
        .read_to_string(&mut output)
        .expect("read pty output");
    let status = child.child.wait().expect("wait for child");

    assert!(status.success());
    assert!(output.contains("literal-value:ready:ui-test"));
    assert!(child.child_pid.is_some());
}

#[test]
fn pty_rejects_empty_argv_without_shell_fallback() {
    let error = spawn_pty(PtyCommandSpec {
        argv: Vec::new(),
        cwd: PathBuf::from("/tmp"),
        env: Default::default(),
    })
    .unwrap_err();

    assert!(
        error.to_string().contains("argv"),
        "error should explain argv validation: {error}"
    );
}

#[test]
fn pty_screen_snapshot_model_preserves_cells_styles_and_wide_continuations() {
    let mut state = TerminalStateBuffer::new(2, 8, 256, 0);
    state.process_output("\x1b[31;1mA\x1b[0m界".as_bytes());

    let snapshot = state.screen_snapshot();

    assert_eq!(snapshot.rows, 2);
    assert_eq!(snapshot.cols, 8);
    assert_eq!(snapshot.cells[0][0].symbol, "A");
    assert!(snapshot.cells[0][0].style.bold);
    assert_eq!(snapshot.cells[0][0].fg, ScreenColor::Indexed { index: 1 });
    assert_eq!(snapshot.cells[0][1].symbol, "界");
    assert_eq!(snapshot.cells[0][1].width, 2);
    assert!(snapshot.cells[0][2].continuation);
    assert_eq!(snapshot.cells[0][3].symbol, " ");
    assert_eq!(snapshot.cells[0][3].width, 1);
    assert_eq!(snapshot.cells[0][3].fg, ScreenColor::Default);
    assert!(!snapshot.cells[0][3].style.bold);
}
