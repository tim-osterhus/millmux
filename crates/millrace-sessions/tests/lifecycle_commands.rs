use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::Path,
    process::Command,
    thread,
    time::Duration,
};

use assert_cmd::prelude::*;
use millrace_sessions_core::protocol::{AttachStreamFrame, ControlResponse, SessionAttachResponse};
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

    fn current_host_pid(&self) -> Pid {
        let host_json = self.state_dir.path().join("host.json");
        let raw = fs::read_to_string(host_json).expect("host metadata exists");
        let value: Value = serde_json::from_str(&raw).expect("host metadata json");
        let pid = value
            .get("pid")
            .and_then(Value::as_u64)
            .expect("host pid is recorded");
        Pid::from_raw(pid as i32)
    }

    fn kill_host(&self) -> i32 {
        let pid = self.current_host_pid();
        kill(pid, Signal::SIGKILL).expect("kill host");
        for _ in 0..40 {
            if kill(pid, None).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        pid.as_raw()
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
fn lifecycle_commands_emit_stable_json_results() {
    let host = TempHost::new();
    let workspace = tempfile::tempdir().expect("workspace");

    let stop_id = start_session(
        &host,
        workspace.path(),
        "cli-stop",
        "trap 'exit 0' INT TERM; printf 'ready\\n'; while true; do sleep 1; done",
    );
    let stop_output = millmux_command(&host)
        .args(["stop", &stop_id, "--grace-seconds", "0", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stop: Value = serde_json::from_slice(&stop_output).expect("stop json");
    assert_eq!(stop["session_id"], stop_id);
    assert_eq!(stop["stop_requested"], true);
    assert_ne!(stop["process_state"], "killed");

    let kill_id = start_session(
        &host,
        workspace.path(),
        "cli-kill",
        "printf 'ready\\n'; while true; do sleep 1; done",
    );
    let kill_output = millmux_command(&host)
        .args(["kill", &kill_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let killed: Value = serde_json::from_slice(&kill_output).expect("kill json");
    assert_eq!(killed["session_id"], kill_id);
    assert_eq!(killed["kill_requested"], true);
    assert_eq!(killed["process_state"], "killed");

    let delete_id = start_session(
        &host,
        workspace.path(),
        "cli-delete",
        "printf 'ready\\n'; sleep 0.1; printf 'done\\n'",
    );
    let session_root = host.state_dir().join("sessions").join(&delete_id);
    wait_for_file_contains(&session_root.join("pty.log"), "done");
    wait_for_terminal_meta(&session_root.join("meta.json"));

    let delete_output = millmux_command(&host)
        .args(["delete", &delete_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let deleted: Value = serde_json::from_slice(&delete_output).expect("delete json");
    assert_eq!(deleted["session_id"], delete_id);
    assert_eq!(deleted["deleted"], true);
    assert_eq!(deleted["archived"], true);
    assert_eq!(deleted["purged"], false);
    assert!(deleted["archive_path"].as_str().is_some());

    let purge_output = millmux_command(&host)
        .args(["delete", &delete_id, "--purge", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let purged: Value = serde_json::from_slice(&purge_output).expect("purge json");
    assert_eq!(purged["session_id"], delete_id);
    assert_eq!(purged["deleted"], true);
    assert_eq!(purged["purged"], true);
}

#[test]
fn pipe_lifecycle_commands_emit_stable_json_results() {
    let host = TempHost::new();
    let workspace = tempfile::tempdir().expect("workspace");

    let stop_id = start_pipe_session(
        &host,
        workspace.path(),
        "cli-pipe-stop",
        "trap 'exit 0' TERM; printf 'pipe-ready\\n'; while true; do sleep 1; done",
    );
    let logs_output = millmux_command(&host)
        .args(["logs", "--json", &stop_id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let logs: Value = serde_json::from_slice(&logs_output).expect("pipe logs json");
    assert!(logs["lines"]
        .as_array()
        .unwrap()
        .iter()
        .any(|line| { line["stream"] == "stdout" && line["line"] == "pipe-ready" }));

    let events_output = millmux_command(&host)
        .args(["events", "--json", &stop_id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let events: Value = serde_json::from_slice(&events_output).expect("pipe events json");
    assert!(events["events"].as_array().unwrap().iter().any(|event| {
        event["kind"] == "output"
            && event["fields"]["stream"] == "stdout"
            && event["fields"]["record_kind"] == "chunk"
    }));

    let stop_output = millmux_command(&host)
        .args(["stop", &stop_id, "--grace-seconds", "1", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stopped: Value = serde_json::from_slice(&stop_output).expect("pipe stop json");
    assert_eq!(stopped["session_id"], stop_id);
    assert_eq!(stopped["stop_requested"], true);
    assert_eq!(stopped["stop_reason"], "session_stop");
    let stop_requested_at = stopped["stop_requested_at"]
        .as_str()
        .expect("stop requested timestamp");

    let session_root = host.state_dir().join("sessions").join(&stop_id);
    let meta: Value = serde_json::from_str(
        &fs::read_to_string(session_root.join("meta.json")).expect("pipe meta json"),
    )
    .expect("pipe meta value");
    assert_eq!(meta["stop_requested_at"].as_str(), Some(stop_requested_at));
    assert_eq!(meta["stop_reason"], "session_stop");
    let worker: Value = serde_json::from_str(
        &fs::read_to_string(session_root.join("worker.json")).expect("pipe worker json"),
    )
    .expect("pipe worker value");
    assert_eq!(
        worker["stop_requested_at"].as_str(),
        Some(stop_requested_at)
    );
    assert_eq!(worker["stop_reason"], "session_stop");

    let post_stop_events_output = millmux_command(&host)
        .args(["events", "--json", &stop_id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let post_stop_events: Value =
        serde_json::from_slice(&post_stop_events_output).expect("post-stop pipe events json");
    let post_stop_events = post_stop_events["events"].as_array().unwrap();
    assert!(post_stop_events.iter().any(|event| {
        event["kind"] == "stop_requested"
            && event["fields"]["reason"] == "session_stop"
            && event["fields"]["stop_requested_at"].as_str() == Some(stop_requested_at)
    }));
    assert!(post_stop_events.iter().any(|event| {
        event["kind"] == "stop_requested"
            && event["fields"]["reason"] == "sigterm_stop"
            && event["fields"]["signal"] == "SIGTERM"
    }));

    let delete_output = millmux_command(&host)
        .args(["delete", &stop_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let deleted: Value = serde_json::from_slice(&delete_output).expect("pipe delete json");
    assert_eq!(deleted["session_id"], stop_id);
    assert_eq!(deleted["archived"], true);

    let kill_id = start_pipe_session(
        &host,
        workspace.path(),
        "cli-pipe-kill",
        "printf 'pipe-kill-ready\\n'; while true; do sleep 1; done",
    );
    let kill_output = millmux_command(&host)
        .args(["kill", &kill_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let killed: Value = serde_json::from_slice(&kill_output).expect("pipe kill json");
    assert_eq!(killed["session_id"], kill_id);
    assert_eq!(killed["kill_requested"], true);
    assert_eq!(killed["process_state"], "killed");

    let purge_output = millmux_command(&host)
        .args(["delete", &kill_id, "--purge", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let purged: Value = serde_json::from_slice(&purge_output).expect("pipe purge json");
    assert_eq!(purged["session_id"], kill_id);
    assert_eq!(purged["purged"], true);
}

#[test]
fn restart_pty_commands_autostart_host_and_preserve_supported_surfaces() {
    let host = TempHost::new();
    let workspace = tempfile::tempdir().expect("workspace");

    let session_id = start_session(
        &host,
        workspace.path(),
        "cli-restart-pty",
        "printf 'ready\\n'; while IFS= read -r line; do printf 'got:%s\\n' \"$line\"; done",
    );
    let session_root = host.state_dir().join("sessions").join(&session_id);
    let worker = read_json_file(&session_root.join("worker.json"));
    let worker_pid = worker["pid"].as_i64().expect("worker pid") as i32;
    let child_pid = worker["child_pid"].as_i64().expect("child pid") as i32;

    let old_host_pid = host.kill_host();
    assert_pid_alive(worker_pid, "worker after CLI host kill");
    assert_pid_alive(child_pid, "child after CLI host kill");

    let status_output = millmux_command(&host)
        .args(["status", &session_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status: Value = serde_json::from_slice(&status_output).expect("status json");
    assert_eq!(status["session"]["process_state"], "running");
    assert_eq!(status["session"]["spawn_mode"], "pty");
    assert_eq!(status["session"]["liveness"]["worker"], "alive");
    assert_eq!(status["session"]["liveness"]["child"], "alive");
    assert_ne!(host.current_host_pid().as_raw(), old_host_pid);

    let inspect_output = millmux_command(&host)
        .args(["inspect", &session_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let inspect: Value = serde_json::from_slice(&inspect_output).expect("inspect json");
    assert_eq!(inspect["session"]["process_state"], "running");
    assert_eq!(inspect["worker"]["pid"], worker_pid);

    let logs_output = millmux_command(&host)
        .args(["logs", "--json", "--tail", "10", &session_id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let logs: Value = serde_json::from_slice(&logs_output).expect("logs json");
    assert!(logs["lines"]
        .as_array()
        .unwrap()
        .iter()
        .any(|line| line["line"]
            .as_str()
            .is_some_and(|line| line.contains("ready"))));

    let events_output = millmux_command(&host)
        .args(["events", "--json", &session_id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let events: Value = serde_json::from_slice(&events_output).expect("events json");
    assert!(events["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| event["kind"] == "output"));

    let send_output = millmux_command(&host)
        .args(["send", &session_id, "--text", "after-restart\n", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let sent: Value = serde_json::from_slice(&send_output).expect("send json");
    assert_eq!(sent["session_id"], session_id);
    wait_for_file_contains(&session_root.join("pty.log"), "got:after-restart");
    assert_attach_scrollback_contains(&host, &session_id, "ready");

    millmux_command(&host)
        .args(["kill", &session_id, "--json"])
        .assert()
        .success();
}

#[test]
fn restart_pipe_commands_autostart_host_and_preserve_supported_surfaces() {
    let host = TempHost::new();
    let workspace = tempfile::tempdir().expect("workspace");

    let session_id = start_pipe_session(
        &host,
        workspace.path(),
        "cli-restart-pipe",
        "trap 'exit 0' TERM; printf 'ready\\n'; while true; do sleep 1; done",
    );
    let session_root = host.state_dir().join("sessions").join(&session_id);
    let worker = read_json_file(&session_root.join("worker.json"));
    let worker_pid = worker["pid"].as_i64().expect("worker pid") as i32;
    let child_pid = worker["child_pid"].as_i64().expect("child pid") as i32;

    let old_host_pid = host.kill_host();
    assert_pid_alive(worker_pid, "pipe worker after CLI host kill");
    assert_pid_alive(child_pid, "pipe child after CLI host kill");

    let status_output = millmux_command(&host)
        .args(["status", &session_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status: Value = serde_json::from_slice(&status_output).expect("pipe status json");
    assert_eq!(status["session"]["process_state"], "running");
    assert_eq!(status["session"]["spawn_mode"], "pipe");
    assert_eq!(status["session"]["liveness"]["worker"], "alive");
    assert_eq!(status["session"]["liveness"]["child"], "alive");
    assert_ne!(host.current_host_pid().as_raw(), old_host_pid);

    let inspect_output = millmux_command(&host)
        .args(["inspect", &session_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let inspect: Value = serde_json::from_slice(&inspect_output).expect("pipe inspect json");
    assert_eq!(inspect["session"]["spawn_mode"], "pipe");
    assert_eq!(inspect["worker"]["pid"], worker_pid);

    let logs_output = millmux_command(&host)
        .args(["logs", "--json", &session_id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let logs: Value = serde_json::from_slice(&logs_output).expect("pipe logs json");
    assert!(logs["lines"]
        .as_array()
        .unwrap()
        .iter()
        .any(|line| { line["stream"] == "stdout" && line["line"].as_str() == Some("ready") }));

    let events_output = millmux_command(&host)
        .args(["events", "--json", &session_id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let events: Value = serde_json::from_slice(&events_output).expect("pipe events json");
    assert!(events["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| { event["kind"] == "output" && event["fields"]["stream"] == "stdout" }));

    let stop_output = millmux_command(&host)
        .args(["stop", &session_id, "--grace-seconds", "2", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stopped: Value = serde_json::from_slice(&stop_output).expect("pipe stop json");
    assert_eq!(stopped["session_id"], session_id);
    assert_eq!(stopped["stop_requested"], true);
    wait_for_terminal_meta(&session_root.join("meta.json"));

    let delete_output = millmux_command(&host)
        .args(["delete", &session_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let deleted: Value = serde_json::from_slice(&delete_output).expect("pipe delete json");
    assert_eq!(deleted["session_id"], session_id);
    assert_eq!(deleted["archived"], true);
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
        .args(["--cwd"])
        .arg(workspace)
        .args(["--", "sh", "-c", script])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).expect("start json");
    let session_id = value["session"]["session_id"]
        .as_str()
        .expect("session id")
        .to_string();
    wait_for_file_contains(
        &host
            .state_dir()
            .join("sessions")
            .join(&session_id)
            .join("pty.log"),
        "ready",
    );
    session_id
}

fn start_pipe_session(host: &TempHost, workspace: &Path, name: &str, script: &str) -> String {
    let output = millmux_command(host)
        .args([
            "start",
            "--json",
            "--spawn-mode",
            "pipe",
            "--name",
            name,
            "--role",
            "shell",
            "--workspace",
        ])
        .arg(workspace)
        .args(["--cwd"])
        .arg(workspace)
        .args(["--", "sh", "-c", script])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).expect("pipe start json");
    assert_eq!(value["session"]["spawn_mode"], "pipe");
    let session_id = value["session"]["session_id"]
        .as_str()
        .expect("session id")
        .to_string();
    wait_for_file_contains(
        &host
            .state_dir()
            .join("sessions")
            .join(&session_id)
            .join("stdout.log"),
        "ready",
    );
    session_id
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

fn wait_for_file_contains(path: &Path, needle: &str) {
    for _ in 0..200 {
        if fs::read_to_string(path)
            .map(|raw| raw.contains(needle))
            .unwrap_or(false)
        {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("{} did not contain {needle:?}", path.display());
}

fn read_json_file(path: &Path) -> Value {
    let raw = fs::read_to_string(path).unwrap_or_else(|error| {
        panic!("failed to read {}: {error}", path.display());
    });
    serde_json::from_str(&raw).unwrap_or_else(|error| {
        panic!("failed to parse {} as json: {error}", path.display());
    })
}

fn assert_pid_alive(pid: i32, label: &str) {
    assert!(
        kill(Pid::from_raw(pid), None).is_ok(),
        "{label} pid {pid} should still be alive"
    );
}

fn assert_attach_scrollback_contains(host: &TempHost, session_id: &str, needle: &str) {
    let mut stream =
        UnixStream::connect(host.state_dir().join("session-control.sock")).expect("connect attach");
    stream
        .write_all(
            format!(
                "{}\n",
                serde_json::to_string(&serde_json::json!({
                    "id": "attach-after-cli-restart",
                    "method": "session.attach",
                    "params": {
                        "selector": {"type": "id", "session_id": session_id},
                        "read_only": true,
                        "replay": "line_scrollback"
                    }
                }))
                .unwrap()
            )
            .as_bytes(),
        )
        .unwrap();

    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut response_line = String::new();
    reader.read_line(&mut response_line).unwrap();
    let response: ControlResponse = serde_json::from_str(response_line.trim_end()).unwrap();
    assert!(response.ok, "{response:#?}");
    let attach: SessionAttachResponse = response.result_as().unwrap();
    assert!(attach.stream.read_only);

    let mut frame_line = String::new();
    reader.read_line(&mut frame_line).unwrap();
    let frame = AttachStreamFrame::from_json_line(frame_line.trim_end()).unwrap();
    assert!(
        matches!(&frame, AttachStreamFrame::Scrollback { lines } if lines.iter().any(|line| line.contains(needle))),
        "attach scrollback did not contain {needle:?}: {frame:?}"
    );
    stream
        .write_all(AttachStreamFrame::Close.to_json_line().unwrap().as_bytes())
        .unwrap();
}

fn wait_for_terminal_meta(path: &Path) {
    for _ in 0..200 {
        if let Ok(raw) = fs::read_to_string(path) {
            let value: Value = serde_json::from_str(&raw).expect("meta json");
            if !matches!(
                value["process_state"].as_str(),
                Some("starting" | "running")
            ) {
                return;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("{} did not reach terminal state", path.display());
}
