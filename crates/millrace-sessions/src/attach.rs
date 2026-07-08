use std::{
    io::{self, Read, Write},
    os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd},
    thread,
};

use crossterm::terminal;
use millrace_sessions_core::protocol::{
    AttachStreamFrame, SessionAttachRequest, SessionAttachResponse, TerminalDimensions,
};
use nix::{
    sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg, Termios},
    unistd::isatty,
};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::client::{ClientError, SessionControlClient};

pub async fn run_attach(
    client: &SessionControlClient,
    request: &SessionAttachRequest,
) -> Result<(), AttachError> {
    let request = request_with_terminal_size(request);
    let raw_requested = request.requests_raw_stream();
    let connection = client.attach(&request).await?;
    let (result, mut reader, mut writer) = connection.split();
    validate_attach_negotiation(&request, &result)?;
    let _guard = if result.stream.input_owner {
        TerminalModeGuard::activate()?
    } else {
        TerminalModeGuard::inactive()
    };

    let input_enabled = result.stream.input_owner;
    let mut input_rx = spawn_stdin_reader(input_enabled);
    let mut resize_rx = spawn_resize_watcher(raw_requested);

    loop {
        tokio::select! {
            frame = reader.next_frame() => {
                match frame? {
                    Some(AttachStreamFrame::Scrollback { lines }) => write_scrollback(&lines)?,
                    Some(AttachStreamFrame::Output { text }) => write_stdout(text.as_bytes())?,
                    Some(AttachStreamFrame::RawOutput { data }) => write_stdout(data.as_slice())?,
                    Some(AttachStreamFrame::Error { error }) => return Err(AttachError::Stream(error.message)),
                    Some(AttachStreamFrame::Closed) | None => break,
                    Some(_) => {}
                }
            }
            input = input_rx.recv(), if input_enabled => {
                match input {
                    Some(bytes) => {
                        let frame = if raw_requested {
                            AttachStreamFrame::raw_input(bytes)
                        } else {
                            AttachStreamFrame::Input {
                                text: String::from_utf8_lossy(&bytes).to_string(),
                            }
                        };
                        writer.write_frame(&frame).await?;
                    }
                    None => {
                        let _ = writer.write_frame(&AttachStreamFrame::Close).await;
                    }
                }
            }
            resize = resize_rx.recv(), if raw_requested => {
                if let Some(size) = resize {
                    writer.write_frame(&AttachStreamFrame::Resize {
                        rows: size.rows,
                        cols: size.cols,
                    }).await?;
                }
            }
            signal = tokio::signal::ctrl_c() => {
                signal?;
                let _ = writer.write_frame(&AttachStreamFrame::Close).await;
                break;
            }
        }
    }

    Ok(())
}

fn validate_attach_negotiation(
    request: &SessionAttachRequest,
    response: &SessionAttachResponse,
) -> Result<(), AttachError> {
    if !request.requests_raw_stream()
        || (response.confirms_raw_stream() && response.confirms_raw_input())
    {
        return Ok(());
    }

    Err(AttachError::Compatibility(format!(
        "raw attach requires host-confirmed v2 raw-byte negotiation; got attach_protocol={:?}, stream_encoding={:?}, accepted_frame_types={:?}, input_owner={}",
        response.negotiated_attach_protocol_version,
        response.negotiated_stream_encoding,
        response.accepted_frame_types,
        response.stream.input_owner
    )))
}

fn request_with_terminal_size(request: &SessionAttachRequest) -> SessionAttachRequest {
    let mut request = request.clone();
    if request.requests_raw_stream() && request.requested_terminal_size.is_none() {
        request.requested_terminal_size = current_terminal_dimensions();
    }
    request
}

fn current_terminal_dimensions() -> Option<TerminalDimensions> {
    terminal::size()
        .ok()
        .map(|(cols, rows)| TerminalDimensions::new(rows, cols))
}

fn spawn_stdin_reader(enabled: bool) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel(16);
    if !enabled || !stdin_is_tty() {
        return rx;
    }

    thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut buffer = [0_u8; 512];
        loop {
            match stdin.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if tx.blocking_send(buffer[..count].to_vec()).is_err() {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
    rx
}

fn spawn_resize_watcher(enabled: bool) -> mpsc::Receiver<TerminalDimensions> {
    let (tx, rx) = mpsc::channel(8);
    if !enabled {
        return rx;
    }

    tokio::spawn(async move {
        let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
        else {
            return;
        };
        while signal.recv().await.is_some() {
            let Some(size) = current_terminal_dimensions() else {
                continue;
            };
            if tx.send(size).await.is_err() {
                break;
            }
        }
    });

    rx
}

fn write_scrollback(lines: &[String]) -> Result<(), AttachError> {
    if lines.is_empty() {
        return Ok(());
    }
    let mut stdout = io::stdout();
    for line in lines {
        stdout.write_all(line.as_bytes())?;
        stdout.write_all(b"\n")?;
    }
    stdout.flush()?;
    Ok(())
}

fn write_stdout(bytes: &[u8]) -> Result<(), AttachError> {
    let mut stdout = io::stdout();
    stdout.write_all(bytes)?;
    stdout.flush()?;
    Ok(())
}

fn stdin_is_tty() -> bool {
    isatty(io::stdin().as_raw_fd()).unwrap_or(false)
}

pub struct TerminalModeGuard {
    fd: Option<RawFd>,
    original: Option<Termios>,
}

impl TerminalModeGuard {
    pub fn activate() -> Result<Self, AttachError> {
        let stdin = io::stdin();
        if !isatty(stdin.as_raw_fd()).map_err(terminal_error)? {
            return Ok(Self::inactive());
        }

        let original = tcgetattr(stdin.as_fd()).map_err(terminal_error)?;
        let mut raw = original.clone();
        cfmakeraw(&mut raw);
        tcsetattr(stdin.as_fd(), SetArg::TCSANOW, &raw).map_err(terminal_error)?;

        Ok(Self {
            fd: Some(stdin.as_raw_fd()),
            original: Some(original),
        })
    }

    pub fn inactive() -> Self {
        Self {
            fd: None,
            original: None,
        }
    }

    #[cfg(test)]
    pub fn is_active(&self) -> bool {
        self.fd.is_some()
    }
}

impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        let (Some(fd), Some(original)) = (self.fd, self.original.as_ref()) else {
            return;
        };
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let _ = tcsetattr(borrowed, SetArg::TCSANOW, original);
    }
}

fn terminal_error(error: nix::errno::Errno) -> AttachError {
    AttachError::Terminal(error.to_string())
}

#[derive(Debug, Error)]
pub enum AttachError {
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("terminal error: {0}")]
    Terminal(String),
    #[error("attach stream error: {0}")]
    Stream(String),
    #[error("attach compatibility error: {0}")]
    Compatibility(String),
}

impl From<serde_json::Error> for AttachError {
    fn from(error: serde_json::Error) -> Self {
        Self::Stream(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use millrace_sessions_core::{
        ids::SessionId,
        protocol::{
            AttachFrameType, AttachInitialReplay, AttachStreamEncoding, StreamKind, StreamSetup,
            M1_PROTOCOL_VERSION, M2_ATTACH_PROTOCOL_VERSION,
        },
    };

    use super::*;

    #[test]
    fn attach_terminal_guard_inactive_is_noop() {
        let guard = TerminalModeGuard::inactive();
        assert!(!guard.is_active());
    }

    #[test]
    fn attach_stream_frame_round_trips_output() {
        let frame = AttachStreamFrame::Output {
            text: "ready\n".to_string(),
        };
        let line = frame.to_json_line().unwrap();
        assert_eq!(AttachStreamFrame::from_json_line(&line).unwrap(), frame);
    }

    #[test]
    fn raw_attach_negotiation_accepts_confirmed_v2_raw_bytes() {
        let request = raw_attach_request();
        let response = raw_attach_response(
            Some(M2_ATTACH_PROTOCOL_VERSION),
            Some(AttachStreamEncoding::RawBytes),
            vec![AttachFrameType::RawOutput],
        );

        validate_attach_negotiation(&request, &response).unwrap();
    }

    #[test]
    fn raw_attach_negotiation_requires_raw_input_for_writable_stream() {
        let mut request = raw_attach_request();
        request.read_only = false;
        request.accepted_frame_types.push(AttachFrameType::RawInput);
        let mut response = raw_attach_response(
            Some(M2_ATTACH_PROTOCOL_VERSION),
            Some(AttachStreamEncoding::RawBytes),
            vec![AttachFrameType::RawOutput],
        );
        response.stream.read_only = false;
        response.stream.input_owner = true;

        let error = validate_attach_negotiation(&request, &response).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("raw attach requires host-confirmed v2 raw-byte negotiation"),
            "{error}"
        );

        response
            .accepted_frame_types
            .push(AttachFrameType::RawInput);
        validate_attach_negotiation(&request, &response).unwrap();
    }

    #[test]
    fn raw_attach_negotiation_fails_closed_without_v2_raw_bytes() {
        let request = raw_attach_request();
        for response in [
            raw_attach_response(None, None, Vec::new()),
            raw_attach_response(
                Some(M1_PROTOCOL_VERSION),
                Some(AttachStreamEncoding::RawBytes),
                vec![AttachFrameType::RawOutput],
            ),
            raw_attach_response(
                Some(M2_ATTACH_PROTOCOL_VERSION),
                Some(AttachStreamEncoding::Text),
                vec![AttachFrameType::RawOutput],
            ),
            raw_attach_response(
                Some(M2_ATTACH_PROTOCOL_VERSION),
                Some(AttachStreamEncoding::RawBytes),
                Vec::new(),
            ),
        ] {
            let error = validate_attach_negotiation(&request, &response).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("raw attach requires host-confirmed v2 raw-byte negotiation"),
                "{error}"
            );
        }
    }

    fn raw_attach_request() -> SessionAttachRequest {
        SessionAttachRequest {
            selector: millrace_sessions_core::protocol::SessionSelector::Name {
                name: "shell".to_string(),
            },
            read_only: true,
            replay: millrace_sessions_core::protocol::AttachReplayMode::None,
            requested_terminal_size: None,
            client_protocol_version: Some(M2_ATTACH_PROTOCOL_VERSION),
            accepted_frame_types: vec![AttachFrameType::RawOutput],
            stream_encoding: Some(AttachStreamEncoding::RawBytes),
            initial_replay: Some(AttachInitialReplay::None),
        }
    }

    fn raw_attach_response(
        negotiated_attach_protocol_version: Option<u32>,
        negotiated_stream_encoding: Option<AttachStreamEncoding>,
        accepted_frame_types: Vec<AttachFrameType>,
    ) -> SessionAttachResponse {
        SessionAttachResponse {
            schema_version: M1_PROTOCOL_VERSION,
            protocol_version: M1_PROTOCOL_VERSION,
            session_id: SessionId::new(),
            stream: StreamSetup {
                stream_id: "attach-test".to_string(),
                kind: StreamKind::Attach,
                read_only: true,
                input_owner: false,
            },
            negotiated_attach_protocol_version,
            negotiated_stream_encoding,
            negotiated_initial_replay: Some(AttachInitialReplay::None),
            accepted_frame_types,
        }
    }
}
