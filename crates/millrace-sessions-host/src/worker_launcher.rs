use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

use millrace_sessions_core::{
    ids::SessionId,
    paths::{StatePaths, STATE_DIR_ENV},
};
use thiserror::Error;

pub const WORKER_BIN_ENV: &str = "MILLMUX_WORKER_BIN";
const WORKER_BIN_NAME: &str = "millrace-session-worker";

#[derive(Debug, Error)]
pub enum WorkerLaunchError {
    #[error("could not locate millrace-session-worker; set {WORKER_BIN_ENV}")]
    WorkerBinaryNotFound,
    #[error("worker binary is not executable: {0}")]
    WorkerBinaryNotExecutable(PathBuf),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug)]
pub struct WorkerProcess {
    child: Child,
}

impl WorkerProcess {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

pub fn launch_worker(
    paths: &StatePaths,
    session_id: SessionId,
) -> Result<WorkerProcess, WorkerLaunchError> {
    let worker_bin = resolve_worker_binary()?;
    let mut command = Command::new(worker_bin);
    command
        .arg("--session-id")
        .arg(session_id.to_string())
        .arg("--state-dir")
        .arg(&paths.root)
        .env(STATE_DIR_ENV, &paths.root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let child = command.spawn()?;
    Ok(WorkerProcess { child })
}

pub fn resolve_worker_binary() -> Result<PathBuf, WorkerLaunchError> {
    if let Some(path) = env::var_os(WORKER_BIN_ENV).filter(|value| !value.is_empty()) {
        let path = PathBuf::from(path);
        return require_executable(path);
    }

    if let Ok(current_exe) = env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            let sibling = parent.join(WORKER_BIN_NAME);
            if is_executable(&sibling) {
                return Ok(sibling);
            }
        }
    }

    if let Some(path) = find_on_path(WORKER_BIN_NAME) {
        return Ok(path);
    }

    Err(WorkerLaunchError::WorkerBinaryNotFound)
}

fn require_executable(path: PathBuf) -> Result<PathBuf, WorkerLaunchError> {
    if is_executable(&path) {
        Ok(path)
    } else {
        Err(WorkerLaunchError::WorkerBinaryNotExecutable(path))
    }
}

fn find_on_path(binary_name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(binary_name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn worker_launcher_resolves_env_override() {
        let _lock = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let worker = temp.path().join("worker-bin");
        fs::write(&worker, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).unwrap();
        }

        env::set_var(WORKER_BIN_ENV, &worker);
        let resolved = resolve_worker_binary().unwrap();
        env::remove_var(WORKER_BIN_ENV);

        assert_eq!(resolved, worker);
    }

    #[test]
    fn worker_launcher_rejects_non_executable_env_override() {
        let _lock = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let worker = temp.path().join("worker-bin");
        fs::write(&worker, "not executable").unwrap();

        env::set_var(WORKER_BIN_ENV, &worker);
        let error = resolve_worker_binary().unwrap_err();
        env::remove_var(WORKER_BIN_ENV);

        assert!(matches!(
            error,
            WorkerLaunchError::WorkerBinaryNotExecutable(path) if path == worker
        ));
    }
}
