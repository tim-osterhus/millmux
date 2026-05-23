use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

use millrace_sessions_core::{
    ids::SessionId,
    paths::{StatePaths, STATE_DIR_ENV},
    protocol::{ControlErrorCode, SessionInspectResponse, SessionListResponse},
    state::{AttentionState, HostMeta, ProcessState, SessionMeta, SessionRole},
    storage::write_json_atomic,
    workspace::WorkspaceIdentity,
};
use serde_json::{json, Value};

#[test]
fn foreground_daemon_serves_read_only_jsonl_contract() {
    let temp = tempfile::tempdir().unwrap();
    let paths = StatePaths::new(temp.path().join("state"));
    fs::create_dir_all(&paths.sessions_dir).unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let session = sample_session(&workspace);
    write_session_meta(&paths, &session);

    let mut child = DaemonChild::spawn(&paths);
    wait_for_socket(&paths.control_sock);

    let status = request_json(
        &paths,
        json!({"id": "status-1", "method": "host.status", "params": {}}),
    );
    assert_eq!(status["id"], "status-1");
    assert_eq!(status["ok"], true);
    assert_eq!(status["result"]["session_count"], 1);
    let host: HostMeta = serde_json::from_value(status["result"]["host"].clone()).unwrap();
    assert_eq!(host.state_root, paths.root);

    let list = request_json(
        &paths,
        json!({"id": "list-1", "method": "session.list", "params": {}}),
    );
    assert_eq!(list["ok"], true);
    let list_result: SessionListResponse =
        serde_json::from_value(list["result"].clone()).expect("list result shape");
    assert_eq!(list_result.sessions.len(), 1);
    assert_eq!(list_result.sessions[0].session_id, session.id);

    let inspect = request_json(
        &paths,
        json!({
            "id": "inspect-1",
            "method": "session.inspect",
            "params": {
                "selector": {
                    "type": "id",
                    "session_id": session.id
                }
            }
        }),
    );
    assert_eq!(inspect["ok"], true);
    let inspect_result: SessionInspectResponse =
        serde_json::from_value(inspect["result"].clone()).expect("inspect result shape");
    assert_eq!(inspect_result.session.session_id, session.id);
    assert_eq!(
        inspect_result.paths.meta_json,
        paths.session_paths(session.id).meta_json
    );

    let invalid = request_line(&paths, "not-json\n");
    assert_error(
        &invalid,
        "invalid_request",
        ControlErrorCode::InvalidRequest,
    );

    let unsupported = request_json(
        &paths,
        json!({"id": "start-1", "method": "session.start", "params": {}}),
    );
    assert_error(&unsupported, "start-1", ControlErrorCode::InvalidRequest);

    let missing: SessionId = "018f5d8d-3e79-4a62-9bc5-51c3c7f4d5c8".parse().unwrap();
    let missing_response = request_json(
        &paths,
        json!({
            "id": "inspect-missing",
            "method": "session.inspect",
            "params": {
                "selector": {
                    "type": "id",
                    "session_id": missing
                }
            }
        }),
    );
    assert_error(
        &missing_response,
        "inspect-missing",
        ControlErrorCode::SessionNotFound,
    );

    child.kill();
}

fn request_json(paths: &StatePaths, value: Value) -> Value {
    request_line(
        paths,
        &format!("{}\n", serde_json::to_string(&value).unwrap()),
    )
}

fn request_line(paths: &StatePaths, line: &str) -> Value {
    let mut stream = UnixStream::connect(&paths.control_sock).expect("connect to daemon socket");
    stream.write_all(line.as_bytes()).expect("write request");
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

struct DaemonChild {
    child: Child,
}

impl DaemonChild {
    fn spawn(paths: &StatePaths) -> Self {
        let child = Command::new(sessiond_bin())
            .arg("--foreground")
            .env(STATE_DIR_ENV, &paths.root)
            .spawn()
            .expect("spawn millrace-sessiond");
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
    ensure_sessiond_bin(&path);
    path
}

fn ensure_sessiond_bin(path: &Path) {
    if is_executable(path) {
        return;
    }

    let status = Command::new("cargo")
        .args([
            "build",
            "-p",
            "millrace-sessions",
            "--bin",
            "millrace-sessiond",
        ])
        .current_dir(workspace_root())
        .status()
        .expect("build millrace-sessiond");
    assert!(status.success(), "failed to build millrace-sessiond");
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

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

fn write_session_meta(paths: &StatePaths, meta: &SessionMeta) {
    let session_paths = paths.session_paths(meta.id);
    fs::create_dir_all(&session_paths.root).unwrap();
    write_json_atomic(&session_paths.meta_json, meta).unwrap();
}

fn sample_session(workspace: impl AsRef<Path>) -> SessionMeta {
    let workspace = workspace.as_ref();
    SessionMeta {
        id: SessionId::new(),
        name: Some("daemon".to_string()),
        role: SessionRole::MillraceDaemon,
        process_state: ProcessState::Running,
        attention_state: AttentionState::MillraceIdle,
        workspace: Some(WorkspaceIdentity::capture(workspace).unwrap()),
        cwd: workspace.to_path_buf(),
        argv: vec![
            "millrace".to_string(),
            "run".to_string(),
            "daemon".to_string(),
        ],
        env: BTreeMap::new(),
        worker_pid: None,
        child_pid: None,
        child_pgid: None,
        started_at: None,
        ended_at: None,
        exit_code: None,
        exit_signal: None,
        failure_message: None,
        created_at: "2026-05-20T18:00:00Z".to_string(),
        updated_at: "2026-05-20T18:01:00Z".to_string(),
    }
}
