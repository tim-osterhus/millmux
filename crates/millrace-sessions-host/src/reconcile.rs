use std::collections::BTreeSet;

use millrace_sessions_core::{
    events::{append_event, current_timestamp, read_events, SessionEvent, SessionEventKind},
    paths::StatePaths,
    state::{ProcessState, SessionMeta, WorkerMeta},
    storage::{create_private_dir_all, read_json, write_json_atomic},
};
use nix::{errno::Errno, sys::signal::kill, unistd::Pid};
use thiserror::Error;

use crate::registry::{HostRegistry, RegistryError, SessionRecord};

#[derive(Debug, Error)]
pub enum ReconcileError {
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Core(#[from] millrace_sessions_core::error::MillmuxError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReconcileSummary {
    pub scanned: usize,
    pub preserved: usize,
    pub marked_terminal: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PidStatus {
    Alive,
    Dead,
    Indeterminate,
}

pub fn reconcile_startup(paths: &StatePaths) -> Result<ReconcileSummary, ReconcileError> {
    let registry = HostRegistry::load(paths.clone())?;
    let mut summary = ReconcileSummary::default();

    for record in registry.sessions().values() {
        if record.archived || !is_active_process_state(&record.meta.process_state) {
            continue;
        }
        summary.scanned += 1;

        let pids = collect_record_pids(record);
        if pids.iter().any(|pid| pid_status(*pid) == PidStatus::Alive) {
            summary.preserved += 1;
            continue;
        }
        if pids
            .iter()
            .any(|pid| pid_status(*pid) == PidStatus::Indeterminate)
        {
            summary.skipped += 1;
            continue;
        }

        let target_state = terminal_state_from_evidence(record).unwrap_or(ProcessState::Lost);
        mark_record_terminal(record, target_state)?;
        summary.marked_terminal += 1;
    }

    Ok(summary)
}

pub(crate) fn is_active_process_state(state: &ProcessState) -> bool {
    matches!(state, ProcessState::Starting | ProcessState::Running)
}

pub(crate) fn is_terminal_process_state(state: &ProcessState) -> bool {
    !is_active_process_state(state)
}

pub(crate) fn collect_record_pids(record: &SessionRecord) -> Vec<u32> {
    let mut pids = BTreeSet::new();
    if let Some(pid) = record.meta.worker_pid {
        pids.insert(pid);
    }
    if let Some(pid) = record.meta.child_pid {
        pids.insert(pid);
    }
    if let Some(worker) = &record.worker {
        pids.insert(worker.pid);
        if let Some(pid) = worker.child_pid {
            pids.insert(pid);
        }
    }
    pids.into_iter().filter(|pid| *pid > 0).collect()
}

pub(crate) fn pid_status(pid: u32) -> PidStatus {
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => PidStatus::Alive,
        Err(Errno::ESRCH) => PidStatus::Dead,
        Err(Errno::EPERM) => PidStatus::Indeterminate,
        Err(_) => PidStatus::Indeterminate,
    }
}

fn terminal_state_from_evidence(record: &SessionRecord) -> Option<ProcessState> {
    if let Some(worker) = &record.worker {
        if is_terminal_process_state(&worker.process_state) {
            return Some(worker.process_state.clone());
        }
    }

    read_events(&record.paths.events_jsonl)
        .ok()
        .and_then(|events| {
            events
                .into_iter()
                .rev()
                .find_map(|event| event.process_state.filter(is_terminal_process_state))
        })
}

fn mark_record_terminal(
    record: &SessionRecord,
    target_state: ProcessState,
) -> Result<(), ReconcileError> {
    let now = current_timestamp();
    let mut meta = read_json::<SessionMeta>(&record.paths.meta_json)?;
    if !is_active_process_state(&meta.process_state) {
        return Ok(());
    }
    meta.process_state = target_state.clone();
    meta.ended_at.get_or_insert_with(|| now.clone());
    meta.updated_at = now.clone();
    if meta.failure_message.is_none() && target_state == ProcessState::Lost {
        meta.failure_message = Some("startup reconciliation found no live recorded process".into());
    }
    write_json_atomic(&record.paths.meta_json, &meta)?;

    if record.paths.worker_json.exists() {
        let mut worker = read_json::<WorkerMeta>(&record.paths.worker_json)?;
        if is_active_process_state(&worker.process_state) {
            worker.process_state = target_state.clone();
            worker.ended_at.get_or_insert_with(|| now.clone());
            worker.attached_clients = 0;
            worker.input_owner = None;
            worker.updated_at = now.clone();
            write_json_atomic(&record.paths.worker_json, &worker)?;
        }
    }

    create_private_dir_all(
        record
            .paths
            .events_jsonl
            .parent()
            .expect("session events path has a parent"),
    )?;
    let mut event = SessionEvent::new(meta.id, SessionEventKind::StateChanged);
    event.process_state = Some(target_state);
    event.message = Some("startup reconciliation marked session terminal".to_string());
    event
        .fields
        .insert("reason".to_string(), "startup_reconcile".to_string());
    append_event(&record.paths.events_jsonl, &event)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_treats_current_process_as_alive() {
        assert_eq!(pid_status(std::process::id()), PidStatus::Alive);
    }
}
