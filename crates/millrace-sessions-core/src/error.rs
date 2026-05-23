use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum MillmuxError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("invalid protocol data: {0}")]
    InvalidProtocolData(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid session id: {0}")]
    InvalidSessionId(#[from] uuid::Error),
    #[error("duplicate millrace daemon for workspace: {0}")]
    DuplicateDaemon(String),
    #[error("invalid selector: {0}")]
    InvalidSelector(String),
    #[error("unsafe delete refused: {0}")]
    UnsafeDelete(String),
    #[error("worker unavailable: {0}")]
    WorkerUnavailable(String),
    #[error("workspace not found: {0}")]
    WorkspaceNotFound(PathBuf),
    #[error("workspace identity conflict: {0}")]
    WorkspaceIdentityConflict(String),
    #[error("command not found: {0}")]
    CommandNotFound(String),
    #[error("permission denied: {0}")]
    Permission(String),
    #[error("internal error: {0}")]
    Internal(String),
}

pub type MillmuxResult<T> = Result<T, MillmuxError>;
