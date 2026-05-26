use millrace_sessions_core::{
    ids::{SessionId, UiId},
    protocol::{
        UiContextCloseRequest, UiContextCloseResponse, UiContextSetRequest, UiContextSetResponse,
        M1_PROTOCOL_VERSION,
    },
    state::{UiContextPaths, UiEventKind},
};
use millrace_sessions_tui::{AppModel, ShellExit, TuiShell, UiContextSink};

#[derive(Default)]
struct FixtureControl {
    set_requests: Vec<UiContextSetRequest>,
    close_requests: Vec<UiContextCloseRequest>,
    fixture_session_running: bool,
}

impl FixtureControl {
    fn new() -> Self {
        Self {
            set_requests: Vec::new(),
            close_requests: Vec::new(),
            fixture_session_running: true,
        }
    }
}

impl UiContextSink for FixtureControl {
    type Error = std::convert::Infallible;

    fn set_ui_context(
        &mut self,
        request: UiContextSetRequest,
    ) -> Result<UiContextSetResponse, Self::Error> {
        let response = UiContextSetResponse {
            schema_version: M1_PROTOCOL_VERSION,
            protocol_version: M1_PROTOCOL_VERSION,
            context: request.context.clone(),
            paths: paths(request.context.ui_id),
        };
        self.set_requests.push(request);
        Ok(response)
    }

    fn close_ui_context(
        &mut self,
        request: UiContextCloseRequest,
    ) -> Result<UiContextCloseResponse, Self::Error> {
        let response = UiContextCloseResponse {
            schema_version: M1_PROTOCOL_VERSION,
            protocol_version: M1_PROTOCOL_VERSION,
            ui_id: request.ui_id,
            closed: true,
            paths: paths(request.ui_id),
        };
        self.close_requests.push(request);
        Ok(response)
    }
}

#[test]
fn shell_start_and_detach_record_context_without_stopping_fixture_session() {
    let ui_id = UiId::new();
    let session_id = SessionId::new();
    let app = AppModel::daemon_console_fixture(ui_id, session_id, ["ready"].map(str::to_string));
    let mut shell = TuiShell::new(app);
    let mut control = FixtureControl::new();

    shell.start(&mut control).unwrap();
    assert_eq!(control.set_requests.len(), 1);
    assert_eq!(
        control.set_requests[0].events[0].kind,
        UiEventKind::UiStarted
    );
    assert_eq!(
        control.set_requests[0].context.active_daemon_session_id,
        Some(session_id)
    );

    assert_eq!(shell.detach(&mut control).unwrap(), ShellExit::Detached);
    assert!(control.fixture_session_running);
    assert!(control.close_requests.is_empty());
    assert_eq!(
        control.set_requests[1].events[0].kind,
        UiEventKind::UiDetached
    );
}

#[test]
fn shell_close_records_ui_closed_then_closes_context_only() {
    let ui_id = UiId::new();
    let app =
        AppModel::daemon_console_fixture(ui_id, SessionId::new(), ["ready"].map(str::to_string));
    let mut shell = TuiShell::new(app);
    let mut control = FixtureControl::new();

    assert_eq!(shell.close(&mut control).unwrap(), ShellExit::Closed);

    assert!(control.fixture_session_running);
    assert_eq!(control.set_requests.len(), 1);
    assert_eq!(
        control.set_requests[0].events[0].kind,
        UiEventKind::UiClosed
    );
    assert_eq!(control.close_requests.len(), 1);
    assert_eq!(control.close_requests[0].ui_id, ui_id);
}

fn paths(ui_id: UiId) -> UiContextPaths {
    let root = std::path::PathBuf::from(format!("views/{ui_id}"));
    UiContextPaths {
        context_json: root.join("context.json"),
        events_jsonl: root.join("events.jsonl"),
        root,
    }
}
