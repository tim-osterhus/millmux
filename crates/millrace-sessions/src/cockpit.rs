use std::{
    collections::BTreeMap,
    env,
    io::{self, IsTerminal, Stdout},
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
        AttachStreamFrame, ControlErrorCode, SessionAttachRequest, SessionListRequest,
        SessionLogsRequest, SessionSelector, SessionStartRequest, UiContextSetRequest,
    },
    state::{MonitorProfile, ProcessState, SessionRole, UiEvent, UiEventKind},
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
};

const LOG_TAIL: usize = 4000;
const SNAPSHOT_WIDTH: u16 = 120;
const SNAPSHOT_HEIGHT: u16 = 28;
const REFRESH_INTERVAL: Duration = Duration::from_millis(300);
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
        seed_agent_terminal_from_logs(&client, &mut app).await?;
        record_ui_event(
            &client,
            &app,
            UiEventKind::UiDetached,
            "agent cockpit snapshot detached",
            BTreeMap::new(),
        )
        .await?;
        print!(
            "{}",
            render_to_string(&app, SNAPSHOT_WIDTH, SNAPSHOT_HEIGHT)
        );
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
            monitor_profile: monitor.clone(),
            env: Default::default(),
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
    let argv = args.resolved_agent_argv();
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
    let mut env = BTreeMap::from([
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
    ]);
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
            monitor_profile: MonitorProfile::Auto,
            env,
        })
        .await?
        .session)
}

async fn seed_agent_terminal_from_logs(
    client: &SessionControlClient,
    app: &mut AppModel,
) -> Result<(), CockpitError> {
    let Some(agent_session_id) = app.agent_session_id else {
        return Ok(());
    };
    let mut emulator = TerminalEmulator::new(16, 72, TERMINAL_SCROLLBACK);
    let mut response = client
        .logs(&SessionLogsRequest {
            selector: SessionSelector::Id {
                session_id: agent_session_id,
            },
            tail: Some(LOG_TAIL),
            follow: false,
        })
        .await?;
    for _ in 0..10 {
        if !response.lines.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        response = client
            .logs(&SessionLogsRequest {
                selector: SessionSelector::Id {
                    session_id: agent_session_id,
                },
                tail: Some(LOG_TAIL),
                follow: false,
            })
            .await?;
    }
    for line in response.lines {
        emulator.process_text(&line.line);
        emulator.process_text("\n");
    }
    app.update_agent_terminal(emulator.snapshot());
    Ok(())
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
    Ok(response.lines.into_iter().map(|line| line.line).collect())
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
    let mut attach = open_agent_attach(&client, agent_session_id).await?;
    if attach.read_only {
        app.set_agent_input_read_only();
    }
    let mut last_refresh = Instant::now();
    let mut last_size = (rows, cols);

    loop {
        terminal.terminal.draw(|frame| render_app(frame, &app))?;

        match tokio::time::timeout(Duration::from_millis(5), attach.reader.next_frame()).await {
            Ok(Ok(Some(frame))) => match frame {
                AttachStreamFrame::Scrollback { lines } => {
                    for line in lines {
                        emulator.process_text(&line);
                        emulator.process_text("\n");
                    }
                    app.update_agent_terminal(emulator.snapshot());
                }
                AttachStreamFrame::Output { text } => {
                    emulator.process_text(&text);
                    app.update_agent_terminal(emulator.snapshot());
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
                AttachStreamFrame::Closed => break,
                _ => {}
            },
            Ok(Ok(None)) => {
                app.set_host_reconnecting(1, "agent attach stream closed");
                match reopen_agent_attach(&client, agent_session_id, &mut app).await {
                    Ok(new_attach) => {
                        attach = new_attach;
                        app.set_host_connected();
                    }
                    Err(error) => {
                        app.set_host_disconnected(error.to_string());
                        tokio::time::sleep(REFRESH_INTERVAL).await;
                    }
                }
            }
            Ok(Err(error)) => {
                app.set_host_reconnecting(1, error.to_string());
                match reopen_agent_attach(&client, agent_session_id, &mut app).await {
                    Ok(new_attach) => {
                        attach = new_attach;
                        app.set_host_connected();
                    }
                    Err(reopen_error) => {
                        app.set_host_disconnected(reopen_error.to_string());
                        tokio::time::sleep(REFRESH_INTERVAL).await;
                    }
                }
            }
            Err(_) => {}
        }

        if event::poll(Duration::from_millis(30))? {
            match event::read()? {
                Event::Key(event) => {
                    let should_exit =
                        handle_cockpit_key(&client, &mut app, &mut attach, &mut terminal, event)
                            .await?;
                    if should_exit {
                        break;
                    }
                }
                Event::Resize(width, height) => {
                    if let Some((rows, cols)) = app.agent_terminal_size_for(width, height) {
                        if (rows, cols) != last_size {
                            emulator.resize(rows, cols);
                            app.resize_agent_terminal(rows, cols);
                            app.update_agent_terminal(emulator.snapshot());
                            attach
                                .writer
                                .write_frame(&AttachStreamFrame::Resize { rows, cols })
                                .await?;
                            last_size = (rows, cols);
                        }
                    }
                }
                _ => {}
            }
        }

        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            refresh_logs(&client, &mut app).await?;
            last_refresh = Instant::now();
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

async fn open_agent_attach(
    client: &SessionControlClient,
    session_id: SessionId,
) -> Result<OpenedAgentAttach, CockpitError> {
    let request = SessionAttachRequest {
        selector: SessionSelector::Id { session_id },
        read_only: false,
        include_scrollback: true,
    };
    match client.attach(&request).await {
        Ok(connection) => Ok(opened_agent_attach(connection, false)),
        Err(ClientError::Control(error)) if error.code == ControlErrorCode::InputOwnerConflict => {
            let connection = client
                .attach(&SessionAttachRequest {
                    read_only: true,
                    ..request
                })
                .await?;
            Ok(opened_agent_attach(connection, true))
        }
        Err(error) => Err(error.into()),
    }
}

async fn reopen_agent_attach(
    client: &SessionControlClient,
    session_id: SessionId,
    app: &mut AppModel,
) -> Result<OpenedAgentAttach, CockpitError> {
    client.ensure_host_ready().await?;
    let attach = open_agent_attach(client, session_id).await?;
    if attach.read_only {
        app.set_agent_input_read_only();
    }
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

struct OpenedAgentAttach {
    reader: crate::client::AttachReader,
    writer: crate::client::AttachWriter,
    read_only: bool,
}

async fn handle_cockpit_key(
    client: &SessionControlClient,
    app: &mut AppModel,
    attach: &mut OpenedAgentAttach,
    terminal: &mut TerminalSession,
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
    let action = app.handle_key(event, 20);
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
        KeyAction::ExitScrollMode | KeyAction::JumpBottom | KeyAction::Escape => {
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
