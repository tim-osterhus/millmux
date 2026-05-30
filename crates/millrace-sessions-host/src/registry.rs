use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use millrace_sessions_core::{
    error::{MillmuxError, MillmuxResult},
    ids::SessionId,
    paths::{session_paths as session_paths_in, StatePaths},
    protocol::{
        SessionInspectResponse, SessionListRequest, SessionSelector, SessionSummary,
        M1_PROTOCOL_VERSION,
    },
    state::{MonitorProfile, ProcessState, SessionMeta, SessionRole, WorkerMeta},
    storage::read_json,
    workspace::WorkspaceIdentity,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Core(#[from] MillmuxError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryLoadIssue {
    pub path: PathBuf,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub meta: SessionMeta,
    pub paths: millrace_sessions_core::state::SessionPaths,
    pub worker: Option<WorkerMeta>,
    pub archived: bool,
}

#[derive(Debug, Clone)]
pub struct HostRegistry {
    sessions: BTreeMap<SessionId, SessionRecord>,
    load_issues: Vec<RegistryLoadIssue>,
}

impl HostRegistry {
    pub fn load(paths: StatePaths) -> Result<Self, RegistryError> {
        let mut registry = Self {
            sessions: BTreeMap::new(),
            load_issues: Vec::new(),
        };

        load_session_dir(&mut registry, &paths, &paths.sessions_dir, false)?;
        load_session_dir(&mut registry, &paths, &paths.archive_dir, true)?;

        Ok(registry)
    }

    pub fn sessions(&self) -> &BTreeMap<SessionId, SessionRecord> {
        &self.sessions
    }

    pub fn load_issues(&self) -> &[RegistryLoadIssue] {
        &self.load_issues
    }

    pub fn active_count(&self) -> usize {
        self.sessions
            .values()
            .filter(|record| !record.archived)
            .count()
    }

    pub fn list(&self, request: &SessionListRequest) -> Vec<SessionSummary> {
        let workspace = request
            .workspace
            .as_ref()
            .and_then(|path| WorkspaceIdentity::capture(path).ok());

        self.sessions
            .values()
            .filter(|record| request.include_archived || !record.archived)
            .filter(|record| {
                request
                    .role
                    .as_ref()
                    .map_or(true, |role| record.meta.role == *role)
            })
            .filter(|record| {
                workspace.as_ref().map_or(true, |identity| {
                    record
                        .meta
                        .workspace
                        .as_ref()
                        .is_some_and(|stored| workspace_identity_matches(stored, identity))
                })
            })
            .map(summary_from_record)
            .collect()
    }

    pub fn inspect(&self, selector: &SessionSelector) -> Option<SessionInspectResponse> {
        let record = self.resolve(selector)?;
        Some(SessionInspectResponse {
            schema_version: M1_PROTOCOL_VERSION,
            protocol_version: M1_PROTOCOL_VERSION,
            session: summary_from_record(record),
            paths: record.paths.clone(),
            worker: record.worker.clone(),
        })
    }

    pub fn find_active_millrace_daemon(
        &self,
        identity: &WorkspaceIdentity,
    ) -> Option<&SessionRecord> {
        self.sessions.values().find(|record| {
            !record.archived
                && record.meta.role == SessionRole::MillraceDaemon
                && is_active_process_state(&record.meta.process_state)
                && record
                    .meta
                    .workspace
                    .as_ref()
                    .is_some_and(|stored| workspace_identity_matches(stored, identity))
        })
    }

    pub fn find_duplicate_millrace_daemon(
        &self,
        workspace: impl AsRef<Path>,
    ) -> MillmuxResult<Option<SessionSummary>> {
        let identity = WorkspaceIdentity::capture(workspace)?;
        Ok(self
            .find_active_millrace_daemon(&identity)
            .map(summary_from_record))
    }

    fn resolve(&self, selector: &SessionSelector) -> Option<&SessionRecord> {
        match selector {
            SessionSelector::Id { session_id } => self.sessions.get(session_id),
            SessionSelector::Name { name } => self
                .sessions
                .values()
                .filter(|record| record.meta.name.as_ref() == Some(name))
                .max_by_key(|record| resolution_rank(record)),
            SessionSelector::WorkspaceRole { workspace, role } => {
                let identity = WorkspaceIdentity::capture(workspace).ok()?;
                self.sessions
                    .values()
                    .filter(|record| {
                        record.meta.role == *role
                            && record
                                .meta
                                .workspace
                                .as_ref()
                                .is_some_and(|stored| workspace_identity_matches(stored, &identity))
                    })
                    .max_by_key(|record| resolution_rank(record))
            }
        }
    }
}

fn resolution_rank(record: &SessionRecord) -> (bool, &str, SessionId) {
    (
        is_active_process_state(&record.meta.process_state),
        record.meta.updated_at.as_str(),
        record.meta.id,
    )
}

fn load_session_dir(
    registry: &mut HostRegistry,
    paths: &StatePaths,
    dir: &Path,
    archived: bool,
) -> Result<(), RegistryError> {
    if !dir.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }

        let meta_path = entry.path().join("meta.json");
        if !meta_path.exists() {
            continue;
        }

        match read_json::<SessionMeta>(&meta_path) {
            Ok(meta) => {
                if registry.sessions.contains_key(&meta.id) {
                    continue;
                }
                let session_paths = if archived {
                    session_paths_in(&paths.archive_dir, meta.id)
                } else {
                    paths.session_paths(meta.id)
                };
                let worker = load_optional_worker(&session_paths.worker_json, registry);
                registry.sessions.insert(
                    meta.id,
                    SessionRecord {
                        meta,
                        paths: session_paths,
                        worker,
                        archived,
                    },
                );
            }
            Err(error) => registry.load_issues.push(RegistryLoadIssue {
                path: meta_path,
                error: error.to_string(),
            }),
        }
    }

    Ok(())
}

fn load_optional_worker(path: &Path, registry: &mut HostRegistry) -> Option<WorkerMeta> {
    if !path.exists() {
        return None;
    }

    match read_json::<WorkerMeta>(path) {
        Ok(worker) => Some(worker),
        Err(error) => {
            registry.load_issues.push(RegistryLoadIssue {
                path: path.to_path_buf(),
                error: error.to_string(),
            });
            None
        }
    }
}

fn summary_from_record(record: &SessionRecord) -> SessionSummary {
    summary_from_meta(&record.meta, record.worker.as_ref())
}

fn summary_from_meta(meta: &SessionMeta, worker: Option<&WorkerMeta>) -> SessionSummary {
    let active = is_active_process_state(&meta.process_state);
    let attached_clients = worker
        .filter(|_| active)
        .map_or(0, |worker| worker.attached_clients);
    let input_owner = worker
        .filter(|_| active)
        .and_then(|worker| worker.input_owner.clone());

    SessionSummary {
        session_id: meta.id,
        name: meta.name.clone(),
        role: meta.role.clone(),
        process_state: meta.process_state.clone(),
        attention_state: meta.attention_state.clone(),
        failure_message: meta.failure_message.clone(),
        workspace: meta.workspace.clone(),
        cwd: meta.cwd.clone(),
        argv: meta.argv.clone(),
        monitor_profile: monitor_profile_from_meta(meta),
        created_at: meta.created_at.clone(),
        updated_at: meta.updated_at.clone(),
        attached_clients,
        input_owner,
    }
}

fn monitor_profile_from_meta(meta: &SessionMeta) -> MonitorProfile {
    if !meta.monitor_profile.is_auto() {
        return meta.monitor_profile.clone();
    }
    monitor_profile_from_argv(&meta.argv).unwrap_or_default()
}

fn monitor_profile_from_argv(argv: &[String]) -> Option<MonitorProfile> {
    let mut args = argv.iter();
    while let Some(arg) = args.next() {
        if arg == "--monitor" {
            return args
                .next()
                .and_then(|value| value.parse::<MonitorProfile>().ok());
        }
        if let Some(value) = arg.strip_prefix("--monitor=") {
            return value.parse::<MonitorProfile>().ok();
        }
    }
    None
}

fn workspace_identity_matches(stored: &WorkspaceIdentity, candidate: &WorkspaceIdentity) -> bool {
    if stored.unix_device.is_some()
        && stored.unix_inode.is_some()
        && stored.unix_device == candidate.unix_device
        && stored.unix_inode == candidate.unix_inode
    {
        return true;
    }

    stored.canonical_path == candidate.canonical_path
}

fn is_active_process_state(state: &ProcessState) -> bool {
    matches!(state, ProcessState::Starting | ProcessState::Running)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, str::FromStr};

    use millrace_sessions_core::{
        state::{AttentionState, ProcessState},
        storage::write_json_atomic,
    };

    use super::*;

    #[test]
    fn registry_resolves_sessions_by_id() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StatePaths::new(temp.path().join("state"));
        fs::create_dir_all(&paths.sessions_dir).unwrap();
        let meta = SessionMeta {
            id: SessionId::new(),
            name: Some("shell".to_string()),
            role: SessionRole::Shell,
            process_state: ProcessState::Running,
            attention_state: AttentionState::Active,
            workspace: None,
            cwd: temp.path().to_path_buf(),
            argv: vec!["sh".to_string()],
            monitor_profile: MonitorProfile::Auto,
            env: BTreeMap::new(),
            worker_pid: None,
            child_pid: None,
            child_pgid: None,
            started_at: None,
            ended_at: None,
            exit_code: None,
            exit_signal: None,
            failure_message: None,
            created_at: "2026-05-20T18:00:00Z".to_string(),
            updated_at: "2026-05-20T18:01:00Z".to_string(),
        };
        let session_paths = paths.session_paths(meta.id);
        fs::create_dir_all(&session_paths.root).unwrap();
        write_json_atomic(&session_paths.meta_json, &meta).unwrap();

        let registry = HostRegistry::load(paths).unwrap();
        let inspected = registry
            .inspect(&SessionSelector::Id {
                session_id: meta.id,
            })
            .unwrap();

        assert_eq!(inspected.session.session_id, meta.id);
    }

    #[test]
    fn registry_workspace_role_resolution_prefers_running_session() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let paths = StatePaths::new(temp.path().join("state"));
        fs::create_dir_all(&paths.sessions_dir).unwrap();

        let exited = SessionId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        let running = SessionId::from_str("ffffffff-ffff-ffff-ffff-ffffffffffff").unwrap();
        write_session_meta(
            &paths,
            session_meta(
                exited,
                &workspace,
                ProcessState::Exited,
                "2026-05-20T18:00:00Z",
            ),
        );
        write_session_meta(
            &paths,
            session_meta(
                running,
                &workspace,
                ProcessState::Running,
                "2026-05-20T18:01:00Z",
            ),
        );

        let registry = HostRegistry::load(paths).unwrap();
        let inspected = registry
            .inspect(&SessionSelector::WorkspaceRole {
                workspace,
                role: SessionRole::MillraceDaemon,
            })
            .unwrap();

        assert_eq!(inspected.session.session_id, running);
    }

    fn write_session_meta(paths: &StatePaths, meta: SessionMeta) {
        let session_paths = paths.session_paths(meta.id);
        fs::create_dir_all(&session_paths.root).unwrap();
        write_json_atomic(&session_paths.meta_json, &meta).unwrap();
    }

    fn session_meta(
        id: SessionId,
        workspace: &Path,
        process_state: ProcessState,
        updated_at: &str,
    ) -> SessionMeta {
        SessionMeta {
            id,
            name: Some("daemon:millrace".to_string()),
            role: SessionRole::MillraceDaemon,
            process_state,
            attention_state: AttentionState::Active,
            workspace: Some(WorkspaceIdentity::capture(workspace).unwrap()),
            cwd: workspace.to_path_buf(),
            argv: vec![
                "millrace".to_string(),
                "run".to_string(),
                "daemon".to_string(),
            ],
            monitor_profile: MonitorProfile::Basic,
            env: BTreeMap::new(),
            worker_pid: None,
            child_pid: None,
            child_pgid: None,
            started_at: None,
            ended_at: None,
            exit_code: None,
            exit_signal: None,
            failure_message: None,
            created_at: "2026-05-20T18:00:00Z".to_string(),
            updated_at: updated_at.to_string(),
        }
    }
}
