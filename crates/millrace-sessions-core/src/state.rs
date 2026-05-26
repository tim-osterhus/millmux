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
    Exited,
    Crashed,
    Killed,
    FailedStart,
    Failed,
    Lost,
    Stale,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPaths {
    pub root: PathBuf,
    pub meta_json: PathBuf,
    pub worker_json: PathBuf,
    pub pty_log: PathBuf,
    pub events_jsonl: PathBuf,
    pub scrollback_snapshot: PathBuf,
    pub terminal_snapshot: PathBuf,
    pub raw_replay_ring: PathBuf,
    pub worker_sock: PathBuf,
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
    pub process_state: ProcessState,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
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
            monitor_profile: MonitorProfile::Auto,
            env: BTreeMap::from([("PATH".to_string(), "/bin".to_string())]),
            worker_pid: Some(100),
            child_pid: Some(101),
            child_pgid: Some(101),
            started_at: Some("2026-05-20T00:00:01Z".to_string()),
            ended_at: None,
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
    fn custom_session_roles_round_trip_as_strings() {
        let role = SessionRole::Other("custom_role".to_string());
        let encoded = serde_json::to_string(&role).unwrap();
        assert_eq!(encoded, "\"custom_role\"");
        assert_eq!(serde_json::from_str::<SessionRole>(&encoded).unwrap(), role);
    }
}
