mod attach;
mod client;
mod cockpit;
mod commands;
mod console;
mod launch_env;
mod output;

use std::{
    io::{self, Write},
    time::Duration,
};

use clap::Parser;
use commands::{Cli, CliCommand};
use millrace_sessions_core::ids::SessionId;
use millrace_sessions_core::paths::state_paths;
use millrace_sessions_core::protocol::{
    EventStreamFrame, LogStreamFrame, SessionInspectRequest, SessionInspectResponse,
    SessionListRequest, SessionListResponse, SessionSelector, SessionSummary,
};
use millrace_sessions_core::state::ProcessState;
use thiserror::Error;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        if error.is_broken_pipe() {
            return;
        }
        eprintln!("millmux: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), MillmuxCliError> {
    let cli = Cli::parse();

    if let Some(command) = cli.command.unsupported_name() {
        return Err(MillmuxCliError::Unsupported(command));
    }

    match cli.command {
        CliCommand::Doctor(args) => {
            let request = args.request()?;
            let result = match client::SessionControlClient::new() {
                Ok(client) => {
                    match tokio::time::timeout(Duration::from_millis(200), client.doctor(&request))
                        .await
                    {
                        Ok(Ok(result)) => result,
                        _ => {
                            let paths = state_paths()?;
                            millrace_sessions_host::doctor::run_doctor(&paths, None, &request)?
                        }
                    }
                }
                Err(_) => {
                    let paths = state_paths()?;
                    millrace_sessions_host::doctor::run_doctor(&paths, None, &request)?
                }
            };
            write_stdout(if args.json {
                output::render_json(&result)?
            } else {
                output::render_doctor(&result)
            })?;
            return Ok(());
        }
        CliCommand::Start(args) => {
            let client = ready_client().await?;
            let request = args.request()?;
            let result = client.start(&request).await?;
            write_stdout(if args.json {
                output::render_json(&result)?
            } else {
                output::render_start(&result)
            })?;
        }
        CliCommand::List(args) => {
            let client = ready_client().await?;
            let mut result = client.list(&args.request()).await?;
            if !args.all {
                retain_active_sessions(&mut result);
            }
            write_stdout(if args.json {
                output::render_json(&result)?
            } else {
                output::render_list(&result)
            })?;
        }
        CliCommand::Status(args) => {
            let client = ready_client().await?;
            if let Some(selector) = args.selector.optional()? {
                let result = inspect_preferred_status_session(&client, selector).await?;
                write_stdout(if args.json {
                    output::render_json(&result)?
                } else {
                    output::render_session_status(&result)
                })?;
            } else {
                let result = client.host_status().await?;
                write_stdout(if args.json {
                    output::render_json(&result)?
                } else {
                    output::render_host_status(&result)
                })?;
            }
        }
        CliCommand::Inspect(args) => {
            let client = ready_client().await?;
            let result = client.inspect(&args.request()?).await?;
            write_stdout(if args.json {
                output::render_json(&result)?
            } else {
                output::render_inspect(&result)
            })?;
        }
        CliCommand::Logs(args) => {
            let client = ready_client().await?;
            let request = args.request()?;
            if args.follow {
                let (result, mut reader) = client.logs_follow(&request).await?.split();
                write_stdout(if args.json {
                    output::render_json(&result)?
                } else {
                    output::render_logs(&result)
                })?;
                while let Some(frame) = reader.next_frame().await? {
                    let closed = matches!(frame, LogStreamFrame::Closed);
                    write_stdout(if args.json {
                        output::render_json(&frame)?
                    } else {
                        output::render_log_stream_frame(&frame)
                    })?;
                    if closed {
                        break;
                    }
                }
            } else {
                let result = client.logs(&request).await?;
                write_stdout(if args.json {
                    output::render_json(&result)?
                } else {
                    output::render_logs(&result)
                })?;
            }
        }
        CliCommand::Events(args) => {
            let client = ready_client().await?;
            let request = args.request()?;
            if args.follow {
                let (result, mut reader) = client.events_follow(&request).await?.split();
                write_stdout(if args.json {
                    output::render_json(&result)?
                } else {
                    output::render_events(&result)
                })?;
                while let Some(frame) = reader.next_frame().await? {
                    let closed = matches!(frame, EventStreamFrame::Closed);
                    write_stdout(if args.json {
                        output::render_json(&frame)?
                    } else {
                        output::render_event_stream_frame(&frame)
                    })?;
                    if closed {
                        break;
                    }
                }
            } else {
                let result = client.events(&request).await?;
                write_stdout(if args.json {
                    output::render_json(&result)?
                } else {
                    output::render_events(&result)
                })?;
            }
        }
        CliCommand::Send(args) => {
            let client = ready_client().await?;
            let result = client.send(&args.request()?).await?;
            write_stdout(if args.json {
                output::render_json(&result)?
            } else {
                output::render_send(&result)
            })?;
        }
        CliCommand::Resize(args) => {
            let client = ready_client().await?;
            let result = client.resize(&args.request()?).await?;
            write_stdout(if args.json {
                output::render_json(&result)?
            } else {
                output::render_resize(&result)
            })?;
        }
        CliCommand::Stop(args) => {
            let client = ready_client().await?;
            let result = client.stop(&args.request()?).await?;
            write_stdout(if args.json {
                output::render_json(&result)?
            } else {
                output::render_stop(&result)
            })?;
        }
        CliCommand::Kill(args) => {
            let client = ready_client().await?;
            let result = client.kill(&args.request()?).await?;
            write_stdout(if args.json {
                output::render_json(&result)?
            } else {
                output::render_kill(&result)
            })?;
        }
        CliCommand::Delete(args) => {
            let client = ready_client().await?;
            let result = client.delete(&args.request()?).await?;
            write_stdout(if args.json {
                output::render_json(&result)?
            } else {
                output::render_delete(&result)
            })?;
        }
        CliCommand::Context(args) => {
            let client = ready_client().await?;
            if args.list {
                let result = client.ui_context_list(&args.list_request()).await?;
                write_stdout(if args.json {
                    output::render_json(&result)?
                } else {
                    output::render_context_list(&result)
                })?;
            } else {
                let result = client.ui_context_get(&args.get_request()?).await?;
                write_stdout(if args.json {
                    output::render_json(&result)?
                } else {
                    output::render_context(&result)
                })?;
            }
        }
        CliCommand::Console(args) => {
            console::run_console(args).await?;
        }
        CliCommand::Cockpit(args) => {
            cockpit::run_cockpit(args).await?;
        }
        CliCommand::Attach(args) => {
            let client = ready_client().await?;
            attach::run_attach(&client, &args.request()?).await?;
        }
    }

    Ok(())
}

async fn inspect_preferred_status_session(
    client: &client::SessionControlClient,
    selector: SessionSelector,
) -> Result<SessionInspectResponse, MillmuxCliError> {
    let selector = match &selector {
        SessionSelector::WorkspaceRole { workspace, role } => {
            let list = client
                .list(&SessionListRequest {
                    role: Some(role.clone()),
                    workspace: Some(workspace.clone()),
                    include_archived: false,
                })
                .await?;
            preferred_session_id(&list.sessions)
                .map(|session_id| SessionSelector::Id { session_id })
                .unwrap_or(selector)
        }
        _ => selector,
    };

    Ok(client.inspect(&SessionInspectRequest { selector }).await?)
}

fn preferred_session_id(sessions: &[SessionSummary]) -> Option<SessionId> {
    sessions
        .iter()
        .max_by_key(|session| {
            (
                is_active_process_state(&session.process_state),
                session.updated_at.as_str(),
                session.session_id,
            )
        })
        .map(|session| session.session_id)
}

fn is_active_process_state(state: &ProcessState) -> bool {
    matches!(state, ProcessState::Starting | ProcessState::Running)
}

fn retain_active_sessions(result: &mut SessionListResponse) {
    result
        .sessions
        .retain(|session| is_active_process_state(&session.process_state));
}

async fn ready_client() -> Result<client::SessionControlClient, MillmuxCliError> {
    let client = client::SessionControlClient::new()?;
    client.ensure_host_ready().await?;
    Ok(client)
}

fn write_stdout(output: String) -> Result<(), io::Error> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(output.as_bytes())?;
    stdout.flush()
}

#[derive(Debug, Error)]
enum MillmuxCliError {
    #[error("command `{0}` is unsupported in this CLI slice")]
    Unsupported(&'static str),
    #[error(transparent)]
    Commands(#[from] commands::CommandError),
    #[error(transparent)]
    Core(#[from] millrace_sessions_core::error::MillmuxError),
    #[error(transparent)]
    Doctor(#[from] millrace_sessions_host::doctor::DoctorError),
    #[error(transparent)]
    Client(#[from] client::ClientError),
    #[error(transparent)]
    Output(#[from] output::OutputError),
    #[error(transparent)]
    Attach(#[from] attach::AttachError),
    #[error(transparent)]
    Console(#[from] console::ConsoleError),
    #[error(transparent)]
    Cockpit(#[from] cockpit::CockpitError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl MillmuxCliError {
    fn is_broken_pipe(&self) -> bool {
        match self {
            Self::Io(error) => error.kind() == io::ErrorKind::BrokenPipe,
            Self::Attach(attach::AttachError::Io(error)) => {
                error.kind() == io::ErrorKind::BrokenPipe
            }
            Self::Console(console::ConsoleError::Io(error)) => {
                error.kind() == io::ErrorKind::BrokenPipe
            }
            Self::Cockpit(cockpit::CockpitError::Io(error)) => {
                error.kind() == io::ErrorKind::BrokenPipe
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, str::FromStr};

    use millrace_sessions_core::{
        ids::SessionId,
        protocol::{
            SessionArtifacts, SessionCapabilities, SessionListResponse, M1_PROTOCOL_VERSION,
        },
        state::{AttentionState, MonitorProfile, SessionRole, SpawnMode},
    };

    use super::*;

    #[test]
    fn preferred_session_id_prefers_running_session_over_newer_terminal_session() {
        let running = SessionId::from_str("ffffffff-ffff-ffff-ffff-ffffffffffff").unwrap();
        let exited = SessionId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        let sessions = vec![
            summary(exited, ProcessState::Exited, "2026-05-20T18:10:00Z"),
            summary(running, ProcessState::Running, "2026-05-20T18:00:00Z"),
        ];

        assert_eq!(preferred_session_id(&sessions), Some(running));
    }

    #[test]
    fn retain_active_sessions_hides_terminal_records_from_default_list() {
        let running = SessionId::from_str("ffffffff-ffff-ffff-ffff-ffffffffffff").unwrap();
        let exited = SessionId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        let mut result = SessionListResponse {
            schema_version: M1_PROTOCOL_VERSION,
            protocol_version: M1_PROTOCOL_VERSION,
            sessions: vec![
                summary(exited, ProcessState::Exited, "2026-05-20T18:10:00Z"),
                summary(running, ProcessState::Running, "2026-05-20T18:00:00Z"),
            ],
        };

        retain_active_sessions(&mut result);

        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].session_id, running);
    }

    fn summary(
        session_id: SessionId,
        process_state: ProcessState,
        updated_at: &str,
    ) -> SessionSummary {
        SessionSummary {
            session_id,
            name: Some("daemon:millrace".to_string()),
            role: SessionRole::MillraceDaemon,
            spawn_mode: SpawnMode::Pty,
            process_state,
            attention_state: AttentionState::Active,
            failure_message: None,
            workspace: None,
            cwd: PathBuf::from("/tmp"),
            argv: vec![
                "millrace".to_string(),
                "run".to_string(),
                "daemon".to_string(),
            ],
            monitor_profile: MonitorProfile::Basic,
            created_at: "2026-05-20T18:00:00Z".to_string(),
            updated_at: updated_at.to_string(),
            stop_requested_at: None,
            stop_reason: None,
            attached_clients: 0,
            input_owner: None,
            capabilities: SessionCapabilities::for_spawn_mode(SpawnMode::Pty),
            artifacts: SessionArtifacts::default(),
            liveness: Default::default(),
        }
    }
}
