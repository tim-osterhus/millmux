use std::path::PathBuf;

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;

use crate::{
    events::SessionEvent,
    ids::SessionId,
    state::{AttentionState, HostMeta, ProcessState, SessionPaths, SessionRole, WorkerMeta},
    workspace::WorkspaceIdentity,
};

pub const M1_PROTOCOL_VERSION: u32 = 1;

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
    pub cwd: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<SessionRole>,
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
pub struct SessionAttachRequest {
    pub selector: SessionSelector,
    #[serde(default, skip_serializing_if = "is_false")]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_scrollback: bool,
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
    pub process_state: ProcessState,
    pub attention_state: AttentionState,
    pub workspace: Option<WorkspaceIdentity>,
    pub cwd: PathBuf,
    pub argv: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    pub attached_clients: u32,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    pub line: String,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttachStreamFrame {
    Scrollback { lines: Vec<String> },
    Output { text: String },
    Input { text: String },
    Resize { rows: u16, cols: u16 },
    Error { error: ControlErrorBody },
    Close,
    Closed,
}

impl AttachStreamFrame {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerAttachRequest {
    pub stream_id: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_scrollback: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerAttachResponse {
    pub stream_id: String,
    pub read_only: bool,
    pub input_owner: bool,
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
            (ControlMethod::SessionLogs, "session.logs"),
            (ControlMethod::SessionEvents, "session.events"),
            (ControlMethod::SessionSend, "session.send"),
            (ControlMethod::SessionResize, "session.resize"),
            (ControlMethod::SessionStop, "session.stop"),
            (ControlMethod::SessionKill, "session.kill"),
            (ControlMethod::SessionDelete, "session.delete"),
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
