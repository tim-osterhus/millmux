use std::{fs, path::Path, process::Command, thread, time::Duration};

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
