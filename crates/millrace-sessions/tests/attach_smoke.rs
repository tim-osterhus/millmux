use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

use assert_cmd::prelude::*;
use nix::{
    sys::signal::{kill, Signal},
    unistd::Pid,
};
use serde_json::Value;

struct TempHost {
    state_dir: tempfile::TempDir,
}

impl TempHost {
    fn new() -> Self {
        Self {
            state_dir: tempfile::tempdir().expect("temp state dir"),
        }
    }

    fn state_dir(&self) -> &Path {
        self.state_dir.path()
    }
}

impl Drop for TempHost {
    fn drop(&mut self) {
        let host_json = self.state_dir.path().join("host.json");
        let Ok(raw) = fs::read_to_string(host_json) else {
            return;
        };
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            return;
        };
        let Some(pid) = value.get("pid").and_then(Value::as_u64) else {
            return;
        };

        let pid = Pid::from_raw(pid as i32);
        let _ = kill(pid, Signal::SIGTERM);
        for _ in 0..40 {
            if kill(pid, None).is_err() {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        let _ = kill(pid, Signal::SIGKILL);
    }
}

#[test]
fn cli_smoke_send_logs_events_resize_and_stream_through_host() {
    let host = TempHost::new();
    let workspace = tempfile::tempdir().expect("workspace");

    let session_id = start_session(
        &host,
        workspace.path(),
        "interactive",
        "printf 'ready\\n'; while IFS= read -r line; do printf 'got:%s\\n' \"$line\"; done",
    );
    wait_for_logs(&host, &session_id, "ready");
    assert_cli_json_attach_state_consistency(&host, &session_id, 0, Value::Null);

    millmux_command(&host)
        .args(["send", &session_id, "--text", "ping\n"])
        .assert()
        .success();
    wait_for_logs(&host, &session_id, "got:ping");

    let logs = millmux_command(&host)
        .args(["logs", &session_id, "--tail", "1", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let logs: Value = serde_json::from_slice(&logs).expect("logs json");
    assert_eq!(logs["lines"][0]["line"], "got:ping");

    let events = millmux_command(&host)
        .args(["events", &session_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let events: Value = serde_json::from_slice(&events).expect("events json");
    assert!(events["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| { event["kind"] == "input_sent" }));

    let resize = millmux_command(&host)
        .args([
            "resize",
            &session_id,
            "--rows",
            "30",
            "--cols",
            "100",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let resize: Value = serde_json::from_slice(&resize).expect("resize json");
    assert_eq!(resize["rows"], 30);
    assert_eq!(resize["cols"], 100);

    let attach_id = start_session(
        &host,
        workspace.path(),
        "attach",
        "printf 'attach-ready\\n'; sleep 3",
    );
    wait_for_logs(&host, &attach_id, "attach-ready");
    wait_for_attach_output(&host, &attach_id, "attach-ready");
}

#[test]
fn cli_smoke_raw_attach_replay_none_preserves_live_bytes() {
    let host = TempHost::new();
    let workspace = tempfile::tempdir().expect("workspace");

    let session_id = start_session(
        &host,
        workspace.path(),
        "raw-attach",
        "printf 'ready-before-raw\\n'; while [ ! -f go-raw ]; do sleep 0.05; done; printf '\\377raw-live\\n'",
    );
    wait_for_logs(&host, &session_id, "ready-before-raw");

    let mut attach = millmux_command(&host);
    let attach = attach
        .args([
            "attach",
            &session_id,
            "--read-only",
            "--raw",
            "--replay",
            "none",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn raw attach");

    wait_for_attached_clients(&host, &session_id, 1);
    fs::write(workspace.path().join("go-raw"), b"go").expect("release raw fixture");

    let output = attach.wait_with_output().expect("wait for raw attach");
    assert!(
        output.status.success(),
        "raw attach failed: status={:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        contains_bytes(&output.stdout, b"\xffraw-live"),
        "raw attach did not preserve invalid live bytes: {:?}",
        output.stdout
    );
    assert!(
        !contains_bytes(&output.stdout, b"ready-before-raw"),
        "raw attach --replay none unexpectedly replayed legacy scrollback: {:?}",
        output.stdout
    );
}

#[test]
fn cli_follow_logs_and_events_stream_late_output() {
    let host = TempHost::new();
    let workspace = tempfile::tempdir().expect("workspace");

    let logs_session_id = start_session(
        &host,
        workspace.path(),
        "logs-follow",
        "printf 'first\\n'; sleep 1; printf 'second\\n'",
    );
    wait_for_logs(&host, &logs_session_id, "first");

    let logs = millmux_command(&host)
        .args(["logs", &logs_session_id, "--follow"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let logs = String::from_utf8_lossy(&logs);
    assert!(logs.contains("first"), "{logs}");
    assert!(logs.contains("second"), "{logs}");

    let events_session_id = start_session(
        &host,
        workspace.path(),
        "events-follow",
        "printf 'event-first\\n'; sleep 1; printf 'event-second\\n'",
    );
    wait_for_logs(&host, &events_session_id, "event-first");

    let events = millmux_command(&host)
        .args(["events", &events_session_id, "--follow", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let events = String::from_utf8_lossy(&events);
    let frames = events
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("event follow json line"))
        .collect::<Vec<_>>();
    assert!(
        frames
            .first()
            .and_then(|frame| frame.get("events"))
            .and_then(Value::as_array)
            .is_some(),
        "{events}"
    );
    assert!(
        frames
            .iter()
            .skip(1)
            .any(|frame| { frame["type"] == "event" && frame["event"]["kind"] == "output" }),
        "{events}"
    );
}

fn assert_cli_json_attach_state_consistency(
    host: &TempHost,
    session_id: &str,
    attached_clients: u64,
    input_owner: Value,
) {
    let listed = millmux_command(host)
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let listed: Value = serde_json::from_slice(&listed).expect("list json");
    let listed_session = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["session_id"] == session_id)
        .unwrap_or_else(|| panic!("missing session {session_id} in {listed:#}"));

    let status = millmux_command(host)
        .args(["status", session_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status: Value = serde_json::from_slice(&status).expect("status json");

    let inspect = millmux_command(host)
        .args(["inspect", session_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let inspect: Value = serde_json::from_slice(&inspect).expect("inspect json");

    for session in [listed_session, &status["session"], &inspect["session"]] {
        assert_eq!(session["attached_clients"], attached_clients, "{session:#}");
        assert_eq!(session["input_owner"], input_owner, "{session:#}");
    }
}

fn start_session(host: &TempHost, workspace: &Path, name: &str, script: &str) -> String {
    let output = millmux_command(host)
        .args([
            "start",
            "--json",
            "--name",
            name,
            "--role",
            "shell",
            "--workspace",
        ])
        .arg(workspace)
        .arg("--cwd")
        .arg(workspace)
        .args(["--", "sh", "-c", script])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).expect("start json");
    value["session"]["session_id"].as_str().unwrap().to_string()
}

fn wait_for_logs(host: &TempHost, session_id: &str, needle: &str) {
    for _ in 0..120 {
        let output = millmux_command(host)
            .args(["logs", session_id])
            .output()
            .expect("run logs");
        if output.status.success() && String::from_utf8_lossy(&output.stdout).contains(needle) {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("logs for {session_id} did not contain {needle:?}");
}

fn wait_for_attach_output(host: &TempHost, session_id: &str, needle: &str) {
    let mut last_output = String::new();
    for _ in 0..120 {
        let output = millmux_command(host)
            .args(["attach", session_id, "--read-only"])
            .output()
            .expect("run attach");
        last_output = String::from_utf8_lossy(&output.stdout).to_string();
        if output.status.success() && last_output.contains(needle) {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("attach output for {session_id} did not contain {needle:?}: {last_output}");
}

fn wait_for_attached_clients(host: &TempHost, session_id: &str, expected: u64) {
    for _ in 0..120 {
        let output = millmux_command(host)
            .args(["status", session_id, "--json"])
            .output()
            .expect("run status");
        if output.status.success() {
            let value: Value = serde_json::from_slice(&output.stdout).expect("status json");
            if value["session"]["attached_clients"] == expected {
                return;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("status for {session_id} did not report attached_clients={expected}");
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn millmux_command(host: &TempHost) -> Command {
    let mut command = Command::cargo_bin("millmux").expect("millmux binary");
    command.env("MILLMUX_STATE_DIR", host.state_dir());
    command.env(
        "MILLMUX_HOST_BIN",
        binary_override("MILLMUX_HOST_BIN", "millrace-sessiond"),
    );
    command.env(
        "MILLMUX_WORKER_BIN",
        binary_override("MILLMUX_WORKER_BIN", "millrace-session-worker"),
    );
    command
}

fn binary_override(name: &str, binary_name: &str) -> std::path::PathBuf {
    if let Some(value) = std::env::var_os(name) {
        let path = std::path::PathBuf::from(value);
        if path.is_absolute() {
            return path;
        }
        return workspace_root().join(path);
    }

    workspace_root()
        .join("target")
        .join("debug")
        .join(binary_name)
}

fn workspace_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}
