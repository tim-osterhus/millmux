use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use millrace_sessions_core::{
    ids::{PaneId, SessionId, UiId},
    protocol::SessionSummary,
    state::{
        AttentionState, LivenessState, MonitorProfile, ProcessState, SessionRole, SpawnMode,
        StatusSummary, StatusSummarySource, UiContext, UiDaemonHealth, UiDaemonRecoveryAction,
        UiMode, UiPaneContext, UiPaneView, UiPaneViewKind, UiPaneViewMode,
    },
    workspace::{GitWorktreeIdentity, WorkspaceIdentity},
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use time::OffsetDateTime;

use crate::{
    keymap::{KeyAction, PrefixKeymap},
    pane::{
        AgentCockpitLayout, AgentTerminalPane, CommandOutput, CommandPalette, ConfirmationPrompt,
        DaemonConsoleLayout, DaemonSwitcherOverlay, HelpOverlay, LineLogPane, Pane, PaneKind,
        SearchMatch, WorkspaceSessionRow, COCKPIT_SESSION_LIST_HEIGHT,
    },
    terminal::{TerminalSearchMatch, TerminalSnapshot},
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
            panes: vec![
                agent_pane,
                daemon_pane,
                Pane::session_list(),
                Pane::command_output(),
            ],
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
        match self.active_view_kind() {
            Some(UiPaneViewKind::SessionTerminal) if self.active_terminal_is_attached() => {
                self.set_agent_terminal_following(true)
            }
            Some(UiPaneViewKind::SessionTerminal) => {}
            Some(UiPaneViewKind::DaemonMonitor) => self.jump_active_log_bottom(),
            Some(UiPaneViewKind::SessionList | UiPaneViewKind::CommandOutput) | None => {}
        }
        self.status_message = "live".to_string();
    }

    pub fn switch_focus(&mut self) {
        let focusable = self.focusable_pane_indices();
        if focusable.is_empty() {
            self.active_pane_id = None;
            return;
        }

        let current = self
            .active_pane_id
            .and_then(|id| {
                focusable
                    .iter()
                    .position(|index| self.panes[*index].id == id)
            })
            .unwrap_or(0);
        let next = focusable[(current + 1) % focusable.len()];
        let _ = self.focus_pane_index(next);
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
            PaneKind::SessionList => "session_list",
            PaneKind::CommandOutput => "command_output",
            PaneKind::StatusBar => "status_bar",
            PaneKind::HelpOverlay => "help_overlay",
            PaneKind::CommandPalette => "command_palette",
        })
    }

    pub fn active_pane(&self) -> Option<&Pane> {
        let active = self.active_pane_id?;
        self.panes.iter().find(|pane| pane.id == active)
    }

    fn active_view_kind(&self) -> Option<UiPaneViewKind> {
        self.active_pane()
            .filter(|pane| !pane.stale)
            .map(|pane| pane.view.kind)
    }

    fn target_pane_id_for_kind(&self, kind: PaneKind) -> Option<PaneId> {
        self.active_pane()
            .filter(|pane| pane.kind == kind && !pane.stale)
            .map(|pane| pane.id)
            .or_else(|| {
                self.panes
                    .iter()
                    .find(|pane| pane.kind == kind && !pane.stale)
                    .map(|pane| pane.id)
            })
    }

    fn pane_id_for_view_session(
        &self,
        kind: UiPaneViewKind,
        session_id: SessionId,
    ) -> Option<PaneId> {
        self.panes
            .iter()
            .find(|pane| {
                pane.view.kind == kind && pane.view.session_id == Some(session_id) && !pane.stale
            })
            .map(|pane| pane.id)
    }

    pub fn focused_session_id(&self) -> Option<SessionId> {
        let pane = self.active_pane()?;
        (!pane.stale).then_some(pane.view.session_id).flatten()
    }

    pub fn active_attach_session_id(&self) -> Option<SessionId> {
        self.agent_session_id
    }

    pub fn focused_attach_session_id(&self) -> Option<SessionId> {
        let pane = self.active_pane()?;
        (!pane.stale && pane.view.kind == UiPaneViewKind::SessionTerminal)
            .then_some(pane.view.session_id)
            .flatten()
    }

    pub fn focused_attach_matches(&self, attached_session_id: SessionId) -> bool {
        self.focused_attach_session_id() == Some(attached_session_id)
    }

    pub fn managed_raw_attach_target(
        &self,
        attached_session_id: SessionId,
    ) -> Result<SessionId, &'static str> {
        if self.command_palette.open
            || self.daemon_switcher.open
            || self.confirmation.is_some()
            || self.help_overlay.open
        {
            return Err("overlay_active");
        }
        if self.scroll_mode || self.search_mode {
            return Err("terminal_not_live");
        }
        let pane = self.active_pane().ok_or("no_focused_pane")?;
        if pane.stale {
            return Err("stale_pane");
        }
        if pane.view.kind != UiPaneViewKind::SessionTerminal {
            return Err("focused_pane_not_terminal");
        }
        let session_id = pane.view.session_id.ok_or("terminal_unassigned")?;
        if session_id != attached_session_id || self.agent_session_id != Some(session_id) {
            return Err("pane_session_mismatch");
        }
        let session = self
            .workspace_sessions
            .iter()
            .find(|session| session.session_id == session_id)
            .ok_or("session_missing")?;
        if session.spawn_mode != SpawnMode::Pty {
            return Err("session_not_pty");
        }
        if session.process_state != ProcessState::Running {
            return Err("session_not_running");
        }
        if !session.capabilities.attach {
            return Err("session_not_attachable");
        }
        let terminal = self
            .agent_terminal
            .as_ref()
            .ok_or("terminal_not_attached")?;
        if terminal.initializing {
            return Err("terminal_initializing");
        }
        if terminal.read_only || !terminal.input_owner {
            return Err("input_not_owned");
        }
        Ok(session_id)
    }

    pub fn focus_pane_kind(&mut self, kind: PaneKind) -> bool {
        let Some(index) = self
            .panes
            .iter()
            .position(|pane| pane.kind == kind && !pane.stale)
        else {
            return false;
        };
        self.focus_pane_index(index)
    }

    pub fn focus_pane_id(&mut self, pane_id: PaneId) -> bool {
        let Some(index) = self
            .panes
            .iter()
            .position(|pane| pane.id == pane_id && !pane.stale)
        else {
            return false;
        };
        self.focus_pane_index(index)
    }

    fn focus_pane_index(&mut self, index: usize) -> bool {
        for (pane_index, pane) in self.panes.iter_mut().enumerate() {
            pane.focused = pane_index == index;
        }
        self.active_pane_id = Some(self.panes[index].id);
        if self.panes[index].view.kind == UiPaneViewKind::DaemonMonitor {
            if let Some(session_id) = self.panes[index].view.session_id {
                let _ = self.select_daemon(session_id);
            }
        }
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

    pub fn set_agent_search_match(&mut self, label: &str, found: &TerminalSearchMatch) {
        if let Some(terminal) = &mut self.agent_terminal {
            terminal.set_search_match(SearchMatch {
                index: found.physical_row,
                occurrence: found.occurrence,
                start_cell: found.start_cell,
                end_cell: found.end_cell,
                query: found.query.clone(),
                line: found.line.clone(),
                matched_text: found.matched_text.clone(),
            });
        }
        self.status_message = format!(
            "{label}: {} row {} match {}",
            found.query,
            found.physical_row + 1,
            found.occurrence + 1
        );
    }

    pub fn set_agent_search_not_found(&mut self, label: &str) {
        if let Some(terminal) = &mut self.agent_terminal {
            terminal.clear_search();
        }
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
            self.set_agent_search_not_found("search");
            return;
        }

        let query = self.search_query.clone();
        let found = match self.active_view_kind() {
            Some(UiPaneViewKind::SessionTerminal) if self.active_terminal_is_attached() => {
                if let Some(terminal) = &mut self.agent_terminal {
                    terminal.clear_search();
                }
                self.status_message = format!("search: {query} indexing history");
                return;
            }
            Some(UiPaneViewKind::SessionTerminal) => {
                self.status_message = "search: terminal not attached".to_string();
                return;
            }
            Some(UiPaneViewKind::DaemonMonitor) => {
                let result = self.line_log.search(query.clone());
                if let Some(log) = self.active_daemon_log_mut() {
                    let _ = log.search(query.clone());
                }
                result
            }
            Some(UiPaneViewKind::SessionList | UiPaneViewKind::CommandOutput) | None => {
                self.status_message = "search: view not searchable".to_string();
                return;
            }
        };
        self.status_message = match found {
            Some(found) => format!("search: {} line {}", query, found.index + 1),
            None => format!("search: {} not found", query),
        };
    }

    fn next_search_match(&mut self) {
        let found = match self.active_view_kind() {
            Some(UiPaneViewKind::SessionTerminal) if self.active_terminal_is_attached() => None,
            Some(UiPaneViewKind::SessionTerminal) => None,
            Some(UiPaneViewKind::DaemonMonitor) => {
                let result = self.line_log.next_match();
                if let Some(log) = self.active_daemon_log_mut() {
                    let _ = log.next_match();
                }
                result
            }
            Some(UiPaneViewKind::SessionList | UiPaneViewKind::CommandOutput) | None => None,
        };
        self.status_message = match found {
            Some(found) => format!("search next: line {}", found.index + 1),
            None => "search next: no match".to_string(),
        };
    }

    fn previous_search_match(&mut self) {
        let found = match self.active_view_kind() {
            Some(UiPaneViewKind::SessionTerminal) if self.active_terminal_is_attached() => None,
            Some(UiPaneViewKind::SessionTerminal) => None,
            Some(UiPaneViewKind::DaemonMonitor) => {
                let result = self.line_log.previous_match();
                if let Some(log) = self.active_daemon_log_mut() {
                    let _ = log.previous_match();
                }
                result
            }
            Some(UiPaneViewKind::SessionList | UiPaneViewKind::CommandOutput) | None => None,
        };
        self.status_message = match found {
            Some(found) => format!("search previous: line {}", found.index + 1),
            None => "search previous: no match".to_string(),
        };
    }

    fn copy_current_search_match(&mut self) {
        let found = match self.active_view_kind() {
            Some(UiPaneViewKind::SessionTerminal) if self.active_terminal_is_attached() => self
                .agent_terminal
                .as_ref()
                .and_then(AgentTerminalPane::current_match),
            Some(UiPaneViewKind::SessionTerminal) => None,
            Some(UiPaneViewKind::DaemonMonitor) => self.line_log.current_match(),
            Some(UiPaneViewKind::SessionList | UiPaneViewKind::CommandOutput) | None => None,
        };
        match found {
            Some(found) => {
                self.copy_buffer = Some(found.matched_text);
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
        let body = Rect::new(0, 0, width, body_height);
        let (_, terminal_area) = self.agent_terminal_rect_for(body)?;
        Some((
            terminal_area.height.saturating_sub(1).max(1),
            terminal_area.width.max(1),
        ))
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
        let has_selected_pane = self.panes.iter().any(|pane| {
            pane.kind == PaneKind::DaemonMonitor
                && pane.session_id == Some(session_id)
                && !pane.stale
        });
        let target_pane_id = (!has_selected_pane)
            .then(|| self.target_pane_id_for_kind(PaneKind::DaemonMonitor))
            .flatten();
        for pane in &mut self.panes {
            if pane.kind == PaneKind::DaemonMonitor && Some(pane.id) == target_pane_id {
                pane.session_id = Some(session_id);
                pane.view.session_id = Some(session_id);
                pane.stale = false;
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
        self.refresh_pane_staleness();
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
                if let Some(pane_id) =
                    self.pane_id_for_view_session(UiPaneViewKind::DaemonMonitor, session_id)
                {
                    let _ = self.focus_pane_id(pane_id);
                } else {
                    let _ = self.focus_pane_kind(PaneKind::DaemonMonitor);
                }
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
        let target_pane_id = self.target_pane_id_for_kind(PaneKind::AgentTerminal);
        for pane in &mut self.panes {
            if Some(pane.id) == target_pane_id {
                pane.session_id = Some(session_id);
                pane.view.session_id = Some(session_id);
                pane.stale = false;
            }
        }
        if let Some(pane_id) = target_pane_id {
            let _ = self.focus_pane_id(pane_id);
        } else {
            let _ = self.focus_pane_kind(PaneKind::AgentTerminal);
        }
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
            let has_selected_pane = self.panes.iter().any(|pane| {
                pane.kind == PaneKind::DaemonMonitor
                    && pane.session_id == Some(session_id)
                    && !pane.stale
            });
            let target_pane_id = (!has_selected_pane)
                .then(|| self.target_pane_id_for_kind(PaneKind::DaemonMonitor))
                .flatten();
            for pane in &mut self.panes {
                if Some(pane.id) == target_pane_id {
                    pane.session_id = Some(session_id);
                    pane.view.session_id = Some(session_id);
                    pane.stale = false;
                }
            }
        } else {
            self.active_workspace = None;
        }

        self.refresh_pane_staleness();
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
            let target_pane_id = self.target_pane_id_for_kind(PaneKind::AgentTerminal);
            for pane in &mut self.panes {
                if Some(pane.id) == target_pane_id {
                    pane.session_id = self.agent_session_id;
                    pane.view.session_id = self.agent_session_id;
                    pane.stale = false;
                }
            }
        }

        if !self.session_exists(self.selected_session_id) {
            self.selected_session_id = self
                .focused_session_id()
                .or(self.agent_session_id)
                .or(self.active_daemon_session_id);
        }
        self.refresh_pane_staleness();
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

    pub fn pane_contexts(&self) -> Vec<UiPaneContext> {
        self.panes
            .iter()
            .map(|pane| {
                let mut context = pane.to_context();
                context.focused = Some(pane.id) == self.active_pane_id;
                context.view.view_mode = self.view_mode_for_pane(pane);
                context.read_only = self.pane_is_read_only_for_external_input(pane);
                context.overlay_active = self.overlay_owns_input();
                context
            })
            .collect()
    }

    pub fn assign_pane_view(&mut self, pane_id: PaneId, view: UiPaneView) -> bool {
        let Some(index) = self.panes.iter().position(|pane| pane.id == pane_id) else {
            return false;
        };
        let stale = self.pane_view_is_stale(&view);
        self.panes[index].set_view(view.clone());
        self.panes[index].stale = stale;
        if stale {
            self.status_message = "pane assignment stale; focus kept safe".to_string();
            if Some(pane_id) == self.active_pane_id {
                self.focus_safe_fallback();
            }
            return true;
        }

        if Some(pane_id) == self.active_pane_id {
            self.update_selected_session_from_focus();
        }
        true
    }

    pub fn split_pane_with_view(
        &mut self,
        target_pane_id: PaneId,
        view: UiPaneView,
    ) -> Option<PaneId> {
        let target_index = self
            .panes
            .iter()
            .position(|pane| pane.id == target_pane_id)?;
        let mut pane = pane_for_view(view);
        pane.stale = self.pane_view_is_stale(&pane.view);
        let pane_id = pane.id;
        self.panes.insert(target_index + 1, pane);
        Some(pane_id)
    }

    pub fn close_pane(&mut self, pane_id: PaneId) -> bool {
        let Some(index) = self.panes.iter().position(|pane| pane.id == pane_id) else {
            return false;
        };
        if self
            .focusable_pane_indices()
            .into_iter()
            .filter(|candidate| self.panes[*candidate].id != pane_id)
            .count()
            == 0
        {
            self.status_message = "cannot close last pane".to_string();
            return false;
        }
        self.panes.remove(index);
        if self.active_pane_id == Some(pane_id) {
            self.focus_safe_fallback();
        }
        true
    }

    pub fn restore_ui_context_selection(&mut self, context: &UiContext) {
        self.restore_pane_contexts(context);

        if let Some(session_id) = context.active_daemon_session_id {
            let _ = self.select_daemon(session_id);
        }

        if let Some(session_id) = context.agent_session_id {
            if matches!(
                self.workspace_session_selection(session_id),
                WorkspaceSessionSelection::AttachSelected(_)
            ) {
                self.agent_session_id = Some(session_id);
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

        if context
            .active_pane_id
            .is_some_and(|pane_id| self.focus_pane_id(pane_id))
        {
            self.apply_restored_view_modes(context);
            if self.panes.iter().any(|pane| pane.stale) {
                self.status_message = "stale pane recovered; focus moved".to_string();
            }
            return;
        }

        if context.panes.is_empty() {
            self.restore_legacy_focus(context);
        } else {
            self.focus_safe_fallback();
        }
        self.apply_restored_view_modes(context);
        if self.panes.iter().any(|pane| pane.stale) {
            self.status_message = "stale pane recovered; focus moved".to_string();
        }
    }

    fn restore_pane_contexts(&mut self, context: &UiContext) {
        if context.panes.is_empty() {
            return;
        }

        let default_panes = std::mem::take(&mut self.panes);
        self.panes = context
            .panes
            .iter()
            .map(|pane_context| {
                let stale = self.pane_view_is_stale(&pane_context.view);
                Pane::from_context(pane_context, stale)
            })
            .collect::<Vec<_>>();

        for index in 0..self.panes.len() {
            let stale = self.pane_view_is_stale(&self.panes[index].view);
            self.panes[index].stale = stale;
            if stale {
                self.panes[index].focused = false;
            }
        }

        if self.focusable_pane_indices().is_empty() {
            if let Some(mut fallback) = default_panes
                .into_iter()
                .find(|pane| !self.pane_view_is_stale(&pane.view))
            {
                fallback.focused = false;
                fallback.stale = false;
                self.panes.push(fallback);
            }
        }

        self.active_pane_id = None;
        if let Some(active_pane_id) = context.active_pane_id {
            if !self.focus_pane_id(active_pane_id) {
                self.status_message = "stale pane recovered; focus moved".to_string();
                self.focus_safe_fallback();
            }
        } else {
            self.focus_safe_fallback();
        }
    }

    fn restore_legacy_focus(&mut self, context: &UiContext) {
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
                    let target_pane_id = self.target_pane_id_for_kind(PaneKind::AgentTerminal);
                    for pane in &mut self.panes {
                        if Some(pane.id) == target_pane_id {
                            pane.session_id = Some(session_id);
                            pane.view.session_id = Some(session_id);
                            pane.stale = false;
                        }
                    }
                    let _ = self.focus_pane_kind(PaneKind::AgentTerminal);
                    self.selected_session_id = Some(session_id);
                }
            }
            _ => self.focus_safe_fallback(),
        }
    }

    fn apply_restored_view_modes(&mut self, context: &UiContext) {
        for pane_context in &context.panes {
            let follow = pane_context.view.view_mode == UiPaneViewMode::Live;
            match pane_context.view.kind {
                UiPaneViewKind::SessionTerminal if Some(pane_context.id) == self.active_pane_id => {
                    self.set_agent_terminal_following(follow);
                }
                UiPaneViewKind::DaemonMonitor => {
                    if Some(pane_context.view.session_id).flatten() == self.active_daemon_session_id
                    {
                        self.line_log.set_following(follow);
                        if let Some(log) = self.active_daemon_log_mut() {
                            log.set_following(follow);
                        }
                    }
                }
                UiPaneViewKind::SessionList
                | UiPaneViewKind::CommandOutput
                | UiPaneViewKind::SessionTerminal => {}
            }
        }
    }

    fn focus_safe_fallback(&mut self) {
        let fallback = self
            .focusable_pane_indices()
            .into_iter()
            .find(|index| self.panes[*index].view.kind == UiPaneViewKind::SessionTerminal)
            .or_else(|| {
                self.focusable_pane_indices()
                    .into_iter()
                    .find(|index| self.panes[*index].view.kind == UiPaneViewKind::DaemonMonitor)
            })
            .or_else(|| self.focusable_pane_indices().into_iter().next());
        if let Some(index) = fallback {
            let _ = self.focus_pane_index(index);
        } else {
            self.active_pane_id = None;
        }
    }

    fn focusable_pane_indices(&self) -> Vec<usize> {
        self.panes
            .iter()
            .enumerate()
            .filter_map(|(index, pane)| {
                (!pane.stale
                    && (pane.kind != PaneKind::CommandOutput || self.command_output.is_visible()))
                .then_some(index)
            })
            .collect()
    }

    fn pane_view_is_stale(&self, view: &UiPaneView) -> bool {
        match view.kind {
            UiPaneViewKind::SessionTerminal => match view.session_id {
                Some(session_id) => !matches!(
                    self.workspace_session_selection(session_id),
                    WorkspaceSessionSelection::AttachSelected(_)
                ),
                None => true,
            },
            UiPaneViewKind::DaemonMonitor => match view.session_id {
                Some(session_id) => !self.daemon_session_exists(session_id),
                None => true,
            },
            UiPaneViewKind::SessionList | UiPaneViewKind::CommandOutput => false,
        }
    }

    fn view_mode_for_pane(&self, pane: &Pane) -> UiPaneViewMode {
        let following = match pane.view.kind {
            UiPaneViewKind::SessionTerminal => self.agent_terminal_is_following(),
            UiPaneViewKind::DaemonMonitor => pane
                .view
                .session_id
                .and_then(|session_id| self.daemon_logs.get(&session_id))
                .map_or_else(|| self.line_log.is_following(), LineLogPane::is_following),
            UiPaneViewKind::SessionList | UiPaneViewKind::CommandOutput => true,
        };
        if following {
            UiPaneViewMode::Live
        } else {
            UiPaneViewMode::Scrollback
        }
    }

    fn pane_is_read_only_for_external_input(&self, pane: &Pane) -> bool {
        if pane.view.kind != UiPaneViewKind::SessionTerminal {
            return false;
        }
        self.agent_terminal
            .as_ref()
            .map_or(true, |terminal| terminal.read_only || !terminal.input_owner)
    }

    fn overlay_owns_input(&self) -> bool {
        self.command_palette.open || self.help_overlay.open || self.confirmation.is_some()
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
            panes: self.pane_contexts(),
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
            | KeyAction::ManagedRawAttach
            | KeyAction::CloseRequested
            | KeyAction::Prefix
            | KeyAction::Input(_)
            | KeyAction::Ignored => {}
        }
    }

    fn scroll_active_view_up(&mut self, viewport_height: u16, lines: usize) {
        match self.active_view_kind() {
            Some(UiPaneViewKind::SessionTerminal) if self.active_terminal_is_attached() => {
                self.set_agent_terminal_following(false)
            }
            Some(UiPaneViewKind::SessionTerminal) => {
                self.status_message = "terminal not attached".to_string();
            }
            Some(UiPaneViewKind::DaemonMonitor) => {
                self.scroll_active_log_up(viewport_height, lines)
            }
            Some(UiPaneViewKind::SessionList | UiPaneViewKind::CommandOutput) | None => {
                self.status_message = "scroll unavailable for view".to_string();
            }
        }
    }

    fn scroll_active_view_down(&mut self, lines: usize) {
        match self.active_view_kind() {
            Some(UiPaneViewKind::SessionTerminal) if self.active_terminal_is_attached() => {
                self.set_agent_terminal_following(false)
            }
            Some(UiPaneViewKind::SessionTerminal) => {
                self.status_message = "terminal not attached".to_string();
            }
            Some(UiPaneViewKind::DaemonMonitor) => self.scroll_active_log_down(lines),
            Some(UiPaneViewKind::SessionList | UiPaneViewKind::CommandOutput) | None => {
                self.status_message = "scroll unavailable for view".to_string();
            }
        }
    }

    fn page_active_view_up(&mut self, viewport_height: u16) {
        match self.active_view_kind() {
            Some(UiPaneViewKind::SessionTerminal) if self.active_terminal_is_attached() => {
                self.set_agent_terminal_following(false)
            }
            Some(UiPaneViewKind::SessionTerminal) => {
                self.status_message = "terminal not attached".to_string();
            }
            Some(UiPaneViewKind::DaemonMonitor) => self.page_active_log_up(viewport_height),
            Some(UiPaneViewKind::SessionList | UiPaneViewKind::CommandOutput) | None => {
                self.status_message = "scroll unavailable for view".to_string();
            }
        }
    }

    fn page_active_view_down(&mut self, viewport_height: u16) {
        match self.active_view_kind() {
            Some(UiPaneViewKind::SessionTerminal) if self.active_terminal_is_attached() => {
                self.set_agent_terminal_following(false)
            }
            Some(UiPaneViewKind::SessionTerminal) => {
                self.status_message = "terminal not attached".to_string();
            }
            Some(UiPaneViewKind::DaemonMonitor) => self.page_active_log_down(viewport_height),
            Some(UiPaneViewKind::SessionList | UiPaneViewKind::CommandOutput) | None => {
                self.status_message = "scroll unavailable for view".to_string();
            }
        }
    }

    fn jump_active_view_top(&mut self, viewport_height: u16) {
        match self.active_view_kind() {
            Some(UiPaneViewKind::SessionTerminal) if self.active_terminal_is_attached() => {
                self.set_agent_terminal_following(false)
            }
            Some(UiPaneViewKind::SessionTerminal) => {
                self.status_message = "terminal not attached".to_string();
            }
            Some(UiPaneViewKind::DaemonMonitor) => self.jump_active_log_top(viewport_height),
            Some(UiPaneViewKind::SessionList | UiPaneViewKind::CommandOutput) | None => {
                self.status_message = "scroll unavailable for view".to_string();
            }
        }
    }

    fn jump_active_view_bottom(&mut self) {
        match self.active_view_kind() {
            Some(UiPaneViewKind::SessionTerminal) if self.active_terminal_is_attached() => {
                self.set_agent_terminal_following(true)
            }
            Some(UiPaneViewKind::SessionTerminal) => {
                self.status_message = "terminal not attached".to_string();
            }
            Some(UiPaneViewKind::DaemonMonitor) => self.jump_active_log_bottom(),
            Some(UiPaneViewKind::SessionList | UiPaneViewKind::CommandOutput) | None => {
                self.status_message = "scroll unavailable for view".to_string();
            }
        }
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

    fn active_terminal_is_attached(&self) -> bool {
        self.active_pane().is_some_and(|pane| {
            !pane.stale
                && pane.view.kind == UiPaneViewKind::SessionTerminal
                && pane.view.session_id == self.agent_session_id
        })
    }

    fn refresh_pane_staleness(&mut self) {
        let mut active_became_stale = false;
        for index in 0..self.panes.len() {
            let view = self.panes[index].view.clone();
            let stale = self.pane_view_is_stale(&view);
            self.panes[index].stale = stale;
            if stale {
                self.panes[index].focused = false;
                active_became_stale |= self.active_pane_id == Some(self.panes[index].id);
            }
        }
        if active_became_stale {
            self.status_message = "stale pane recovered; focus moved".to_string();
            self.focus_safe_fallback();
        }
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

    fn agent_terminal_rect_for(&self, area: Rect) -> Option<(PaneId, Rect)> {
        let (_, content_area) = if self.cockpit_session_list_visible() {
            cockpit_session_list_split(area, self.workspace_sessions.len())
        } else {
            (Rect::default(), area)
        };
        let panes = self.cockpit_visible_content_panes();
        if panes.is_empty() {
            return None;
        }

        let pane_id = self.agent_terminal_pane_id_for_geometry()?;
        let pane_index = panes.iter().position(|pane| pane.id == pane_id)?;
        let pane_area = cockpit_content_pane_rect(
            content_area,
            self.cockpit_layout,
            panes.len(),
            pane_index,
            self.active_pane_id,
            panes.iter().map(|pane| pane.id),
        )?;
        Some((pane_id, pane_area))
    }

    fn cockpit_session_list_visible(&self) -> bool {
        self.panes
            .iter()
            .any(|pane| !pane.stale && pane.kind == PaneKind::SessionList)
    }

    fn cockpit_visible_content_panes(&self) -> Vec<&Pane> {
        self.panes
            .iter()
            .filter(|pane| {
                !pane.stale
                    && pane.kind != PaneKind::SessionList
                    && (pane.kind != PaneKind::CommandOutput || self.command_output.is_visible())
            })
            .collect()
    }

    fn agent_terminal_pane_id_for_geometry(&self) -> Option<PaneId> {
        let agent_session_id = self.agent_session_id;
        self.active_pane()
            .filter(|pane| {
                !pane.stale
                    && pane.kind == PaneKind::AgentTerminal
                    && pane.session_id == agent_session_id
            })
            .map(|pane| pane.id)
            .or_else(|| {
                self.panes
                    .iter()
                    .find(|pane| {
                        !pane.stale
                            && pane.kind == PaneKind::AgentTerminal
                            && pane.session_id == agent_session_id
                    })
                    .map(|pane| pane.id)
            })
    }
}

fn cockpit_session_list_split(area: Rect, session_count: usize) -> (Rect, Rect) {
    let session_rows = session_count.clamp(1, 3) as u16;
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

fn cockpit_content_pane_rect(
    area: Rect,
    layout: AgentCockpitLayout,
    pane_count: usize,
    pane_index: usize,
    active_pane_id: Option<PaneId>,
    pane_ids: impl IntoIterator<Item = PaneId>,
) -> Option<Rect> {
    if pane_count == 0 || pane_index >= pane_count {
        return None;
    }
    if layout == AgentCockpitLayout::Focus {
        let ids = pane_ids.into_iter().collect::<Vec<_>>();
        let focused_index = active_pane_id
            .and_then(|pane_id| ids.iter().position(|id| *id == pane_id))
            .unwrap_or(0);
        return (pane_index == focused_index).then_some(area);
    }
    if pane_count == 1 {
        return Some(area);
    }

    let (direction, first_percent) = match layout {
        AgentCockpitLayout::Right => {
            if area.width >= 64 {
                (Direction::Horizontal, 55)
            } else {
                (Direction::Vertical, 60)
            }
        }
        AgentCockpitLayout::Bottom => (Direction::Vertical, 60),
        AgentCockpitLayout::Wide => {
            if area.width >= 64 {
                (Direction::Horizontal, 65)
            } else {
                (Direction::Vertical, 65)
            }
        }
        AgentCockpitLayout::Focus => unreachable!("focus layout handled before split"),
    };
    let constraints = if pane_count == 2 {
        vec![
            Constraint::Percentage(first_percent),
            Constraint::Percentage(100 - first_percent),
        ]
    } else {
        vec![Constraint::Percentage(100 / pane_count as u16); pane_count]
    };
    let chunks = Layout::default()
        .direction(direction)
        .constraints(constraints)
        .split(area);
    Some(chunks[pane_index])
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
        SessionRole::Generic | SessionRole::Worker | SessionRole::Other(_) => 3,
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
    let attention = attention_label_for_row(session).to_string();
    let unread = unread_label_for_row(session);
    let status = effective_status_summary(session);
    let status_summary = format!(
        "status {}:{} liveness={}",
        status.source,
        status.label,
        liveness_label(session)
    );
    let source_summary = format!(
        "status {}:{} millrace_runtime={} terminal_screen={} operator={} inferred={} attention_sources={} read_open={}",
        status.source,
        status.label,
        runtime_source_value(session, &status),
        terminal_screen_source_value(&status),
        operator_source_value(&status),
        inferred_source_value(&status, inferred_source),
        attention_sources_label(&session.attention),
        session.attention.read_open_count
    );
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
        unread,
        attention,
        attention_rollup: session.attention.clone(),
        status,
        selected,
        focused,
        status_summary,
        source_summary,
    }
}

fn session_role_label(role: &SessionRole) -> &str {
    match role {
        SessionRole::Shell => "shell",
        SessionRole::MillraceDaemon => "millrace_daemon",
        SessionRole::Agent => "millrace_agent",
        SessionRole::Generic | SessionRole::Worker | SessionRole::Other(_) => "generic",
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

fn attention_label_for_row(session: &SessionSummary) -> String {
    if session.attention.open_count > 0 {
        let severity = session
            .attention
            .highest_severity
            .map(|severity| severity.to_string())
            .unwrap_or_else(|| "info".to_string());
        let kinds = if session.attention.kinds.is_empty() {
            "none".to_string()
        } else {
            session
                .attention
                .kinds
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        };
        return format!(
            "open={} unread={} read={} sev={} kinds={} src={}",
            session.attention.open_count,
            session.attention.unread_count,
            session.attention.read_open_count,
            severity,
            kinds,
            attention_sources_label(&session.attention)
        );
    }
    match session.attention_state {
        AttentionState::Unknown => "unknown",
        AttentionState::Active => "active",
        AttentionState::Idle => "idle",
        AttentionState::NeedsAttention => "needs_attention",
        AttentionState::MillraceIdle => "millrace_idle",
        AttentionState::MillraceBusy => "millrace_busy",
    }
    .to_string()
}

fn attention_sources_label(attention: &millrace_sessions_core::state::AttentionRollup) -> String {
    if attention.sources.is_empty() {
        "none".to_string()
    } else {
        attention
            .sources
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn unread_label_for_row(session: &SessionSummary) -> String {
    if has_attention_rollup_evidence(session) {
        format!("unread={}", session.attention.unread_count)
    } else {
        "unread=unavailable".to_string()
    }
}

fn has_attention_rollup_evidence(session: &SessionSummary) -> bool {
    session.attention.open_count > 0
        || session.attention.unread_count > 0
        || session.attention.highest_severity.is_some()
        || !session.attention.kinds.is_empty()
        || session.attention.top_message.is_some()
        || session.attention.status_label.is_some()
        || session.attention.status_detail.is_some()
}

fn effective_status_summary(session: &SessionSummary) -> StatusSummary {
    if session.status_summary.source != StatusSummarySource::Unavailable {
        return session.status_summary.clone();
    }
    StatusSummary::millmux_session(
        process_state_label(&session.process_state),
        Some(liveness_label(session)),
    )
}

fn runtime_source_value(session: &SessionSummary, status: &StatusSummary) -> String {
    if status.source == StatusSummarySource::MillraceRuntime {
        return status.label.clone();
    }
    if session.role == SessionRole::MillraceDaemon {
        match session.attention_state {
            AttentionState::MillraceIdle => "idle".to_string(),
            AttentionState::MillraceBusy => "busy".to_string(),
            _ => "unavailable".to_string(),
        }
    } else {
        "unavailable".to_string()
    }
}

fn terminal_screen_source_value(status: &StatusSummary) -> String {
    if status.source == StatusSummarySource::TerminalScreen {
        status.label.clone()
    } else {
        "unavailable".to_string()
    }
}

fn operator_source_value(status: &StatusSummary) -> String {
    if status.source == StatusSummarySource::Operator {
        status.label.clone()
    } else {
        "unavailable".to_string()
    }
}

fn inferred_source_value(status: &StatusSummary, fallback: &str) -> String {
    if status.source == StatusSummarySource::Inferred {
        status.label.clone()
    } else {
        fallback.to_string()
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

fn pane_for_view(view: UiPaneView) -> Pane {
    let title = match view.kind {
        UiPaneViewKind::SessionTerminal => "Session Terminal",
        UiPaneViewKind::DaemonMonitor => "Daemon Monitor",
        UiPaneViewKind::SessionList => "Session List",
        UiPaneViewKind::CommandOutput => "Command Output",
    };
    let mut pane = match view.kind {
        UiPaneViewKind::SessionTerminal => Pane::agent_terminal(title, view.session_id),
        UiPaneViewKind::DaemonMonitor => Pane::daemon_monitor(title, view.session_id),
        UiPaneViewKind::SessionList => Pane::session_list(),
        UiPaneViewKind::CommandOutput => Pane::command_output(),
    };
    pane.set_view(view);
    pane.focused = false;
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
        assert!(daemon.source_summary.contains("millrace_runtime=busy"));
        assert!(daemon
            .source_summary
            .contains("terminal_screen=unavailable"));
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
            panes: vec![UiPaneContext {
                id: app.active_pane_id.expect("active pane"),
                title: "Agent Terminal".to_string(),
                view: UiPaneView::new(UiPaneViewKind::SessionTerminal, Some(second_agent_id)),
                focused: true,
                stale: false,
                read_only: false,
                overlay_active: false,
            }],
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
    fn agent_cockpit_resize_uses_rendered_three_pane_terminal_rect() {
        let daemon = summary("daemon");
        let daemon_id = daemon.session_id;
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let mut shell = summary("shell");
        shell.role = SessionRole::Shell;
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
        let agent_pane_id = app.active_pane_id.expect("active pane");
        assert!(app
            .split_pane_with_view(
                agent_pane_id,
                UiPaneView::new(UiPaneViewKind::SessionTerminal, Some(shell_id)),
            )
            .is_some());

        assert_eq!(app.agent_terminal_size_for(120, 30), Some((21, 40)));
    }

    #[test]
    fn agent_cockpit_resize_tracks_removed_session_list() {
        let daemon = summary("daemon");
        let daemon_id = daemon.session_id;
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let mut shell = summary("shell");
        shell.role = SessionRole::Shell;
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
        let agent_pane_id = app.active_pane_id.expect("active pane");
        assert!(app
            .split_pane_with_view(
                agent_pane_id,
                UiPaneView::new(UiPaneViewKind::SessionTerminal, Some(shell_id)),
            )
            .is_some());
        let session_list_pane_id = app
            .panes
            .iter()
            .find(|pane| pane.kind == PaneKind::SessionList)
            .map(|pane| pane.id)
            .expect("session list pane");

        assert!(app.close_pane(session_list_pane_id));

        assert_eq!(app.agent_terminal_size_for(120, 30), Some((28, 40)));
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
        let found = terminal
            .search_scrollback("can you", crate::terminal::TerminalSearchDirection::First)
            .expect("physical history match");
        app.update_agent_terminal_view(terminal.snapshot(), terminal.is_following());
        app.set_agent_search_match("search", &found);

        assert_eq!(app.active_view_label(), "search");
        assert!(
            app.status_message.contains("match"),
            "{}",
            app.status_message
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 4),
            KeyAction::CopySearchMatch
        );
        assert_eq!(app.copy_buffer_text(), Some("can you"));
        assert!(!app.search_mode);
    }

    #[test]
    fn managed_raw_attach_validation_fails_before_suspension_for_every_unsafe_target() {
        let daemon = summary("daemon");
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let session_id = agent.session_id;
        let terminal = crate::terminal::TerminalEmulator::new(4, 40, 20);
        let app = AppModel::agent_cockpit(
            UiId::new(),
            agent,
            vec![daemon],
            None,
            BTreeMap::new(),
            AgentTerminalPane::with_snapshot(terminal.snapshot(), true, false),
            AgentCockpitLayout::Right,
            MonitorProfile::Basic,
        );

        assert_eq!(app.managed_raw_attach_target(session_id), Ok(session_id));

        let mut unsafe_app = app.clone();
        unsafe_app.help_overlay.open = true;
        assert_eq!(
            unsafe_app.managed_raw_attach_target(session_id),
            Err("overlay_active")
        );

        let mut unsafe_app = app.clone();
        unsafe_app.command_palette.open = true;
        assert_eq!(
            unsafe_app.managed_raw_attach_target(session_id),
            Err("overlay_active")
        );

        let mut unsafe_app = app.clone();
        unsafe_app.daemon_switcher.open = true;
        assert_eq!(
            unsafe_app.managed_raw_attach_target(session_id),
            Err("overlay_active")
        );

        let mut unsafe_app = app.clone();
        unsafe_app.confirmation = Some(ConfirmationPrompt::new("stop", "agent", "confirm"));
        assert_eq!(
            unsafe_app.managed_raw_attach_target(session_id),
            Err("overlay_active")
        );

        let mut unsafe_app = app.clone();
        unsafe_app.scroll_mode = true;
        assert_eq!(
            unsafe_app.managed_raw_attach_target(session_id),
            Err("terminal_not_live")
        );

        let mut unsafe_app = app.clone();
        unsafe_app.search_mode = true;
        assert_eq!(
            unsafe_app.managed_raw_attach_target(session_id),
            Err("terminal_not_live")
        );

        let mut unsafe_app = app.clone();
        unsafe_app.active_pane_id = None;
        assert_eq!(
            unsafe_app.managed_raw_attach_target(session_id),
            Err("no_focused_pane")
        );

        let mut unsafe_app = app.clone();
        unsafe_app.agent_terminal.as_mut().unwrap().set_read_only();
        assert_eq!(
            unsafe_app.managed_raw_attach_target(session_id),
            Err("input_not_owned")
        );

        let mut unsafe_app = app.clone();
        unsafe_app.active_pane_id = unsafe_app
            .panes
            .iter()
            .find(|pane| pane.kind == PaneKind::DaemonMonitor)
            .map(|pane| pane.id);
        assert_eq!(
            unsafe_app.managed_raw_attach_target(session_id),
            Err("focused_pane_not_terminal")
        );

        let mut unsafe_app = app.clone();
        unsafe_app
            .panes
            .iter_mut()
            .find(|pane| pane.kind == PaneKind::AgentTerminal)
            .unwrap()
            .view
            .session_id = None;
        assert_eq!(
            unsafe_app.managed_raw_attach_target(session_id),
            Err("terminal_unassigned")
        );

        let mut unsafe_app = app.clone();
        unsafe_app
            .panes
            .iter_mut()
            .find(|pane| pane.kind == PaneKind::AgentTerminal)
            .unwrap()
            .stale = true;
        assert_eq!(
            unsafe_app.managed_raw_attach_target(session_id),
            Err("stale_pane")
        );

        let mut unsafe_app = app.clone();
        unsafe_app
            .workspace_sessions
            .iter_mut()
            .find(|session| session.session_id == session_id)
            .unwrap()
            .spawn_mode = SpawnMode::Pipe;
        assert_eq!(
            unsafe_app.managed_raw_attach_target(session_id),
            Err("session_not_pty")
        );

        let mut unsafe_app = app.clone();
        unsafe_app
            .workspace_sessions
            .iter_mut()
            .find(|session| session.session_id == session_id)
            .unwrap()
            .process_state = ProcessState::Exited;
        assert_eq!(
            unsafe_app.managed_raw_attach_target(session_id),
            Err("session_not_running")
        );

        let mut unsafe_app = app.clone();
        unsafe_app
            .workspace_sessions
            .iter_mut()
            .find(|session| session.session_id == session_id)
            .unwrap()
            .capabilities
            .attach = false;
        assert_eq!(
            unsafe_app.managed_raw_attach_target(session_id),
            Err("session_not_attachable")
        );

        let mut unsafe_app = app.clone();
        unsafe_app
            .workspace_sessions
            .retain(|session| session.session_id != session_id);
        assert_eq!(
            unsafe_app.managed_raw_attach_target(session_id),
            Err("session_missing")
        );

        let mut unsafe_app = app.clone();
        unsafe_app.agent_terminal = None;
        assert_eq!(
            unsafe_app.managed_raw_attach_target(session_id),
            Err("terminal_not_attached")
        );

        let mut unsafe_app = app.clone();
        unsafe_app.agent_terminal.as_mut().unwrap().initializing = true;
        assert_eq!(
            unsafe_app.managed_raw_attach_target(session_id),
            Err("terminal_initializing")
        );

        let mut unsafe_app = app.clone();
        unsafe_app.agent_terminal.as_mut().unwrap().input_owner = false;
        assert_eq!(
            unsafe_app.managed_raw_attach_target(session_id),
            Err("input_not_owned")
        );

        assert_eq!(
            app.managed_raw_attach_target(SessionId::new()),
            Err("pane_session_mismatch")
        );
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

    #[test]
    fn agent_cockpit_pane_ids_are_stable_across_redraw() {
        let daemon = summary("daemon");
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let app = AppModel::agent_cockpit(
            UiId::new(),
            agent,
            vec![daemon],
            None,
            BTreeMap::new(),
            AgentTerminalPane::new(10, 40, true, false),
            AgentCockpitLayout::Right,
            MonitorProfile::Basic,
        );
        let before = app
            .pane_contexts()
            .into_iter()
            .map(|pane| pane.id)
            .collect::<Vec<_>>();

        let _ = crate::renderer::render_to_string(&app, 120, 30);

        let after = app
            .pane_contexts()
            .into_iter()
            .map(|pane| pane.id)
            .collect::<Vec<_>>();
        assert_eq!(before, after);
    }

    #[test]
    fn agent_cockpit_view_assignment_does_not_mutate_session_identity() {
        let daemon = summary("daemon");
        let daemon_id = daemon.session_id;
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let agent_id = agent.session_id;
        let mut shell = summary("shell");
        shell.role = SessionRole::Shell;
        shell.name = Some("shell".to_string());
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
        let ids_before = app
            .workspace_sessions
            .iter()
            .map(|session| session.session_id)
            .collect::<Vec<_>>();
        let pane_id = app.active_pane_id.expect("active pane");

        assert!(app.assign_pane_view(
            pane_id,
            UiPaneView::new(UiPaneViewKind::SessionTerminal, Some(shell_id))
        ));

        let ids_after = app
            .workspace_sessions
            .iter()
            .map(|session| session.session_id)
            .collect::<Vec<_>>();
        assert_eq!(ids_before, ids_after);
        assert!(ids_after.contains(&agent_id));
        assert_eq!(app.agent_session_id, Some(agent_id));
        assert_eq!(app.selected_session_id, Some(shell_id));
        assert_eq!(app.focused_attach_session_id(), Some(shell_id));
        assert!(!app.active_terminal_is_attached());
    }

    #[test]
    fn agent_cockpit_refresh_marks_missing_terminal_pane_stale() {
        let daemon = summary("daemon");
        let daemon_id = daemon.session_id;
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let agent_id = agent.session_id;
        let mut shell = summary("shell");
        shell.role = SessionRole::Shell;
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
        app.replace_workspace_sessions(vec![daemon.clone(), agent.clone(), shell]);
        let agent_pane_id = app.active_pane_id.expect("active pane");
        let shell_pane_id = app
            .split_pane_with_view(
                agent_pane_id,
                UiPaneView::new(UiPaneViewKind::SessionTerminal, Some(shell_id)),
            )
            .expect("shell pane");
        assert!(app.focus_pane_id(shell_pane_id));

        app.replace_workspace_sessions(vec![daemon, agent]);

        assert!(app
            .panes
            .iter()
            .any(|pane| pane.id == shell_pane_id && pane.stale));
        assert_ne!(app.active_pane_id, Some(shell_pane_id));
        assert_eq!(app.focused_attach_session_id(), Some(agent_id));
    }

    #[test]
    fn agent_cockpit_direct_daemon_refresh_marks_missing_monitor_pane_stale() {
        let first_daemon = summary("daemon-one");
        let first_daemon_id = first_daemon.session_id;
        let second_daemon = summary("daemon-two");
        let second_daemon_id = second_daemon.session_id;
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let mut app = AppModel::agent_cockpit(
            UiId::new(),
            agent,
            vec![first_daemon.clone(), second_daemon.clone()],
            Some(first_daemon_id),
            BTreeMap::new(),
            AgentTerminalPane::new(10, 40, true, false),
            AgentCockpitLayout::Right,
            MonitorProfile::Basic,
        );
        let agent_pane_id = app.active_pane_id.expect("active pane");
        let second_monitor_pane_id = app
            .split_pane_with_view(
                agent_pane_id,
                UiPaneView::new(UiPaneViewKind::DaemonMonitor, Some(second_daemon_id)),
            )
            .expect("second monitor pane");
        assert!(app.focus_pane_id(second_monitor_pane_id));

        app.replace_daemon_sessions(vec![first_daemon]);

        assert!(app
            .panes
            .iter()
            .any(|pane| pane.id == second_monitor_pane_id && pane.stale));
        assert_ne!(app.active_pane_id, Some(second_monitor_pane_id));
        assert!(app
            .active_pane()
            .is_some_and(|pane| !pane.stale && pane.view.session_id != Some(second_daemon_id)));
    }

    #[test]
    fn agent_cockpit_mismatched_terminal_view_does_not_mutate_attached_terminal_state() {
        let daemon = summary("daemon");
        let daemon_id = daemon.session_id;
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let mut shell = summary("shell");
        shell.role = SessionRole::Shell;
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
        let pane_id = app.active_pane_id.expect("active pane");
        assert!(app.assign_pane_view(
            pane_id,
            UiPaneView::new(UiPaneViewKind::SessionTerminal, Some(shell_id))
        ));
        assert!(app.agent_terminal_is_following());

        app.enter_scroll_mode();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), 2),
            KeyAction::ScrollUp
        );

        assert!(app.agent_terminal_is_following());
        assert_eq!(app.status_message, "terminal not attached");

        app.begin_search_mode();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), 2),
            KeyAction::SearchInput('x')
        );
        assert_eq!(app.status_message, "search: terminal not attached");
        assert!(app
            .agent_terminal
            .as_ref()
            .and_then(AgentTerminalPane::current_match)
            .is_none());
    }

    #[test]
    fn agent_cockpit_mismatched_terminal_exit_scroll_does_not_resume_attached_terminal() {
        let daemon = summary("daemon");
        let daemon_id = daemon.session_id;
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let mut shell = summary("shell");
        shell.role = SessionRole::Shell;
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
        app.set_agent_terminal_following(false);
        let pane_id = app.active_pane_id.expect("active pane");
        assert!(app.assign_pane_view(
            pane_id,
            UiPaneView::new(UiPaneViewKind::SessionTerminal, Some(shell_id))
        ));

        app.exit_scroll_mode();

        assert!(!app.agent_terminal_is_following());
    }

    #[test]
    fn agent_cockpit_session_switch_keeps_focus_on_assigned_terminal_pane() {
        let daemon = summary("daemon");
        let daemon_id = daemon.session_id;
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let agent_id = agent.session_id;
        let mut shell = summary("shell");
        shell.role = SessionRole::Shell;
        let shell_id = shell.session_id;
        let mut worker = summary("worker");
        worker.role = SessionRole::Agent;
        let worker_id = worker.session_id;
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
        app.replace_workspace_sessions(vec![daemon, agent, shell, worker]);
        let first_terminal_pane_id = app.active_pane_id.expect("active pane");
        let shell_pane_id = app
            .split_pane_with_view(
                first_terminal_pane_id,
                UiPaneView::new(UiPaneViewKind::SessionTerminal, Some(shell_id)),
            )
            .expect("shell pane");
        assert!(app.focus_pane_id(shell_pane_id));

        let selection = app.select_workspace_session(worker_id);

        assert_eq!(
            selection,
            WorkspaceSessionSelection::AttachSelected(worker_id)
        );
        assert_eq!(app.active_pane_id, Some(shell_pane_id));
        assert_eq!(app.agent_session_id, Some(worker_id));
        assert_eq!(app.focused_attach_session_id(), Some(worker_id));
        assert_ne!(app.focused_attach_session_id(), Some(agent_id));
    }

    #[test]
    fn agent_cockpit_split_focus_assign_and_close_have_safe_fallback() {
        let daemon = summary("daemon");
        let daemon_id = daemon.session_id;
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let agent_id = agent.session_id;
        let mut app = AppModel::agent_cockpit(
            UiId::new(),
            agent,
            vec![daemon],
            Some(daemon_id),
            BTreeMap::new(),
            AgentTerminalPane::new(10, 40, true, false),
            AgentCockpitLayout::Right,
            MonitorProfile::Basic,
        );
        let agent_pane_id = app.active_pane_id.expect("active pane");
        let new_pane_id = app
            .split_pane_with_view(
                agent_pane_id,
                UiPaneView::new(UiPaneViewKind::DaemonMonitor, Some(daemon_id)),
            )
            .expect("pane split");

        assert!(app.focus_pane_id(new_pane_id));
        assert_eq!(app.focused_session_id(), Some(daemon_id));
        assert!(app.close_pane(new_pane_id));

        assert_ne!(app.active_pane_id, Some(new_pane_id));
        assert_eq!(app.focused_attach_session_id(), Some(agent_id));
    }

    #[test]
    fn agent_cockpit_restore_marks_stale_pane_and_falls_back_safely() {
        let daemon = summary("daemon");
        let daemon_id = daemon.session_id;
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let agent_id = agent.session_id;
        let stale_session_id = SessionId::new();
        let stale_pane_id = PaneId::new();
        let mut app = AppModel::agent_cockpit(
            UiId::new(),
            agent,
            vec![daemon],
            Some(daemon_id),
            BTreeMap::new(),
            AgentTerminalPane::new(10, 40, true, false),
            AgentCockpitLayout::Right,
            MonitorProfile::Basic,
        );
        let context = UiContext {
            schema_version: 1,
            ui_id: app.ui_id,
            mode: UiMode::AgentCockpit,
            active_pane_id: Some(stale_pane_id),
            panes: vec![UiPaneContext {
                id: stale_pane_id,
                title: "Former Agent".to_string(),
                view: UiPaneView::new(UiPaneViewKind::SessionTerminal, Some(stale_session_id)),
                focused: true,
                stale: false,
                read_only: false,
                overlay_active: false,
            }],
            selected_session_id: Some(stale_session_id),
            focused_session_id: Some(stale_session_id),
            focused_pane_kind: Some("agent_terminal".to_string()),
            active_daemon_session_id: Some(daemon_id),
            active_workspace: app.active_workspace.clone(),
            agent_session_id: Some(stale_session_id),
            managed_session_ids: vec![daemon_id, stale_session_id],
            managed_daemon_session_ids: vec![daemon_id],
            monitor_profile: MonitorProfile::Basic,
            daemon_health: Vec::new(),
            updated_at: OffsetDateTime::now_utc(),
        };

        app.restore_ui_context_selection(&context);

        assert!(app
            .pane_contexts()
            .iter()
            .any(|pane| pane.id == stale_pane_id && pane.stale));
        assert_ne!(app.active_pane_id, Some(stale_pane_id));
        assert_eq!(app.focused_attach_session_id(), Some(agent_id));
        assert!(app.status_message.contains("stale pane"));
    }

    #[test]
    fn agent_cockpit_restore_preserves_valid_extra_panes() {
        let first_daemon = summary("daemon-one");
        let first_daemon_id = first_daemon.session_id;
        let second_daemon = summary("daemon-two");
        let second_daemon_id = second_daemon.session_id;
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let agent_id = agent.session_id;
        let mut shell = summary("shell");
        shell.role = SessionRole::Shell;
        let shell_id = shell.session_id;
        let agent_pane_id = PaneId::new();
        let shell_pane_id = PaneId::new();
        let daemon_pane_id = PaneId::new();
        let mut app = AppModel::agent_cockpit(
            UiId::new(),
            agent.clone(),
            vec![first_daemon.clone()],
            Some(first_daemon_id),
            BTreeMap::new(),
            AgentTerminalPane::new(10, 40, true, false),
            AgentCockpitLayout::Right,
            MonitorProfile::Basic,
        );
        app.replace_workspace_sessions(vec![first_daemon, second_daemon, agent, shell]);
        let context = UiContext {
            schema_version: 1,
            ui_id: app.ui_id,
            mode: UiMode::AgentCockpit,
            active_pane_id: Some(shell_pane_id),
            panes: vec![
                UiPaneContext {
                    id: agent_pane_id,
                    title: "Agent".to_string(),
                    view: UiPaneView::new(UiPaneViewKind::SessionTerminal, Some(agent_id)),
                    focused: false,
                    stale: false,
                    read_only: false,
                    overlay_active: false,
                },
                UiPaneContext {
                    id: shell_pane_id,
                    title: "Shell".to_string(),
                    view: UiPaneView::new(UiPaneViewKind::SessionTerminal, Some(shell_id)),
                    focused: true,
                    stale: false,
                    read_only: false,
                    overlay_active: false,
                },
                UiPaneContext {
                    id: daemon_pane_id,
                    title: "Second Monitor".to_string(),
                    view: UiPaneView::new(UiPaneViewKind::DaemonMonitor, Some(second_daemon_id)),
                    focused: false,
                    stale: false,
                    read_only: false,
                    overlay_active: false,
                },
            ],
            selected_session_id: Some(shell_id),
            focused_session_id: Some(shell_id),
            focused_pane_kind: Some("agent_terminal".to_string()),
            active_daemon_session_id: Some(second_daemon_id),
            active_workspace: app.active_workspace.clone(),
            agent_session_id: Some(shell_id),
            managed_session_ids: vec![first_daemon_id, second_daemon_id, agent_id, shell_id],
            managed_daemon_session_ids: vec![first_daemon_id, second_daemon_id],
            monitor_profile: MonitorProfile::Basic,
            daemon_health: Vec::new(),
            updated_at: OffsetDateTime::now_utc(),
        };

        app.restore_ui_context_selection(&context);

        let panes = app.pane_contexts();
        assert!(panes
            .iter()
            .any(|pane| pane.id == agent_pane_id && !pane.stale));
        assert!(panes
            .iter()
            .any(|pane| pane.id == shell_pane_id && !pane.stale));
        assert!(panes
            .iter()
            .any(|pane| pane.id == daemon_pane_id && !pane.stale));
        assert_eq!(app.active_pane_id, Some(shell_pane_id));
        assert_eq!(app.focused_attach_session_id(), Some(shell_id));
    }

    #[test]
    fn agent_cockpit_restore_active_daemon_does_not_rewrite_terminal_panes() {
        let daemon = summary("daemon");
        let daemon_id = daemon.session_id;
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let agent_id = agent.session_id;
        let mut shell = summary("shell");
        shell.role = SessionRole::Shell;
        let shell_id = shell.session_id;
        let agent_pane_id = PaneId::new();
        let shell_pane_id = PaneId::new();
        let daemon_pane_id = PaneId::new();
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
        let context = UiContext {
            schema_version: 1,
            ui_id: app.ui_id,
            mode: UiMode::AgentCockpit,
            active_pane_id: Some(daemon_pane_id),
            panes: vec![
                UiPaneContext {
                    id: agent_pane_id,
                    title: "Agent".to_string(),
                    view: UiPaneView::new(UiPaneViewKind::SessionTerminal, Some(agent_id)),
                    focused: false,
                    stale: false,
                    read_only: false,
                    overlay_active: false,
                },
                UiPaneContext {
                    id: shell_pane_id,
                    title: "Shell".to_string(),
                    view: UiPaneView::new(UiPaneViewKind::SessionTerminal, Some(shell_id)),
                    focused: false,
                    stale: false,
                    read_only: false,
                    overlay_active: false,
                },
                UiPaneContext {
                    id: daemon_pane_id,
                    title: "Daemon".to_string(),
                    view: UiPaneView::new(UiPaneViewKind::DaemonMonitor, Some(daemon_id)),
                    focused: true,
                    stale: false,
                    read_only: false,
                    overlay_active: false,
                },
            ],
            selected_session_id: Some(daemon_id),
            focused_session_id: Some(daemon_id),
            focused_pane_kind: Some("daemon_monitor".to_string()),
            active_daemon_session_id: Some(daemon_id),
            active_workspace: app.active_workspace.clone(),
            agent_session_id: Some(shell_id),
            managed_session_ids: vec![daemon_id, agent_id, shell_id],
            managed_daemon_session_ids: vec![daemon_id],
            monitor_profile: MonitorProfile::Basic,
            daemon_health: Vec::new(),
            updated_at: OffsetDateTime::now_utc(),
        };

        app.restore_ui_context_selection(&context);

        let panes = app.pane_contexts();
        assert!(panes.iter().any(|pane| {
            pane.id == agent_pane_id
                && pane.view.kind == UiPaneViewKind::SessionTerminal
                && pane.view.session_id == Some(agent_id)
        }));
        assert!(panes.iter().any(|pane| {
            pane.id == shell_pane_id
                && pane.view.kind == UiPaneViewKind::SessionTerminal
                && pane.view.session_id == Some(shell_id)
        }));
        assert_eq!(app.active_pane_id, Some(daemon_pane_id));
        assert_eq!(app.focused_session_id(), Some(daemon_id));
    }

    #[test]
    fn agent_cockpit_render_respects_closed_panes() {
        let daemon = summary("daemon");
        let daemon_id = daemon.session_id;
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let mut app = AppModel::agent_cockpit(
            UiId::new(),
            agent,
            vec![daemon],
            Some(daemon_id),
            BTreeMap::new(),
            AgentTerminalPane::new(10, 40, true, false),
            AgentCockpitLayout::Right,
            MonitorProfile::Basic,
        );
        let daemon_pane_id = app
            .panes
            .iter()
            .find(|pane| pane.kind == PaneKind::DaemonMonitor)
            .map(|pane| pane.id)
            .expect("daemon pane");

        assert!(app.close_pane(daemon_pane_id));

        let rendered = crate::renderer::render_to_string(&app, 120, 30);
        assert!(rendered.contains("Agent Terminal"), "{rendered}");
        assert!(!rendered.contains("Daemon Monitor"), "{rendered}");
    }

    #[test]
    fn agent_cockpit_restore_preserves_closed_default_panes() {
        let daemon = summary("daemon");
        let daemon_id = daemon.session_id;
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
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
        for kind in [
            PaneKind::DaemonMonitor,
            PaneKind::SessionList,
            PaneKind::CommandOutput,
        ] {
            let pane_id = app
                .panes
                .iter()
                .find(|pane| pane.kind == kind)
                .map(|pane| pane.id)
                .expect("default pane");
            assert!(app.close_pane(pane_id));
        }
        let context = app.ui_context();

        let mut restored = AppModel::agent_cockpit(
            UiId::new(),
            agent,
            vec![daemon],
            Some(daemon_id),
            BTreeMap::new(),
            AgentTerminalPane::new(10, 40, true, false),
            AgentCockpitLayout::Right,
            MonitorProfile::Basic,
        );
        restored.restore_ui_context_selection(&context);

        assert!(restored
            .panes
            .iter()
            .any(|pane| pane.kind == PaneKind::AgentTerminal));
        assert!(!restored
            .panes
            .iter()
            .any(|pane| pane.kind == PaneKind::DaemonMonitor));
        assert!(!restored
            .panes
            .iter()
            .any(|pane| pane.kind == PaneKind::SessionList));
        assert!(!restored
            .panes
            .iter()
            .any(|pane| pane.kind == PaneKind::CommandOutput));
    }

    #[test]
    fn agent_cockpit_command_output_renders_only_when_pane_is_visible() {
        let daemon = summary("daemon");
        let daemon_id = daemon.session_id;
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let mut app = AppModel::agent_cockpit(
            UiId::new(),
            agent,
            vec![daemon],
            Some(daemon_id),
            BTreeMap::new(),
            AgentTerminalPane::new(10, 40, true, false),
            AgentCockpitLayout::Right,
            MonitorProfile::Basic,
        );
        app.set_command_success(
            vec!["millmux".to_string(), "status".to_string()],
            "daemon",
            vec!["ok".to_string()],
        );
        let command_pane_id = app
            .panes
            .iter()
            .find(|pane| pane.kind == PaneKind::CommandOutput)
            .map(|pane| pane.id)
            .expect("command output pane");

        assert!(crate::renderer::render_to_string(&app, 120, 30).contains("Command Output"));
        assert!(app.close_pane(command_pane_id));

        let rendered = crate::renderer::render_to_string(&app, 120, 30);
        assert!(!rendered.contains("Command Output"), "{rendered}");
    }

    #[test]
    fn session_list_scroll_does_not_mutate_daemon_log_view() {
        let daemon = summary("daemon");
        let daemon_id = daemon.session_id;
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let mut app = AppModel::agent_cockpit(
            UiId::new(),
            agent,
            vec![daemon],
            Some(daemon_id),
            BTreeMap::from([(daemon_id, vec!["one".to_string(), "two".to_string()])]),
            AgentTerminalPane::new(10, 40, true, false),
            AgentCockpitLayout::Right,
            MonitorProfile::Basic,
        );
        assert!(app.focus_pane_kind(PaneKind::SessionList));
        assert!(app.line_log.is_following());

        app.enter_scroll_mode();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), 2),
            KeyAction::ScrollUp
        );

        assert!(app.line_log.is_following());
        assert_eq!(app.status_message, "scroll unavailable for view");
    }

    #[test]
    fn session_list_exit_scroll_does_not_mutate_daemon_log_view() {
        let daemon = summary("daemon");
        let daemon_id = daemon.session_id;
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let mut app = AppModel::agent_cockpit(
            UiId::new(),
            agent,
            vec![daemon],
            Some(daemon_id),
            BTreeMap::from([(daemon_id, vec!["one".to_string(), "two".to_string()])]),
            AgentTerminalPane::new(10, 40, true, false),
            AgentCockpitLayout::Right,
            MonitorProfile::Basic,
        );
        assert!(app.focus_pane_kind(PaneKind::DaemonMonitor));
        app.enter_scroll_mode();
        app.scroll_active_view_up(1, 1);
        assert!(app.line_log.is_scrolled());
        assert!(app.focus_pane_kind(PaneKind::SessionList));

        app.exit_scroll_mode();

        assert!(app.line_log.is_scrolled());
        assert_eq!(app.status_message, "live");
    }

    #[test]
    fn agent_cockpit_context_persists_pane_views_and_follow_state() {
        let daemon = summary("daemon");
        let daemon_id = daemon.session_id;
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
        let agent_id = agent.session_id;
        let mut app = AppModel::agent_cockpit(
            UiId::new(),
            agent,
            vec![daemon],
            Some(daemon_id),
            BTreeMap::new(),
            AgentTerminalPane::new(10, 40, true, false),
            AgentCockpitLayout::Right,
            MonitorProfile::Basic,
        );
        app.set_agent_terminal_following(false);

        let context = app.ui_context();
        let terminal_pane = context
            .panes
            .iter()
            .find(|pane| pane.view.kind == UiPaneViewKind::SessionTerminal)
            .expect("terminal pane context");
        assert_eq!(terminal_pane.view.session_id, Some(agent_id));
        assert_eq!(terminal_pane.view.view_mode, UiPaneViewMode::Scrollback);
        assert!(terminal_pane.focused);
        assert!(context
            .panes
            .iter()
            .any(|pane| pane.view.kind == UiPaneViewKind::DaemonMonitor
                && pane.view.session_id == Some(daemon_id)));
        assert!(context
            .panes
            .iter()
            .any(|pane| pane.view.kind == UiPaneViewKind::SessionList));
    }

    #[test]
    fn attention_rollup_and_status_source_feed_workspace_rows() {
        let mut daemon = summary("daemon");
        daemon.attention = millrace_sessions_core::state::AttentionRollup {
            schema_version: 1,
            open_count: 2,
            unread_count: 1,
            highest_severity: Some(millrace_sessions_core::state::AttentionSeverity::Critical),
            kinds: vec![
                millrace_sessions_core::state::AttentionKind::Unread,
                millrace_sessions_core::state::AttentionKind::Blocked,
            ],
            sources: vec![
                millrace_sessions_core::state::AttentionSource::Agent,
                millrace_sessions_core::state::AttentionSource::Operator,
            ],
            read_open_count: 1,
            top_message: Some("operator needed".to_string()),
            status_label: Some("blocked".to_string()),
            status_detail: Some("approval required".to_string()),
        };
        daemon.status_summary = millrace_sessions_core::state::StatusSummary {
            schema_version: 1,
            source: millrace_sessions_core::state::StatusSummarySource::MillraceRuntime,
            label: "idle".to_string(),
            detail: Some("runtime reported idle".to_string()),
            updated_at: Some("2026-05-26T00:00:02Z".to_string()),
        };
        let daemon_id = daemon.session_id;
        let mut agent = summary("agent");
        agent.role = SessionRole::Agent;
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
        let row = rows
            .iter()
            .find(|row| row.session_id == daemon_id)
            .expect("daemon row");

        assert_eq!(row.attention_rollup.open_count, 2);
        assert_eq!(row.attention_rollup.unread_count, 1);
        assert_eq!(row.status.source.to_string(), "millrace_runtime");
        assert_eq!(row.status.label, "idle");
        assert_eq!(row.unread, "unread=1");
        assert!(row.attention.contains("critical"), "{}", row.attention);
        assert!(
            row.attention.contains("src=agent,operator"),
            "{}",
            row.attention
        );
        assert!(
            row.source_summary.contains("status millrace_runtime:idle"),
            "{}",
            row.source_summary
        );
        assert!(
            row.source_summary.contains("read_open=1"),
            "{}",
            row.source_summary
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
            attention: Default::default(),
            status_summary: Default::default(),
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
