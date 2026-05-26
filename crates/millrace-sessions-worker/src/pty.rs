use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::PathBuf,
};

use millrace_sessions_core::error::{MillmuxError, MillmuxResult};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyCommandSpec {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
}

pub struct RunningPty {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    pub child: Box<dyn Child + Send + Sync>,
    pub child_pid: Option<u32>,
    pub child_pgid: Option<u32>,
    pub master: Box<dyn MasterPty + Send>,
}

impl std::fmt::Debug for RunningPty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunningPty")
            .field("child_pid", &self.child_pid)
            .field("child_pgid", &self.child_pgid)
            .finish_non_exhaustive()
    }
}

pub fn spawn_pty(spec: PtyCommandSpec) -> MillmuxResult<RunningPty> {
    if spec.argv.is_empty() {
        return Err(MillmuxError::InvalidRequest(
            "session argv must contain at least one argument".to_string(),
        ));
    }
    if !spec.cwd.exists() {
        return Err(MillmuxError::WorkspaceNotFound(spec.cwd));
    }
    if !spec.cwd.is_dir() {
        return Err(MillmuxError::InvalidRequest(format!(
            "cwd is not a directory: {}",
            spec.cwd.display()
        )));
    }

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| MillmuxError::WorkerUnavailable(format!("failed to open pty: {error}")))?;
    let reader = pair.master.try_clone_reader().map_err(|error| {
        MillmuxError::WorkerUnavailable(format!("failed to clone pty reader: {error}"))
    })?;
    let writer = pair.master.take_writer().map_err(|error| {
        MillmuxError::WorkerUnavailable(format!("failed to take pty writer: {error}"))
    })?;

    let mut command = CommandBuilder::new(&spec.argv[0]);
    command.args(spec.argv.iter().skip(1).map(String::as_str));
    command.cwd(spec.cwd.as_os_str());
    for (key, value) in &spec.env {
        command.env(key, value);
    }

    let child = pair.slave.spawn_command(command).map_err(|error| {
        MillmuxError::WorkerUnavailable(format!("failed to spawn pty child: {error}"))
    })?;
    let child_pid = child.process_id();
    #[cfg(unix)]
    let child_pgid = pair
        .master
        .process_group_leader()
        .and_then(|pid| u32::try_from(pid).ok());
    #[cfg(not(unix))]
    let child_pgid = None;

    drop(pair.slave);
    Ok(RunningPty {
        reader,
        writer,
        child,
        child_pid,
        child_pgid,
        master: pair.master,
    })
}
