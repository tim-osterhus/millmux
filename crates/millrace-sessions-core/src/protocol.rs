use std::{collections::BTreeMap, fmt, path::PathBuf};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::{
    events::SessionEvent,
    ids::{SessionId, UiId},
    state::{
        AttentionState, HostMeta, MonitorProfile, ProcessState, SessionLiveness, SessionPaths,
        SessionRole, SpawnMode, UiContext, UiContextPaths, UiEvent, WorkerMeta,
    },
    workspace::WorkspaceIdentity,
};

pub const M1_PROTOCOL_VERSION: u32 = 1;
pub const M2_ATTACH_PROTOCOL_VERSION: u32 = 2;
pub const SESSION_CAPABILITIES_SCHEMA_VERSION: u32 = 1;
pub const SESSION_ARTIFACTS_SCHEMA_VERSION: u32 = 1;
pub const SCREEN_SNAPSHOT_SCHEMA_VERSION: u32 = 1;
pub const MAX_SCREEN_SNAPSHOT_CELLS: usize = 80_000;
pub const MAX_SCREEN_SNAPSHOT_JSON_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlMethod {
    #[serde(rename = "host.status")]
    HostStatus,
    #[serde(rename = "host.doctor")]
    HostDoctor,
    #[serde(rename = "session.start")]
    SessionStart,
    #[serde(rename = "session.attach")]
    SessionAttach,
    #[serde(rename = "session.list")]
    SessionList,
    #[serde(rename = "session.inspect")]
    SessionInspect,
    #[serde(rename = "session.screen")]
    SessionScreen,
    #[serde(rename = "session.logs")]
    SessionLogs,
    #[serde(rename = "session.events")]
    SessionEvents,
    #[serde(rename = "session.send")]
    SessionSend,
    #[serde(rename = "session.resize")]
    SessionResize,
    #[serde(rename = "session.stop")]
    SessionStop,
    #[serde(rename = "session.kill")]
    SessionKill,
    #[serde(rename = "session.delete")]
    SessionDelete,
    #[serde(rename = "ui.context.get")]
    UiContextGet,
    #[serde(rename = "ui.context.set")]
    UiContextSet,
    #[serde(rename = "ui.context.list")]
    UiContextList,
    #[serde(rename = "ui.context.close")]
    UiContextClose,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlRequest {
    pub id: String,
    pub method: ControlMethod,
    #[serde(default = "empty_params")]
    pub params: Value,
}

impl ControlRequest {
    pub fn new(id: impl Into<String>, method: ControlMethod) -> Self {
        Self {
            id: id.into(),
            method,
            params: empty_params(),
        }
    }

    pub fn with_params<T>(
        id: impl Into<String>,
        method: ControlMethod,
        params: &T,
    ) -> serde_json::Result<Self>
    where
        T: Serialize,
    {
        Ok(Self {
            id: id.into(),
            method,
            params: serde_json::to_value(params)?,
        })
    }

    pub fn params_as<T>(&self) -> serde_json::Result<T>
    where
        T: DeserializeOwned,
    {
        serde_json::from_value(self.params.clone())
    }

    pub fn to_json_line(&self) -> serde_json::Result<String> {
        to_json_line(self)
    }

    pub fn from_json_line(line: &str) -> serde_json::Result<Self> {
        serde_json::from_str(line)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlResponse {
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ControlErrorBody>,
}

impl ControlResponse {
    pub fn success<T>(id: impl Into<String>, result: &T) -> serde_json::Result<Self>
    where
        T: Serialize,
    {
        Ok(Self {
            id: id.into(),
            ok: true,
            result: Some(serde_json::to_value(result)?),
            error: None,
        })
    }

    pub fn empty_success(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ok: true,
            result: Some(empty_params()),
            error: None,
        }
    }

    pub fn failure(id: impl Into<String>, error: ControlErrorBody) -> Self {
        Self {
            id: id.into(),
            ok: false,
            result: None,
            error: Some(error),
        }
    }

    pub fn result_as<T>(&self) -> serde_json::Result<T>
    where
        T: DeserializeOwned,
    {
        serde_json::from_value(self.result.clone().unwrap_or(Value::Null))
    }

    pub fn to_json_line(&self) -> serde_json::Result<String> {
        to_json_line(self)
    }

    pub fn from_json_line(line: &str) -> serde_json::Result<Self> {
        serde_json::from_str(line)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlErrorCode {
    HostUnavailable,
    InvalidRequest,
    UnknownMethod,
    SessionNotFound,
    SessionNotRunning,
    DuplicateMillraceDaemon,
    WorkspaceNotFound,
    WorkspaceIdentityConflict,
    CommandNotFound,
    UnsafeDeleteRunning,
    InputOwnerConflict,
    PermissionError,
    IoError,
    WorkerUnavailable,
    MillraceStopFailed,
    UiContextNotFound,
    AmbiguousUiContext,
    UnsupportedSpawnMode,
    InternalError,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlErrorBody {
    pub code: ControlErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ControlErrorBody {
    pub fn new(code: ControlErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

impl fmt::Display for ControlErrorBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = serde_json::to_value(self.code)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_else(|| "internal_error".to_string());
        write!(f, "{code}: {}", self.message)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HostStatusRequest {}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DoctorRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repair: Option<DoctorRepairMode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DoctorRepairMode {
    #[serde(rename = "ARCHIVE_STALE")]
    ArchiveStale,
    #[serde(rename = "CLOSE_STALE_UI_CONTEXTS")]
    CloseStaleUiContexts,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionSelector {
    Id {
        session_id: SessionId,
    },
    Name {
        name: String,
    },
    WorkspaceRole {
        workspace: PathBuf,
        role: SessionRole,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStartRequest {
    pub argv: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<SessionRole>,
    #[serde(default, skip_serializing_if = "SpawnMode::is_pty")]
    pub spawn_mode: SpawnMode,
    #[serde(default, skip_serializing_if = "MonitorProfile::is_auto")]
    pub monitor_profile: MonitorProfile,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionListRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<SessionRole>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_archived: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInspectRequest {
    pub selector: SessionSelector,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionScreenRequest {
    pub selector: SessionSelector,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_terminal_size: Option<TerminalDimensions>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachReplayMode {
    #[default]
    None,
    LineScrollback,
    RawReplay,
    TerminalSnapshot,
}

impl AttachReplayMode {
    pub fn is_none(value: &Self) -> bool {
        matches!(value, Self::None)
    }

    pub fn uses_raw_payloads(&self) -> bool {
        matches!(self, Self::RawReplay | Self::TerminalSnapshot)
    }

    fn from_legacy_include_scrollback(include_scrollback: bool) -> Self {
        if include_scrollback {
            Self::LineScrollback
        } else {
            Self::None
        }
    }
}

impl fmt::Display for AttachReplayMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::None => "none",
            Self::LineScrollback => "line_scrollback",
            Self::RawReplay => "raw_replay",
            Self::TerminalSnapshot => "terminal_snapshot",
        };
        f.write_str(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachFrameType {
    RawOutput,
    RawInput,
    StreamLagged,
    SnapshotUnavailable,
    ScreenSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachStreamEncoding {
    #[default]
    Text,
    RawBytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachInitialReplay {
    #[default]
    None,
    LineScrollback,
    RawReplay,
    ScreenSnapshot,
}

impl AttachInitialReplay {
    pub fn from_legacy_replay(replay: AttachReplayMode) -> Self {
        match replay {
            AttachReplayMode::None => Self::None,
            AttachReplayMode::LineScrollback => Self::LineScrollback,
            AttachReplayMode::RawReplay | AttachReplayMode::TerminalSnapshot => Self::RawReplay,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotUnavailableReason {
    NoSnapshot,
    StaleSnapshot,
    SizeMismatch,
    UnsupportedSpawnMode,
    PayloadTooLarge,
    TerminalModelUnavailable,
    PermissionDenied,
    InternalError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachStreamLagReason {
    ObserverBackpressure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalDimensions {
    pub rows: u16,
    pub cols: u16,
}

impl TerminalDimensions {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            rows: rows.max(1),
            cols: cols.max(1),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionAttachRequest {
    pub selector: SessionSelector,
    #[serde(default, skip_serializing_if = "is_false")]
    pub read_only: bool,
    #[serde(default)]
    pub replay: AttachReplayMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_terminal_size: Option<TerminalDimensions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_protocol_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepted_frame_types: Vec<AttachFrameType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_encoding: Option<AttachStreamEncoding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_replay: Option<AttachInitialReplay>,
}

impl SessionAttachRequest {
    pub fn requests_raw_stream(&self) -> bool {
        self.stream_encoding == Some(AttachStreamEncoding::RawBytes)
    }

    pub fn negotiated_attach_protocol_version(&self) -> Option<u32> {
        self.client_protocol_version
            .filter(|version| *version >= M2_ATTACH_PROTOCOL_VERSION)
            .map(|version| version.min(M2_ATTACH_PROTOCOL_VERSION))
    }

    pub fn negotiated_stream_encoding(&self) -> Option<AttachStreamEncoding> {
        self.negotiated_attach_protocol_version().map(|_| {
            match self.stream_encoding.unwrap_or_default() {
                AttachStreamEncoding::RawBytes
                    if self.client_accepts_frame_type(AttachFrameType::RawOutput) =>
                {
                    AttachStreamEncoding::RawBytes
                }
                _ => AttachStreamEncoding::Text,
            }
        })
    }

    pub fn negotiated_initial_replay(&self) -> Option<AttachInitialReplay> {
        self.negotiated_attach_protocol_version().and_then(|_| {
            match self
                .initial_replay
                .unwrap_or_else(|| AttachInitialReplay::from_legacy_replay(self.replay))
            {
                AttachInitialReplay::RawReplay
                    if !self.client_accepts_frame_type(AttachFrameType::RawOutput) =>
                {
                    None
                }
                AttachInitialReplay::ScreenSnapshot
                    if !self.client_accepts_frame_type(AttachFrameType::ScreenSnapshot)
                        || !self
                            .client_accepts_frame_type(AttachFrameType::SnapshotUnavailable) =>
                {
                    None
                }
                replay => Some(replay),
            }
        })
    }

    pub fn negotiated_frame_types(&self) -> Vec<AttachFrameType> {
        let mut frame_types = Vec::new();
        if self.negotiated_attach_protocol_version().is_none() {
            return frame_types;
        }

        if self.negotiated_stream_encoding() == Some(AttachStreamEncoding::RawBytes)
            && self.client_accepts_frame_type(AttachFrameType::RawOutput)
        {
            frame_types.push(AttachFrameType::RawOutput);
        }

        if self.negotiated_stream_encoding() == Some(AttachStreamEncoding::RawBytes)
            && !self.read_only
            && self.client_accepts_frame_type(AttachFrameType::RawInput)
        {
            frame_types.push(AttachFrameType::RawInput);
        }

        if self.client_accepts_frame_type(AttachFrameType::StreamLagged) {
            frame_types.push(AttachFrameType::StreamLagged);
        }

        match self.negotiated_initial_replay() {
            Some(AttachInitialReplay::RawReplay)
                if self.client_accepts_frame_type(AttachFrameType::RawOutput)
                    && !frame_types.contains(&AttachFrameType::RawOutput) =>
            {
                frame_types.push(AttachFrameType::RawOutput);
            }
            Some(AttachInitialReplay::ScreenSnapshot)
                if self.client_accepts_frame_type(AttachFrameType::ScreenSnapshot)
                    && self.client_accepts_frame_type(AttachFrameType::SnapshotUnavailable) =>
            {
                frame_types.push(AttachFrameType::ScreenSnapshot);
                frame_types.push(AttachFrameType::SnapshotUnavailable);
            }
            _ => {}
        }

        frame_types
    }

    pub fn accepts_frame_type(&self, frame_type: AttachFrameType) -> bool {
        self.client_accepts_frame_type(frame_type)
    }

    pub fn client_accepts_frame_type(&self, frame_type: AttachFrameType) -> bool {
        self.negotiated_attach_protocol_version().is_some()
            && self.accepted_frame_types.contains(&frame_type)
    }
}

impl<'de> Deserialize<'de> for SessionAttachRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            selector: SessionSelector,
            #[serde(default)]
            read_only: bool,
            #[serde(default)]
            replay: Option<AttachReplayMode>,
            #[serde(default)]
            requested_terminal_size: Option<TerminalDimensions>,
            #[serde(default)]
            include_scrollback: Option<bool>,
            #[serde(default)]
            client_protocol_version: Option<u32>,
            #[serde(default)]
            accepted_frame_types: Vec<AttachFrameType>,
            #[serde(default)]
            stream_encoding: Option<AttachStreamEncoding>,
            #[serde(default)]
            initial_replay: Option<AttachInitialReplay>,
        }

        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            selector: wire.selector,
            read_only: wire.read_only,
            replay: wire.replay.unwrap_or_else(|| {
                wire.include_scrollback
                    .map(AttachReplayMode::from_legacy_include_scrollback)
                    .unwrap_or_default()
            }),
            requested_terminal_size: wire.requested_terminal_size,
            client_protocol_version: wire.client_protocol_version,
            accepted_frame_types: wire.accepted_frame_types,
            stream_encoding: wire.stream_encoding,
            initial_replay: wire.initial_replay,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLogsRequest {
    pub selector: SessionSelector,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tail: Option<usize>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub follow: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEventsRequest {
    pub selector: SessionSelector,
    #[serde(default, skip_serializing_if = "is_false")]
    pub follow: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSendRequest {
    pub selector: SessionSelector,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResizeRequest {
    pub selector: SessionSelector,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStopRequest {
    pub selector: SessionSelector,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grace_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionKillRequest {
    pub selector: SessionSelector,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDeleteRequest {
    pub selector: SessionSelector,
    #[serde(default, skip_serializing_if = "is_false")]
    pub purge: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub kill: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UiContextGetRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ui_id: Option<UiId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiContextSetRequest {
    pub context: UiContext,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<UiEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UiContextListRequest {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiContextCloseRequest {
    pub ui_id: UiId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostStatusResponse {
    pub schema_version: u32,
    pub protocol_version: u32,
    pub host: Option<HostMeta>,
    pub session_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoctorResponse {
    pub schema_version: u32,
    pub protocol_version: u32,
    pub status: DoctorStatus,
    pub issues: Vec<DoctorIssue>,
    pub repairs: Vec<DoctorRepair>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionScreenResponse {
    pub protocol_version: u32,
    pub session_id: SessionId,
    #[serde(flatten)]
    pub frame: ScreenFrame,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScreenFrame {
    ScreenSnapshot {
        #[serde(flatten)]
        snapshot: ScreenSnapshot,
    },
    SnapshotUnavailable {
        reason: SnapshotUnavailableReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenSnapshot {
    pub schema_version: u32,
    pub rows: u16,
    pub cols: u16,
    pub cursor: ScreenCursor,
    pub alternate_screen: bool,
    pub cells: Vec<Vec<ScreenCell>>,
    pub source: ScreenSnapshotSource,
    pub captured_at: String,
}

impl ScreenSnapshot {
    pub fn validate_for_wire(&self) -> Result<(), ScreenSnapshotValidationError> {
        let cell_count = usize::from(self.rows).saturating_mul(usize::from(self.cols));
        if cell_count > MAX_SCREEN_SNAPSHOT_CELLS {
            return Err(ScreenSnapshotValidationError::TooManyCells {
                rows: self.rows,
                cols: self.cols,
                cell_count,
                max_cells: MAX_SCREEN_SNAPSHOT_CELLS,
            });
        }

        if self.cells.len() != usize::from(self.rows) {
            return Err(ScreenSnapshotValidationError::InvalidShape(format!(
                "screen_snapshot rows={} but cells has {} rows",
                self.rows,
                self.cells.len()
            )));
        }

        for (row_index, row) in self.cells.iter().enumerate() {
            if row.len() != usize::from(self.cols) {
                return Err(ScreenSnapshotValidationError::InvalidShape(format!(
                    "screen_snapshot row {row_index} has {} cells; expected {}",
                    row.len(),
                    self.cols
                )));
            }
        }

        let serialized_bytes = serde_json::to_vec(self)
            .map_err(ScreenSnapshotValidationError::Serialize)?
            .len();
        if serialized_bytes > MAX_SCREEN_SNAPSHOT_JSON_BYTES {
            return Err(ScreenSnapshotValidationError::PayloadTooLarge {
                rows: self.rows,
                cols: self.cols,
                cell_count,
                serialized_bytes,
                max_serialized_bytes: MAX_SCREEN_SNAPSHOT_JSON_BYTES,
            });
        }

        Ok(())
    }

    pub fn plain_lines(&self) -> Vec<String> {
        self.cells
            .iter()
            .map(|row| {
                row.iter()
                    .filter(|cell| !cell.continuation)
                    .map(|cell| cell.symbol.as_str())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenCursor {
    pub row: u16,
    pub col: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenSnapshotSource {
    pub pty_log_offset: u64,
    pub raw_replay_start_offset: u64,
    pub raw_replay_end_offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenCell {
    pub symbol: String,
    #[serde(default = "default_screen_cell_width", skip_serializing_if = "is_one")]
    pub width: u8,
    #[serde(default, skip_serializing_if = "ScreenColor::is_default")]
    pub fg: ScreenColor,
    #[serde(default, skip_serializing_if = "ScreenColor::is_default")]
    pub bg: ScreenColor,
    #[serde(default, skip_serializing_if = "ScreenStyle::is_default")]
    pub style: ScreenStyle,
    #[serde(default, skip_serializing_if = "is_false")]
    pub continuation: bool,
}

impl ScreenCell {
    pub fn blank() -> Self {
        Self {
            symbol: " ".to_string(),
            width: 1,
            fg: ScreenColor::Default,
            bg: ScreenColor::Default,
            style: ScreenStyle::default(),
            continuation: false,
        }
    }

    pub fn default_symbol(symbol: impl Into<String>) -> Self {
        Self {
            symbol: symbol.into(),
            ..Self::blank()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScreenColor {
    Default,
    Indexed { index: u8 },
    Rgb { r: u8, g: u8, b: u8 },
}

impl Default for ScreenColor {
    fn default() -> Self {
        Self::Default
    }
}

impl ScreenColor {
    fn is_default(&self) -> bool {
        matches!(self, Self::Default)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ScreenStyle {
    #[serde(default, skip_serializing_if = "is_false")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub dim: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub italic: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub underline: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub inverse: bool,
}

impl ScreenStyle {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

fn default_screen_cell_width() -> u8 {
    1
}

fn is_one(value: &u8) -> bool {
    *value == 1
}

#[derive(Debug)]
pub enum ScreenSnapshotValidationError {
    InvalidShape(String),
    TooManyCells {
        rows: u16,
        cols: u16,
        cell_count: usize,
        max_cells: usize,
    },
    PayloadTooLarge {
        rows: u16,
        cols: u16,
        cell_count: usize,
        serialized_bytes: usize,
        max_serialized_bytes: usize,
    },
    Serialize(serde_json::Error),
}

impl ScreenSnapshotValidationError {
    pub fn unavailable_reason(&self) -> SnapshotUnavailableReason {
        match self {
            Self::TooManyCells { .. } | Self::PayloadTooLarge { .. } => {
                SnapshotUnavailableReason::PayloadTooLarge
            }
            Self::InvalidShape(_) | Self::Serialize(_) => SnapshotUnavailableReason::InternalError,
        }
    }

    pub fn unavailable_details(&self) -> Option<Value> {
        match self {
            Self::InvalidShape(message) => Some(serde_json::json!({ "message": message })),
            Self::TooManyCells {
                rows,
                cols,
                cell_count,
                max_cells,
            } => Some(serde_json::json!({
                "rows": rows,
                "cols": cols,
                "cell_count": cell_count,
                "max_cells": max_cells
            })),
            Self::PayloadTooLarge {
                rows,
                cols,
                cell_count,
                serialized_bytes,
                max_serialized_bytes,
            } => Some(serde_json::json!({
                "rows": rows,
                "cols": cols,
                "cell_count": cell_count,
                "serialized_bytes": serialized_bytes,
                "max_serialized_bytes": max_serialized_bytes
            })),
            Self::Serialize(error) => Some(serde_json::json!({ "message": error.to_string() })),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorStatus {
    Ok,
    Warning,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoctorIssue {
    pub code: String,
    pub severity: DoctorSeverity,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub repairable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorSeverity {
    Critical,
    Warning,
    Info,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoctorRepair {
    pub mode: DoctorRepairMode,
    pub status: DoctorRepairStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorRepairStatus {
    Applied,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub name: Option<String>,
    pub role: SessionRole,
    #[serde(default)]
    pub spawn_mode: SpawnMode,
    pub process_state: ProcessState,
    pub attention_state: AttentionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_message: Option<String>,
    pub workspace: Option<WorkspaceIdentity>,
    pub cwd: PathBuf,
    pub argv: Vec<String>,
    pub monitor_profile: MonitorProfile,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_requested_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub attached_clients: u32,
    #[serde(default)]
    pub input_owner: Option<String>,
    #[serde(default)]
    pub capabilities: SessionCapabilities,
    #[serde(default)]
    pub artifacts: SessionArtifacts,
    #[serde(default)]
    pub liveness: SessionLiveness,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCapabilities {
    #[serde(default = "default_session_capabilities_schema_version")]
    pub schema_version: u32,
    pub attach: bool,
    pub raw_attach: bool,
    pub send: bool,
    pub resize: bool,
    pub screen: bool,
}

impl Default for SessionCapabilities {
    fn default() -> Self {
        Self::for_spawn_mode(SpawnMode::Pty)
    }
}

impl SessionCapabilities {
    pub fn for_spawn_mode(spawn_mode: SpawnMode) -> Self {
        match spawn_mode {
            SpawnMode::Pty => Self {
                schema_version: SESSION_CAPABILITIES_SCHEMA_VERSION,
                attach: true,
                raw_attach: true,
                send: true,
                resize: true,
                screen: true,
            },
            SpawnMode::Pipe => Self {
                schema_version: SESSION_CAPABILITIES_SCHEMA_VERSION,
                attach: false,
                raw_attach: false,
                send: false,
                resize: false,
                screen: false,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionArtifacts {
    #[serde(default = "default_session_artifacts_schema_version")]
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pty: Option<PtyArtifacts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipe: Option<PipeArtifacts>,
}

impl Default for SessionArtifacts {
    fn default() -> Self {
        Self {
            schema_version: SESSION_ARTIFACTS_SCHEMA_VERSION,
            pty: None,
            pipe: None,
        }
    }
}

impl SessionArtifacts {
    pub fn for_paths(spawn_mode: SpawnMode, paths: &SessionPaths) -> Self {
        match spawn_mode {
            SpawnMode::Pty => Self {
                schema_version: SESSION_ARTIFACTS_SCHEMA_VERSION,
                pty: Some(PtyArtifacts {
                    pty_log: paths.pty_log.clone(),
                    scrollback_snapshot: paths.scrollback_snapshot.clone(),
                    terminal_snapshot: paths.terminal_snapshot.clone(),
                    raw_replay_ring: paths.raw_replay_ring.clone(),
                }),
                pipe: None,
            },
            SpawnMode::Pipe => Self {
                schema_version: SESSION_ARTIFACTS_SCHEMA_VERSION,
                pty: None,
                pipe: Some(PipeArtifacts {
                    stdout_log: paths.stdout_log.clone(),
                    stderr_log: paths.stderr_log.clone(),
                }),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtyArtifacts {
    pub pty_log: PathBuf,
    pub scrollback_snapshot: PathBuf,
    pub terminal_snapshot: PathBuf,
    pub raw_replay_ring: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipeArtifacts {
    pub stdout_log: PathBuf,
    pub stderr_log: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStartResponse {
    pub schema_version: u32,
    pub protocol_version: u32,
    pub session: SessionSummary,
    pub attached_existing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionListResponse {
    pub schema_version: u32,
    pub protocol_version: u32,
    pub sessions: Vec<SessionSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInspectResponse {
    pub schema_version: u32,
    pub protocol_version: u32,
    pub session: SessionSummary,
    pub paths: SessionPaths,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker: Option<WorkerMeta>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAttachResponse {
    pub schema_version: u32,
    pub protocol_version: u32,
    pub session_id: SessionId,
    pub stream: StreamSetup,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub negotiated_attach_protocol_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub negotiated_stream_encoding: Option<AttachStreamEncoding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub negotiated_initial_replay: Option<AttachInitialReplay>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepted_frame_types: Vec<AttachFrameType>,
}

impl SessionAttachResponse {
    pub fn confirms_raw_stream(&self) -> bool {
        self.negotiated_attach_protocol_version == Some(M2_ATTACH_PROTOCOL_VERSION)
            && self.negotiated_stream_encoding == Some(AttachStreamEncoding::RawBytes)
            && self
                .accepted_frame_types
                .contains(&AttachFrameType::RawOutput)
    }

    pub fn confirms_raw_input(&self) -> bool {
        !self.stream.input_owner
            || self
                .accepted_frame_types
                .contains(&AttachFrameType::RawInput)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLogsResponse {
    pub schema_version: u32,
    pub protocol_version: u32,
    pub session_id: SessionId,
    pub lines: Vec<LogLine>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub follow: Option<StreamSetup>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogLine {
    #[serde(default)]
    pub stream: LogStream,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    pub line: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    #[default]
    Pty,
    Stdout,
    Stderr,
}

impl LogStream {
    pub fn as_wire_value(&self) -> &'static str {
        match self {
            Self::Pty => "pty",
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEventsResponse {
    pub schema_version: u32,
    pub protocol_version: u32,
    pub session_id: SessionId,
    pub events: Vec<SessionEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub follow: Option<StreamSetup>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSendResponse {
    pub schema_version: u32,
    pub protocol_version: u32,
    pub session_id: SessionId,
    pub bytes_sent: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResizeResponse {
    pub schema_version: u32,
    pub protocol_version: u32,
    pub session_id: SessionId,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStopResponse {
    pub schema_version: u32,
    pub protocol_version: u32,
    pub session_id: SessionId,
    pub process_state: ProcessState,
    pub stop_requested: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_requested_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionKillResponse {
    pub schema_version: u32,
    pub protocol_version: u32,
    pub session_id: SessionId,
    pub process_state: ProcessState,
    pub kill_requested: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDeleteResponse {
    pub schema_version: u32,
    pub protocol_version: u32,
    pub session_id: SessionId,
    pub deleted: bool,
    pub archived: bool,
    pub purged: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiContextGetResponse {
    pub schema_version: u32,
    pub protocol_version: u32,
    pub context: UiContext,
    pub paths: UiContextPaths,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiContextSetResponse {
    pub schema_version: u32,
    pub protocol_version: u32,
    pub context: UiContext,
    pub paths: UiContextPaths,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiContextListResponse {
    pub schema_version: u32,
    pub protocol_version: u32,
    pub contexts: Vec<UiContextListEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiContextListEntry {
    pub context: UiContext,
    pub paths: UiContextPaths,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiContextCloseResponse {
    pub schema_version: u32,
    pub protocol_version: u32,
    pub ui_id: UiId,
    pub closed: bool,
    pub paths: UiContextPaths,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamSetup {
    pub stream_id: String,
    pub kind: StreamKind,
    pub read_only: bool,
    pub input_owner: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    Attach,
    Logs,
    Events,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachRawBytes(Vec<u8>);

impl AttachRawBytes {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

impl Serialize for AttachRawBytes {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&BASE64_STANDARD.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for AttachRawBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        let bytes = BASE64_STANDARD
            .decode(encoded.as_bytes())
            .map_err(serde::de::Error::custom)?;
        Ok(Self(bytes))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttachStreamFrame {
    Scrollback {
        lines: Vec<String>,
    },
    Output {
        text: String,
    },
    RawOutput {
        data: AttachRawBytes,
    },
    RawInput {
        data: AttachRawBytes,
    },
    ScreenSnapshot {
        #[serde(flatten)]
        snapshot: ScreenSnapshot,
    },
    SnapshotUnavailable {
        reason: SnapshotUnavailableReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
    StreamLagged {
        dropped_bytes: u64,
        dropped_from_offset: u64,
        dropped_to_offset: u64,
        current_pty_log_offset: u64,
        reason: AttachStreamLagReason,
        recover: String,
    },
    Input {
        text: String,
    },
    Resize {
        rows: u16,
        cols: u16,
    },
    Error {
        error: ControlErrorBody,
    },
    Close,
    Closed,
}

impl AttachStreamFrame {
    pub fn raw_output(bytes: impl Into<Vec<u8>>) -> Self {
        Self::RawOutput {
            data: AttachRawBytes::new(bytes),
        }
    }

    pub fn raw_input(bytes: impl Into<Vec<u8>>) -> Self {
        Self::RawInput {
            data: AttachRawBytes::new(bytes),
        }
    }

    pub fn screen_snapshot(
        snapshot: ScreenSnapshot,
    ) -> Result<Self, ScreenSnapshotValidationError> {
        snapshot.validate_for_wire()?;
        let frame = Self::ScreenSnapshot { snapshot };
        let serialized_bytes = serde_json::to_vec(&frame)
            .map_err(ScreenSnapshotValidationError::Serialize)?
            .len();
        if serialized_bytes > MAX_SCREEN_SNAPSHOT_JSON_BYTES {
            let Self::ScreenSnapshot { snapshot } = &frame else {
                unreachable!("frame was just constructed as screen_snapshot");
            };
            return Err(ScreenSnapshotValidationError::PayloadTooLarge {
                rows: snapshot.rows,
                cols: snapshot.cols,
                cell_count: usize::from(snapshot.rows).saturating_mul(usize::from(snapshot.cols)),
                serialized_bytes,
                max_serialized_bytes: MAX_SCREEN_SNAPSHOT_JSON_BYTES,
            });
        }
        Ok(frame)
    }

    pub fn to_json_line(&self) -> serde_json::Result<String> {
        to_json_line(self)
    }

    pub fn from_json_line(line: &str) -> serde_json::Result<Self> {
        serde_json::from_str(line)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LogStreamFrame {
    Line { line: LogLine },
    Closed,
}

impl LogStreamFrame {
    pub fn to_json_line(&self) -> serde_json::Result<String> {
        to_json_line(self)
    }

    pub fn from_json_line(line: &str) -> serde_json::Result<Self> {
        serde_json::from_str(line)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventStreamFrame {
    Event { event: SessionEvent },
    Closed,
}

impl EventStreamFrame {
    pub fn to_json_line(&self) -> serde_json::Result<String> {
        to_json_line(self)
    }

    pub fn from_json_line(line: &str) -> serde_json::Result<Self> {
        serde_json::from_str(line)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerControlMethod {
    Send,
    Resize,
    AcquireAttach,
    ReleaseAttach,
    AttachState,
    ObserveAttach,
    PrepareStopInterrupt,
    ForwardKill,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerControlRequest {
    pub id: String,
    pub method: WorkerControlMethod,
    #[serde(default = "empty_params")]
    pub params: Value,
}

impl WorkerControlRequest {
    pub fn with_params<T>(
        id: impl Into<String>,
        method: WorkerControlMethod,
        params: &T,
    ) -> serde_json::Result<Self>
    where
        T: Serialize,
    {
        Ok(Self {
            id: id.into(),
            method,
            params: serde_json::to_value(params)?,
        })
    }

    pub fn params_as<T>(&self) -> serde_json::Result<T>
    where
        T: DeserializeOwned,
    {
        serde_json::from_value(self.params.clone())
    }

    pub fn to_json_line(&self) -> serde_json::Result<String> {
        to_json_line(self)
    }

    pub fn from_json_line(line: &str) -> serde_json::Result<Self> {
        serde_json::from_str(line)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerControlResponse {
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ControlErrorBody>,
}

impl WorkerControlResponse {
    pub fn success<T>(id: impl Into<String>, result: &T) -> serde_json::Result<Self>
    where
        T: Serialize,
    {
        Ok(Self {
            id: id.into(),
            ok: true,
            result: Some(serde_json::to_value(result)?),
            error: None,
        })
    }

    pub fn failure(id: impl Into<String>, error: ControlErrorBody) -> Self {
        Self {
            id: id.into(),
            ok: false,
            result: None,
            error: Some(error),
        }
    }

    pub fn result_as<T>(&self) -> serde_json::Result<T>
    where
        T: DeserializeOwned,
    {
        serde_json::from_value(self.result.clone().unwrap_or(Value::Null))
    }

    pub fn to_json_line(&self) -> serde_json::Result<String> {
        to_json_line(self)
    }

    pub fn from_json_line(line: &str) -> serde_json::Result<Self> {
        serde_json::from_str(line)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerSendRequest {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerSendResponse {
    pub bytes_sent: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerResizeRequest {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerResizeResponse {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkerAttachRequest {
    pub stream_id: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub read_only: bool,
    #[serde(default)]
    pub replay: AttachReplayMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_terminal_size: Option<TerminalDimensions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_protocol_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepted_frame_types: Vec<AttachFrameType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_encoding: Option<AttachStreamEncoding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_replay: Option<AttachInitialReplay>,
}

impl WorkerAttachRequest {
    pub fn negotiated_attach_protocol_version(&self) -> Option<u32> {
        self.client_protocol_version
            .filter(|version| *version >= M2_ATTACH_PROTOCOL_VERSION)
            .map(|version| version.min(M2_ATTACH_PROTOCOL_VERSION))
    }

    pub fn negotiated_stream_encoding(&self) -> Option<AttachStreamEncoding> {
        self.negotiated_attach_protocol_version().map(|_| {
            match self.stream_encoding.unwrap_or_default() {
                AttachStreamEncoding::RawBytes
                    if self.client_accepts_frame_type(AttachFrameType::RawOutput) =>
                {
                    AttachStreamEncoding::RawBytes
                }
                _ => AttachStreamEncoding::Text,
            }
        })
    }

    pub fn negotiated_initial_replay(&self) -> Option<AttachInitialReplay> {
        self.negotiated_attach_protocol_version().and_then(|_| {
            match self
                .initial_replay
                .unwrap_or_else(|| AttachInitialReplay::from_legacy_replay(self.replay))
            {
                AttachInitialReplay::RawReplay
                    if !self.client_accepts_frame_type(AttachFrameType::RawOutput) =>
                {
                    None
                }
                AttachInitialReplay::ScreenSnapshot
                    if !self.client_accepts_frame_type(AttachFrameType::ScreenSnapshot)
                        || !self
                            .client_accepts_frame_type(AttachFrameType::SnapshotUnavailable) =>
                {
                    None
                }
                replay => Some(replay),
            }
        })
    }

    pub fn negotiated_frame_types(&self) -> Vec<AttachFrameType> {
        let mut frame_types = Vec::new();
        if self.negotiated_attach_protocol_version().is_none() {
            return frame_types;
        }

        if self.negotiated_stream_encoding() == Some(AttachStreamEncoding::RawBytes)
            && self.client_accepts_frame_type(AttachFrameType::RawOutput)
        {
            frame_types.push(AttachFrameType::RawOutput);
        }

        if self.negotiated_stream_encoding() == Some(AttachStreamEncoding::RawBytes)
            && !self.read_only
            && self.client_accepts_frame_type(AttachFrameType::RawInput)
        {
            frame_types.push(AttachFrameType::RawInput);
        }

        if self.client_accepts_frame_type(AttachFrameType::StreamLagged) {
            frame_types.push(AttachFrameType::StreamLagged);
        }

        match self.negotiated_initial_replay() {
            Some(AttachInitialReplay::RawReplay)
                if self.client_accepts_frame_type(AttachFrameType::RawOutput)
                    && !frame_types.contains(&AttachFrameType::RawOutput) =>
            {
                frame_types.push(AttachFrameType::RawOutput);
            }
            Some(AttachInitialReplay::ScreenSnapshot)
                if self.client_accepts_frame_type(AttachFrameType::ScreenSnapshot)
                    && self.client_accepts_frame_type(AttachFrameType::SnapshotUnavailable) =>
            {
                frame_types.push(AttachFrameType::ScreenSnapshot);
                frame_types.push(AttachFrameType::SnapshotUnavailable);
            }
            _ => {}
        }

        frame_types
    }

    pub fn client_accepts_frame_type(&self, frame_type: AttachFrameType) -> bool {
        self.negotiated_attach_protocol_version().is_some()
            && self.accepted_frame_types.contains(&frame_type)
    }
}

impl<'de> Deserialize<'de> for WorkerAttachRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            stream_id: String,
            #[serde(default)]
            read_only: bool,
            #[serde(default)]
            replay: Option<AttachReplayMode>,
            #[serde(default)]
            requested_terminal_size: Option<TerminalDimensions>,
            #[serde(default)]
            include_scrollback: Option<bool>,
            #[serde(default)]
            client_protocol_version: Option<u32>,
            #[serde(default)]
            accepted_frame_types: Vec<AttachFrameType>,
            #[serde(default)]
            stream_encoding: Option<AttachStreamEncoding>,
            #[serde(default)]
            initial_replay: Option<AttachInitialReplay>,
        }

        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            stream_id: wire.stream_id,
            read_only: wire.read_only,
            replay: wire.replay.unwrap_or_else(|| {
                wire.include_scrollback
                    .map(AttachReplayMode::from_legacy_include_scrollback)
                    .unwrap_or_default()
            }),
            requested_terminal_size: wire.requested_terminal_size,
            client_protocol_version: wire.client_protocol_version,
            accepted_frame_types: wire.accepted_frame_types,
            stream_encoding: wire.stream_encoding,
            initial_replay: wire.initial_replay,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerAttachResponse {
    pub stream_id: String,
    pub read_only: bool,
    pub input_owner: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub negotiated_attach_protocol_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub negotiated_stream_encoding: Option<AttachStreamEncoding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub negotiated_initial_replay: Option<AttachInitialReplay>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepted_frame_types: Vec<AttachFrameType>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WorkerAttachStateResponse {
    pub attached_clients: u32,
    pub input_owner: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerReleaseAttachRequest {
    pub stream_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerAckResponse {
    pub accepted: bool,
}

fn to_json_line<T>(value: &T) -> serde_json::Result<String>
where
    T: Serialize,
{
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    Ok(line)
}

fn empty_params() -> Value {
    Value::Object(Default::default())
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn default_session_capabilities_schema_version() -> u32 {
    SESSION_CAPABILITIES_SCHEMA_VERSION
}

fn default_session_artifacts_schema_version() -> u32 {
    SESSION_ARTIFACTS_SCHEMA_VERSION
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    #[test]
    fn protocol_methods_use_exact_m1_wire_names() {
        let cases = [
            (ControlMethod::HostStatus, "host.status"),
            (ControlMethod::HostDoctor, "host.doctor"),
            (ControlMethod::SessionStart, "session.start"),
            (ControlMethod::SessionAttach, "session.attach"),
            (ControlMethod::SessionList, "session.list"),
            (ControlMethod::SessionInspect, "session.inspect"),
            (ControlMethod::SessionScreen, "session.screen"),
            (ControlMethod::SessionLogs, "session.logs"),
            (ControlMethod::SessionEvents, "session.events"),
            (ControlMethod::SessionSend, "session.send"),
            (ControlMethod::SessionResize, "session.resize"),
            (ControlMethod::SessionStop, "session.stop"),
            (ControlMethod::SessionKill, "session.kill"),
            (ControlMethod::SessionDelete, "session.delete"),
            (ControlMethod::UiContextGet, "ui.context.get"),
            (ControlMethod::UiContextSet, "ui.context.set"),
            (ControlMethod::UiContextList, "ui.context.list"),
            (ControlMethod::UiContextClose, "ui.context.close"),
        ];

        for (method, wire_name) in cases {
            assert_eq!(
                serde_json::to_string(&method).unwrap(),
                format!("\"{wire_name}\"")
            );
            assert_eq!(
                serde_json::from_str::<ControlMethod>(&format!("\"{wire_name}\"")).unwrap(),
                method
            );
        }
    }

    #[test]
    fn protocol_error_codes_use_exact_m1_wire_names() {
        let cases = [
            (ControlErrorCode::HostUnavailable, "host_unavailable"),
            (ControlErrorCode::InvalidRequest, "invalid_request"),
            (ControlErrorCode::UnknownMethod, "unknown_method"),
            (ControlErrorCode::SessionNotFound, "session_not_found"),
            (ControlErrorCode::SessionNotRunning, "session_not_running"),
            (
                ControlErrorCode::DuplicateMillraceDaemon,
                "duplicate_millrace_daemon",
            ),
            (ControlErrorCode::WorkspaceNotFound, "workspace_not_found"),
            (
                ControlErrorCode::WorkspaceIdentityConflict,
                "workspace_identity_conflict",
            ),
            (ControlErrorCode::CommandNotFound, "command_not_found"),
            (
                ControlErrorCode::UnsafeDeleteRunning,
                "unsafe_delete_running",
            ),
            (ControlErrorCode::InputOwnerConflict, "input_owner_conflict"),
            (ControlErrorCode::PermissionError, "permission_error"),
            (ControlErrorCode::IoError, "io_error"),
            (ControlErrorCode::WorkerUnavailable, "worker_unavailable"),
            (
                ControlErrorCode::UnsupportedSpawnMode,
                "unsupported_spawn_mode",
            ),
            (ControlErrorCode::MillraceStopFailed, "millrace_stop_failed"),
            (ControlErrorCode::InternalError, "internal_error"),
        ];

        for (code, wire_name) in cases {
            assert_eq!(
                serde_json::to_string(&code).unwrap(),
                format!("\"{wire_name}\"")
            );
            assert_eq!(
                serde_json::from_str::<ControlErrorCode>(&format!("\"{wire_name}\"")).unwrap(),
                code
            );
        }
    }

    #[test]
    fn protocol_request_round_trips_typed_params_as_json_line() {
        let params = SessionStartRequest {
            argv: vec!["echo".to_string(), "hello".to_string()],
            cwd: Some(PathBuf::from("/tmp")),
            workspace: None,
            name: Some("hello".to_string()),
            role: Some(SessionRole::Shell),
            spawn_mode: SpawnMode::Pty,
            session_id: None,
            monitor_profile: MonitorProfile::Auto,
            env: BTreeMap::new(),
        };

        let request =
            ControlRequest::with_params("req_1", ControlMethod::SessionStart, &params).unwrap();
        let line = request.to_json_line().unwrap();
        let decoded = ControlRequest::from_json_line(&line).unwrap();

        assert_eq!(decoded.id, "req_1");
        assert_eq!(decoded.method, ControlMethod::SessionStart);
        assert_eq!(decoded.params_as::<SessionStartRequest>().unwrap(), params);
    }

    #[test]
    fn protocol_response_round_trips_typed_result_as_json_line() {
        let result = SessionListResponse {
            schema_version: M1_PROTOCOL_VERSION,
            protocol_version: M1_PROTOCOL_VERSION,
            sessions: Vec::new(),
        };

        let response = ControlResponse::success("req_2", &result).unwrap();
        let line = response.to_json_line().unwrap();
        let decoded = ControlResponse::from_json_line(&line).unwrap();

        assert!(decoded.ok);
        assert!(decoded.error.is_none());
        assert_eq!(decoded.result_as::<SessionListResponse>().unwrap(), result);
    }

    #[test]
    fn protocol_response_error_shape_is_typed_and_structured() {
        let response = ControlResponse::failure(
            "req_3",
            ControlErrorBody::new(ControlErrorCode::WorkerUnavailable, "worker is unavailable")
                .with_details(json!({"session_id": "session-1"})),
        );

        assert_eq!(
            serde_json::from_str::<Value>(&response.to_json_line().unwrap()).unwrap(),
            json!({
                "id": "req_3",
                "ok": false,
                "error": {
                    "code": "worker_unavailable",
                    "message": "worker is unavailable",
                    "details": {
                        "session_id": "session-1"
                    }
                }
            })
        );
    }
}
