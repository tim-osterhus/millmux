use std::{
    path::{Path, PathBuf},
    process::Command,
};

use serde::{Deserialize, Serialize};

use crate::error::{MillmuxError, MillmuxResult};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceIdentity {
    pub canonical_path: PathBuf,
    pub unix_device: Option<u64>,
    pub unix_inode: Option<u64>,
}

impl WorkspaceIdentity {
    pub fn capture(path: impl AsRef<Path>) -> MillmuxResult<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Err(MillmuxError::WorkspaceNotFound(path.to_path_buf()));
        }

        let canonical_path = path.canonicalize()?;
        let metadata = canonical_path.metadata()?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                canonical_path,
                unix_device: Some(metadata.dev()),
                unix_inode: Some(metadata.ino()),
            })
        }

        #[cfg(not(unix))]
        {
            let _ = metadata;
            Ok(Self {
                canonical_path,
                unix_device: None,
                unix_inode: None,
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitWorktreeIdentity {
    pub root: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

impl GitWorktreeIdentity {
    pub fn discover(path: impl AsRef<Path>) -> Option<Self> {
        let path = path.as_ref();
        let root = run_git(path, &["rev-parse", "--show-toplevel"])?;
        let branch = run_git(path, &["branch", "--show-current"]);
        Some(Self {
            root: PathBuf::from(root),
            branch,
        })
    }
}

fn run_git(path: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let value = text.trim();
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn workspace_rejects_missing_path() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        assert!(matches!(
            WorkspaceIdentity::capture(&missing),
            Err(MillmuxError::WorkspaceNotFound(path)) if path == missing
        ));
    }

    #[test]
    fn workspace_captures_canonical_identity() {
        let temp = tempfile::tempdir().unwrap();
        let identity = WorkspaceIdentity::capture(temp.path()).unwrap();
        assert_eq!(identity.canonical_path, temp.path().canonicalize().unwrap());
        #[cfg(unix)]
        {
            assert!(identity.unix_device.is_some());
            assert!(identity.unix_inode.is_some());
        }
    }

    #[test]
    fn git_worktree_discovery_returns_none_outside_git() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(GitWorktreeIdentity::discover(temp.path()), None);
    }

    #[test]
    fn git_worktree_discovery_finds_root_and_branch() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("file.txt"), "hello").unwrap();
        if Command::new("git")
            .arg("-C")
            .arg(temp.path())
            .arg("init")
            .arg("-b")
            .arg("main")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
        {
            let info = GitWorktreeIdentity::discover(temp.path()).unwrap();
            assert_eq!(info.root, temp.path());
            assert_eq!(info.branch.as_deref(), Some("main"));
        }
    }
}
