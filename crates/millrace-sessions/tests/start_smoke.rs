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
fn cli_json_launches_host_worker_and_observes_output_after_exit() {
    let host = TempHost::new();
    let workspace = tempfile::tempdir().expect("workspace");

    let output = millmux_command(&host)
        .args([
            "start",
            "--json",
            "--name",
            "ready",
            "--role",
            "shell",
            "--workspace",
        ])
        .arg(workspace.path())
        .args(["--cwd"])
        .arg(workspace.path())
        .args(["--", "sh", "-c", "printf ready; sleep 0.1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value: Value = serde_json::from_slice(&output).expect("json output");
    assert_eq!(value["attached_existing"], false);
    assert!(value["session"]["session_id"].as_str().is_some());
    assert!(value.get("id").is_none());
    assert!(value.get("ok").is_none());

    let session_id = value["session"]["session_id"].as_str().unwrap();
    let session_root = host.state_dir().join("sessions").join(session_id);
    wait_for_file_contains(&session_root.join("pty.log"), "ready");
    assert!(session_root.join("worker.json").exists());
    wait_for_file_contains(&session_root.join("events.jsonl"), "\"output\"");
}

#[cfg(unix)]
#[test]
fn cli_json_launches_private_session_artifacts() {
    let host = TempHost::new();
    let workspace = tempfile::tempdir().expect("workspace");

    let output = millmux_command(&host)
        .args([
            "start",
            "--json",
            "--name",
            "private",
            "--role",
            "shell",
            "--workspace",
        ])
        .arg(workspace.path())
        .args(["--cwd"])
        .arg(workspace.path())
        .args(["--", "sh", "-c", "printf ready; sleep 0.1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value: Value = serde_json::from_slice(&output).expect("json output");
    let session_id = value["session"]["session_id"].as_str().unwrap();
    let session_root = host.state_dir().join("sessions").join(session_id);
    wait_for_file_contains(&session_root.join("pty.log"), "ready");
    wait_for_file_contains(&session_root.join("worker.json"), "\"session_id\"");
    wait_for_file_contains(&session_root.join("events.jsonl"), "\"output\"");
    wait_for_file_contains(&session_root.join("scrollback.snapshot"), "ready");

    assert_private_dir(&session_root);
    assert_private_file(&session_root.join("meta.json"));
    assert_private_file(&session_root.join("worker.json"));
    assert_private_file(&session_root.join("events.jsonl"));
    assert_private_file(&session_root.join("pty.log"));
    assert_private_file(&session_root.join("scrollback.snapshot"));
}

fn millmux_command(host: &TempHost) -> Command {
    let mut command = Command::cargo_bin("millmux").expect("millmux binary");
    command.env("MILLMUX_STATE_DIR", host.state_dir());
    command.env(
        "MILLMUX_HOST_BIN",
        binary_override("MILLMUX_HOST_BIN", "millrace-sessions", "millrace-sessiond"),
    );
    command.env(
        "MILLMUX_WORKER_BIN",
        binary_override(
            "MILLMUX_WORKER_BIN",
            "millrace-sessions",
            "millrace-session-worker",
        ),
    );
    command
}

fn binary_override(name: &str, package_name: &str, binary_name: &str) -> std::path::PathBuf {
    if let Some(value) = std::env::var_os(name) {
        let path = std::path::PathBuf::from(value);
        if path.is_absolute() {
            return path;
        }
        return workspace_root().join(path);
    }

    let path = workspace_root()
        .join("target")
        .join("debug")
        .join(binary_name);
    ensure_binary(&path, package_name, binary_name);
    path
}

fn ensure_binary(path: &Path, package_name: &str, binary_name: &str) {
    if is_executable(path) {
        return;
    }

    let status = Command::new("cargo")
        .args(["build", "-p", package_name, "--bin", binary_name])
        .current_dir(workspace_root())
        .status()
        .unwrap_or_else(|error| panic!("build {binary_name}: {error}"));
    assert!(status.success(), "failed to build {binary_name}");
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
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

#[cfg(unix)]
fn assert_private_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o700,
        "{} should be 0700",
        path.display()
    );
}

#[cfg(unix)]
fn assert_private_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600,
        "{} should be 0600",
        path.display()
    );
}
