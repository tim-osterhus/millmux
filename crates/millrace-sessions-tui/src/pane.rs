use std::{collections::VecDeque, fmt, str::FromStr};

use millrace_sessions_core::{
    ids::{PaneId, SessionId},
    state::{AttentionRollup, StatusSummary, UiPaneContext, UiPaneView, UiPaneViewKind},
};

use crate::terminal::TerminalSnapshot;

pub const COCKPIT_SESSION_LIST_HEIGHT: u16 = 7;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneKind {
    AgentTerminal,
    DaemonMonitor,
    DaemonList,
    SessionList,
    CommandOutput,
    StatusBar,
    HelpOverlay,
    CommandPalette,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonConsoleLayout {
    Single,
    Split,
    Grid,
    List,
}

impl DaemonConsoleLayout {
    pub fn default_for_daemon_count(count: usize) -> Self {
        if count > 1 {
            Self::List
        } else {
            Self::Single
        }
    }
}

impl fmt::Display for DaemonConsoleLayout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Single => "single",
            Self::Split => "split",
            Self::Grid => "grid",
            Self::List => "list",
        })
    }
}

impl FromStr for DaemonConsoleLayout {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "single" => Ok(Self::Single),
            "split" => Ok(Self::Split),
            "grid" => Ok(Self::Grid),
            "list" => Ok(Self::List),
            other => Err(format!("invalid daemon console layout: {other}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentCockpitLayout {
    #[default]
    Right,
    Bottom,
    Wide,
    Focus,
}

impl AgentCockpitLayout {
    pub fn default_for_size(cols: u16) -> Self {
        if cols >= 100 {
            Self::Right
        } else {
            Self::Bottom
        }
    }
}

impl fmt::Display for AgentCockpitLayout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Right => "right",
            Self::Bottom => "bottom",
            Self::Wide => "wide",
            Self::Focus => "focus",
        })
    }
}

impl FromStr for AgentCockpitLayout {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "right" => Ok(Self::Right),
            "bottom" => Ok(Self::Bottom),
            "wide" => Ok(Self::Wide),
            "focus" => Ok(Self::Focus),
            other => Err(format!("invalid agent cockpit layout: {other}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub id: PaneId,
    pub kind: PaneKind,
    pub title: String,
    pub view: UiPaneView,
    pub session_id: Option<SessionId>,
    pub focused: bool,
    pub stale: bool,
}

impl Pane {
    pub fn agent_terminal(title: impl Into<String>, session_id: Option<SessionId>) -> Self {
        Self {
            id: PaneId::new(),
            kind: PaneKind::AgentTerminal,
            title: title.into(),
            view: UiPaneView::new(UiPaneViewKind::SessionTerminal, session_id),
            session_id,
            focused: true,
            stale: false,
        }
    }

    pub fn daemon_monitor(title: impl Into<String>, session_id: Option<SessionId>) -> Self {
        Self {
            id: PaneId::new(),
            kind: PaneKind::DaemonMonitor,
            title: title.into(),
            view: UiPaneView::new(UiPaneViewKind::DaemonMonitor, session_id),
            session_id,
            focused: true,
            stale: false,
        }
    }

    pub fn command_output() -> Self {
        Self {
            id: PaneId::new(),
            kind: PaneKind::CommandOutput,
            title: "Command Output".to_string(),
            view: UiPaneView::new(UiPaneViewKind::CommandOutput, None),
            session_id: None,
            focused: false,
            stale: false,
        }
    }

    pub fn daemon_list() -> Self {
        Self {
            id: PaneId::new(),
            kind: PaneKind::DaemonList,
            title: "Daemon List".to_string(),
            view: UiPaneView::new(UiPaneViewKind::SessionList, None),
            session_id: None,
            focused: false,
            stale: false,
        }
    }

    pub fn session_list() -> Self {
        Self {
            id: PaneId::new(),
            kind: PaneKind::SessionList,
            title: "Session List".to_string(),
            view: UiPaneView::new(UiPaneViewKind::SessionList, None),
            session_id: None,
            focused: false,
            stale: false,
        }
    }

    pub fn stale_from_context(context: &UiPaneContext) -> Self {
        Self::from_context(context, true)
    }

    pub fn from_context(context: &UiPaneContext, stale: bool) -> Self {
        Self {
            id: context.id,
            kind: pane_kind_for_view(context.view.kind),
            title: context.title.clone(),
            view: context.view.clone(),
            session_id: context.view.session_id,
            focused: false,
            stale,
        }
    }

    pub fn set_view(&mut self, view: UiPaneView) {
        self.kind = pane_kind_for_view(view.kind);
        self.session_id = view.session_id;
        self.view = view;
    }

    pub fn to_context(&self) -> UiPaneContext {
        UiPaneContext {
            id: self.id,
            title: self.title.clone(),
            view: self.view.clone(),
            focused: self.focused,
            stale: self.stale,
            read_only: false,
            overlay_active: false,
        }
    }
}

fn pane_kind_for_view(kind: UiPaneViewKind) -> PaneKind {
    match kind {
        UiPaneViewKind::SessionTerminal => PaneKind::AgentTerminal,
        UiPaneViewKind::DaemonMonitor => PaneKind::DaemonMonitor,
        UiPaneViewKind::SessionList => PaneKind::SessionList,
        UiPaneViewKind::CommandOutput => PaneKind::CommandOutput,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSessionRow {
    pub session_id: SessionId,
    pub role: String,
    pub name: String,
    pub location: String,
    pub worktree: String,
    pub branch: String,
    pub process_state: String,
    pub liveness: String,
    pub unread: String,
    pub attention: String,
    pub attention_rollup: AttentionRollup,
    pub status: StatusSummary,
    pub selected: bool,
    pub focused: bool,
    pub status_summary: String,
    pub source_summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTerminalPane {
    pub snapshot: TerminalSnapshot,
    pub input_owner: bool,
    pub read_only: bool,
    pub follow: bool,
    pub active_search: Option<ActiveSearch>,
    selected_search_match: Option<SearchMatch>,
    pub initializing: bool,
    pub rows: u16,
    pub cols: u16,
}

impl AgentTerminalPane {
    pub fn new(rows: u16, cols: u16, input_owner: bool, read_only: bool) -> Self {
        Self {
            snapshot: TerminalSnapshot::blank(rows, cols),
            input_owner,
            read_only,
            follow: true,
            active_search: None,
            selected_search_match: None,
            initializing: true,
            rows: rows.max(1),
            cols: cols.max(1),
        }
    }

    pub fn with_snapshot(snapshot: TerminalSnapshot, input_owner: bool, read_only: bool) -> Self {
        Self {
            rows: snapshot.rows,
            cols: snapshot.cols,
            snapshot,
            input_owner,
            read_only,
            follow: true,
            active_search: None,
            selected_search_match: None,
            initializing: false,
        }
    }

    pub fn set_snapshot(&mut self, snapshot: TerminalSnapshot) {
        self.set_snapshot_view(snapshot, true);
    }

    pub fn set_snapshot_view(&mut self, snapshot: TerminalSnapshot, follow: bool) {
        self.rows = snapshot.rows;
        self.cols = snapshot.cols;
        self.snapshot = snapshot;
        self.follow = follow;
        self.active_search = None;
        self.selected_search_match = None;
        self.initializing = false;
    }

    pub fn set_following(&mut self, follow: bool) {
        self.follow = follow;
    }

    pub fn set_read_only(&mut self) {
        self.read_only = true;
        self.input_owner = false;
    }

    pub fn set_input_owner(&mut self, input_owner: bool) {
        self.input_owner = input_owner;
        self.read_only = !input_owner;
    }

    pub fn is_following(&self) -> bool {
        self.follow
    }

    pub fn is_scrolled(&self) -> bool {
        !self.follow
    }

    pub fn search(&mut self, query: impl Into<String>) -> Option<SearchMatch> {
        let query = query.into();
        if query.is_empty() {
            self.active_search = None;
            self.selected_search_match = None;
            return None;
        }

        let lines = self.snapshot.plain_lines();
        let Some(found) = lines.iter().position(|line| line.contains(query.as_str())) else {
            self.active_search = None;
            self.selected_search_match = None;
            return None;
        };
        self.active_search = Some(ActiveSearch {
            query: query.clone(),
            current_index: found,
        });
        let found = self.search_match(found, query);
        self.selected_search_match = Some(found.clone());
        Some(found)
    }

    pub fn next_match(&mut self) -> Option<SearchMatch> {
        let search = self.active_search.clone()?;
        let lines = self.snapshot.plain_lines();
        let len = lines.len();
        for step in 1..=len {
            let index = (search.current_index + step) % len;
            if lines[index].contains(search.query.as_str()) {
                self.active_search = Some(ActiveSearch {
                    query: search.query.clone(),
                    current_index: index,
                });
                let found = self.search_match(index, search.query);
                self.selected_search_match = Some(found.clone());
                return Some(found);
            }
        }
        None
    }

    pub fn previous_match(&mut self) -> Option<SearchMatch> {
        let search = self.active_search.clone()?;
        let lines = self.snapshot.plain_lines();
        let len = lines.len();
        for step in 1..=len {
            let index = (search.current_index + len - step) % len;
            if lines[index].contains(search.query.as_str()) {
                self.active_search = Some(ActiveSearch {
                    query: search.query.clone(),
                    current_index: index,
                });
                let found = self.search_match(index, search.query);
                self.selected_search_match = Some(found.clone());
                return Some(found);
            }
        }
        None
    }

    pub fn current_match(&self) -> Option<SearchMatch> {
        if let Some(found) = &self.selected_search_match {
            return Some(found.clone());
        }
        let search = self.active_search.clone()?;
        Some(self.search_match(search.current_index, search.query))
    }

    pub fn set_search_match(&mut self, found: SearchMatch) {
        self.active_search = None;
        self.selected_search_match = Some(found);
    }

    pub fn clear_search(&mut self) {
        self.active_search = None;
        self.selected_search_match = None;
    }

    fn search_match(&self, index: usize, query: String) -> SearchMatch {
        SearchMatch {
            index,
            occurrence: 0,
            start_cell: 0,
            end_cell: 0,
            matched_text: query.clone(),
            query,
            line: self.snapshot.line_text(index).unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineLogPane {
    capacity: usize,
    lines: VecDeque<String>,
    offset_from_bottom: usize,
    follow: bool,
    active_search: Option<ActiveSearch>,
}

impl LineLogPane {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            lines: VecDeque::with_capacity(capacity),
            offset_from_bottom: 0,
            follow: true,
            active_search: None,
        }
    }

    pub fn with_prior_lines(capacity: usize, lines: impl IntoIterator<Item = String>) -> Self {
        let mut pane = Self::new(capacity);
        pane.extend_prior_lines(lines);
        pane
    }

    pub fn extend_prior_lines(&mut self, lines: impl IntoIterator<Item = String>) {
        for line in lines {
            self.push_line(line);
        }
        self.jump_bottom();
    }

    pub fn replace_lines_preserving_view(&mut self, lines: impl IntoIterator<Item = String>) {
        let was_following = self.follow;
        let offset = self.offset_from_bottom;
        self.lines.clear();
        self.active_search = None;
        for line in lines {
            self.push_line(line);
        }
        if was_following {
            self.jump_bottom();
        } else {
            self.offset_from_bottom = offset.min(self.max_offset(1));
            self.follow = self.offset_from_bottom == 0;
        }
    }

    pub fn append_live_line(&mut self, line: impl Into<String>) {
        self.push_line(line.into());
        if self.follow {
            self.offset_from_bottom = 0;
        }
    }

    pub fn visible_lines(&self, height: u16) -> Vec<String> {
        let height = usize::from(height);
        if height == 0 || self.lines.is_empty() {
            return Vec::new();
        }

        let len = self.lines.len();
        let offset = self.offset_from_bottom.min(self.max_offset(height));
        let start = len.saturating_sub(height + offset);
        self.lines
            .iter()
            .skip(start)
            .take(height)
            .cloned()
            .collect()
    }

    pub fn scroll_up(&mut self, height: u16, lines: usize) {
        let max_offset = self.max_offset(usize::from(height));
        self.offset_from_bottom = (self.offset_from_bottom + lines).min(max_offset);
        self.follow = self.offset_from_bottom == 0;
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.offset_from_bottom = self.offset_from_bottom.saturating_sub(lines);
        self.follow = self.offset_from_bottom == 0;
    }

    pub fn page_up(&mut self, height: u16) {
        self.scroll_up(height, usize::from(height).max(1));
    }

    pub fn page_down(&mut self, height: u16) {
        self.scroll_down(usize::from(height).max(1));
    }

    pub fn jump_top(&mut self, height: u16) {
        self.offset_from_bottom = self.max_offset(usize::from(height));
        self.follow = self.offset_from_bottom == 0;
    }

    pub fn jump_bottom(&mut self) {
        self.offset_from_bottom = 0;
        self.follow = true;
    }

    pub fn search(&mut self, query: impl Into<String>) -> Option<SearchMatch> {
        let query = query.into();
        if query.is_empty() {
            self.active_search = None;
            return None;
        }

        let Some(found) = self
            .lines
            .iter()
            .position(|line| line.contains(query.as_str()))
        else {
            self.active_search = None;
            return None;
        };
        self.active_search = Some(ActiveSearch {
            query: query.clone(),
            current_index: found,
        });
        Some(self.search_match(found, query))
    }

    pub fn next_match(&mut self) -> Option<SearchMatch> {
        let search = self.active_search.clone()?;
        let len = self.lines.len();
        for step in 1..=len {
            let index = (search.current_index + step) % len;
            if self.lines[index].contains(search.query.as_str()) {
                self.active_search = Some(ActiveSearch {
                    query: search.query.clone(),
                    current_index: index,
                });
                return Some(self.search_match(index, search.query));
            }
        }
        None
    }

    pub fn previous_match(&mut self) -> Option<SearchMatch> {
        let search = self.active_search.clone()?;
        let len = self.lines.len();
        for step in 1..=len {
            let index = (search.current_index + len - step) % len;
            if self.lines[index].contains(search.query.as_str()) {
                self.active_search = Some(ActiveSearch {
                    query: search.query.clone(),
                    current_index: index,
                });
                return Some(self.search_match(index, search.query));
            }
        }
        None
    }

    pub fn current_match(&self) -> Option<SearchMatch> {
        let search = self.active_search.clone()?;
        Some(self.search_match(search.current_index, search.query))
    }

    pub fn is_following(&self) -> bool {
        self.follow
    }

    pub fn is_scrolled(&self) -> bool {
        !self.follow
    }

    pub fn set_following(&mut self, follow: bool) {
        self.follow = follow;
        if follow {
            self.offset_from_bottom = 0;
        }
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn lines(&self) -> Vec<String> {
        self.lines.iter().cloned().collect()
    }

    fn push_line(&mut self, line: String) {
        if self.capacity == 0 {
            return;
        }

        while self.lines.len() >= self.capacity {
            self.lines.pop_front();
            if let Some(search) = &mut self.active_search {
                search.current_index = search.current_index.saturating_sub(1);
            }
        }
        self.lines.push_back(line);
        self.offset_from_bottom = self.offset_from_bottom.min(self.max_offset(1));
    }

    fn max_offset(&self, height: usize) -> usize {
        self.lines.len().saturating_sub(height)
    }

    fn search_match(&self, index: usize, query: String) -> SearchMatch {
        SearchMatch {
            index,
            occurrence: 0,
            start_cell: 0,
            end_cell: 0,
            matched_text: query.clone(),
            query,
            line: self.lines.get(index).cloned().unwrap_or_default(),
        }
    }
}

impl Default for LineLogPane {
    fn default() -> Self {
        Self::new(4000)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveSearch {
    query: String,
    current_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    pub index: usize,
    pub occurrence: usize,
    pub start_cell: usize,
    pub end_cell: usize,
    pub query: String,
    pub line: String,
    pub matched_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPalette {
    pub open: bool,
    pub input: String,
    pub target: String,
    pub commands: Vec<String>,
}

impl CommandPalette {
    pub fn default_commands() -> Self {
        Self {
            open: false,
            input: String::new(),
            target: String::new(),
            commands: vec![
                "status".to_string(),
                "inspect".to_string(),
                "logs".to_string(),
                "events".to_string(),
                "doctor".to_string(),
                "stop".to_string(),
                "kill".to_string(),
                "delete".to_string(),
                "purge".to_string(),
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpOverlay {
    pub open: bool,
    pub entries: Vec<(&'static str, &'static str)>,
}

impl Default for HelpOverlay {
    fn default() -> Self {
        Self {
            open: false,
            entries: vec![
                ("Ctrl-] Tab", "switch focus"),
                ("Ctrl-] [", "scroll mode"),
                ("Ctrl-] ]", "live follow"),
                ("/", "search scrollback"),
                ("n / N", "next / previous match"),
                ("Enter", "copy match"),
                ("Esc", "close search / scroll"),
                ("Ctrl-] a", "managed raw attach"),
                ("Ctrl-] d", "detach cockpit / return from raw"),
                ("Ctrl-] p", "command palette"),
                ("Ctrl-] l", "session switcher"),
                ("Ctrl-] ?", "help"),
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DaemonSwitcherOverlay {
    pub open: bool,
    pub selected_session_id: Option<SessionId>,
}

impl DaemonSwitcherOverlay {
    pub fn open_with(&mut self, session_id: Option<SessionId>) {
        self.open = true;
        self.selected_session_id = session_id;
    }

    pub fn close(&mut self) {
        self.open = false;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub argv: Vec<String>,
    pub target: String,
    pub state: CommandOutputState,
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
}

impl CommandOutput {
    pub fn hidden() -> Self {
        Self {
            argv: Vec::new(),
            target: String::new(),
            state: CommandOutputState::Idle,
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    pub fn running(argv: Vec<String>, target: impl Into<String>) -> Self {
        Self {
            argv,
            target: target.into(),
            state: CommandOutputState::Running,
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    pub fn succeeded(argv: Vec<String>, target: impl Into<String>, stdout: Vec<String>) -> Self {
        Self {
            argv,
            target: target.into(),
            state: CommandOutputState::Succeeded,
            stdout,
            stderr: Vec::new(),
        }
    }

    pub fn failed(argv: Vec<String>, target: impl Into<String>, stderr: Vec<String>) -> Self {
        Self {
            argv,
            target: target.into(),
            state: CommandOutputState::Failed,
            stdout: Vec::new(),
            stderr,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.state != CommandOutputState::Idle
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmationPrompt {
    pub operation: String,
    pub target: String,
    pub challenge: String,
    pub input: String,
}

impl ConfirmationPrompt {
    pub fn new(
        operation: impl Into<String>,
        target: impl Into<String>,
        challenge: impl Into<String>,
    ) -> Self {
        Self {
            operation: operation.into(),
            target: target.into(),
            challenge: challenge.into(),
            input: String::new(),
        }
    }

    pub fn matches_challenge(&self) -> bool {
        self.input.trim() == self.challenge
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutputState {
    Idle,
    Running,
    Succeeded,
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_log_pauses_follow_when_scrolled_up_and_resumes_at_bottom() {
        let mut pane =
            LineLogPane::with_prior_lines(10, ["one", "two", "three"].map(str::to_string));

        assert!(pane.is_following());
        assert_eq!(pane.visible_lines(2), ["two", "three"]);

        pane.scroll_up(2, 1);
        assert!(pane.is_scrolled());
        assert_eq!(pane.visible_lines(2), ["one", "two"]);

        pane.append_live_line("four");
        assert_eq!(pane.visible_lines(2), ["two", "three"]);

        pane.jump_bottom();
        assert!(pane.is_following());
        assert_eq!(pane.visible_lines(2), ["three", "four"]);
    }

    #[test]
    fn line_log_searches_buffered_output() {
        let mut pane = LineLogPane::with_prior_lines(
            10,
            ["daemon ready", "agent ready", "daemon idle"].map(str::to_string),
        );

        assert_eq!(pane.search("daemon").unwrap().index, 0);
        assert_eq!(pane.next_match().unwrap().index, 2);
        assert_eq!(pane.previous_match().unwrap().index, 0);
        assert!(pane.search("missing").is_none());
    }

    #[test]
    fn line_log_enforces_capacity() {
        let mut pane = LineLogPane::new(3);
        for index in 0..10 {
            pane.append_live_line(format!("line-{index}"));
        }

        assert_eq!(pane.len(), 3);
        assert_eq!(pane.lines(), ["line-7", "line-8", "line-9"]);
        assert!(pane.is_following());
    }

    #[test]
    fn daemon_console_layout_parses_known_values() {
        assert_eq!(
            "single".parse::<DaemonConsoleLayout>().unwrap(),
            DaemonConsoleLayout::Single
        );
        assert_eq!(
            "grid".parse::<DaemonConsoleLayout>().unwrap(),
            DaemonConsoleLayout::Grid
        );
        assert!("unknown".parse::<DaemonConsoleLayout>().is_err());
    }

    #[test]
    fn help_overlay_includes_search_copy_and_scrollback_keys() {
        let help = HelpOverlay::default();
        assert!(help
            .entries
            .iter()
            .any(|(key, action)| *key == "/" && action.contains("search")));
        assert!(help
            .entries
            .iter()
            .any(|(key, action)| *key == "n / N" && action.contains("previous")));
        assert!(help
            .entries
            .iter()
            .any(|(key, action)| *key == "Enter" && action.contains("copy")));
        assert!(help
            .entries
            .iter()
            .any(|(key, action)| { *key == "Ctrl-] a" && action.contains("managed raw attach") }));
        assert!(help.entries.iter().any(|(key, action)| {
            *key == "Ctrl-] d"
                && action.contains("detach cockpit")
                && action.contains("return from raw")
        }));
    }

    #[test]
    fn command_output_can_show_success_and_failure() {
        let ok = CommandOutput::succeeded(
            vec!["millmux".to_string(), "status".to_string()],
            "/tmp/work",
            vec!["{}".to_string()],
        );
        assert!(ok.is_visible());
        assert_eq!(ok.state, CommandOutputState::Succeeded);

        let failed = CommandOutput::failed(
            vec!["millmux".to_string(), "stop".to_string()],
            "/tmp/work",
            vec!["missing confirmation".to_string()],
        );
        assert_eq!(failed.state, CommandOutputState::Failed);
    }
}
