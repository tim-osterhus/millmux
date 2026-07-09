use crate::{client::SessionControlClient, commands::ContextExportArgs};

use std::{collections::BTreeSet, str::FromStr};

use millrace_sessions_core::{
    ids::SessionId,
    protocol::{
        ScreenFrame, SessionEventsRequest, SessionInspectRequest, SessionInspectResponse,
        SessionListRequest, SessionLogsRequest, SessionScreenRequest, SessionSelector,
        SessionSummary, UiContextGetResponse,
    },
    state::{AttentionItem, UiPaneViewKind},
    workspace::{GitWorktreeIdentity, WorkspaceIdentity},
};
use serde_json::{json, Value};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

const LOG_TAIL: usize = 8;
const EVENT_TAIL: usize = 12;
const SCREEN_LINE_LIMIT: usize = 6;

pub(crate) async fn build_context_export(
    client: &SessionControlClient,
    args: &ContextExportArgs,
) -> Result<Value, crate::MillmuxCliError> {
    let ui = client.ui_context_get(&args.get_request()?).await?;
    let source_session = resolve_handoff_session(
        client,
        "source",
        args.source_session.as_deref(),
        ui.context.focused_session_id,
    )
    .await?;
    let destination_session = resolve_handoff_session(
        client,
        "destination",
        args.destination_session.as_deref(),
        None,
    )
    .await?;
    let mut collection_issues = Vec::new();
    if ui.context.active_workspace.is_none() {
        collection_issues.push(json!({
            "source": "ui_context",
            "surface": "session.list",
            "warning": "UI context has no active workspace; export is limited to explicit UI session refs"
        }));
    }
    let mut session_ids = BTreeSet::new();
    session_ids.extend(ui.context.managed_session_ids.iter().copied());
    session_ids.extend(ui.context.managed_daemon_session_ids.iter().copied());
    session_ids.extend(ui.context.active_daemon_session_id);
    session_ids.extend(ui.context.agent_session_id);
    session_ids.extend(ui.context.selected_session_id);
    session_ids.extend(ui.context.focused_session_id);
    session_ids.extend(source_session.session_id);
    session_ids.extend(destination_session.session_id);

    let mut sessions = Vec::new();
    if !session_ids.is_empty() {
        for session_id in session_ids {
            match client
                .inspect(&SessionInspectRequest {
                    selector: SessionSelector::Id { session_id },
                })
                .await
            {
                Ok(inspect) => sessions.push(inspect.session),
                Err(error) => collection_issues.push(json!({
                    "source": "ui_context",
                    "session_id": session_id,
                    "surface": "session.inspect",
                    "error": error.to_string()
                })),
            }
        }
    } else if let Some(workspace_path) = ui
        .context
        .active_workspace
        .as_ref()
        .map(|workspace| workspace.canonical_path.clone())
    {
        collection_issues.push(json!({
            "source": "ui_context",
            "surface": "session.list",
            "warning": "UI context has no managed session ids; export fell back to active workspace"
        }));
        sessions = client
            .list(&SessionListRequest {
                role: None,
                workspace: Some(workspace_path),
                include_archived: false,
            })
            .await?
            .sessions;
    } else {
        collection_issues.push(json!({
            "source": "ui_context",
            "surface": "session.list",
            "error": "UI context has no active workspace or managed session ids"
        }));
    }
    let mut session_exports = Vec::new();
    let mut open_attention_items = Vec::new();

    for summary in sessions {
        let selector = SessionSelector::Id {
            session_id: summary.session_id,
        };
        let inspect = match client
            .inspect(&SessionInspectRequest {
                selector: selector.clone(),
            })
            .await
        {
            Ok(inspect) => inspect,
            Err(error) => {
                collection_issues.push(json!({
                    "source": "millmux_session",
                    "session_id": summary.session_id,
                    "surface": "session.inspect",
                    "error": error.to_string()
                }));
                session_exports.push(session_export_from_summary(client, summary, None).await);
                continue;
            }
        };

        open_attention_items.extend(
            inspect
                .attention_items
                .iter()
                .filter(|item| item.is_open())
                .cloned()
                .map(|item| attention_item_export(inspect.session.session_id, item)),
        );
        session_exports.push(
            session_export_from_summary(client, inspect.session.clone(), Some(inspect)).await,
        );
    }

    let active_daemon_session_id = ui.context.active_daemon_session_id;
    let daemon_monitor = match active_daemon_session_id {
        Some(session_id) => daemon_monitor_summary(client, session_id).await,
        None => unavailable("millrace_runtime", "no active daemon in UI context"),
    };

    Ok(json!({
        "schema_version": 1,
        "kind": "millmux_context_export",
        "generated_at": now_rfc3339(),
        "workspace": workspace_export(ui.context.active_workspace.as_ref()),
        "ui": ui_export(&ui),
        "handoff": {
            "source": "operator",
            "source_session_id": source_session.session_id,
            "source_session_selector": source_session.selector,
            "intended_destination_session_id": destination_session.session_id,
            "intended_destination_session_selector": destination_session.selector,
            "current_objective": objective_or_unavailable(args),
            "operator_note": note_or_unavailable(args),
            "last_approved_plan": unavailable("unavailable", "Millmux does not persist approved plan state")
        },
        "sessions": session_exports,
        "open_attention_items": open_attention_items,
        "daemon_monitor_summary": daemon_monitor,
        "artifact_references": {
            "source": "millmux_session",
            "sessions_root": "see per-session paths and artifacts",
            "ui_context": ui.paths.context_json,
            "ui_events": ui.paths.events_jsonl
        },
        "source_attribution": {
            "runtime": "millrace_runtime fields are included only when Millrace exposes them through status summaries or daemon output",
            "session": "millmux_session fields come from session metadata, worker metadata, and registry inspection",
            "terminal": "terminal_screen and terminal_log fields come from PTY/pipe logs, events, and screen snapshots",
            "operator": "operator fields come from explicit context export flags",
            "inferred": "inferred fields are derived from local git/workspace inspection",
            "unavailable": "unavailable fields are explicit when Millmux has no persisted source"
        },
        "collection_issues": collection_issues
    }))
}

struct HandoffSessionRef {
    session_id: Option<SessionId>,
    selector: Value,
}

async fn resolve_handoff_session(
    client: &SessionControlClient,
    label: &str,
    selector: Option<&str>,
    fallback: Option<SessionId>,
) -> Result<HandoffSessionRef, crate::MillmuxCliError> {
    let Some(selector) = selector else {
        return Ok(HandoffSessionRef {
            session_id: fallback,
            selector: unavailable("unavailable", "no operator selector supplied"),
        });
    };

    if let Ok(session_id) = SessionId::from_str(selector) {
        let inspect = client
            .inspect(&SessionInspectRequest {
                selector: SessionSelector::Id { session_id },
            })
            .await?;
        return Ok(HandoffSessionRef {
            session_id: Some(inspect.session.session_id),
            selector: json!({
                "source": "operator",
                "kind": "session_id",
                "value": selector
            }),
        });
    }

    let sessions = client
        .list(&SessionListRequest {
            role: None,
            workspace: None,
            include_archived: false,
        })
        .await?;
    let matches = sessions
        .sessions
        .iter()
        .filter(|session| session.name.as_deref() == Some(selector))
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [session] => Ok(HandoffSessionRef {
            session_id: Some(session.session_id),
            selector: json!({
                "source": "operator",
                "kind": "session_name",
                "value": selector
            }),
        }),
        [] => Err(crate::commands::CommandError::InvalidSelector(format!(
            "{label} session selector `{selector}` did not match an active session"
        ))
        .into()),
        _ => Err(crate::commands::CommandError::InvalidSelector(format!(
            "{label} session selector `{selector}` matched {} active sessions; use a session id",
            matches.len()
        ))
        .into()),
    }
}

async fn session_export_from_summary(
    client: &SessionControlClient,
    summary: SessionSummary,
    inspect: Option<SessionInspectResponse>,
) -> Value {
    let selector = SessionSelector::Id {
        session_id: summary.session_id,
    };
    let logs = bounded_logs(client, selector.clone()).await;
    let events = bounded_events(client, selector.clone()).await;
    let screen = bounded_screen(client, selector).await;
    let paths = inspect.as_ref().map(|inspect| inspect.paths.clone());
    let attention_items = inspect
        .as_ref()
        .map(|inspect| inspect.attention_items.clone())
        .unwrap_or_default();
    let worktree = GitWorktreeIdentity::discover(&summary.cwd);

    json!({
        "source": "millmux_session",
        "session_id": summary.session_id,
        "name": summary.name,
        "role": summary.role,
        "spawn_mode": summary.spawn_mode,
        "process_state": summary.process_state,
        "liveness": summary.liveness,
        "attention_state": summary.attention_state,
        "attention": summary.attention,
        "open_attention_items": attention_items
            .into_iter()
            .filter(|item| item.is_open())
            .map(|item| attention_item_export(summary.session_id, item))
            .collect::<Vec<_>>(),
        "status_summary": summary.status_summary,
        "cwd": summary.cwd,
        "workspace": summary.workspace.as_ref().map(|workspace| workspace_export(Some(workspace))),
        "worktree": worktree.map(|worktree| json!({
            "source": "inferred",
            "root": worktree.root,
            "branch": worktree.branch
        })),
        "argv": summary.argv,
        "monitor_profile": summary.monitor_profile,
        "created_at": summary.created_at,
        "updated_at": summary.updated_at,
        "attached_clients": summary.attached_clients,
        "input_owner": summary.input_owner,
        "read_only": !summary.capabilities.send || summary.input_owner.is_some(),
        "capabilities": summary.capabilities,
        "artifact_references": {
            "source": "millmux_session",
            "paths": paths,
            "artifacts": summary.artifacts
        },
        "bounded_log_summary": logs,
        "bounded_event_summary": events,
        "bounded_screen_summary": screen
    })
}

async fn daemon_monitor_summary(client: &SessionControlClient, session_id: SessionId) -> Value {
    let selector = SessionSelector::Id { session_id };
    let logs = bounded_logs(client, selector.clone()).await;
    let events = bounded_events(client, selector).await;
    json!({
        "source": "terminal_log",
        "session_id": session_id,
        "logs": logs,
        "events": events
    })
}

async fn bounded_logs(client: &SessionControlClient, selector: SessionSelector) -> Value {
    let session_id = selector_session_id(&selector);
    match client
        .logs(&SessionLogsRequest {
            selector,
            tail: Some(LOG_TAIL),
            follow: false,
        })
        .await
    {
        Ok(result) => json!({
            "source": "terminal_log",
            "session_id": result.session_id,
            "tail_limit": LOG_TAIL,
            "lines": result.lines
        }),
        Err(error) => json!({
            "source": "unavailable",
            "session_id": session_id,
            "tail_limit": LOG_TAIL,
            "error": error.to_string()
        }),
    }
}

async fn bounded_events(client: &SessionControlClient, selector: SessionSelector) -> Value {
    let session_id = selector_session_id(&selector);
    match client
        .events(&SessionEventsRequest {
            selector,
            tail: Some(EVENT_TAIL),
            follow: false,
        })
        .await
    {
        Ok(result) => {
            let returned_count = result.events.len();
            let events = result.events;
            let cursor = format!("events:{}:tail:{}", result.session_id, returned_count);
            json!({
                "source": "millmux_session",
                "session_id": result.session_id,
                "tail_limit": EVENT_TAIL,
                "returned_count": returned_count,
                "events": events,
                "event_stream": {
                    "source": "millmux_session",
                    "method": "events.subscribe",
                    "cursor": cursor
                }
            })
        }
        Err(error) => json!({
            "source": "unavailable",
            "session_id": session_id,
            "tail_limit": EVENT_TAIL,
            "error": error.to_string()
        }),
    }
}

async fn bounded_screen(client: &SessionControlClient, selector: SessionSelector) -> Value {
    let session_id = selector_session_id(&selector);
    match client
        .screen(&SessionScreenRequest {
            selector,
            requested_terminal_size: None,
        })
        .await
    {
        Ok(result) => match result.frame {
            ScreenFrame::ScreenSnapshot { snapshot } => {
                let lines = snapshot
                    .plain_lines()
                    .into_iter()
                    .take(SCREEN_LINE_LIMIT)
                    .collect::<Vec<_>>();
                json!({
                    "source": "terminal_screen",
                    "session_id": result.session_id,
                    "line_limit": SCREEN_LINE_LIMIT,
                    "rows": snapshot.rows,
                    "cols": snapshot.cols,
                    "captured_at": snapshot.captured_at,
                    "plain_lines": lines,
                    "screen_snapshot_ref": {
                        "source": "millmux_session",
                        "pty_log_offset": snapshot.source.pty_log_offset,
                        "raw_replay_start_offset": snapshot.source.raw_replay_start_offset,
                        "raw_replay_end_offset": snapshot.source.raw_replay_end_offset
                    }
                })
            }
            ScreenFrame::SnapshotUnavailable { reason, details } => json!({
                "source": "unavailable",
                "session_id": result.session_id,
                "reason": reason,
                "details": details
            }),
        },
        Err(error) => json!({
            "source": "unavailable",
            "session_id": session_id,
            "error": error.to_string()
        }),
    }
}

fn workspace_export(workspace: Option<&WorkspaceIdentity>) -> Value {
    match workspace {
        Some(workspace) => json!({
            "source": "millmux_session",
            "id": format!(
                "{}:{}",
                workspace.unix_device.unwrap_or_default(),
                workspace.unix_inode.unwrap_or_default()
            ),
            "path": workspace.canonical_path,
            "name": workspace
                .canonical_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("")
        }),
        None => unavailable("unavailable", "no active workspace in UI context"),
    }
}

fn ui_export(result: &UiContextGetResponse) -> Value {
    let context = &result.context;
    json!({
        "source": "ui_context",
        "ui_id": context.ui_id,
        "mode": context.mode,
        "context_path": result.paths.context_json,
        "events_path": result.paths.events_jsonl,
        "active_pane_id": context.active_pane_id,
        "selected_session_id": context.selected_session_id,
        "focused_session_id": context.focused_session_id,
        "focused_pane_kind": context.focused_pane_kind,
        "active_daemon_session_id": context.active_daemon_session_id,
        "agent_session_id": context.agent_session_id,
        "managed_session_ids": context.managed_session_ids,
        "managed_daemon_session_ids": context.managed_daemon_session_ids,
        "monitor_profile": context.monitor_profile,
        "updated_at": context
            .updated_at
            .format(&Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string()),
        "panes": context.panes.iter().map(|pane| {
            json!({
                "source": "ui_context",
                "id": pane.id,
                "title": pane.title,
                "view": pane.view,
                "focused": pane.focused,
                "stale": pane.stale,
                "read_only": pane.read_only || pane.stale || pane.overlay_active || pane.view.kind != UiPaneViewKind::SessionTerminal,
                "overlay_active": pane.overlay_active
            })
        }).collect::<Vec<_>>(),
        "daemon_health": context.daemon_health
    })
}

fn attention_item_export(session_id: SessionId, item: AttentionItem) -> Value {
    json!({
        "source": "millmux_session",
        "session_id": session_id,
        "item": item
    })
}

fn objective_or_unavailable(args: &ContextExportArgs) -> Value {
    args.objective.as_ref().map_or_else(
        || unavailable("unavailable", "no operator objective supplied"),
        |objective| {
            json!({
                "source": "operator",
                "value": objective
            })
        },
    )
}

fn note_or_unavailable(args: &ContextExportArgs) -> Value {
    args.note.as_ref().map_or_else(
        || unavailable("unavailable", "no operator note supplied"),
        |note| {
            json!({
                "source": "operator",
                "value": note
            })
        },
    )
}

fn unavailable(source: &str, reason: &str) -> Value {
    json!({
        "source": source,
        "available": false,
        "reason": reason
    })
}

fn selector_session_id(selector: &SessionSelector) -> Option<SessionId> {
    match selector {
        SessionSelector::Id { session_id } => Some(*session_id),
        _ => None,
    }
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}
