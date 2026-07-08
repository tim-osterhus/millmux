use std::{env, path::PathBuf};

use directories::BaseDirs;

use crate::{
    error::MillmuxResult,
    ids::{SessionId, UiId},
    state::{SessionPaths, UiContextPaths},
};

pub const STATE_DIR_ENV: &str = "MILLMUX_STATE_DIR";
pub const UI_ID_ENV: &str = "MILLMUX_UI_ID";
pub const CONTROL_SOCK_ENV: &str = "MILLMUX_CONTROL_SOCK";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatePaths {
    pub root: PathBuf,
    pub host_lock: PathBuf,
    pub host_json: PathBuf,
    pub control_sock: PathBuf,
    pub sessions_dir: PathBuf,
    pub archive_dir: PathBuf,
    pub views_dir: PathBuf,
}

impl StatePaths {
    pub fn new(root: PathBuf) -> Self {
        Self {
            host_lock: root.join("host.lock"),
            host_json: root.join("host.json"),
            control_sock: root.join("session-control.sock"),
            sessions_dir: root.join("sessions"),
            archive_dir: root.join("archive"),
            views_dir: root.join("views"),
            root,
        }
    }

    pub fn session_paths(&self, session_id: SessionId) -> SessionPaths {
        let mut paths = session_paths(&self.sessions_dir, session_id);
        paths.worker_sock = self
            .root
            .join("w")
            .join(format!("{}.sock", short_session_id(session_id)));
        paths
    }

    pub fn ui_context_paths(&self, ui_id: UiId) -> UiContextPaths {
        let root = self.views_dir.join(ui_id.to_string());
        UiContextPaths {
            context_json: root.join("context.json"),
            events_jsonl: root.join("events.jsonl"),
            root,
        }
    }
}

pub fn state_root() -> MillmuxResult<PathBuf> {
    if let Some(value) = env::var_os(STATE_DIR_ENV) {
        return Ok(PathBuf::from(value));
    }
    default_state_root()
}

pub fn state_paths() -> MillmuxResult<StatePaths> {
    Ok(StatePaths::new(state_root()?))
}

pub fn default_state_root() -> MillmuxResult<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let base = BaseDirs::new().ok_or_else(|| {
            crate::error::MillmuxError::Internal("home directory not found".into())
        })?;
        Ok(base
            .home_dir()
            .join("Library")
            .join("Application Support")
            .join("millmux"))
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(value) = env::var_os("XDG_STATE_HOME") {
            Ok(PathBuf::from(value).join("millmux"))
        } else {
            let base = BaseDirs::new().ok_or_else(|| {
                crate::error::MillmuxError::Internal("home directory not found".into())
            })?;
            Ok(base.home_dir().join(".local").join("state").join("millmux"))
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let base = BaseDirs::new().ok_or_else(|| {
            crate::error::MillmuxError::Internal("home directory not found".into())
        })?;
        Ok(base.home_dir().join(".millmux"))
    }
}

pub fn session_paths(sessions_dir: impl Into<PathBuf>, session_id: SessionId) -> SessionPaths {
    let root = sessions_dir.into().join(session_id.to_string());
    SessionPaths {
        meta_json: root.join("meta.json"),
        worker_json: root.join("worker.json"),
        pty_log: root.join("pty.log"),
        stdout_log: root.join("stdout.log"),
        stderr_log: root.join("stderr.log"),
        events_jsonl: root.join("events.jsonl"),
        scrollback_snapshot: root.join("scrollback.snapshot"),
        terminal_snapshot: root.join("terminal.snapshot.json"),
        raw_replay_ring: root.join("pty.replay"),
        worker_sock: root.join("worker.sock"),
        root,
    }
}

fn short_session_id(session_id: SessionId) -> String {
    session_id.to_string().chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_use_state_dir_override() {
        let temp = tempfile::tempdir().unwrap();
        env::set_var(STATE_DIR_ENV, temp.path());
        assert_eq!(state_root().unwrap(), temp.path());
        env::remove_var(STATE_DIR_ENV);
    }

    #[test]
    fn paths_build_expected_layout() {
        let id = SessionId::new();
        let paths = StatePaths::new(PathBuf::from("/state"));
        assert_eq!(paths.host_lock, PathBuf::from("/state/host.lock"));
        assert_eq!(paths.host_json, PathBuf::from("/state/host.json"));
        assert_eq!(
            paths.control_sock,
            PathBuf::from("/state/session-control.sock")
        );
        assert_eq!(paths.sessions_dir, PathBuf::from("/state/sessions"));
        assert_eq!(paths.archive_dir, PathBuf::from("/state/archive"));
        assert_eq!(paths.views_dir, PathBuf::from("/state/views"));

        let session = paths.session_paths(id);
        let root = PathBuf::from("/state/sessions").join(id.to_string());
        assert_eq!(session.root, root);
        assert_eq!(session.meta_json, session.root.join("meta.json"));
        assert_eq!(session.worker_json, session.root.join("worker.json"));
        assert_eq!(session.pty_log, session.root.join("pty.log"));
        assert_eq!(session.stdout_log, session.root.join("stdout.log"));
        assert_eq!(session.stderr_log, session.root.join("stderr.log"));
        assert_eq!(session.events_jsonl, session.root.join("events.jsonl"));
        assert_eq!(
            session.scrollback_snapshot,
            session.root.join("scrollback.snapshot")
        );
        assert_eq!(
            session.terminal_snapshot,
            session.root.join("terminal.snapshot.json")
        );
        assert_eq!(session.raw_replay_ring, session.root.join("pty.replay"));
        assert_eq!(
            session.worker_sock,
            PathBuf::from("/state/w").join(format!("{}.sock", short_session_id(id)))
        );

        let ui_id = UiId::new();
        let ui = paths.ui_context_paths(ui_id);
        assert_eq!(
            ui.root,
            PathBuf::from("/state/views").join(ui_id.to_string())
        );
        assert_eq!(ui.context_json, ui.root.join("context.json"));
        assert_eq!(ui.events_jsonl, ui.root.join("events.jsonl"));
    }

    #[test]
    fn default_state_root_has_platform_shape() {
        let root = default_state_root().unwrap();
        #[cfg(target_os = "macos")]
        assert!(root.ends_with("Library/Application Support/millmux"));
        #[cfg(target_os = "linux")]
        assert!(root.ends_with("millmux"));
    }
}
