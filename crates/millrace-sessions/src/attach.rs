use std::{
    io::{self, Read, Write},
    os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd},
    thread,
};

use millrace_sessions_core::protocol::{AttachStreamFrame, SessionAttachRequest};
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
    let connection = client.attach(request).await?;
    let (result, mut reader, mut writer) = connection.split();
    let _guard = if result.stream.input_owner {
        TerminalModeGuard::activate()?
    } else {
        TerminalModeGuard::inactive()
    };

    let mut input_rx = spawn_stdin_reader(result.stream.input_owner);

    loop {
        tokio::select! {
            frame = reader.next_frame() => {
                match frame? {
                    Some(AttachStreamFrame::Scrollback { lines }) => write_scrollback(&lines)?,
                    Some(AttachStreamFrame::Output { text }) => write_stdout(text.as_bytes())?,
                    Some(AttachStreamFrame::Error { error }) => return Err(AttachError::Stream(error.message)),
                    Some(AttachStreamFrame::Closed) | None => break,
                    Some(_) => {}
                }
            }
            input = input_rx.recv() => {
                match input {
                    Some(text) => writer.write_frame(&AttachStreamFrame::Input { text }).await?,
                    None => {
                        let _ = writer.write_frame(&AttachStreamFrame::Close).await;
                    }
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

fn spawn_stdin_reader(enabled: bool) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel(16);
    if !enabled || !stdin_is_tty() {
        return rx;
    }

    thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut buffer = [0_u8; 1024];
        loop {
            match stdin.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if tx
                        .blocking_send(String::from_utf8_lossy(&buffer[..count]).to_string())
                        .is_err()
                    {
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
}

impl From<serde_json::Error> for AttachError {
    fn from(error: serde_json::Error) -> Self {
        Self::Stream(error.to_string())
    }
}

#[cfg(test)]
mod tests {
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
}
