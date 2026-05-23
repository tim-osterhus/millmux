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
