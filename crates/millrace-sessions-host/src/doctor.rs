use std::{
    collections::BTreeSet,
    fs,
    os::unix::{
        fs::{FileTypeExt, PermissionsExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
};

use millrace_sessions_core::{
    events::{append_event, read_events, SessionEvent, SessionEventKind},
    ids::{SessionId, UiId},
    paths::StatePaths,
    protocol::{
        DoctorIssue, DoctorRepair, DoctorRepairMode, DoctorRepairStatus, DoctorRequest,
        DoctorResponse, DoctorSeverity, DoctorStatus, M1_PROTOCOL_VERSION,
    },
    scrollback::{legacy_line_scrollback_tui_sequence_name, ScrollbackBuffer},
    state::{
        HostMeta, LivenessState, ProcessState, SessionRole, SpawnMode, UiContext, UiContextPaths,
        UiEvent, UiEventKind,
    },
    storage::{append_json_line, create_private_dir_all, read_json},
};
use serde_json::json;
use thiserror::Error;
use time::{Duration, OffsetDateTime};

use crate::{
    reconcile::{
        collect_record_pids, is_active_process_state, pid_status, record_liveness, PidStatus,
        RecordLiveness,
    },
    registry::{HostRegistry, RegistryError, SessionRecord},
};

const STALE_UI_CONTEXT_AFTER: Duration = Duration::hours(24);

#[derive(Debug, Error)]
pub enum DoctorError {
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Core(#[from] millrace_sessions_core::error::MillmuxError),
}

pub fn run_doctor(
    paths: &StatePaths,
    host: Option<&HostMeta>,
    request: &DoctorRequest,
) -> Result<DoctorResponse, DoctorError> {
    let mut issues = Vec::new();
    check_state_root(paths, &mut issues);
    check_host_socket(paths, host, &mut issues);

    let registry = HostRegistry::load(paths.clone())?;
    for load_issue in registry.load_issues() {
        let code =
            if load_issue.path.file_name().and_then(|name| name.to_str()) == Some("meta.json") {
                "corrupted_meta_json"
            } else {
                "corrupted_worker_json"
            };
        issues.push(issue(
            code,
            DoctorSeverity::Critical,
            format!("could not decode {}", load_issue.path.display()),
            None,
            Some(load_issue.path.clone()),
            false,
            Some("preserve the file and inspect or repair it manually".to_string()),
            Some(json!({ "error": load_issue.error })),
        ));
    }

    for record in registry.sessions().values() {
        check_session_record(record, &mut issues);
    }
    check_ui_contexts(paths, &registry, &mut issues)?;

    let repairs = match request.repair {
        Some(DoctorRepairMode::ArchiveStale) => archive_stale(paths, &registry)?,
        Some(DoctorRepairMode::CloseStaleUiContexts) => close_stale_ui_contexts(paths, &registry)?,
        None => Vec::new(),
    };

    Ok(DoctorResponse {
        schema_version: M1_PROTOCOL_VERSION,
        protocol_version: M1_PROTOCOL_VERSION,
        status: doctor_status(&issues),
        issues,
        repairs,
    })
}

fn check_state_root(paths: &StatePaths, issues: &mut Vec<DoctorIssue>) {
    let Ok(metadata) = fs::metadata(&paths.root) else {
        issues.push(issue(
            "state_dir_missing",
            DoctorSeverity::Critical,
            format!("state directory is missing: {}", paths.root.display()),
            None,
            Some(paths.root.clone()),
            false,
            Some("create the Millmux state directory with user-private permissions".to_string()),
            None,
        ));
        return;
    };

    if !metadata.is_dir() {
        issues.push(issue(
            "state_dir_not_directory",
            DoctorSeverity::Critical,
            format!("state path is not a directory: {}", paths.root.display()),
            None,
            Some(paths.root.clone()),
            false,
            Some("move the file away and recreate the Millmux state directory".to_string()),
            None,
        ));
        return;
    }

    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        issues.push(issue(
            "bad_state_dir_permissions",
            DoctorSeverity::Critical,
            format!(
                "state directory permissions are {:o}; expected no group/other access",
                mode
            ),
            None,
            Some(paths.root.clone()),
            false,
            Some("run an explicit permission repair once one is available".to_string()),
            Some(json!({ "mode": format!("{mode:o}") })),
        ));
    } else {
        issues.push(issue(
            "state_dir_permissions_ok",
            DoctorSeverity::Info,
            "state directory permissions are user-private".to_string(),
            None,
            Some(paths.root.clone()),
            false,
            None,
            Some(json!({ "mode": format!("{mode:o}") })),
        ));
    }
}

fn check_host_socket(paths: &StatePaths, host: Option<&HostMeta>, issues: &mut Vec<DoctorIssue>) {
    let Ok(metadata) = fs::symlink_metadata(&paths.control_sock) else {
        return;
    };

    if !metadata.file_type().is_socket() {
        issues.push(issue(
            "host_socket_not_socket",
            DoctorSeverity::Critical,
            format!(
                "host control socket path is not a socket: {}",
                paths.control_sock.display()
            ),
            None,
            Some(paths.control_sock.clone()),
            false,
            Some("move the path aside before starting Millmux".to_string()),
            None,
        ));
        return;
    }

    let mode = metadata.permissions().mode() & 0o777;
    if mode != 0o600 {
        issues.push(issue(
            "bad_socket_permissions",
            DoctorSeverity::Critical,
            format!("host socket permissions are {:o}; expected 600", mode),
            None,
            Some(paths.control_sock.clone()),
            false,
            Some("run an explicit socket permission repair once one is available".to_string()),
            Some(json!({ "mode": format!("{mode:o}") })),
        ));
    }

    if let Some(host) = host {
        issues.push(issue(
            "host_socket_responsive",
            DoctorSeverity::Info,
            format!("host socket is responsive for pid {}", host.pid),
            None,
            Some(paths.control_sock.clone()),
            false,
            None,
            Some(json!({ "pid": host.pid })),
        ));
        return;
    }

    match std::os::unix::net::UnixStream::connect(&paths.control_sock) {
        Ok(_) => issues.push(issue(
            "host_socket_responsive",
            DoctorSeverity::Info,
            "host socket accepted a local connection".to_string(),
            None,
            Some(paths.control_sock.clone()),
            false,
            None,
            None,
        )),
        Err(error) => issues.push(issue(
            "stale_host_socket",
            DoctorSeverity::Critical,
            format!("host socket exists but no responsive host accepted a connection: {error}"),
            None,
            Some(paths.control_sock.clone()),
            true,
            Some(
                "restart the host or remove the stale socket after confirming no host is running"
                    .to_string(),
            ),
            Some(json!({ "error": error.to_string() })),
        )),
    }
}

fn check_session_record(record: &SessionRecord, issues: &mut Vec<DoctorIssue>) {
    if !record.archived {
        match record.meta.spawn_mode {
            SpawnMode::Pty => {
                if !record.paths.pty_log.exists() {
                    issues.push(issue(
                        "missing_pty_log",
                        DoctorSeverity::Warning,
                        format!("session {} is missing pty.log", record.meta.id),
                        Some(record.meta.id),
                        Some(record.paths.pty_log.clone()),
                        false,
                        Some(
                            "preserve the session directory and inspect worker events".to_string(),
                        ),
                        None,
                    ));
                }
                check_unexpected_artifact(
                    record,
                    &record.paths.stdout_log,
                    "unexpected_stdout_log_for_pty",
                    issues,
                );
                check_unexpected_artifact(
                    record,
                    &record.paths.stderr_log,
                    "unexpected_stderr_log_for_pty",
                    issues,
                );
            }
            SpawnMode::Pipe => {
                check_pipe_artifact(
                    record,
                    &record.paths.stdout_log,
                    "missing_stdout_log",
                    issues,
                );
                check_pipe_artifact(
                    record,
                    &record.paths.stderr_log,
                    "missing_stderr_log",
                    issues,
                );
                check_unexpected_artifact(
                    record,
                    &record.paths.pty_log,
                    "unexpected_pty_log_for_pipe",
                    issues,
                );
                check_unexpected_artifact(
                    record,
                    &record.paths.scrollback_snapshot,
                    "unexpected_scrollback_for_pipe",
                    issues,
                );
                check_unexpected_artifact(
                    record,
                    &record.paths.terminal_snapshot,
                    "unexpected_terminal_snapshot_for_pipe",
                    issues,
                );
                check_unexpected_artifact(
                    record,
                    &record.paths.raw_replay_ring,
                    "unexpected_raw_replay_for_pipe",
                    issues,
                );
            }
        }
    }
    check_legacy_line_scrollback(record, issues);
    check_attach_stream_lag(record, issues);
    check_attach_state_consistency(record, issues);

    if !should_check_liveness(record) {
        return;
    }

    let liveness = record_liveness(record);
    if is_active_process_state(&record.meta.process_state) {
        check_worker_socket(record, &liveness, issues);
    }

    let pids = collect_record_pids(record);
    if pids.is_empty() {
        issues.push(issue(
            "missing_pid",
            DoctorSeverity::Critical,
            format!(
                "session {} has no recorded worker or child pid for liveness diagnostics",
                record.meta.id
            ),
            Some(record.meta.id),
            Some(record.paths.meta_json.clone()),
            false,
            Some("run startup reconciliation or inspect metadata manually".to_string()),
            None,
        ));
        return;
    }

    match (liveness.worker, liveness.child) {
        (LivenessState::Dead | LivenessState::Unknown, LivenessState::Alive) => {
            issues.push(issue(
                "orphaned_child_process",
                DoctorSeverity::Critical,
                format!(
                    "session {} has no live worker but its child process is still alive",
                    record.meta.id
                ),
                Some(record.meta.id),
                Some(record.paths.meta_json.clone()),
                false,
                Some(orphan_recovery_action(record)),
                Some(liveness_details(
                    record,
                    &liveness,
                    orphan_recovery_actions(record),
                )),
            ));
        }
        (LivenessState::Alive, LivenessState::Dead) => {
            issues.push(issue(
                "worker_child_liveness_mismatch",
                DoctorSeverity::Warning,
                format!(
                    "session {} has a live worker but its child process is gone",
                    record.meta.id
                ),
                Some(record.meta.id),
                Some(record.paths.meta_json.clone()),
                false,
                Some(
                    "inspect worker.json and events.jsonl; run startup reconciliation or stop the stale worker before archiving"
                        .to_string(),
                ),
                Some(liveness_details(
                    record,
                    &liveness,
                    vec!["inspect".to_string(), "signal_worker".to_string(), "archive_after_stopped".to_string()],
                )),
            ));
        }
        _ => {
            let statuses = pids
                .iter()
                .map(|pid| (*pid, pid_status(*pid)))
                .collect::<Vec<_>>();
            if statuses
                .iter()
                .all(|(_, status)| *status == PidStatus::Dead)
            {
                issues.push(issue(
                    "stale_worker_record",
                    DoctorSeverity::Warning,
                    format!(
                        "session {} is active or degraded in metadata but all recorded pids are gone",
                        record.meta.id
                    ),
                    Some(record.meta.id),
                    Some(record.paths.meta_json.clone()),
                    true,
                    Some(
                        "run millmux doctor --repair ARCHIVE_STALE if the session is no longer needed"
                            .to_string(),
                    ),
                    Some(json!({
                        "pids": pids,
                        "worker_liveness": liveness.worker,
                        "child_liveness": liveness.child,
                    })),
                ));
            }
        }
    }
}

fn should_check_liveness(record: &SessionRecord) -> bool {
    if record.archived {
        return false;
    }
    is_active_process_state(&record.meta.process_state)
        || matches!(
            record.meta.process_state,
            ProcessState::Orphaned | ProcessState::Stale
        )
}

fn check_worker_socket(
    record: &SessionRecord,
    liveness: &RecordLiveness,
    issues: &mut Vec<DoctorIssue>,
) {
    if record.archived || !is_active_process_state(&record.meta.process_state) {
        return;
    }
    if !record.meta.spawn_mode.is_pty() {
        return;
    }
    if record.meta.worker_pid.is_none() && record.worker.is_none() {
        return;
    }

    if !record.paths.worker_sock.exists() {
        issues.push(issue(
            "worker_socket_missing",
            DoctorSeverity::Warning,
            format!(
                "session {} has active worker metadata but no worker socket",
                record.meta.id
            ),
            Some(record.meta.id),
            Some(record.paths.worker_sock.clone()),
            false,
            Some(
                "inspect worker liveness; stop or archive only after recovery is explicit"
                    .to_string(),
            ),
            Some(json!({
                "worker_liveness": liveness.worker,
                "child_liveness": liveness.child,
                "worker_pids": &liveness.worker_pids,
                "child_pids": &liveness.child_pids,
            })),
        ));
        return;
    }

    match UnixStream::connect(&record.paths.worker_sock) {
        Ok(_) => {
            if liveness.worker != LivenessState::Alive {
                issues.push(issue(
                    "worker_socket_without_live_worker",
                    DoctorSeverity::Warning,
                    format!(
                        "session {} has a reachable worker socket but no live worker pid",
                        record.meta.id
                    ),
                    Some(record.meta.id),
                    Some(record.paths.worker_sock.clone()),
                    false,
                    Some(
                        "inspect worker socket ownership and session metadata before recovery"
                            .to_string(),
                    ),
                    Some(json!({
                        "worker_liveness": liveness.worker,
                        "child_liveness": liveness.child,
                    })),
                ));
            }
        }
        Err(error) => issues.push(issue(
            "worker_socket_unreachable",
            DoctorSeverity::Warning,
            format!(
                "session {} worker socket is not reachable: {error}",
                record.meta.id
            ),
            Some(record.meta.id),
            Some(record.paths.worker_sock.clone()),
            false,
            Some(
                "inspect worker liveness; use native stop or signal recovery before archiving"
                    .to_string(),
            ),
            Some(json!({
                "error": error.to_string(),
                "worker_liveness": liveness.worker,
                "child_liveness": liveness.child,
                "worker_pids": &liveness.worker_pids,
                "child_pids": &liveness.child_pids,
            })),
        )),
    }
}

fn check_attach_state_consistency(record: &SessionRecord, issues: &mut Vec<DoctorIssue>) {
    if record.archived {
        return;
    }
    let Some(worker) = &record.worker else {
        return;
    };
    if worker.attached_clients == 0 && worker.input_owner.is_none() {
        return;
    }

    let liveness = record_liveness(record);
    let active = is_active_process_state(&record.meta.process_state);
    let worker_active = is_active_process_state(&worker.process_state);
    let inconsistent = !active
        || !worker_active
        || !record.meta.spawn_mode.is_pty()
        || liveness.worker != LivenessState::Alive
        || liveness.child == LivenessState::Dead;

    if !inconsistent {
        return;
    }

    issues.push(issue(
        "attach_state_inconsistent",
        DoctorSeverity::Warning,
        format!(
            "session {} has stale attached_clients/input_owner metadata",
            record.meta.id
        ),
        Some(record.meta.id),
        Some(record.paths.worker_json.clone()),
        false,
        Some(
            "clear by normal attach close or startup reconciliation; inspect before editing worker.json manually"
                .to_string(),
        ),
        Some(json!({
            "attached_clients": worker.attached_clients,
            "input_owner": &worker.input_owner,
            "session_process_state": &record.meta.process_state,
            "worker_process_state": &worker.process_state,
            "spawn_mode": record.meta.spawn_mode,
            "worker_liveness": liveness.worker,
            "child_liveness": liveness.child,
        })),
    ));
}

fn orphan_recovery_action(record: &SessionRecord) -> String {
    let actions = orphan_recovery_actions(record);
    format!(
        "worker is gone while the child remains alive; available recovery actions: {}",
        actions.join(", ")
    )
}

fn orphan_recovery_actions(record: &SessionRecord) -> Vec<String> {
    let mut actions = vec!["inspect".to_string()];
    if record.meta.role == SessionRole::MillraceDaemon {
        actions.push("native_stop".to_string());
    }
    if record.meta.child_pgid.is_some()
        || record.meta.child_pid.is_some()
        || record
            .worker
            .as_ref()
            .and_then(|worker| worker.child_pgid)
            .is_some()
        || record
            .worker
            .as_ref()
            .and_then(|worker| worker.child_pid)
            .is_some()
    {
        actions.push("signal_child".to_string());
    }
    actions.push("archive_after_stopped".to_string());
    actions
}

fn liveness_details(
    record: &SessionRecord,
    liveness: &RecordLiveness,
    recovery_actions: Vec<String>,
) -> serde_json::Value {
    json!({
        "worker_liveness": liveness.worker,
        "child_liveness": liveness.child,
        "worker_pids": &liveness.worker_pids,
        "child_pids": &liveness.child_pids,
        "worker_socket": record.paths.worker_sock.display().to_string(),
        "worker_json": record.paths.worker_json.display().to_string(),
        "events_jsonl": record.paths.events_jsonl.display().to_string(),
        "recovery_actions": recovery_actions,
    })
}

fn check_pipe_artifact(
    record: &SessionRecord,
    path: &Path,
    code: &'static str,
    issues: &mut Vec<DoctorIssue>,
) {
    if path.exists() {
        return;
    }
    issues.push(issue(
        code,
        DoctorSeverity::Warning,
        format!(
            "pipe session {} is missing {}",
            record.meta.id,
            path.display()
        ),
        Some(record.meta.id),
        Some(path.to_path_buf()),
        false,
        Some("preserve the session directory and inspect worker events".to_string()),
        Some(json!({
            "spawn_mode": record.meta.spawn_mode,
        })),
    ));
}

fn check_unexpected_artifact(
    record: &SessionRecord,
    path: &Path,
    code: &'static str,
    issues: &mut Vec<DoctorIssue>,
) {
    if !path.exists() {
        return;
    }
    issues.push(issue(
        code,
        DoctorSeverity::Warning,
        format!(
            "session {} has artifact unexpected for spawn_mode={}: {}",
            record.meta.id,
            record.meta.spawn_mode,
            path.display()
        ),
        Some(record.meta.id),
        Some(path.to_path_buf()),
        false,
        Some("preserve the session directory and inspect worker events".to_string()),
        Some(json!({
            "spawn_mode": record.meta.spawn_mode,
        })),
    ));
}

fn check_attach_stream_lag(record: &SessionRecord, issues: &mut Vec<DoctorIssue>) {
    if record.archived || !record.paths.events_jsonl.exists() {
        return;
    }

    let Ok(events) = read_events(&record.paths.events_jsonl) else {
        return;
    };
    let lag_events = events
        .iter()
        .filter(|event| event.kind == SessionEventKind::AttachStreamLagged)
        .collect::<Vec<_>>();
    let Some(last_event) = lag_events.last() else {
        return;
    };

    issues.push(issue(
        "attach_stream_lagged",
        DoctorSeverity::Warning,
        format!(
            "session {} dropped output for one or more attach observers",
            record.meta.id
        ),
        Some(record.meta.id),
        Some(record.paths.events_jsonl.clone()),
        false,
        Some(
            "request millmux screen for structured state, reattach with raw replay from pty.replay, or inspect pty.log and events.jsonl; slow attach clients should drain frames promptly"
                .to_string(),
        ),
        Some(json!({
            "lag_event_count": lag_events.len(),
            "last_stream_id": last_event.fields.get("stream_id"),
            "last_dropped_bytes": last_event.fields.get("dropped_bytes"),
            "last_dropped_from_offset": last_event.fields.get("dropped_from_offset"),
            "last_dropped_to_offset": last_event.fields.get("dropped_to_offset"),
            "current_pty_log_offset": last_event.fields.get("current_pty_log_offset"),
            "reason": last_event.fields.get("reason"),
            "pty_log": record.paths.pty_log.display().to_string(),
            "events_jsonl": record.paths.events_jsonl.display().to_string(),
            "terminal_snapshot": record.paths.terminal_snapshot.display().to_string(),
            "raw_replay_ring": record.paths.raw_replay_ring.display().to_string(),
        })),
    ));
}

fn check_legacy_line_scrollback(record: &SessionRecord, issues: &mut Vec<DoctorIssue>) {
    if record.archived || !agent_like_session(record) || !record.paths.scrollback_snapshot.exists()
    {
        return;
    }

    let Ok(scrollback) = ScrollbackBuffer::restore_snapshot(&record.paths.scrollback_snapshot)
    else {
        return;
    };
    let lines = scrollback.lines();
    let Some(sequence_name) = legacy_line_scrollback_tui_sequence_name(&lines) else {
        return;
    };

    let repairable = archive_eligible(record);
    issues.push(issue(
        "unsafe_legacy_line_scrollback",
        DoctorSeverity::Warning,
        format!(
            "session {} has legacy line scrollback with likely TUI control sequences",
            record.meta.id
        ),
        Some(record.meta.id),
        Some(record.paths.scrollback_snapshot.clone()),
        repairable,
        Some(
            "use millmux screen for structured screen state; ignore legacy line scrollback for agent TUI replay; archive with ARCHIVE_STALE only when the session is stale or no longer needed; preserve pty.log, events.jsonl, terminal.snapshot.json, and pty.replay as evidence"
                .to_string(),
        ),
        Some(json!({
            "detected_sequence": sequence_name,
            "line_count": lines.len(),
            "screen_command": format!("millmux screen {} --json", record.meta.id),
            "pty_log": record.paths.pty_log.display().to_string(),
            "events_jsonl": record.paths.events_jsonl.display().to_string(),
            "terminal_snapshot": record.paths.terminal_snapshot.display().to_string(),
            "raw_replay_ring": record.paths.raw_replay_ring.display().to_string(),
            "scrollback_snapshot": record.paths.scrollback_snapshot.display().to_string(),
        })),
    ));
}

fn agent_like_session(record: &SessionRecord) -> bool {
    match &record.meta.role {
        SessionRole::Agent => return true,
        SessionRole::Other(value) if value.to_ascii_lowercase().contains("agent") => return true,
        _ => {}
    }

    record
        .meta
        .name
        .as_ref()
        .is_some_and(|name| has_agent_hint(name))
        || record.meta.argv.iter().any(|arg| has_agent_hint(arg))
}

fn has_agent_hint(value: &str) -> bool {
    let normalized = value.to_ascii_lowercase();
    normalized.contains("agent")
        || normalized.contains("codex")
        || normalized.contains("claude")
        || normalized.contains("millracer")
}

fn check_ui_contexts(
    paths: &StatePaths,
    registry: &HostRegistry,
    issues: &mut Vec<DoctorIssue>,
) -> Result<(), DoctorError> {
    let entries = match fs::read_dir(&paths.views_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };

    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let file_name = entry.file_name();
        let Some(raw_ui_id) = file_name.to_str() else {
            continue;
        };
        let Ok(ui_id) = raw_ui_id.parse::<UiId>() else {
            continue;
        };
        let context_paths = paths.ui_context_paths(ui_id);
        if !context_paths.context_json.exists() {
            continue;
        }
        let context = match read_json::<UiContext>(&context_paths.context_json) {
            Ok(context) => context,
            Err(error) => {
                issues.push(issue(
                    "corrupted_ui_context_json",
                    DoctorSeverity::Critical,
                    format!("could not decode {}", context_paths.context_json.display()),
                    None,
                    Some(context_paths.context_json.clone()),
                    false,
                    Some(
                        "preserve the view directory and inspect or close it manually".to_string(),
                    ),
                    Some(json!({ "error": error.to_string(), "ui_id": ui_id })),
                ));
                continue;
            }
        };
        if context.ui_id != ui_id {
            issues.push(issue(
                "mismatched_ui_context_id",
                DoctorSeverity::Critical,
                format!(
                    "UI context {} has mismatched ui_id {}",
                    context_paths.context_json.display(),
                    context.ui_id
                ),
                None,
                Some(context_paths.context_json.clone()),
                false,
                Some("preserve the view directory and inspect it manually".to_string()),
                Some(json!({ "directory_ui_id": ui_id, "context_ui_id": context.ui_id })),
            ));
            continue;
        }

        let live_refs = live_referenced_session_ids(&context, registry);
        if !live_refs.is_empty() {
            issues.push(issue(
                "ui_context_has_live_session_refs",
                DoctorSeverity::Info,
                format!("UI context {ui_id} references live sessions"),
                None,
                Some(context_paths.context_json.clone()),
                false,
                None,
                Some(json!({ "ui_id": ui_id, "live_session_ids": live_refs })),
            ));
            continue;
        }

        if ui_context_age_is_stale(&context) {
            issues.push(issue(
                "stale_ui_context",
                DoctorSeverity::Warning,
                format!("UI context {ui_id} is stale and references no live sessions"),
                None,
                Some(context_paths.context_json.clone()),
                true,
                Some("run millmux doctor --repair CLOSE_STALE_UI_CONTEXTS".to_string()),
                Some(json!({
                    "ui_id": ui_id,
                    "updated_at": context.updated_at,
                    "referenced_session_ids": referenced_session_id_strings(&context),
                })),
            ));
        }
    }

    Ok(())
}

fn archive_stale(
    paths: &StatePaths,
    registry: &HostRegistry,
) -> Result<Vec<DoctorRepair>, DoctorError> {
    let mut repairs = Vec::new();

    for record in registry.sessions().values() {
        if !archive_eligible(record) {
            continue;
        }

        let archive_path = paths.archive_dir.join(record.meta.id.to_string());
        if archive_path.exists() {
            repairs.push(repair(
                DoctorRepairMode::ArchiveStale,
                DoctorRepairStatus::Failed,
                Some(record.meta.id),
                Some(record.paths.root.clone()),
                Some(archive_path),
                Some("archive target already exists".to_string()),
                None,
            ));
            continue;
        }

        append_doctor_repair_event(record, &archive_path);
        create_private_dir_all(&paths.archive_dir)?;
        fs::rename(&record.paths.root, &archive_path)?;
        remove_worker_socket(&record.paths.worker_sock)?;
        repairs.push(repair(
            DoctorRepairMode::ArchiveStale,
            DoctorRepairStatus::Applied,
            Some(record.meta.id),
            Some(record.paths.root.clone()),
            Some(archive_path),
            Some("archived stale or lost session".to_string()),
            None,
        ));
    }

    if repairs.is_empty() {
        repairs.push(repair(
            DoctorRepairMode::ArchiveStale,
            DoctorRepairStatus::Skipped,
            None,
            None,
            None,
            Some("no stale or lost sessions were eligible for archive".to_string()),
            None,
        ));
    }

    Ok(repairs)
}

fn close_stale_ui_contexts(
    paths: &StatePaths,
    registry: &HostRegistry,
) -> Result<Vec<DoctorRepair>, DoctorError> {
    let mut repairs = Vec::new();
    let entries = match fs::read_dir(&paths.views_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            repairs.push(repair(
                DoctorRepairMode::CloseStaleUiContexts,
                DoctorRepairStatus::Skipped,
                None,
                None,
                None,
                Some("no UI context directory exists".to_string()),
                None,
            ));
            return Ok(repairs);
        }
        Err(error) => return Err(error.into()),
    };

    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let file_name = entry.file_name();
        let Some(raw_ui_id) = file_name.to_str() else {
            continue;
        };
        let Ok(ui_id) = raw_ui_id.parse::<UiId>() else {
            continue;
        };
        let context_paths = paths.ui_context_paths(ui_id);
        if !context_paths.context_json.exists() {
            continue;
        }
        let context = match read_json::<UiContext>(&context_paths.context_json) {
            Ok(context) => context,
            Err(error) => {
                repairs.push(repair(
                    DoctorRepairMode::CloseStaleUiContexts,
                    DoctorRepairStatus::Failed,
                    None,
                    Some(context_paths.context_json.clone()),
                    None,
                    Some("could not decode UI context".to_string()),
                    Some(json!({ "ui_id": ui_id, "error": error.to_string() })),
                ));
                continue;
            }
        };

        if context.ui_id != ui_id
            || !ui_context_age_is_stale(&context)
            || !live_referenced_session_ids(&context, registry).is_empty()
        {
            continue;
        }

        append_ui_context_repair_event(&context, &context_paths)?;
        fs::remove_file(&context_paths.context_json)?;
        repairs.push(repair(
            DoctorRepairMode::CloseStaleUiContexts,
            DoctorRepairStatus::Applied,
            None,
            Some(context_paths.context_json.clone()),
            None,
            Some("closed stale UI context without touching session artifacts".to_string()),
            Some(json!({
                "ui_id": ui_id,
                "events_jsonl": context_paths.events_jsonl,
                "referenced_session_ids": referenced_session_id_strings(&context),
            })),
        ));
    }

    if repairs.is_empty() {
        repairs.push(repair(
            DoctorRepairMode::CloseStaleUiContexts,
            DoctorRepairStatus::Skipped,
            None,
            None,
            None,
            Some("no stale UI contexts were eligible to close".to_string()),
            None,
        ));
    }

    Ok(repairs)
}

fn archive_eligible(record: &SessionRecord) -> bool {
    if record.archived {
        return false;
    }

    if matches!(
        record.meta.process_state,
        ProcessState::Lost | ProcessState::Stale | ProcessState::Orphaned
    ) {
        let pids = collect_record_pids(record);
        return pids.is_empty() || pids.iter().all(|pid| pid_status(*pid) == PidStatus::Dead);
    }

    if !is_active_process_state(&record.meta.process_state) {
        return false;
    }

    let pids = collect_record_pids(record);
    !pids.is_empty() && pids.iter().all(|pid| pid_status(*pid) == PidStatus::Dead)
}

fn ui_context_age_is_stale(context: &UiContext) -> bool {
    OffsetDateTime::now_utc() - context.updated_at >= STALE_UI_CONTEXT_AFTER
}

fn referenced_session_ids(context: &UiContext) -> BTreeSet<SessionId> {
    let mut ids = BTreeSet::new();
    if let Some(session_id) = context.active_daemon_session_id {
        ids.insert(session_id);
    }
    if let Some(session_id) = context.agent_session_id {
        ids.insert(session_id);
    }
    ids.extend(context.managed_daemon_session_ids.iter().copied());
    ids
}

fn referenced_session_id_strings(context: &UiContext) -> Vec<String> {
    referenced_session_ids(context)
        .into_iter()
        .map(|session_id| session_id.to_string())
        .collect()
}

fn live_referenced_session_ids(context: &UiContext, registry: &HostRegistry) -> Vec<String> {
    referenced_session_ids(context)
        .into_iter()
        .filter_map(|session_id| {
            let record = registry.sessions().get(&session_id)?;
            (!record.archived && is_active_process_state(&record.meta.process_state))
                .then(|| session_id.to_string())
        })
        .collect()
}

fn append_ui_context_repair_event(
    context: &UiContext,
    paths: &UiContextPaths,
) -> Result<(), DoctorError> {
    append_json_line(
        &paths.events_jsonl,
        &UiEvent {
            timestamp: millrace_sessions_core::events::current_timestamp(),
            ui_id: context.ui_id,
            kind: UiEventKind::UiClosed,
            message: Some("doctor closed stale UI context".to_string()),
            fields: [
                ("mode".to_string(), "CLOSE_STALE_UI_CONTEXTS".to_string()),
                ("reason".to_string(), "stale_ui_context".to_string()),
            ]
            .into_iter()
            .collect(),
        },
    )?;
    Ok(())
}

fn append_doctor_repair_event(record: &SessionRecord, archive_path: &Path) {
    let mut event = SessionEvent::new(record.meta.id, SessionEventKind::DoctorRepair);
    event.process_state = Some(record.meta.process_state.clone());
    event.message = Some("doctor archived stale session".to_string());
    event
        .fields
        .insert("mode".to_string(), "ARCHIVE_STALE".to_string());
    event.fields.insert(
        "archive_path".to_string(),
        archive_path.display().to_string(),
    );
    let _ = append_event(&record.paths.events_jsonl, &event);
}

fn remove_worker_socket(path: &Path) -> Result<(), std::io::Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[allow(clippy::too_many_arguments)]
fn issue(
    code: impl Into<String>,
    severity: DoctorSeverity,
    message: impl Into<String>,
    session_id: Option<millrace_sessions_core::ids::SessionId>,
    path: Option<PathBuf>,
    repairable: bool,
    suggested_action: Option<String>,
    details: Option<serde_json::Value>,
) -> DoctorIssue {
    DoctorIssue {
        code: code.into(),
        severity,
        message: message.into(),
        session_id,
        path,
        repairable,
        suggested_action,
        details,
    }
}

fn repair(
    mode: DoctorRepairMode,
    status: DoctorRepairStatus,
    session_id: Option<millrace_sessions_core::ids::SessionId>,
    source_path: Option<PathBuf>,
    archive_path: Option<PathBuf>,
    message: Option<String>,
    details: Option<serde_json::Value>,
) -> DoctorRepair {
    DoctorRepair {
        mode,
        status,
        session_id,
        source_path,
        archive_path,
        message,
        details,
    }
}

fn doctor_status(issues: &[DoctorIssue]) -> DoctorStatus {
    if issues
        .iter()
        .any(|issue| issue.severity == DoctorSeverity::Critical)
    {
        DoctorStatus::Critical
    } else if issues
        .iter()
        .any(|issue| issue.severity == DoctorSeverity::Warning)
    {
        DoctorStatus::Warning
    } else {
        DoctorStatus::Ok
    }
}
