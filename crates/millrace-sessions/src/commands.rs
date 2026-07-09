use std::{env, path::PathBuf, str::FromStr};

use clap::{Args, Parser, Subcommand, ValueEnum};
use millrace_sessions_core::{
    ids::{PaneId, SessionId, UiId},
    paths::UI_ID_ENV,
    protocol::{
        AttachFrameType, AttachInitialReplay, AttachReplayMode, AttachStreamEncoding,
        AttentionClearRequest, AttentionListRequest, AttentionMarkRequest, AttentionReadRequest,
        DoctorRepairMode, DoctorRequest, EventSubscribeRequest, InputSendRequest, InputTarget,
        SessionAttachRequest, SessionDeleteRequest, SessionEventsRequest, SessionInspectRequest,
        SessionKillRequest, SessionListRequest, SessionLogsRequest, SessionResizeRequest,
        SessionScreenRequest, SessionSelector, SessionSendRequest, SessionStartRequest,
        SessionStopRequest, UiContextGetRequest, UiContextListRequest, M2_ATTACH_PROTOCOL_VERSION,
    },
    state::{
        AttentionKind, AttentionSeverity, AttentionSource, AttentionTargetType, MonitorProfile,
        SessionRole, SpawnMode,
    },
};
use millrace_sessions_tui::{AgentCockpitLayout, DaemonConsoleLayout};
use thiserror::Error;

use crate::launch_env::current_launch_env;

#[derive(Debug, Parser)]
#[command(name = "millmux", about = "Control local Millrace sessions")]
pub struct Cli {
    #[command(subcommand)]
    pub command: CliCommand,
}

#[derive(Debug, Subcommand)]
pub enum CliCommand {
    Workspace(WorkspaceArgs),
    Session(SessionArgs),
    Agent(RoleCommandArgs),
    Shell(RoleCommandArgs),
    Daemon(RoleCommandArgs),
    Pane(PaneArgs),
    Input(InputArgs),
    Start(StartArgs),
    Attach(AttachArgs),
    List(ListArgs),
    Status(StatusArgs),
    Inspect(InspectArgs),
    Screen(ScreenArgs),
    Logs(LogsArgs),
    Events(EventsArgs),
    #[command(name = "events-subscribe")]
    EventsSubscribe(EventsSubscribeArgs),
    Send(SendArgs),
    Scrollback(ScrollbackArgs),
    Resize(ResizeArgs),
    Stop(StopArgs),
    Kill(KillArgs),
    Delete(DeleteArgs),
    Attention(AttentionArgs),
    Api(ApiArgs),
    Identify(IdentifyArgs),
    Context(ContextArgs),
    Console(ConsoleArgs),
    Cockpit(CockpitArgs),
    Doctor(DoctorArgs),
}

impl CliCommand {
    pub fn unsupported_name(&self) -> Option<&'static str> {
        match self {
            Self::Workspace(_)
            | Self::Session(_)
            | Self::Agent(_)
            | Self::Shell(_)
            | Self::Daemon(_)
            | Self::Pane(_)
            | Self::Input(_)
            | Self::Start(_)
            | Self::List(_)
            | Self::Status(_)
            | Self::Inspect(_)
            | Self::Screen(_)
            | Self::Attach(_)
            | Self::Logs(_)
            | Self::Events(_)
            | Self::EventsSubscribe(_)
            | Self::Send(_)
            | Self::Scrollback(_)
            | Self::Resize(_)
            | Self::Stop(_)
            | Self::Kill(_)
            | Self::Delete(_)
            | Self::Attention(_)
            | Self::Api(_)
            | Self::Identify(_)
            | Self::Context(_)
            | Self::Console(_)
            | Self::Cockpit(_)
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
    #[arg(long, value_parser = parse_monitor_profile, default_value = "auto")]
    pub monitor: MonitorProfile,
    #[arg(long, value_parser = parse_spawn_mode, default_value = "pty")]
    pub spawn_mode: SpawnMode,
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
            session_id: None,
            spawn_mode: self.spawn_mode,
            monitor_profile: self.monitor.clone(),
            env: current_launch_env(),
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
    pub raw: bool,
    #[arg(long)]
    pub no_scrollback: bool,
    #[arg(long, value_enum, conflicts_with = "no_scrollback")]
    pub replay: Option<AttachReplayArg>,
}

impl AttachArgs {
    pub fn request(&self) -> Result<SessionAttachRequest, CommandError> {
        let replay_choice = if self.no_scrollback {
            Some(AttachReplayArg::None)
        } else {
            self.replay
        };
        let uses_v2_attach = self.raw || self.replay.is_some();
        let initial_replay = if uses_v2_attach {
            Some(
                replay_choice
                    .unwrap_or(AttachReplayArg::None)
                    .initial_replay(),
            )
        } else {
            None
        };
        let replay = replay_choice.map_or_else(
            || {
                if self.raw {
                    AttachReplayMode::None
                } else {
                    AttachReplayMode::LineScrollback
                }
            },
            AttachReplayArg::legacy_replay,
        );
        let accepted_frame_types =
            accepted_attach_frame_types(self.raw, self.read_only, initial_replay);

        Ok(SessionAttachRequest {
            selector: self.selector.required()?,
            read_only: self.read_only,
            replay,
            requested_terminal_size: None,
            client_protocol_version: uses_v2_attach.then_some(M2_ATTACH_PROTOCOL_VERSION),
            accepted_frame_types,
            stream_encoding: uses_v2_attach.then_some(if self.raw {
                AttachStreamEncoding::RawBytes
            } else {
                AttachStreamEncoding::Text
            }),
            initial_replay,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AttachReplayArg {
    None,
    Raw,
    Screen,
}

impl AttachReplayArg {
    fn initial_replay(self) -> AttachInitialReplay {
        match self {
            Self::None => AttachInitialReplay::None,
            Self::Raw => AttachInitialReplay::RawReplay,
            Self::Screen => AttachInitialReplay::ScreenSnapshot,
        }
    }

    fn legacy_replay(self) -> AttachReplayMode {
        match self {
            Self::None | Self::Screen => AttachReplayMode::None,
            Self::Raw => AttachReplayMode::RawReplay,
        }
    }
}

fn accepted_attach_frame_types(
    raw: bool,
    read_only: bool,
    initial_replay: Option<AttachInitialReplay>,
) -> Vec<AttachFrameType> {
    let mut frame_types = Vec::new();
    if raw || initial_replay == Some(AttachInitialReplay::RawReplay) {
        frame_types.push(AttachFrameType::RawOutput);
    }
    if raw && !read_only {
        frame_types.push(AttachFrameType::RawInput);
    }
    if raw {
        frame_types.push(AttachFrameType::StreamLagged);
    }
    if raw || initial_replay == Some(AttachInitialReplay::ScreenSnapshot) {
        frame_types.push(AttachFrameType::SnapshotUnavailable);
        frame_types.push(AttachFrameType::ScreenSnapshot);
    }
    frame_types
}

#[derive(Debug, Args)]
pub struct WorkspaceArgs {
    #[command(subcommand)]
    pub command: WorkspaceCommand,
}

#[derive(Debug, Subcommand)]
pub enum WorkspaceCommand {
    Sessions(WorkspaceSessionsArgs),
}

#[derive(Debug, Args)]
pub struct WorkspaceSessionsArgs {
    #[arg(long)]
    pub workspace: PathBuf,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub include_archived: bool,
}

impl WorkspaceSessionsArgs {
    pub fn request(&self) -> SessionListRequest {
        SessionListRequest {
            role: None,
            workspace: Some(self.workspace.clone()),
            include_archived: self.include_archived,
        }
    }
}

#[derive(Debug, Args)]
pub struct SessionArgs {
    #[command(subcommand)]
    pub command: SessionCommand,
}

#[derive(Debug, Subcommand)]
pub enum SessionCommand {
    Start(StartArgs),
    Attach(AttachArgs),
    List(ListArgs),
    Status(StatusArgs),
    Inspect(InspectArgs),
    Screen(ScreenArgs),
    Logs(LogsArgs),
    Events(EventsArgs),
    Send(SendArgs),
    Resize(ResizeArgs),
    Stop(StopArgs),
    Kill(KillArgs),
    Delete(DeleteArgs),
}

#[derive(Debug, Args)]
pub struct RoleCommandArgs {
    #[command(subcommand)]
    pub command: RoleCommand,
}

#[derive(Debug, Subcommand)]
pub enum RoleCommand {
    Start(StartArgs),
    List(RoleListArgs),
}

#[derive(Debug, Args)]
pub struct RoleListArgs {
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    #[arg(long)]
    pub include_archived: bool,
    #[arg(long)]
    pub json: bool,
}

impl RoleListArgs {
    pub fn request(&self, role: SessionRole) -> SessionListRequest {
        SessionListRequest {
            role: Some(role),
            workspace: self.workspace.clone(),
            include_archived: self.include_archived,
        }
    }
}

pub fn request_with_role(
    args: &StartArgs,
    role: SessionRole,
) -> Result<SessionStartRequest, CommandError> {
    let mut request = args.request()?;
    request.role = Some(role);
    Ok(request)
}

#[derive(Debug, Args)]
pub struct PaneArgs {
    #[command(subcommand)]
    pub command: PaneCommand,
}

#[derive(Debug, Subcommand)]
pub enum PaneCommand {
    List(PaneListArgs),
}

#[derive(Debug, Args)]
pub struct PaneListArgs {
    #[arg(long)]
    pub ui: Option<String>,
    #[arg(long)]
    pub json: bool,
}

impl PaneListArgs {
    pub fn request(&self) -> Result<UiContextGetRequest, CommandError> {
        Ok(UiContextGetRequest {
            ui_id: self.ui.as_deref().map(parse_ui_id).transpose()?,
        })
    }
}

#[derive(Debug, Args)]
pub struct InputArgs {
    #[command(subcommand)]
    pub command: InputCommand,
}

#[derive(Debug, Subcommand)]
pub enum InputCommand {
    Send(InputSendArgs),
}

#[derive(Debug, Args)]
pub struct InputSendArgs {
    #[command(flatten)]
    pub selector: SelectorArgs,
    #[arg(long)]
    pub pane: Option<String>,
    #[arg(long)]
    pub ui: Option<String>,
    #[arg(long)]
    pub text: String,
    #[arg(long, default_value_t = true)]
    pub require_focus: bool,
    #[arg(long)]
    pub owner: Option<String>,
    #[arg(long)]
    pub json: bool,
}

impl InputSendArgs {
    pub fn request(&self) -> Result<InputSendRequest, CommandError> {
        if self.selector.selector.is_some() && self.pane.is_some() {
            return Err(CommandError::InvalidInputTarget(
                "use either a session selector or --ui UI --pane PANE".to_string(),
            ));
        }
        let target = if let Some(pane) = &self.pane {
            let ui = self
                .ui
                .as_deref()
                .ok_or_else(|| {
                    CommandError::InvalidInputTarget("pane targets require --ui UI".to_string())
                })
                .and_then(parse_ui_id)?;
            let pane_id = parse_pane_id(pane)?;
            InputTarget::Pane { ui_id: ui, pane_id }
        } else {
            InputTarget::Session {
                selector: self.selector.required()?,
            }
        };
        Ok(InputSendRequest {
            target,
            text: self.text.clone(),
            require_focus: self.require_focus,
            owner: self.owner.clone(),
        })
    }
}

#[derive(Debug, Args)]
pub struct ListArgs {
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub all: bool,
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
pub struct ScreenArgs {
    #[command(flatten)]
    pub selector: SelectorArgs,
    #[arg(long, conflicts_with = "text")]
    pub json: bool,
    #[arg(long)]
    pub text: bool,
}

impl ScreenArgs {
    pub fn request(&self) -> Result<SessionScreenRequest, CommandError> {
        Ok(SessionScreenRequest {
            selector: self.selector.required()?,
            requested_terminal_size: None,
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

#[derive(Debug, Args)]
pub struct EventsSubscribeArgs {
    #[command(flatten)]
    pub selector: SelectorArgs,
    #[arg(long)]
    pub cursor: Option<String>,
    #[arg(long)]
    pub replay_limit: Option<usize>,
    #[arg(long)]
    pub heartbeat_ms: Option<u64>,
    #[arg(long)]
    pub subscriber_queue_limit: Option<usize>,
    #[arg(long)]
    pub json: bool,
}

impl EventsSubscribeArgs {
    pub fn request(&self) -> Result<EventSubscribeRequest, CommandError> {
        Ok(EventSubscribeRequest {
            selector: self.selector.required()?,
            cursor: self.cursor.clone(),
            replay_limit: self.replay_limit,
            heartbeat_ms: self.heartbeat_ms,
            subscriber_queue_limit: self.subscriber_queue_limit,
        })
    }
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
pub struct ScrollbackArgs {
    #[command(subcommand)]
    pub command: ScrollbackCommand,
}

#[derive(Debug, Subcommand)]
pub enum ScrollbackCommand {
    Show(ScrollbackShowArgs),
}

#[derive(Debug, Args)]
pub struct ScrollbackShowArgs {
    #[command(flatten)]
    pub selector: SelectorArgs,
    #[arg(long)]
    pub json: bool,
}

impl ScrollbackShowArgs {
    pub fn request(&self) -> Result<SessionScreenRequest, CommandError> {
        Ok(SessionScreenRequest {
            selector: self.selector.required()?,
            requested_terminal_size: None,
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

#[derive(Debug, Args)]
pub struct ApiArgs {
    #[command(subcommand)]
    pub command: ApiCommand,
}

#[derive(Debug, Subcommand)]
pub enum ApiCommand {
    Capabilities(ApiOutputArgs),
    Identify(ApiOutputArgs),
}

#[derive(Debug, Args)]
pub struct ApiOutputArgs {
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct IdentifyArgs {
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
            reason: None,
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
pub struct AttentionArgs {
    #[command(subcommand)]
    pub command: AttentionCommand,
}

#[derive(Debug, Subcommand)]
pub enum AttentionCommand {
    List(AttentionListArgs),
    Mark(AttentionMarkArgs),
    Read(AttentionReadArgs),
    Clear(AttentionClearArgs),
}

#[derive(Debug, Args)]
pub struct AttentionListArgs {
    #[command(flatten)]
    pub selector: SelectorArgs,
    #[arg(long)]
    pub include_read: bool,
    #[arg(long)]
    pub include_cleared: bool,
    #[arg(long)]
    pub json: bool,
}

impl AttentionListArgs {
    pub fn request(&self) -> Result<AttentionListRequest, CommandError> {
        Ok(AttentionListRequest {
            selector: self.selector.required()?,
            include_read: self.include_read,
            include_cleared: self.include_cleared,
        })
    }
}

#[derive(Debug, Args)]
pub struct AttentionMarkArgs {
    #[command(flatten)]
    pub selector: SelectorArgs,
    #[arg(long, value_parser = parse_attention_target_type, default_value = "session")]
    pub target_type: AttentionTargetType,
    #[arg(long)]
    pub target_id: Option<String>,
    #[arg(long, value_parser = parse_attention_kind)]
    pub kind: AttentionKind,
    #[arg(long, value_parser = parse_attention_severity, default_value = "info")]
    pub severity: AttentionSeverity,
    #[arg(long, value_parser = parse_attention_source, default_value = "cli")]
    pub source: AttentionSource,
    #[arg(long)]
    pub message: String,
    #[arg(long)]
    pub dedupe_key: Option<String>,
    #[arg(long)]
    pub status_label: Option<String>,
    #[arg(long)]
    pub status_detail: Option<String>,
    #[arg(long)]
    pub json: bool,
}

impl AttentionMarkArgs {
    pub fn request(&self) -> Result<AttentionMarkRequest, CommandError> {
        Ok(AttentionMarkRequest {
            selector: self.selector.required()?,
            target_type: self.target_type,
            target_id: self.target_id.clone(),
            kind: self.kind,
            severity: self.severity,
            source: self.source,
            message: self.message.clone(),
            dedupe_key: self.dedupe_key.clone(),
            status_label: self.status_label.clone(),
            status_detail: self.status_detail.clone(),
        })
    }
}

#[derive(Debug, Args)]
pub struct AttentionReadArgs {
    #[command(flatten)]
    pub selector: SelectorArgs,
    #[arg(long)]
    pub item: Option<String>,
    #[arg(long = "kind", value_parser = parse_attention_kind)]
    pub kinds: Vec<AttentionKind>,
    #[arg(long)]
    pub json: bool,
}

impl AttentionReadArgs {
    pub fn request(&self) -> Result<AttentionReadRequest, CommandError> {
        Ok(AttentionReadRequest {
            selector: self.selector.required()?,
            item_id: self.item.clone(),
            kinds: self.kinds.clone(),
        })
    }
}

#[derive(Debug, Args)]
pub struct AttentionClearArgs {
    #[command(flatten)]
    pub selector: SelectorArgs,
    #[arg(long)]
    pub item: Option<String>,
    #[arg(long = "kind", value_parser = parse_attention_kind)]
    pub kinds: Vec<AttentionKind>,
    #[arg(long)]
    pub json: bool,
}

impl AttentionClearArgs {
    pub fn request(&self) -> Result<AttentionClearRequest, CommandError> {
        Ok(AttentionClearRequest {
            selector: self.selector.required()?,
            item_id: self.item.clone(),
            kinds: self.kinds.clone(),
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

#[derive(Debug, Args)]
pub struct ContextArgs {
    #[command(subcommand)]
    pub command: Option<ContextCommand>,
    #[arg(long)]
    pub json: bool,
    #[arg(long, conflicts_with = "list")]
    pub ui: Option<String>,
    #[arg(long)]
    pub list: bool,
}

#[derive(Debug, Subcommand)]
pub enum ContextCommand {
    Export(ContextExportArgs),
}

#[derive(Debug, Args)]
pub struct ContextExportArgs {
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub ui: Option<String>,
}

impl ContextExportArgs {
    pub fn get_request(&self) -> Result<UiContextGetRequest, CommandError> {
        Ok(UiContextGetRequest {
            ui_id: self.ui.as_deref().map(parse_ui_id).transpose()?,
        })
    }
}

impl ContextArgs {
    pub fn get_request(&self) -> Result<UiContextGetRequest, CommandError> {
        Ok(UiContextGetRequest {
            ui_id: self.selected_ui_id()?,
        })
    }

    pub fn list_request(&self) -> UiContextListRequest {
        UiContextListRequest::default()
    }

    fn selected_ui_id(&self) -> Result<Option<UiId>, CommandError> {
        if let Some(value) = &self.ui {
            return parse_ui_id(value).map(Some);
        }

        match env::var(UI_ID_ENV) {
            Ok(value) if !value.trim().is_empty() => parse_ui_id(&value).map(Some),
            _ => Ok(None),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleCommand {
    Status,
    Inspect,
    Logs,
    Events,
    Doctor,
    Stop,
    Kill,
    Delete,
    Archive,
    Purge,
}

impl ConsoleCommand {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Inspect => "inspect",
            Self::Logs => "logs",
            Self::Events => "events",
            Self::Doctor => "doctor",
            Self::Stop => "stop",
            Self::Kill => "kill",
            Self::Delete => "delete",
            Self::Archive => "archive",
            Self::Purge => "purge",
        }
    }

    pub fn is_destructive(self) -> bool {
        matches!(
            self,
            Self::Stop | Self::Kill | Self::Delete | Self::Archive | Self::Purge
        )
    }
}

#[derive(Debug, Args)]
pub struct ConsoleArgs {
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    #[arg(long, value_parser = parse_role, default_value = "millrace-daemon")]
    pub role: SessionRole,
    #[arg(long, value_parser = parse_monitor_profile)]
    pub monitor: Option<MonitorProfile>,
    #[arg(long, value_parser = parse_console_layout)]
    pub layout: Option<DaemonConsoleLayout>,
    #[arg(long)]
    pub no_start: bool,
    #[arg(long)]
    pub ui: Option<String>,
    #[arg(long)]
    pub once: bool,
    #[arg(long, value_parser = parse_console_command)]
    pub command: Option<ConsoleCommand>,
    #[arg(long)]
    pub confirm: Option<String>,
}

#[derive(Debug, Args)]
pub struct CockpitArgs {
    #[arg(long)]
    pub workspace: PathBuf,
    #[arg(long, default_value = "millracer")]
    pub agent: String,
    #[arg(long = "agent-argv")]
    pub agent_argv: bool,
    #[arg(last = true)]
    pub argv: Vec<String>,
    #[arg(long, value_parser = parse_monitor_profile)]
    pub monitor: Option<MonitorProfile>,
    #[arg(long, value_parser = parse_cockpit_layout)]
    pub layout: Option<AgentCockpitLayout>,
    #[arg(long)]
    pub no_start: bool,
    #[arg(long)]
    pub ui: Option<String>,
    #[arg(long)]
    pub once: bool,
}

impl CockpitArgs {
    pub fn resolved_agent_argv(&self) -> Vec<String> {
        if !self.argv.is_empty() {
            return self.argv.clone();
        }
        vec![self.agent.clone()]
    }

    pub fn requested_monitor_profile(&self) -> MonitorProfile {
        self.monitor.clone().unwrap_or_default()
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
    #[error("invalid input target: {0}")]
    InvalidInputTarget(String),
    #[error("invalid pane id: {0}")]
    InvalidPaneId(String),
    #[error("invalid role: {0}")]
    InvalidRole(String),
    #[error("invalid doctor repair mode: {0}")]
    InvalidDoctorRepair(String),
    #[error("invalid UI id: {0}")]
    InvalidUiId(String),
    #[error("invalid monitor profile: {0}")]
    InvalidMonitorProfile(String),
    #[error("invalid spawn mode: {0}")]
    InvalidSpawnMode(String),
    #[error("invalid daemon console layout: {0}")]
    InvalidConsoleLayout(String),
    #[error("invalid agent cockpit layout: {0}")]
    InvalidCockpitLayout(String),
    #[error("invalid daemon console command: {0}")]
    InvalidConsoleCommand(String),
    #[error("invalid attention target type: {0}")]
    InvalidAttentionTargetType(String),
    #[error("invalid attention kind: {0}")]
    InvalidAttentionKind(String),
    #[error("invalid attention severity: {0}")]
    InvalidAttentionSeverity(String),
    #[error("invalid attention source: {0}")]
    InvalidAttentionSource(String),
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
    SessionRole::from_cli_value(value).map_err(|_| CommandError::InvalidRole(value.to_string()))
}

fn parse_doctor_repair(value: &str) -> Result<DoctorRepairMode, CommandError> {
    match value.trim() {
        "ARCHIVE_STALE" => Ok(DoctorRepairMode::ArchiveStale),
        "CLOSE_STALE_UI_CONTEXTS" => Ok(DoctorRepairMode::CloseStaleUiContexts),
        other => Err(CommandError::InvalidDoctorRepair(other.to_string())),
    }
}

fn parse_ui_id(value: &str) -> Result<UiId, CommandError> {
    value
        .trim()
        .parse()
        .map_err(|_| CommandError::InvalidUiId(value.to_string()))
}

fn parse_pane_id(value: &str) -> Result<PaneId, CommandError> {
    value
        .trim()
        .parse()
        .map_err(|_| CommandError::InvalidPaneId(value.to_string()))
}

fn parse_monitor_profile(value: &str) -> Result<MonitorProfile, CommandError> {
    value
        .parse()
        .map_err(|_| CommandError::InvalidMonitorProfile(value.to_string()))
}

fn parse_spawn_mode(value: &str) -> Result<SpawnMode, CommandError> {
    value
        .parse()
        .map_err(|_| CommandError::InvalidSpawnMode(value.to_string()))
}

fn parse_console_layout(value: &str) -> Result<DaemonConsoleLayout, CommandError> {
    value
        .parse()
        .map_err(|_| CommandError::InvalidConsoleLayout(value.to_string()))
}

fn parse_cockpit_layout(value: &str) -> Result<AgentCockpitLayout, CommandError> {
    value
        .parse()
        .map_err(|_| CommandError::InvalidCockpitLayout(value.to_string()))
}

fn parse_console_command(value: &str) -> Result<ConsoleCommand, CommandError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "status" => Ok(ConsoleCommand::Status),
        "inspect" => Ok(ConsoleCommand::Inspect),
        "logs" => Ok(ConsoleCommand::Logs),
        "events" => Ok(ConsoleCommand::Events),
        "doctor" => Ok(ConsoleCommand::Doctor),
        "stop" => Ok(ConsoleCommand::Stop),
        "kill" => Ok(ConsoleCommand::Kill),
        "delete" => Ok(ConsoleCommand::Delete),
        "archive" => Ok(ConsoleCommand::Archive),
        "purge" => Ok(ConsoleCommand::Purge),
        other => Err(CommandError::InvalidConsoleCommand(other.to_string())),
    }
}

fn parse_attention_target_type(value: &str) -> Result<AttentionTargetType, CommandError> {
    value
        .parse()
        .map_err(|_| CommandError::InvalidAttentionTargetType(value.to_string()))
}

fn parse_attention_kind(value: &str) -> Result<AttentionKind, CommandError> {
    value
        .parse()
        .map_err(|_| CommandError::InvalidAttentionKind(value.to_string()))
}

fn parse_attention_severity(value: &str) -> Result<AttentionSeverity, CommandError> {
    value
        .parse()
        .map_err(|_| CommandError::InvalidAttentionSeverity(value.to_string()))
}

fn parse_attention_source(value: &str) -> Result<AttentionSource, CommandError> {
    value
        .parse()
        .map_err(|_| CommandError::InvalidAttentionSource(value.to_string()))
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
                assert!(!args.all);
                assert_eq!(args.workspace, Some(PathBuf::from("/tmp/work")));
                assert_eq!(args.role, Some(SessionRole::MillraceDaemon));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn commands_parse_list_all() {
        let cli = Cli::try_parse_from(["millmux", "list", "--all"]).unwrap();

        match cli.command {
            CliCommand::List(args) => assert!(args.all),
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
            "millmux", "start", "--name", "build", "--role", "generic", "--", "cargo", "test",
        ])
        .unwrap();

        match cli.command {
            CliCommand::Start(args) => {
                assert_eq!(args.name.as_deref(), Some("build"));
                assert_eq!(args.role, Some(SessionRole::Generic));
                assert_eq!(args.argv, ["cargo", "test"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn commands_parse_pipe_spawn_mode_for_start() {
        let cli = Cli::try_parse_from([
            "millmux",
            "start",
            "--spawn-mode",
            "pipe",
            "--",
            "sh",
            "-c",
            "echo ready",
        ])
        .unwrap();

        match cli.command {
            CliCommand::Start(args) => {
                let request = args.request().unwrap();
                assert_eq!(request.spawn_mode, SpawnMode::Pipe);
                assert_eq!(request.argv, ["sh", "-c", "echo ready"]);
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
                assert_eq!(request.spawn_mode, SpawnMode::Pty);
                assert_eq!(request.name.as_deref(), Some("build"));
                assert_eq!(request.role, Some(SessionRole::MillraceDaemon));
                assert_eq!(request.workspace, Some(PathBuf::from("/tmp/workspace")));
                assert_eq!(request.cwd, Some(PathBuf::from("/tmp/workspace")));
                if let Ok(path) = env::var("PATH") {
                    assert_eq!(
                        request.env.get("PATH").map(String::as_str),
                        Some(path.as_str())
                    );
                }
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
            "generic",
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
                    assert_eq!(role, SessionRole::Generic);
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
                assert_eq!(request.replay, AttachReplayMode::LineScrollback);
                assert_eq!(request.client_protocol_version, None);
                assert_eq!(request.stream_encoding, None);
                assert_eq!(request.initial_replay, None);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn commands_build_raw_attach_request_with_independent_replay_axis() {
        let attach = Cli::try_parse_from([
            "millmux",
            "attach",
            "shell",
            "--read-only",
            "--raw",
            "--replay",
            "none",
        ])
        .unwrap();
        match attach.command {
            CliCommand::Attach(args) => {
                let request = args.request().unwrap();
                assert!(request.read_only);
                assert_eq!(request.replay, AttachReplayMode::None);
                assert_eq!(
                    request.client_protocol_version,
                    Some(M2_ATTACH_PROTOCOL_VERSION)
                );
                assert_eq!(
                    request.stream_encoding,
                    Some(AttachStreamEncoding::RawBytes)
                );
                assert_eq!(request.initial_replay, Some(AttachInitialReplay::None));
                assert_eq!(
                    request.accepted_frame_types,
                    vec![
                        AttachFrameType::RawOutput,
                        AttachFrameType::StreamLagged,
                        AttachFrameType::SnapshotUnavailable,
                        AttachFrameType::ScreenSnapshot,
                    ]
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn commands_build_attach_replay_modes_as_v2_initial_replay() {
        for (wire, initial_replay, legacy_replay, accepted_frame_types) in [
            (
                "none",
                AttachInitialReplay::None,
                AttachReplayMode::None,
                Vec::new(),
            ),
            (
                "raw",
                AttachInitialReplay::RawReplay,
                AttachReplayMode::RawReplay,
                vec![AttachFrameType::RawOutput],
            ),
            (
                "screen",
                AttachInitialReplay::ScreenSnapshot,
                AttachReplayMode::None,
                vec![
                    AttachFrameType::SnapshotUnavailable,
                    AttachFrameType::ScreenSnapshot,
                ],
            ),
        ] {
            let attach =
                Cli::try_parse_from(["millmux", "attach", "shell", "--replay", wire]).unwrap();
            match attach.command {
                CliCommand::Attach(args) => {
                    let request = args.request().unwrap();
                    assert_eq!(request.replay, legacy_replay);
                    assert_eq!(
                        request.client_protocol_version,
                        Some(M2_ATTACH_PROTOCOL_VERSION)
                    );
                    assert_eq!(request.stream_encoding, Some(AttachStreamEncoding::Text));
                    assert_eq!(request.initial_replay, Some(initial_replay));
                    assert_eq!(request.accepted_frame_types, accepted_frame_types);
                }
                other => panic!("unexpected command: {other:?}"),
            }
        }
    }

    #[test]
    fn commands_build_raw_attach_defaults_to_no_initial_replay() {
        let attach = Cli::try_parse_from(["millmux", "attach", "shell", "--raw"]).unwrap();
        match attach.command {
            CliCommand::Attach(args) => {
                let request = args.request().unwrap();
                assert_eq!(request.replay, AttachReplayMode::None);
                assert_eq!(
                    request.stream_encoding,
                    Some(AttachStreamEncoding::RawBytes)
                );
                assert_eq!(request.initial_replay, Some(AttachInitialReplay::None));
                assert_eq!(
                    request.accepted_frame_types,
                    vec![
                        AttachFrameType::RawOutput,
                        AttachFrameType::RawInput,
                        AttachFrameType::StreamLagged,
                        AttachFrameType::SnapshotUnavailable,
                        AttachFrameType::ScreenSnapshot,
                    ]
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn commands_parse_doctor_stale_ui_context_repair() {
        let cli = Cli::try_parse_from([
            "millmux",
            "doctor",
            "--repair",
            "CLOSE_STALE_UI_CONTEXTS",
            "--json",
        ])
        .unwrap();

        match cli.command {
            CliCommand::Doctor(args) => {
                assert!(args.json);
                assert_eq!(
                    args.request().unwrap().repair,
                    Some(DoctorRepairMode::CloseStaleUiContexts)
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn commands_parse_context_modes() {
        let ui_id = UiId::new().to_string();
        let cli = Cli::try_parse_from(["millmux", "context", "--ui", &ui_id, "--json"]).unwrap();

        match cli.command {
            CliCommand::Context(args) => {
                assert!(args.json);
                assert_eq!(
                    args.get_request().unwrap().ui_id.unwrap().to_string(),
                    ui_id
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::try_parse_from(["millmux", "context", "--list", "--json"]).unwrap();
        match cli.command {
            CliCommand::Context(args) => assert!(args.list),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn commands_parse_agent_cockpit_args() {
        let cli = Cli::try_parse_from([
            "millmux",
            "cockpit",
            "--workspace",
            "/tmp/work",
            "--agent",
            "codex",
            "--layout",
            "wide",
            "--once",
            "--agent-argv",
            "--",
            "codex",
            "exec",
            "--dangerously-bypass-approvals-and-sandbox",
        ])
        .unwrap();

        match cli.command {
            CliCommand::Cockpit(args) => {
                assert_eq!(args.workspace, PathBuf::from("/tmp/work"));
                assert_eq!(args.agent, "codex");
                assert_eq!(
                    args.resolved_agent_argv(),
                    [
                        "codex",
                        "exec",
                        "--dangerously-bypass-approvals-and-sandbox"
                    ]
                );
                assert_eq!(args.layout, Some(AgentCockpitLayout::Wide));
                assert!(args.once);
                assert_eq!(args.monitor, None);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn commands_parse_monitor_profiles_for_start_and_tui_modes() {
        let start = Cli::try_parse_from([
            "millmux",
            "start",
            "--role",
            "millrace-daemon",
            "--workspace",
            "/tmp/work",
            "--monitor",
            "jsonl",
            "--",
            "millrace",
            "run",
            "daemon",
        ])
        .unwrap();
        match start.command {
            CliCommand::Start(args) => {
                assert_eq!(
                    args.request().unwrap().monitor_profile,
                    MonitorProfile::Jsonl
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let console = Cli::try_parse_from([
            "millmux",
            "console",
            "--workspace",
            "/tmp/work",
            "--monitor",
            "other:semantic",
        ])
        .unwrap();
        match console.command {
            CliCommand::Console(args) => {
                assert_eq!(
                    args.monitor,
                    Some(MonitorProfile::Other("semantic".to_string()))
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
