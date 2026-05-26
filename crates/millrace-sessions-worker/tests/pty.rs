use std::{collections::BTreeMap, fs, io::Read, path::PathBuf};

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
