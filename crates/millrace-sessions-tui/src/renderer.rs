use millrace_sessions_core::{ids::SessionId, state::UiMode};
use ratatui::{
    backend::TestBackend,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
    Terminal,
};

use crate::{
    app::{AppModel, HostConnectionState},
    pane::{
        AgentCockpitLayout, CommandOutputState, DaemonConsoleLayout, Pane, PaneKind,
        COCKPIT_SESSION_LIST_HEIGHT,
    },
    terminal::{TerminalCell, TerminalColor, TerminalSnapshot},
    width::{cell_symbol_width, terminal_text_width, truncate_terminal_text},
};

pub fn render_app(frame: &mut ratatui::Frame<'_>, app: &AppModel) {
    let area = frame.area();
    if area.height == 0 || area.width == 0 {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    render_body(frame, chunks[0], app);
    render_status(frame, chunks[1], app);
}

pub fn render_to_string(app: &AppModel, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test backend is in-memory");
    terminal
        .draw(|frame| render_app(frame, app))
        .expect("test render should not fail");
    buffer_to_string(terminal.backend().buffer())
}

pub fn render_terminal_snapshot(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    snapshot: &TerminalSnapshot,
) {
    render_terminal_cells(frame.buffer_mut(), area, &snapshot.cells);
}

fn render_body(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppModel) {
    match &app.host_connection {
        HostConnectionState::Reconnecting { .. } | HostConnectionState::Disconnected { .. } => {
            render_reconnect(frame, area, app);
            return;
        }
        HostConnectionState::Connected => {}
    }

    let command_output_outside_pane = app.command_output.is_visible()
        && app.mode != millrace_sessions_core::state::UiMode::AgentCockpit;
    if command_output_outside_pane {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(5)])
            .split(area);
        render_mode_body(frame, chunks[0], app);
        render_command_output(frame, chunks[1], app);
    } else {
        render_mode_body(frame, area, app);
    }

    if app.command_palette.open {
        render_palette(frame, centered(area, 50, 8), app);
    }
    if app.daemon_switcher.open {
        render_daemon_switcher(frame, centered(area, 72, 12), app);
    }
    if app.help_overlay.open {
        render_help(frame, centered(area, 64, 13), app);
    }
    if app.confirmation.is_some() {
        render_confirmation(frame, centered(area, 58, 6), app);
    }
}

fn render_mode_body(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppModel) {
    match app.mode {
        millrace_sessions_core::state::UiMode::DaemonConsole => render_console(frame, area, app),
        millrace_sessions_core::state::UiMode::AgentCockpit => render_cockpit(frame, area, app),
    }
}

fn render_console(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppModel) {
    match app.console_layout {
        DaemonConsoleLayout::Single => {
            let session_id = app.active_daemon_session_id;
            render_log(frame, area, app, session_id, "Daemon Monitor", true);
        }
        DaemonConsoleLayout::Split => render_split(frame, area, app),
        DaemonConsoleLayout::Grid => render_grid(frame, area, app),
        DaemonConsoleLayout::List => render_list_layout(frame, area, app),
    }
}

fn render_cockpit(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppModel) {
    let session_list = app
        .panes
        .iter()
        .find(|pane| !pane.stale && pane.kind == PaneKind::SessionList);
    let (session_area, content_area) = if session_list.is_some() {
        cockpit_session_list_split(area, app)
    } else {
        (Rect::default(), area)
    };
    if session_list.is_some() {
        render_workspace_session_list(frame, session_area, app);
    }

    let content_panes = app
        .panes
        .iter()
        .filter(|pane| {
            !pane.stale
                && pane.kind != PaneKind::SessionList
                && (pane.kind != PaneKind::CommandOutput || app.command_output.is_visible())
        })
        .collect::<Vec<_>>();
    render_cockpit_content_panes(frame, content_area, app, &content_panes);
}

fn render_cockpit_content_panes(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &AppModel,
    panes: &[&Pane],
) {
    if panes.is_empty() {
        frame.render_widget(Paragraph::new("No visible cockpit panes"), area);
        return;
    }

    if app.cockpit_layout == AgentCockpitLayout::Focus {
        let pane = app
            .active_pane_id
            .and_then(|pane_id| panes.iter().find(|pane| pane.id == pane_id).copied())
            .unwrap_or(panes[0]);
        render_cockpit_pane(frame, area, app, pane);
        return;
    }

    if panes.len() == 1 {
        render_cockpit_pane(frame, area, app, panes[0]);
        return;
    }

    match app.cockpit_layout {
        AgentCockpitLayout::Right => {
            if area.width >= 64 {
                render_cockpit_split_panes(frame, area, app, panes, Direction::Horizontal, 55);
            } else {
                render_cockpit_split_panes(frame, area, app, panes, Direction::Vertical, 60);
            }
        }
        AgentCockpitLayout::Bottom => {
            render_cockpit_split_panes(frame, area, app, panes, Direction::Vertical, 60)
        }
        AgentCockpitLayout::Wide => {
            if area.width >= 64 {
                render_cockpit_split_panes(frame, area, app, panes, Direction::Horizontal, 65);
            } else {
                render_cockpit_split_panes(frame, area, app, panes, Direction::Vertical, 65);
            }
        }
        AgentCockpitLayout::Focus => unreachable!("focus layout handled before split"),
    }
}

fn render_cockpit_split_panes(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &AppModel,
    panes: &[&Pane],
    direction: Direction,
    first_percent: u16,
) {
    let constraints = if panes.len() == 2 {
        vec![
            Constraint::Percentage(first_percent),
            Constraint::Percentage(100 - first_percent),
        ]
    } else {
        vec![Constraint::Percentage(100 / panes.len() as u16); panes.len()]
    };
    let chunks = Layout::default()
        .direction(direction)
        .constraints(constraints)
        .split(area);
    for (index, pane) in panes.iter().enumerate() {
        render_cockpit_pane(frame, chunks[index], app, pane);
    }
}

fn render_cockpit_pane(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppModel, pane: &Pane) {
    let focused = Some(pane.id) == app.active_pane_id;
    match pane.kind {
        PaneKind::AgentTerminal => {
            render_agent_terminal_view(frame, area, app, pane.session_id, &pane.title, focused)
        }
        PaneKind::DaemonMonitor => {
            render_log(frame, area, app, pane.session_id, &pane.title, focused)
        }
        PaneKind::SessionList | PaneKind::DaemonList => {
            render_workspace_session_list(frame, area, app)
        }
        PaneKind::CommandOutput => render_command_output(frame, area, app),
        PaneKind::StatusBar | PaneKind::HelpOverlay | PaneKind::CommandPalette => {
            frame.render_widget(Paragraph::new(pane.title.as_str()), area);
        }
    }
}

fn cockpit_session_list_split(area: Rect, app: &AppModel) -> (Rect, Rect) {
    let session_rows = app.workspace_sessions.len().clamp(1, 3) as u16;
    let desired = 1 + session_rows.saturating_mul(2);
    let list_height = desired
        .min(COCKPIT_SESSION_LIST_HEIGHT)
        .min(area.height.saturating_sub(2))
        .max(1);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(list_height), Constraint::Min(1)])
        .split(area);
    (chunks[0], chunks[1])
}

fn render_agent_terminal_view(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &AppModel,
    session_id: Option<SessionId>,
    title: &str,
    focused: bool,
) {
    if session_id != app.agent_session_id {
        let label = session_id
            .map(|session_id| session_id.to_string())
            .unwrap_or_else(|| "unassigned".to_string());
        frame.render_widget(
            Paragraph::new(format!(
                "{title} | session={label} not attached{}",
                focus_suffix(focused)
            )),
            area,
        );
        return;
    }
    let Some(terminal) = &app.agent_terminal else {
        frame.render_widget(Paragraph::new(format!("{title} | no session")), area);
        return;
    };
    let input = if terminal.input_owner && !terminal.read_only {
        "owned"
    } else {
        "read-only"
    };
    let screen = if terminal.snapshot.alternate_screen {
        " alt"
    } else {
        ""
    };
    let view = if app.search_mode && focused {
        "search"
    } else if app.scroll_mode && focused {
        "scroll"
    } else if terminal.is_following() {
        "live"
    } else {
        "paused"
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            format!(
                "{title} | {input} {view}{screen} cur={},{}{}",
                terminal.snapshot.cursor_row,
                terminal.snapshot.cursor_col,
                focus_suffix(focused)
            ),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        )])),
        chunks[0],
    );
    if terminal.initializing {
        frame.render_widget(Paragraph::new("agent terminal initializing"), chunks[1]);
    } else {
        render_terminal_snapshot(frame, chunks[1], &terminal.snapshot);
    }
}

fn render_terminal_cells(buffer: &mut Buffer, area: Rect, rows: &[Vec<TerminalCell>]) {
    for (row_index, row) in rows.iter().take(usize::from(area.height)).enumerate() {
        for (col_index, terminal_cell) in row.iter().take(usize::from(area.width)).enumerate() {
            let x = area.x + u16::try_from(col_index).expect("terminal column fits u16");
            let y = area.y + u16::try_from(row_index).expect("terminal row fits u16");
            let Some(cell) = buffer.cell_mut((x, y)) else {
                continue;
            };
            let rendered_width = cell_symbol_width(terminal_cell.display_symbol());
            if !terminal_cell.continuation
                && rendered_width > 1
                && col_index + rendered_width > usize::from(area.width)
            {
                cell.set_symbol(" ");
                cell.set_style(terminal_cell_style(terminal_cell));
                continue;
            }
            cell.set_symbol(if terminal_cell.continuation {
                ""
            } else {
                terminal_cell.display_symbol()
            });
            cell.set_style(terminal_cell_style(terminal_cell));
        }
    }
}

fn render_split(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppModel) {
    let ids = visible_daemon_ids(app, 2);
    if ids.is_empty() {
        render_empty_daemons(frame, area);
        return;
    }
    let direction = if area.width >= 100 {
        Direction::Horizontal
    } else {
        Direction::Vertical
    };
    let constraints = vec![Constraint::Percentage(100 / ids.len() as u16); ids.len()];
    let chunks = Layout::default()
        .direction(direction)
        .constraints(constraints)
        .split(area);
    for (index, session_id) in ids.into_iter().enumerate() {
        let title = app
            .daemon_sessions
            .iter()
            .find(|session| session.session_id == session_id)
            .and_then(|session| session.name.as_deref())
            .unwrap_or("daemon");
        render_log(
            frame,
            chunks[index],
            app,
            Some(session_id),
            title,
            app.active_daemon_session_id == Some(session_id),
        );
    }
}

fn render_grid(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppModel) {
    let ids = visible_daemon_ids(app, 4);
    if ids.is_empty() {
        render_empty_daemons(frame, area);
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    let mut areas = Vec::new();
    for row in rows.iter() {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(*row);
        areas.extend(cols.iter().copied());
    }
    for (index, session_id) in ids.into_iter().enumerate() {
        let title = app
            .daemon_sessions
            .iter()
            .find(|session| session.session_id == session_id)
            .and_then(|session| session.name.as_deref())
            .unwrap_or("daemon");
        render_log(
            frame,
            areas[index],
            app,
            Some(session_id),
            title,
            app.active_daemon_session_id == Some(session_id),
        );
    }
}

fn render_list_layout(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppModel) {
    if area.width < 80 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(6), Constraint::Min(3)])
            .split(area);
        render_daemon_list(frame, chunks[0], app);
        render_log(
            frame,
            chunks[1],
            app,
            app.active_daemon_session_id,
            "Daemon Monitor",
            true,
        );
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(34), Constraint::Min(10)])
        .split(area);
    render_daemon_list(frame, chunks[0], app);
    render_log(
        frame,
        chunks[1],
        app,
        app.active_daemon_session_id,
        "Daemon Monitor",
        true,
    );
}

fn render_daemon_list(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppModel) {
    let mut lines = vec![Line::from(vec![Span::styled(
        format!("Daemon List | count={}", app.daemon_sessions.len()),
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
    )])];
    for session in &app.daemon_sessions {
        let marker = if Some(session.session_id) == app.active_daemon_session_id {
            ">"
        } else {
            " "
        };
        let name = session.name.as_deref().unwrap_or("-");
        let workspace = session
            .workspace
            .as_ref()
            .map(|workspace| workspace.canonical_path.display().to_string())
            .unwrap_or_else(|| session.cwd.display().to_string());
        lines.push(Line::from(format!(
            "{marker} {name} {}/{} m={}",
            process_label(&session.process_state),
            attention_label(&session.attention_state),
            session.monitor_profile
        )));
        lines.push(Line::from(format!("  {}", compact_path(&workspace, 30))));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_workspace_session_list(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppModel) {
    let rows = app.workspace_session_rows();
    let focused = pane_focused(app, PaneKind::SessionList);
    let mut lines = vec![Line::from(vec![Span::styled(
        format!("Sessions | count={}{}", rows.len(), focus_suffix(focused)),
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
    )])];
    if rows.is_empty() {
        lines.push(Line::from("no workspace sessions"));
    }

    let width = area.width.saturating_sub(2).max(16) as usize;
    for row in rows {
        let marker = if row.focused {
            ">"
        } else if row.selected {
            "*"
        } else {
            " "
        };
        let compact_liveness = row
            .liveness
            .replace("worker:", "w:")
            .replace(" child:", " c:");
        let path_width = if width >= 96 { 28 } else { 16 };
        lines.push(Line::from(format!(
            "{marker} {} {} st={} att={} live={} {} cwd={} git={}@{}",
            row.role,
            compact_text(&row.name, 14),
            row.process_state,
            row.attention,
            compact_liveness,
            row.unread,
            compact_path(&row.location, path_width),
            compact_path(&row.worktree, 16),
            compact_text(&row.branch, 10)
        )));
        lines.push(Line::from(format!(
            "  {}",
            truncate_text(&status_source_line(&row), width.saturating_sub(2))
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn status_source_line(row: &crate::pane::WorkspaceSessionRow) -> String {
    row.source_summary.clone()
}

fn render_log(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &AppModel,
    session_id: Option<SessionId>,
    title: &str,
    focused: bool,
) {
    let log = session_id
        .and_then(|session_id| app.daemon_logs.get(&session_id))
        .unwrap_or(&app.line_log);
    let daemon = session_id.and_then(|session_id| {
        app.daemon_sessions
            .iter()
            .find(|session| session.session_id == session_id)
    });
    let profile = daemon
        .map(|session| session.monitor_profile.to_string())
        .unwrap_or_else(|| app.monitor_profile.to_string());
    let state = daemon
        .filter(|session| !daemon_state_is_healthy(&session.process_state))
        .map(|session| format!(" | state={}", process_label(&session.process_state)))
        .unwrap_or_default();
    let header = if app.scroll_mode || log.is_scrolled() {
        format!(
            "{title}{state} | mon={profile} | follow=paused scroll{}",
            focus_suffix(focused)
        )
    } else if log.is_following() {
        format!(
            "{title}{state} | mon={profile} | follow=live{}",
            focus_suffix(focused)
        )
    } else {
        format!(
            "{title}{state} | mon={profile} | follow=paused{}",
            focus_suffix(focused)
        )
    };

    let mut lines = vec![Line::from(vec![Span::styled(
        header,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )])];
    if let Some(session) = daemon.filter(|session| !daemon_state_is_healthy(&session.process_state))
    {
        lines.push(Line::from(format!(
            "daemon degraded: state={} attention={}",
            process_label(&session.process_state),
            attention_label(&session.attention_state)
        )));
        if let Some(message) = &session.failure_message {
            lines.push(Line::from(format!(
                "failure: {}",
                compact_text(message, 96)
            )));
        }
        lines.push(Line::from(format!(
            "recovery: {}",
            recovery_actions_label(&session.process_state)
        )));
    }
    let content_height = area.height.saturating_sub(lines.len() as u16);
    for line in log.visible_lines(content_height) {
        lines.push(Line::from(line));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_empty_daemons(frame: &mut ratatui::Frame<'_>, area: Rect) {
    frame.render_widget(Paragraph::new("No millrace-daemon sessions"), area);
}

fn render_command_output(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppModel) {
    let state = match app.command_output.state {
        CommandOutputState::Idle => "idle",
        CommandOutputState::Running => "running",
        CommandOutputState::Succeeded => "succeeded",
        CommandOutputState::Failed => "failed",
    };
    let mut lines = vec![
        Line::from(vec![Span::styled(
            format!(
                "Command Output | state={state} target={}",
                app.command_output.target
            ),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(format!("argv: {}", app.command_output.argv.join(" "))),
    ];
    for line in app.command_output.stderr.iter().take(3) {
        lines.push(Line::from(format!("stderr: {line}")));
    }
    for line in app.command_output.stdout.iter().take(3) {
        lines.push(Line::from(format!("stdout: {line}")));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_reconnect(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppModel) {
    let lines = vec![
        Line::from(vec![Span::styled(
            "Host Reconnecting",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(app.host_connection.label()),
        Line::from("sessions remain hosted by SessionControl when available"),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_palette(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppModel) {
    frame.render_widget(Clear, area);
    let mut lines = vec![
        Line::from("Command Palette"),
        Line::from(format!("target: {}", app.command_palette.target)),
        Line::from(format!("> {}", app.command_palette.input)),
    ];
    for command in app.command_palette.commands.iter().take(4) {
        lines.push(Line::from(format!("  {command}")));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_confirmation(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppModel) {
    let Some(prompt) = &app.confirmation else {
        return;
    };
    frame.render_widget(Clear, area);
    let lines = vec![
        Line::from(vec![Span::styled(
            "Confirmation Required",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )]),
        Line::from(format!("operation: {}", prompt.operation)),
        Line::from(format!("target: {}", prompt.target)),
        Line::from(format!("type: {}", prompt.challenge)),
        Line::from(format!("> {}", prompt.input)),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_help(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppModel) {
    frame.render_widget(Clear, area);
    let mut lines = vec![Line::from("Help")];
    for (key, action) in &app.help_overlay.entries {
        lines.push(Line::from(format!("{key:<12} {action}")));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_daemon_switcher(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppModel) {
    frame.render_widget(Clear, area);
    let mut lines = vec![Line::from(vec![Span::styled(
        "Session Switcher",
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
    )])];
    let rows = app.workspace_session_rows();
    if rows.is_empty() {
        lines.push(Line::from("no workspace sessions"));
    }
    for row in rows {
        let marker = if Some(row.session_id) == app.daemon_switcher.selected_session_id {
            ">"
        } else if row.selected {
            "*"
        } else {
            " "
        };
        let compact_liveness = row
            .liveness
            .replace("worker:", "w:")
            .replace(" child:", " c:");
        lines.push(Line::from(format!(
            "{marker} {} {} st={} att={} live={} {} cwd={} git={}@{}",
            row.role,
            compact_text(&row.name, 14),
            row.process_state,
            row.attention,
            compact_liveness,
            row.unread,
            compact_path(&row.location, 20),
            compact_path(&row.worktree, 16),
            compact_text(&row.branch, 10)
        )));
        lines.push(Line::from(format!(
            "  {}",
            truncate_text(&status_source_line(&row), 66)
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_status(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppModel) {
    let prefix = if app.prefix_pending {
        "prefix"
    } else {
        "ready"
    };
    let status = if app.mode == UiMode::AgentCockpit {
        let controls = " ^]a=raw ^]d=detach/raw-return";
        let fixed = format!(" {prefix} status=");
        let available = usize::from(area.width).saturating_sub(
            terminal_text_width(&fixed).saturating_add(terminal_text_width(controls)),
        );
        format!(
            "{fixed}{}{controls}",
            truncate_text(&app.status_message, available)
        )
    } else {
        format!(
            " mode={:?} monitor={:?} host={} input={} view={} status={} ",
            app.mode,
            app.monitor_profile,
            app.host_connection.label(),
            prefix,
            app.active_view_label(),
            app.status_message
        )
    };
    frame.render_widget(
        Paragraph::new(status).style(Style::default().bg(Color::Blue).fg(Color::White)),
        area,
    );
}

fn visible_daemon_ids(app: &AppModel, limit: usize) -> Vec<SessionId> {
    let mut ids = Vec::new();
    if let Some(session_id) = app.active_daemon_session_id {
        ids.push(session_id);
    }
    for session in &app.daemon_sessions {
        if ids.len() >= limit {
            break;
        }
        if Some(session.session_id) != app.active_daemon_session_id {
            ids.push(session.session_id);
        }
    }
    ids
}

fn focus_suffix(focused: bool) -> &'static str {
    if focused {
        " | selected"
    } else {
        ""
    }
}

fn pane_focused(app: &AppModel, kind: PaneKind) -> bool {
    app.focused_pane_kind() == Some(kind)
}

fn process_label(value: &millrace_sessions_core::state::ProcessState) -> &'static str {
    match value {
        millrace_sessions_core::state::ProcessState::Starting => "starting",
        millrace_sessions_core::state::ProcessState::Running => "running",
        millrace_sessions_core::state::ProcessState::Exited => "exited",
        millrace_sessions_core::state::ProcessState::Crashed => "crashed",
        millrace_sessions_core::state::ProcessState::Killed => "killed",
        millrace_sessions_core::state::ProcessState::FailedStart => "failed_start",
        millrace_sessions_core::state::ProcessState::Failed => "failed",
        millrace_sessions_core::state::ProcessState::Lost => "lost",
        millrace_sessions_core::state::ProcessState::Stale => "stale",
        millrace_sessions_core::state::ProcessState::Orphaned => "orphaned",
    }
}

fn daemon_state_is_healthy(value: &millrace_sessions_core::state::ProcessState) -> bool {
    matches!(
        value,
        millrace_sessions_core::state::ProcessState::Starting
            | millrace_sessions_core::state::ProcessState::Running
    )
}

fn recovery_actions_label(value: &millrace_sessions_core::state::ProcessState) -> &'static str {
    match value {
        millrace_sessions_core::state::ProcessState::Starting
        | millrace_sessions_core::state::ProcessState::Running => "inspect logs stop kill",
        millrace_sessions_core::state::ProcessState::FailedStart => "inspect logs doctor delete",
        millrace_sessions_core::state::ProcessState::Exited
        | millrace_sessions_core::state::ProcessState::Killed => "inspect logs archive delete",
        millrace_sessions_core::state::ProcessState::Crashed
        | millrace_sessions_core::state::ProcessState::Failed
        | millrace_sessions_core::state::ProcessState::Lost
        | millrace_sessions_core::state::ProcessState::Stale
        | millrace_sessions_core::state::ProcessState::Orphaned => {
            "inspect logs doctor archive delete"
        }
    }
}

fn attention_label(value: &millrace_sessions_core::state::AttentionState) -> &'static str {
    match value {
        millrace_sessions_core::state::AttentionState::Unknown => "unknown",
        millrace_sessions_core::state::AttentionState::Active => "active",
        millrace_sessions_core::state::AttentionState::Idle => "idle",
        millrace_sessions_core::state::AttentionState::NeedsAttention => "needs_attention",
        millrace_sessions_core::state::AttentionState::MillraceIdle => "idle",
        millrace_sessions_core::state::AttentionState::MillraceBusy => "busy",
    }
}

fn terminal_cell_style(cell: &TerminalCell) -> Style {
    let mut style = Style::default()
        .fg(to_ratatui_color(cell.fg))
        .bg(to_ratatui_color(cell.bg));
    if cell.style.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.style.dim {
        style = style.add_modifier(Modifier::DIM);
    }
    if cell.style.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.style.underline {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.style.inverse {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

fn to_ratatui_color(color: TerminalColor) -> Color {
    match color {
        TerminalColor::Default => Color::Reset,
        TerminalColor::Indexed(index) => match index {
            0 => Color::Black,
            1 => Color::Red,
            2 => Color::Green,
            3 => Color::Yellow,
            4 => Color::Blue,
            5 => Color::Magenta,
            6 => Color::Cyan,
            7 => Color::White,
            value => Color::Indexed(value),
        },
        TerminalColor::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

fn compact_path(path: &str, width: usize) -> String {
    compact_text(path, width)
}

fn compact_text(value: &str, width: usize) -> String {
    let value = value.replace('\n', " ");
    if value.chars().count() <= width {
        return value;
    }
    let keep = width.saturating_sub(1);
    let tail = value
        .chars()
        .rev()
        .take(keep)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("~{tail}")
}

fn truncate_text(value: &str, width: usize) -> String {
    truncate_terminal_text(value, width)
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn buffer_to_string(buffer: &Buffer) -> String {
    let area = buffer.area;
    let mut output = String::new();
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if let Some(cell) = buffer.cell((x, y)) {
                output.push_str(cell.symbol());
            }
        }
        if y + 1 < area.y + area.height {
            output.push('\n');
        }
    }
    output
}
