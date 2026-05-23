use std::{path::PathBuf, str::FromStr};

use clap::{Args, Parser, Subcommand};
use millrace_sessions_core::{
    ids::SessionId,
    protocol::{
        DoctorRepairMode, DoctorRequest, SessionAttachRequest, SessionDeleteRequest,
        SessionEventsRequest, SessionInspectRequest, SessionKillRequest, SessionListRequest,
        SessionLogsRequest, SessionResizeRequest, SessionSelector, SessionSendRequest,
        SessionStartRequest, SessionStopRequest,
    },
    state::SessionRole,
};
use thiserror::Error;

#[derive(Debug, Parser)]
#[command(name = "millmux", about = "Control local Millrace sessions")]
pub struct Cli {
    #[command(subcommand)]
    pub command: CliCommand,
}

#[derive(Debug, Subcommand)]
pub enum CliCommand {
    Start(StartArgs),
    Attach(AttachArgs),
    List(ListArgs),
    Status(StatusArgs),
    Inspect(InspectArgs),
    Logs(LogsArgs),
    Events(EventsArgs),
    Send(SendArgs),
    Resize(ResizeArgs),
    Stop(StopArgs),
    Kill(KillArgs),
    Delete(DeleteArgs),
    Doctor(DoctorArgs),
}

impl CliCommand {
    pub fn unsupported_name(&self) -> Option<&'static str> {
        match self {
            Self::Start(_)
            | Self::List(_)
            | Self::Status(_)
            | Self::Inspect(_)
            | Self::Attach(_)
            | Self::Logs(_)
            | Self::Events(_)
            | Self::Send(_)
            | Self::Resize(_)
            | Self::Stop(_)
            | Self::Kill(_)
            | Self::Delete(_)
            | Self::Doctor(_) => None,
        }
    }
}

#[derive(Debug, Args)]
pub struct StartArgs {
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long, value_parser = parse_role)]
    pub role: Option<SessionRole>,
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
    #[arg(last = true, required = true, num_args = 1..)]
    pub argv: Vec<String>,
}

impl StartArgs {
    pub fn request(&self) -> Result<SessionStartRequest, CommandError> {
        if self.role == Some(SessionRole::MillraceDaemon) && self.workspace.is_none() {
            return Err(CommandError::MissingMillraceDaemonWorkspace);
        }

        Ok(SessionStartRequest {
            argv: self.argv.clone(),
            cwd: self.cwd.clone(),
            workspace: self.workspace.clone(),
            name: self.name.clone(),
            role: self.role.clone(),
        })
    }
}

#[derive(Debug, Args)]
pub struct AttachArgs {
    #[command(flatten)]
    pub selector: SelectorArgs,
    #[arg(long)]
    pub read_only: bool,
    #[arg(long)]
    pub no_scrollback: bool,
}

impl AttachArgs {
    pub fn request(&self) -> Result<SessionAttachRequest, CommandError> {
        Ok(SessionAttachRequest {
            selector: self.selector.required()?,
            read_only: self.read_only,
            include_scrollback: !self.no_scrollback,
        })
    }
}

#[derive(Debug, Args)]
pub struct ListArgs {
    #[arg(long)]
    pub json: bool,
    #[arg(long, value_parser = parse_role)]
    pub role: Option<SessionRole>,
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    #[arg(long)]
    pub include_archived: bool,
}

impl ListArgs {
    pub fn request(&self) -> SessionListRequest {
        SessionListRequest {
            role: self.role.clone(),
            workspace: self.workspace.clone(),
            include_archived: self.include_archived,
        }
    }
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    #[command(flatten)]
    pub selector: SelectorArgs,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct InspectArgs {
    #[command(flatten)]
    pub selector: SelectorArgs,
    #[arg(long)]
    pub json: bool,
}

impl InspectArgs {
    pub fn request(&self) -> Result<SessionInspectRequest, CommandError> {
        Ok(SessionInspectRequest {
            selector: self.selector.required()?,
        })
    }
}

#[derive(Debug, Args)]
pub struct LogsArgs {
    #[command(flatten)]
    pub selector: SelectorArgs,
    #[arg(long)]
    pub tail: Option<usize>,
    #[arg(long)]
    pub follow: bool,
    #[arg(long)]
    pub json: bool,
}

impl LogsArgs {
    pub fn request(&self) -> Result<SessionLogsRequest, CommandError> {
        Ok(SessionLogsRequest {
            selector: self.selector.required()?,
            tail: self.tail,
            follow: self.follow,
        })
    }
}

#[derive(Debug, Args)]
pub struct EventsArgs {
    #[command(flatten)]
    pub selector: SelectorArgs,
    #[arg(long)]
    pub follow: bool,
    #[arg(long)]
    pub json: bool,
}

impl EventsArgs {
    pub fn request(&self) -> Result<SessionEventsRequest, CommandError> {
        Ok(SessionEventsRequest {
            selector: self.selector.required()?,
            follow: self.follow,
        })
    }
}

#[derive(Debug, Args)]
pub struct SendArgs {
    #[command(flatten)]
    pub selector: SelectorArgs,
    #[arg(long)]
    pub text: String,
    #[arg(long)]
    pub json: bool,
}

impl SendArgs {
    pub fn request(&self) -> Result<SessionSendRequest, CommandError> {
        Ok(SessionSendRequest {
            selector: self.selector.required()?,
            text: self.text.clone(),
        })
    }
}

#[derive(Debug, Args)]
pub struct ResizeArgs {
    #[command(flatten)]
    pub selector: SelectorArgs,
    #[arg(long)]
    pub rows: u16,
    #[arg(long)]
    pub cols: u16,
    #[arg(long)]
    pub json: bool,
}

impl ResizeArgs {
    pub fn request(&self) -> Result<SessionResizeRequest, CommandError> {
        Ok(SessionResizeRequest {
            selector: self.selector.required()?,
            rows: self.rows,
            cols: self.cols,
        })
    }
}

#[derive(Debug, Args)]
pub struct StopArgs {
    #[command(flatten)]
    pub selector: SelectorArgs,
    #[arg(long)]
    pub grace_seconds: Option<u64>,
    #[arg(long)]
    pub json: bool,
}

impl StopArgs {
    pub fn request(&self) -> Result<SessionStopRequest, CommandError> {
        Ok(SessionStopRequest {
            selector: self.selector.required()?,
            grace_seconds: self.grace_seconds,
        })
    }
}

#[derive(Debug, Args)]
pub struct KillArgs {
    #[command(flatten)]
    pub selector: SelectorArgs,
    #[arg(long)]
    pub json: bool,
}

impl KillArgs {
    pub fn request(&self) -> Result<SessionKillRequest, CommandError> {
        Ok(SessionKillRequest {
            selector: self.selector.required()?,
        })
    }
}

#[derive(Debug, Args)]
pub struct DeleteArgs {
    #[command(flatten)]
    pub selector: SelectorArgs,
    #[arg(long)]
    pub purge: bool,
    #[arg(long)]
    pub kill: bool,
    #[arg(long)]
    pub json: bool,
}

impl DeleteArgs {
    pub fn request(&self) -> Result<SessionDeleteRequest, CommandError> {
        Ok(SessionDeleteRequest {
            selector: self.selector.required()?,
            purge: self.purge,
            kill: self.kill,
        })
    }
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub repair: Option<String>,
}

impl DoctorArgs {
    pub fn request(&self) -> Result<DoctorRequest, CommandError> {
        Ok(DoctorRequest {
            repair: self
                .repair
                .as_deref()
                .map(parse_doctor_repair)
                .transpose()?,
        })
    }
}

#[derive(Debug, Clone, Args)]
pub struct SelectorArgs {
    #[arg(value_name = "SESSION", conflicts_with_all = ["workspace", "role"])]
    pub selector: Option<String>,
    #[arg(long, value_name = "PATH", requires = "role")]
    pub workspace: Option<PathBuf>,
    #[arg(long, value_name = "ROLE", requires = "workspace", value_parser = parse_role)]
    pub role: Option<SessionRole>,
}

impl SelectorArgs {
    pub fn optional(&self) -> Result<Option<SessionSelector>, CommandError> {
        match (&self.selector, &self.workspace, &self.role) {
            (Some(value), None, None) => Ok(Some(selector_from_value(value))),
            (None, Some(workspace), Some(role)) => Ok(Some(SessionSelector::WorkspaceRole {
                workspace: workspace.clone(),
                role: role.clone(),
            })),
            (None, None, None) => Ok(None),
            _ => Err(CommandError::InvalidSelector(
                "use either a session id/name or --workspace PATH --role ROLE".to_string(),
            )),
        }
    }

    pub fn required(&self) -> Result<SessionSelector, CommandError> {
        self.optional()?.ok_or(CommandError::MissingSelector)
    }
}

#[derive(Debug, Error)]
pub enum CommandError {
    #[error("missing session selector")]
    MissingSelector,
    #[error("millrace-daemon sessions require --workspace")]
    MissingMillraceDaemonWorkspace,
    #[error("invalid session selector: {0}")]
    InvalidSelector(String),
    #[error("invalid role: {0}")]
    InvalidRole(String),
    #[error("invalid doctor repair mode: {0}")]
    InvalidDoctorRepair(String),
}

fn selector_from_value(value: &str) -> SessionSelector {
    match SessionId::from_str(value) {
        Ok(session_id) => SessionSelector::Id { session_id },
        Err(_) => SessionSelector::Name {
            name: value.to_string(),
        },
    }
}

pub fn parse_role(value: &str) -> Result<SessionRole, CommandError> {
    let normalized = value.trim().to_ascii_lowercase().replace('-', "_");
    if normalized.is_empty() {
        return Err(CommandError::InvalidRole(value.to_string()));
    }

    Ok(match normalized.as_str() {
        "shell" => SessionRole::Shell,
        "millrace_daemon" => SessionRole::MillraceDaemon,
        "agent" => SessionRole::Agent,
        "generic" => SessionRole::Generic,
        "worker" => SessionRole::Worker,
        other => SessionRole::Other(other.to_string()),
    })
}

fn parse_doctor_repair(value: &str) -> Result<DoctorRepairMode, CommandError> {
    match value.trim() {
        "ARCHIVE_STALE" => Ok(DoctorRepairMode::ArchiveStale),
        other => Err(CommandError::InvalidDoctorRepair(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse_list_json_and_filters() {
        let cli = Cli::try_parse_from([
            "millmux",
            "list",
            "--json",
            "--workspace",
            "/tmp/work",
            "--role",
            "millrace-daemon",
        ])
        .unwrap();

        match cli.command {
            CliCommand::List(args) => {
                assert!(args.json);
                assert_eq!(args.workspace, Some(PathBuf::from("/tmp/work")));
                assert_eq!(args.role, Some(SessionRole::MillraceDaemon));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn commands_parse_workspace_role_selector() {
        let cli = Cli::try_parse_from([
            "millmux",
            "inspect",
            "--workspace",
            "/tmp/work",
            "--role",
            "shell",
            "--json",
        ])
        .unwrap();

        match cli.command {
            CliCommand::Inspect(args) => match args.request().unwrap().selector {
                SessionSelector::WorkspaceRole { workspace, role } => {
                    assert_eq!(workspace, PathBuf::from("/tmp/work"));
                    assert_eq!(role, SessionRole::Shell);
                }
                other => panic!("unexpected selector: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn commands_parse_start_argv_after_dash() {
        let cli = Cli::try_parse_from([
            "millmux", "start", "--name", "build", "--role", "worker", "--", "cargo", "test",
        ])
        .unwrap();

        match cli.command {
            CliCommand::Start(args) => {
                assert_eq!(args.name.as_deref(), Some("build"));
                assert_eq!(args.role, Some(SessionRole::Worker));
                assert_eq!(args.argv, ["cargo", "test"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn commands_build_start_request_without_shell_string_execution() {
        let cli = Cli::try_parse_from([
            "millmux",
            "start",
            "--name",
            "build",
            "--role",
            "millrace-daemon",
            "--workspace",
            "/tmp/workspace",
            "--cwd",
            "/tmp/workspace",
            "--",
            "sh",
            "-c",
            "echo ready",
        ])
        .unwrap();

        assert_eq!(cli.command.unsupported_name(), None);
        match cli.command {
            CliCommand::Start(args) => {
                let request = args.request().unwrap();
                assert_eq!(request.argv, ["sh", "-c", "echo ready"]);
                assert_eq!(request.name.as_deref(), Some("build"));
                assert_eq!(request.role, Some(SessionRole::MillraceDaemon));
                assert_eq!(request.workspace, Some(PathBuf::from("/tmp/workspace")));
                assert_eq!(request.cwd, Some(PathBuf::from("/tmp/workspace")));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn commands_classify_future_commands_as_unsupported() {
        let cli = Cli::try_parse_from(["millmux", "send", "shell", "--text", "hello"]).unwrap();

        assert_eq!(cli.command.unsupported_name(), None);
    }

    #[test]
    fn commands_parse_resize_as_unsupported_future_command() {
        let cli =
            Cli::try_parse_from(["millmux", "resize", "shell", "--rows", "24", "--cols", "80"])
                .unwrap();

        assert_eq!(cli.command.unsupported_name(), None);
        match cli.command {
            CliCommand::Resize(args) => {
                assert_eq!(args.rows, 24);
                assert_eq!(args.cols, 80);
                match args.selector.required().unwrap() {
                    SessionSelector::Name { name } => assert_eq!(name, "shell"),
                    other => panic!("unexpected selector: {other:?}"),
                }
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn commands_parse_resize_workspace_role_selector() {
        let cli = Cli::try_parse_from([
            "millmux",
            "resize",
            "--workspace",
            "/tmp/work",
            "--role",
            "worker",
            "--rows",
            "24",
            "--cols",
            "80",
        ])
        .unwrap();

        assert_eq!(cli.command.unsupported_name(), None);
        match cli.command {
            CliCommand::Resize(args) => match args.selector.required().unwrap() {
                SessionSelector::WorkspaceRole { workspace, role } => {
                    assert_eq!(workspace, PathBuf::from("/tmp/work"));
                    assert_eq!(role, SessionRole::Worker);
                }
                other => panic!("unexpected selector: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn commands_build_logs_events_send_attach_requests() {
        let logs =
            Cli::try_parse_from(["millmux", "logs", "shell", "--tail", "20", "--json"]).unwrap();
        match logs.command {
            CliCommand::Logs(args) => {
                assert_eq!(args.request().unwrap().tail, Some(20));
                assert!(args.json);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let events = Cli::try_parse_from(["millmux", "events", "shell", "--follow"]).unwrap();
        match events.command {
            CliCommand::Events(args) => assert!(args.request().unwrap().follow),
            other => panic!("unexpected command: {other:?}"),
        }

        let send = Cli::try_parse_from(["millmux", "send", "shell", "--text", "hello\n"]).unwrap();
        match send.command {
            CliCommand::Send(args) => assert_eq!(args.request().unwrap().text, "hello\n"),
            other => panic!("unexpected command: {other:?}"),
        }

        let attach = Cli::try_parse_from(["millmux", "attach", "shell", "--read-only"]).unwrap();
        match attach.command {
            CliCommand::Attach(args) => {
                let request = args.request().unwrap();
                assert!(request.read_only);
                assert!(request.include_scrollback);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
