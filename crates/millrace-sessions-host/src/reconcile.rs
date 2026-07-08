use std::collections::BTreeSet;

use millrace_sessions_core::{
    events::{append_event, current_timestamp, read_events, SessionEvent, SessionEventKind},
    paths::StatePaths,
    state::{LivenessState, ProcessState, SessionMeta, WorkerMeta},
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecordLiveness {
    pub worker: LivenessState,
    pub child: LivenessState,
    pub worker_pids: Vec<u32>,
    pub child_pids: Vec<u32>,
}

impl RecordLiveness {
    fn has_indeterminate(&self) -> bool {
        self.worker == LivenessState::Indeterminate || self.child == LivenessState::Indeterminate
    }
}

pub fn reconcile_startup(paths: &StatePaths) -> Result<ReconcileSummary, ReconcileError> {
    let registry = HostRegistry::load(paths.clone())?;
    let mut summary = ReconcileSummary::default();

    for record in registry.sessions().values() {
        if record.archived || !is_active_process_state(&record.meta.process_state) {
            continue;
        }
        summary.scanned += 1;

        let liveness = record_liveness(record);
        if liveness.has_indeterminate() {
            summary.skipped += 1;
            continue;
        }

        match (liveness.worker, liveness.child) {
            (LivenessState::Alive, LivenessState::Alive | LivenessState::Unknown) => {
                summary.preserved += 1;
            }
            (LivenessState::Alive, LivenessState::Dead) => {
                mark_record_terminal(
                    record,
                    ProcessState::Stale,
                    "stale_worker_child_dead",
                    Some(
                        "startup reconciliation found a live worker but the child process was gone",
                    ),
                    &liveness,
                )?;
                summary.marked_terminal += 1;
            }
            (LivenessState::Dead | LivenessState::Unknown, LivenessState::Alive) => {
                mark_record_terminal(
                    record,
                    ProcessState::Orphaned,
                    "orphaned_child",
                    Some("startup reconciliation found a live child without a live worker"),
                    &liveness,
                )?;
                summary.marked_terminal += 1;
            }
            (
                LivenessState::Dead | LivenessState::Unknown,
                LivenessState::Dead | LivenessState::Unknown,
            ) => {
                let target_state =
                    terminal_state_from_evidence(record).unwrap_or(ProcessState::Lost);
                mark_record_terminal(
                    record,
                    target_state,
                    "all_recorded_processes_dead",
                    Some("startup reconciliation found no live recorded process"),
                    &liveness,
                )?;
                summary.marked_terminal += 1;
            }
            (_, LivenessState::Indeterminate) | (LivenessState::Indeterminate, _) => {
                summary.skipped += 1;
            }
        }
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
    let mut pids = collect_worker_pids(record);
    pids.extend(collect_child_pids(record));
    pids.sort_unstable();
    pids.dedup();
    pids
}

pub(crate) fn record_liveness(record: &SessionRecord) -> RecordLiveness {
    let worker_pids = collect_worker_pids(record);
    let child_pids = collect_child_pids(record);
    RecordLiveness {
        worker: liveness_from_pids(&worker_pids),
        child: liveness_from_pids(&child_pids),
        worker_pids,
        child_pids,
    }
}

fn collect_worker_pids(record: &SessionRecord) -> Vec<u32> {
    let mut pids = BTreeSet::new();
    if let Some(pid) = record.meta.worker_pid {
        pids.insert(pid);
    }
    if let Some(worker) = &record.worker {
        pids.insert(worker.pid);
    }
    pids.into_iter().filter(|pid| *pid > 0).collect()
}

fn collect_child_pids(record: &SessionRecord) -> Vec<u32> {
    let mut pids = BTreeSet::new();
    if let Some(pid) = record.meta.child_pid {
        pids.insert(pid);
    }
    if let Some(worker) = &record.worker {
        if let Some(pid) = worker.child_pid {
            pids.insert(pid);
        }
    }
    pids.into_iter().filter(|pid| *pid > 0).collect()
}

fn liveness_from_pids(pids: &[u32]) -> LivenessState {
    if pids.is_empty() {
        return LivenessState::Unknown;
    }
    let statuses = pids.iter().map(|pid| pid_status(*pid)).collect::<Vec<_>>();
    if statuses.contains(&PidStatus::Alive) {
        return LivenessState::Alive;
    }
    if statuses.contains(&PidStatus::Indeterminate) {
        return LivenessState::Indeterminate;
    }
    LivenessState::Dead
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
    liveness_reason: &'static str,
    failure_message: Option<&'static str>,
    liveness: &RecordLiveness,
) -> Result<(), ReconcileError> {
    let now = current_timestamp();
    let mut meta = read_json::<SessionMeta>(&record.paths.meta_json)?;
    if !is_active_process_state(&meta.process_state) {
        return Ok(());
    }
    meta.process_state = target_state.clone();
    meta.ended_at.get_or_insert_with(|| now.clone());
    meta.updated_at = now.clone();
    if meta.failure_message.is_none() {
        if let Some(message) = failure_message {
            meta.failure_message = Some(message.to_string());
        } else if target_state == ProcessState::Lost {
            meta.failure_message =
                Some("startup reconciliation found no live recorded process".to_string());
        }
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
    event
        .fields
        .insert("liveness_reason".to_string(), liveness_reason.to_string());
    event.fields.insert(
        "worker_liveness".to_string(),
        serde_json::to_string(&liveness.worker)
            .unwrap_or_else(|_| "\"unknown\"".to_string())
            .trim_matches('"')
            .to_string(),
    );
    event.fields.insert(
        "child_liveness".to_string(),
        serde_json::to_string(&liveness.child)
            .unwrap_or_else(|_| "\"unknown\"".to_string())
            .trim_matches('"')
            .to_string(),
    );
    event
        .fields
        .insert("worker_pids".to_string(), pid_list(&liveness.worker_pids));
    event
        .fields
        .insert("child_pids".to_string(), pid_list(&liveness.child_pids));
    append_event(&record.paths.events_jsonl, &event)?;
    Ok(())
}

fn pid_list(pids: &[u32]) -> String {
    pids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_treats_current_process_as_alive() {
        assert_eq!(pid_status(std::process::id()), PidStatus::Alive);
    }
}
