use std::{
    env,
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Write},
    os::{unix::fs::PermissionsExt, unix::net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

use millrace_sessions_core::{
    events::{read_events, SessionEventKind},
    paths::{StatePaths, STATE_DIR_ENV},
    protocol::{ControlErrorCode, SessionStartResponse, SessionStopResponse},
    state::{ProcessState, SessionMeta},
    storage::read_json,
};
use serde_json::{json, Value};

#[test]
fn daemon_start_requires_workspace_and_does_not_create_session() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let path_env = fake_millrace_path(
        temp.path(),
        r#"if [ "$1" = "status" ]; then
  printf '{"process_running":false}\n'
  exit 0
fi
exit 0
"#,
    );
    let mut daemon = DaemonChild::spawn(&paths, &path_env, &[]);
    wait_for_socket(&paths.control_sock);

    let response = request_json(
        &paths,
        json!({
            "id": "missing-workspace",
            "method": "session.start",
            "params": {
                "role": "millrace_daemon",
                "cwd": temp.path(),
                "argv": ["sh", "-c", "sleep 1"]
            }
        }),
    );

    assert_error(
        &response,
        "missing-workspace",
        ControlErrorCode::InvalidRequest,
    );
    assert_eq!(active_session_count(&paths), 0);

    daemon.kill();
}

#[test]
fn daemon_duplicate_resolution_uses_canonical_workspace_and_command() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    let workspace_link = temp.path().join("workspace-link");
    fs::create_dir_all(&workspace).unwrap();
    std::os::unix::fs::symlink(&workspace, &workspace_link).unwrap();
    let path_env = fake_millrace_path(
        temp.path(),
        r#"if [ "$1" = "status" ]; then
  printf '{"process_running":false}\n'
  exit 0
fi
exit 0
"#,
    );
    let mut daemon = DaemonChild::spawn(&paths, &path_env, &[]);
    wait_for_socket(&paths.control_sock);

    let first = start_role(
        &paths,
        "daemon-1",
        "millrace_daemon",
        &workspace,
        &workspace,
        "printf ready; sleep 3",
    );
    let first_start: SessionStartResponse =
        serde_json::from_value(first["result"].clone()).expect("first start result");

    let same_command_via_symlink = start_role(
        &paths,
        "daemon-2",
        "millrace_daemon",
        &workspace_link,
        &workspace_link,
        "printf ready; sleep 3",
    );
    let same_start: SessionStartResponse =
        serde_json::from_value(same_command_via_symlink["result"].clone())
            .expect("same command result");

    assert!(same_start.attached_existing);
    assert_eq!(
        same_start.session.session_id,
        first_start.session.session_id
    );

    let conflicting = request_json(
        &paths,
        json!({
            "id": "daemon-conflict",
            "method": "session.start",
            "params": {
                "name": "daemon-conflict",
                "role": "millrace_daemon",
                "workspace": workspace_link,
                "cwd": workspace_link,
                "argv": ["sh", "-c", "printf different; sleep 3"]
            }
        }),
    );
    assert_error(
        &conflicting,
        "daemon-conflict",
        ControlErrorCode::DuplicateMillraceDaemon,
    );

    daemon.kill();
}

#[test]
fn auxiliary_roles_can_share_daemon_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
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
    let mut daemon = DaemonChild::spawn(&paths, &path_env, &[]);
    wait_for_socket(&paths.control_sock);

    let daemon_start = start_role(
        &paths,
        "daemon",
        "millrace_daemon",
        &workspace,
        &workspace,
        "printf daemon; sleep 3",
    );
    let daemon_start: SessionStartResponse =
        serde_json::from_value(daemon_start["result"].clone()).expect("daemon start result");

    for role in ["agent", "shell", "generic", "custom_helper"] {
        let response = start_role(
            &paths,
            role,
            role,
            &workspace,
            &workspace,
            "printf aux; sleep 0.2",
        );
        let start: SessionStartResponse =
            serde_json::from_value(response["result"].clone()).expect("aux start result");
        assert!(!start.attached_existing);
        assert_ne!(start.session.session_id, daemon_start.session.session_id);
        assert_eq!(
            start
                .session
                .workspace
                .as_ref()
                .expect("workspace")
                .canonical_path,
            workspace.canonicalize().unwrap()
        );
    }

    daemon.kill();
}

#[test]
fn status_probe_running_refuses_daemon_without_registry_match() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let path_env = fake_millrace_path(
        temp.path(),
        r#"if [ "$1" = "status" ]; then
  printf '{"process_running":true}\n'
  exit 0
fi
exit 0
"#,
    );
    let mut daemon = DaemonChild::spawn(&paths, &path_env, &[]);
    wait_for_socket(&paths.control_sock);

    let response = request_json(
        &paths,
        json!({
            "id": "status-running",
            "method": "session.start",
            "params": {
                "name": "daemon",
                "role": "millrace_daemon",
                "workspace": workspace,
                "cwd": workspace,
                "argv": ["sh", "-c", "sleep 1"]
            }
        }),
    );

    assert_error(
        &response,
        "status-running",
        ControlErrorCode::DuplicateMillraceDaemon,
    );
    assert_eq!(active_session_count(&paths), 0);

    daemon.kill();
}

#[test]
fn status_probe_issue_is_recorded_but_start_continues() {
    let unusable = tempfile::tempdir().unwrap();
    assert_status_probe_issue_is_recorded(
        fake_millrace_path(
            unusable.path(),
            r#"if [ "$1" = "status" ]; then
  printf 'not-json\n'
  exit 0
fi
exit 0
"#,
        ),
        "unusable_json",
    );
    let nonzero = tempfile::tempdir().unwrap();
    assert_status_probe_issue_is_recorded(
        fake_millrace_path(
            nonzero.path(),
            r#"if [ "$1" = "status" ]; then
  printf 'status failed\n' >&2
  exit 42
fi
exit 0
"#,
        ),
        "nonzero_exit",
    );

    let temp = tempfile::tempdir().unwrap();
    assert_status_probe_issue_is_recorded(path_without_millrace(temp.path()), "unavailable");
}

fn assert_status_probe_issue_is_recorded(path_env: OsString, expected_outcome: &str) {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut daemon = DaemonChild::spawn(&paths, &path_env, &[]);
    wait_for_socket(&paths.control_sock);

    let response = start_role(
        &paths,
        "daemon",
        "millrace_daemon",
        &workspace,
        &workspace,
        "printf ready; sleep 0.2",
    );
    let start: SessionStartResponse =
        serde_json::from_value(response["result"].clone()).expect("start result");
    let session_paths = paths.session_paths(start.session.session_id);
    let events = read_events(&session_paths.events_jsonl).unwrap();

    assert!(events.iter().any(|event| {
        event.kind == SessionEventKind::MillraceStatusProbe
            && event.fields.get("outcome").map(String::as_str) == Some(expected_outcome)
    }));

    daemon.kill();
}

#[test]
fn daemon_stop_attempts_native_control_before_generic_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    let workspace = temp.path().join("workspace");
    let fake_log = temp.path().join("millrace.log");
    fs::create_dir_all(&workspace).unwrap();
    let path_env = fake_millrace_path(
        temp.path(),
        r#"printf '%s\n' "$*" >> "$MILLRACE_FAKE_LOG"
if [ "$1" = "status" ]; then
  printf '{"process_running":false}\n'
  exit 0
fi
if [ "$1" = "control" ] && [ "$2" = "stop" ]; then
  exit 7
fi
exit 0
"#,
    );
    let fake_log_value = fake_log.as_os_str().to_os_string();
    let mut daemon = DaemonChild::spawn(
        &paths,
        &path_env,
        &[("MILLRACE_FAKE_LOG", fake_log_value.as_os_str())],
    );
    wait_for_socket(&paths.control_sock);

    let response = start_role(
        &paths,
        "daemon-stop",
        "millrace_daemon",
        &workspace,
        &workspace,
        "trap 'printf stopped\\n; exit 0' INT TERM; printf ready\\n; while true; do sleep 1; done",
    );
    let start: SessionStartResponse =
        serde_json::from_value(response["result"].clone()).expect("start result");
    let session_paths = paths.session_paths(start.session.session_id);
    wait_for_running_meta(&session_paths.meta_json);
    wait_for_file_contains(&session_paths.pty_log, "ready");

    let stop = request_json(
        &paths,
        json!({
            "id": "stop-daemon",
            "method": "session.stop",
            "params": {
                "selector": {"type": "id", "session_id": start.session.session_id},
                "grace_seconds": 1
            }
        }),
    );
    assert_eq!(stop["ok"], true, "{stop:#}");
    let stop: SessionStopResponse =
        serde_json::from_value(stop["result"].clone()).expect("stop result");
    assert!(stop.stop_requested);

    let log = fs::read_to_string(&fake_log).unwrap();
    assert!(log.contains(&format!(
        "control stop --workspace {}",
        workspace.canonicalize().unwrap().display()
    )));

    let events = read_events(&session_paths.events_jsonl).unwrap();
    assert!(events
        .iter()
        .any(|event| event.kind == SessionEventKind::MillraceStopRequested));
    assert!(events
        .iter()
        .any(|event| event.kind == SessionEventKind::MillraceStopFailed));

    daemon.kill();
}

fn start_role(
    paths: &StatePaths,
    name: &str,
    role: &str,
    workspace: &Path,
    cwd: &Path,
    script: &str,
) -> Value {
    let response = request_json(
        paths,
        json!({
            "id": format!("start-{name}"),
            "method": "session.start",
            "params": {
                "name": name,
                "role": role,
                "workspace": workspace,
                "cwd": cwd,
                "argv": ["sh", "-c", script]
            }
        }),
    );
    assert_eq!(response["ok"], true, "{response:#}");
    response
}

fn request_json(paths: &StatePaths, value: Value) -> Value {
    let mut stream = UnixStream::connect(&paths.control_sock).expect("connect to daemon socket");
    stream
        .write_all(format!("{}\n", serde_json::to_string(&value).unwrap()).as_bytes())
        .expect("write request");
    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .expect("read response");
    serde_json::from_str(response.trim_end()).expect("response is json")
}

fn assert_error(response: &Value, id: &str, code: ControlErrorCode) {
    assert_eq!(response["id"], id);
    assert_eq!(response["ok"], false);
    assert_eq!(
        serde_json::from_value::<ControlErrorCode>(response["error"]["code"].clone()).unwrap(),
        code
    );
}

fn active_session_count(paths: &StatePaths) -> usize {
    fs::read_dir(&paths.sessions_dir)
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or(0)
}

fn wait_for_socket(path: &Path) {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if path.exists() && UnixStream::connect(path).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("daemon socket did not become ready at {}", path.display());
}

fn wait_for_file_contains(path: &Path, needle: &str) {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
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

fn wait_for_running_meta(path: &Path) {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if let Ok(meta) = read_json::<SessionMeta>(path) {
            if meta.process_state == ProcessState::Running {
                return;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("{} did not reach running state", path.display());
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

fn path_without_millrace(root: &Path) -> OsString {
    let bin = root.join("empty-bin");
    fs::create_dir_all(&bin).unwrap();
    env::join_paths([bin, PathBuf::from("/bin"), PathBuf::from("/usr/bin")]).unwrap()
}

struct DaemonChild {
    child: Child,
}

impl DaemonChild {
    fn spawn(
        paths: &StatePaths,
        path_env: &OsString,
        extra_env: &[(&str, &std::ffi::OsStr)],
    ) -> Self {
        let mut command = Command::new(sessiond_bin());
        command
            .arg("--foreground")
            .env(STATE_DIR_ENV, &paths.root)
            .env("MILLMUX_WORKER_BIN", worker_bin())
            .env("PATH", path_env);
        for (name, value) in extra_env {
            command.env(name, value);
        }
        let child = command.spawn().expect("spawn millrace-sessiond");
        Self { child }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for DaemonChild {
    fn drop(&mut self) {
        self.kill();
    }
}

fn sessiond_bin() -> PathBuf {
    let path = workspace_root()
        .join("target")
        .join("debug")
        .join("millrace-sessiond");
    ensure_bin(&path, "millrace-sessiond");
    path
}

fn worker_bin() -> PathBuf {
    if let Some(value) = std::env::var_os("MILLMUX_WORKER_BIN") {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            return path;
        }
        return workspace_root().join(path);
    }

    let path = workspace_root()
        .join("target")
        .join("debug")
        .join("millrace-session-worker");
    ensure_bin(&path, "millrace-session-worker");
    path
}

fn ensure_bin(path: &Path, binary_name: &str) {
    if is_executable(path) {
        return;
    }

    let status = Command::new("cargo")
        .args(["build", "-p", "millrace-sessions", "--bin", binary_name])
        .current_dir(workspace_root())
        .status()
        .unwrap_or_else(|error| panic!("build {binary_name}: {error}"));
    assert!(status.success(), "failed to build {binary_name}");
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}
