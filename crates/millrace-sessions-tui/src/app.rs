use std::collections::BTreeMap;

use crossterm::event::KeyEvent;
use millrace_sessions_core::{
    ids::{PaneId, SessionId, UiId},
    protocol::SessionSummary,
    state::{
        MonitorProfile, ProcessState, UiContext, UiDaemonHealth, UiDaemonRecoveryAction, UiMode,
    },
    workspace::WorkspaceIdentity,
};
use time::OffsetDateTime;

use crate::{
    keymap::{KeyAction, PrefixKeymap},
    pane::{
        AgentCockpitLayout, AgentTerminalPane, CommandOutput, CommandPalette, ConfirmationPrompt,
        DaemonConsoleLayout, DaemonSwitcherOverlay, HelpOverlay, LineLogPane, Pane, PaneKind,
    },
    terminal::TerminalSnapshot,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppModel {
    pub ui_id: UiId,
    pub mode: UiMode,
    pub monitor_profile: MonitorProfile,
    pub panes: Vec<Pane>,
    pub daemon_sessions: Vec<SessionSummary>,
    pub daemon_logs: BTreeMap<SessionId, LineLogPane>,
    pub console_layout: DaemonConsoleLayout,
    pub cockpit_layout: AgentCockpitLayout,
    pub active_pane_id: Option<PaneId>,
    pub active_daemon_session_id: Option<SessionId>,
    pub active_workspace: Option<WorkspaceIdentity>,
    pub agent_session_id: Option<SessionId>,
    pub agent_terminal: Option<AgentTerminalPane>,
    pub managed_daemon_session_ids: Vec<SessionId>,
    pub line_log: LineLogPane,
    pub keymap: PrefixKeymap,
    pub prefix_pending: bool,
    pub scroll_mode: bool,
    pub command_palette: CommandPalette,
    pub daemon_switcher: DaemonSwitcherOverlay,
    pub confirmation: Option<ConfirmationPrompt>,
    pub help_overlay: HelpOverlay,
    pub command_output: CommandOutput,
    pub host_connection: HostConnectionState,
    pub status_message: String,
}

impl AppModel {
    pub fn daemon_console_fixture(
        ui_id: UiId,
        daemon_session_id: SessionId,
        prior_lines: impl IntoIterator<Item = String>,
    ) -> Self {
        let pane = Pane::daemon_monitor("Daemon Monitor", Some(daemon_session_id));
        let active_pane_id = Some(pane.id);
        let line_log = LineLogPane::with_prior_lines(4000, prior_lines);
        let daemon_logs = BTreeMap::from([(daemon_session_id, line_log.clone())]);

        Self {
            ui_id,
            mode: UiMode::DaemonConsole,
            monitor_profile: MonitorProfile::Auto,
            panes: vec![pane, Pane::command_output()],
            daemon_sessions: Vec::new(),
            daemon_logs,
            console_layout: DaemonConsoleLayout::Single,
            cockpit_layout: AgentCockpitLayout::Right,
            active_pane_id,
            active_daemon_session_id: Some(daemon_session_id),
            active_workspace: None,
            agent_session_id: None,
            agent_terminal: None,
            managed_daemon_session_ids: vec![daemon_session_id],
            line_log,
            keymap: PrefixKeymap::default(),
            prefix_pending: false,
            scroll_mode: false,
            command_palette: CommandPalette::default_commands(),
            daemon_switcher: DaemonSwitcherOverlay::default(),
            confirmation: None,
            help_overlay: HelpOverlay::default(),
            command_output: CommandOutput::hidden(),
            host_connection: HostConnectionState::Connected,
            status_message: "ready".to_string(),
        }
    }

    pub fn daemon_console(
        ui_id: UiId,
        sessions: Vec<SessionSummary>,
        selected_daemon: Option<SessionId>,
        logs: BTreeMap<SessionId, Vec<String>>,
        layout: DaemonConsoleLayout,
        monitor_profile: MonitorProfile,
    ) -> Self {
        let selected = selected_daemon
            .filter(|session_id| {
                sessions
                    .iter()
                    .any(|session| session.session_id == *session_id)
            })
            .or_else(|| sessions.first().map(|session| session.session_id));
        let panes = panes_for_layout(layout, &sessions, selected);
        let active_pane_id = panes.iter().find(|pane| pane.focused).map(|pane| pane.id);
        let active_session = selected.and_then(|session_id| {
            sessions
                .iter()
                .find(|session| session.session_id == session_id)
        });
        let line_log = selected
            .and_then(|session_id| logs.get(&session_id))
            .cloned()
            .map(|lines| LineLogPane::with_prior_lines(4000, lines))
            .unwrap_or_default();
        let daemon_logs = logs
            .into_iter()
            .map(|(session_id, lines)| (session_id, LineLogPane::with_prior_lines(4000, lines)))
            .collect();

        let mut command_palette = CommandPalette::default_commands();
        command_palette.target = command_target_label_for(active_session);

        Self {
            ui_id,
            mode: UiMode::DaemonConsole,
            monitor_profile,
            panes,
            daemon_sessions: sessions.clone(),
            daemon_logs,
            console_layout: layout,
            cockpit_layout: AgentCockpitLayout::Right,
            active_pane_id,
            active_daemon_session_id: selected,
            active_workspace: active_session.and_then(|session| session.workspace.clone()),
            agent_session_id: None,
            agent_terminal: None,
            managed_daemon_session_ids: sessions.iter().map(|session| session.session_id).collect(),
            line_log,
            keymap: PrefixKeymap::default(),
            prefix_pending: false,
            scroll_mode: false,
            command_palette,
            daemon_switcher: DaemonSwitcherOverlay::default(),
            confirmation: None,
            help_overlay: HelpOverlay::default(),
            command_output: CommandOutput::hidden(),
            host_connection: HostConnectionState::Connected,
            status_message: daemon_status_message(&sessions, selected),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn agent_cockpit(
        ui_id: UiId,
        agent_session: SessionSummary,
        daemon_sessions: Vec<SessionSummary>,
        selected_daemon: Option<SessionId>,
        daemon_logs: BTreeMap<SessionId, Vec<String>>,
        agent_terminal: AgentTerminalPane,
        layout: AgentCockpitLayout,
        monitor_profile: MonitorProfile,
    ) -> Self {
        let selected = selected_daemon
            .filter(|session_id| {
                daemon_sessions
                    .iter()
                    .any(|session| session.session_id == *session_id)
            })
            .or_else(|| daemon_sessions.first().map(|session| session.session_id));
        let daemon = selected.and_then(|session_id| {
            daemon_sessions
                .iter()
                .find(|session| session.session_id == session_id)
        });
        let mut agent_pane = Pane::agent_terminal("Agent Terminal", Some(agent_session.session_id));
        agent_pane.focused = true;
        let mut daemon_pane = Pane::daemon_monitor("Daemon Monitor", selected);
        daemon_pane.focused = false;
        let active_pane_id = Some(agent_pane.id);
        let line_log = selected
            .and_then(|session_id| daemon_logs.get(&session_id))
            .cloned()
            .map(|lines| LineLogPane::with_prior_lines(4000, lines))
            .unwrap_or_default();
        let daemon_logs = daemon_logs
            .into_iter()
            .map(|(session_id, lines)| (session_id, LineLogPane::with_prior_lines(4000, lines)))
            .collect();

        Self {
            ui_id,
            mode: UiMode::AgentCockpit,
            monitor_profile,
            panes: vec![agent_pane, daemon_pane, Pane::command_output()],
            daemon_sessions: daemon_sessions.clone(),
            daemon_logs,
            console_layout: DaemonConsoleLayout::Single,
            cockpit_layout: layout,
            active_pane_id,
            active_daemon_session_id: selected,
            active_workspace: daemon.and_then(|session| session.workspace.clone()),
            agent_session_id: Some(agent_session.session_id),
            agent_terminal: Some(agent_terminal),
            managed_daemon_session_ids: daemon_sessions
                .iter()
                .map(|session| session.session_id)
                .collect(),
            line_log,
            keymap: PrefixKeymap::default(),
            prefix_pending: false,
            scroll_mode: false,
            command_palette: CommandPalette::default_commands(),
            daemon_switcher: DaemonSwitcherOverlay::default(),
            confirmation: None,
            help_overlay: HelpOverlay::default(),
            command_output: CommandOutput::hidden(),
            host_connection: HostConnectionState::Connected,
            status_message: daemon_status_message(&daemon_sessions, selected),
        }
    }

    pub fn handle_key(&mut self, event: KeyEvent, viewport_height: u16) -> KeyAction {
        if self.prefix_pending {
            self.prefix_pending = false;
            let action = self
                .keymap
                .prefix_action(event)
                .unwrap_or(KeyAction::Ignored);
            self.apply_action(&action, viewport_height);
            return action;
        }

        if self.keymap.is_prefix(event) {
            self.prefix_pending = true;
            return KeyAction::Prefix;
        }

        if self.scroll_mode {
            if let Some(action) = self.keymap.scroll_action(event) {
                self.apply_action(&action, viewport_height);
                return action;
            }
        }

        KeyAction::Input(event)
    }

    pub fn enter_scroll_mode(&mut self) {
        self.scroll_mode = true;
        self.status_message = "scroll".to_string();
    }

    pub fn exit_scroll_mode(&mut self) {
        self.scroll_mode = false;
        if self.focused_agent_terminal() {
            self.set_agent_terminal_following(true);
        } else {
            self.jump_active_log_bottom();
        }
        self.status_message = "live".to_string();
    }

    pub fn switch_focus(&mut self) {
        if self.panes.is_empty() {
            self.active_pane_id = None;
            return;
        }

        let current = self
            .active_pane_id
            .and_then(|id| self.panes.iter().position(|pane| pane.id == id))
            .unwrap_or(0);
        let next = (current + 1) % self.panes.len();
        for (index, pane) in self.panes.iter_mut().enumerate() {
            pane.focused = index == next;
        }
        self.active_pane_id = Some(self.panes[next].id);
        if self.panes[next].kind == PaneKind::DaemonMonitor {
            if let Some(session_id) = self.panes[next].session_id {
                self.select_daemon(session_id);
            }
        }
    }

    pub fn focused_pane_kind(&self) -> Option<PaneKind> {
        let active = self.active_pane_id?;
        self.panes
            .iter()
            .find(|pane| pane.id == active)
            .map(|pane| pane.kind.clone())
    }

    pub fn focused_agent_terminal(&self) -> bool {
        self.focused_pane_kind() == Some(PaneKind::AgentTerminal)
    }

    pub fn update_agent_terminal(&mut self, snapshot: TerminalSnapshot) {
        self.update_agent_terminal_view(snapshot, true);
    }

    pub fn update_agent_terminal_view(&mut self, snapshot: TerminalSnapshot, follow: bool) {
        match &mut self.agent_terminal {
            Some(terminal) => terminal.set_snapshot_view(snapshot, follow),
            None => {
                let mut terminal = AgentTerminalPane::with_snapshot(snapshot, false, true);
                terminal.set_following(follow);
                self.agent_terminal = Some(terminal);
            }
        }
    }

    pub fn set_agent_terminal_following(&mut self, follow: bool) {
        if let Some(terminal) = &mut self.agent_terminal {
            terminal.set_following(follow);
        }
    }

    pub fn set_agent_input_read_only(&mut self) {
        if let Some(terminal) = &mut self.agent_terminal {
            terminal.set_read_only();
        }
        self.status_message = "agent input read-only".to_string();
    }

    pub fn set_agent_input_owner(&mut self, input_owner: bool) {
        if let Some(terminal) = &mut self.agent_terminal {
            terminal.set_input_owner(input_owner);
        }
        self.status_message = if input_owner {
            "agent input owned".to_string()
        } else {
            "agent input read-only".to_string()
        };
    }

    pub fn agent_terminal_can_accept_input(&self) -> bool {
        self.agent_terminal
            .as_ref()
            .is_some_and(|terminal| terminal.input_owner && !terminal.read_only)
    }

    pub fn agent_terminal_is_following(&self) -> bool {
        self.agent_terminal
            .as_ref()
            .map_or(true, AgentTerminalPane::is_following)
    }

    pub fn resize_agent_terminal(&mut self, rows: u16, cols: u16) -> bool {
        let Some(terminal) = &mut self.agent_terminal else {
            return false;
        };
        let rows = rows.max(1);
        let cols = cols.max(1);
        if terminal.rows == rows && terminal.cols == cols {
            return false;
        }
        terminal.rows = rows;
        terminal.cols = cols;
        true
    }

    pub fn agent_terminal_size_for(&self, width: u16, height: u16) -> Option<(u16, u16)> {
        if self.mode != UiMode::AgentCockpit {
            return None;
        }
        let body_height = height.saturating_sub(1).max(1);
        let (pane_width, pane_height) = match self.cockpit_layout {
            AgentCockpitLayout::Right => {
                if width >= 100 {
                    (width.saturating_mul(60) / 100, body_height)
                } else {
                    (width, body_height.saturating_mul(60) / 100)
                }
            }
            AgentCockpitLayout::Bottom => (width, body_height.saturating_mul(60) / 100),
            AgentCockpitLayout::Wide => {
                if width >= 100 {
                    (width.saturating_mul(70) / 100, body_height)
                } else {
                    (width, body_height.saturating_mul(65) / 100)
                }
            }
            AgentCockpitLayout::Focus => {
                if self.focused_agent_terminal() {
                    (width, body_height)
                } else {
                    (width, body_height.saturating_mul(40) / 100)
                }
            }
        };
        Some((pane_height.saturating_sub(1).max(1), pane_width.max(1)))
    }

    pub fn select_daemon_by_offset(&mut self, delta: isize) -> bool {
        if self.daemon_sessions.is_empty() {
            return false;
        }
        let current = self
            .active_daemon_session_id
            .and_then(|session_id| {
                self.daemon_sessions
                    .iter()
                    .position(|session| session.session_id == session_id)
            })
            .unwrap_or(0);
        let len = self.daemon_sessions.len() as isize;
        let next = (current as isize + delta).rem_euclid(len) as usize;
        let session_id = self.daemon_sessions[next].session_id;
        self.select_daemon(session_id)
    }

    pub fn select_daemon(&mut self, session_id: SessionId) -> bool {
        let Some(index) = self
            .daemon_sessions
            .iter()
            .position(|session| session.session_id == session_id)
        else {
            return false;
        };
        self.active_daemon_session_id = Some(session_id);
        self.active_workspace = self.daemon_sessions[index].workspace.clone();
        self.monitor_profile = self.daemon_sessions[index].monitor_profile.clone();
        if let Some(log) = self.daemon_logs.get(&session_id).cloned() {
            self.line_log = log;
        }
        for pane in &mut self.panes {
            if pane.kind == PaneKind::DaemonMonitor {
                pane.session_id = Some(session_id);
            }
            pane.focused = pane.session_id == Some(session_id)
                && pane.kind == PaneKind::DaemonMonitor
                && self.mode == UiMode::DaemonConsole;
            if pane.focused {
                self.active_pane_id = Some(pane.id);
            }
        }
        if self.mode == UiMode::AgentCockpit {
            for pane in &mut self.panes {
                if Some(pane.id) == self.active_pane_id {
                    pane.focused = true;
                }
            }
        }
        self.command_palette.target = self.command_target_label();
        self.status_message =
            daemon_status_message(&self.daemon_sessions, self.active_daemon_session_id);
        true
    }

    pub fn replace_daemon_sessions(&mut self, sessions: Vec<SessionSummary>) {
        let previous = self.active_daemon_session_id;
        self.daemon_sessions = sessions;
        self.managed_daemon_session_ids = self
            .daemon_sessions
            .iter()
            .map(|session| session.session_id)
            .collect();
        self.active_daemon_session_id = previous
            .filter(|session_id| {
                self.daemon_sessions
                    .iter()
                    .any(|session| session.session_id == *session_id)
            })
            .or_else(|| {
                self.daemon_sessions
                    .first()
                    .map(|session| session.session_id)
            });

        if let Some(session_id) = self.active_daemon_session_id {
            if let Some(session) = self
                .daemon_sessions
                .iter()
                .find(|session| session.session_id == session_id)
            {
                self.active_workspace = session.workspace.clone();
                self.monitor_profile = session.monitor_profile.clone();
            }
            if let Some(log) = self.daemon_logs.get(&session_id).cloned() {
                self.line_log = log;
            }
            for pane in &mut self.panes {
                if pane.kind == PaneKind::DaemonMonitor {
                    pane.session_id = Some(session_id);
                }
            }
        } else {
            self.active_workspace = None;
        }

        self.command_palette.target = self.command_target_label();
        self.status_message =
            daemon_status_message(&self.daemon_sessions, self.active_daemon_session_id);
    }

    pub fn open_daemon_switcher(&mut self) {
        self.daemon_switcher
            .open_with(self.active_daemon_session_id);
        self.status_message = "daemon switcher".to_string();
    }

    pub fn close_daemon_switcher(&mut self) {
        self.daemon_switcher.close();
        self.status_message =
            daemon_status_message(&self.daemon_sessions, self.active_daemon_session_id);
    }

    pub fn move_daemon_switcher_selection(&mut self, delta: isize) -> bool {
        if self.daemon_sessions.is_empty() {
            return false;
        }
        let current = self
            .daemon_switcher
            .selected_session_id
            .or(self.active_daemon_session_id)
            .and_then(|session_id| {
                self.daemon_sessions
                    .iter()
                    .position(|session| session.session_id == session_id)
            })
            .unwrap_or(0);
        let len = self.daemon_sessions.len() as isize;
        let next = (current as isize + delta).rem_euclid(len) as usize;
        self.daemon_switcher.selected_session_id = Some(self.daemon_sessions[next].session_id);
        true
    }

    pub fn activate_daemon_switcher_selection(&mut self) -> bool {
        let Some(session_id) = self.daemon_switcher.selected_session_id else {
            return false;
        };
        self.daemon_switcher.close();
        self.select_daemon(session_id)
    }

    pub fn append_live_output(&mut self, line: impl Into<String>) {
        match self.active_daemon_session_id {
            Some(session_id) => self.append_live_output_for(session_id, line),
            None => self.line_log.append_live_line(line),
        }
    }

    pub fn append_live_output_for(&mut self, session_id: SessionId, line: impl Into<String>) {
        let line = line.into();
        self.daemon_logs
            .entry(session_id)
            .or_default()
            .append_live_line(line.clone());
        if self.active_daemon_session_id == Some(session_id) {
            self.line_log.append_live_line(line);
        }
    }

    pub fn replace_daemon_output(
        &mut self,
        session_id: SessionId,
        lines: impl IntoIterator<Item = String>,
    ) {
        let lines = lines.into_iter().collect::<Vec<_>>();
        self.daemon_logs
            .entry(session_id)
            .or_default()
            .replace_lines_preserving_view(lines.clone());
        if self.active_daemon_session_id == Some(session_id) {
            self.line_log.replace_lines_preserving_view(lines);
        }
    }

    pub fn selected_daemon(&self) -> Option<&SessionSummary> {
        let session_id = self.active_daemon_session_id?;
        self.daemon_sessions
            .iter()
            .find(|session| session.session_id == session_id)
    }

    pub fn command_target_label(&self) -> String {
        command_target_label_for(self.selected_daemon())
    }

    pub fn set_command_running(&mut self, argv: Vec<String>, target: impl Into<String>) {
        self.command_output = CommandOutput::running(argv, target);
    }

    pub fn set_command_success(
        &mut self,
        argv: Vec<String>,
        target: impl Into<String>,
        stdout: Vec<String>,
    ) {
        self.command_output = CommandOutput::succeeded(argv, target, stdout);
        self.status_message = "command succeeded".to_string();
    }

    pub fn set_command_failure(
        &mut self,
        argv: Vec<String>,
        target: impl Into<String>,
        stderr: Vec<String>,
    ) {
        self.command_output = CommandOutput::failed(argv, target, stderr);
        self.status_message = "command failed".to_string();
    }

    pub fn require_confirmation(
        &mut self,
        operation: impl Into<String>,
        target: impl Into<String>,
        challenge: impl Into<String>,
    ) {
        self.confirmation = Some(ConfirmationPrompt::new(operation, target, challenge));
    }

    pub fn set_host_reconnecting(&mut self, attempt: u32, message: impl Into<String>) {
        self.host_connection = HostConnectionState::Reconnecting {
            attempt,
            message: message.into(),
        };
    }

    pub fn set_host_connected(&mut self) {
        self.host_connection = HostConnectionState::Connected;
    }

    pub fn set_host_disconnected(&mut self, message: impl Into<String>) {
        self.host_connection = HostConnectionState::Disconnected {
            message: message.into(),
        };
    }

    pub fn ui_context(&self) -> UiContext {
        UiContext {
            schema_version: 1,
            ui_id: self.ui_id,
            mode: self.mode.clone(),
            active_pane_id: self.active_pane_id,
            active_daemon_session_id: self.active_daemon_session_id,
            active_workspace: self.active_workspace.clone(),
            agent_session_id: self.agent_session_id,
            managed_daemon_session_ids: self.managed_daemon_session_ids.clone(),
            monitor_profile: self.monitor_profile.clone(),
            daemon_health: self
                .daemon_sessions
                .iter()
                .map(daemon_health_from_summary)
                .collect(),
            updated_at: OffsetDateTime::now_utc(),
        }
    }

    pub fn active_view_label(&self) -> &'static str {
        if self.scroll_mode {
            "scroll"
        } else if self.focused_agent_terminal() {
            if self.agent_terminal_is_following() {
                "live"
            } else {
                "paused"
            }
        } else if self.line_log.is_following() {
            "live"
        } else {
            "paused"
        }
    }

    fn apply_action(&mut self, action: &KeyAction, viewport_height: u16) {
        match action {
            KeyAction::SwitchFocus => self.switch_focus(),
            KeyAction::EnterScrollMode => self.enter_scroll_mode(),
            KeyAction::ExitScrollMode => self.exit_scroll_mode(),
            KeyAction::OpenCommandPalette => {
                self.command_palette.target = self.command_target_label();
                self.command_palette.open = true;
            }
            KeyAction::OpenDaemonList => {
                self.open_daemon_switcher();
            }
            KeyAction::ToggleHelp => self.help_overlay.open = !self.help_overlay.open,
            KeyAction::Redraw => self.status_message = "redraw".to_string(),
            KeyAction::ScrollUp => self.scroll_active_view_up(viewport_height, 1),
            KeyAction::ScrollDown => self.scroll_active_view_down(1),
            KeyAction::PageUp => self.page_active_view_up(viewport_height),
            KeyAction::PageDown => self.page_active_view_down(viewport_height),
            KeyAction::JumpTop => self.jump_active_view_top(viewport_height),
            KeyAction::JumpBottom => {
                self.jump_active_view_bottom();
                self.scroll_mode = false;
            }
            KeyAction::Escape => self.exit_scroll_mode(),
            KeyAction::Detach
            | KeyAction::CloseRequested
            | KeyAction::BeginSearch
            | KeyAction::NextSearch
            | KeyAction::PreviousSearch
            | KeyAction::Prefix
            | KeyAction::Input(_)
            | KeyAction::Ignored => {}
        }
    }

    fn scroll_active_view_up(&mut self, viewport_height: u16, lines: usize) {
        if self.focused_agent_terminal() {
            self.set_agent_terminal_following(false);
            return;
        }
        self.scroll_active_log_up(viewport_height, lines);
    }

    fn scroll_active_view_down(&mut self, lines: usize) {
        if self.focused_agent_terminal() {
            self.set_agent_terminal_following(false);
            return;
        }
        self.scroll_active_log_down(lines);
    }

    fn page_active_view_up(&mut self, viewport_height: u16) {
        if self.focused_agent_terminal() {
            self.set_agent_terminal_following(false);
            return;
        }
        self.page_active_log_up(viewport_height);
    }

    fn page_active_view_down(&mut self, viewport_height: u16) {
        if self.focused_agent_terminal() {
            self.set_agent_terminal_following(false);
            return;
        }
        self.page_active_log_down(viewport_height);
    }

    fn jump_active_view_top(&mut self, viewport_height: u16) {
        if self.focused_agent_terminal() {
            self.set_agent_terminal_following(false);
            return;
        }
        self.jump_active_log_top(viewport_height);
    }

    fn jump_active_view_bottom(&mut self) {
        if self.focused_agent_terminal() {
            self.set_agent_terminal_following(true);
            return;
        }
        self.jump_active_log_bottom();
    }

    fn scroll_active_log_up(&mut self, viewport_height: u16, lines: usize) {
        self.line_log.scroll_up(viewport_height, lines);
        if let Some(log) = self.active_daemon_log_mut() {
            log.scroll_up(viewport_height, lines);
        }
    }

    fn scroll_active_log_down(&mut self, lines: usize) {
        self.line_log.scroll_down(lines);
        if let Some(log) = self.active_daemon_log_mut() {
            log.scroll_down(lines);
        }
    }

    fn page_active_log_up(&mut self, viewport_height: u16) {
        self.line_log.page_up(viewport_height);
        if let Some(log) = self.active_daemon_log_mut() {
            log.page_up(viewport_height);
        }
    }

    fn page_active_log_down(&mut self, viewport_height: u16) {
        self.line_log.page_down(viewport_height);
        if let Some(log) = self.active_daemon_log_mut() {
            log.page_down(viewport_height);
        }
    }

    fn jump_active_log_top(&mut self, viewport_height: u16) {
        self.line_log.jump_top(viewport_height);
        if let Some(log) = self.active_daemon_log_mut() {
            log.jump_top(viewport_height);
        }
    }

    fn jump_active_log_bottom(&mut self) {
        self.line_log.jump_bottom();
        if let Some(log) = self.active_daemon_log_mut() {
            log.jump_bottom();
        }
    }

    fn active_daemon_log_mut(&mut self) -> Option<&mut LineLogPane> {
        let session_id = self.active_daemon_session_id?;
        self.daemon_logs.get_mut(&session_id)
    }
}

fn panes_for_layout(
    layout: DaemonConsoleLayout,
    sessions: &[SessionSummary],
    selected: Option<SessionId>,
) -> Vec<Pane> {
    let mut panes = Vec::new();
    let selected = selected.or_else(|| sessions.first().map(|session| session.session_id));
    match layout {
        DaemonConsoleLayout::Single => {
            if let Some(session) = selected.and_then(|id| find_session(sessions, id)) {
                panes.push(monitor_pane_for(session, true));
            }
        }
        DaemonConsoleLayout::Split => {
            for session in visible_sessions(sessions, selected, 2) {
                panes.push(monitor_pane_for(
                    session,
                    Some(session.session_id) == selected,
                ));
            }
        }
        DaemonConsoleLayout::Grid => {
            for session in visible_sessions(sessions, selected, 4) {
                panes.push(monitor_pane_for(
                    session,
                    Some(session.session_id) == selected,
                ));
            }
        }
        DaemonConsoleLayout::List => {
            let mut list = Pane::daemon_list();
            list.focused = selected.is_none();
            panes.push(list);
            if let Some(session) = selected.and_then(|id| find_session(sessions, id)) {
                panes.push(monitor_pane_for(session, true));
            }
        }
    }
    panes.push(Pane::command_output());
    panes
}

fn visible_sessions(
    sessions: &[SessionSummary],
    selected: Option<SessionId>,
    limit: usize,
) -> Vec<&SessionSummary> {
    let mut visible = Vec::new();
    if let Some(selected_session) = selected.and_then(|id| find_session(sessions, id)) {
        visible.push(selected_session);
    }
    for session in sessions {
        if visible.len() >= limit {
            break;
        }
        if Some(session.session_id) != selected {
            visible.push(session);
        }
    }
    visible
}

fn find_session(sessions: &[SessionSummary], session_id: SessionId) -> Option<&SessionSummary> {
    sessions
        .iter()
        .find(|session| session.session_id == session_id)
}

fn monitor_pane_for(session: &SessionSummary, focused: bool) -> Pane {
    let title = session
        .name
        .clone()
        .unwrap_or_else(|| session.session_id.to_string());
    let mut pane = Pane::daemon_monitor(title, Some(session.session_id));
    pane.focused = focused;
    pane
}

fn command_target_label_for(session: Option<&SessionSummary>) -> String {
    match session {
        Some(session) => session
            .workspace
            .as_ref()
            .map(|workspace| workspace.canonical_path.display().to_string())
            .unwrap_or_else(|| session.session_id.to_string()),
        None => "no daemon selected".to_string(),
    }
}

fn daemon_status_message(sessions: &[SessionSummary], selected: Option<SessionId>) -> String {
    if sessions.is_empty() {
        return "no daemons".to_string();
    }

    if let Some(session) = selected.and_then(|session_id| find_session(sessions, session_id)) {
        if !daemon_state_is_healthy(&session.process_state) {
            return format!("degraded {}", process_state_label(&session.process_state));
        }
    }

    let degraded_count = sessions
        .iter()
        .filter(|session| !daemon_state_is_healthy(&session.process_state))
        .count();
    if degraded_count == 0 {
        "ready".to_string()
    } else {
        format!("degraded daemons={degraded_count}")
    }
}

fn daemon_health_from_summary(session: &SessionSummary) -> UiDaemonHealth {
    UiDaemonHealth {
        session_id: session.session_id,
        process_state: session.process_state.clone(),
        attention_state: session.attention_state.clone(),
        failure_message: session.failure_message.clone(),
        recovery_actions: daemon_recovery_actions(&session.process_state),
    }
}

fn daemon_state_is_healthy(state: &ProcessState) -> bool {
    matches!(state, ProcessState::Starting | ProcessState::Running)
}

fn daemon_recovery_actions(state: &ProcessState) -> Vec<UiDaemonRecoveryAction> {
    match state {
        ProcessState::Starting | ProcessState::Running => vec![
            UiDaemonRecoveryAction::Inspect,
            UiDaemonRecoveryAction::Logs,
            UiDaemonRecoveryAction::Stop,
            UiDaemonRecoveryAction::Kill,
        ],
        ProcessState::FailedStart => vec![
            UiDaemonRecoveryAction::Inspect,
            UiDaemonRecoveryAction::Logs,
            UiDaemonRecoveryAction::Doctor,
            UiDaemonRecoveryAction::Delete,
        ],
        ProcessState::Exited | ProcessState::Killed => vec![
            UiDaemonRecoveryAction::Inspect,
            UiDaemonRecoveryAction::Logs,
            UiDaemonRecoveryAction::Archive,
            UiDaemonRecoveryAction::Delete,
        ],
        ProcessState::Crashed | ProcessState::Failed | ProcessState::Lost | ProcessState::Stale => {
            vec![
                UiDaemonRecoveryAction::Inspect,
                UiDaemonRecoveryAction::Logs,
                UiDaemonRecoveryAction::Doctor,
                UiDaemonRecoveryAction::Archive,
                UiDaemonRecoveryAction::Delete,
            ]
        }
    }
}

fn process_state_label(state: &ProcessState) -> &'static str {
    match state {
        ProcessState::Starting => "starting",
        ProcessState::Running => "running",
        ProcessState::Exited => "exited",
        ProcessState::Crashed => "crashed",
        ProcessState::Killed => "killed",
        ProcessState::FailedStart => "failed_start",
        ProcessState::Failed => "failed",
        ProcessState::Lost => "lost",
        ProcessState::Stale => "stale",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostConnectionState {
    Connected,
    Reconnecting { attempt: u32, message: String },
    Disconnected { message: String },
}

impl HostConnectionState {
    pub fn label(&self) -> String {
        match self {
            Self::Connected => "host connected".to_string(),
            Self::Reconnecting { attempt, message } => {
                format!("host reconnecting attempt={attempt} {message}")
            }
            Self::Disconnected { message } => format!("host disconnected {message}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers};
    use millrace_sessions_core::protocol::{SessionArtifacts, SessionCapabilities};
    use millrace_sessions_core::state::{
        AttentionState, ProcessState, SessionRole, SpawnMode, UiDaemonRecoveryAction,
    };

    use super::*;

    fn app() -> AppModel {
        AppModel::daemon_console_fixture(
            UiId::new(),
            SessionId::new(),
            ["one", "two", "three"].map(str::to_string),
        )
    }

    #[test]
    fn app_handles_prefix_scroll_keys() {
        let mut app = app();

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL), 2),
            KeyAction::Prefix
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE), 2),
            KeyAction::EnterScrollMode
        );
        assert!(app.scroll_mode);

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), 2),
            KeyAction::ScrollUp
        );
        assert!(app.line_log.is_scrolled());

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), 2),
            KeyAction::JumpBottom
        );
        assert!(!app.scroll_mode);
        assert!(app.line_log.is_following());
    }

    #[test]
    fn app_context_reflects_visible_daemon() {
        let app = app();
        let context = app.ui_context();

        assert_eq!(context.mode, UiMode::DaemonConsole);
        assert_eq!(
            context.active_daemon_session_id,
            app.active_daemon_session_id
        );
        assert_eq!(context.managed_daemon_session_ids.len(), 1);
    }

    #[test]
    fn daemon_console_layout_tracks_selection_and_context() {
        let first = summary("first");
        let second = summary("second");
        let second_id = second.session_id;
        let app = AppModel::daemon_console(
            UiId::new(),
            vec![first, second],
            Some(second_id),
            BTreeMap::from([(second_id, vec!["ready".to_string()])]),
            DaemonConsoleLayout::List,
            MonitorProfile::Basic,
        );

        assert_eq!(app.active_daemon_session_id, Some(second_id));
        assert_eq!(app.managed_daemon_session_ids.len(), 2);
        assert_eq!(app.console_layout, DaemonConsoleLayout::List);
        assert!(app.command_target_label().contains("second"));
        assert_eq!(app.ui_context().active_daemon_session_id, Some(second_id));
    }

    #[test]
    fn daemon_console_switch_focus_updates_selected_daemon() {
        let first = summary("first");
        let second = summary("second");
        let first_id = first.session_id;
        let second_id = second.session_id;
        let mut app = AppModel::daemon_console(
            UiId::new(),
            vec![first, second],
            Some(first_id),
            BTreeMap::new(),
            DaemonConsoleLayout::Split,
            MonitorProfile::Basic,
        );

        app.switch_focus();

        assert_eq!(app.active_daemon_session_id, Some(second_id));
    }

    #[test]
    fn agent_cockpit_context_tracks_agent_and_visible_daemon() {
        let daemon = summary("daemon");
        let daemon_id = daemon.session_id;
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let agent_id = agent.session_id;
        let app = AppModel::agent_cockpit(
            UiId::new(),
            agent,
            vec![daemon],
            Some(daemon_id),
            BTreeMap::new(),
            AgentTerminalPane::new(10, 40, true, false),
            AgentCockpitLayout::Right,
            MonitorProfile::Basic,
        );

        let context = app.ui_context();

        assert_eq!(context.mode, UiMode::AgentCockpit);
        assert_eq!(context.agent_session_id, Some(agent_id));
        assert_eq!(context.active_daemon_session_id, Some(daemon_id));
        assert_eq!(
            context.daemon_health[0].process_state,
            ProcessState::Running
        );
    }

    #[test]
    fn app_context_and_status_surface_degraded_daemon_states() {
        let mut failed_start = summary("failed");
        failed_start.process_state = ProcessState::FailedStart;
        failed_start.attention_state = AttentionState::NeedsAttention;
        failed_start.failure_message = Some("failed to spawn pty child".to_string());
        let mut exited = summary("exited");
        exited.process_state = ProcessState::Exited;
        let mut killed = summary("killed");
        killed.process_state = ProcessState::Killed;
        let mut stale = summary("stale");
        stale.process_state = ProcessState::Stale;
        let failed_id = failed_start.session_id;

        let app = AppModel::daemon_console(
            UiId::new(),
            vec![failed_start, exited, killed, stale],
            Some(failed_id),
            BTreeMap::new(),
            DaemonConsoleLayout::List,
            MonitorProfile::Basic,
        );

        assert_eq!(app.status_message, "degraded failed_start");
        let context = app.ui_context();
        let states = context
            .daemon_health
            .iter()
            .map(|daemon| daemon.process_state.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            states,
            vec![
                ProcessState::FailedStart,
                ProcessState::Exited,
                ProcessState::Killed,
                ProcessState::Stale,
            ]
        );
        assert!(context.daemon_health[0]
            .recovery_actions
            .contains(&UiDaemonRecoveryAction::Doctor));
    }

    #[test]
    fn agent_cockpit_input_conflict_marks_terminal_read_only() {
        let daemon = summary("daemon");
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let mut app = AppModel::agent_cockpit(
            UiId::new(),
            agent,
            vec![daemon],
            None,
            BTreeMap::new(),
            AgentTerminalPane::new(10, 40, true, false),
            AgentCockpitLayout::Right,
            MonitorProfile::Basic,
        );

        assert!(app.agent_terminal_can_accept_input());
        app.set_agent_input_read_only();

        assert!(!app.agent_terminal_can_accept_input());
        assert_eq!(app.status_message, "agent input read-only");
    }

    #[test]
    fn agent_cockpit_can_recover_input_ownership_after_read_only_attach() {
        let daemon = summary("daemon");
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let mut app = AppModel::agent_cockpit(
            UiId::new(),
            agent,
            vec![daemon],
            None,
            BTreeMap::new(),
            AgentTerminalPane::new(10, 40, false, true),
            AgentCockpitLayout::Right,
            MonitorProfile::Basic,
        );

        assert!(!app.agent_terminal_can_accept_input());
        app.set_agent_input_owner(true);

        assert!(app.agent_terminal_can_accept_input());
        assert_eq!(app.status_message, "agent input owned");
    }

    #[test]
    fn agent_cockpit_resize_calculates_agent_pane_size() {
        let daemon = summary("daemon");
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let mut app = AppModel::agent_cockpit(
            UiId::new(),
            agent,
            vec![daemon],
            None,
            BTreeMap::new(),
            AgentTerminalPane::new(10, 40, true, false),
            AgentCockpitLayout::Wide,
            MonitorProfile::Basic,
        );

        assert_eq!(app.agent_terminal_size_for(120, 30), Some((28, 84)));
        assert!(app.resize_agent_terminal(28, 84));
        assert!(!app.resize_agent_terminal(28, 84));
    }

    #[test]
    fn agent_cockpit_scroll_keys_pause_agent_view_without_scrolling_daemon_log() {
        let daemon = summary("daemon");
        let daemon_id = daemon.session_id;
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let mut app = AppModel::agent_cockpit(
            UiId::new(),
            agent,
            vec![daemon],
            Some(daemon_id),
            BTreeMap::from([(
                daemon_id,
                vec!["daemon one".to_string(), "daemon two".to_string()],
            )]),
            AgentTerminalPane::new(10, 40, true, false),
            AgentCockpitLayout::Right,
            MonitorProfile::Basic,
        );

        assert!(app.line_log.is_following());
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL), 2),
            KeyAction::Prefix
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE), 2),
            KeyAction::EnterScrollMode
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), 2),
            KeyAction::ScrollUp
        );

        assert!(app.line_log.is_following());
        assert!(app.agent_terminal.as_ref().unwrap().is_scrolled());

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), 2),
            KeyAction::JumpBottom
        );
        assert!(app.agent_terminal.as_ref().unwrap().is_following());
    }

    #[test]
    fn agent_cockpit_switcher_changes_visible_daemon_without_agent_turnover() {
        let mut first = summary("first");
        first.monitor_profile = MonitorProfile::Raw;
        let first_id = first.session_id;
        let mut second = summary("second");
        second.monitor_profile = MonitorProfile::Other("future".to_string());
        let second_id = second.session_id;
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let agent_id = agent.session_id;
        let mut app = AppModel::agent_cockpit(
            UiId::new(),
            agent,
            vec![first, second],
            Some(first_id),
            BTreeMap::new(),
            AgentTerminalPane::new(10, 40, true, false),
            AgentCockpitLayout::Right,
            MonitorProfile::Raw,
        );

        app.open_daemon_switcher();
        assert!(app.daemon_switcher.open);
        assert!(app.move_daemon_switcher_selection(1));
        assert!(app.activate_daemon_switcher_selection());

        let context = app.ui_context();
        assert_eq!(context.agent_session_id, Some(agent_id));
        assert_eq!(context.active_daemon_session_id, Some(second_id));
        assert_eq!(
            context.managed_daemon_session_ids,
            vec![first_id, second_id]
        );
        assert_eq!(
            context.monitor_profile,
            MonitorProfile::Other("future".to_string())
        );
    }

    fn summary(name: &str) -> SessionSummary {
        let cwd = std::path::PathBuf::from(format!("/tmp/{name}"));
        SessionSummary {
            session_id: SessionId::new(),
            name: Some(name.to_string()),
            role: SessionRole::MillraceDaemon,
            spawn_mode: SpawnMode::Pty,
            process_state: ProcessState::Running,
            attention_state: AttentionState::MillraceIdle,
            failure_message: None,
            workspace: Some(WorkspaceIdentity {
                canonical_path: cwd.clone(),
                unix_device: None,
                unix_inode: None,
            }),
            cwd,
            argv: vec![
                "millrace".to_string(),
                "run".to_string(),
                "daemon".to_string(),
            ],
            monitor_profile: MonitorProfile::Auto,
            created_at: "2026-05-26T00:00:00Z".to_string(),
            updated_at: "2026-05-26T00:00:01Z".to_string(),
            stop_requested_at: None,
            stop_reason: None,
            attached_clients: 0,
            input_owner: None,
            capabilities: SessionCapabilities::for_spawn_mode(SpawnMode::Pty),
            artifacts: SessionArtifacts::default(),
        }
    }
}
