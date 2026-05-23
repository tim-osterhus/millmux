use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{ids::SessionId, workspace::WorkspaceIdentity};

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
pub struct SessionPaths {
    pub root: PathBuf,
    pub meta_json: PathBuf,
    pub worker_json: PathBuf,
    pub pty_log: PathBuf,
    pub events_jsonl: PathBuf,
    pub scrollback_snapshot: PathBuf,
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
