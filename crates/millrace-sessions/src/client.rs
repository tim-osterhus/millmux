use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use millrace_sessions_core::{
    error::MillmuxError,
    paths::{state_paths, StatePaths, STATE_DIR_ENV},
    protocol::{
        AttachStreamFrame, ControlErrorBody, ControlMethod, ControlRequest, ControlResponse,
        DoctorRequest, DoctorResponse, EventStreamFrame, HostStatusRequest, HostStatusResponse,
        LogStreamFrame, SessionAttachRequest, SessionAttachResponse, SessionDeleteRequest,
        SessionDeleteResponse, SessionEventsRequest, SessionEventsResponse, SessionInspectRequest,
        SessionInspectResponse, SessionKillRequest, SessionKillResponse, SessionListRequest,
        SessionListResponse, SessionLogsRequest, SessionLogsResponse, SessionResizeRequest,
        SessionResizeResponse, SessionSendRequest, SessionSendResponse, SessionStartRequest,
        SessionStartResponse, SessionStopRequest, SessionStopResponse,
    },
};
use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{unix::OwnedReadHalf, unix::OwnedWriteHalf, UnixStream},
    time::{sleep, Instant},
};

pub const HOST_BIN_ENV: &str = "MILLMUX_HOST_BIN";
const HOST_BIN_NAME: &str = "millrace-sessiond";
const HOST_READY_TIMEOUT: Duration = Duration::from_secs(3);
const HOST_READY_POLL: Duration = Duration::from_millis(50);

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct SessionControlClient {
    paths: StatePaths,
}

impl SessionControlClient {
    pub fn new() -> Result<Self, ClientError> {
        Ok(Self {
            paths: state_paths()?,
        })
    }

    #[cfg(test)]
    fn with_paths(paths: StatePaths) -> Self {
        Self { paths }
    }

    pub async fn ensure_host_ready(&self) -> Result<(), ClientError> {
        if self.probe_host_status().await.is_ok() {
            return Ok(());
        }

        self.spawn_host_detached()?;
        let deadline = Instant::now() + HOST_READY_TIMEOUT;
        let mut last_error = None;

        while Instant::now() < deadline {
            match self.probe_host_status().await {
                Ok(_) => return Ok(()),
                Err(error) => {
                    last_error = Some(error);
                    sleep(HOST_READY_POLL).await;
                }
            }
        }

        Err(ClientError::HostUnavailable {
            socket: self.paths.control_sock.clone(),
            source: last_error.map(Box::new),
        })
    }

    pub async fn host_status(&self) -> Result<HostStatusResponse, ClientError> {
        self.request(ControlMethod::HostStatus, &HostStatusRequest::default())
            .await
    }

    pub async fn doctor(&self, request: &DoctorRequest) -> Result<DoctorResponse, ClientError> {
        self.request(ControlMethod::HostDoctor, request).await
    }

    pub async fn list(
        &self,
        request: &SessionListRequest,
    ) -> Result<SessionListResponse, ClientError> {
        self.request(ControlMethod::SessionList, request).await
    }

    pub async fn start(
        &self,
        request: &SessionStartRequest,
    ) -> Result<SessionStartResponse, ClientError> {
        self.request(ControlMethod::SessionStart, request).await
    }

    pub async fn inspect(
        &self,
        request: &SessionInspectRequest,
    ) -> Result<SessionInspectResponse, ClientError> {
        self.request(ControlMethod::SessionInspect, request).await
    }

    pub async fn logs(
        &self,
        request: &SessionLogsRequest,
    ) -> Result<SessionLogsResponse, ClientError> {
        self.request(ControlMethod::SessionLogs, request).await
    }

    pub async fn events(
        &self,
        request: &SessionEventsRequest,
    ) -> Result<SessionEventsResponse, ClientError> {
        self.request(ControlMethod::SessionEvents, request).await
    }

    pub async fn logs_follow(
        &self,
        request: &SessionLogsRequest,
    ) -> Result<LogsConnection, ClientError> {
        let mut request = request.clone();
        request.follow = true;
        let (result, reader) = self
            .open_response_stream(
                ControlMethod::SessionLogs,
                &request,
                "host closed logs stream without a response",
            )
            .await?;
        Ok(LogsConnection { result, reader })
    }

    pub async fn events_follow(
        &self,
        request: &SessionEventsRequest,
    ) -> Result<EventsConnection, ClientError> {
        let mut request = request.clone();
        request.follow = true;
        let (result, reader) = self
            .open_response_stream(
                ControlMethod::SessionEvents,
                &request,
                "host closed events stream without a response",
            )
            .await?;
        Ok(EventsConnection { result, reader })
    }

    pub async fn send(
        &self,
        request: &SessionSendRequest,
    ) -> Result<SessionSendResponse, ClientError> {
        self.request(ControlMethod::SessionSend, request).await
    }

    pub async fn resize(
        &self,
        request: &SessionResizeRequest,
    ) -> Result<SessionResizeResponse, ClientError> {
        self.request(ControlMethod::SessionResize, request).await
    }

    pub async fn stop(
        &self,
        request: &SessionStopRequest,
    ) -> Result<SessionStopResponse, ClientError> {
        self.request(ControlMethod::SessionStop, request).await
    }

    pub async fn kill(
        &self,
        request: &SessionKillRequest,
    ) -> Result<SessionKillResponse, ClientError> {
        self.request(ControlMethod::SessionKill, request).await
    }

    pub async fn delete(
        &self,
        request: &SessionDeleteRequest,
    ) -> Result<SessionDeleteResponse, ClientError> {
        self.request(ControlMethod::SessionDelete, request).await
    }

    pub async fn attach(
        &self,
        request: &SessionAttachRequest,
    ) -> Result<AttachConnection, ClientError> {
        let id = next_request_id();
        let request =
            ControlRequest::with_params(id.clone(), ControlMethod::SessionAttach, request)?;
        let stream = UnixStream::connect(&self.paths.control_sock).await?;
        let (reader, mut writer) = stream.into_split();
        writer.write_all(request.to_json_line()?.as_bytes()).await?;
        writer.flush().await?;

        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        let bytes = reader.read_line(&mut line).await?;
        if bytes == 0 {
            return Err(ClientError::Protocol(
                "host closed attach stream without a response".to_string(),
            ));
        }
        let response = ControlResponse::from_json_line(&line)?;
        let result = response_result::<SessionAttachResponse>(response, &id)?;
        let reader = reader.into_inner();
        Ok(AttachConnection {
            result,
            reader: BufReader::new(reader),
            writer,
        })
    }

    async fn probe_host_status(&self) -> Result<HostStatusResponse, ClientError> {
        self.host_status().await
    }

    async fn request<P, R>(&self, method: ControlMethod, params: &P) -> Result<R, ClientError>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let id = next_request_id();
        let request = ControlRequest::with_params(id.clone(), method, params)?;
        let response = self.exchange(&request).await?;
        response_result(response, &id)
    }

    async fn open_response_stream<P, R>(
        &self,
        method: ControlMethod,
        params: &P,
        empty_response_message: &'static str,
    ) -> Result<(R, BufReader<OwnedReadHalf>), ClientError>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let id = next_request_id();
        let request = ControlRequest::with_params(id.clone(), method, params)?;
        let stream = UnixStream::connect(&self.paths.control_sock).await?;
        let (reader, mut writer) = stream.into_split();
        writer.write_all(request.to_json_line()?.as_bytes()).await?;
        writer.flush().await?;

        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        let bytes = reader.read_line(&mut line).await?;
        if bytes == 0 {
            return Err(ClientError::Protocol(empty_response_message.to_string()));
        }

        let response = ControlResponse::from_json_line(&line)?;
        let result = response_result::<R>(response, &id)?;
        Ok((result, reader))
    }

    async fn exchange(&self, request: &ControlRequest) -> Result<ControlResponse, ClientError> {
        let stream = UnixStream::connect(&self.paths.control_sock).await?;
        let (reader, mut writer) = stream.into_split();
        writer.write_all(request.to_json_line()?.as_bytes()).await?;
        writer.flush().await?;

        let mut line = String::new();
        let mut reader = BufReader::new(reader);
        let bytes = reader.read_line(&mut line).await?;
        if bytes == 0 {
            return Err(ClientError::Protocol(
                "host closed socket without a response".to_string(),
            ));
        }

        Ok(ControlResponse::from_json_line(&line)?)
    }

    fn spawn_host_detached(&self) -> Result<(), ClientError> {
        let host_bin = resolve_host_binary()?;
        let mut command = Command::new(host_bin);
        command
            .arg("--foreground")
            .env(STATE_DIR_ENV, &self.paths.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        detach_command(&mut command);
        let _child = command.spawn()?;
        Ok(())
    }
}

pub struct AttachConnection {
    pub result: SessionAttachResponse,
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl AttachConnection {
    pub fn split(self) -> (SessionAttachResponse, AttachReader, AttachWriter) {
        (
            self.result,
            AttachReader {
                reader: self.reader,
            },
            AttachWriter {
                writer: self.writer,
            },
        )
    }
}

pub struct LogsConnection {
    pub result: SessionLogsResponse,
    reader: BufReader<OwnedReadHalf>,
}

impl LogsConnection {
    pub fn split(self) -> (SessionLogsResponse, LogReader) {
        (
            self.result,
            LogReader {
                reader: self.reader,
            },
        )
    }
}

pub struct LogReader {
    reader: BufReader<OwnedReadHalf>,
}

impl LogReader {
    pub async fn next_frame(&mut self) -> Result<Option<LogStreamFrame>, ClientError> {
        let mut line = String::new();
        let bytes = self.reader.read_line(&mut line).await?;
        if bytes == 0 {
            return Ok(None);
        }
        Ok(Some(LogStreamFrame::from_json_line(&line)?))
    }
}

pub struct EventsConnection {
    pub result: SessionEventsResponse,
    reader: BufReader<OwnedReadHalf>,
}

impl EventsConnection {
    pub fn split(self) -> (SessionEventsResponse, EventReader) {
        (
            self.result,
            EventReader {
                reader: self.reader,
            },
        )
    }
}

pub struct EventReader {
    reader: BufReader<OwnedReadHalf>,
}

impl EventReader {
    pub async fn next_frame(&mut self) -> Result<Option<EventStreamFrame>, ClientError> {
        let mut line = String::new();
        let bytes = self.reader.read_line(&mut line).await?;
        if bytes == 0 {
            return Ok(None);
        }
        Ok(Some(EventStreamFrame::from_json_line(&line)?))
    }
}

pub struct AttachReader {
    reader: BufReader<OwnedReadHalf>,
}

impl AttachReader {
    pub async fn next_frame(&mut self) -> Result<Option<AttachStreamFrame>, ClientError> {
        let mut line = String::new();
        let bytes = self.reader.read_line(&mut line).await?;
        if bytes == 0 {
            return Ok(None);
        }
        Ok(Some(AttachStreamFrame::from_json_line(&line)?))
    }
}

pub struct AttachWriter {
    writer: OwnedWriteHalf,
}

impl AttachWriter {
    pub async fn write_frame(&mut self, frame: &AttachStreamFrame) -> Result<(), ClientError> {
        self.writer
            .write_all(frame.to_json_line()?.as_bytes())
            .await?;
        self.writer.flush().await?;
        Ok(())
    }
}

fn response_result<R>(response: ControlResponse, request_id: &str) -> Result<R, ClientError>
where
    R: DeserializeOwned,
{
    if response.id != request_id {
        return Err(ClientError::Protocol(format!(
            "response id {} did not match request id {request_id}",
            response.id
        )));
    }

    if !response.ok {
        return Err(ClientError::Control(response.error.unwrap_or_else(|| {
            ControlErrorBody::new(
                millrace_sessions_core::protocol::ControlErrorCode::InternalError,
                "host returned an error without an error body",
            )
        })));
    }

    let result = response
        .result
        .ok_or_else(|| ClientError::Protocol("successful response omitted result".to_string()))?;
    Ok(serde_json::from_value(result)?)
}

fn next_request_id() -> String {
    let counter = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("millmux_{}_{}", std::process::id(), counter)
}

fn resolve_host_binary() -> Result<PathBuf, ClientError> {
    if let Some(path) = env::var_os(HOST_BIN_ENV).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }

    if let Ok(current_exe) = env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            let sibling = parent.join(HOST_BIN_NAME);
            if is_executable(&sibling) {
                return Ok(sibling);
            }
        }
    }

    if let Some(path) = find_on_path(HOST_BIN_NAME) {
        return Ok(path);
    }

    Err(ClientError::HostBinaryNotFound)
}

fn find_on_path(binary_name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(binary_name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(unix)]
fn detach_command(command: &mut Command) {
    use std::{io, os::unix::process::CommandExt};

    unsafe {
        command.pre_exec(|| {
            nix::unistd::setsid()
                .map(|_| ())
                .map_err(|errno| io::Error::from_raw_os_error(errno as i32))
        });
    }
}

#[cfg(not(unix))]
fn detach_command(_command: &mut Command) {}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("host is unavailable at {socket}")]
    HostUnavailable {
        socket: PathBuf,
        #[source]
        source: Option<Box<ClientError>>,
    },
    #[error("could not locate millrace-sessiond; set {HOST_BIN_ENV}")]
    HostBinaryNotFound,
    #[error("session control error: {0:?}")]
    Control(millrace_sessions_core::protocol::ControlErrorBody),
    #[error("session control protocol error: {0}")]
    Protocol(String),
    #[error(transparent)]
    Core(#[from] MillmuxError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use millrace_sessions_core::{
        paths::StatePaths,
        protocol::{ControlErrorBody, ControlErrorCode},
    };
    use serde_json::{json, Value};

    use super::*;

    #[test]
    fn client_decodes_raw_success_result() {
        let response = ControlResponse::success(
            "req",
            &json!({
                "schema_version": 1,
                "protocol_version": 1,
                "sessions": []
            }),
        )
        .unwrap();

        let result: SessionListResponse = response_result(response, "req").unwrap();

        assert!(result.sessions.is_empty());
    }

    #[test]
    fn client_rejects_mismatched_response_id() {
        let response = ControlResponse::empty_success("other");

        let error = response_result::<Value>(response, "req").unwrap_err();

        assert!(matches!(error, ClientError::Protocol(_)));
    }

    #[test]
    fn client_maps_error_response() {
        let response = ControlResponse::failure(
            "req",
            ControlErrorBody::new(ControlErrorCode::SessionNotFound, "missing"),
        );

        let error = response_result::<Value>(response, "req").unwrap_err();

        assert!(
            matches!(error, ClientError::Control(body) if body.code == ControlErrorCode::SessionNotFound)
        );
    }

    #[test]
    fn client_constructs_with_explicit_paths_for_tests() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StatePaths::new(temp.path().join("state"));
        let client = SessionControlClient::with_paths(paths.clone());

        assert_eq!(client.paths.control_sock, paths.control_sock);
    }
}
