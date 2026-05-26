use std::{collections::BTreeMap, path::PathBuf};

use millrace_sessions_core::{
    ids::{SessionId, UiId},
    protocol::SessionSummary,
    state::{AttentionState, MonitorProfile, ProcessState, SessionRole},
    workspace::WorkspaceIdentity,
};
use millrace_sessions_tui::{
    renderer::render_to_string, AgentCockpitLayout, AgentTerminalPane, AppModel,
    DaemonConsoleLayout, TerminalEmulator,
};

fn fixture_app() -> AppModel {
    AppModel::daemon_console_fixture(
        UiId::new(),
        SessionId::new(),
        [
            "[00:00:01] runtime started",
            "[00:00:02] snapshot status execution=IDLE",
            "[00:00:03] idle reason=no_work",
        ]
        .map(str::to_string),
    )
}

fn snapshot_text(app: &AppModel, width: u16, height: u16) -> String {
    render_to_string(app, width, height)
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn foundation_layout_snapshot() {
    let snapshot = snapshot_text(&fixture_app(), 64, 8);
    insta::assert_snapshot!(snapshot, @r###"
Daemon Monitor | mon=auto | follow=live | selected
[00:00:01] runtime started
[00:00:02] snapshot status execution=IDLE
[00:00:03] idle reason=no_work



 mode=DaemonConsole monitor=Auto host=host connected input=ready
"###);
}

#[test]
fn scroll_mode_indicator_snapshot() {
    let mut app = fixture_app();
    app.enter_scroll_mode();
    app.line_log.scroll_up(3, 1);

    let snapshot = snapshot_text(&app, 64, 8);
    insta::assert_snapshot!(snapshot, @r###"
Daemon Monitor | mon=auto | follow=paused scroll | selected
[00:00:01] runtime started
[00:00:02] snapshot status execution=IDLE
[00:00:03] idle reason=no_work



 mode=DaemonConsole monitor=Auto host=host connected input=ready
"###);
}

#[test]
fn command_output_failure_snapshot() {
    let mut app = fixture_app();
    app.set_command_failure(
        vec!["millrace".to_string(), "status".to_string()],
        "/tmp/workspace",
        vec!["workspace not found".to_string()],
    );

    let snapshot = snapshot_text(&app, 72, 10);
    insta::assert_snapshot!(snapshot, @r###"
Daemon Monitor | mon=auto | follow=live | selected
[00:00:01] runtime started
[00:00:02] snapshot status execution=IDLE
[00:00:03] idle reason=no_work
Command Output | state=failed target=/tmp/workspace
argv: millrace status
stderr: workspace not found


 mode=DaemonConsole monitor=Auto host=host connected input=ready view=li
"###);
}

#[test]
fn host_reconnecting_snapshot() {
    let mut app = fixture_app();
    app.set_host_reconnecting(2, "session-control.sock unavailable");

    let snapshot = snapshot_text(&app, 72, 8);
    insta::assert_snapshot!(snapshot, @r###"
Host Reconnecting
host reconnecting attempt=2 session-control.sock unavailable
sessions remain hosted by SessionControl when available




 mode=DaemonConsole monitor=Auto host=host reconnecting attempt=2 sessio
"###);
}

#[test]
fn daemon_console_list_layout_snapshot() {
    let app = console_app(DaemonConsoleLayout::List);

    let snapshot = snapshot_text(&app, 96, 10);
    insta::assert_snapshot!(snapshot, @r###"
Daemon List | count=4             Daemon Monitor | mon=basic | follow=live | selected
  alpha running/idle m=basic      beta boot
  /tmp/alpha                      beta busy
> beta running/idle m=basic
  /tmp/beta
  gamma running/idle m=basic
  /tmp/gamma
  delta running/idle m=basic
  /tmp/delta
 mode=DaemonConsole monitor=Basic host=host connected input=ready view=live status=ready
"###);
}

#[test]
fn daemon_console_grid_layout_snapshot() {
    let app = console_app(DaemonConsoleLayout::Grid);

    let snapshot = snapshot_text(&app, 88, 12);
    insta::assert_snapshot!(snapshot, @r###"
beta | mon=basic | follow=live | selected   alpha | mon=basic | follow=live
beta boot                                   alpha boot
beta busy                                   alpha idle



gamma | mon=basic | follow=live             delta | mon=basic | follow=live
gamma boot                                  delta boot
gamma idle                                  delta idle


 mode=DaemonConsole monitor=Basic host=host connected input=ready view=live status=ready
"###);
}

#[test]
fn agent_cockpit_right_layout_snapshot() {
    let app = cockpit_app(AgentCockpitLayout::Right);

    let snapshot = snapshot_text(&app, 120, 12);
    insta::assert_snapshot!(snapshot, @r###"
Agent Terminal | input=owned screen=main cursor=1,2 | selected          Daemon Monitor | mon=basic | follow=live
Millracer operator ready                                                daemon ready
>                                                                       daemon idle








 mode=AgentCockpit monitor=Basic host=host connected input=ready view=live status=ready
"###);
}

#[test]
fn agent_cockpit_bottom_layout_snapshot() {
    let app = cockpit_app(AgentCockpitLayout::Bottom);

    let snapshot = snapshot_text(&app, 80, 12);
    insta::assert_snapshot!(snapshot, @r###"
Agent Terminal | input=owned screen=main cursor=1,2 | selected
Millracer operator ready
>




Daemon Monitor | mon=basic | follow=live
daemon ready
daemon idle

 mode=AgentCockpit monitor=Basic host=host connected input=ready view=live statu
"###);
}

#[test]
fn daemon_monitor_keeps_raw_output_for_future_monitor_profile() {
    let mut app = console_app(DaemonConsoleLayout::Single);
    let session_id = app.active_daemon_session_id.expect("selected daemon");
    app.monitor_profile = MonitorProfile::Other("future".to_string());
    if let Some(session) = app
        .daemon_sessions
        .iter_mut()
        .find(|session| session.session_id == session_id)
    {
        session.monitor_profile = MonitorProfile::Other("future".to_string());
    }
    app.replace_daemon_output(
        session_id,
        [
            "{not-json monitor frame",
            "future status frame still visible",
        ]
        .map(str::to_string),
    );

    let rendered = snapshot_text(&app, 100, 10);

    assert!(rendered.contains("mon=other:future"), "{rendered}");
    assert!(rendered.contains("{not-json monitor frame"), "{rendered}");
    assert!(
        rendered.contains("future status frame still visible"),
        "{rendered}"
    );
}

fn console_app(layout: DaemonConsoleLayout) -> AppModel {
    let alpha = summary("alpha");
    let beta = summary("beta");
    let gamma = summary("gamma");
    let delta = summary("delta");
    let selected = beta.session_id;
    let logs = BTreeMap::from([
        (
            alpha.session_id,
            vec!["alpha boot".to_string(), "alpha idle".to_string()],
        ),
        (
            beta.session_id,
            vec!["beta boot".to_string(), "beta busy".to_string()],
        ),
        (
            gamma.session_id,
            vec!["gamma boot".to_string(), "gamma idle".to_string()],
        ),
        (
            delta.session_id,
            vec!["delta boot".to_string(), "delta idle".to_string()],
        ),
    ]);
    AppModel::daemon_console(
        UiId::new(),
        vec![alpha, beta, gamma, delta],
        Some(selected),
        logs,
        layout,
        MonitorProfile::Basic,
    )
}

fn cockpit_app(layout: AgentCockpitLayout) -> AppModel {
    let daemon = summary("daemon");
    let agent = agent_summary("agent");
    let mut terminal = TerminalEmulator::new(6, 72, 100);
    terminal.process_text("Millracer operator ready\r\n> ");
    AppModel::agent_cockpit(
        UiId::new(),
        agent,
        vec![daemon.clone()],
        Some(daemon.session_id),
        BTreeMap::from([(
            daemon.session_id,
            vec!["daemon ready".to_string(), "daemon idle".to_string()],
        )]),
        AgentTerminalPane::with_snapshot(terminal.snapshot(), true, false),
        layout,
        MonitorProfile::Basic,
    )
}

fn summary(name: &str) -> SessionSummary {
    let cwd = PathBuf::from(format!("/tmp/{name}"));
    SessionSummary {
        session_id: SessionId::new(),
        name: Some(name.to_string()),
        role: SessionRole::MillraceDaemon,
        process_state: ProcessState::Running,
        attention_state: AttentionState::MillraceIdle,
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
        monitor_profile: MonitorProfile::Basic,
        created_at: "2026-05-26T00:00:00Z".to_string(),
        updated_at: "2026-05-26T00:00:01Z".to_string(),
        attached_clients: 0,
    }
}

fn agent_summary(name: &str) -> SessionSummary {
    let mut session = summary(name);
    session.role = SessionRole::Agent;
    session.argv = vec!["millracer".to_string(), "operator".to_string()];
    session.name = Some(name.to_string());
    session
}
