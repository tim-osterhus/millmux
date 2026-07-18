use std::{
    collections::BTreeMap,
    env,
    fs::{File, OpenOptions},
    io::{self, IsTerminal, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crossterm::{
    cursor::{Hide, Show},
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyModifiers,
    },
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    Command, QueueableCommand,
};
use millrace_sessions_core::{
    error::MillmuxError,
    events::current_timestamp,
    ids::{SessionId, UiId},
    paths::{state_paths, CONTROL_SOCK_ENV, STATE_DIR_ENV, UI_ID_ENV},
    protocol::{
        AttachFrameType, AttachInitialReplay, AttachReplayMode, AttachStreamEncoding,
        AttachStreamFrame, AttentionReadRequest, ControlErrorCode, SessionAttachRequest,
        SessionInspectRequest, SessionListRequest, SessionLogsRequest, SessionSelector,
        SessionStartRequest, SessionSummary, TerminalDimensions, UiContextGetRequest,
        UiContextSetRequest, M2_ATTACH_PROTOCOL_VERSION,
    },
    state::{
        AttentionKind, LivenessState, MonitorProfile, ProcessState, SessionRole, SpawnMode,
        UiContext, UiEvent, UiEventKind, WorkerMeta,
    },
};
use millrace_sessions_tui::{
    renderer::{render_app, render_to_string},
    AgentCockpitLayout, AgentTerminalPane, AppModel, KeyAction, TerminalEmulator,
    TerminalSearchDirection, WorkspaceSessionSelection,
};
use ratatui::{backend::CrosstermBackend, Terminal};
use thiserror::Error;

use crate::{
    attach::{
        close_attach_stream, close_attach_stream_without_output, managed_output_write_timeout,
        prepare_managed_raw_attach, record_managed_raw_test_phase, AttachError,
        ManagedRawAttachEvent, ManagedRawAttachExit,
    },
    client::{AttachConnection, AttachReader, AttachWriter, ClientError, SessionControlClient},
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
const MAX_ATTACH_INPUT_FRAME_BYTES: usize = 512;
const SNAPSHOT_SEED_TIMEOUT: Duration = Duration::from_millis(3_000);
const SNAPSHOT_SEED_FRAME_WAIT: Duration = Duration::from_millis(1_000);
const SNAPSHOT_SEED_OUTPUT_QUIET: Duration = Duration::from_millis(75);
const SNAPSHOT_SEED_RETRY_INTERVAL: Duration = Duration::from_millis(25);
const RAW_REPLAY_ATTEMPTS: usize = 3;
const TERMINAL_SCROLLBACK: usize = 4000;
const CONTEXT_FILE_ENV: &str = "MILLMUX_CONTEXT_FILE";
const AGENT_SESSION_ID_ENV: &str = "MILLMUX_AGENT_SESSION_ID";
const MILLRACE_WORKSPACE_ENV: &str = "MILLRACE_WORKSPACE";
const BRACKETED_PASTE_BEGIN: &str = "\x1b[200~";
const BRACKETED_PASTE_END: &str = "\x1b[201~";
pub const MAX_COCKPIT_PASTE_BYTES: usize = 64 * 1024;

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
    let prior_context = load_prior_ui_context(client, ui_id).await?;
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
    let mut app = AppModel::agent_cockpit(
        ui_id,
        agent,
        daemons,
        selected_daemon,
        logs,
        AgentTerminalPane::new(16, 72, true, false),
        layout,
        monitor_profile,
    );
    app.replace_workspace_sessions(discover_workspace_sessions(client, &workspace).await?);
    if let Some(context) = prior_context.as_ref() {
        app.restore_ui_context_selection(context);
    }
    Ok(app)
}

async fn load_prior_ui_context(
    client: &SessionControlClient,
    ui_id: UiId,
) -> Result<Option<UiContext>, CockpitError> {
    match client
        .ui_context_get(&UiContextGetRequest { ui_id: Some(ui_id) })
        .await
    {
        Ok(response) => Ok(Some(response.context)),
        Err(ClientError::Control(error)) if error.code == ControlErrorCode::UiContextNotFound => {
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

async fn discover_daemons(
    client: &SessionControlClient,
) -> Result<Vec<millrace_sessions_core::protocol::SessionSummary>, CockpitError> {
    let sessions = client
        .list(&SessionListRequest {
            role: Some(SessionRole::MillraceDaemon),
            workspace: None,
            include_archived: false,
        })
        .await?
        .sessions;
    Ok(sessions)
}

async fn discover_workspace_sessions(
    client: &SessionControlClient,
    workspace: &Path,
) -> Result<Vec<millrace_sessions_core::protocol::SessionSummary>, CockpitError> {
    let sessions = client
        .list(&SessionListRequest {
            role: None,
            workspace: None,
            include_archived: false,
        })
        .await?
        .sessions
        .into_iter()
        .filter(|session| session_scoped_to_workspace(session, workspace))
        .collect::<Vec<_>>();
    Ok(sessions)
}

fn session_scoped_to_workspace(
    session: &millrace_sessions_core::protocol::SessionSummary,
    workspace: &Path,
) -> bool {
    if workspace_matches(session, workspace) {
        return true;
    }
    session
        .cwd
        .canonicalize()
        .ok()
        .is_some_and(|cwd| cwd == workspace || cwd.starts_with(workspace))
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
    while Instant::now() < deadline {
        let Some(connection) =
            open_seed_agent_attach(client, agent_session_id, rows, cols, deadline).await?
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
    let mut snapshot_suffix_pending = false;

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let ready = agent_terminal_seeded(app, snapshot_suffix_pending);
        let wait = seed_frame_wait(ready, last_frame_at).min(remaining);
        match tokio::time::timeout(wait, reader.next_frame()).await {
            Ok(Ok(Some(frame))) => {
                match &frame {
                    AttachStreamFrame::ScreenSnapshot { snapshot } => {
                        snapshot_suffix_pending =
                            snapshot.source.pty_log_offset < snapshot.source.raw_replay_end_offset;
                    }
                    AttachStreamFrame::RawOutput { .. } if snapshot_suffix_pending => {
                        snapshot_suffix_pending = false;
                    }
                    _ => {}
                }
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
    Ok(agent_terminal_seeded(app, snapshot_suffix_pending))
}

fn seed_frame_wait(ready: bool, last_frame_at: Option<Instant>) -> Duration {
    if ready {
        let Some(last_frame_at) = last_frame_at else {
            return Duration::ZERO;
        };
        return SNAPSHOT_SEED_OUTPUT_QUIET
            .saturating_sub(last_frame_at.elapsed())
            .min(SNAPSHOT_SEED_OUTPUT_QUIET);
    }
    SNAPSHOT_SEED_FRAME_WAIT
}

fn agent_terminal_seeded(app: &AppModel, snapshot_suffix_pending: bool) -> bool {
    !snapshot_suffix_pending
        && app.agent_terminal.as_ref().is_some_and(|terminal| {
            !terminal.initializing
                && terminal
                    .snapshot
                    .cells
                    .iter()
                    .flatten()
                    .any(|cell| cell.occupied)
        })
}

async fn open_seed_agent_attach(
    client: &SessionControlClient,
    session_id: SessionId,
    rows: u16,
    cols: u16,
    deadline: Instant,
) -> Result<Option<AttachConnection>, CockpitError> {
    loop {
        match attach_before_deadline(
            client,
            &agent_attach_request(session_id, true, rows, cols),
            deadline,
        )
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
    if let Some(workspace) = app
        .active_workspace
        .as_ref()
        .map(|identity| identity.canonical_path.clone())
    {
        let sessions = discover_workspace_sessions(client, &workspace).await?;
        app.replace_workspace_sessions(sessions);
        return Ok(());
    }

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
    let Some(mut attached_session_id) = app.active_attach_session_id() else {
        return Err(CockpitError::NoAgentFound);
    };
    let terminal = Arc::new(Mutex::new(TerminalSession::enter()?));
    let size = terminal
        .lock()
        .expect("cockpit terminal lock poisoned")
        .terminal
        .size()?;
    let (rows, cols) = app
        .agent_terminal_size_for(size.width, size.height)
        .unwrap_or((16, 72));
    let mut emulator = TerminalEmulator::new(rows, cols, TERMINAL_SCROLLBACK);
    let mut attach = open_agent_attach(&client, attached_session_id, rows, cols).await?;
    apply_agent_input_ownership(&mut app, attach.read_only);
    sync_agent_terminal_to_interactive_size(&mut app, &mut emulator, rows, cols);
    sync_agent_attach_size(&mut attach, rows, cols).await?;
    let mut last_refresh = Instant::now();
    let mut redraw = RedrawGate::new(Instant::now());
    let mut last_size = (rows, cols);
    terminal
        .lock()
        .expect("cockpit terminal lock poisoned")
        .terminal
        .draw(|frame| render_app(frame, &app))?;

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
                match reopen_agent_preview_after_loss(
                    &client,
                    attached_session_id,
                    last_size,
                    &mut app,
                )
                .await
                {
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
                match reopen_agent_preview_after_loss(
                    &client,
                    attached_session_id,
                    last_size,
                    &mut app,
                )
                .await
                {
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

        let mut key_action_completed = false;
        if event::poll(redraw.event_wait())? {
            match event::read()? {
                Event::Key(event) => {
                    let should_exit = handle_cockpit_key(
                        &client,
                        &mut app,
                        &terminal,
                        CockpitAttachState {
                            attach: &mut attach,
                            emulator: &mut emulator,
                            attached_session_id: &mut attached_session_id,
                            terminal_size: last_size,
                        },
                        event,
                    )
                    .await?;
                    redraw.mark_dirty();
                    if should_exit {
                        break;
                    }
                    key_action_completed = true;
                }
                Event::Paste(text) => {
                    handle_cockpit_paste(&client, &mut app, &mut attach, attached_session_id, text)
                        .await?;
                    redraw.mark_dirty();
                }
                Event::Resize(_, _) => {
                    let resized = sync_agent_geometry_from_terminal(
                        &mut app,
                        &terminal,
                        &mut attach,
                        &mut emulator,
                        &mut last_size,
                    )
                    .await?;
                    if resized {
                        redraw.mark_dirty();
                    }
                }
                _ => {}
            }
        }

        if key_action_completed {
            draw_cockpit_if_due(&terminal, &app, &mut redraw)?;
        }

        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            refresh_daemon_sessions(&client, &mut app).await?;
            refresh_logs(&client, &mut app).await?;
            if sync_agent_geometry_from_terminal(
                &mut app,
                &terminal,
                &mut attach,
                &mut emulator,
                &mut last_size,
            )
            .await?
            {
                redraw.mark_dirty();
            }
            last_refresh = Instant::now();
            redraw.mark_dirty();
        }

        draw_cockpit_if_due(&terminal, &app, &mut redraw)?;
    }

    let _ = attach.writer.write_frame(&AttachStreamFrame::Close).await;
    record_managed_raw_test_phase("preview_close_sent");
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
        match try_open_agent_attach(client, session_id, rows, cols, deadline).await {
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
    deadline: Instant,
) -> Result<OpenedAgentAttach, ClientError> {
    let request = agent_attach_request(session_id, false, rows, cols);
    match attach_before_deadline(client, &request, deadline).await {
        Ok(connection) => Ok(opened_agent_attach(connection, false)),
        Err(ClientError::Control(error)) if error.code == ControlErrorCode::InputOwnerConflict => {
            let connection = attach_before_deadline(
                client,
                &agent_attach_request(session_id, true, rows, cols),
                deadline,
            )
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
    agent_attach_request_with_replay(
        session_id,
        read_only,
        rows,
        cols,
        AttachReplayMode::TerminalSnapshot,
    )
}

fn agent_attach_request_with_replay(
    session_id: SessionId,
    read_only: bool,
    rows: u16,
    cols: u16,
    replay: AttachReplayMode,
) -> SessionAttachRequest {
    let mut accepted_frame_types = vec![
        AttachFrameType::RawOutput,
        AttachFrameType::StreamLagged,
        AttachFrameType::SnapshotUnavailable,
        AttachFrameType::ScreenSnapshot,
    ];
    if !read_only {
        accepted_frame_types.insert(1, AttachFrameType::RawInput);
    }
    SessionAttachRequest {
        selector: SessionSelector::Id { session_id },
        read_only,
        replay,
        requested_terminal_size: Some(TerminalDimensions::new(rows, cols)),
        client_protocol_version: Some(M2_ATTACH_PROTOCOL_VERSION),
        accepted_frame_types,
        stream_encoding: Some(AttachStreamEncoding::RawBytes),
        initial_replay: Some(match replay {
            AttachReplayMode::None => AttachInitialReplay::None,
            AttachReplayMode::TerminalSnapshot => AttachInitialReplay::ScreenSnapshot,
            AttachReplayMode::LineScrollback | AttachReplayMode::RawReplay => {
                AttachInitialReplay::RawReplay
            }
        }),
    }
}

fn managed_raw_attach_request(session_id: SessionId, rows: u16, cols: u16) -> SessionAttachRequest {
    SessionAttachRequest {
        selector: SessionSelector::Id { session_id },
        read_only: false,
        replay: AttachReplayMode::RawReplay,
        requested_terminal_size: Some(TerminalDimensions::new(rows, cols)),
        client_protocol_version: Some(M2_ATTACH_PROTOCOL_VERSION),
        accepted_frame_types: vec![
            AttachFrameType::RawOutput,
            AttachFrameType::RawInput,
            AttachFrameType::StreamLagged,
            AttachFrameType::SnapshotUnavailable,
        ],
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

async fn reopen_agent_attach_with_snapshot(
    client: &SessionControlClient,
    session_id: SessionId,
    rows: u16,
    cols: u16,
    emulator: &mut TerminalEmulator,
    deadline: Instant,
    interrupt: &mut tokio::signal::unix::Signal,
) -> Result<OpenedAgentAttach, CockpitError> {
    let request = agent_attach_request(session_id, false, rows, cols);
    for attempt in 0..RAW_REPLAY_ATTEMPTS {
        let connection =
            open_exclusive_attach_interruptible(client, &request, deadline, interrupt).await?;
        let mut attach = opened_agent_attach(connection, false);
        let frame = await_managed_transition(
            async {
                #[cfg(debug_assertions)]
                if env::var_os("MILLMUX_TEST_MANAGED_RAW_RETURN_PREVIEW_STALL").is_some() {
                    record_managed_raw_test_phase("return_preview_recovery_waiting");
                    std::future::pending::<()>().await;
                }
                attach
                    .reader
                    .next_frame()
                    .await
                    .map_err(|error| error.to_string())
            },
            deadline,
            interrupt,
            "return preview recovery",
        )
        .await
        .map_err(CockpitError::Transition)?
        .ok_or_else(|| {
            CockpitError::Transition(
                "return preview closed before its terminal snapshot".to_string(),
            )
        })?;
        #[cfg(debug_assertions)]
        let frame = if env::var_os("MILLMUX_TEST_MANAGED_RAW_SNAPSHOT_UNAVAILABLE").is_some() {
            AttachStreamFrame::SnapshotUnavailable {
                reason: millrace_sessions_core::protocol::SnapshotUnavailableReason::StaleSnapshot,
                details: None,
            }
        } else {
            frame
        };

        match frame {
            AttachStreamFrame::ScreenSnapshot { snapshot } => {
                snapshot.validate_for_wire().map_err(|error| {
                    CockpitError::Transition(format!(
                        "return preview supplied an invalid terminal snapshot: {error:?}"
                    ))
                })?;
                let snapshot_size_matches = snapshot.rows == rows && snapshot.cols == cols;
                let replay_reaches_requested_size = !snapshot_size_matches
                    && snapshot.source.pty_log_offset < snapshot.source.raw_replay_end_offset;
                if !snapshot_size_matches && !replay_reaches_requested_size {
                    close_attach_stream_interruptible(
                        &mut attach.reader,
                        &mut attach.writer,
                        deadline,
                        interrupt,
                        "mismatched return preview close",
                    )
                    .await?;
                    if attempt + 1 < RAW_REPLAY_ATTEMPTS {
                        wait_for_attach_retry(interrupt, deadline).await?;
                        continue;
                    }
                    return Err(CockpitError::Transition(format!(
                        "return preview snapshot size {}x{} did not match {}x{}",
                        snapshot.rows, snapshot.cols, rows, cols
                    )));
                }
                emulator.adopt_screen_snapshot(&snapshot);
                if replay_reaches_requested_size {
                    let suffix = await_managed_transition(
                        async {
                            attach
                                .reader
                                .next_frame()
                                .await
                                .map_err(|error| error.to_string())
                        },
                        deadline,
                        interrupt,
                        "return preview terminal suffix",
                    )
                    .await
                    .map_err(CockpitError::Transition)?
                    .ok_or_else(|| {
                        CockpitError::Transition(
                            "return preview closed before its terminal suffix".to_string(),
                        )
                    })?;
                    match suffix {
                        AttachStreamFrame::RawOutput { data } => {
                            emulator.process(data.as_slice());
                            emulator.resize(rows, cols);
                        }
                        AttachStreamFrame::StreamLagged {
                            reason, recover, ..
                        } => {
                            let _ = attach.writer.shutdown().await;
                            return Err(CockpitError::Transition(format!(
                                "return preview lagged before completing its terminal suffix: {reason:?}; {recover}"
                            )));
                        }
                        frame => {
                            let _ = attach.writer.shutdown().await;
                            return Err(CockpitError::Transition(format!(
                                "return preview supplied an incompatible terminal suffix: {frame:?}"
                            )));
                        }
                    }
                }
                return Ok(attach);
            }
            AttachStreamFrame::SnapshotUnavailable { reason, .. } => {
                close_attach_stream_interruptible(
                    &mut attach.reader,
                    &mut attach.writer,
                    deadline,
                    interrupt,
                    "unavailable return preview close",
                )
                .await?;
                if attempt + 1 < RAW_REPLAY_ATTEMPTS {
                    wait_for_attach_retry(interrupt, deadline).await?;
                    continue;
                }
                return Err(CockpitError::Transition(format!(
                    "return preview terminal snapshot is unavailable: {reason:?}"
                )));
            }
            AttachStreamFrame::StreamLagged { .. } => {
                let _ = attach.writer.shutdown().await;
                return Err(CockpitError::Transition(
                    "return preview lagged before its terminal snapshot".to_string(),
                ));
            }
            frame => {
                let _ = attach.writer.shutdown().await;
                return Err(CockpitError::Transition(format!(
                    "return preview supplied an incompatible initial frame: {frame:?}"
                )));
            }
        }
    }
    unreachable!("snapshot retry loop returns on its final attempt")
}

async fn close_attach_stream_interruptible(
    reader: &mut AttachReader,
    writer: &mut AttachWriter,
    deadline: Instant,
    interrupt: &mut tokio::signal::unix::Signal,
    stage: &str,
) -> Result<(), CockpitError> {
    await_managed_transition(
        async {
            close_attach_stream_without_output(reader, writer)
                .await
                .map_err(|error| error.to_string())
        },
        deadline,
        interrupt,
        stage,
    )
    .await
    .map_err(CockpitError::Transition)
}

fn raw_replay_retry_allowed(attempt: usize, error: &AttachError) -> bool {
    attempt + 1 < RAW_REPLAY_ATTEMPTS && matches!(error, AttachError::SnapshotUnavailable(_))
}

fn spawn_managed_raw_waiter(
    operation: &crate::attach::ManagedRawAttachOperation,
) -> tokio::task::JoinHandle<Result<ManagedRawAttachExit, AttachError>> {
    let waiter = operation.waiter();
    tokio::spawn(async move {
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        waiter.run(&mut interrupt).await
    })
}

async fn run_managed_raw_attach_with_replay_retry(
    client: SessionControlClient,
    request: SessionAttachRequest,
    deadline: Instant,
    terminal_cleanup: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
) -> Result<ManagedRawAttachExit, AttachError> {
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    for attempt in 0..RAW_REPLAY_ATTEMPTS {
        let connection =
            open_exclusive_attach_interruptible(&client, &request, deadline, &mut interrupt)
                .await?;
        let mut operation = prepare_managed_raw_attach(connection, &request).await?;
        *terminal_cleanup
            .lock()
            .expect("managed terminal cleanup lock poisoned") =
            Some(operation.take_terminal_cleanup_receiver());
        let caller = spawn_managed_raw_waiter(&operation);
        let result = match caller.await {
            Ok(result) => result,
            Err(error) if error.is_panic() => {
                let cleanup = operation.cancel_and_join().await;
                drop(operation);
                cleanup?;
                std::panic::resume_unwind(error.into_panic());
            }
            Err(error) => {
                let cleanup = operation.cancel_and_join().await;
                cleanup?;
                Err(AttachError::Stream(format!(
                    "managed raw caller task failed: {error}"
                )))
            }
        };
        drop(operation);
        if let Err(error) = &result {
            if raw_replay_retry_allowed(attempt, error) {
                wait_for_attach_retry(&mut interrupt, deadline).await?;
                continue;
            }
        }
        return result;
    }
    unreachable!("managed raw retry loop returns on its final attempt")
}

struct ManagedRawTransitionOutcome {
    result: Result<ManagedRawAttachExit, AttachError>,
    resume: Result<(), CockpitError>,
}

struct SuspendedManagedRawTransition {
    body_abort: tokio::task::AbortHandle,
    cleanup_owner: Option<tokio::task::JoinHandle<ManagedRawTransitionOutcome>>,
}

impl SuspendedManagedRawTransition {
    fn spawn(
        suspension: TerminalSuspension,
        client: SessionControlClient,
        request: SessionAttachRequest,
        deadline: Instant,
    ) -> Self {
        let terminal_cleanup = Arc::new(Mutex::new(None));
        let body_cleanup = Arc::clone(&terminal_cleanup);
        let body = tokio::spawn(async move {
            run_managed_raw_attach_with_replay_retry(client, request, deadline, body_cleanup).await
        });
        let body_abort = body.abort_handle();
        let cleanup_owner = tokio::spawn(async move {
            let joined = body.await;
            let terminal_cleanup = terminal_cleanup
                .lock()
                .expect("managed terminal cleanup lock poisoned")
                .take();
            if let Some(terminal_cleanup) = terminal_cleanup {
                let _ = terminal_cleanup.await;
            }
            record_managed_raw_test_phase("terminal_resume_started");
            let resume = suspension.resume();
            record_managed_raw_test_phase("terminal_resume_complete");
            let result = match joined {
                Ok(result) => result,
                Err(error) if error.is_cancelled() => Err(AttachError::Cancelled),
                Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
                Err(error) => Err(AttachError::Stream(format!(
                    "managed raw transition task failed: {error}"
                ))),
            };
            ManagedRawTransitionOutcome { result, resume }
        });
        Self {
            body_abort,
            cleanup_owner: Some(cleanup_owner),
        }
    }

    async fn join(&mut self) -> ManagedRawTransitionOutcome {
        let joined = self
            .cleanup_owner
            .take()
            .expect("managed transition cleanup owner already joined")
            .await;
        match joined {
            Ok(outcome) => outcome,
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(error) => ManagedRawTransitionOutcome {
                result: Err(AttachError::Stream(format!(
                    "managed raw cleanup owner failed: {error}"
                ))),
                resume: Err(CockpitError::Transition(
                    "managed raw cleanup owner could not restore the terminal".to_string(),
                )),
            },
        }
    }
}

impl Drop for SuspendedManagedRawTransition {
    fn drop(&mut self) {
        self.body_abort.abort();
    }
}

#[cfg(debug_assertions)]
async fn wait_for_managed_raw_test_phase(phase: &str, deadline: Instant) -> bool {
    let Some(path) = env::var_os("MILLMUX_TEST_MANAGED_RAW_PHASE_FILE") else {
        return false;
    };
    while Instant::now() < deadline {
        if std::fs::read_to_string(&path)
            .is_ok_and(|contents| contents.lines().any(|line| line == phase))
        {
            return true;
        }
        tokio::time::sleep(ATTACH_DRAIN_INTERVAL).await;
    }
    false
}

async fn attach_before_deadline(
    client: &SessionControlClient,
    request: &SessionAttachRequest,
    deadline: Instant,
) -> Result<AttachConnection, ClientError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(attach_response_timeout());
    }
    tokio::time::timeout(remaining, client.attach(request))
        .await
        .map_err(|_| attach_response_timeout())?
}

async fn open_exclusive_attach_interruptible(
    client: &SessionControlClient,
    request: &SessionAttachRequest,
    deadline: Instant,
    interrupt: &mut tokio::signal::unix::Signal,
) -> Result<AttachConnection, AttachError> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(AttachError::Client(attach_response_timeout()));
        }
        let attempt = async {
            #[cfg(debug_assertions)]
            if env::var_os("MILLMUX_TEST_MANAGED_RAW_ATTACH_RESPONSE_STALL").is_some() {
                env::remove_var("MILLMUX_TEST_MANAGED_RAW_ATTACH_RESPONSE_STALL");
                record_managed_raw_test_phase("raw_attach_negotiation_waiting");
                std::future::pending::<()>().await;
            }
            client.attach(request).await
        };
        let result = tokio::select! {
            signal = interrupt.recv() => {
                return if signal.is_some() {
                    Err(AttachError::Cancelled)
                } else {
                    Err(AttachError::Stream("SIGINT listener closed".to_string()))
                };
            }
            result = tokio::time::timeout(remaining, attempt) => {
                result.map_err(|_| AttachError::Client(attach_response_timeout()))?
            }
        };
        match result {
            Ok(connection) if connection.result.stream.input_owner => return Ok(connection),
            Ok(_) => {
                return Err(AttachError::Client(ClientError::Protocol(
                    "exclusive attach negotiation did not grant input ownership".to_string(),
                )));
            }
            Err(ClientError::Control(error))
                if error.code == ControlErrorCode::InputOwnerConflict
                    && Instant::now() < deadline =>
            {
                wait_for_attach_retry(interrupt, deadline).await?;
            }
            Err(error) => return Err(AttachError::Client(error)),
        }
    }
}

async fn wait_for_attach_retry(
    interrupt: &mut tokio::signal::unix::Signal,
    deadline: Instant,
) -> Result<(), AttachError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(AttachError::Client(attach_response_timeout()));
    }
    tokio::select! {
        signal = interrupt.recv() => {
            if signal.is_some() {
                Err(AttachError::Cancelled)
            } else {
                Err(AttachError::Stream("SIGINT listener closed".to_string()))
            }
        }
        () = tokio::time::sleep(remaining.min(SNAPSHOT_SEED_RETRY_INTERVAL)) => Ok(()),
    }
}

fn attach_response_timeout() -> ClientError {
    ClientError::Io(io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "attach response timed out after {}ms",
            SNAPSHOT_SEED_TIMEOUT.as_millis()
        ),
    ))
}
async fn reopen_agent_preview_after_loss(
    client: &SessionControlClient,
    session_id: SessionId,
    terminal_size: (u16, u16),
    app: &mut AppModel,
) -> Result<OpenedAgentAttach, CockpitError> {
    reopen_agent_attach(client, session_id, terminal_size, app).await
}

fn opened_agent_attach(connection: AttachConnection, read_only: bool) -> OpenedAgentAttach {
    let (result, reader, writer) = connection.split();
    OpenedAgentAttach {
        reader,
        writer,
        read_only: read_only || !result.stream.input_owner,
        raw_input: result.confirms_raw_input(),
        stream_id: result.stream.stream_id,
    }
}

async fn validate_managed_raw_attach_fresh(
    client: &SessionControlClient,
    session_id: SessionId,
    attach: &OpenedAgentAttach,
) -> Result<(), String> {
    #[cfg(debug_assertions)]
    if env::var_os("MILLMUX_TEST_MANAGED_RAW_STATUS_RESPONSE_STALL").is_some() {
        record_managed_raw_test_phase("raw_fresh_status_waiting");
        std::future::pending::<()>().await;
    }
    let response = client
        .status(&SessionInspectRequest {
            selector: SessionSelector::Id { session_id },
        })
        .await
        .map_err(|error| format!("host_inspect_failed: {error}"))?;

    #[cfg(debug_assertions)]
    if env::var_os("MILLMUX_TEST_MANAGED_RAW_STATUS_ERROR").is_some() {
        return Err("host_inspect_failed: injected validation error".to_string());
    }

    validate_managed_raw_host_state(
        &response.session,
        response.worker.as_ref(),
        session_id,
        &attach.stream_id,
        attach.read_only,
    )
    .map_err(str::to_string)
}

async fn await_managed_transition<T, F>(
    future: F,
    deadline: Instant,
    interrupt: &mut tokio::signal::unix::Signal,
    stage: &str,
) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, String>>,
{
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(format!("{stage} timed out before managed raw attach"));
    }
    tokio::select! {
        result = future => result,
        () = tokio::time::sleep(remaining) => {
            Err(format!("{stage} timed out before managed raw attach"))
        }
        signal = interrupt.recv() => {
            if signal.is_some() {
                #[cfg(debug_assertions)]
                if stage == "fresh status" {
                    record_managed_raw_test_phase("raw_fresh_status_cancelled");
                }
                Err(format!("{stage} cancelled by external SIGINT"))
            } else {
                Err(format!("{stage} SIGINT listener closed"))
            }
        }
    }
}

fn validate_managed_raw_host_state(
    session: &SessionSummary,
    worker: Option<&WorkerMeta>,
    expected_session_id: SessionId,
    expected_stream_id: &str,
    preview_read_only: bool,
) -> Result<(), &'static str> {
    if preview_read_only {
        return Err("preview_read_only");
    }
    if session.session_id != expected_session_id {
        return Err("host_session_mismatch");
    }
    if session.process_state != ProcessState::Running {
        return Err("host_session_not_running");
    }
    if session.spawn_mode != SpawnMode::Pty {
        return Err("host_session_not_pty");
    }
    if !session.capabilities.attach || !session.capabilities.raw_attach {
        return Err("host_raw_attach_unavailable");
    }
    if session.liveness.worker != LivenessState::Alive {
        return Err("host_worker_not_alive");
    }
    if session.liveness.child != LivenessState::Alive {
        return Err("host_child_not_alive");
    }
    if session.attached_clients == 0 || session.input_owner.as_deref() != Some(expected_stream_id) {
        return Err("host_input_owner_changed");
    }

    let worker = worker.ok_or("host_worker_missing")?;
    if worker.session_id != expected_session_id
        || worker.spawn_mode != SpawnMode::Pty
        || worker.process_state != ProcessState::Running
    {
        return Err("host_worker_mismatch");
    }
    if worker.attached_clients == 0 || worker.input_owner.as_deref() != Some(expected_stream_id) {
        return Err("host_worker_input_owner_changed");
    }
    Ok(())
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

async fn sync_agent_geometry_from_terminal(
    app: &mut AppModel,
    terminal: &Arc<Mutex<TerminalSession>>,
    attach: &mut OpenedAgentAttach,
    emulator: &mut TerminalEmulator,
    last_size: &mut (u16, u16),
) -> Result<bool, CockpitError> {
    let size = terminal
        .lock()
        .expect("cockpit terminal lock poisoned")
        .terminal
        .size()?;
    let Some((rows, cols)) = app.agent_terminal_size_for(size.width, size.height) else {
        return Ok(false);
    };
    if (rows, cols) == *last_size {
        return Ok(false);
    }
    sync_agent_terminal_to_interactive_size(app, emulator, rows, cols);
    sync_agent_attach_size(attach, rows, cols).await?;
    *last_size = (rows, cols);
    Ok(true)
}

async fn switch_agent_attach(
    client: &SessionControlClient,
    app: &mut AppModel,
    attach: &mut OpenedAgentAttach,
    emulator: &mut TerminalEmulator,
    session_id: SessionId,
    terminal_size: (u16, u16),
) -> Result<(), CockpitError> {
    let (rows, cols) = terminal_size;
    let mut next_attach = open_agent_attach(client, session_id, rows, cols).await?;
    sync_agent_attach_size(&mut next_attach, rows, cols).await?;
    let _ = attach.writer.write_frame(&AttachStreamFrame::Close).await;
    *emulator = TerminalEmulator::new(rows, cols, TERMINAL_SCROLLBACK);
    app.reset_agent_terminal(rows, cols, !next_attach.read_only, next_attach.read_only);
    apply_agent_input_ownership(app, next_attach.read_only);
    *attach = next_attach;
    app.set_host_connected();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn enter_managed_raw_attach(
    client: &SessionControlClient,
    app: &mut AppModel,
    terminal: &Arc<Mutex<TerminalSession>>,
    attach: &mut OpenedAgentAttach,
    emulator: &mut TerminalEmulator,
    session_id: SessionId,
    pane_size: (u16, u16),
    deadline: Instant,
    interrupt: &mut tokio::signal::unix::Signal,
) -> Result<(), CockpitError> {
    app.status_message = "raw attach: Ctrl-] d returns to cockpit".to_string();
    #[cfg(debug_assertions)]
    if env::var_os("MILLMUX_TEST_MANAGED_RAW_ENTER_EVENT_ERROR").is_some() {
        app.status_message =
            "raw attach rejected before terminal suspension: injected entry event error"
                .to_string();
        return Ok(());
    }
    let persist_entry = async {
        record_ui_event(
            client,
            app,
            UiEventKind::RawInputModeEntered,
            "managed raw attach entered",
            bound_fields("session_id", Some(session_id)),
        )
        .await
        .map_err(|error| format!("entry event: {error}"))
    };
    if let Err(error) =
        await_managed_transition(persist_entry, deadline, interrupt, "entry persistence").await
    {
        app.status_message = format!("raw attach rejected before terminal suspension: {error}");
        return Ok(());
    }

    let mut transition_errors = Vec::new();
    let mut preview_closed = false;
    let preview_close = await_managed_transition(
        async {
            close_attach_stream(
                &mut attach.reader,
                &mut attach.writer,
                &mut |event| match event {
                    ManagedRawAttachEvent::Output { bytes } => {
                        emulator.process(bytes);
                        Ok(())
                    }
                },
            )
            .await
            .map_err(|error| error.to_string())
        },
        deadline,
        interrupt,
        "preview close",
    )
    .await;
    match preview_close {
        Ok(()) => preview_closed = true,
        Err(error) => {
            transition_errors.push(format!("preview close: {error}"));
            if let Err(error) = attach.writer.shutdown().await {
                transition_errors.push(format!("preview transport release: {error}"));
            }
        }
    }

    let mut raw_result = None;
    let mut resume_failed = false;
    if preview_closed {
        let outer_size = terminal
            .lock()
            .expect("cockpit terminal lock poisoned")
            .terminal
            .size();
        match outer_size {
            Ok(outer_size) => {
                let request =
                    managed_raw_attach_request(session_id, outer_size.height, outer_size.width);
                match TerminalSession::suspend(terminal) {
                    Ok(suspension) => {
                        let mut transition = SuspendedManagedRawTransition::spawn(
                            suspension,
                            client.clone(),
                            request,
                            deadline,
                        );
                        #[cfg(debug_assertions)]
                        let drop_transition = env::var_os("MILLMUX_TEST_MANAGED_RAW_ABORT_OUTER")
                            .is_some()
                            && wait_for_managed_raw_test_phase("raw_loop_entered", deadline).await;
                        #[cfg(not(debug_assertions))]
                        let drop_transition = false;
                        if drop_transition {
                            #[cfg(debug_assertions)]
                            {
                                drop(transition);
                                record_managed_raw_test_phase("raw_outer_transition_dropped");
                                if !wait_for_managed_raw_test_phase(
                                    "terminal_resume_complete",
                                    deadline,
                                )
                                .await
                                {
                                    transition_errors.push(
                                        "managed raw cleanup did not restore the terminal before the transition deadline"
                                            .to_string(),
                                    );
                                }
                                resume_failed = !terminal
                                    .lock()
                                    .expect("cockpit terminal lock poisoned")
                                    .cockpit_display_active;
                                raw_result = Some(Err(AttachError::Cancelled));
                                record_managed_raw_test_phase(
                                    "raw_outer_transition_cleanup_joined",
                                );
                            }
                        } else {
                            let outcome = transition.join().await;
                            if let Err(error) = outcome.resume {
                                resume_failed = true;
                                transition_errors.push(format!("terminal resume: {error}"));
                            }
                            raw_result = Some(outcome.result);
                        }
                    }
                    Err(error) => {
                        resume_failed = true;
                        transition_errors.push(format!("terminal suspend: {error}"));
                    }
                }
            }
            Err(error) => transition_errors.push(format!("terminal size: {error}")),
        }
    }

    if raw_result.is_some() {
        *interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    }

    let return_size = terminal
        .lock()
        .expect("cockpit terminal lock poisoned")
        .terminal
        .size()
        .ok()
        .and_then(|size| app.agent_terminal_size_for(size.width, size.height))
        .unwrap_or(pane_size);
    let recovery_deadline = Instant::now() + SNAPSHOT_SEED_TIMEOUT;
    let mut preview_restored = false;
    match reopen_agent_attach_with_snapshot(
        client,
        session_id,
        return_size.0,
        return_size.1,
        emulator,
        recovery_deadline,
        interrupt,
    )
    .await
    {
        Ok(next_attach) => {
            app.resize_agent_terminal(return_size.0, return_size.1);
            app.update_agent_terminal_view(emulator.snapshot(), emulator.is_following());
            *attach = next_attach;
            apply_agent_input_ownership(app, false);
            app.set_host_connected();
            preview_restored = true;
            record_managed_raw_test_phase("preview_reopened");
        }
        Err(error) => {
            transition_errors.push(format!("terminal snapshot preview reopen: {error}"));
        }
    }

    let (mut status, reason) = match raw_result {
        Some(Ok(ManagedRawAttachExit::LocalDetach)) => (
            "returned from raw attach".to_string(),
            "local_detach".to_string(),
        ),
        Some(Ok(ManagedRawAttachExit::RemoteClosed)) => (
            "raw attach closed remotely; cockpit restored".to_string(),
            "remote_close".to_string(),
        ),
        Some(Ok(ManagedRawAttachExit::InputEof)) => (
            "raw attach input closed; cockpit restored".to_string(),
            "input_eof".to_string(),
        ),
        Some(Err(error)) => (
            format!("raw attach error; cockpit restored: {error}"),
            "error".to_string(),
        ),
        None => (
            "raw attach could not start; cockpit restoration attempted".to_string(),
            "transition_error".to_string(),
        ),
    };
    if !transition_errors.is_empty() {
        status.push_str("; ");
        status.push_str(&transition_errors.join("; "));
    }
    app.status_message = status;
    let mut fields = bound_fields("session_id", Some(session_id));
    fields.insert("reason".to_string(), reason);
    if let Err(error) = record_ui_event(
        client,
        app,
        UiEventKind::RawInputModeExited,
        "managed raw attach exited",
        fields,
    )
    .await
    {
        if !app.status_message.is_empty() {
            app.status_message.push_str("; ");
        }
        app.status_message.push_str(&format!("exit event: {error}"));
    }

    if raw_resume_failure_is_fatal(
        resume_failed,
        terminal
            .lock()
            .expect("cockpit terminal lock poisoned")
            .cockpit_display_active,
    ) {
        return Err(CockpitError::Transition(
            "cockpit terminal display could not be restored after raw attach".to_string(),
        ));
    }
    if !preview_restored {
        return Err(CockpitError::Transition(format!(
            "managed raw return could not restore a coherent terminal snapshot: {}",
            app.status_message
        )));
    }

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
    raw_input: bool,
    stream_id: String,
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

fn draw_cockpit_if_due(
    terminal: &Arc<Mutex<TerminalSession>>,
    app: &AppModel,
    redraw: &mut RedrawGate,
) -> Result<(), CockpitError> {
    let now = Instant::now();
    if redraw.should_draw(now) {
        terminal
            .lock()
            .expect("cockpit terminal lock poisoned")
            .terminal
            .draw(|frame| render_app(frame, app))?;
        redraw.mark_drawn(now);
    }
    Ok(())
}

fn agent_input_frame(raw_input: bool, text: String) -> AttachStreamFrame {
    if raw_input {
        AttachStreamFrame::raw_input(text.into_bytes())
    } else {
        AttachStreamFrame::Input { text }
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
            emulator.process(text.as_bytes());
            app.update_agent_terminal_view(emulator.snapshot(), emulator.is_following());
        }
        AttachStreamFrame::RawOutput { data } => {
            emulator.process(data.as_slice());
            app.update_agent_terminal_view(emulator.snapshot(), emulator.is_following());
        }
        AttachStreamFrame::ScreenSnapshot { snapshot } => {
            if let Err(error) = snapshot.validate_for_wire() {
                app.set_host_reconnecting(1, format!("{error:?}"));
                return false;
            }
            emulator.adopt_screen_snapshot(&snapshot);
            app.update_agent_terminal_view(emulator.snapshot(), emulator.is_following());
        }
        AttachStreamFrame::StreamLagged { .. } | AttachStreamFrame::SnapshotUnavailable { .. } => {
            app.set_host_reconnecting(1, "agent attach replay coverage was lost".to_string());
            return false;
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

struct CockpitAttachState<'a> {
    attach: &'a mut OpenedAgentAttach,
    emulator: &'a mut TerminalEmulator,
    attached_session_id: &'a mut SessionId,
    terminal_size: (u16, u16),
}

async fn handle_cockpit_key(
    client: &SessionControlClient,
    app: &mut AppModel,
    terminal: &Arc<Mutex<TerminalSession>>,
    attach_state: CockpitAttachState<'_>,
    event: KeyEvent,
) -> Result<bool, CockpitError> {
    if app.daemon_switcher.open {
        let previous_daemon = app.active_daemon_session_id;
        if let Some(session_id) = handle_daemon_switcher_key(app, event) {
            match app.workspace_session_selection(session_id) {
                WorkspaceSessionSelection::AttachSelected(session_id)
                    if *attach_state.attached_session_id != session_id =>
                {
                    match switch_agent_attach(
                        client,
                        app,
                        &mut *attach_state.attach,
                        &mut *attach_state.emulator,
                        session_id,
                        attach_state.terminal_size,
                    )
                    .await
                    {
                        Ok(()) => {
                            let _ = app.select_workspace_session(session_id);
                            mark_unread_attention_read_for_session(client, app, session_id).await;
                            *attach_state.attached_session_id = session_id;
                            record_ui_event(
                                client,
                                app,
                                UiEventKind::AgentSessionBound,
                                "agent session switched",
                                bound_fields("agent_session_id", Some(session_id)),
                            )
                            .await?;
                        }
                        Err(error) => {
                            app.status_message = format!("session attach failed: {error}");
                        }
                    }
                }
                WorkspaceSessionSelection::DaemonSelected(session_id) => {
                    let _ = app.select_workspace_session(session_id);
                    mark_unread_attention_read_for_session(client, app, session_id).await;
                }
                WorkspaceSessionSelection::AttachSelected(session_id) => {
                    let _ = app.select_workspace_session(session_id);
                    mark_unread_attention_read_for_session(client, app, session_id).await;
                }
                WorkspaceSessionSelection::NotAttachable(session_id) => {
                    app.status_message = format!("session not attachable {session_id}");
                }
                WorkspaceSessionSelection::Missing => {
                    app.status_message = "session missing".to_string();
                }
            }
        }
        if app.active_daemon_session_id != previous_daemon {
            record_active_daemon_changed(client, app).await?;
        }
        return Ok(false);
    }

    let previous_daemon = app.active_daemon_session_id;
    let attached_agent_was_focused =
        cockpit_attached_terminal_focused(app, *attach_state.attached_session_id);
    let search_was_active = app.search_mode;
    let viewport_height = terminal
        .lock()
        .expect("cockpit terminal lock poisoned")
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
        KeyAction::ManagedRawAttach => {
            match app.managed_raw_attach_target(*attach_state.attached_session_id) {
                Ok(session_id) => {
                    let deadline = Instant::now() + SNAPSHOT_SEED_TIMEOUT;
                    let mut interrupt =
                        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
                    let validation = await_managed_transition(
                        validate_managed_raw_attach_fresh(client, session_id, attach_state.attach),
                        deadline,
                        &mut interrupt,
                        "fresh status",
                    )
                    .await;
                    match validation {
                        Ok(()) => {
                            enter_managed_raw_attach(
                                client,
                                app,
                                terminal,
                                &mut *attach_state.attach,
                                &mut *attach_state.emulator,
                                session_id,
                                attach_state.terminal_size,
                                deadline,
                                &mut interrupt,
                            )
                            .await?;
                        }
                        Err(reason) => {
                            app.status_message = format!("raw attach rejected: {reason}");
                        }
                    }
                }
                Err(reason) => {
                    app.status_message = format!("raw attach rejected: {reason}");
                }
            }
        }
        KeyAction::Redraw => {
            terminal
                .lock()
                .expect("cockpit terminal lock poisoned")
                .recover_display()?;
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
        KeyAction::ScrollUp if attached_agent_was_focused => {
            attach_state.emulator.scroll_up(1);
            app.update_agent_terminal_view(
                attach_state.emulator.snapshot(),
                attach_state.emulator.is_following(),
            );
        }
        KeyAction::ScrollDown if attached_agent_was_focused => {
            attach_state.emulator.scroll_down(1);
            app.update_agent_terminal_view(
                attach_state.emulator.snapshot(),
                attach_state.emulator.is_following(),
            );
        }
        KeyAction::PageUp if attached_agent_was_focused => {
            attach_state.emulator.page_up(viewport_height);
            app.update_agent_terminal_view(
                attach_state.emulator.snapshot(),
                attach_state.emulator.is_following(),
            );
        }
        KeyAction::PageDown if attached_agent_was_focused => {
            attach_state.emulator.page_down(viewport_height);
            app.update_agent_terminal_view(
                attach_state.emulator.snapshot(),
                attach_state.emulator.is_following(),
            );
        }
        KeyAction::JumpTop if attached_agent_was_focused => {
            attach_state.emulator.jump_top();
            app.update_agent_terminal_view(
                attach_state.emulator.snapshot(),
                attach_state.emulator.is_following(),
            );
        }
        KeyAction::SearchInput(_) | KeyAction::SearchBackspace if attached_agent_was_focused => {
            sync_agent_search_from_emulator(
                app,
                &mut *attach_state.emulator,
                TerminalSearchDirection::First,
                "search",
            );
        }
        KeyAction::NextSearch if attached_agent_was_focused => {
            sync_agent_search_from_emulator(
                app,
                &mut *attach_state.emulator,
                TerminalSearchDirection::Next,
                "search next",
            );
        }
        KeyAction::PreviousSearch if attached_agent_was_focused => {
            sync_agent_search_from_emulator(
                app,
                &mut *attach_state.emulator,
                TerminalSearchDirection::Previous,
                "search previous",
            );
        }
        KeyAction::Escape if search_was_active => {}
        KeyAction::ExitScrollMode | KeyAction::JumpBottom | KeyAction::Escape => {
            if attached_agent_was_focused {
                attach_state.emulator.jump_bottom();
                app.update_agent_terminal_view(
                    attach_state.emulator.snapshot(),
                    attach_state.emulator.is_following(),
                );
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
        KeyAction::Input(event) => {
            if let Some(text) =
                cockpit_key_input_text_for_attach(app, event, *attach_state.attached_session_id)
            {
                attach_state
                    .attach
                    .writer
                    .write_frame(&agent_input_frame(attach_state.attach.raw_input, text))
                    .await?;
            } else if !app.focused_attach_matches(*attach_state.attached_session_id) {
                app.status_message = "input rejected: pane_session_mismatch".to_string();
            }
        }
        _ => {}
    }
    if app.active_daemon_session_id != previous_daemon {
        record_active_daemon_changed(client, app).await?;
    }
    Ok(false)
}

fn sync_agent_search_from_emulator(
    app: &mut AppModel,
    emulator: &mut TerminalEmulator,
    direction: TerminalSearchDirection,
    label: &str,
) {
    if app.search_query.is_empty() {
        emulator.clear_search();
        app.set_agent_search_not_found(label);
        return;
    }

    if let Some(found) = emulator.search_scrollback(app.search_query.as_str(), direction) {
        app.update_agent_terminal_view(emulator.snapshot(), emulator.is_following());
        app.set_agent_search_match(label, &found);
    } else {
        app.set_agent_search_not_found(label);
    }
}

async fn handle_cockpit_paste(
    client: &SessionControlClient,
    app: &mut AppModel,
    attach: &mut OpenedAgentAttach,
    attached_session_id: SessionId,
    text: String,
) -> Result<(), CockpitError> {
    match cockpit_paste_decision_for_attach(app, &text, Some(attached_session_id)) {
        CockpitPasteDecision::Accepted {
            text,
            byte_count,
            bracketed,
        } => {
            for chunk in bounded_attach_input_chunks(&text) {
                attach
                    .writer
                    .write_frame(&agent_input_frame(attach.raw_input, chunk.to_string()))
                    .await?;
            }
            app.status_message = "paste sent".to_string();
            record_ui_event(
                client,
                app,
                UiEventKind::InputAccepted,
                "agent paste accepted",
                input_audit_fields(app, "paste", byte_count, true, None, bracketed),
            )
            .await?;
        }
        CockpitPasteDecision::Rejected {
            reason,
            byte_count,
            bracketed,
        } => {
            app.status_message = format!("paste rejected: {}", reason.as_str());
            record_ui_event(
                client,
                app,
                UiEventKind::InputRejected,
                "agent paste rejected",
                input_audit_fields(
                    app,
                    "paste",
                    byte_count,
                    false,
                    Some(reason.as_str()),
                    bracketed,
                ),
            )
            .await?;
        }
    }
    Ok(())
}

fn bounded_attach_input_chunks(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return vec![text];
    }

    let mut chunks = Vec::with_capacity(text.len().div_ceil(MAX_ATTACH_INPUT_FRAME_BYTES));
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + MAX_ATTACH_INPUT_FRAME_BYTES).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(&text[start..end]);
        start = end;
    }
    chunks
}

fn handle_daemon_switcher_key(app: &mut AppModel, event: KeyEvent) -> Option<SessionId> {
    match event.code {
        KeyCode::Esc => {
            app.close_daemon_switcher();
            None
        }
        KeyCode::Enter => app.activate_session_switcher_selection(),
        KeyCode::Up | KeyCode::Char('k') => {
            let _ = app.move_daemon_switcher_selection(-1);
            None
        }
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
            let _ = app.move_daemon_switcher_selection(1);
            None
        }
        _ => None,
    }
}

async fn mark_unread_attention_read_for_session(
    client: &SessionControlClient,
    app: &mut AppModel,
    session_id: SessionId,
) {
    match client
        .attention_read(&AttentionReadRequest {
            selector: SessionSelector::Id { session_id },
            item_id: None,
            kinds: vec![AttentionKind::Unread],
        })
        .await
    {
        Ok(response) if response.mutated_count > 0 => {
            app.status_message = format!("marked {} unread item(s) read", response.mutated_count);
        }
        Ok(_) => {}
        Err(error) => {
            app.status_message = format!("attention read failed: {error}");
        }
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
    #[cfg(debug_assertions)]
    if kind == UiEventKind::RawInputModeEntered
        && env::var_os("MILLMUX_TEST_MANAGED_RAW_UI_CONTEXT_SET_ERROR").is_some()
    {
        return Err(CockpitError::Transition(
            "injected RawInputModeEntered ui.context.set failure".to_string(),
        ));
    }
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

fn cockpit_accepts_agent_text_input(app: &AppModel) -> bool {
    !cockpit_overlay_accepts_input(app)
        && app.focused_agent_terminal()
        && app.agent_terminal_can_accept_input()
}

fn cockpit_attached_terminal_focused(app: &AppModel, attached_session_id: SessionId) -> bool {
    app.focused_attach_matches(attached_session_id)
}

fn cockpit_overlay_accepts_input(app: &AppModel) -> bool {
    app.daemon_switcher.open
        || app.command_palette.open
        || app.help_overlay.open
        || app.confirmation.is_some()
}

fn cockpit_key_input_text(app: &AppModel, event: KeyEvent) -> Option<String> {
    cockpit_accepts_agent_text_input(app)
        .then(|| key_event_to_text(event))
        .flatten()
}

fn cockpit_key_input_text_for_attach(
    app: &AppModel,
    event: KeyEvent,
    attached_session_id: SessionId,
) -> Option<String> {
    app.focused_attach_matches(attached_session_id)
        .then(|| cockpit_key_input_text(app, event))
        .flatten()
}

#[cfg(test)]
fn cockpit_paste_input_text(app: &AppModel, text: &str) -> Option<String> {
    match cockpit_paste_decision(app, text) {
        CockpitPasteDecision::Accepted { text, .. } => Some(text),
        CockpitPasteDecision::Rejected { .. } => None,
    }
}

#[cfg(test)]
fn cockpit_paste_decision(app: &AppModel, text: &str) -> CockpitPasteDecision {
    cockpit_paste_decision_for_attach(app, text, None)
}

fn cockpit_paste_decision_for_attach(
    app: &AppModel,
    text: &str,
    attached_session_id: Option<SessionId>,
) -> CockpitPasteDecision {
    let byte_count = text.len();
    let bracketed = paste_event_is_bracketed_or_multiline(text);
    if byte_count > MAX_COCKPIT_PASTE_BYTES {
        return CockpitPasteDecision::Rejected {
            reason: CockpitPasteRejectReason::TooLarge,
            byte_count,
            bracketed,
        };
    }
    if cockpit_overlay_accepts_input(app) {
        return CockpitPasteDecision::Rejected {
            reason: CockpitPasteRejectReason::OverlayActive,
            byte_count,
            bracketed,
        };
    }
    if !app.focused_agent_terminal() {
        return CockpitPasteDecision::Rejected {
            reason: CockpitPasteRejectReason::AgentUnfocused,
            byte_count,
            bracketed,
        };
    }
    if !app.agent_terminal_can_accept_input() {
        return CockpitPasteDecision::Rejected {
            reason: CockpitPasteRejectReason::ReadOnly,
            byte_count,
            bracketed,
        };
    }
    if attached_session_id.is_some_and(|session_id| !app.focused_attach_matches(session_id)) {
        return CockpitPasteDecision::Rejected {
            reason: CockpitPasteRejectReason::PaneSessionMismatch,
            byte_count,
            bracketed,
        };
    }

    CockpitPasteDecision::Accepted {
        text: paste_event_to_text(text),
        byte_count,
        bracketed,
    }
}

fn input_audit_fields(
    app: &AppModel,
    input_kind: &str,
    byte_count: usize,
    accepted: bool,
    reason: Option<&str>,
    bracketed: bool,
) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::from([
        ("sender_client".to_string(), "cockpit".to_string()),
        ("sender_ui_id".to_string(), app.ui_id.to_string()),
        ("target".to_string(), "agent_terminal".to_string()),
        ("input_kind".to_string(), input_kind.to_string()),
        ("byte_count".to_string(), byte_count.to_string()),
        ("accepted".to_string(), accepted.to_string()),
        ("bracketed".to_string(), bracketed.to_string()),
    ]);
    if let Some(session_id) = app.focused_attach_session_id().or(app.agent_session_id) {
        fields.insert("target_session_id".to_string(), session_id.to_string());
    }
    if let Some(pane_id) = app.active_pane_id {
        fields.insert("target_pane_id".to_string(), pane_id.to_string());
    }
    if let Some(reason) = reason {
        fields.insert("reason".to_string(), reason.to_string());
    }
    fields
}

fn key_event_to_text(event: KeyEvent) -> Option<String> {
    let mut modifiers = event.modifiers;
    if matches!(event.code, KeyCode::Char(_)) {
        modifiers.remove(KeyModifiers::SHIFT);
    }

    match event.code {
        KeyCode::Char(value) if modifiers == KeyModifiers::CONTROL => control_char(value),
        KeyCode::Char(value) if modifiers == KeyModifiers::ALT => Some(format!("\x1b{value}")),
        KeyCode::Char(value) if modifiers.is_empty() => Some(value.to_string()),
        KeyCode::Enter if modifiers.is_empty() => Some("\r".to_string()),
        KeyCode::Tab if modifiers.is_empty() => Some("\t".to_string()),
        KeyCode::Backspace if modifiers.is_empty() => Some("\x7f".to_string()),
        KeyCode::Esc if modifiers.is_empty() => Some("\x1b".to_string()),
        KeyCode::Left if modifiers.is_empty() => Some("\x1b[D".to_string()),
        KeyCode::Right if modifiers.is_empty() => Some("\x1b[C".to_string()),
        KeyCode::Up if modifiers.is_empty() => Some("\x1b[A".to_string()),
        KeyCode::Down if modifiers.is_empty() => Some("\x1b[B".to_string()),
        KeyCode::Home if modifiers.is_empty() => Some("\x1b[H".to_string()),
        KeyCode::End if modifiers.is_empty() => Some("\x1b[F".to_string()),
        KeyCode::PageUp if modifiers.is_empty() => Some("\x1b[5~".to_string()),
        KeyCode::PageDown if modifiers.is_empty() => Some("\x1b[6~".to_string()),
        KeyCode::Delete if modifiers.is_empty() => Some("\x1b[3~".to_string()),
        KeyCode::F(value) if modifiers.is_empty() => f_key_sequence(value).map(str::to_string),
        _ => None,
    }
}

fn paste_event_to_text(text: &str) -> String {
    if is_bracketed_paste(text) || !(text.contains('\n') || text.contains('\r')) {
        text.to_string()
    } else {
        format!("{BRACKETED_PASTE_BEGIN}{text}{BRACKETED_PASTE_END}")
    }
}

fn is_bracketed_paste(text: &str) -> bool {
    text.starts_with(BRACKETED_PASTE_BEGIN) && text.ends_with(BRACKETED_PASTE_END)
}

fn paste_event_is_bracketed_or_multiline(text: &str) -> bool {
    is_bracketed_paste(text) || text.contains('\n') || text.contains('\r')
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CockpitPasteDecision {
    Accepted {
        text: String,
        byte_count: usize,
        bracketed: bool,
    },
    Rejected {
        reason: CockpitPasteRejectReason,
        byte_count: usize,
        bracketed: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CockpitPasteRejectReason {
    OverlayActive,
    AgentUnfocused,
    ReadOnly,
    PaneSessionMismatch,
    TooLarge,
}

impl CockpitPasteRejectReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::OverlayActive => "overlay_active",
            Self::AgentUnfocused => "agent_unfocused",
            Self::ReadOnly => "read_only",
            Self::PaneSessionMismatch => "pane_session_mismatch",
            Self::TooLarge => "too_large",
        }
    }
}

fn f_key_sequence(value: u8) -> Option<&'static str> {
    match value {
        1 => Some("\x1bOP"),
        2 => Some("\x1bOQ"),
        3 => Some("\x1bOR"),
        4 => Some("\x1bOS"),
        5 => Some("\x1b[15~"),
        6 => Some("\x1b[17~"),
        7 => Some("\x1b[18~"),
        8 => Some("\x1b[19~"),
        9 => Some("\x1b[20~"),
        10 => Some("\x1b[21~"),
        11 => Some("\x1b[23~"),
        12 => Some("\x1b[24~"),
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
    use millrace_sessions_core::state::{UiPaneView, UiPaneViewKind};

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
        assert_eq!(
            request.client_protocol_version,
            Some(M2_ATTACH_PROTOCOL_VERSION)
        );
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
        assert_eq!(
            request.stream_encoding,
            Some(AttachStreamEncoding::RawBytes)
        );
        assert_eq!(
            request.initial_replay,
            Some(AttachInitialReplay::ScreenSnapshot)
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

    #[test]
    fn cockpit_snapshot_seed_accepts_structured_snapshot_and_raw_suffix() {
        let request = agent_attach_request(SessionId::new(), true, 24, 80);

        assert!(request.read_only);
        assert_eq!(request.replay, AttachReplayMode::TerminalSnapshot);
        assert_eq!(
            request.requested_terminal_size,
            Some(TerminalDimensions { rows: 24, cols: 80 })
        );
        assert_eq!(
            request.client_protocol_version,
            Some(M2_ATTACH_PROTOCOL_VERSION)
        );
        assert_eq!(
            request.accepted_frame_types,
            vec![
                AttachFrameType::RawOutput,
                AttachFrameType::StreamLagged,
                AttachFrameType::SnapshotUnavailable,
                AttachFrameType::ScreenSnapshot,
            ]
        );
        assert_eq!(
            request.stream_encoding,
            Some(AttachStreamEncoding::RawBytes)
        );
        assert_eq!(
            request.initial_replay,
            Some(AttachInitialReplay::ScreenSnapshot)
        );
    }

    #[test]
    fn managed_raw_attach_requests_exclusive_raw_bytes_for_the_same_session() {
        let session_id = SessionId::new();
        let request = managed_raw_attach_request(session_id, 42, 132);

        assert_eq!(request.selector, SessionSelector::Id { session_id });
        assert!(!request.read_only);
        assert_eq!(request.replay, AttachReplayMode::RawReplay);
        assert_eq!(
            request.requested_terminal_size,
            Some(TerminalDimensions {
                rows: 42,
                cols: 132
            })
        );
        assert_eq!(
            request.client_protocol_version,
            Some(M2_ATTACH_PROTOCOL_VERSION)
        );
        assert_eq!(
            request.accepted_frame_types,
            vec![
                AttachFrameType::RawOutput,
                AttachFrameType::RawInput,
                AttachFrameType::StreamLagged,
                AttachFrameType::SnapshotUnavailable,
            ]
        );
        assert_eq!(
            request.stream_encoding,
            Some(AttachStreamEncoding::RawBytes)
        );
        assert_eq!(request.initial_replay, Some(AttachInitialReplay::RawReplay));
    }

    #[test]
    fn managed_raw_return_reopens_preview_with_a_size_matched_snapshot() {
        let session_id = SessionId::new();
        let request = agent_attach_request(session_id, false, 38, 42);

        assert_eq!(request.selector, SessionSelector::Id { session_id });
        assert!(!request.read_only);
        assert_eq!(request.replay, AttachReplayMode::TerminalSnapshot);
        assert_eq!(
            request.requested_terminal_size,
            Some(TerminalDimensions::new(38, 42))
        );
        assert_eq!(
            request.initial_replay,
            Some(AttachInitialReplay::ScreenSnapshot)
        );
        assert!(request
            .accepted_frame_types
            .contains(&AttachFrameType::ScreenSnapshot));
        assert!(request
            .accepted_frame_types
            .contains(&AttachFrameType::RawInput));
    }

    #[test]
    fn negotiated_preview_input_uses_raw_bytes() {
        assert!(matches!(
            agent_input_frame(true, "\u{00e9}".to_string()),
            AttachStreamFrame::RawInput { data }
                if data.as_slice() == "\u{00e9}".as_bytes()
        ));
        assert!(matches!(
            agent_input_frame(false, "input".to_string()),
            AttachStreamFrame::Input { text } if text == "input"
        ));
    }

    #[test]
    fn failed_raw_resume_or_partial_suspend_is_fatal_when_no_display_remains() {
        assert!(raw_resume_failure_is_fatal(false, false));
        assert!(!raw_resume_failure_is_fatal(true, true));
        assert!(raw_resume_failure_is_fatal(true, false));
        let partially_left_display_is_usable = cockpit_display_is_usable(
            TerminalControlState::Inactive,
            TerminalControlState::Unknown,
            TerminalControlState::Inactive,
            TerminalControlState::Active,
        );
        assert!(!partially_left_display_is_usable);
        assert!(raw_resume_failure_is_fatal(
            true,
            partially_left_display_is_usable
        ));
    }

    #[test]
    fn managed_raw_fresh_host_state_fails_closed_for_every_authority_change() {
        let mut session = session_summary("agent", SessionRole::Agent);
        let session_id = session.session_id;
        session.attached_clients = 1;
        session.input_owner = Some("preview-stream".to_string());
        session.liveness.worker = LivenessState::Alive;
        session.liveness.child = LivenessState::Alive;
        let worker = WorkerMeta {
            session_id,
            pid: 10,
            child_pid: Some(11),
            child_pgid: Some(11),
            spawn_mode: SpawnMode::Pty,
            process_state: ProcessState::Running,
            started_at: "2026-07-10T00:00:00Z".to_string(),
            ended_at: None,
            stop_requested_at: None,
            stop_reason: None,
            exit_code: None,
            exit_signal: None,
            attached_clients: 1,
            input_owner: Some("preview-stream".to_string()),
            updated_at: "2026-07-10T00:00:01Z".to_string(),
        };

        assert_eq!(
            validate_managed_raw_host_state(
                &session,
                Some(&worker),
                session_id,
                "preview-stream",
                false,
            ),
            Ok(())
        );
        assert_eq!(
            validate_managed_raw_host_state(
                &session,
                Some(&worker),
                session_id,
                "preview-stream",
                true,
            ),
            Err("preview_read_only")
        );

        let mut changed = session.clone();
        changed.session_id = SessionId::new();
        assert_eq!(
            validate_managed_raw_host_state(
                &changed,
                Some(&worker),
                session_id,
                "preview-stream",
                false,
            ),
            Err("host_session_mismatch")
        );
        changed = session.clone();
        changed.process_state = ProcessState::Exited;
        assert_eq!(
            validate_managed_raw_host_state(
                &changed,
                Some(&worker),
                session_id,
                "preview-stream",
                false,
            ),
            Err("host_session_not_running")
        );
        changed = session.clone();
        changed.spawn_mode = SpawnMode::Pipe;
        assert_eq!(
            validate_managed_raw_host_state(
                &changed,
                Some(&worker),
                session_id,
                "preview-stream",
                false,
            ),
            Err("host_session_not_pty")
        );
        changed = session.clone();
        changed.capabilities.raw_attach = false;
        assert_eq!(
            validate_managed_raw_host_state(
                &changed,
                Some(&worker),
                session_id,
                "preview-stream",
                false,
            ),
            Err("host_raw_attach_unavailable")
        );
        changed = session.clone();
        changed.liveness.worker = LivenessState::Dead;
        assert_eq!(
            validate_managed_raw_host_state(
                &changed,
                Some(&worker),
                session_id,
                "preview-stream",
                false,
            ),
            Err("host_worker_not_alive")
        );
        changed = session.clone();
        changed.liveness.child = LivenessState::Dead;
        assert_eq!(
            validate_managed_raw_host_state(
                &changed,
                Some(&worker),
                session_id,
                "preview-stream",
                false,
            ),
            Err("host_child_not_alive")
        );
        changed = session.clone();
        changed.input_owner = Some("other-stream".to_string());
        assert_eq!(
            validate_managed_raw_host_state(
                &changed,
                Some(&worker),
                session_id,
                "preview-stream",
                false,
            ),
            Err("host_input_owner_changed")
        );
        assert_eq!(
            validate_managed_raw_host_state(&session, None, session_id, "preview-stream", false,),
            Err("host_worker_missing")
        );

        let mut changed_worker = worker.clone();
        changed_worker.input_owner = Some("other-stream".to_string());
        assert_eq!(
            validate_managed_raw_host_state(
                &session,
                Some(&changed_worker),
                session_id,
                "preview-stream",
                false,
            ),
            Err("host_worker_input_owner_changed")
        );
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

    #[test]
    fn cockpit_key_input_contract_maps_supported_keys() {
        let cases = [
            (
                "printable unicode",
                key(KeyCode::Char('λ'), KeyModifiers::NONE),
                Some("λ"),
            ),
            (
                "ctrl letter",
                key(KeyCode::Char('c'), KeyModifiers::CONTROL),
                Some("\x03"),
            ),
            (
                "ctrl escape alias",
                key(KeyCode::Char('['), KeyModifiers::CONTROL),
                Some("\x1b"),
            ),
            (
                "alt printable",
                key(KeyCode::Char('x'), KeyModifiers::ALT),
                Some("\x1bx"),
            ),
            ("enter", key(KeyCode::Enter, KeyModifiers::NONE), Some("\r")),
            ("tab", key(KeyCode::Tab, KeyModifiers::NONE), Some("\t")),
            (
                "backspace",
                key(KeyCode::Backspace, KeyModifiers::NONE),
                Some("\x7f"),
            ),
            (
                "escape",
                key(KeyCode::Esc, KeyModifiers::NONE),
                Some("\x1b"),
            ),
            (
                "left",
                key(KeyCode::Left, KeyModifiers::NONE),
                Some("\x1b[D"),
            ),
            (
                "right",
                key(KeyCode::Right, KeyModifiers::NONE),
                Some("\x1b[C"),
            ),
            ("up", key(KeyCode::Up, KeyModifiers::NONE), Some("\x1b[A")),
            (
                "down",
                key(KeyCode::Down, KeyModifiers::NONE),
                Some("\x1b[B"),
            ),
            (
                "home",
                key(KeyCode::Home, KeyModifiers::NONE),
                Some("\x1b[H"),
            ),
            ("end", key(KeyCode::End, KeyModifiers::NONE), Some("\x1b[F")),
            (
                "delete",
                key(KeyCode::Delete, KeyModifiers::NONE),
                Some("\x1b[3~"),
            ),
            (
                "page up",
                key(KeyCode::PageUp, KeyModifiers::NONE),
                Some("\x1b[5~"),
            ),
            (
                "page down",
                key(KeyCode::PageDown, KeyModifiers::NONE),
                Some("\x1b[6~"),
            ),
            ("f1", key(KeyCode::F(1), KeyModifiers::NONE), Some("\x1bOP")),
            (
                "f5",
                key(KeyCode::F(5), KeyModifiers::NONE),
                Some("\x1b[15~"),
            ),
            (
                "f12",
                key(KeyCode::F(12), KeyModifiers::NONE),
                Some("\x1b[24~"),
            ),
            (
                "unsupported alt arrow",
                key(KeyCode::Left, KeyModifiers::ALT),
                None,
            ),
            (
                "unsupported shifted arrow",
                key(KeyCode::Left, KeyModifiers::SHIFT),
                None,
            ),
            (
                "unsupported shifted tab",
                key(KeyCode::Tab, KeyModifiers::SHIFT),
                None,
            ),
            (
                "unsupported shifted enter",
                key(KeyCode::Enter, KeyModifiers::SHIFT),
                None,
            ),
            (
                "unsupported shifted f-key",
                key(KeyCode::F(5), KeyModifiers::SHIFT),
                None,
            ),
            (
                "unsupported ctrl alt char",
                key(
                    KeyCode::Char('x'),
                    KeyModifiers::CONTROL | KeyModifiers::ALT,
                ),
                None,
            ),
            (
                "unsupported f13",
                key(KeyCode::F(13), KeyModifiers::NONE),
                None,
            ),
        ];

        for (name, event, expected) in cases {
            assert_eq!(
                key_event_to_text(event),
                expected.map(str::to_string),
                "{name}"
            );
        }
    }

    #[test]
    fn cockpit_paste_contract_wraps_multiline_once() {
        assert_eq!(paste_event_to_text("one line"), "one line");
        assert_eq!(
            paste_event_to_text("first\nsecond"),
            "\x1b[200~first\nsecond\x1b[201~"
        );

        let already_bracketed = "\x1b[200~first\nsecond\x1b[201~";
        assert_eq!(paste_event_to_text(already_bracketed), already_bracketed);
    }

    #[test]
    fn cockpit_paste_rejects_read_only_or_unfocused_agent_pane() {
        let mut app = cockpit_input_app(true, false);
        assert_eq!(
            cockpit_paste_input_text(&app, "first\nsecond"),
            Some("\x1b[200~first\nsecond\x1b[201~".to_string())
        );

        app.set_agent_input_read_only();
        assert_eq!(cockpit_paste_input_text(&app, "blocked"), None);

        let mut app = cockpit_input_app(true, false);
        app.switch_focus();
        assert_eq!(cockpit_paste_input_text(&app, "blocked"), None);
    }

    #[test]
    fn cockpit_paste_rejects_while_overlay_owns_ui_focus() {
        let mut app = cockpit_input_app(true, false);

        app.open_daemon_switcher();

        assert_eq!(cockpit_paste_input_text(&app, "blocked"), None);
        assert_eq!(
            cockpit_key_input_text(&app, key(KeyCode::Char('x'), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn cockpit_paste_decision_reports_rejection_reasons_and_size() {
        let mut app = cockpit_input_app(true, false);
        let accepted = cockpit_paste_decision(&app, "first\nsecond");
        assert_eq!(
            accepted,
            CockpitPasteDecision::Accepted {
                text: "\x1b[200~first\nsecond\x1b[201~".to_string(),
                byte_count: "first\nsecond".len(),
                bracketed: true,
            }
        );

        app.set_agent_input_read_only();
        assert_eq!(
            cockpit_paste_decision(&app, "blocked"),
            CockpitPasteDecision::Rejected {
                reason: CockpitPasteRejectReason::ReadOnly,
                byte_count: "blocked".len(),
                bracketed: false,
            }
        );

        let oversized = "x".repeat(MAX_COCKPIT_PASTE_BYTES + 1);
        assert_eq!(
            cockpit_paste_decision(&cockpit_input_app(true, false), &oversized),
            CockpitPasteDecision::Rejected {
                reason: CockpitPasteRejectReason::TooLarge,
                byte_count: oversized.len(),
                bracketed: false,
            }
        );
    }

    #[test]
    fn cockpit_paste_rejects_when_focused_pane_does_not_match_open_attach() {
        let mut app = cockpit_input_app(true, false);
        let attached_session_id = app.agent_session_id.expect("initial agent");
        let second_agent = session_summary("agent-two", SessionRole::Agent);
        let second_agent_id = second_agent.session_id;
        let mut sessions = app.workspace_sessions.clone();
        sessions.push(second_agent);
        app.replace_workspace_sessions(sessions);
        let pane_id = app.active_pane_id.expect("active terminal pane");
        assert!(app.assign_pane_view(
            pane_id,
            UiPaneView::new(UiPaneViewKind::SessionTerminal, Some(second_agent_id))
        ));

        assert_eq!(
            cockpit_paste_decision_for_attach(&app, "blocked", Some(attached_session_id)),
            CockpitPasteDecision::Rejected {
                reason: CockpitPasteRejectReason::PaneSessionMismatch,
                byte_count: "blocked".len(),
                bracketed: false,
            }
        );
    }

    #[test]
    fn cockpit_key_input_rejects_when_focused_pane_does_not_match_open_attach() {
        let mut app = cockpit_input_app(true, false);
        let attached_session_id = app.agent_session_id.expect("initial agent");
        let second_agent = session_summary("agent-two", SessionRole::Agent);
        let second_agent_id = second_agent.session_id;
        let mut sessions = app.workspace_sessions.clone();
        sessions.push(second_agent);
        app.replace_workspace_sessions(sessions);
        let pane_id = app.active_pane_id.expect("active terminal pane");
        assert!(app.assign_pane_view(
            pane_id,
            UiPaneView::new(UiPaneViewKind::SessionTerminal, Some(second_agent_id))
        ));

        assert_eq!(
            cockpit_key_input_text_for_attach(
                &app,
                key(KeyCode::Char('x'), KeyModifiers::NONE),
                attached_session_id
            ),
            None
        );
    }

    #[test]
    fn cockpit_emulator_actions_require_focused_attached_terminal() {
        let mut app = cockpit_input_app(true, false);
        let attached_session_id = app.agent_session_id.expect("initial agent");
        assert!(cockpit_attached_terminal_focused(&app, attached_session_id));

        let second_agent = session_summary("agent-two", SessionRole::Agent);
        let second_agent_id = second_agent.session_id;
        let mut sessions = app.workspace_sessions.clone();
        sessions.push(second_agent);
        app.replace_workspace_sessions(sessions);
        let pane_id = app.active_pane_id.expect("active terminal pane");
        assert!(app.assign_pane_view(
            pane_id,
            UiPaneView::new(UiPaneViewKind::SessionTerminal, Some(second_agent_id))
        ));

        assert!(!cockpit_attached_terminal_focused(
            &app,
            attached_session_id
        ));
    }

    #[test]
    fn cockpit_paste_audit_fields_include_sender_target_result_and_reason() {
        let app = cockpit_input_app(true, false);
        let fields = input_audit_fields(
            &app,
            "paste",
            12,
            false,
            Some(CockpitPasteRejectReason::OverlayActive.as_str()),
            true,
        );

        assert_eq!(
            fields.get("sender_client").map(String::as_str),
            Some("cockpit")
        );
        assert_eq!(fields.get("sender_ui_id"), Some(&app.ui_id.to_string()));
        assert_eq!(
            fields.get("target").map(String::as_str),
            Some("agent_terminal")
        );
        let expected_session_id = app.agent_session_id.map(|id| id.to_string());
        assert_eq!(
            fields.get("target_session_id"),
            expected_session_id.as_ref()
        );
        assert_eq!(fields.get("input_kind").map(String::as_str), Some("paste"));
        assert_eq!(fields.get("byte_count").map(String::as_str), Some("12"));
        assert_eq!(fields.get("accepted").map(String::as_str), Some("false"));
        assert_eq!(fields.get("bracketed").map(String::as_str), Some("true"));
        assert_eq!(
            fields.get("reason").map(String::as_str),
            Some("overlay_active")
        );
    }

    #[test]
    fn cockpit_agent_search_sync_searches_scrollback_history_for_copy() {
        let mut app = cockpit_input_app(true, false);
        let mut emulator = TerminalEmulator::new(4, 40, 20);
        for index in 0..10 {
            emulator.process_text(&format!("history line {index}\r\n"));
        }
        app.update_agent_terminal_view(emulator.snapshot(), emulator.is_following());

        assert!(!app
            .agent_terminal
            .as_ref()
            .unwrap()
            .snapshot
            .contains_text("history line 2"));

        app.begin_search_mode();
        app.search_query = "history line 2".to_string();
        sync_agent_search_from_emulator(
            &mut app,
            &mut emulator,
            TerminalSearchDirection::First,
            "search",
        );

        assert!(app
            .agent_terminal
            .as_ref()
            .unwrap()
            .snapshot
            .contains_text("history line 2"));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE), 4),
            KeyAction::CopySearchMatch
        );
        assert!(app
            .copy_buffer_text()
            .expect("copied")
            .starts_with("history line 2"));
    }

    #[test]
    fn cockpit_backspace_to_empty_clears_search_before_copy() {
        let mut app = cockpit_input_app(true, false);
        let mut emulator = TerminalEmulator::new(4, 40, 20);
        emulator.process_text("before target after\r\n");
        app.update_agent_terminal_view(emulator.snapshot(), emulator.is_following());

        app.begin_search_mode();
        app.search_query = "target".to_string();
        sync_agent_search_from_emulator(
            &mut app,
            &mut emulator,
            TerminalSearchDirection::First,
            "search",
        );
        assert!(emulator.current_search_match().is_some());
        assert!(app
            .agent_terminal
            .as_ref()
            .and_then(AgentTerminalPane::current_match)
            .is_some());

        for _ in "target".chars() {
            assert_eq!(
                app.handle_key(key(KeyCode::Backspace, KeyModifiers::NONE), 4),
                KeyAction::SearchBackspace
            );
            sync_agent_search_from_emulator(
                &mut app,
                &mut emulator,
                TerminalSearchDirection::First,
                "search",
            );
        }

        assert!(app.search_query.is_empty());
        assert!(emulator.current_search_match().is_none());
        assert!(app
            .agent_terminal
            .as_ref()
            .and_then(AgentTerminalPane::current_match)
            .is_none());
        assert_eq!(
            app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE), 4),
            KeyAction::CopySearchMatch
        );
        assert_eq!(app.copy_buffer_text(), None);
        assert_eq!(app.status_message, "copy: no search match");
    }

    #[test]
    fn cockpit_reserved_prefix_and_scroll_keys_do_not_become_input_text() {
        let mut app = cockpit_input_app(true, false);
        assert_eq!(
            forwarded_key_text(&mut app, key(KeyCode::Char(']'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            forwarded_key_text(&mut app, key(KeyCode::Char('d'), KeyModifiers::NONE)),
            None
        );

        let mut app = cockpit_input_app(true, false);
        assert_eq!(
            forwarded_key_text(&mut app, key(KeyCode::Char(']'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            forwarded_key_text(&mut app, key(KeyCode::Char('['), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            forwarded_key_text(&mut app, key(KeyCode::Char('G'), KeyModifiers::SHIFT)),
            None
        );
    }

    #[test]
    fn partial_terminal_resume_with_paste_still_active_is_not_usable_and_needs_cleanup() {
        assert!(!cockpit_display_is_usable(
            TerminalControlState::Active,
            TerminalControlState::Active,
            TerminalControlState::Inactive,
            TerminalControlState::Active,
        ));
        assert!(cockpit_terminal_needs_cleanup(
            TerminalControlState::Inactive,
            TerminalControlState::Inactive,
            TerminalControlState::Active,
            TerminalControlState::Inactive,
        ));
    }

    #[test]
    fn terminal_control_write_failure_after_applied_bytes_remains_unknown_until_reasserted() {
        let mut writer = FaultWriter::fail_after_write();
        let mut state = TerminalControlState::Inactive;

        let error = apply_terminal_control(
            &mut writer,
            EnterAlternateScreen,
            &mut state,
            TerminalControlState::Active,
        )
        .expect_err("injected post-write failure");

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(writer.bytes, b"\x1b[?1049h");
        assert_eq!(state, TerminalControlState::Unknown);
        assert!(terminal_control_needs_reassertion(
            state,
            TerminalControlState::Active
        ));

        apply_terminal_control(
            &mut writer,
            EnterAlternateScreen,
            &mut state,
            TerminalControlState::Active,
        )
        .expect("reassert uncertain control");
        assert_eq!(state, TerminalControlState::Active);
        assert_eq!(writer.bytes, b"\x1b[?1049h\x1b[?1049h");
    }

    #[test]
    fn terminal_control_flush_failure_remains_unknown_until_cleanup_reasserts() {
        let mut writer = FaultWriter::fail_flush();
        let mut state = TerminalControlState::Active;

        apply_terminal_control(
            &mut writer,
            LeaveAlternateScreen,
            &mut state,
            TerminalControlState::Inactive,
        )
        .expect_err("injected flush failure");

        assert_eq!(writer.bytes, b"\x1b[?1049l");
        assert_eq!(state, TerminalControlState::Unknown);
        assert!(cockpit_terminal_needs_cleanup(
            TerminalControlState::Inactive,
            state,
            TerminalControlState::Inactive,
            TerminalControlState::Inactive,
        ));

        apply_terminal_control(
            &mut writer,
            LeaveAlternateScreen,
            &mut state,
            TerminalControlState::Inactive,
        )
        .expect("cleanup reasserts uncertain control");
        assert_eq!(state, TerminalControlState::Inactive);
        assert_eq!(writer.bytes, b"\x1b[?1049l\x1b[?1049l");
    }

    #[derive(Default)]
    struct FaultWriter {
        bytes: Vec<u8>,
        fail_after_write: bool,
        fail_flush: bool,
    }

    impl FaultWriter {
        fn fail_after_write() -> Self {
            Self {
                fail_after_write: true,
                ..Self::default()
            }
        }

        fn fail_flush() -> Self {
            Self {
                fail_flush: true,
                ..Self::default()
            }
        }
    }

    impl Write for FaultWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            if self.fail_after_write {
                self.fail_after_write = false;
                return Err(io::Error::other("injected post-write failure"));
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                self.fail_flush = false;
                return Err(io::Error::other("injected flush failure"));
            }
            Ok(())
        }
    }

    fn forwarded_key_text(app: &mut AppModel, event: KeyEvent) -> Option<String> {
        match app.handle_key(event, 10) {
            KeyAction::Input(event) => cockpit_key_input_text(app, event),
            _ => None,
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn cockpit_input_app(input_owner: bool, read_only: bool) -> AppModel {
        let agent = session_summary("agent", SessionRole::Agent);
        let daemon = session_summary("daemon", SessionRole::MillraceDaemon);
        let daemon_id = daemon.session_id;
        AppModel::agent_cockpit(
            UiId::new(),
            agent,
            vec![daemon],
            Some(daemon_id),
            BTreeMap::new(),
            AgentTerminalPane::new(8, 72, input_owner, read_only),
            AgentCockpitLayout::Right,
            MonitorProfile::Basic,
        )
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
            attention: Default::default(),
            status_summary: Default::default(),
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
    terminal: Terminal<CrosstermBackend<BoundedTerminalOutput>>,
    output_deadline: Arc<Mutex<Option<Instant>>>,
    bracketed_paste_state: TerminalControlState,
    raw_mode_state: TerminalControlState,
    alternate_screen_state: TerminalControlState,
    cursor_hidden_state: TerminalControlState,
    cockpit_display_active: bool,
}

struct BoundedTerminalOutput {
    file: File,
    operation_deadline: Arc<Mutex<Option<Instant>>>,
}

impl BoundedTerminalOutput {
    fn open(operation_deadline: Arc<Mutex<Option<Instant>>>) -> io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(nix::libc::O_NONBLOCK)
            .open("/dev/stdout")?;
        Ok(Self {
            file,
            operation_deadline,
        })
    }

    fn write_deadline(&self) -> Instant {
        self.operation_deadline
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .unwrap_or_else(|| Instant::now() + managed_output_write_timeout())
    }
}

fn begin_terminal_output_operation(deadline: &Arc<Mutex<Option<Instant>>>) {
    *deadline
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
        Some(Instant::now() + managed_output_write_timeout());
}

fn finish_terminal_output_operation(deadline: &Arc<Mutex<Option<Instant>>>) {
    *deadline
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

impl Write for BoundedTerminalOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let deadline = self.write_deadline();
        loop {
            match self.file.write(bytes) {
                Ok(written) => return Ok(written),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "terminal output write timed out",
                        ));
                    }
                    let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
                    let mut poll_fd = nix::libc::pollfd {
                        fd: self.file.as_raw_fd(),
                        events: nix::libc::POLLOUT,
                        revents: 0,
                    };
                    let result = unsafe { nix::libc::poll(&mut poll_fd, 1, timeout_ms.max(1)) };
                    if result < 0 {
                        let error = io::Error::last_os_error();
                        if error.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        return Err(error);
                    }
                    if result == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "terminal output write timed out",
                        ));
                    }
                    if poll_fd.revents
                        & (nix::libc::POLLERR | nix::libc::POLLHUP | nix::libc::POLLNVAL)
                        != 0
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "terminal output is unavailable",
                        ));
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalControlState {
    Inactive,
    Active,
    Unknown,
}

fn cockpit_display_is_usable(
    raw_mode_state: TerminalControlState,
    alternate_screen_state: TerminalControlState,
    bracketed_paste_state: TerminalControlState,
    cursor_hidden_state: TerminalControlState,
) -> bool {
    raw_mode_state == TerminalControlState::Active
        && alternate_screen_state == TerminalControlState::Active
        && bracketed_paste_state == TerminalControlState::Active
        && cursor_hidden_state == TerminalControlState::Active
}

fn raw_resume_failure_is_fatal(_resume_failed: bool, cockpit_display_active: bool) -> bool {
    !cockpit_display_active
}

fn cockpit_terminal_needs_cleanup(
    raw_mode_state: TerminalControlState,
    alternate_screen_state: TerminalControlState,
    bracketed_paste_state: TerminalControlState,
    cursor_hidden_state: TerminalControlState,
) -> bool {
    [
        raw_mode_state,
        alternate_screen_state,
        bracketed_paste_state,
        cursor_hidden_state,
    ]
    .into_iter()
    .any(|state| state != TerminalControlState::Inactive)
}

fn terminal_control_needs_reassertion(
    state: TerminalControlState,
    target: TerminalControlState,
) -> bool {
    state != target
}

fn apply_terminal_control<W, C>(
    writer: &mut W,
    command: C,
    state: &mut TerminalControlState,
    target: TerminalControlState,
) -> io::Result<()>
where
    W: Write,
    C: Command,
{
    *state = TerminalControlState::Unknown;
    writer.queue(command)?;
    writer.flush()?;
    *state = target;
    Ok(())
}

fn apply_raw_mode(
    state: &mut TerminalControlState,
    target: TerminalControlState,
) -> io::Result<()> {
    *state = TerminalControlState::Unknown;
    let result = match target {
        TerminalControlState::Active => enable_raw_mode(),
        TerminalControlState::Inactive => disable_raw_mode(),
        TerminalControlState::Unknown => unreachable!("unknown is not a terminal mode target"),
    };
    if result.is_ok() {
        *state = target;
    }
    result
}

impl TerminalSession {
    fn enter() -> Result<Self, CockpitError> {
        let output_deadline = Arc::new(Mutex::new(None));
        let backend =
            CrosstermBackend::new(BoundedTerminalOutput::open(Arc::clone(&output_deadline))?);
        let mut session = Self {
            terminal: Terminal::new(backend)?,
            output_deadline,
            bracketed_paste_state: TerminalControlState::Unknown,
            raw_mode_state: TerminalControlState::Unknown,
            alternate_screen_state: TerminalControlState::Unknown,
            cursor_hidden_state: TerminalControlState::Unknown,
            cockpit_display_active: false,
        };
        begin_terminal_output_operation(&session.output_deadline);
        apply_raw_mode(&mut session.raw_mode_state, TerminalControlState::Active)?;
        apply_terminal_control(
            session.terminal.backend_mut(),
            EnterAlternateScreen,
            &mut session.alternate_screen_state,
            TerminalControlState::Active,
        )?;
        apply_terminal_control(
            session.terminal.backend_mut(),
            EnableBracketedPaste,
            &mut session.bracketed_paste_state,
            TerminalControlState::Active,
        )?;
        apply_terminal_control(
            session.terminal.backend_mut(),
            Hide,
            &mut session.cursor_hidden_state,
            TerminalControlState::Active,
        )?;
        finish_terminal_output_operation(&session.output_deadline);
        session.cockpit_display_active = true;
        Ok(session)
    }

    fn suspend(session: &Arc<Mutex<Self>>) -> Result<TerminalSuspension, CockpitError> {
        let mut terminal = session.lock().expect("cockpit terminal lock poisoned");
        if let Err(error) = terminal.leave_cockpit_display() {
            let _ = terminal.resume_cockpit_display();
            return Err(error);
        }
        drop(terminal);
        Ok(TerminalSuspension {
            session: Arc::clone(session),
            resumed: false,
        })
    }

    fn leave_cockpit_display(&mut self) -> Result<(), CockpitError> {
        let mut first_error = None;
        begin_terminal_output_operation(&self.output_deadline);
        if terminal_control_needs_reassertion(self.raw_mode_state, TerminalControlState::Inactive) {
            if let Err(error) =
                apply_raw_mode(&mut self.raw_mode_state, TerminalControlState::Inactive)
            {
                first_error.get_or_insert(CockpitError::Io(error));
            }
        }
        if terminal_control_needs_reassertion(
            self.bracketed_paste_state,
            TerminalControlState::Inactive,
        ) {
            if let Err(error) = apply_terminal_control(
                self.terminal.backend_mut(),
                DisableBracketedPaste,
                &mut self.bracketed_paste_state,
                TerminalControlState::Inactive,
            ) {
                first_error.get_or_insert(CockpitError::Io(error));
            }
        }
        if terminal_control_needs_reassertion(
            self.alternate_screen_state,
            TerminalControlState::Inactive,
        ) {
            if let Err(error) = apply_terminal_control(
                self.terminal.backend_mut(),
                LeaveAlternateScreen,
                &mut self.alternate_screen_state,
                TerminalControlState::Inactive,
            ) {
                first_error.get_or_insert(CockpitError::Io(error));
            }
        }
        if terminal_control_needs_reassertion(
            self.cursor_hidden_state,
            TerminalControlState::Inactive,
        ) {
            if let Err(error) = apply_terminal_control(
                self.terminal.backend_mut(),
                Show,
                &mut self.cursor_hidden_state,
                TerminalControlState::Inactive,
            ) {
                first_error.get_or_insert(CockpitError::Io(error));
            }
        }
        finish_terminal_output_operation(&self.output_deadline);
        self.cockpit_display_active = false;
        first_error.map_or(Ok(()), Err)
    }

    fn resume_cockpit_display(&mut self) -> Result<(), CockpitError> {
        let mut first_error = None;
        begin_terminal_output_operation(&self.output_deadline);
        if terminal_control_needs_reassertion(self.raw_mode_state, TerminalControlState::Active) {
            if let Err(error) =
                apply_raw_mode(&mut self.raw_mode_state, TerminalControlState::Active)
            {
                first_error.get_or_insert(CockpitError::Io(error));
            }
        }
        if terminal_control_needs_reassertion(
            self.alternate_screen_state,
            TerminalControlState::Active,
        ) {
            if let Err(error) = apply_terminal_control(
                self.terminal.backend_mut(),
                EnterAlternateScreen,
                &mut self.alternate_screen_state,
                TerminalControlState::Active,
            ) {
                first_error.get_or_insert(CockpitError::Io(error));
            }
        }
        if let Err(error) = self.terminal.clear() {
            first_error.get_or_insert(CockpitError::Io(error));
        }
        if terminal_control_needs_reassertion(
            self.bracketed_paste_state,
            TerminalControlState::Active,
        ) {
            if let Err(error) = apply_terminal_control(
                self.terminal.backend_mut(),
                EnableBracketedPaste,
                &mut self.bracketed_paste_state,
                TerminalControlState::Active,
            ) {
                first_error.get_or_insert(CockpitError::Io(error));
            }
        }
        if terminal_control_needs_reassertion(
            self.cursor_hidden_state,
            TerminalControlState::Active,
        ) {
            if let Err(error) = apply_terminal_control(
                self.terminal.backend_mut(),
                Hide,
                &mut self.cursor_hidden_state,
                TerminalControlState::Active,
            ) {
                first_error.get_or_insert(CockpitError::Io(error));
            }
        }
        finish_terminal_output_operation(&self.output_deadline);
        self.cockpit_display_active = first_error.is_none()
            && cockpit_display_is_usable(
                self.raw_mode_state,
                self.alternate_screen_state,
                self.bracketed_paste_state,
                self.cursor_hidden_state,
            );
        first_error.map_or(Ok(()), Err)
    }

    fn recover_display(&mut self) -> Result<(), CockpitError> {
        self.resume_cockpit_display()
    }
}

struct TerminalSuspension {
    session: Arc<Mutex<TerminalSession>>,
    resumed: bool,
}

impl TerminalSuspension {
    fn resume(mut self) -> Result<(), CockpitError> {
        self.session
            .lock()
            .expect("cockpit terminal lock poisoned")
            .resume_cockpit_display()?;
        self.resumed = true;
        Ok(())
    }
}

impl Drop for TerminalSuspension {
    fn drop(&mut self) {
        if !self.resumed {
            let _ = self
                .session
                .lock()
                .expect("cockpit terminal lock poisoned")
                .resume_cockpit_display();
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if self.cockpit_display_active
            || cockpit_terminal_needs_cleanup(
                self.raw_mode_state,
                self.alternate_screen_state,
                self.bracketed_paste_state,
                self.cursor_hidden_state,
            )
        {
            begin_terminal_output_operation(&self.output_deadline);
            if terminal_control_needs_reassertion(
                self.raw_mode_state,
                TerminalControlState::Inactive,
            ) {
                let _ = apply_raw_mode(&mut self.raw_mode_state, TerminalControlState::Inactive);
            }
            if terminal_control_needs_reassertion(
                self.bracketed_paste_state,
                TerminalControlState::Inactive,
            ) {
                let _ = apply_terminal_control(
                    self.terminal.backend_mut(),
                    DisableBracketedPaste,
                    &mut self.bracketed_paste_state,
                    TerminalControlState::Inactive,
                );
            }
            if terminal_control_needs_reassertion(
                self.alternate_screen_state,
                TerminalControlState::Inactive,
            ) {
                let _ = apply_terminal_control(
                    self.terminal.backend_mut(),
                    LeaveAlternateScreen,
                    &mut self.alternate_screen_state,
                    TerminalControlState::Inactive,
                );
            }
            if terminal_control_needs_reassertion(
                self.cursor_hidden_state,
                TerminalControlState::Inactive,
            ) {
                let _ = apply_terminal_control(
                    self.terminal.backend_mut(),
                    Show,
                    &mut self.cursor_hidden_state,
                    TerminalControlState::Inactive,
                );
            }
            finish_terminal_output_operation(&self.output_deadline);
        }
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
    Attach(#[from] crate::attach::AttachError),
    #[error(transparent)]
    Core(#[from] MillmuxError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("cockpit transition error: {0}")]
    Transition(String),
}
