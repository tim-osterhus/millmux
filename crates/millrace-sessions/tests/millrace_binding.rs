use std::{
    env,
    ffi::OsString,
    fs,
    os::{unix::fs::PermissionsExt, unix::net::UnixStream},
    path::{Path, PathBuf},
    process::Command,
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
fn cli_requires_workspace_for_millrace_daemon_without_creating_session() {
    let host = TempHost::new();

    let output = millmux_command(&host)
        .args([
            "start",
            "--json",
            "--role",
            "millrace-daemon",
            "--",
            "sh",
            "-c",
            "sleep 1",
        ])
        .output()
        .expect("run millmux");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("millrace-daemon sessions require --workspace"),
        "{stderr}"
    );
    assert_eq!(active_session_count(&host), 0);
}

#[test]
fn cli_roles_round_trip_and_auxiliary_roles_share_daemon_workspace() {
    let host = TempHost::new();
    let temp = tempfile::tempdir().expect("workspace root");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let path_env = fake_millrace_path(
        temp.path(),
        r#"if [ "$1" = "status" ]; then
  printf '{"process_running":false}\n'
  exit 0
fi
exit 0
"#,
    );

    let daemon = start_role(
        &host,
        &path_env,
        "millrace-daemon",
        &workspace,
        "printf daemon; sleep 3",
    );
    assert_eq!(daemon["session"]["role"], "millrace_daemon");
    assert_eq!(daemon["attached_existing"], false);

    for (input_role, expected_role) in [
        ("agent", "agent"),
        ("shell", "shell"),
        ("generic", "generic"),
        ("custom-role", "custom_role"),
    ] {
        let value = start_role(
            &host,
            &path_env,
            input_role,
            &workspace,
            "printf aux; sleep 0.2",
        );
        assert_eq!(value["session"]["role"], expected_role);
        assert_eq!(value["attached_existing"], false);
        assert!(value["session"]["session_id"].as_str().is_some());
    }
}

#[test]
fn cli_surfaces_duplicate_daemon_conflict() {
    let host = TempHost::new();
    let temp = tempfile::tempdir().expect("workspace root");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let path_env = fake_millrace_path(
        temp.path(),
        r#"if [ "$1" = "status" ]; then
  printf '{"process_running":false}\n'
  exit 0
fi
exit 0
"#,
    );

    let first = start_role(
        &host,
        &path_env,
        "millrace-daemon",
        &workspace,
        "printf daemon; sleep 3",
    );
    assert_eq!(first["attached_existing"], false);

    let output = millmux_command(&host)
        .env("PATH", &path_env)
        .args([
            "start",
            "--json",
            "--role",
            "millrace-daemon",
            "--workspace",
        ])
        .arg(&workspace)
        .args(["--cwd"])
        .arg(&workspace)
        .args(["--", "sh", "-c", "printf different; sleep 3"])
        .output()
        .expect("run millmux duplicate");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("DuplicateMillraceDaemon") || stderr.contains("duplicate millrace-daemon"),
        "{stderr}"
    );
}

#[test]
fn cockpit_autostart_failure_renders_degraded_state_from_client_path() {
    let host = TempHost::new();
    let temp = tempfile::tempdir().expect("workspace root");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let stale_path = path_without_millrace(temp.path());

    millmux_command(&host)
        .env("PATH", &stale_path)
        .args(["list", "--json"])
        .assert()
        .success();

    let path_env = fake_millrace_path(
        temp.path(),
        r#"if [ "$1" = "status" ]; then
  printf '{"process_running":false}\n'
  exit 0
fi
if [ "$1" = "run" ] && [ "$2" = "daemon" ]; then
  printf 'daemon auto-start failed before ready\n' >&2
  exit 42
fi
printf 'unexpected fake millrace args: %s\n' "$*" >&2
exit 1
"#,
    );
    let fake_millrace = temp.path().join("fake-bin").join("millrace");

    let output = millmux_command(&host)
        .env("PATH", &path_env)
        .args(["cockpit", "--workspace"])
        .arg(&workspace)
        .args([
            "--monitor",
            "basic",
            "--once",
            "--agent",
            "fixture-agent",
            "--agent-argv",
            "--",
            "/bin/sh",
            "-c",
            "printf 'agent ready\\n'; sleep 5",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("Daemon Monitor | state=exited"), "{text}");
    assert!(
        text.contains("daemon auto-start failed before ready"),
        "{text}"
    );
    assert!(
        text.contains("recovery: inspect logs archive delete"),
        "{text}"
    );
    assert!(text.contains("status=degraded exited"), "{text}");
    assert!(!text.contains("status=ready"), "{text}");

    let sessions = daemon_sessions(&host, &workspace);
    let daemon = sessions
        .iter()
        .find(|session| {
            session["argv"]
                .as_array()
                .is_some_and(|argv| argv.iter().any(|value| value.as_str() == Some("daemon")))
        })
        .unwrap_or_else(|| panic!("missing auto-started daemon session: {sessions:#?}"));
    let fake_millrace = fake_millrace.to_string_lossy().to_string();
    assert_eq!(
        daemon["argv"][0].as_str(),
        Some(fake_millrace.as_str()),
        "{daemon:#?}"
    );
}

fn start_role(
    host: &TempHost,
    path_env: &OsString,
    role: &str,
    workspace: &Path,
    script: &str,
) -> Value {
    let output = millmux_command(host)
        .env("PATH", path_env)
        .args(["start", "--json", "--role", role, "--workspace"])
        .arg(workspace)
        .args(["--cwd"])
        .arg(workspace)
        .args(["--", "sh", "-c", script])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).expect("start json")
}

fn active_session_count(host: &TempHost) -> usize {
    fs::read_dir(host.state_dir().join("sessions"))
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or(0)
}

fn daemon_sessions(host: &TempHost, workspace: &Path) -> Vec<Value> {
    let output = millmux_command(host)
        .args(["list", "--json", "--role", "millrace-daemon", "--workspace"])
        .arg(workspace)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).expect("list json");
    value["sessions"]
        .as_array()
        .expect("sessions array")
        .clone()
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

fn fake_millrace_path(root: &Path, script: &str) -> OsString {
    let bin = root.join("fake-bin");
    fs::create_dir_all(&bin).unwrap();
    let millrace = bin.join("millrace");
    fs::write(&millrace, format!("#!/bin/sh\n{script}\n")).unwrap();
    let mut permissions = fs::metadata(&millrace).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&millrace, permissions).unwrap();
    prepend_path(&bin)
}

fn path_without_millrace(root: &Path) -> OsString {
    let bin = root.join("empty-bin");
    fs::create_dir_all(&bin).unwrap();
    env::join_paths([bin]).unwrap()
}

fn prepend_path(dir: &Path) -> OsString {
    let mut paths = vec![dir.to_path_buf()];
    if let Some(existing) = env::var_os("PATH") {
        paths.extend(env::split_paths(&existing));
    }
    env::join_paths(paths).unwrap()
}

fn binary_override(name: &str, binary_name: &str) -> PathBuf {
    if let Some(value) = std::env::var_os(name) {
        let path = PathBuf::from(value);
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

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn _assert_socket(path: &Path) {
    let _ = UnixStream::connect(path);
}
