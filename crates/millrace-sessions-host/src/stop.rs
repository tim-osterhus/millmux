use millrace_sessions_core::{
    events::{append_event, SessionEvent, SessionEventKind},
    protocol::{ControlErrorBody, ControlErrorCode},
    state::SessionMeta,
};
use nix::{
    errno::Errno,
    sys::signal::{kill, Signal},
    unistd::Pid,
};
use std::{path::Path, process::Command};

pub fn request_sigterm(meta: &SessionMeta) -> Result<bool, ControlErrorBody> {
    signal_session(meta, Signal::SIGTERM)
}

pub fn request_sigkill(meta: &SessionMeta) -> Result<bool, ControlErrorBody> {
    signal_session(meta, Signal::SIGKILL)
}

pub fn request_millrace_control_stop(
    meta: &SessionMeta,
    events_jsonl: &Path,
) -> Result<(), ControlErrorBody> {
    let Some(workspace) = meta.workspace.as_ref() else {
        let mut event = SessionEvent::new(meta.id, SessionEventKind::MillraceStopFailed);
        event.message = Some("millrace-daemon session has no workspace metadata".to_string());
        append_event(events_jsonl, &event).map_err(control_core_error)?;
        return Ok(());
    };

    let mut event = SessionEvent::new(meta.id, SessionEventKind::MillraceStopRequested);
    event.fields.insert(
        "workspace".to_string(),
        workspace.canonical_path.display().to_string(),
    );
    event
        .fields
        .insert("reason".to_string(), "millrace_control_stop".to_string());
    event
        .fields
        .insert("stop_requested_at".to_string(), event.timestamp.clone());
    append_event(events_jsonl, &event).map_err(control_core_error)?;

    match Command::new("millrace")
        .arg("control")
        .arg("stop")
        .arg("--workspace")
        .arg(&workspace.canonical_path)
        .output()
    {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            let mut event = SessionEvent::new(meta.id, SessionEventKind::MillraceStopFailed);
            event.message = Some(format!(
                "millrace control stop exited with status {}",
                output
                    .status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "signal".to_string())
            ));
            event.fields.insert(
                "workspace".to_string(),
                workspace.canonical_path.display().to_string(),
            );
            event.fields.insert(
                "reason".to_string(),
                "millrace_control_stop_failed".to_string(),
            );
            event.fields.insert(
                "stderr".to_string(),
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            );
            append_event(events_jsonl, &event).map_err(control_core_error)
        }
        Err(error) => {
            let mut event = SessionEvent::new(meta.id, SessionEventKind::MillraceStopFailed);
            event.message = Some(format!("failed to run millrace control stop: {error}"));
            event.fields.insert(
                "workspace".to_string(),
                workspace.canonical_path.display().to_string(),
            );
            event.fields.insert(
                "reason".to_string(),
                "millrace_control_stop_failed".to_string(),
            );
            append_event(events_jsonl, &event).map_err(control_core_error)
        }
    }
}

fn signal_session(meta: &SessionMeta, signal: Signal) -> Result<bool, ControlErrorBody> {
    if let Some(pgid) = meta.child_pgid {
        return signal_raw_pid(negative_pid(pgid)?, signal);
    }
    if let Some(pid) = meta.child_pid {
        return signal_raw_pid(raw_pid(pid)?, signal);
    }
    if let Some(pid) = meta.worker_pid {
        return signal_raw_pid(raw_pid(pid)?, signal);
    }

    Err(ControlErrorBody::new(
        ControlErrorCode::WorkerUnavailable,
        "session metadata does not include a signalable process id",
    ))
}

fn signal_raw_pid(pid: i32, signal: Signal) -> Result<bool, ControlErrorBody> {
    match kill(Pid::from_raw(pid), signal) {
        Ok(()) => Ok(true),
        Err(Errno::ESRCH) => Ok(false),
        Err(Errno::EPERM) => Err(ControlErrorBody::new(
            ControlErrorCode::PermissionError,
            "permission denied while signaling session process",
        )),
        Err(error) => Err(ControlErrorBody::new(
            ControlErrorCode::WorkerUnavailable,
            format!("failed to signal session process: {error}"),
        )),
    }
}

fn control_core_error(error: millrace_sessions_core::error::MillmuxError) -> ControlErrorBody {
    ControlErrorBody::new(ControlErrorCode::IoError, error.to_string())
}

fn raw_pid(pid: u32) -> Result<i32, ControlErrorBody> {
    i32::try_from(pid).map_err(|_| {
        ControlErrorBody::new(
            ControlErrorCode::InvalidRequest,
            format!("process id {pid} does not fit in a platform pid"),
        )
    })
}

fn negative_pid(pid: u32) -> Result<i32, ControlErrorBody> {
    raw_pid(pid).map(|pid| -pid)
}
