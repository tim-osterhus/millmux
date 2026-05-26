use std::{
    env, ffi::OsString, fs, os::unix::fs::PermissionsExt, path::Path, process::Command, thread,
    time::Duration,
};

use assert_cmd::prelude::*;
use millrace_sessions_core::{ids::UiId, paths::StatePaths, storage::write_json_atomic};
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

        for _ in 0..20 {
            if kill(pid, None).is_err() {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }

        let _ = kill(pid, Signal::SIGKILL);
    }
}

fn millmux_command(host: &TempHost) -> Command {
    let mut command = Command::cargo_bin("millmux").expect("millmux binary");
    command.env("MILLMUX_STATE_DIR", host.state_dir());
    if let Some(host_bin) = host_bin_override() {
        command.env("MILLMUX_HOST_BIN", host_bin);
    }
    command
}

fn host_bin_override() -> Option<std::path::PathBuf> {
    let value = std::env::var_os("MILLMUX_HOST_BIN")?;
    let path = std::path::PathBuf::from(value);
    if path.is_absolute() {
        return Some(path);
    }

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.parent()?.parent()?;
    Some(workspace_root.join(path))
}

#[test]
fn cli_smoke_list_json_autostarts_host_and_prints_raw_result() {
    let host = TempHost::new();

    let output = millmux_command(&host)
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value: Value = serde_json::from_slice(&output).expect("json output");
    assert_eq!(value["sessions"], Value::Array(Vec::new()));
    assert!(
        value.get("id").is_none(),
        "json output must omit envelope id"
    );
    assert!(
        value.get("ok").is_none(),
        "json output must omit envelope ok"
    );
    assert!(
        value.get("error").is_none(),
        "json output must omit envelope error"
    );
    assert!(
        host.state_dir().join("session-control.sock").exists(),
        "host socket should be ready after list"
    );
}

#[test]
fn cli_smoke_concurrent_list_json_calls_share_autostarted_host() {
    let host = TempHost::new();
    let state_dir = host.state_dir().to_path_buf();
    let host_bin = host_bin_override();

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let state_dir = state_dir.clone();
            let host_bin = host_bin.clone();
            thread::spawn(move || {
                let mut command = Command::cargo_bin("millmux").expect("millmux binary");
                command.env("MILLMUX_STATE_DIR", state_dir);
                if let Some(host_bin) = host_bin {
                    command.env("MILLMUX_HOST_BIN", host_bin);
                }
                command
                    .args(["list", "--json"])
                    .output()
                    .expect("run millmux")
            })
        })
        .collect();

    for handle in handles {
        let output = handle.join().expect("thread joined");
        assert!(
            output.status.success(),
            "millmux failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).expect("json output");
        assert_eq!(value["sessions"], Value::Array(Vec::new()));
        assert!(value.get("ok").is_none());
    }
}

#[test]
fn cli_smoke_context_json_uses_protocol_and_millmux_ui_id() {
    let host = TempHost::new();
    let ui_id = UiId::new();
    seed_context(&host, ui_id, "daemon_console", "2026-05-26T04:00:00Z");

    let output = millmux_command(&host)
        .args(["context", "--ui", &ui_id.to_string(), "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).expect("json output");
    assert_eq!(value["context"]["ui_id"], ui_id.to_string());
    assert_eq!(value["context"]["mode"], "daemon_console");
    assert!(value.get("id").is_none());
    assert!(value.get("ok").is_none());

    let env_output = millmux_command(&host)
        .env("MILLMUX_UI_ID", ui_id.to_string())
        .args(["context", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let env_value: Value = serde_json::from_slice(&env_output).expect("json output");
    assert_eq!(env_value["context"]["ui_id"], ui_id.to_string());

    let list_output = millmux_command(&host)
        .args(["context", "--list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let list_value: Value = serde_json::from_slice(&list_output).expect("json output");
    assert_eq!(list_value["contexts"].as_array().unwrap().len(), 1);

    let second_ui_id = UiId::new();
    seed_context(&host, second_ui_id, "agent_cockpit", "2026-05-26T04:01:00Z");
    let stderr = millmux_command(&host)
        .args(["context", "--json"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    assert!(
        String::from_utf8_lossy(&stderr).contains("ambiguous_ui_context"),
        "stderr should name ambiguous context error: {}",
        String::from_utf8_lossy(&stderr)
    );
}

#[test]
fn cli_smoke_console_renders_existing_daemon_and_writes_context() {
    let host = TempHost::new();
    let workspace = tempfile::tempdir().expect("workspace");
    let session_id = start_daemon(
        &host,
        workspace.path(),
        "printf 'daemon ready\\n'; sleep 0.2",
    );
    thread::sleep(Duration::from_millis(150));

    let output = millmux_command(&host)
        .args(["console", "--workspace"])
        .arg(workspace.path())
        .args(["--no-start", "--once"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("Daemon Monitor"), "{text}");
    assert!(text.contains("daemon ready"), "{text}");

    let context_output = millmux_command(&host)
        .args(["context", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let context: Value = serde_json::from_slice(&context_output).expect("context json");
    assert_eq!(
        context["context"]["active_daemon_session_id"].as_str(),
        Some(session_id.as_str())
    );
    assert_eq!(context["context"]["mode"], "daemon_console");
}

#[test]
fn cli_smoke_console_destructive_command_requires_confirmation() {
    let host = TempHost::new();
    let workspace = tempfile::tempdir().expect("workspace");
    let session_id = start_daemon(
        &host,
        workspace.path(),
        "printf 'daemon running\\n'; sleep 5",
    );
    thread::sleep(Duration::from_millis(150));

    let stderr = millmux_command(&host)
        .args(["console", "--workspace"])
        .arg(workspace.path())
        .args(["--no-start", "--command", "stop"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    assert!(
        String::from_utf8_lossy(&stderr).contains("confirmation required"),
        "{}",
        String::from_utf8_lossy(&stderr)
    );

    let output = millmux_command(&host)
        .args(["console", "--workspace"])
        .arg(workspace.path())
        .args(["--no-start", "--command", "stop", "--confirm", &session_id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("Command Output | state=succeeded"), "{text}");
}

#[test]
fn cli_smoke_console_starts_new_daemon_when_only_terminal_record_exists() {
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
if [ "$1" = "run" ] && [ "$2" = "daemon" ]; then
  printf 'new daemon ready\n'
  sleep 5
  exit 0
fi
printf 'unexpected fake millrace args: %s\n' "$*" >&2
exit 1
"#,
    );

    let old_session_id = start_daemon_with_path(
        &host,
        &path_env,
        &workspace,
        "printf 'old daemon exited\\n'",
    );
    wait_for_session_state(&host, &old_session_id, "exited");

    let output = millmux_command(&host)
        .env("PATH", &path_env)
        .args(["console", "--workspace"])
        .arg(&workspace)
        .args(["--monitor", "jsonl", "--once"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("Daemon Monitor"), "{text}");

    let sessions = daemon_sessions(&host, &workspace);
    assert_eq!(sessions.len(), 2, "{sessions:?}");
    assert!(
        sessions.iter().any(|session| session["session_id"]
            .as_str()
            .is_some_and(|session_id| session_id == old_session_id)
            && session["process_state"] == "exited"),
        "{sessions:?}"
    );
    let new_session_id = sessions
        .iter()
        .find(|session| {
            session["session_id"].as_str() != Some(old_session_id.as_str())
                && matches!(
                    session["process_state"].as_str(),
                    Some("starting" | "running")
                )
        })
        .and_then(|session| session["session_id"].as_str())
        .expect("new active daemon session")
        .to_string();
    let new_session = sessions
        .iter()
        .find(|session| session["session_id"].as_str() == Some(new_session_id.as_str()))
        .expect("new active daemon summary");
    assert_eq!(new_session["monitor_profile"], "jsonl");

    let context_output = millmux_command(&host)
        .args(["context", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let context: Value = serde_json::from_slice(&context_output).expect("context json");
    assert_eq!(
        context["context"]["active_daemon_session_id"].as_str(),
        Some(new_session_id.as_str())
    );
    assert_eq!(context["context"]["monitor_profile"], "jsonl");
}

#[test]
fn cli_smoke_cockpit_starts_agent_daemon_and_writes_context() {
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
if [ "$1" = "run" ] && [ "$2" = "daemon" ]; then
  printf 'cockpit daemon ready\n'
  sleep 5
  exit 0
fi
printf 'unexpected fake millrace args: %s\n' "$*" >&2
exit 1
"#,
    );
    let side_workspace = temp.path().join("side-workspace");
    fs::create_dir_all(&side_workspace).unwrap();
    let side_session_id = start_daemon_with_path(
        &host,
        &path_env,
        &side_workspace,
        "printf 'side daemon ready\\n'; sleep 5",
    );

    let output = millmux_command(&host)
        .env("PATH", &path_env)
        .args(["cockpit", "--workspace"])
        .arg(&workspace)
        .args([
            "--monitor",
            "raw",
            "--once",
            "--agent",
            "fixture-agent",
            "--agent-argv",
            "--",
            "sh",
            "-c",
            "printf 'agent:%s\\n' \"$MILLMUX_AGENT_SESSION_ID\"; printf 'workspace:%s\\n' \"$MILLRACE_WORKSPACE\"; printf 'context:%s\\n' \"${MILLMUX_CONTEXT_FILE##*/}\"; sleep 5",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("Agent Terminal"), "{text}");
    assert!(text.contains("Daemon Monitor"), "{text}");
    assert!(text.contains("agent:"), "{text}");
    assert!(text.contains("workspace:"), "{text}");
    assert!(text.contains("context:context.json"), "{text}");

    let context_output = millmux_command(&host)
        .args(["context", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let context: Value = serde_json::from_slice(&context_output).expect("context json");
    assert_eq!(context["context"]["mode"], "agent_cockpit");
    assert!(context["context"]["agent_session_id"].as_str().is_some());
    assert!(context["context"]["active_daemon_session_id"]
        .as_str()
        .is_some());
    assert_eq!(context["context"]["monitor_profile"], "raw");
    let managed = context["context"]["managed_daemon_session_ids"]
        .as_array()
        .expect("managed daemon list");
    assert!(
        managed
            .iter()
            .any(|value| value.as_str() == Some(side_session_id.as_str())),
        "{managed:?}"
    );
}

fn seed_context(host: &TempHost, ui_id: UiId, mode: &str, updated_at: &str) {
    let paths = StatePaths::new(host.state_dir().to_path_buf());
    let ui_paths = paths.ui_context_paths(ui_id);
    write_json_atomic(
        &ui_paths.context_json,
        &serde_json::json!({
            "schema_version": 1,
            "ui_id": ui_id,
            "mode": mode,
            "active_pane_id": null,
            "active_daemon_session_id": null,
            "active_workspace": null,
            "agent_session_id": null,
            "managed_daemon_session_ids": [],
            "monitor_profile": "auto",
            "updated_at": updated_at
        }),
    )
    .expect("seed context");
}

fn start_daemon(host: &TempHost, workspace: &Path, script: &str) -> String {
    start_daemon_command(millmux_command(host), workspace, script)
}

fn start_daemon_with_path(
    host: &TempHost,
    path_env: &OsString,
    workspace: &Path,
    script: &str,
) -> String {
    let mut command = millmux_command(host);
    command.env("PATH", path_env);
    start_daemon_command(command, workspace, script)
}

fn start_daemon_command(mut command: Command, workspace: &Path, script: &str) -> String {
    let output = command
        .args([
            "start",
            "--json",
            "--role",
            "millrace-daemon",
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
    value["session"]["session_id"]
        .as_str()
        .expect("session id")
        .to_string()
}

fn wait_for_session_state(host: &TempHost, session_id: &str, expected: &str) {
    for _ in 0..60 {
        if session_state(host, session_id).as_deref() == Some(expected) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("{session_id} did not reach {expected}");
}

fn session_state(host: &TempHost, session_id: &str) -> Option<String> {
    let output = millmux_command(host)
        .args(["status", "--json", session_id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).expect("status json");
    value["session"]["process_state"]
        .as_str()
        .map(str::to_string)
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

fn prepend_path(dir: &Path) -> OsString {
    let mut paths = vec![dir.to_path_buf()];
    if let Some(existing) = env::var_os("PATH") {
        paths.extend(env::split_paths(&existing));
    }
    env::join_paths(paths).unwrap()
}
