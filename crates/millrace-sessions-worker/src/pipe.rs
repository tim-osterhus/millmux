use std::{
    collections::BTreeMap,
    path::PathBuf,
    process::{Child, ChildStderr, ChildStdout, Command, Stdio},
};

use millrace_sessions_core::error::{MillmuxError, MillmuxResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeCommandSpec {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
}

pub struct RunningPipe {
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
    pub child: Child,
    pub child_pid: Option<u32>,
    pub child_pgid: Option<u32>,
}

impl std::fmt::Debug for RunningPipe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunningPipe")
            .field("child_pid", &self.child_pid)
            .field("child_pgid", &self.child_pgid)
            .finish_non_exhaustive()
    }
}

pub fn spawn_pipe(spec: PipeCommandSpec) -> MillmuxResult<RunningPipe> {
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

    let mut command = Command::new(&spec.argv[0]);
    command
        .args(spec.argv.iter().skip(1))
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in &spec.env {
        command.env(key, value);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = command.spawn().map_err(|error| {
        MillmuxError::WorkerUnavailable(format!("failed to spawn pipe child: {error}"))
    })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        MillmuxError::WorkerUnavailable("failed to capture pipe child stdout".to_string())
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        MillmuxError::WorkerUnavailable("failed to capture pipe child stderr".to_string())
    })?;
    let child_pid = Some(child.id());
    #[cfg(unix)]
    let child_pgid = child_pid;
    #[cfg(not(unix))]
    let child_pgid = None;

    Ok(RunningPipe {
        stdout,
        stderr,
        child,
        child_pid,
        child_pgid,
    })
}
