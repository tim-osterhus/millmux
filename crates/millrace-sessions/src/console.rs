use std::{
    collections::BTreeMap,
    io::{self, IsTerminal, Stdout, Write},
    path::Path,
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
    protocol::{
        DoctorRequest, SessionDeleteRequest, SessionEventsRequest, SessionInspectRequest,
        SessionKillRequest, SessionListRequest, SessionLogsRequest, SessionSelector,
        SessionStartRequest, SessionStopRequest, UiContextSetRequest,
    },
    state::{MonitorProfile, ProcessState, SessionRole, SpawnMode, UiEvent, UiEventKind},
};
use millrace_sessions_tui::{
    renderer::{render_app, render_to_string},
    AppModel, DaemonConsoleLayout,
};
use ratatui::{backend::CrosstermBackend, Terminal};
use serde::Serialize;
use thiserror::Error;

use crate::{
    client::{ClientError, SessionControlClient},
    commands::{CommandError, ConsoleArgs, ConsoleCommand},
    launch_env::{current_launch_env, resolve_argv_executable},
    output::render_log_line_text,
};

const LOG_TAIL: usize = 4000;
const SNAPSHOT_WIDTH: u16 = 100;
const SNAPSHOT_HEIGHT: u16 = 24;
const REFRESH_INTERVAL: Duration = Duration::from_millis(300);

pub async fn run_console(args: ConsoleArgs) -> Result<(), ConsoleError> {
    if args.role != SessionRole::MillraceDaemon {
        return Err(ConsoleError::UnsupportedRole(args.role));
    }

    let client = SessionControlClient::new()?;
    client.ensure_host_ready().await?;
    let ui_id = parse_or_new_ui_id(args.ui.as_deref())?;
    let mut app = build_console_app(&client, &args, ui_id).await?;
    record_ui_event(
        &client,
        &app,
        UiEventKind::UiStarted,
        "daemon console started",
        BTreeMap::new(),
    )
    .await?;

    if let Some(command) = args.command {
        execute_console_command(&client, &mut app, command, args.confirm.as_deref()).await?;
        record_ui_event(
            &client,
            &app,
            UiEventKind::UiDetached,
            "daemon console snapshot detached",
            BTreeMap::new(),
        )
        .await?;
        write_snapshot(&render_to_string(&app, SNAPSHOT_WIDTH, SNAPSHOT_HEIGHT))?;
        return Ok(());
    }

    if args.once || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        record_ui_event(
            &client,
            &app,
            UiEventKind::UiDetached,
            "daemon console snapshot detached",
            BTreeMap::new(),
        )
        .await?;
        write_snapshot(&render_to_string(&app, SNAPSHOT_WIDTH, SNAPSHOT_HEIGHT))?;
        return Ok(());
    }

    run_interactive_console(client, app).await
}

async fn build_console_app(
    client: &SessionControlClient,
    args: &ConsoleArgs,
    ui_id: UiId,
) -> Result<AppModel, ConsoleError> {
    let mut sessions = discover_daemons(client, args.workspace.as_deref()).await?;
    let requested_monitor = args.monitor.clone().unwrap_or_default();
    let has_active_daemon = sessions
        .iter()
        .any(|session| is_active_daemon_state(&session.process_state));
    if sessions.is_empty() && !args.no_start && args.workspace.is_none() {
        return Err(ConsoleError::NoDaemonsWithoutWorkspace);
    }
    if args.workspace.is_some() && !has_active_daemon && !args.no_start {
        let Some(workspace) = &args.workspace else {
            return Err(ConsoleError::NoDaemonsWithoutWorkspace);
        };
        let started = start_workspace_daemon(client, workspace, &requested_monitor).await?;
        sessions.push(started);
    }
    if sessions.is_empty() {
        return Err(ConsoleError::NoDaemonsFound);
    }
    retain_terminal_daemons_only_when_active_daemon_exists(
        &mut sessions,
        args.workspace.as_deref(),
    );

    sessions.sort_by(|left, right| {
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

    let selected = sessions
        .iter()
        .find(|session| is_active_daemon_state(&session.process_state))
        .or_else(|| sessions.first())
        .map(|session| session.session_id);
    let selected_session = selected.and_then(|session_id| {
        sessions
            .iter()
            .find(|session| session.session_id == session_id)
    });
    let monitor_profile = args.monitor.clone().unwrap_or_else(|| {
        selected_session.map_or(MonitorProfile::Auto, |session| {
            session.monitor_profile.clone()
        })
    });
    let mut logs = BTreeMap::new();
    for session in &sessions {
        logs.insert(
            session.session_id,
            fetch_log_lines(client, session.session_id).await?,
        );
    }

    let layout = args
        .layout
        .unwrap_or_else(|| DaemonConsoleLayout::default_for_daemon_count(sessions.len()));
    Ok(AppModel::daemon_console(
        ui_id,
        sessions,
        selected,
        logs,
        layout,
        monitor_profile,
    ))
}

async fn discover_daemons(
    client: &SessionControlClient,
    workspace: Option<&Path>,
) -> Result<Vec<millrace_sessions_core::protocol::SessionSummary>, ConsoleError> {
    let response = client
        .list(&SessionListRequest {
            role: Some(SessionRole::MillraceDaemon),
            workspace: workspace.map(Path::to_path_buf),
            include_archived: false,
        })
        .await?;
    Ok(response.sessions)
}

fn is_active_daemon_state(state: &ProcessState) -> bool {
    matches!(state, ProcessState::Starting | ProcessState::Running)
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

fn retain_terminal_daemons_only_when_active_daemon_exists(
    sessions: &mut Vec<millrace_sessions_core::protocol::SessionSummary>,
    workspace: Option<&Path>,
) {
    let has_active = sessions.iter().any(|session| {
        workspace.map_or(true, |workspace| workspace_matches(session, workspace))
            && is_active_daemon_state(&session.process_state)
    });
    if has_active {
        sessions.retain(|session| {
            !workspace.map_or(true, |workspace| workspace_matches(session, workspace))
                || is_active_daemon_state(&session.process_state)
        });
    }
}

async fn start_workspace_daemon(
    client: &SessionControlClient,
    workspace: &Path,
    monitor: &MonitorProfile,
) -> Result<millrace_sessions_core::protocol::SessionSummary, ConsoleError> {
    let canonical_workspace = workspace.canonicalize()?;
    let mut argv = vec![
        "millrace".to_string(),
        "run".to_string(),
        "daemon".to_string(),
        "--workspace".to_string(),
        canonical_workspace.display().to_string(),
    ];
    if *monitor != MonitorProfile::Auto {
        argv.push("--monitor".to_string());
        argv.push(monitor.as_wire_value());
    }
    resolve_argv_executable(&mut argv);
    let name = canonical_workspace
        .file_name()
        .and_then(|value| value.to_str())
        .map(|name| format!("daemon:{name}"));
    let result = client
        .start(&SessionStartRequest {
            argv,
            cwd: Some(canonical_workspace.clone()),
            workspace: Some(canonical_workspace),
            name,
            role: Some(SessionRole::MillraceDaemon),
            session_id: None,
            spawn_mode: SpawnMode::Pty,
            monitor_profile: monitor.clone(),
            env: current_launch_env(),
        })
        .await?;
    Ok(result.session)
}

async fn fetch_log_lines(
    client: &SessionControlClient,
    session_id: SessionId,
) -> Result<Vec<String>, ConsoleError> {
    let response = client
        .logs(&SessionLogsRequest {
            selector: SessionSelector::Id { session_id },
            tail: Some(LOG_TAIL),
            follow: false,
        })
        .await?;
    Ok(response.lines.iter().map(render_log_line_text).collect())
}

async fn run_interactive_console(
    client: SessionControlClient,
    mut app: AppModel,
) -> Result<(), ConsoleError> {
    let mut terminal = TerminalSession::enter()?;
    let mut last_refresh = Instant::now();

    loop {
        terminal.terminal.draw(|frame| render_app(frame, &app))?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(event) = event::read()? {
                if handle_console_key(&client, &mut app, &mut terminal, event).await? {
                    break;
                }
            }
        }

        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            refresh_logs(&client, &mut app).await?;
            last_refresh = Instant::now();
        }
    }

    Ok(())
}

fn write_snapshot(output: &str) -> Result<(), ConsoleError> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(output.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

async fn handle_console_key(
    client: &SessionControlClient,
    app: &mut AppModel,
    terminal: &mut TerminalSession,
    event: KeyEvent,
) -> Result<bool, ConsoleError> {
    if app.daemon_switcher.open {
        let previous_daemon = app.active_daemon_session_id;
        handle_daemon_switcher_key(app, event);
        if app.active_daemon_session_id != previous_daemon {
            record_active_daemon_changed(client, app).await?;
        }
        return Ok(false);
    }
    if app.confirmation.is_some() {
        return handle_confirmation_key(client, app, event).await;
    }
    if app.command_palette.open {
        handle_palette_key(client, app, event).await?;
        return Ok(false);
    }

    let previous_daemon = app.active_daemon_session_id;
    let action = app.handle_key(event, 20);
    match action {
        millrace_sessions_tui::KeyAction::Detach => {
            record_ui_event(
                client,
                app,
                UiEventKind::UiDetached,
                "daemon console detached",
                BTreeMap::new(),
            )
            .await?;
            return Ok(true);
        }
        millrace_sessions_tui::KeyAction::CloseRequested => {
            app.require_confirmation("close", "ui context", "close");
        }
        millrace_sessions_tui::KeyAction::Redraw => {
            terminal.recover_display()?;
        }
        millrace_sessions_tui::KeyAction::SwitchFocus => {
            let mut fields = BTreeMap::new();
            if let Some(pane_id) = app.active_pane_id {
                fields.insert("pane_id".to_string(), pane_id.to_string());
            }
            record_ui_event(
                client,
                app,
                UiEventKind::PaneFocused,
                "pane focused",
                fields,
            )
            .await?;
            if app.active_daemon_session_id != previous_daemon {
                record_active_daemon_changed(client, app).await?;
            }
        }
        millrace_sessions_tui::KeyAction::EnterScrollMode => {
            record_ui_event(
                client,
                app,
                UiEventKind::ScrollModeEntered,
                "scroll mode entered",
                BTreeMap::new(),
            )
            .await?;
        }
        millrace_sessions_tui::KeyAction::ExitScrollMode
        | millrace_sessions_tui::KeyAction::JumpBottom
        | millrace_sessions_tui::KeyAction::Escape => {
            record_ui_event(
                client,
                app,
                UiEventKind::ScrollModeExited,
                "scroll mode exited",
                BTreeMap::new(),
            )
            .await?;
        }
        _ => {}
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

async fn handle_palette_key(
    client: &SessionControlClient,
    app: &mut AppModel,
    event: KeyEvent,
) -> Result<(), ConsoleError> {
    match event.code {
        KeyCode::Esc => {
            app.command_palette.open = false;
            app.command_palette.input.clear();
        }
        KeyCode::Backspace => {
            app.command_palette.input.pop();
        }
        KeyCode::Enter => {
            let input = app.command_palette.input.trim().to_string();
            app.command_palette.open = false;
            app.command_palette.input.clear();
            if input.is_empty() {
                return Ok(());
            }
            let command = parse_palette_command(&input)?;
            if command.is_destructive() {
                let challenge = app
                    .active_daemon_session_id
                    .map(|session_id| session_id.to_string())
                    .unwrap_or_else(|| "confirm".to_string());
                app.require_confirmation(command.as_str(), app.command_target_label(), challenge);
            } else {
                execute_console_command(client, app, command, None).await?;
            }
        }
        KeyCode::Char(value) if event.modifiers == KeyModifiers::NONE => {
            app.command_palette.input.push(value);
        }
        _ => {}
    }
    Ok(())
}

async fn handle_confirmation_key(
    client: &SessionControlClient,
    app: &mut AppModel,
    event: KeyEvent,
) -> Result<bool, ConsoleError> {
    match event.code {
        KeyCode::Esc => {
            app.confirmation = None;
            Ok(false)
        }
        KeyCode::Backspace => {
            if let Some(prompt) = &mut app.confirmation {
                prompt.input.pop();
            }
            Ok(false)
        }
        KeyCode::Enter => {
            let Some(prompt) = app.confirmation.take() else {
                return Ok(false);
            };
            if !prompt.matches_challenge() {
                app.set_command_failure(
                    vec![prompt.operation.clone()],
                    prompt.target,
                    vec!["confirmation did not match".to_string()],
                );
                return Ok(false);
            }
            if prompt.operation == "close" {
                record_ui_event(
                    client,
                    app,
                    UiEventKind::UiClosed,
                    "daemon console closed",
                    BTreeMap::new(),
                )
                .await?;
                client
                    .ui_context_close(&millrace_sessions_core::protocol::UiContextCloseRequest {
                        ui_id: app.ui_id,
                    })
                    .await?;
                return Ok(true);
            }
            let command = parse_palette_command(&prompt.operation)?;
            execute_console_command(client, app, command, Some(&prompt.challenge)).await?;
            Ok(false)
        }
        KeyCode::Char(value) if event.modifiers == KeyModifiers::NONE => {
            if let Some(prompt) = &mut app.confirmation {
                prompt.input.push(value);
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}

async fn execute_console_command(
    client: &SessionControlClient,
    app: &mut AppModel,
    command: ConsoleCommand,
    confirmation: Option<&str>,
) -> Result<(), ConsoleError> {
    let session_id = app
        .active_daemon_session_id
        .ok_or(ConsoleError::NoSelectedDaemon)?;
    let confirmation_challenge = session_id.to_string();
    if command.is_destructive() && confirmation != Some(confirmation_challenge.as_str()) {
        return Err(ConsoleError::ConfirmationRequired {
            operation: command.as_str(),
            challenge: confirmation_challenge,
        });
    }

    let target = app.command_target_label();
    let argv = vec!["millmux".to_string(), command.as_str().to_string()];
    app.set_command_running(argv.clone(), target.clone());
    let mut fields = BTreeMap::new();
    fields.insert("command".to_string(), command.as_str().to_string());
    fields.insert("target".to_string(), target.clone());
    fields.insert("session_id".to_string(), session_id.to_string());
    record_ui_event(
        client,
        app,
        UiEventKind::CommandStarted,
        "command started",
        fields.clone(),
    )
    .await?;

    let selector = SessionSelector::Id { session_id };
    let result = match command {
        ConsoleCommand::Status | ConsoleCommand::Inspect => {
            encode_result(client.inspect(&SessionInspectRequest { selector }).await)
        }
        ConsoleCommand::Logs => encode_result(
            client
                .logs(&SessionLogsRequest {
                    selector,
                    tail: Some(80),
                    follow: false,
                })
                .await,
        ),
        ConsoleCommand::Events => encode_result(
            client
                .events(&SessionEventsRequest {
                    selector,
                    tail: None,
                    follow: false,
                })
                .await,
        ),
        ConsoleCommand::Doctor => encode_result(client.doctor(&DoctorRequest::default()).await),
        ConsoleCommand::Stop => encode_result(
            client
                .stop(&SessionStopRequest {
                    selector,
                    grace_seconds: None,
                    reason: None,
                })
                .await,
        ),
        ConsoleCommand::Kill => encode_result(client.kill(&SessionKillRequest { selector }).await),
        ConsoleCommand::Delete | ConsoleCommand::Archive => encode_result(
            client
                .delete(&SessionDeleteRequest {
                    selector,
                    purge: false,
                    kill: false,
                })
                .await,
        ),
        ConsoleCommand::Purge => encode_result(
            client
                .delete(&SessionDeleteRequest {
                    selector,
                    purge: true,
                    kill: false,
                })
                .await,
        ),
    };

    match result {
        Ok(lines) => {
            app.set_command_success(argv, target, lines);
            record_ui_event(
                client,
                app,
                UiEventKind::CommandFinished,
                "command finished",
                fields,
            )
            .await?;
        }
        Err(error) => {
            app.set_command_failure(argv, target, vec![error.to_string()]);
            record_ui_event(
                client,
                app,
                UiEventKind::CommandFailed,
                "command failed",
                fields,
            )
            .await?;
        }
    }

    Ok(())
}

async fn refresh_logs(
    client: &SessionControlClient,
    app: &mut AppModel,
) -> Result<(), ConsoleError> {
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
) -> Result<(), ConsoleError> {
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
) -> Result<(), ConsoleError> {
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

fn json_lines<T: Serialize>(value: &T) -> Result<Vec<String>, serde_json::Error> {
    Ok(serde_json::to_string_pretty(value)?
        .lines()
        .map(str::to_string)
        .collect())
}

fn encode_result<T: Serialize>(result: Result<T, ClientError>) -> Result<Vec<String>, String> {
    result
        .map_err(|error| error.to_string())
        .and_then(|value| json_lines(&value).map_err(|error| error.to_string()))
}

fn parse_palette_command(value: &str) -> Result<ConsoleCommand, ConsoleError> {
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
        other => Err(ConsoleError::InvalidConsoleCommand(other.to_string())),
    }
}

fn parse_or_new_ui_id(value: Option<&str>) -> Result<UiId, ConsoleError> {
    value
        .map(str::parse)
        .transpose()
        .map_err(|_| CommandError::InvalidUiId(value.unwrap_or_default().to_string()).into())
        .map(|ui_id| ui_id.unwrap_or_else(UiId::new))
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> Result<Self, ConsoleError> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }

    fn recover_display(&mut self) -> Result<(), ConsoleError> {
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
pub enum ConsoleError {
    #[error("millmux console only supports role=millrace-daemon, got {0:?}")]
    UnsupportedRole(SessionRole),
    #[error("no millrace-daemon sessions found; pass --workspace to start one")]
    NoDaemonsWithoutWorkspace,
    #[error("no millrace-daemon sessions found")]
    NoDaemonsFound,
    #[error("no selected daemon")]
    NoSelectedDaemon,
    #[error("confirmation required for {operation}; pass --confirm {challenge}")]
    ConfirmationRequired {
        operation: &'static str,
        challenge: String,
    },
    #[error("invalid daemon console command: {0}")]
    InvalidConsoleCommand(String),
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
