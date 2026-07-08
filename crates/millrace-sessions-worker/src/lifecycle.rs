use std::collections::BTreeMap;

use millrace_sessions_core::{
    error::MillmuxResult,
    events::{append_event, current_timestamp, SessionEvent, SessionEventKind},
    state::{ProcessState, SessionMeta, SessionPaths, SpawnMode, WorkerMeta},
    storage::{read_json, write_json_atomic},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerFacts {
    pub worker_pid: u32,
    pub child_pid: Option<u32>,
    pub child_pgid: Option<u32>,
    pub spawn_mode: SpawnMode,
}

pub fn load_session_meta(paths: &SessionPaths) -> MillmuxResult<SessionMeta> {
    read_json(&paths.meta_json)
}

pub fn write_worker_meta(paths: &SessionPaths, facts: WorkerFacts) -> MillmuxResult<WorkerMeta> {
    let mut meta = load_session_meta(paths)?;
    let now = current_timestamp();
    meta.worker_pid = Some(facts.worker_pid);
    meta.child_pid = facts.child_pid;
    meta.child_pgid = facts.child_pgid;
    meta.updated_at = now.clone();
    let worker = WorkerMeta {
        session_id: meta.id,
        pid: facts.worker_pid,
        child_pid: facts.child_pid,
        child_pgid: facts.child_pgid,
        spawn_mode: facts.spawn_mode,
        process_state: ProcessState::Starting,
        started_at: now.clone(),
        ended_at: None,
        stop_requested_at: None,
        stop_reason: None,
        exit_code: None,
        exit_signal: None,
        attached_clients: 0,
        input_owner: None,
        updated_at: now,
    };
    write_json_atomic(&paths.meta_json, &meta)?;
    write_json_atomic(&paths.worker_json, &worker)?;
    Ok(worker)
}

pub fn record_running(
    paths: &SessionPaths,
    child_pid: Option<u32>,
    child_pgid: Option<u32>,
) -> MillmuxResult<()> {
    update_meta(paths, |meta, now| {
        meta.process_state = ProcessState::Running;
        meta.child_pid = child_pid;
        meta.child_pgid = child_pgid;
        meta.started_at.get_or_insert_with(|| now.to_string());
        meta.updated_at = now.to_string();
        meta.failure_message = None;
    })?;
    update_worker(paths, |worker, now| {
        worker.process_state = ProcessState::Running;
        worker.child_pid = child_pid;
        worker.child_pgid = child_pgid;
        worker.updated_at = now.to_string();
    })?;

    let mut event = state_event(
        paths,
        SessionEventKind::WorkerStarted,
        ProcessState::Running,
    )?;
    event.fields = process_fields(child_pid, child_pgid);
    append_event(&paths.events_jsonl, &event)?;
    let mut event = state_event(
        paths,
        SessionEventKind::ProcessStarted,
        ProcessState::Running,
    )?;
    event.fields = process_fields(child_pid, child_pgid);
    append_event(&paths.events_jsonl, &event)?;
    Ok(())
}

pub fn record_process_exit(
    paths: &SessionPaths,
    exit_code: i32,
    exit_signal: Option<String>,
) -> MillmuxResult<()> {
    let existing = load_session_meta(paths)?;
    let state = if existing.process_state == ProcessState::Killed {
        ProcessState::Killed
    } else if exit_signal.is_some() {
        ProcessState::Crashed
    } else {
        ProcessState::Exited
    };
    update_meta(paths, |meta, now| {
        meta.process_state = state.clone();
        meta.ended_at = Some(now.to_string());
        meta.exit_code = Some(exit_code);
        meta.exit_signal = exit_signal.clone();
        meta.updated_at = now.to_string();
    })?;
    update_worker(paths, |worker, now| {
        worker.process_state = state.clone();
        worker.ended_at = Some(now.to_string());
        worker.exit_code = Some(exit_code);
        worker.exit_signal = exit_signal.clone();
        worker.attached_clients = 0;
        worker.input_owner = None;
        worker.updated_at = now.to_string();
    })?;

    let mut event = state_event(paths, SessionEventKind::ProcessExited, state)?;
    event
        .fields
        .insert("exit_code".to_string(), exit_code.to_string());
    if let Some(signal) = exit_signal {
        event.fields.insert("exit_signal".to_string(), signal);
    }
    append_event(&paths.events_jsonl, &event)?;
    Ok(())
}

pub fn record_failed_start(paths: &SessionPaths, message: impl Into<String>) -> MillmuxResult<()> {
    let message = message.into();
    update_meta(paths, |meta, now| {
        meta.process_state = ProcessState::FailedStart;
        meta.ended_at = Some(now.to_string());
        meta.failure_message = Some(message.clone());
        meta.updated_at = now.to_string();
    })?;
    update_worker(paths, |worker, now| {
        worker.process_state = ProcessState::FailedStart;
        worker.ended_at = Some(now.to_string());
        worker.attached_clients = 0;
        worker.input_owner = None;
        worker.updated_at = now.to_string();
    })?;

    let mut event = state_event(
        paths,
        SessionEventKind::StateChanged,
        ProcessState::FailedStart,
    )?;
    event.message = Some(message);
    append_event(&paths.events_jsonl, &event)?;
    Ok(())
}

fn update_meta<F>(paths: &SessionPaths, update: F) -> MillmuxResult<()>
where
    F: FnOnce(&mut SessionMeta, &str),
{
    let mut meta = load_session_meta(paths)?;
    let now = current_timestamp();
    update(&mut meta, &now);
    write_json_atomic(&paths.meta_json, &meta)
}

fn update_worker<F>(paths: &SessionPaths, update: F) -> MillmuxResult<()>
where
    F: FnOnce(&mut WorkerMeta, &str),
{
    if !paths.worker_json.exists() {
        return Ok(());
    }
    let mut worker: WorkerMeta = read_json(&paths.worker_json)?;
    let now = current_timestamp();
    update(&mut worker, &now);
    write_json_atomic(&paths.worker_json, &worker)
}

fn state_event(
    paths: &SessionPaths,
    kind: SessionEventKind,
    state: ProcessState,
) -> MillmuxResult<SessionEvent> {
    let meta = load_session_meta(paths)?;
    let mut event = SessionEvent::new(meta.id, kind);
    event.process_state = Some(state);
    Ok(event)
}

fn process_fields(child_pid: Option<u32>, child_pgid: Option<u32>) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    if let Some(pid) = child_pid {
        fields.insert("child_pid".to_string(), pid.to_string());
    }
    if let Some(pgid) = child_pgid {
        fields.insert("child_pgid".to_string(), pgid.to_string());
    }
    fields
}
