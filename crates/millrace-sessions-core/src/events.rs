use std::{collections::BTreeMap, path::Path};

use serde::{Deserialize, Serialize};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::{
    error::MillmuxResult,
    ids::SessionId,
    state::ProcessState,
    storage::{append_json_line, read_json_lines},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEventKind {
    SessionCreated,
    WorkerStarted,
    ProcessStarted,
    ProcessExited,
    Output,
    Input,
    InputSent,
    Resize,
    AttachOpened,
    AttachClosed,
    AttachStreamLagged,
    StateChanged,
    StopRequested,
    MillraceStatusProbe,
    MillraceStopRequested,
    MillraceStopFailed,
    KillRequested,
    Deleted,
    Archived,
    Purged,
    DoctorRepair,
    AttentionMarked,
    AttentionRead,
    AttentionCleared,
    StatusSummaryUpdated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEvent {
    pub timestamp: String,
    pub session_id: SessionId,
    pub kind: SessionEventKind,
    pub message: Option<String>,
    pub process_state: Option<ProcessState>,
    pub fields: BTreeMap<String, String>,
}

impl SessionEvent {
    pub fn new(session_id: SessionId, kind: SessionEventKind) -> Self {
        Self {
            timestamp: current_timestamp(),
            session_id,
            kind,
            message: None,
            process_state: None,
            fields: BTreeMap::new(),
        }
    }
}

pub fn current_timestamp() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

pub fn append_event(path: impl AsRef<Path>, event: &SessionEvent) -> MillmuxResult<()> {
    append_json_line(path, event)
}

pub fn read_events(path: impl AsRef<Path>) -> MillmuxResult<Vec<SessionEvent>> {
    read_json_lines(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_kind_uses_m1_wire_names() {
        assert_eq!(
            serde_json::to_string(&SessionEventKind::SessionCreated).unwrap(),
            "\"session_created\""
        );
        assert_eq!(
            serde_json::to_string(&SessionEventKind::DoctorRepair).unwrap(),
            "\"doctor_repair\""
        );
        assert_eq!(
            serde_json::to_string(&SessionEventKind::AttentionMarked).unwrap(),
            "\"attention_marked\""
        );
    }

    #[test]
    fn events_append_without_truncating() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("events.jsonl");
        let id = SessionId::new();
        append_event(
            &path,
            &SessionEvent::new(id, SessionEventKind::SessionCreated),
        )
        .unwrap();
        append_event(
            &path,
            &SessionEvent::new(id, SessionEventKind::WorkerStarted),
        )
        .unwrap();
        let events = read_events(&path).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, SessionEventKind::SessionCreated);
        assert_eq!(events[1].kind, SessionEventKind::WorkerStarted);
    }
}
