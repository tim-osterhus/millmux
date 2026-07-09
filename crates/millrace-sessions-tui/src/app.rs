use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use millrace_sessions_core::{
    ids::{PaneId, SessionId, UiId},
    protocol::SessionSummary,
    state::{
        AttentionState, LivenessState, MonitorProfile, ProcessState, SessionRole, UiContext,
        UiDaemonHealth, UiDaemonRecoveryAction, UiMode,
    },
    workspace::{GitWorktreeIdentity, WorkspaceIdentity},
};
use time::OffsetDateTime;

use crate::{
    keymap::{KeyAction, PrefixKeymap},
    pane::{
        AgentCockpitLayout, AgentTerminalPane, CommandOutput, CommandPalette, ConfirmationPrompt,
        DaemonConsoleLayout, DaemonSwitcherOverlay, HelpOverlay, LineLogPane, Pane, PaneKind,
        WorkspaceSessionRow, COCKPIT_SESSION_LIST_HEIGHT,
    },
    terminal::TerminalSnapshot,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppModel {
    pub ui_id: UiId,
    pub mode: UiMode,
    pub monitor_profile: MonitorProfile,
    pub panes: Vec<Pane>,
    pub workspace_sessions: Vec<SessionSummary>,
    pub workspace_worktrees: BTreeMap<SessionId, Option<GitWorktreeIdentity>>,
    pub daemon_sessions: Vec<SessionSummary>,
    pub daemon_logs: BTreeMap<SessionId, LineLogPane>,
    pub console_layout: DaemonConsoleLayout,
    pub cockpit_layout: AgentCockpitLayout,
    pub active_pane_id: Option<PaneId>,
    pub selected_session_id: Option<SessionId>,
    pub active_daemon_session_id: Option<SessionId>,
    pub active_workspace: Option<WorkspaceIdentity>,
    pub agent_session_id: Option<SessionId>,
    pub managed_session_ids: Vec<SessionId>,
    pub agent_terminal: Option<AgentTerminalPane>,
    pub managed_daemon_session_ids: Vec<SessionId>,
    pub line_log: LineLogPane,
    pub keymap: PrefixKeymap,
    pub prefix_pending: bool,
    pub scroll_mode: bool,
    pub search_mode: bool,
    pub search_query: String,
    pub copy_buffer: Option<String>,
    pub command_palette: CommandPalette,
    pub daemon_switcher: DaemonSwitcherOverlay,
    pub confirmation: Option<ConfirmationPrompt>,
    pub help_overlay: HelpOverlay,
    pub command_output: CommandOutput,
    pub host_connection: HostConnectionState,
    pub status_message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceSessionSelection {
    Missing,
    DaemonSelected(SessionId),
    AttachSelected(SessionId),
    NotAttachable(SessionId),
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
            workspace_sessions: Vec::new(),
            workspace_worktrees: BTreeMap::new(),
            daemon_sessions: Vec::new(),
            daemon_logs,
            console_layout: DaemonConsoleLayout::Single,
            cockpit_layout: AgentCockpitLayout::Right,
            active_pane_id,
            selected_session_id: Some(daemon_session_id),
            active_daemon_session_id: Some(daemon_session_id),
            active_workspace: None,
            agent_session_id: None,
            managed_session_ids: vec![daemon_session_id],
            agent_terminal: None,
            managed_daemon_session_ids: vec![daemon_session_id],
            line_log,
            keymap: PrefixKeymap::default(),
            prefix_pending: false,
            scroll_mode: false,
            search_mode: false,
            search_query: String::new(),
            copy_buffer: None,
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
        let sessions = sort_workspace_sessions(sessions);
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
            workspace_worktrees: worktrees_for_sessions(&sessions),
            workspace_sessions: sessions.clone(),
            daemon_sessions: sessions.clone(),
            daemon_logs,
            console_layout: layout,
            cockpit_layout: AgentCockpitLayout::Right,
            active_pane_id,
            selected_session_id: selected,
            active_daemon_session_id: selected,
            active_workspace: active_session.and_then(|session| session.workspace.clone()),
            agent_session_id: None,
            managed_session_ids: sessions.iter().map(|session| session.session_id).collect(),
            agent_terminal: None,
            managed_daemon_session_ids: sessions.iter().map(|session| session.session_id).collect(),
            line_log,
            keymap: PrefixKeymap::default(),
            prefix_pending: false,
            scroll_mode: false,
            search_mode: false,
            search_query: String::new(),
            copy_buffer: None,
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
        let mut workspace_sessions = daemon_sessions.clone();
        upsert_session(&mut workspace_sessions, agent_session.clone());
        let workspace_sessions = sort_workspace_sessions(workspace_sessions);
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
            workspace_worktrees: worktrees_for_sessions(&workspace_sessions),
            workspace_sessions,
            daemon_sessions: daemon_sessions.clone(),
            daemon_logs,
            console_layout: DaemonConsoleLayout::Single,
            cockpit_layout: layout,
            active_pane_id,
            selected_session_id: Some(agent_session.session_id),
            active_daemon_session_id: selected,
            active_workspace: daemon
                .and_then(|session| session.workspace.clone())
                .or_else(|| agent_session.workspace.clone()),
            agent_session_id: Some(agent_session.session_id),
            managed_session_ids: daemon_sessions
                .iter()
                .map(|session| session.session_id)
                .chain(std::iter::once(agent_session.session_id))
                .collect(),
            agent_terminal: Some(agent_terminal),
            managed_daemon_session_ids: daemon_sessions
                .iter()
                .map(|session| session.session_id)
                .collect(),
            line_log,
            keymap: PrefixKeymap::default(),
            prefix_pending: false,
            scroll_mode: false,
            search_mode: false,
            search_query: String::new(),
            copy_buffer: None,
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

        if self.search_mode {
            let action = self.search_key_action(event);
            self.apply_action(&action, viewport_height);
            return action;
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
        self.search_mode = false;
        self.search_query.clear();
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
        self.update_selected_session_from_focus();
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

    pub fn focused_pane_kind_label(&self) -> Option<&'static str> {
        Some(match self.focused_pane_kind()? {
            PaneKind::AgentTerminal => "agent_terminal",
            PaneKind::DaemonMonitor => "daemon_monitor",
            PaneKind::DaemonList => "daemon_list",
            PaneKind::CommandOutput => "command_output",
            PaneKind::StatusBar => "status_bar",
            PaneKind::HelpOverlay => "help_overlay",
            PaneKind::CommandPalette => "command_palette",
        })
    }

    pub fn focused_session_id(&self) -> Option<SessionId> {
        match self.focused_pane_kind()? {
            PaneKind::AgentTerminal => self.agent_session_id,
            PaneKind::DaemonMonitor => self.active_daemon_session_id,
            _ => None,
        }
    }

    pub fn active_attach_session_id(&self) -> Option<SessionId> {
        self.agent_session_id
    }

    pub fn focus_pane_kind(&mut self, kind: PaneKind) -> bool {
        let Some(index) = self.panes.iter().position(|pane| pane.kind == kind) else {
            return false;
        };
        for (pane_index, pane) in self.panes.iter_mut().enumerate() {
            pane.focused = pane_index == index;
        }
        self.active_pane_id = Some(self.panes[index].id);
        self.update_selected_session_from_focus();
        true
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

    pub fn reset_agent_terminal(
        &mut self,
        rows: u16,
        cols: u16,
        input_owner: bool,
        read_only: bool,
    ) {
        self.agent_terminal = Some(AgentTerminalPane::new(rows, cols, input_owner, read_only));
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

    pub fn begin_search_mode(&mut self) {
        self.scroll_mode = true;
        self.search_mode = true;
        self.search_query.clear();
        self.status_message = "search:".to_string();
    }

    pub fn copy_buffer_text(&self) -> Option<&str> {
        self.copy_buffer.as_deref()
    }

    pub fn refresh_agent_search_from_snapshot(&mut self, label: &str) {
        if self.search_query.is_empty() {
            self.status_message = "search:".to_string();
            return;
        }
        let query = self.search_query.clone();
        let found = self
            .agent_terminal
            .as_mut()
            .and_then(|terminal| terminal.search(query.clone()));
        self.status_message = match found {
            Some(found) => format!("{label}: {} line {}", query, found.index + 1),
            None => format!("{label}: {} not found", query),
        };
    }

    pub fn set_agent_search_not_found(&mut self, label: &str) {
        if self.search_query.is_empty() {
            self.status_message = "search:".to_string();
        } else {
            self.status_message = format!("{label}: {} not found", self.search_query);
        }
    }

    fn search_key_action(&self, event: KeyEvent) -> KeyAction {
        let mut modifiers = event.modifiers;
        if matches!(event.code, KeyCode::Char(_)) {
            modifiers.remove(KeyModifiers::SHIFT);
        }

        match event.code {
            KeyCode::Esc => KeyAction::Escape,
            KeyCode::Enter if modifiers.is_empty() => KeyAction::CopySearchMatch,
            KeyCode::Backspace if modifiers.is_empty() => KeyAction::SearchBackspace,
            KeyCode::Down if modifiers.is_empty() => KeyAction::NextSearch,
            KeyCode::Up if modifiers.is_empty() => KeyAction::PreviousSearch,
            KeyCode::Char(value) if modifiers.is_empty() => KeyAction::SearchInput(value),
            _ => KeyAction::Ignored,
        }
    }

    fn push_search_input(&mut self, value: char) {
        self.search_query.push(value);
        self.search_active_view();
    }

    fn pop_search_input(&mut self) {
        self.search_query.pop();
        self.search_active_view();
    }

    fn search_active_view(&mut self) {
        if self.search_query.is_empty() {
            self.status_message = "search:".to_string();
            return;
        }

        let query = self.search_query.clone();
        let found = if self.focused_agent_terminal() {
            self.agent_terminal
                .as_mut()
                .and_then(|terminal| terminal.search(query.clone()))
        } else {
            let result = self.line_log.search(query.clone());
            if let Some(log) = self.active_daemon_log_mut() {
                let _ = log.search(query.clone());
            }
            result
        };
        self.status_message = match found {
            Some(found) => format!("search: {} line {}", query, found.index + 1),
            None => format!("search: {} not found", query),
        };
    }

    fn next_search_match(&mut self) {
        let found = if self.focused_agent_terminal() {
            self.agent_terminal
                .as_mut()
                .and_then(AgentTerminalPane::next_match)
        } else {
            let result = self.line_log.next_match();
            if let Some(log) = self.active_daemon_log_mut() {
                let _ = log.next_match();
            }
            result
        };
        self.status_message = match found {
            Some(found) => format!("search next: line {}", found.index + 1),
            None => "search next: no match".to_string(),
        };
    }

    fn previous_search_match(&mut self) {
        let found = if self.focused_agent_terminal() {
            self.agent_terminal
                .as_mut()
                .and_then(AgentTerminalPane::previous_match)
        } else {
            let result = self.line_log.previous_match();
            if let Some(log) = self.active_daemon_log_mut() {
                let _ = log.previous_match();
            }
            result
        };
        self.status_message = match found {
            Some(found) => format!("search previous: line {}", found.index + 1),
            None => "search previous: no match".to_string(),
        };
    }

    fn copy_current_search_match(&mut self) {
        let found = if self.focused_agent_terminal() {
            self.agent_terminal
                .as_ref()
                .and_then(AgentTerminalPane::current_match)
        } else {
            self.line_log.current_match()
        };
        match found {
            Some(found) => {
                self.copy_buffer = Some(found.line);
                self.search_mode = false;
                self.status_message = "copied search match".to_string();
            }
            None => {
                self.status_message = "copy: no search match".to_string();
            }
        }
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
        let (width, body_height) =
            cockpit_content_size(width, body_height, self.workspace_sessions.len());
        let (pane_width, pane_height) = match self.cockpit_layout {
            AgentCockpitLayout::Right => {
                if width >= 64 {
                    (width.saturating_mul(55) / 100, body_height)
                } else {
                    (width, body_height.saturating_mul(60) / 100)
                }
            }
            AgentCockpitLayout::Bottom => (width, body_height.saturating_mul(60) / 100),
            AgentCockpitLayout::Wide => {
                if width >= 64 {
                    (width.saturating_mul(65) / 100, body_height)
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

    pub fn select_workspace_session(&mut self, session_id: SessionId) -> WorkspaceSessionSelection {
        let selection = self.workspace_session_selection(session_id);
        if !matches!(
            selection,
            WorkspaceSessionSelection::DaemonSelected(_)
                | WorkspaceSessionSelection::AttachSelected(_)
        ) {
            match selection {
                WorkspaceSessionSelection::Missing => {
                    self.status_message = "session missing".to_string();
                }
                WorkspaceSessionSelection::NotAttachable(_) => {
                    self.status_message = "session not attachable".to_string();
                }
                _ => {}
            }
            return selection;
        }

        let Some(session) = self
            .workspace_sessions
            .iter()
            .find(|session| session.session_id == session_id)
            .cloned()
        else {
            return WorkspaceSessionSelection::Missing;
        };

        self.selected_session_id = Some(session_id);
        if session.role == SessionRole::MillraceDaemon {
            if self.select_daemon(session_id) {
                self.focus_pane_kind(PaneKind::DaemonMonitor);
                self.selected_session_id = Some(session_id);
                self.status_message = format!(
                    "daemon selected {}",
                    session.name.as_deref().unwrap_or("unnamed")
                );
                return selection;
            }
            return WorkspaceSessionSelection::Missing;
        }

        self.agent_session_id = Some(session_id);
        for pane in &mut self.panes {
            if pane.kind == PaneKind::AgentTerminal {
                pane.session_id = Some(session_id);
            }
        }
        self.focus_pane_kind(PaneKind::AgentTerminal);
        self.selected_session_id = Some(session_id);
        self.status_message = format!(
            "session selected {}",
            session.name.as_deref().unwrap_or("unnamed")
        );
        selection
    }

    pub fn workspace_session_selection(&self, session_id: SessionId) -> WorkspaceSessionSelection {
        let Some(session) = self
            .workspace_sessions
            .iter()
            .find(|session| session.session_id == session_id)
        else {
            return WorkspaceSessionSelection::Missing;
        };
        if session.role == SessionRole::MillraceDaemon {
            return WorkspaceSessionSelection::DaemonSelected(session_id);
        }
        if session_can_be_attached(session) {
            WorkspaceSessionSelection::AttachSelected(session_id)
        } else {
            WorkspaceSessionSelection::NotAttachable(session_id)
        }
    }

    pub fn replace_daemon_sessions(&mut self, sessions: Vec<SessionSummary>) {
        let previous = self.active_daemon_session_id;
        self.daemon_sessions = sort_workspace_sessions(sessions);
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

    pub fn replace_workspace_sessions(&mut self, sessions: Vec<SessionSummary>) {
        let existing_worktrees = self.workspace_worktrees.clone();
        self.workspace_sessions = sort_workspace_sessions(sessions);
        self.workspace_worktrees =
            worktrees_for_sessions_with_cache(&self.workspace_sessions, &existing_worktrees);
        self.managed_session_ids = self
            .workspace_sessions
            .iter()
            .map(|session| session.session_id)
            .collect();

        let daemon_sessions = self
            .workspace_sessions
            .iter()
            .filter(|session| session.role == SessionRole::MillraceDaemon)
            .cloned()
            .collect::<Vec<_>>();
        self.replace_daemon_sessions(daemon_sessions);

        if !self.session_exists(self.agent_session_id) {
            self.agent_session_id = self
                .workspace_sessions
                .iter()
                .find(|session| {
                    session.role != SessionRole::MillraceDaemon && session.capabilities.attach
                })
                .map(|session| session.session_id);
            for pane in &mut self.panes {
                if pane.kind == PaneKind::AgentTerminal {
                    pane.session_id = self.agent_session_id;
                }
            }
        }

        if !self.session_exists(self.selected_session_id) {
            self.selected_session_id = self
                .focused_session_id()
                .or(self.agent_session_id)
                .or(self.active_daemon_session_id);
        }
        self.update_selected_session_from_focus();
    }

    pub fn open_daemon_switcher(&mut self) {
        self.daemon_switcher.open_with(
            self.selected_session_id
                .or(self.agent_session_id)
                .or(self.active_daemon_session_id),
        );
        self.status_message = "session switcher".to_string();
    }

    pub fn close_daemon_switcher(&mut self) {
        self.daemon_switcher.close();
        self.status_message =
            workspace_status_message(&self.workspace_sessions, self.selected_session_id);
    }

    pub fn move_daemon_switcher_selection(&mut self, delta: isize) -> bool {
        if self.workspace_sessions.is_empty() {
            return false;
        }
        let current = self
            .daemon_switcher
            .selected_session_id
            .or(self.selected_session_id)
            .or(self.agent_session_id)
            .or(self.active_daemon_session_id)
            .and_then(|session_id| {
                self.workspace_sessions
                    .iter()
                    .position(|session| session.session_id == session_id)
            })
            .unwrap_or(0);
        let len = self.workspace_sessions.len() as isize;
        let next = (current as isize + delta).rem_euclid(len) as usize;
        self.daemon_switcher.selected_session_id = Some(self.workspace_sessions[next].session_id);
        true
    }

    pub fn activate_session_switcher_selection(&mut self) -> Option<SessionId> {
        let session_id = self.daemon_switcher.selected_session_id?;
        self.daemon_switcher.close();
        Some(session_id)
    }

    pub fn activate_daemon_switcher_selection(&mut self) -> bool {
        self.activate_session_switcher_selection()
            .map(|session_id| {
                matches!(
                    self.select_workspace_session(session_id),
                    WorkspaceSessionSelection::DaemonSelected(_)
                        | WorkspaceSessionSelection::AttachSelected(_)
                )
            })
            .unwrap_or(false)
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

    pub fn workspace_session_rows(&self) -> Vec<WorkspaceSessionRow> {
        let focused_session_id = self.focused_session_id();
        self.workspace_sessions
            .iter()
            .map(|session| {
                let worktree = self
                    .workspace_worktrees
                    .get(&session.session_id)
                    .and_then(|worktree| worktree.as_ref());
                workspace_session_row(
                    session,
                    worktree,
                    self.selected_session_id == Some(session.session_id),
                    focused_session_id == Some(session.session_id),
                )
            })
            .collect()
    }

    pub fn restore_ui_context_selection(&mut self, context: &UiContext) {
        if let Some(session_id) = context.active_daemon_session_id {
            let _ = self.select_daemon(session_id);
        }

        if let Some(session_id) = context.agent_session_id {
            if matches!(
                self.workspace_session_selection(session_id),
                WorkspaceSessionSelection::AttachSelected(_)
            ) {
                self.agent_session_id = Some(session_id);
                for pane in &mut self.panes {
                    if pane.kind == PaneKind::AgentTerminal {
                        pane.session_id = Some(session_id);
                    }
                }
            }
        }

        let selected = context
            .selected_session_id
            .or(context.focused_session_id)
            .or(context.agent_session_id)
            .or(context.active_daemon_session_id);
        if let Some(session_id) =
            selected.filter(|session_id| self.session_exists(Some(*session_id)))
        {
            self.selected_session_id = Some(session_id);
        }

        match context.focused_pane_kind.as_deref() {
            Some("daemon_monitor") => {
                if let Some(session_id) = context
                    .focused_session_id
                    .or(context.active_daemon_session_id)
                    .filter(|session_id| self.daemon_session_exists(*session_id))
                {
                    let _ = self.select_daemon(session_id);
                }
                let _ = self.focus_pane_kind(PaneKind::DaemonMonitor);
            }
            Some("agent_terminal") => {
                if let Some(session_id) = context
                    .focused_session_id
                    .or(context.agent_session_id)
                    .filter(|session_id| {
                        matches!(
                            self.workspace_session_selection(*session_id),
                            WorkspaceSessionSelection::AttachSelected(_)
                        )
                    })
                {
                    self.agent_session_id = Some(session_id);
                    for pane in &mut self.panes {
                        if pane.kind == PaneKind::AgentTerminal {
                            pane.session_id = Some(session_id);
                        }
                    }
                    let _ = self.focus_pane_kind(PaneKind::AgentTerminal);
                    self.selected_session_id = Some(session_id);
                }
            }
            _ => {}
        }
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
            selected_session_id: self.selected_session_id,
            focused_session_id: self.focused_session_id(),
            focused_pane_kind: self.focused_pane_kind_label().map(str::to_string),
            active_daemon_session_id: self.active_daemon_session_id,
            active_workspace: self.active_workspace.clone(),
            agent_session_id: self.agent_session_id,
            managed_session_ids: self.managed_session_ids.clone(),
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
        if self.search_mode {
            "search"
        } else if self.scroll_mode {
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
                self.search_mode = false;
                self.search_query.clear();
            }
            KeyAction::BeginSearch => self.begin_search_mode(),
            KeyAction::SearchInput(value) => self.push_search_input(*value),
            KeyAction::SearchBackspace => self.pop_search_input(),
            KeyAction::NextSearch => self.next_search_match(),
            KeyAction::PreviousSearch => self.previous_search_match(),
            KeyAction::CopySearchMatch => self.copy_current_search_match(),
            KeyAction::Escape => {
                if self.search_mode {
                    self.search_mode = false;
                    self.status_message = "search closed".to_string();
                } else {
                    self.exit_scroll_mode();
                }
            }
            KeyAction::Detach
            | KeyAction::CloseRequested
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

    fn session_exists(&self, session_id: Option<SessionId>) -> bool {
        session_id.is_some_and(|session_id| {
            self.workspace_sessions
                .iter()
                .any(|session| session.session_id == session_id)
        })
    }

    fn daemon_session_exists(&self, session_id: SessionId) -> bool {
        self.daemon_sessions
            .iter()
            .any(|session| session.session_id == session_id)
    }

    fn update_selected_session_from_focus(&mut self) {
        if let Some(session_id) = self.focused_session_id() {
            self.selected_session_id = Some(session_id);
        }
    }
}

fn cockpit_content_size(width: u16, body_height: u16, session_count: usize) -> (u16, u16) {
    let session_rows = session_count.clamp(1, 3) as u16;
    let desired = 1 + session_rows.saturating_mul(2);
    let max_height = COCKPIT_SESSION_LIST_HEIGHT.min(body_height.saturating_sub(2));
    let list_height = desired.min(max_height).max(1);
    (width, body_height.saturating_sub(list_height))
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

fn sort_workspace_sessions(mut sessions: Vec<SessionSummary>) -> Vec<SessionSummary> {
    sessions.sort_by_key(|session| session_role_rank(&session.role));
    sessions
}

fn upsert_session(sessions: &mut Vec<SessionSummary>, session: SessionSummary) {
    if let Some(index) = sessions
        .iter()
        .position(|candidate| candidate.session_id == session.session_id)
    {
        sessions[index] = session;
    } else {
        sessions.push(session);
    }
}

fn session_role_rank(role: &SessionRole) -> u8 {
    match role {
        SessionRole::MillraceDaemon => 0,
        SessionRole::Agent => 1,
        SessionRole::Shell => 2,
        SessionRole::Worker => 3,
        SessionRole::Generic => 4,
        SessionRole::Other(_) => 5,
    }
}

fn session_can_be_attached(session: &SessionSummary) -> bool {
    session.capabilities.attach
        && matches!(
            session.process_state,
            ProcessState::Starting | ProcessState::Running
        )
}

fn worktrees_for_sessions(
    sessions: &[SessionSummary],
) -> BTreeMap<SessionId, Option<GitWorktreeIdentity>> {
    worktrees_for_sessions_with_cache(sessions, &BTreeMap::new())
}

fn worktrees_for_sessions_with_cache(
    sessions: &[SessionSummary],
    existing: &BTreeMap<SessionId, Option<GitWorktreeIdentity>>,
) -> BTreeMap<SessionId, Option<GitWorktreeIdentity>> {
    sessions
        .iter()
        .map(|session| {
            (
                session.session_id,
                existing
                    .get(&session.session_id)
                    .cloned()
                    .unwrap_or_else(|| discover_session_worktree(session)),
            )
        })
        .collect()
}

fn discover_session_worktree(session: &SessionSummary) -> Option<GitWorktreeIdentity> {
    let workspace_path = session
        .workspace
        .as_ref()
        .map(|workspace| workspace.canonical_path.as_path())
        .unwrap_or(session.cwd.as_path());
    GitWorktreeIdentity::discover(workspace_path)
        .or_else(|| GitWorktreeIdentity::discover(session.cwd.as_path()))
}

fn workspace_session_row(
    session: &SessionSummary,
    worktree: Option<&GitWorktreeIdentity>,
    selected: bool,
    focused: bool,
) -> WorkspaceSessionRow {
    let location = session
        .workspace
        .as_ref()
        .map(|workspace| workspace.canonical_path.display().to_string())
        .unwrap_or_else(|| session.cwd.display().to_string());
    let (worktree, branch, inferred_source) = match worktree {
        Some(worktree) => (
            worktree.root.display().to_string(),
            worktree.branch.as_deref().unwrap_or("detached").to_string(),
            "inferred",
        ),
        None => (
            "unavailable".to_string(),
            "unavailable".to_string(),
            "unavailable",
        ),
    };
    let runtime_source = runtime_status_source(session);
    WorkspaceSessionRow {
        session_id: session.session_id,
        role: session_role_label(&session.role).to_string(),
        name: session
            .name
            .clone()
            .unwrap_or_else(|| session.session_id.to_string()),
        location,
        worktree,
        branch,
        process_state: process_state_label(&session.process_state).to_string(),
        liveness: liveness_label(session),
        unread: "unread=unavailable".to_string(),
        attention: attention_label_for_row(&session.attention_state).to_string(),
        selected,
        focused,
        status_summary: format!(
            "status millmux_session:{} liveness={}",
            process_state_label(&session.process_state),
            liveness_label(session)
        ),
        source_summary: format!(
            "source millmux_session runtime={runtime_source} terminal_screen=preview operator=unavailable inferred={inferred_source}"
        ),
    }
}

fn session_role_label(role: &SessionRole) -> &str {
    match role {
        SessionRole::Shell => "shell",
        SessionRole::MillraceDaemon => "millrace_daemon",
        SessionRole::Agent => "agent",
        SessionRole::Generic => "generic",
        SessionRole::Worker => "worker",
        SessionRole::Other(value) => value.as_str(),
    }
}

fn liveness_label(session: &SessionSummary) -> String {
    format!(
        "worker:{} child:{}",
        liveness_state_label(session.liveness.worker),
        liveness_state_label(session.liveness.child)
    )
}

fn liveness_state_label(state: LivenessState) -> &'static str {
    match state {
        LivenessState::Unknown => "unknown",
        LivenessState::Alive => "alive",
        LivenessState::Dead => "dead",
        LivenessState::Indeterminate => "indeterminate",
    }
}

fn attention_label_for_row(state: &AttentionState) -> &'static str {
    match state {
        AttentionState::Unknown => "unknown",
        AttentionState::Active => "active",
        AttentionState::Idle => "idle",
        AttentionState::NeedsAttention => "needs_attention",
        AttentionState::MillraceIdle => "millrace_idle",
        AttentionState::MillraceBusy => "millrace_busy",
    }
}

fn runtime_status_source(session: &SessionSummary) -> &'static str {
    if session.role == SessionRole::MillraceDaemon {
        match session.attention_state {
            AttentionState::MillraceIdle => "millrace_runtime:idle",
            AttentionState::MillraceBusy => "millrace_runtime:busy",
            _ => "unavailable",
        }
    } else {
        "unavailable"
    }
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

fn workspace_status_message(sessions: &[SessionSummary], selected: Option<SessionId>) -> String {
    if sessions.is_empty() {
        return "no workspace sessions".to_string();
    }
    let selected = selected
        .and_then(|session_id| find_session(sessions, session_id))
        .map(|session| {
            format!(
                "{} {}",
                session_role_label(&session.role),
                session.name.as_deref().unwrap_or("unnamed")
            )
        })
        .unwrap_or_else(|| "no session selected".to_string());
    format!("session switcher closed selected={selected}")
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
        ProcessState::Crashed
        | ProcessState::Failed
        | ProcessState::Lost
        | ProcessState::Stale
        | ProcessState::Orphaned => vec![
            UiDaemonRecoveryAction::Inspect,
            UiDaemonRecoveryAction::Logs,
            UiDaemonRecoveryAction::Doctor,
            UiDaemonRecoveryAction::Archive,
            UiDaemonRecoveryAction::Delete,
        ],
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
        ProcessState::Orphaned => "orphaned",
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
        assert_eq!(context.selected_session_id, Some(agent_id));
        assert_eq!(context.focused_session_id, Some(agent_id));
        assert_eq!(context.focused_pane_kind.as_deref(), Some("agent_terminal"));
        assert!(context.managed_session_ids.contains(&agent_id));
        assert!(context.managed_session_ids.contains(&daemon_id));
    }

    #[test]
    fn agent_cockpit_workspace_rows_preserve_source_attribution() {
        let mut daemon = summary("daemon");
        daemon.attention_state = AttentionState::MillraceBusy;
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

        let rows = app.workspace_session_rows();
        assert_eq!(rows.len(), 2);
        let daemon = rows.iter().find(|row| row.session_id == daemon_id).unwrap();
        assert_eq!(daemon.role, "millrace_daemon");
        assert!(daemon.status_summary.contains("millmux_session:running"));
        assert!(daemon.source_summary.contains("millmux_session"));
        assert!(daemon.source_summary.contains("millrace_runtime:busy"));
        assert!(daemon.source_summary.contains("terminal_screen=preview"));
        assert!(daemon.source_summary.contains("operator=unavailable"));
        assert!(daemon.source_summary.contains("inferred=unavailable"));
        let agent = rows.iter().find(|row| row.session_id == agent_id).unwrap();
        assert!(agent.selected);
        assert!(agent.focused);
    }

    #[test]
    fn agent_cockpit_can_select_shell_session_as_attach_target() {
        let daemon = summary("daemon");
        let daemon_id = daemon.session_id;
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let mut shell = summary("shell");
        shell.role = SessionRole::Shell;
        shell.argv = vec!["bash".to_string()];
        let shell_id = shell.session_id;
        let mut app = AppModel::agent_cockpit(
            UiId::new(),
            agent.clone(),
            vec![daemon.clone()],
            Some(daemon_id),
            BTreeMap::new(),
            AgentTerminalPane::new(10, 40, true, false),
            AgentCockpitLayout::Right,
            MonitorProfile::Basic,
        );
        app.replace_workspace_sessions(vec![daemon, agent, shell]);

        let selection = app.select_workspace_session(shell_id);

        assert_eq!(
            selection,
            WorkspaceSessionSelection::AttachSelected(shell_id)
        );
        assert_eq!(app.agent_session_id, Some(shell_id));
        assert!(app.focused_agent_terminal());
        let context = app.ui_context();
        assert_eq!(context.selected_session_id, Some(shell_id));
        assert_eq!(context.focused_session_id, Some(shell_id));
        assert_eq!(context.focused_pane_kind.as_deref(), Some("agent_terminal"));
        assert_eq!(context.active_daemon_session_id, Some(daemon_id));
        assert_eq!(context.managed_daemon_session_ids, vec![daemon_id]);
        assert!(context.managed_session_ids.contains(&shell_id));
    }

    #[test]
    fn agent_cockpit_restores_prior_context_selection_and_focus() {
        let daemon = summary("daemon");
        let daemon_id = daemon.session_id;
        let mut first_agent = summary("agent-one");
        first_agent.role = SessionRole::Agent;
        let mut second_agent = summary("agent-two");
        second_agent.role = SessionRole::Agent;
        let second_agent_id = second_agent.session_id;
        let mut app = AppModel::agent_cockpit(
            UiId::new(),
            first_agent.clone(),
            vec![daemon.clone()],
            Some(daemon_id),
            BTreeMap::new(),
            AgentTerminalPane::new(10, 40, true, false),
            AgentCockpitLayout::Right,
            MonitorProfile::Basic,
        );
        app.replace_workspace_sessions(vec![daemon, first_agent, second_agent]);
        let context = UiContext {
            schema_version: 1,
            ui_id: app.ui_id,
            mode: UiMode::AgentCockpit,
            active_pane_id: app.active_pane_id,
            selected_session_id: Some(second_agent_id),
            focused_session_id: Some(second_agent_id),
            focused_pane_kind: Some("agent_terminal".to_string()),
            active_daemon_session_id: Some(daemon_id),
            active_workspace: app.active_workspace.clone(),
            agent_session_id: Some(second_agent_id),
            managed_session_ids: vec![daemon_id, second_agent_id],
            managed_daemon_session_ids: vec![daemon_id],
            monitor_profile: MonitorProfile::Basic,
            daemon_health: Vec::new(),
            updated_at: OffsetDateTime::now_utc(),
        };

        app.restore_ui_context_selection(&context);

        assert_eq!(app.agent_session_id, Some(second_agent_id));
        assert_eq!(app.selected_session_id, Some(second_agent_id));
        assert_eq!(app.focused_session_id(), Some(second_agent_id));
        assert!(app.focused_agent_terminal());
        assert_eq!(app.active_daemon_session_id, Some(daemon_id));
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

        assert_eq!(app.agent_terminal_size_for(120, 30), Some((23, 78)));
        assert!(app.resize_agent_terminal(23, 78));
        assert!(!app.resize_agent_terminal(23, 78));
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
    fn agent_cockpit_search_mode_captures_input_and_copies_exact_match() {
        let daemon = summary("daemon");
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let mut terminal = crate::terminal::TerminalEmulator::new(4, 40, 20);
        terminal.process_text("alpha\r\n>Hey can you see\r\nomega\r\n");
        let mut app = AppModel::agent_cockpit(
            UiId::new(),
            agent,
            vec![daemon],
            None,
            BTreeMap::new(),
            AgentTerminalPane::with_snapshot(terminal.snapshot(), true, false),
            AgentCockpitLayout::Right,
            MonitorProfile::Basic,
        );

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL), 4),
            KeyAction::Prefix
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE), 4),
            KeyAction::EnterScrollMode
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE), 4),
            KeyAction::BeginSearch
        );
        for value in "can you".chars() {
            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Char(value), KeyModifiers::NONE), 4),
                KeyAction::SearchInput(value)
            );
        }

        assert_eq!(app.active_view_label(), "search");
        assert!(
            app.status_message.contains("line"),
            "{}",
            app.status_message
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 4),
            KeyAction::CopySearchMatch
        );
        assert_eq!(
            app.copy_buffer_text().map(str::trim_end),
            Some(">Hey can you see")
        );
        assert!(!app.search_mode);
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
            liveness: Default::default(),
        }
    }
}
