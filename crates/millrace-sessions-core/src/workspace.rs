use std::path::{Path, PathBuf};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
