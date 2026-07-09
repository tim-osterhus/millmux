use std::{collections::BTreeMap, fmt, path::PathBuf, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::OffsetDateTime;
use uuid::Uuid;

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
    pub fn as_wire_value(&self) -> &str {
        match self {
            Self::Shell => "shell",
            Self::MillraceDaemon => "millrace_daemon",
            Self::Agent => "millrace_agent",
            Self::Generic => "generic",
            Self::Worker | Self::Other(_) => "generic",
        }
    }

    pub fn from_wire_value(value: &str) -> Result<Self, String> {
        match value {
            "shell" => Ok(Self::Shell),
            "millrace_daemon" => Ok(Self::MillraceDaemon),
            "millrace_agent" => Ok(Self::Agent),
            "generic" => Ok(Self::Generic),
            _ => Err(format!(
                "invalid role: {value}; expected shell, millrace_daemon, millrace_agent, or generic"
            )),
        }
    }

    pub fn from_cli_value(value: &str) -> Result<Self, String> {
        let normalized = value.trim().to_ascii_lowercase().replace('-', "_");
        match normalized.as_str() {
            "shell" => Ok(Self::Shell),
            "millrace_daemon" => Ok(Self::MillraceDaemon),
            "millrace_agent" | "agent" => Ok(Self::Agent),
            "generic" => Ok(Self::Generic),
            _ => Err(format!(
                "invalid role: {value}; expected shell, millrace-daemon, millrace-agent, or generic"
            )),
        }
    }

    fn from_persisted_value(value: &str) -> Self {
        Self::from_cli_value(value).unwrap_or(Self::Generic)
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
        Self::from_wire_value(&value).map_err(serde::de::Error::custom)
    }
}

fn deserialize_persisted_session_role<'de, D>(deserializer: D) -> Result<SessionRole, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    Ok(SessionRole::from_persisted_value(&value))
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionTargetType {
    Workspace,
    Session,
    Pane,
}

impl AttentionTargetType {
    pub fn as_wire_value(&self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Session => "session",
            Self::Pane => "pane",
        }
    }
}

impl fmt::Display for AttentionTargetType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire_value())
    }
}

impl FromStr for AttentionTargetType {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "workspace" => Ok(Self::Workspace),
            "session" => Ok(Self::Session),
            "pane" => Ok(Self::Pane),
            _ => Err("attention target type must be workspace, session, or pane".to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    Unread,
    NeedsInput,
    ApprovalRequired,
    Blocked,
    Failed,
    Degraded,
    HandoffPending,
    Stale,
}

impl AttentionKind {
    pub fn as_wire_value(&self) -> &'static str {
        match self {
            Self::Unread => "unread",
            Self::NeedsInput => "needs_input",
            Self::ApprovalRequired => "approval_required",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Degraded => "degraded",
            Self::HandoffPending => "handoff_pending",
            Self::Stale => "stale",
        }
    }
}

impl fmt::Display for AttentionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire_value())
    }
}

impl FromStr for AttentionKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "unread" => Ok(Self::Unread),
            "needs_input" => Ok(Self::NeedsInput),
            "approval_required" => Ok(Self::ApprovalRequired),
            "blocked" => Ok(Self::Blocked),
            "failed" => Ok(Self::Failed),
            "degraded" => Ok(Self::Degraded),
            "handoff_pending" => Ok(Self::HandoffPending),
            "stale" => Ok(Self::Stale),
            _ => Err("attention kind must be unread, needs_input, approval_required, blocked, failed, degraded, handoff_pending, or stale".to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionSeverity {
    Info,
    Progress,
    Success,
    Warning,
    Error,
    Critical,
}

impl AttentionSeverity {
    pub fn as_wire_value(&self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Progress => "progress",
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Error => "error",
            Self::Critical => "critical",
        }
    }

    pub fn rank(self) -> u8 {
        match self {
            Self::Info => 0,
            Self::Progress => 1,
            Self::Success => 2,
            Self::Warning => 3,
            Self::Error => 4,
            Self::Critical => 5,
        }
    }
}

impl fmt::Display for AttentionSeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire_value())
    }
}

impl FromStr for AttentionSeverity {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "info" => Ok(Self::Info),
            "progress" => Ok(Self::Progress),
            "success" => Ok(Self::Success),
            "warning" => Ok(Self::Warning),
            "error" => Ok(Self::Error),
            "critical" => Ok(Self::Critical),
            _ => Err(
                "attention severity must be info, progress, success, warning, error, or critical"
                    .to_string(),
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionSource {
    Operator,
    Cli,
    Api,
    Millmux,
    Millrace,
    Agent,
}

impl AttentionSource {
    pub fn as_wire_value(&self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::Cli => "cli",
            Self::Api => "api",
            Self::Millmux => "millmux",
            Self::Millrace => "millrace",
            Self::Agent => "agent",
        }
    }
}

impl fmt::Display for AttentionSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire_value())
    }
}

impl FromStr for AttentionSource {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "operator" => Ok(Self::Operator),
            "cli" => Ok(Self::Cli),
            "api" => Ok(Self::Api),
            "millmux" => Ok(Self::Millmux),
            "millrace" => Ok(Self::Millrace),
            "agent" => Ok(Self::Agent),
            _ => Err(
                "attention source must be operator, cli, api, millmux, millrace, or agent"
                    .to_string(),
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionItem {
    pub id: String,
    pub target_type: AttentionTargetType,
    pub target_id: String,
    pub kind: AttentionKind,
    pub severity: AttentionSeverity,
    pub source: AttentionSource,
    pub message: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleared_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_detail: Option<String>,
}

impl AttentionItem {
    pub fn new(
        target_type: AttentionTargetType,
        target_id: impl Into<String>,
        kind: AttentionKind,
        severity: AttentionSeverity,
        source: AttentionSource,
        message: impl Into<String>,
        created_at: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            target_type,
            target_id: target_id.into(),
            kind,
            severity,
            source,
            message: message.into(),
            created_at: created_at.into(),
            read_at: None,
            cleared_at: None,
            dedupe_key: None,
            status_label: None,
            status_detail: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.cleared_at.is_none()
    }

    pub fn is_unread(&self) -> bool {
        self.is_open() && self.kind == AttentionKind::Unread && self.read_at.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRollup {
    #[serde(default = "default_attention_rollup_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub open_count: u32,
    #[serde(default)]
    pub unread_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highest_severity: Option<AttentionSeverity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<AttentionKind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<AttentionSource>,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub read_open_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_detail: Option<String>,
}

fn default_attention_rollup_schema_version() -> u32 {
    1
}

impl Default for AttentionRollup {
    fn default() -> Self {
        Self {
            schema_version: default_attention_rollup_schema_version(),
            open_count: 0,
            unread_count: 0,
            highest_severity: None,
            kinds: Vec::new(),
            sources: Vec::new(),
            read_open_count: 0,
            top_message: None,
            status_label: None,
            status_detail: None,
        }
    }
}

impl AttentionRollup {
    pub fn from_items(items: &[AttentionItem]) -> Self {
        let mut rollup = Self::default();
        let open_items = items.iter().filter(|item| item.is_open());
        let mut top: Option<&AttentionItem> = None;

        for item in open_items {
            rollup.open_count = rollup.open_count.saturating_add(1);
            if item.is_unread() {
                rollup.unread_count = rollup.unread_count.saturating_add(1);
            } else if item.read_at.is_some() {
                rollup.read_open_count = rollup.read_open_count.saturating_add(1);
            }
            if !rollup.kinds.contains(&item.kind) {
                rollup.kinds.push(item.kind);
            }
            if !rollup.sources.contains(&item.source) {
                rollup.sources.push(item.source);
            }
            if top.map_or(true, |candidate| {
                item.severity.rank() > candidate.severity.rank()
                    || (item.severity == candidate.severity
                        && item.created_at > candidate.created_at)
            }) {
                top = Some(item);
            }
        }

        if let Some(top) = top {
            rollup.highest_severity = Some(top.severity);
            rollup.top_message = Some(top.message.clone());
            rollup.status_label = top.status_label.clone();
            rollup.status_detail = top.status_detail.clone();
        }
        rollup.kinds.sort();
        rollup.sources.sort();
        rollup
    }
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusSummarySource {
    MillraceRuntime,
    MillmuxSession,
    TerminalScreen,
    Operator,
    Inferred,
    Unavailable,
}

impl StatusSummarySource {
    pub fn as_wire_value(&self) -> &'static str {
        match self {
            Self::MillraceRuntime => "millrace_runtime",
            Self::MillmuxSession => "millmux_session",
            Self::TerminalScreen => "terminal_screen",
            Self::Operator => "operator",
            Self::Inferred => "inferred",
            Self::Unavailable => "unavailable",
        }
    }
}

impl fmt::Display for StatusSummarySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire_value())
    }
}

impl FromStr for StatusSummarySource {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "millrace_runtime" => Ok(Self::MillraceRuntime),
            "millmux_session" => Ok(Self::MillmuxSession),
            "terminal_screen" => Ok(Self::TerminalScreen),
            "operator" => Ok(Self::Operator),
            "inferred" => Ok(Self::Inferred),
            "unavailable" => Ok(Self::Unavailable),
            _ => Err("status summary source must be millrace_runtime, millmux_session, terminal_screen, operator, inferred, or unavailable".to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusSummary {
    #[serde(default = "default_status_summary_schema_version")]
    pub schema_version: u32,
    pub source: StatusSummarySource,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

fn default_status_summary_schema_version() -> u32 {
    1
}

impl Default for StatusSummary {
    fn default() -> Self {
        Self::unavailable()
    }
}

impl StatusSummary {
    pub fn unavailable() -> Self {
        Self {
            schema_version: default_status_summary_schema_version(),
            source: StatusSummarySource::Unavailable,
            label: "unavailable".to_string(),
            detail: None,
            updated_at: None,
        }
    }

    pub fn millmux_session(label: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            schema_version: default_status_summary_schema_version(),
            source: StatusSummarySource::MillmuxSession,
            label: label.into(),
            detail,
            updated_at: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiMode {
    DaemonConsole,
    AgentCockpit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiPaneViewKind {
    SessionTerminal,
    DaemonMonitor,
    SessionList,
    CommandOutput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiPaneViewMode {
    #[default]
    Live,
    Scrollback,
}

impl UiPaneViewMode {
    pub fn is_live(&self) -> bool {
        matches!(self, Self::Live)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiPaneView {
    pub kind: UiPaneViewKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "UiPaneViewMode::is_live")]
    pub view_mode: UiPaneViewMode,
}

impl UiPaneView {
    pub fn new(kind: UiPaneViewKind, session_id: Option<SessionId>) -> Self {
        Self {
            kind,
            session_id,
            view_mode: UiPaneViewMode::Live,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiPaneContext {
    pub id: PaneId,
    pub title: String,
    pub view: UiPaneView,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub stale: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub overlay_active: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub panes: Vec<UiPaneContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_pane_kind: Option<String>,
    pub active_daemon_session_id: Option<SessionId>,
    pub active_workspace: Option<WorkspaceIdentity>,
    pub agent_session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub managed_session_ids: Vec<SessionId>,
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
    InputAccepted,
    InputRejected,
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
    #[serde(deserialize_with = "deserialize_persisted_session_role")]
    pub role: SessionRole,
    pub process_state: ProcessState,
    pub attention_state: AttentionState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attention_items: Vec<AttentionItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_summary: Option<StatusSummary>,
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
            "\"millrace_agent\""
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
            serde_json::to_string(&AttentionKind::ApprovalRequired).unwrap(),
            "\"approval_required\""
        );
        assert_eq!(
            serde_json::to_string(&AttentionSeverity::Critical).unwrap(),
            "\"critical\""
        );
        assert_eq!(
            serde_json::to_string(&AttentionSource::Millrace).unwrap(),
            "\"millrace\""
        );
        assert_eq!(
            serde_json::to_string(&StatusSummarySource::MillmuxSession).unwrap(),
            "\"millmux_session\""
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
            serde_json::to_string(&UiPaneViewKind::SessionTerminal).unwrap(),
            "\"session_terminal\""
        );
        assert_eq!(
            serde_json::to_string(&UiPaneViewMode::Scrollback).unwrap(),
            "\"scrollback\""
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
            attention_items: vec![AttentionItem::new(
                AttentionTargetType::Session,
                "session-1",
                AttentionKind::Unread,
                AttentionSeverity::Info,
                AttentionSource::Millmux,
                "new output",
                "2026-05-20T00:00:00Z",
            )],
            status_summary: Some(StatusSummary::millmux_session(
                "running",
                Some("worker alive".to_string()),
            )),
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
        assert!(meta.attention_items.is_empty());
        assert_eq!(meta.status_summary, None);
    }

    #[test]
    fn attention_rollup_counts_open_and_unread_items() {
        let mut unread = AttentionItem::new(
            AttentionTargetType::Session,
            "session-1",
            AttentionKind::Unread,
            AttentionSeverity::Info,
            AttentionSource::Millmux,
            "new output",
            "2026-05-20T00:00:00Z",
        );
        let blocking = AttentionItem::new(
            AttentionTargetType::Session,
            "session-1",
            AttentionKind::Blocked,
            AttentionSeverity::Critical,
            AttentionSource::Agent,
            "blocked",
            "2026-05-20T00:00:01Z",
        );
        let mut cleared = AttentionItem::new(
            AttentionTargetType::Session,
            "session-1",
            AttentionKind::Failed,
            AttentionSeverity::Error,
            AttentionSource::Millrace,
            "old failure",
            "2026-05-20T00:00:02Z",
        );
        cleared.cleared_at = Some("2026-05-20T00:00:03Z".to_string());
        unread.status_label = Some("unread".to_string());

        let rollup = AttentionRollup::from_items(&[unread, blocking, cleared]);

        assert_eq!(rollup.open_count, 2);
        assert_eq!(rollup.unread_count, 1);
        assert_eq!(rollup.read_open_count, 0);
        assert_eq!(rollup.highest_severity, Some(AttentionSeverity::Critical));
        assert_eq!(
            rollup.kinds,
            vec![AttentionKind::Unread, AttentionKind::Blocked]
        );
        assert_eq!(
            rollup.sources,
            vec![AttentionSource::Millmux, AttentionSource::Agent]
        );
        assert_eq!(rollup.top_message.as_deref(), Some("blocked"));
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
    fn session_roles_deserialize_only_canonical_wire_values() {
        assert_eq!(
            serde_json::from_str::<SessionRole>("\"millrace_agent\"").unwrap(),
            SessionRole::Agent
        );
        assert!(serde_json::from_str::<SessionRole>("\"agent\"").is_err());
        assert!(serde_json::from_str::<SessionRole>("\"millrace-agent\"").is_err());
        assert!(serde_json::from_str::<SessionRole>("\"custom_role\"").is_err());
    }

    #[test]
    fn session_meta_migrates_legacy_persisted_roles_to_generic() {
        let mut value = json!({
            "id": SessionId::new(),
            "name": "legacy",
            "role": "worker",
            "process_state": "running",
            "attention_state": "active",
            "workspace": null,
            "cwd": "/tmp",
            "argv": ["sh"],
            "spawn_mode": "pty",
            "monitor_profile": "auto",
            "env": {},
            "created_at": "2026-05-20T00:00:00Z",
            "updated_at": "2026-05-20T00:00:00Z"
        });

        let worker: SessionMeta = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(worker.role, SessionRole::Generic);

        value["role"] = json!("agent");
        let agent: SessionMeta = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(agent.role, SessionRole::Agent);

        value["role"] = json!("custom_helper");
        let custom: SessionMeta = serde_json::from_value(value).unwrap();
        assert_eq!(custom.role, SessionRole::Generic);
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
