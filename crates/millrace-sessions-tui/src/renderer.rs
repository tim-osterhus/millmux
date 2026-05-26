use millrace_sessions_core::ids::SessionId;
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
    pane::{AgentCockpitLayout, CommandOutputState, DaemonConsoleLayout, PaneKind},
    terminal::{TerminalCell, TerminalColor},
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

fn render_body(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppModel) {
    match &app.host_connection {
        HostConnectionState::Reconnecting { .. } | HostConnectionState::Disconnected { .. } => {
            render_reconnect(frame, area, app);
            return;
        }
        HostConnectionState::Connected => {}
    }

    if app.command_output.is_visible() {
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
        render_help(frame, centered(area, 54, 9), app);
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
    match app.cockpit_layout {
        AgentCockpitLayout::Right => {
            if area.width >= 100 {
                let chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
                    .split(area);
                render_agent_terminal(frame, chunks[0], app);
                render_log(
                    frame,
                    chunks[1],
                    app,
                    app.active_daemon_session_id,
                    "Daemon Monitor",
                    pane_focused(app, PaneKind::DaemonMonitor),
                );
            } else {
                render_cockpit_bottom(frame, area, app, 60);
            }
        }
        AgentCockpitLayout::Bottom => render_cockpit_bottom(frame, area, app, 60),
        AgentCockpitLayout::Wide => {
            if area.width >= 100 {
                let chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
                    .split(area);
                render_agent_terminal(frame, chunks[0], app);
                render_log(
                    frame,
                    chunks[1],
                    app,
                    app.active_daemon_session_id,
                    "Daemon Monitor",
                    pane_focused(app, PaneKind::DaemonMonitor),
                );
            } else {
                render_cockpit_bottom(frame, area, app, 65);
            }
        }
        AgentCockpitLayout::Focus => match app.focused_pane_kind() {
            Some(PaneKind::DaemonMonitor) => render_log(
                frame,
                area,
                app,
                app.active_daemon_session_id,
                "Daemon Monitor",
                true,
            ),
            _ => render_agent_terminal(frame, area, app),
        },
    }
}

fn render_cockpit_bottom(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &AppModel,
    agent_percent: u16,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(agent_percent),
            Constraint::Percentage(100 - agent_percent),
        ])
        .split(area);
    render_agent_terminal(frame, chunks[0], app);
    render_log(
        frame,
        chunks[1],
        app,
        app.active_daemon_session_id,
        "Daemon Monitor",
        pane_focused(app, PaneKind::DaemonMonitor),
    );
}

fn render_agent_terminal(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppModel) {
    let focused = pane_focused(app, PaneKind::AgentTerminal);
    let Some(terminal) = &app.agent_terminal else {
        frame.render_widget(Paragraph::new("Agent Terminal | no session"), area);
        return;
    };
    let input = if terminal.input_owner && !terminal.read_only {
        "owned"
    } else {
        "read-only"
    };
    let screen = if terminal.snapshot.alternate_screen {
        "alt"
    } else {
        "main"
    };
    let view = if app.scroll_mode && focused {
        "scroll"
    } else if terminal.is_following() {
        "live"
    } else {
        "paused"
    };
    let mut lines = vec![Line::from(vec![Span::styled(
        format!(
            "Agent Terminal | {input} {screen} {view} cur={},{}{}",
            terminal.snapshot.cursor_row,
            terminal.snapshot.cursor_col,
            focus_suffix(focused)
        ),
        Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
    )])];
    let content_height = area.height.saturating_sub(1);
    if terminal.initializing {
        lines.push(Line::from("agent terminal initializing"));
    } else {
        for row in terminal
            .snapshot
            .cells
            .iter()
            .take(usize::from(content_height))
        {
            lines.push(Line::from(
                row.iter().map(cell_span).collect::<Vec<Span<'_>>>(),
            ));
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
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
        "Daemon Switcher",
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
    )])];
    if app.daemon_sessions.is_empty() {
        lines.push(Line::from("no managed daemons"));
    }
    for session in &app.daemon_sessions {
        let marker = if Some(session.session_id) == app.daemon_switcher.selected_session_id {
            ">"
        } else if Some(session.session_id) == app.active_daemon_session_id {
            "*"
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
            "{marker} {name} {:?} monitor={}",
            session.process_state, session.monitor_profile
        )));
        lines.push(Line::from(format!("  {}", compact_path(&workspace, 58))));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_status(frame: &mut ratatui::Frame<'_>, area: Rect, app: &AppModel) {
    let prefix = if app.prefix_pending {
        "prefix"
    } else {
        "ready"
    };
    let scroll = app.active_view_label();
    let status = format!(
        " mode={:?} monitor={:?} host={} input={} view={} status={} ",
        app.mode,
        app.monitor_profile,
        app.host_connection.label(),
        prefix,
        scroll,
        app.status_message
    );
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
        | millrace_sessions_core::state::ProcessState::Stale => {
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

fn cell_span(cell: &TerminalCell) -> Span<'_> {
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
    Span::styled(cell.symbol.clone(), style)
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
