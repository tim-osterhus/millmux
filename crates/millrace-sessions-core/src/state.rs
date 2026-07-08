use std::{collections::BTreeMap, fmt, path::PathBuf, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::OffsetDateTime;

use crate::{
    ids::{PaneId, SessionId, UiId},
    workspace::WorkspaceIdentity,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRole {
    Shell,
    MillraceDaemon,
    Agent,
    Generic,
    Worker,
    Other(String),
}

impl SessionRole {
    fn as_wire_value(&self) -> &str {
        match self {
            Self::Shell => "shell",
            Self::MillraceDaemon => "millrace_daemon",
            Self::Agent => "agent",
            Self::Generic => "generic",
            Self::Worker => "worker",
            Self::Other(value) => value.as_str(),
        }
    }
}

impl Serialize for SessionRole {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_wire_value())
    }
}

impl<'de> Deserialize<'de> for SessionRole {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "shell" => Self::Shell,
            "millrace_daemon" => Self::MillraceDaemon,
            "agent" => Self::Agent,
            "generic" => Self::Generic,
            "worker" => Self::Worker,
            _ => Self::Other(value),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    Starting,
    Running,
    Orphaned,
    Exited,
    Crashed,
    Killed,
    FailedStart,
    Failed,
    Lost,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LivenessState {
    #[default]
    Unknown,
    Alive,
    Dead,
    Indeterminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLiveness {
    #[serde(default = "default_liveness_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub worker: LivenessState,
    #[serde(default)]
    pub child: LivenessState,
}

fn default_liveness_schema_version() -> u32 {
    1
}

impl Default for SessionLiveness {
    fn default() -> Self {
        Self {
            schema_version: default_liveness_schema_version(),
            worker: LivenessState::default(),
            child: LivenessState::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpawnMode {
    #[default]
    Pty,
    Pipe,
}

impl SpawnMode {
    pub fn is_pty(&self) -> bool {
        matches!(self, Self::Pty)
    }

    pub fn as_wire_value(&self) -> &'static str {
        match self {
            Self::Pty => "pty",
            Self::Pipe => "pipe",
        }
    }
}

impl fmt::Display for SpawnMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire_value())
    }
}

impl FromStr for SpawnMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "pty" => Ok(Self::Pty),
            "pipe" => Ok(Self::Pipe),
            _ => Err("spawn mode must be pty or pipe".to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionState {
    Unknown,
    Active,
    Idle,
    NeedsAttention,
    MillraceIdle,
    MillraceBusy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiMode {
    DaemonConsole,
    AgentCockpit,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MonitorProfile {
    #[default]
    Auto,
    Raw,
    Basic,
    Jsonl,
    Other(String),
}

impl MonitorProfile {
    pub fn is_auto(&self) -> bool {
        matches!(self, Self::Auto)
    }

    pub fn as_wire_value(&self) -> String {
        match self {
            Self::Auto => "auto".to_string(),
            Self::Raw => "raw".to_string(),
            Self::Basic => "basic".to_string(),
            Self::Jsonl => "jsonl".to_string(),
            Self::Other(value) => format!("other:{value}"),
        }
    }
}

impl fmt::Display for MonitorProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_wire_value())
    }
}

impl FromStr for MonitorProfile {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let trimmed = value.trim();
        Ok(match trimmed {
            "auto" => Self::Auto,
            "raw" => Self::Raw,
            "basic" => Self::Basic,
            "jsonl" => Self::Jsonl,
            other if other.starts_with("other:") && other.len() > "other:".len() => {
                Self::Other(other["other:".len()..].to_string())
            }
            other if !other.is_empty() => Self::Other(other.to_string()),
            _ => return Err("monitor profile cannot be empty".to_string()),
        })
    }
}

impl Serialize for MonitorProfile {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.as_wire_value())
    }
}

impl<'de> Deserialize<'de> for MonitorProfile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_str(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiContext {
    pub schema_version: u32,
    pub ui_id: UiId,
    pub mode: UiMode,
    pub active_pane_id: Option<PaneId>,
    pub active_daemon_session_id: Option<SessionId>,
    pub active_workspace: Option<WorkspaceIdentity>,
    pub agent_session_id: Option<SessionId>,
    pub managed_daemon_session_ids: Vec<SessionId>,
    pub monitor_profile: MonitorProfile,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub daemon_health: Vec<UiDaemonHealth>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiDaemonHealth {
    pub session_id: SessionId,
    pub process_state: ProcessState,
    pub attention_state: AttentionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_message: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recovery_actions: Vec<UiDaemonRecoveryAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiDaemonRecoveryAction {
    Inspect,
    Logs,
    Doctor,
    Stop,
    Kill,
    Archive,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiContextPaths {
    pub root: PathBuf,
    pub context_json: PathBuf,
    pub events_jsonl: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiEventKind {
    UiStarted,
    UiAttached,
    UiDetached,
    UiClosed,
    PaneCreated,
    PaneClosed,
    PaneFocused,
    ActiveDaemonChanged,
    AgentSessionBound,
    DaemonSessionBound,
    CommandStarted,
    CommandFinished,
    CommandFailed,
    ScrollModeEntered,
    ScrollModeExited,
    RawInputModeEntered,
    RawInputModeExited,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiEvent {
    pub timestamp: String,
    pub ui_id: UiId,
    pub kind: UiEventKind,
    pub message: Option<String>,
    pub fields: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionPaths {
    pub root: PathBuf,
    pub meta_json: PathBuf,
    pub worker_json: PathBuf,
    pub pty_log: PathBuf,
    pub stdout_log: PathBuf,
    pub stderr_log: PathBuf,
    pub events_jsonl: PathBuf,
    pub scrollback_snapshot: PathBuf,
    pub terminal_snapshot: PathBuf,
    pub raw_replay_ring: PathBuf,
    pub worker_sock: PathBuf,
}

impl<'de> Deserialize<'de> for SessionPaths {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawSessionPaths {
            root: PathBuf,
            meta_json: PathBuf,
            worker_json: PathBuf,
            pty_log: PathBuf,
            #[serde(default)]
            stdout_log: Option<PathBuf>,
            #[serde(default)]
            stderr_log: Option<PathBuf>,
            events_jsonl: PathBuf,
            scrollback_snapshot: PathBuf,
            #[serde(default)]
            terminal_snapshot: Option<PathBuf>,
            #[serde(default)]
            raw_replay_ring: Option<PathBuf>,
            worker_sock: PathBuf,
        }

        let raw = RawSessionPaths::deserialize(deserializer)?;
        let stdout_log = raw
            .stdout_log
            .unwrap_or_else(|| raw.root.join("stdout.log"));
        let stderr_log = raw
            .stderr_log
            .unwrap_or_else(|| raw.root.join("stderr.log"));
        Ok(Self {
            terminal_snapshot: raw
                .terminal_snapshot
                .unwrap_or_else(|| raw.root.join("terminal.snapshot.json")),
            raw_replay_ring: raw
                .raw_replay_ring
                .unwrap_or_else(|| raw.root.join("pty.replay")),
            root: raw.root,
            meta_json: raw.meta_json,
            worker_json: raw.worker_json,
            pty_log: raw.pty_log,
            stdout_log,
            stderr_log,
            events_jsonl: raw.events_jsonl,
            scrollback_snapshot: raw.scrollback_snapshot,
            worker_sock: raw.worker_sock,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: SessionId,
    pub name: Option<String>,
    pub role: SessionRole,
    pub process_state: ProcessState,
    pub attention_state: AttentionState,
    pub workspace: Option<WorkspaceIdentity>,
    pub cwd: PathBuf,
    pub argv: Vec<String>,
    #[serde(default)]
    pub spawn_mode: SpawnMode,
    #[serde(default)]
    pub monitor_profile: MonitorProfile,
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_pgid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_requested_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_signal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_message: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerMeta {
    pub session_id: SessionId,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_pgid: Option<u32>,
    #[serde(default)]
    pub spawn_mode: SpawnMode,
    pub process_state: ProcessState,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_requested_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_signal: Option<String>,
    #[serde(default)]
    pub attached_clients: u32,
    #[serde(default)]
    pub input_owner: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostMeta {
    pub pid: u32,
    pub state_root: PathBuf,
    pub control_socket: PathBuf,
    pub started_at: String,
    pub updated_at: String,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    #[test]
    fn state_enum_values_are_snake_case() {
        assert_eq!(
            serde_json::to_string(&SessionRole::MillraceDaemon).unwrap(),
            "\"millrace_daemon\""
        );
        assert_eq!(
            serde_json::to_string(&SessionRole::Agent).unwrap(),
            "\"agent\""
        );
        assert_eq!(
            serde_json::to_string(&SessionRole::Generic).unwrap(),
            "\"generic\""
        );
        assert_eq!(
            serde_json::to_string(&ProcessState::Running).unwrap(),
            "\"running\""
        );
        assert_eq!(
            serde_json::to_string(&ProcessState::Orphaned).unwrap(),
            "\"orphaned\""
        );
        assert_eq!(
            serde_json::to_string(&LivenessState::Indeterminate).unwrap(),
            "\"indeterminate\""
        );
        assert_eq!(SessionLiveness::default().schema_version, 1);
        assert_eq!(serde_json::to_string(&SpawnMode::Pty).unwrap(), "\"pty\"");
        assert_eq!(serde_json::to_string(&SpawnMode::Pipe).unwrap(), "\"pipe\"");
        assert_eq!(
            serde_json::to_string(&AttentionState::MillraceIdle).unwrap(),
            "\"millrace_idle\""
        );
        assert_eq!(
            serde_json::to_string(&UiMode::DaemonConsole).unwrap(),
            "\"daemon_console\""
        );
        assert_eq!(
            serde_json::to_string(&UiMode::AgentCockpit).unwrap(),
            "\"agent_cockpit\""
        );
        assert_eq!(
            serde_json::to_string(&UiEventKind::ActiveDaemonChanged).unwrap(),
            "\"active_daemon_changed\""
        );
    }

    #[test]
    fn monitor_profiles_use_string_wire_values() {
        assert_eq!(
            serde_json::to_string(&MonitorProfile::Auto).unwrap(),
            "\"auto\""
        );
        assert_eq!(
            serde_json::to_string(&MonitorProfile::Raw).unwrap(),
            "\"raw\""
        );
        assert_eq!(
            serde_json::to_string(&MonitorProfile::Other("custom".to_string())).unwrap(),
            "\"other:custom\""
        );
        assert_eq!(
            serde_json::from_str::<MonitorProfile>("\"other:custom\"").unwrap(),
            MonitorProfile::Other("custom".to_string())
        );
        assert_eq!(
            serde_json::from_str::<MonitorProfile>("\"future\"").unwrap(),
            MonitorProfile::Other("future".to_string())
        );
        assert!(serde_json::from_str::<MonitorProfile>("\"\"").is_err());
    }

    #[test]
    fn session_meta_round_trips() {
        let meta = SessionMeta {
            id: SessionId::new(),
            name: Some("daemon".to_string()),
            role: SessionRole::MillraceDaemon,
            process_state: ProcessState::Running,
            attention_state: AttentionState::MillraceIdle,
            workspace: None,
            cwd: PathBuf::from("/tmp"),
            argv: vec!["millrace".to_string(), "daemon".to_string()],
            spawn_mode: SpawnMode::Pty,
            monitor_profile: MonitorProfile::Auto,
            env: BTreeMap::from([("PATH".to_string(), "/bin".to_string())]),
            worker_pid: Some(100),
            child_pid: Some(101),
            child_pgid: Some(101),
            started_at: Some("2026-05-20T00:00:01Z".to_string()),
            ended_at: None,
            stop_requested_at: None,
            stop_reason: None,
            exit_code: None,
            exit_signal: None,
            failure_message: None,
            created_at: "2026-05-20T00:00:00Z".to_string(),
            updated_at: "2026-05-20T00:00:00Z".to_string(),
        };
        let encoded = serde_json::to_string(&meta).unwrap();
        let decoded: SessionMeta = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, meta);
    }

    #[test]
    fn session_meta_defaults_missing_spawn_mode_to_pty() {
        let value = json!({
            "id": SessionId::new(),
            "name": null,
            "role": "shell",
            "process_state": "running",
            "attention_state": "active",
            "workspace": null,
            "cwd": "/tmp",
            "argv": ["sh"],
            "env": {},
            "created_at": "2026-05-20T00:00:00Z",
            "updated_at": "2026-05-20T00:00:00Z"
        });

        let meta: SessionMeta = serde_json::from_value(value).unwrap();

        assert_eq!(meta.spawn_mode, SpawnMode::Pty);
    }

    #[test]
    fn worker_meta_defaults_missing_spawn_mode_to_pty() {
        let value = json!({
            "session_id": SessionId::new(),
            "pid": 100,
            "process_state": "running",
            "started_at": "2026-05-20T00:00:00Z",
            "updated_at": "2026-05-20T00:00:00Z"
        });

        let worker: WorkerMeta = serde_json::from_value(value).unwrap();

        assert_eq!(worker.spawn_mode, SpawnMode::Pty);
    }

    #[test]
    fn custom_session_roles_round_trip_as_strings() {
        let role = SessionRole::Other("custom_role".to_string());
        let encoded = serde_json::to_string(&role).unwrap();
        assert_eq!(encoded, "\"custom_role\"");
        assert_eq!(serde_json::from_str::<SessionRole>(&encoded).unwrap(), role);
    }

    #[test]
    fn session_paths_deserializes_legacy_paths_without_terminal_replay_fields() {
        let root = PathBuf::from("/state/sessions/session-1");
        let value = json!({
            "root": root,
            "meta_json": "/state/sessions/session-1/meta.json",
            "worker_json": "/state/sessions/session-1/worker.json",
            "pty_log": "/state/sessions/session-1/pty.log",
            "events_jsonl": "/state/sessions/session-1/events.jsonl",
            "scrollback_snapshot": "/state/sessions/session-1/scrollback.snapshot",
            "worker_sock": "/state/w/session-1.sock"
        });

        let paths: SessionPaths = serde_json::from_value(value).unwrap();

        assert_eq!(
            paths.terminal_snapshot,
            PathBuf::from("/state/sessions/session-1/terminal.snapshot.json")
        );
        assert_eq!(
            paths.raw_replay_ring,
            PathBuf::from("/state/sessions/session-1/pty.replay")
        );
        assert_eq!(
            paths.stdout_log,
            PathBuf::from("/state/sessions/session-1/stdout.log")
        );
        assert_eq!(
            paths.stderr_log,
            PathBuf::from("/state/sessions/session-1/stderr.log")
        );
    }
}
