use std::{
    collections::BTreeMap,
    env,
    io::{self, IsTerminal, Stdout, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crossterm::{
    cursor::MoveTo,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use millrace_sessions_core::{
    error::MillmuxError,
    events::current_timestamp,
    ids::{SessionId, UiId},
    paths::{state_paths, CONTROL_SOCK_ENV, STATE_DIR_ENV, UI_ID_ENV},
    protocol::{
        AttachFrameType, AttachInitialReplay, AttachReplayMode, AttachStreamEncoding,
        AttachStreamFrame, ControlErrorCode, SessionAttachRequest, SessionListRequest,
        SessionLogsRequest, SessionSelector, SessionStartRequest, TerminalDimensions,
        UiContextSetRequest, M2_ATTACH_PROTOCOL_VERSION,
    },
    state::{MonitorProfile, ProcessState, SessionRole, SpawnMode, UiEvent, UiEventKind},
};
use millrace_sessions_tui::{
    renderer::{render_app, render_to_string},
    AgentCockpitLayout, AgentTerminalPane, AppModel, KeyAction, TerminalEmulator,
};
use ratatui::{backend::CrosstermBackend, Terminal};
use thiserror::Error;

use crate::{
    client::{AttachConnection, ClientError, SessionControlClient},
    commands::{CockpitArgs, CommandError},
    launch_env::{current_launch_env, merge_current_launch_env, resolve_argv_executable},
    output::render_log_line_text,
};

const LOG_TAIL: usize = 4000;
const SNAPSHOT_WIDTH: u16 = 120;
const SNAPSHOT_HEIGHT: u16 = 28;
const REFRESH_INTERVAL: Duration = Duration::from_millis(300);
const REDRAW_INTERVAL: Duration = Duration::from_millis(33);
const ATTACH_POLL_INTERVAL: Duration = Duration::from_millis(5);
const ATTACH_DRAIN_INTERVAL: Duration = Duration::from_millis(1);
const SNAPSHOT_SEED_TIMEOUT: Duration = Duration::from_millis(3_000);
const SNAPSHOT_SEED_FRAME_WAIT: Duration = Duration::from_millis(1_000);
const SNAPSHOT_SEED_OUTPUT_QUIET: Duration = Duration::from_millis(75);
const SNAPSHOT_SEED_RETRY_INTERVAL: Duration = Duration::from_millis(25);
const TERMINAL_SCROLLBACK: usize = 4000;
const CONTEXT_FILE_ENV: &str = "MILLMUX_CONTEXT_FILE";
const AGENT_SESSION_ID_ENV: &str = "MILLMUX_AGENT_SESSION_ID";
const MILLRACE_WORKSPACE_ENV: &str = "MILLRACE_WORKSPACE";

pub async fn run_cockpit(args: CockpitArgs) -> Result<(), CockpitError> {
    let client = SessionControlClient::new()?;
    client.ensure_host_ready().await?;
    let ui_id = parse_or_new_ui_id(args.ui.as_deref())?;
    let mut app = build_cockpit_app(&client, &args, ui_id).await?;

    record_ui_event(
        &client,
        &app,
        UiEventKind::UiStarted,
        "agent cockpit started",
        BTreeMap::new(),
    )
    .await?;
    record_ui_event(
        &client,
        &app,
        UiEventKind::AgentSessionBound,
        "agent session bound",
        bound_fields("agent_session_id", app.agent_session_id),
    )
    .await?;
    record_ui_event(
        &client,
        &app,
        UiEventKind::DaemonSessionBound,
        "daemon session bound",
        bound_fields("daemon_session_id", app.active_daemon_session_id),
    )
    .await?;

    if args.once || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        seed_agent_terminal_from_attach(&client, &mut app).await?;
        refresh_daemon_sessions(&client, &mut app).await?;
        refresh_logs(&client, &mut app).await?;
        record_ui_event(
            &client,
            &app,
            UiEventKind::UiDetached,
            "agent cockpit snapshot detached",
            BTreeMap::new(),
        )
        .await?;
        write_snapshot(&render_to_string(&app, SNAPSHOT_WIDTH, SNAPSHOT_HEIGHT))?;
        return Ok(());
    }

    run_interactive_cockpit(client, app).await
}

async fn build_cockpit_app(
    client: &SessionControlClient,
    args: &CockpitArgs,
    ui_id: UiId,
) -> Result<AppModel, CockpitError> {
    let workspace = args.workspace.canonicalize()?;
    let requested_monitor = args.requested_monitor_profile();
    let mut daemons = discover_daemons(client).await?;
    let has_active_workspace_daemon = daemons.iter().any(|session| {
        workspace_matches(session, &workspace) && is_active_state(&session.process_state)
    });
    if !has_active_workspace_daemon && !args.no_start {
        daemons.push(start_workspace_daemon(client, &workspace, &requested_monitor).await?);
    }
    if !daemons
        .iter()
        .any(|session| workspace_matches(session, &workspace))
    {
        return Err(CockpitError::NoDaemonFound);
    }
    if daemons.is_empty() {
        return Err(CockpitError::NoDaemonFound);
    }
    retain_terminal_workspace_daemons_only_when_no_active_daemon(&mut daemons, &workspace);
    daemons.sort_by(|left, right| {
        left.workspace
            .as_ref()
            .map(|workspace| workspace.canonical_path.clone())
            .cmp(
                &right
                    .workspace
                    .as_ref()
                    .map(|workspace| workspace.canonical_path.clone()),
            )
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    let selected_daemon = daemons
        .iter()
        .find(|session| {
            workspace_matches(session, &workspace) && is_active_state(&session.process_state)
        })
        .or_else(|| {
            daemons
                .iter()
                .find(|session| workspace_matches(session, &workspace))
        })
        .or_else(|| {
            daemons
                .iter()
                .find(|session| is_active_state(&session.process_state))
        })
        .or_else(|| daemons.first())
        .map(|session| session.session_id);
    let selected_session = selected_daemon.and_then(|session_id| {
        daemons
            .iter()
            .find(|session| session.session_id == session_id)
    });
    let monitor_profile = args.monitor.clone().unwrap_or_else(|| {
        selected_session.map_or(MonitorProfile::Auto, |session| {
            session.monitor_profile.clone()
        })
    });

    let agent = ensure_agent_session(client, args, ui_id, selected_daemon, &workspace).await?;
    let mut logs = BTreeMap::new();
    for daemon in &daemons {
        logs.insert(
            daemon.session_id,
            fetch_log_lines(client, daemon.session_id).await?,
        );
    }

    let layout = args
        .layout
        .unwrap_or_else(|| AgentCockpitLayout::default_for_size(SNAPSHOT_WIDTH));
    Ok(AppModel::agent_cockpit(
        ui_id,
        agent,
        daemons,
        selected_daemon,
        logs,
        AgentTerminalPane::new(16, 72, true, false),
        layout,
        monitor_profile,
    ))
}

async fn discover_daemons(
    client: &SessionControlClient,
) -> Result<Vec<millrace_sessions_core::protocol::SessionSummary>, CockpitError> {
    Ok(client
        .list(&SessionListRequest {
            role: Some(SessionRole::MillraceDaemon),
            workspace: None,
            include_archived: false,
        })
        .await?
        .sessions)
}

fn workspace_matches(
    session: &millrace_sessions_core::protocol::SessionSummary,
    workspace: &Path,
) -> bool {
    session
        .workspace
        .as_ref()
        .is_some_and(|identity| identity.canonical_path == workspace)
}

fn retain_terminal_workspace_daemons_only_when_no_active_daemon(
    sessions: &mut Vec<millrace_sessions_core::protocol::SessionSummary>,
    workspace: &Path,
) {
    if sessions.iter().any(|session| {
        workspace_matches(session, workspace) && is_active_state(&session.process_state)
    }) {
        sessions.retain(|session| {
            !workspace_matches(session, workspace) || is_active_state(&session.process_state)
        });
    }
}

async fn start_workspace_daemon(
    client: &SessionControlClient,
    workspace: &Path,
    monitor: &MonitorProfile,
) -> Result<millrace_sessions_core::protocol::SessionSummary, CockpitError> {
    let mut argv = vec![
        "millrace".to_string(),
        "run".to_string(),
        "daemon".to_string(),
        "--workspace".to_string(),
        workspace.display().to_string(),
    ];
    if *monitor != MonitorProfile::Auto {
        argv.push("--monitor".to_string());
        argv.push(monitor.as_wire_value());
    }
    resolve_argv_executable(&mut argv);
    let name = workspace
        .file_name()
        .and_then(|value| value.to_str())
        .map(|name| format!("daemon:{name}"));
    Ok(client
        .start(&SessionStartRequest {
            argv,
            cwd: Some(workspace.to_path_buf()),
            workspace: Some(workspace.to_path_buf()),
            name,
            role: Some(SessionRole::MillraceDaemon),
            session_id: None,
            spawn_mode: SpawnMode::Pty,
            monitor_profile: monitor.clone(),
            env: current_launch_env(),
        })
        .await?
        .session)
}

async fn ensure_agent_session(
    client: &SessionControlClient,
    args: &CockpitArgs,
    ui_id: UiId,
    daemon_session_id: Option<SessionId>,
    workspace: &Path,
) -> Result<millrace_sessions_core::protocol::SessionSummary, CockpitError> {
    let mut argv = args.resolved_agent_argv();
    resolve_argv_executable(&mut argv);
    let existing = client
        .list(&SessionListRequest {
            role: Some(SessionRole::Agent),
            workspace: Some(workspace.to_path_buf()),
            include_archived: false,
        })
        .await?
        .sessions
        .into_iter()
        .find(|session| is_active_state(&session.process_state) && session.argv == argv);
    if let Some(agent) = existing {
        return Ok(agent);
    }
    if args.no_start {
        return Err(CockpitError::NoAgentFound);
    }

    let agent_session_id = SessionId::new();
    let state = state_paths()?;
    let ui_paths = state.ui_context_paths(ui_id);
    let mut env = merge_current_launch_env(BTreeMap::from([
        (UI_ID_ENV.to_string(), ui_id.to_string()),
        (
            CONTEXT_FILE_ENV.to_string(),
            absolute_path(&ui_paths.context_json).display().to_string(),
        ),
        (
            STATE_DIR_ENV.to_string(),
            absolute_path(&state.root).display().to_string(),
        ),
        (
            CONTROL_SOCK_ENV.to_string(),
            absolute_path(&state.control_sock).display().to_string(),
        ),
        (
            AGENT_SESSION_ID_ENV.to_string(),
            agent_session_id.to_string(),
        ),
        (
            MILLRACE_WORKSPACE_ENV.to_string(),
            workspace.display().to_string(),
        ),
    ]));
    if let Some(session_id) = daemon_session_id {
        env.insert(
            "MILLMUX_ACTIVE_DAEMON_SESSION_ID".to_string(),
            session_id.to_string(),
        );
    }

    let name = workspace
        .file_name()
        .and_then(|value| value.to_str())
        .map(|name| format!("agent:{name}:{}", args.agent));
    Ok(client
        .start(&SessionStartRequest {
            argv,
            cwd: Some(workspace.to_path_buf()),
            workspace: Some(workspace.to_path_buf()),
            name,
            role: Some(SessionRole::Agent),
            session_id: Some(agent_session_id),
            spawn_mode: SpawnMode::Pty,
            monitor_profile: MonitorProfile::Auto,
            env,
        })
        .await?
        .session)
}

async fn seed_agent_terminal_from_attach(
    client: &SessionControlClient,
    app: &mut AppModel,
) -> Result<(), CockpitError> {
    let Some(agent_session_id) = app.agent_session_id else {
        return Ok(());
    };
    let (rows, cols) = (24, 80);
    let mut emulator = TerminalEmulator::new(rows, cols, TERMINAL_SCROLLBACK);
    let deadline = Instant::now() + SNAPSHOT_SEED_TIMEOUT;
    while Instant::now() < deadline && !agent_terminal_seeded(app) {
        let Some(connection) = open_seed_agent_attach(client, agent_session_id, deadline).await?
        else {
            return Ok(());
        };
        if drain_seed_agent_attach(connection, &mut emulator, app, deadline).await? {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        tokio::time::sleep(remaining.min(SNAPSHOT_SEED_RETRY_INTERVAL)).await;
    }
    Ok(())
}

async fn drain_seed_agent_attach(
    connection: AttachConnection,
    emulator: &mut TerminalEmulator,
    app: &mut AppModel,
    deadline: Instant,
) -> Result<bool, CockpitError> {
    let (_, mut reader, mut writer) = connection.split();
    let mut last_frame_at = None;

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let wait = seed_frame_wait(app, last_frame_at).min(remaining);
        match tokio::time::timeout(wait, reader.next_frame()).await {
            Ok(Ok(Some(frame))) => {
                if !apply_agent_attach_frame(frame, emulator, app) {
                    break;
                }
                last_frame_at = Some(Instant::now());
            }
            Ok(Ok(None)) => break,
            Ok(Err(error)) => {
                let _ = writer.write_frame(&AttachStreamFrame::Close).await;
                return Err(error.into());
            }
            Err(_) => break,
        }
    }

    let _ = writer.write_frame(&AttachStreamFrame::Close).await;
    Ok(agent_terminal_seeded(app))
}

fn seed_frame_wait(app: &AppModel, last_frame_at: Option<Instant>) -> Duration {
    if agent_terminal_seeded(app) {
        let Some(last_frame_at) = last_frame_at else {
            return Duration::ZERO;
        };
        return SNAPSHOT_SEED_OUTPUT_QUIET
            .saturating_sub(last_frame_at.elapsed())
            .min(SNAPSHOT_SEED_OUTPUT_QUIET);
    }
    SNAPSHOT_SEED_FRAME_WAIT
}

fn agent_terminal_seeded(app: &AppModel) -> bool {
    app.agent_terminal
        .as_ref()
        .is_some_and(|terminal| !terminal.initializing)
}

async fn open_seed_agent_attach(
    client: &SessionControlClient,
    session_id: SessionId,
    deadline: Instant,
) -> Result<Option<AttachConnection>, CockpitError> {
    loop {
        match client
            .attach(&agent_raw_replay_attach_request(session_id, true))
            .await
        {
            Ok(connection) => return Ok(Some(connection)),
            Err(ClientError::Control(error))
                if error.code == ControlErrorCode::SessionNotRunning
                    && Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(ClientError::Control(error))
                if error.code == ControlErrorCode::SessionNotRunning =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

async fn fetch_log_lines(
    client: &SessionControlClient,
    session_id: SessionId,
) -> Result<Vec<String>, CockpitError> {
    let response = client
        .logs(&SessionLogsRequest {
            selector: SessionSelector::Id { session_id },
            tail: Some(LOG_TAIL),
            follow: false,
        })
        .await?;
    Ok(response.lines.iter().map(render_log_line_text).collect())
}

async fn refresh_daemon_sessions(
    client: &SessionControlClient,
    app: &mut AppModel,
) -> Result<(), CockpitError> {
    let mut daemons = discover_daemons(client).await?;
    if let Some(workspace) = app
        .active_workspace
        .as_ref()
        .map(|identity| identity.canonical_path.clone())
    {
        retain_terminal_workspace_daemons_only_when_no_active_daemon(&mut daemons, &workspace);
    }
    app.replace_daemon_sessions(daemons);
    Ok(())
}

async fn run_interactive_cockpit(
    client: SessionControlClient,
    mut app: AppModel,
) -> Result<(), CockpitError> {
    let Some(agent_session_id) = app.agent_session_id else {
        return Err(CockpitError::NoAgentFound);
    };
    let mut terminal = TerminalSession::enter()?;
    let size = terminal.terminal.size()?;
    let (rows, cols) = app
        .agent_terminal_size_for(size.width, size.height)
        .unwrap_or((16, 72));
    let mut emulator = TerminalEmulator::new(rows, cols, TERMINAL_SCROLLBACK);
    let mut attach = open_agent_attach(&client, agent_session_id, rows, cols).await?;
    apply_agent_input_ownership(&mut app, attach.read_only);
    sync_agent_terminal_to_interactive_size(&mut app, &mut emulator, rows, cols);
    sync_agent_attach_size(&mut attach, rows, cols).await?;
    let mut last_refresh = Instant::now();
    let mut redraw = RedrawGate::new(Instant::now());
    let mut last_size = (rows, cols);
    terminal.terminal.draw(|frame| render_app(frame, &app))?;

    loop {
        match tokio::time::timeout(ATTACH_POLL_INTERVAL, attach.reader.next_frame()).await {
            Ok(Ok(Some(frame))) => {
                if !apply_agent_attach_frame(frame, &mut emulator, &mut app) {
                    break;
                }
                redraw.mark_dirty();
                let mut stream_closed = false;
                for _ in 0..64 {
                    match tokio::time::timeout(ATTACH_DRAIN_INTERVAL, attach.reader.next_frame())
                        .await
                    {
                        Ok(Ok(Some(frame))) => {
                            if !apply_agent_attach_frame(frame, &mut emulator, &mut app) {
                                stream_closed = true;
                                break;
                            }
                        }
                        Ok(Ok(None)) => break,
                        Ok(Err(error)) => {
                            app.set_host_reconnecting(1, error.to_string());
                            break;
                        }
                        Err(_) => break,
                    }
                }
                if stream_closed {
                    break;
                }
            }
            Ok(Ok(None)) => {
                app.set_host_reconnecting(1, "agent attach stream closed");
                match reopen_agent_attach(&client, agent_session_id, last_size, &mut app).await {
                    Ok(new_attach) => {
                        attach = new_attach;
                        app.set_host_connected();
                        redraw.mark_dirty();
                    }
                    Err(error) => {
                        app.set_host_disconnected(error.to_string());
                        redraw.mark_dirty();
                        tokio::time::sleep(REFRESH_INTERVAL).await;
                    }
                }
            }
            Ok(Err(error)) => {
                app.set_host_reconnecting(1, error.to_string());
                match reopen_agent_attach(&client, agent_session_id, last_size, &mut app).await {
                    Ok(new_attach) => {
                        attach = new_attach;
                        app.set_host_connected();
                        redraw.mark_dirty();
                    }
                    Err(reopen_error) => {
                        app.set_host_disconnected(reopen_error.to_string());
                        redraw.mark_dirty();
                        tokio::time::sleep(REFRESH_INTERVAL).await;
                    }
                }
            }
            Err(_) => {}
        }

        if event::poll(redraw.event_wait())? {
            match event::read()? {
                Event::Key(event) => {
                    let should_exit = handle_cockpit_key(
                        &client,
                        &mut app,
                        &mut attach,
                        &mut terminal,
                        &mut emulator,
                        event,
                    )
                    .await?;
                    redraw.mark_dirty();
                    if should_exit {
                        break;
                    }
                }
                Event::Resize(width, height) => {
                    if let Some((rows, cols)) = app.agent_terminal_size_for(width, height) {
                        if (rows, cols) != last_size {
                            emulator.resize(rows, cols);
                            app.resize_agent_terminal(rows, cols);
                            app.update_agent_terminal_view(
                                emulator.snapshot(),
                                emulator.is_following(),
                            );
                            attach
                                .writer
                                .write_frame(&AttachStreamFrame::Resize { rows, cols })
                                .await?;
                            last_size = (rows, cols);
                            redraw.mark_dirty();
                        }
                    }
                }
                _ => {}
            }
        }

        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            refresh_daemon_sessions(&client, &mut app).await?;
            refresh_logs(&client, &mut app).await?;
            last_refresh = Instant::now();
            redraw.mark_dirty();
        }

        let now = Instant::now();
        if redraw.should_draw(now) {
            terminal.terminal.draw(|frame| render_app(frame, &app))?;
            redraw.mark_drawn(now);
        }
    }

    let _ = attach.writer.write_frame(&AttachStreamFrame::Close).await;
    record_ui_event(
        &client,
        &app,
        UiEventKind::UiDetached,
        "agent cockpit detached",
        BTreeMap::new(),
    )
    .await?;
    Ok(())
}

fn write_snapshot(output: &str) -> Result<(), CockpitError> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(output.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

async fn open_agent_attach(
    client: &SessionControlClient,
    session_id: SessionId,
    rows: u16,
    cols: u16,
) -> Result<OpenedAgentAttach, CockpitError> {
    let deadline = Instant::now() + SNAPSHOT_SEED_TIMEOUT;
    loop {
        match try_open_agent_attach(client, session_id, rows, cols).await {
            Ok(attach) => return Ok(attach),
            Err(error) if should_retry_agent_attach(&error, deadline) => {
                tokio::time::sleep(SNAPSHOT_SEED_RETRY_INTERVAL).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

async fn try_open_agent_attach(
    client: &SessionControlClient,
    session_id: SessionId,
    rows: u16,
    cols: u16,
) -> Result<OpenedAgentAttach, ClientError> {
    let request = agent_attach_request(session_id, false, rows, cols);
    match client.attach(&request).await {
        Ok(connection) => Ok(opened_agent_attach(connection, false)),
        Err(ClientError::Control(error)) if error.code == ControlErrorCode::InputOwnerConflict => {
            let connection = client
                .attach(&agent_attach_request(session_id, true, rows, cols))
                .await?;
            Ok(opened_agent_attach(connection, true))
        }
        Err(error) => Err(error),
    }
}

fn agent_attach_request(
    session_id: SessionId,
    read_only: bool,
    rows: u16,
    cols: u16,
) -> SessionAttachRequest {
    SessionAttachRequest {
        selector: SessionSelector::Id { session_id },
        read_only,
        replay: AttachReplayMode::TerminalSnapshot,
        requested_terminal_size: Some(TerminalDimensions::new(rows, cols)),
        client_protocol_version: None,
        accepted_frame_types: Vec::new(),
        stream_encoding: None,
        initial_replay: None,
    }
}

fn agent_raw_replay_attach_request(session_id: SessionId, read_only: bool) -> SessionAttachRequest {
    SessionAttachRequest {
        selector: SessionSelector::Id { session_id },
        read_only,
        replay: AttachReplayMode::RawReplay,
        requested_terminal_size: None,
        client_protocol_version: Some(M2_ATTACH_PROTOCOL_VERSION),
        accepted_frame_types: vec![AttachFrameType::RawOutput],
        stream_encoding: Some(AttachStreamEncoding::RawBytes),
        initial_replay: Some(AttachInitialReplay::RawReplay),
    }
}

async fn reopen_agent_attach(
    client: &SessionControlClient,
    session_id: SessionId,
    terminal_size: (u16, u16),
    app: &mut AppModel,
) -> Result<OpenedAgentAttach, CockpitError> {
    client.ensure_host_ready().await?;
    let attach = open_agent_attach(client, session_id, terminal_size.0, terminal_size.1).await?;
    apply_agent_input_ownership(app, attach.read_only);
    Ok(attach)
}

fn opened_agent_attach(connection: AttachConnection, read_only: bool) -> OpenedAgentAttach {
    let (result, reader, writer) = connection.split();
    OpenedAgentAttach {
        reader,
        writer,
        read_only: read_only || !result.stream.input_owner,
    }
}

fn should_retry_agent_attach(error: &ClientError, deadline: Instant) -> bool {
    matches!(
        error,
        ClientError::Control(error) if error.code == ControlErrorCode::SessionNotRunning
    ) && Instant::now() < deadline
}

async fn sync_agent_attach_size(
    attach: &mut OpenedAgentAttach,
    rows: u16,
    cols: u16,
) -> Result<(), CockpitError> {
    attach
        .writer
        .write_frame(&AttachStreamFrame::Resize { rows, cols })
        .await?;
    Ok(())
}

fn sync_agent_terminal_to_interactive_size(
    app: &mut AppModel,
    emulator: &mut TerminalEmulator,
    rows: u16,
    cols: u16,
) {
    emulator.resize(rows, cols);
    app.resize_agent_terminal(rows, cols);
    app.update_agent_terminal_view(emulator.snapshot(), emulator.is_following());
}

struct OpenedAgentAttach {
    reader: crate::client::AttachReader,
    writer: crate::client::AttachWriter,
    read_only: bool,
}

fn apply_agent_input_ownership(app: &mut AppModel, read_only: bool) {
    app.set_agent_input_owner(!read_only);
}

#[derive(Debug, Clone)]
struct RedrawGate {
    last_draw: Instant,
    dirty: bool,
}

impl RedrawGate {
    fn new(now: Instant) -> Self {
        Self {
            last_draw: now,
            dirty: false,
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn event_wait(&self) -> Duration {
        if self.dirty {
            Duration::ZERO
        } else {
            Duration::from_millis(30)
        }
    }

    fn should_draw(&self, now: Instant) -> bool {
        self.dirty && now.duration_since(self.last_draw) >= REDRAW_INTERVAL
    }

    fn mark_drawn(&mut self, now: Instant) {
        self.last_draw = now;
        self.dirty = false;
    }
}

fn apply_agent_attach_frame(
    frame: AttachStreamFrame,
    emulator: &mut TerminalEmulator,
    app: &mut AppModel,
) -> bool {
    match frame {
        AttachStreamFrame::Scrollback { lines } => {
            let _ = lines;
        }
        AttachStreamFrame::Output { text } => {
            emulator.process_text(&text);
            app.update_agent_terminal_view(emulator.snapshot(), emulator.is_following());
        }
        AttachStreamFrame::RawOutput { data } => {
            emulator.process(data.as_slice());
            app.update_agent_terminal_view(emulator.snapshot(), emulator.is_following());
        }
        AttachStreamFrame::Error { error } => {
            if error.code == ControlErrorCode::InputOwnerConflict {
                app.set_agent_input_read_only();
            } else {
                app.set_command_failure(
                    vec!["agent attach".to_string()],
                    "agent terminal",
                    vec![error.to_string()],
                );
            }
        }
        AttachStreamFrame::Closed => return false,
        _ => {}
    }
    true
}

async fn handle_cockpit_key(
    client: &SessionControlClient,
    app: &mut AppModel,
    attach: &mut OpenedAgentAttach,
    terminal: &mut TerminalSession,
    emulator: &mut TerminalEmulator,
    event: KeyEvent,
) -> Result<bool, CockpitError> {
    if app.daemon_switcher.open {
        let previous_daemon = app.active_daemon_session_id;
        handle_daemon_switcher_key(app, event);
        if app.active_daemon_session_id != previous_daemon {
            record_active_daemon_changed(client, app).await?;
        }
        return Ok(false);
    }

    let previous_daemon = app.active_daemon_session_id;
    let agent_was_focused = app.focused_agent_terminal();
    let viewport_height = terminal
        .terminal
        .size()
        .ok()
        .and_then(|size| {
            app.agent_terminal_size_for(size.width, size.height)
                .map(|(rows, _)| rows)
        })
        .unwrap_or(20);
    let action = app.handle_key(event, viewport_height);
    match action {
        KeyAction::Detach => return Ok(true),
        KeyAction::Redraw => {
            terminal.recover_display()?;
        }
        KeyAction::SwitchFocus => {
            record_ui_event(
                client,
                app,
                UiEventKind::PaneFocused,
                "pane focused",
                BTreeMap::new(),
            )
            .await?;
        }
        KeyAction::OpenDaemonList => {
            app.open_daemon_switcher();
        }
        KeyAction::EnterScrollMode => {
            record_ui_event(
                client,
                app,
                UiEventKind::ScrollModeEntered,
                "scroll mode entered",
                BTreeMap::new(),
            )
            .await?;
        }
        KeyAction::ScrollUp if agent_was_focused => {
            emulator.scroll_up(1);
            app.update_agent_terminal_view(emulator.snapshot(), emulator.is_following());
        }
        KeyAction::ScrollDown if agent_was_focused => {
            emulator.scroll_down(1);
            app.update_agent_terminal_view(emulator.snapshot(), emulator.is_following());
        }
        KeyAction::PageUp if agent_was_focused => {
            emulator.page_up(viewport_height);
            app.update_agent_terminal_view(emulator.snapshot(), emulator.is_following());
        }
        KeyAction::PageDown if agent_was_focused => {
            emulator.page_down(viewport_height);
            app.update_agent_terminal_view(emulator.snapshot(), emulator.is_following());
        }
        KeyAction::JumpTop if agent_was_focused => {
            emulator.jump_top();
            app.update_agent_terminal_view(emulator.snapshot(), emulator.is_following());
        }
        KeyAction::ExitScrollMode | KeyAction::JumpBottom | KeyAction::Escape => {
            if agent_was_focused {
                emulator.jump_bottom();
                app.update_agent_terminal_view(emulator.snapshot(), emulator.is_following());
            }
            record_ui_event(
                client,
                app,
                UiEventKind::ScrollModeExited,
                "scroll mode exited",
                BTreeMap::new(),
            )
            .await?;
        }
        KeyAction::Input(event)
            if app.focused_agent_terminal() && app.agent_terminal_can_accept_input() =>
        {
            if let Some(text) = key_event_to_text(event) {
                attach
                    .writer
                    .write_frame(&AttachStreamFrame::Input { text })
                    .await?;
            }
        }
        _ => {}
    }
    if app.active_daemon_session_id != previous_daemon {
        record_active_daemon_changed(client, app).await?;
    }
    Ok(false)
}

fn handle_daemon_switcher_key(app: &mut AppModel, event: KeyEvent) {
    match event.code {
        KeyCode::Esc => app.close_daemon_switcher(),
        KeyCode::Enter => {
            let _ = app.activate_daemon_switcher_selection();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            let _ = app.move_daemon_switcher_selection(-1);
        }
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
            let _ = app.move_daemon_switcher_selection(1);
        }
        _ => {}
    }
}

async fn refresh_logs(
    client: &SessionControlClient,
    app: &mut AppModel,
) -> Result<(), CockpitError> {
    let session_ids = app.managed_daemon_session_ids.to_vec();
    for session_id in session_ids {
        match fetch_log_lines(client, session_id).await {
            Ok(lines) => {
                app.replace_daemon_output(session_id, lines);
                app.set_host_connected();
            }
            Err(error) => {
                app.set_host_reconnecting(1, error.to_string());
                match client.ensure_host_ready().await {
                    Ok(()) => match fetch_log_lines(client, session_id).await {
                        Ok(lines) => {
                            app.replace_daemon_output(session_id, lines);
                            app.set_host_connected();
                        }
                        Err(retry_error) => {
                            app.set_host_disconnected(retry_error.to_string());
                            return Ok(());
                        }
                    },
                    Err(reconnect_error) => {
                        app.set_host_disconnected(reconnect_error.to_string());
                        return Ok(());
                    }
                }
            }
        }
    }
    Ok(())
}

async fn record_active_daemon_changed(
    client: &SessionControlClient,
    app: &AppModel,
) -> Result<(), CockpitError> {
    let mut fields = BTreeMap::new();
    if let Some(session_id) = app.active_daemon_session_id {
        fields.insert("session_id".to_string(), session_id.to_string());
    }
    if let Some(workspace) = &app.active_workspace {
        fields.insert(
            "workspace".to_string(),
            workspace.canonical_path.display().to_string(),
        );
    }
    record_ui_event(
        client,
        app,
        UiEventKind::ActiveDaemonChanged,
        "active daemon changed",
        fields,
    )
    .await
}

async fn record_ui_event(
    client: &SessionControlClient,
    app: &AppModel,
    kind: UiEventKind,
    message: impl Into<String>,
    fields: BTreeMap<String, String>,
) -> Result<(), CockpitError> {
    let request = UiContextSetRequest {
        context: app.ui_context(),
        events: vec![UiEvent {
            timestamp: current_timestamp(),
            ui_id: app.ui_id,
            kind,
            message: Some(message.into()),
            fields,
        }],
    };
    match client.ui_context_set(&request).await {
        Ok(_) => {}
        Err(error) => {
            client.ensure_host_ready().await.map_err(|_| error)?;
            client.ui_context_set(&request).await?;
        }
    }
    Ok(())
}

fn bound_fields(key: &str, session_id: Option<SessionId>) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    if let Some(session_id) = session_id {
        fields.insert(key.to_string(), session_id.to_string());
    }
    fields
}

fn key_event_to_text(event: KeyEvent) -> Option<String> {
    match event.code {
        KeyCode::Char(value) if event.modifiers.contains(KeyModifiers::CONTROL) => {
            control_char(value)
        }
        KeyCode::Char(value) => Some(value.to_string()),
        KeyCode::Enter => Some("\r".to_string()),
        KeyCode::Tab => Some("\t".to_string()),
        KeyCode::Backspace => Some("\x7f".to_string()),
        KeyCode::Esc => Some("\x1b".to_string()),
        KeyCode::Left => Some("\x1b[D".to_string()),
        KeyCode::Right => Some("\x1b[C".to_string()),
        KeyCode::Up => Some("\x1b[A".to_string()),
        KeyCode::Down => Some("\x1b[B".to_string()),
        KeyCode::Home => Some("\x1b[H".to_string()),
        KeyCode::End => Some("\x1b[F".to_string()),
        KeyCode::PageUp => Some("\x1b[5~".to_string()),
        KeyCode::PageDown => Some("\x1b[6~".to_string()),
        KeyCode::Delete => Some("\x1b[3~".to_string()),
        _ => None,
    }
}

fn control_char(value: char) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    if lower.is_ascii_lowercase() {
        Some(char::from((lower as u8) & 0x1f).to_string())
    } else {
        match value {
            '[' => Some("\x1b".to_string()),
            ']' => Some("\x1d".to_string()),
            _ => None,
        }
    }
}

fn is_active_state(state: &ProcessState) -> bool {
    matches!(state, ProcessState::Starting | ProcessState::Running)
}

fn parse_or_new_ui_id(value: Option<&str>) -> Result<UiId, CockpitError> {
    value
        .map(str::parse)
        .transpose()
        .map_err(|_| CommandError::InvalidUiId(value.unwrap_or_default().to_string()).into())
        .map(|ui_id| ui_id.unwrap_or_else(UiId::new))
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cockpit_agent_attach_does_not_request_line_scrollback() {
        let session_id = SessionId::new();

        let request = agent_attach_request(session_id, false, 31, 99);

        assert_eq!(request.selector, SessionSelector::Id { session_id });
        assert!(!request.read_only);
        assert_eq!(request.replay, AttachReplayMode::TerminalSnapshot);
        assert_eq!(
            request.requested_terminal_size,
            Some(TerminalDimensions { rows: 31, cols: 99 })
        );
    }

    #[test]
    fn cockpit_agent_read_only_attach_also_avoids_line_scrollback() {
        let request = agent_attach_request(SessionId::new(), true, 18, 72);

        assert!(request.read_only);
        assert_eq!(request.replay, AttachReplayMode::TerminalSnapshot);
        assert_eq!(
            request.requested_terminal_size,
            Some(TerminalDimensions { rows: 18, cols: 72 })
        );
    }

    #[test]
    fn cockpit_snapshot_seed_uses_raw_replay_not_legacy_line_scrollback() {
        let request = agent_raw_replay_attach_request(SessionId::new(), true);

        assert!(request.read_only);
        assert_eq!(request.replay, AttachReplayMode::RawReplay);
        assert_eq!(request.requested_terminal_size, None);
        assert_eq!(
            request.client_protocol_version,
            Some(M2_ATTACH_PROTOCOL_VERSION)
        );
        assert_eq!(
            request.accepted_frame_types,
            vec![AttachFrameType::RawOutput]
        );
        assert_eq!(
            request.stream_encoding,
            Some(AttachStreamEncoding::RawBytes)
        );
        assert_eq!(request.initial_replay, Some(AttachInitialReplay::RawReplay));
    }

    #[test]
    fn cockpit_interactive_size_sync_replaces_stale_seeded_agent_snapshot() {
        let mut stale_terminal = TerminalEmulator::new(24, 80, TERMINAL_SCROLLBACK);
        stale_terminal.process_text("stale 24x80 preattach frame\r\n");
        let terminal_pane =
            AgentTerminalPane::with_snapshot(stale_terminal.snapshot(), true, false);
        let agent = session_summary("agent", SessionRole::Agent);
        let daemon = session_summary("daemon", SessionRole::MillraceDaemon);
        let daemon_id = daemon.session_id;
        let mut app = AppModel::agent_cockpit(
            UiId::new(),
            agent,
            vec![daemon],
            Some(daemon_id),
            BTreeMap::new(),
            terminal_pane,
            AgentCockpitLayout::Bottom,
            MonitorProfile::Basic,
        );
        let mut emulator = TerminalEmulator::new(8, 72, TERMINAL_SCROLLBACK);

        sync_agent_terminal_to_interactive_size(&mut app, &mut emulator, 8, 72);

        let terminal = app.agent_terminal.as_ref().expect("agent terminal");
        assert_eq!((terminal.snapshot.rows, terminal.snapshot.cols), (8, 72));
        assert_eq!((terminal.rows, terminal.cols), (8, 72));
        assert!(!terminal
            .snapshot
            .contains_text("stale 24x80 preattach frame"));
    }

    #[test]
    fn cockpit_attach_retries_starting_session_until_deadline() {
        let error = ClientError::Control(millrace_sessions_core::protocol::ControlErrorBody::new(
            ControlErrorCode::SessionNotRunning,
            "session is still starting",
        ));

        assert!(should_retry_agent_attach(
            &error,
            Instant::now() + Duration::from_secs(1)
        ));
        assert!(!should_retry_agent_attach(&error, Instant::now()));
    }

    #[test]
    fn redraw_gate_coalesces_dirty_draws_until_interval() {
        let start = Instant::now();
        let mut gate = RedrawGate::new(start);

        assert_eq!(gate.event_wait(), Duration::from_millis(30));
        assert!(!gate.should_draw(start + REDRAW_INTERVAL));

        gate.mark_dirty();

        assert_eq!(gate.event_wait(), Duration::ZERO);
        assert!(!gate.should_draw(start + REDRAW_INTERVAL / 2));
        assert!(gate.should_draw(start + REDRAW_INTERVAL));

        gate.mark_drawn(start + REDRAW_INTERVAL);

        assert_eq!(gate.event_wait(), Duration::from_millis(30));
        assert!(!gate.should_draw(start + REDRAW_INTERVAL * 2));
    }

    fn session_summary(
        name: &str,
        role: SessionRole,
    ) -> millrace_sessions_core::protocol::SessionSummary {
        let cwd = PathBuf::from(format!("/tmp/{name}"));
        millrace_sessions_core::protocol::SessionSummary {
            session_id: SessionId::new(),
            name: Some(name.to_string()),
            role,
            spawn_mode: SpawnMode::Pty,
            process_state: ProcessState::Running,
            attention_state: millrace_sessions_core::state::AttentionState::Idle,
            failure_message: None,
            workspace: Some(millrace_sessions_core::workspace::WorkspaceIdentity {
                canonical_path: cwd.clone(),
                unix_device: None,
                unix_inode: None,
            }),
            cwd,
            argv: vec![name.to_string()],
            monitor_profile: MonitorProfile::Auto,
            created_at: "2026-05-26T00:00:00Z".to_string(),
            updated_at: "2026-05-26T00:00:01Z".to_string(),
            stop_requested_at: None,
            stop_reason: None,
            attached_clients: 0,
            input_owner: None,
            capabilities: millrace_sessions_core::protocol::SessionCapabilities::for_spawn_mode(
                SpawnMode::Pty,
            ),
            artifacts: millrace_sessions_core::protocol::SessionArtifacts::default(),
            liveness: Default::default(),
        }
    }
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> Result<Self, CockpitError> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }

    fn recover_display(&mut self) -> Result<(), CockpitError> {
        execute!(
            self.terminal.backend_mut(),
            EnterAlternateScreen,
            Clear(ClearType::All),
            MoveTo(0, 0)
        )?;
        self.terminal.clear()?;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

#[derive(Debug, Error)]
pub enum CockpitError {
    #[error("no millrace-daemon session found or started for workspace")]
    NoDaemonFound,
    #[error("no agent session found or started for workspace")]
    NoAgentFound,
    #[error(transparent)]
    Command(#[from] CommandError),
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error(transparent)]
    Core(#[from] MillmuxError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
