use std::{collections::BTreeMap, fmt};

use millrace_sessions_core::{
    ids::UiId,
    protocol::{
        UiContextCloseRequest, UiContextCloseResponse, UiContextSetRequest, UiContextSetResponse,
        M1_PROTOCOL_VERSION,
    },
    state::{UiContextPaths, UiEvent, UiEventKind},
};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::app::AppModel;

pub trait UiContextSink {
    type Error;

    fn set_ui_context(
        &mut self,
        request: UiContextSetRequest,
    ) -> Result<UiContextSetResponse, Self::Error>;

    fn close_ui_context(
        &mut self,
        request: UiContextCloseRequest,
    ) -> Result<UiContextCloseResponse, Self::Error>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiShell {
    pub app: AppModel,
    pub exit: Option<ShellExit>,
}

impl TuiShell {
    pub fn new(app: AppModel) -> Self {
        Self { app, exit: None }
    }

    pub fn start<C>(&mut self, control: &mut C) -> Result<(), ShellError<C::Error>>
    where
        C: UiContextSink,
    {
        self.set_context(control, UiEventKind::UiStarted, "ui started")
    }

    pub fn detach<C>(&mut self, control: &mut C) -> Result<ShellExit, ShellError<C::Error>>
    where
        C: UiContextSink,
    {
        self.set_context(control, UiEventKind::UiDetached, "ui detached")?;
        self.exit = Some(ShellExit::Detached);
        Ok(ShellExit::Detached)
    }

    pub fn close<C>(&mut self, control: &mut C) -> Result<ShellExit, ShellError<C::Error>>
    where
        C: UiContextSink,
    {
        self.set_context(control, UiEventKind::UiClosed, "ui closed")?;
        control
            .close_ui_context(UiContextCloseRequest {
                ui_id: self.app.ui_id,
            })
            .map_err(ShellError::Control)?;
        self.exit = Some(ShellExit::Closed);
        Ok(ShellExit::Closed)
    }

    fn set_context<C>(
        &self,
        control: &mut C,
        kind: UiEventKind,
        message: &'static str,
    ) -> Result<(), ShellError<C::Error>>
    where
        C: UiContextSink,
    {
        control
            .set_ui_context(UiContextSetRequest {
                context: self.app.ui_context(),
                events: vec![ui_event(self.app.ui_id, kind, message)],
            })
            .map(|_| ())
            .map_err(ShellError::Control)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellExit {
    Detached,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellError<E> {
    Control(E),
}

impl<E> fmt::Display for ShellError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Control(error) => write!(f, "SessionControl UI context update failed: {error}"),
        }
    }
}

impl<E> std::error::Error for ShellError<E> where E: std::error::Error + 'static {}

fn ui_event(ui_id: UiId, kind: UiEventKind, message: &'static str) -> UiEvent {
    UiEvent {
        timestamp: OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| "unknown".to_string()),
        ui_id,
        kind,
        message: Some(message.to_string()),
        fields: BTreeMap::new(),
    }
}

pub fn response_paths(ui_id: UiId) -> UiContextPaths {
    let root = std::path::PathBuf::from(format!("views/{ui_id}"));
    UiContextPaths {
        context_json: root.join("context.json"),
        events_jsonl: root.join("events.jsonl"),
        root,
    }
}

pub fn set_response(request: &UiContextSetRequest) -> UiContextSetResponse {
    UiContextSetResponse {
        schema_version: M1_PROTOCOL_VERSION,
        protocol_version: M1_PROTOCOL_VERSION,
        context: request.context.clone(),
        paths: response_paths(request.context.ui_id),
    }
}

pub fn close_response(ui_id: UiId) -> UiContextCloseResponse {
    UiContextCloseResponse {
        schema_version: M1_PROTOCOL_VERSION,
        protocol_version: M1_PROTOCOL_VERSION,
        ui_id,
        closed: true,
        paths: response_paths(ui_id),
    }
}
