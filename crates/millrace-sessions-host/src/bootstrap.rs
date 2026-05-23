use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

use millrace_sessions_core::{
    error::MillmuxError,
    paths::StatePaths,
    state::HostMeta,
    storage::{create_private_dir_all, write_json_atomic},
};
use nix::{
    errno::Errno,
    fcntl::{Flock, FlockArg},
};
use thiserror::Error;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::reconcile::{reconcile_startup, ReconcileError};

#[derive(Debug, Error)]
pub enum HostBootstrapError {
    #[error("host already running; lock is held at {lock_path}")]
    AlreadyRunning { lock_path: PathBuf },
    #[error("control socket path exists but is not a socket: {path}")]
    SocketPathNotSocket { path: PathBuf },
    #[error("lock error for {path}: {source}")]
    Lock { path: PathBuf, source: Errno },
    #[error(transparent)]
    Reconcile(#[from] ReconcileError),
    #[error(transparent)]
    Core(#[from] MillmuxError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type HostBootstrapResult<T> = Result<T, HostBootstrapError>;

#[derive(Debug)]
pub struct ForegroundHost {
    paths: StatePaths,
    meta: HostMeta,
    _lock: HostLock,
}

impl ForegroundHost {
    pub fn paths(&self) -> &StatePaths {
        &self.paths
    }

    pub fn meta(&self) -> &HostMeta {
        &self.meta
    }
}

#[derive(Debug)]
pub struct HostLock {
    path: PathBuf,
    _file: LockedFile,
}

impl HostLock {
    pub fn acquire(path: impl Into<PathBuf>) -> HostBootstrapResult<Self> {
        let path = path.into();
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;

        let file = lock_file(file, &path)?;
        Ok(Self { path, _file: file })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub fn bootstrap_foreground(paths: StatePaths) -> HostBootstrapResult<ForegroundHost> {
    prepare_state_root(&paths)?;
    let lock = HostLock::acquire(paths.host_lock.clone())?;
    remove_stale_socket_after_lock(&paths.control_sock)?;
    reconcile_startup(&paths)?;
    let meta = host_meta(&paths);
    write_json_atomic(&paths.host_json, &meta)?;

    Ok(ForegroundHost {
        paths,
        meta,
        _lock: lock,
    })
}

pub fn prepare_state_root(paths: &StatePaths) -> HostBootstrapResult<()> {
    create_private_dir(&paths.root)?;
    create_private_dir(&paths.sessions_dir)?;
    create_private_dir(&paths.archive_dir)?;
    Ok(())
}

fn host_meta(paths: &StatePaths) -> HostMeta {
    let now = now_rfc3339();
    HostMeta {
        pid: std::process::id(),
        state_root: paths.root.clone(),
        control_socket: paths.control_sock.clone(),
        started_at: now.clone(),
        updated_at: now,
    }
}

fn create_private_dir(path: &Path) -> HostBootstrapResult<()> {
    create_private_dir_all(path)?;
    Ok(())
}

#[cfg(unix)]
type LockedFile = Flock<File>;

#[cfg(not(unix))]
type LockedFile = File;

#[cfg(unix)]
fn lock_file(file: File, path: &Path) -> HostBootstrapResult<LockedFile> {
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(file) => Ok(file),
        Err((_file, error)) if error == Errno::EWOULDBLOCK || error == Errno::EAGAIN => {
            Err(HostBootstrapError::AlreadyRunning {
                lock_path: path.to_path_buf(),
            })
        }
        Err((_file, source)) => Err(HostBootstrapError::Lock {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(not(unix))]
fn lock_file(file: File, _path: &Path) -> HostBootstrapResult<LockedFile> {
    Ok(file)
}

fn remove_stale_socket_after_lock(path: &Path) -> HostBootstrapResult<()> {
    if !path.exists() {
        return Ok(());
    }

    let metadata = fs::symlink_metadata(path)?;
    if is_socket(&metadata.file_type()) {
        fs::remove_file(path)?;
        return Ok(());
    }

    Err(HostBootstrapError::SocketPathNotSocket {
        path: path.to_path_buf(),
    })
}

#[cfg(unix)]
fn is_socket(file_type: &fs::FileType) -> bool {
    use std::os::unix::fs::FileTypeExt;

    file_type.is_socket()
}

#[cfg(not(unix))]
fn is_socket(_file_type: &fs::FileType) -> bool {
    false
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_lock_rejects_concurrent_acquisition() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StatePaths::new(temp.path().join("state"));
        prepare_state_root(&paths).unwrap();

        let _first = HostLock::acquire(&paths.host_lock).unwrap();
        let second = HostLock::acquire(&paths.host_lock).unwrap_err();

        assert!(matches!(second, HostBootstrapError::AlreadyRunning { .. }));
    }
}
