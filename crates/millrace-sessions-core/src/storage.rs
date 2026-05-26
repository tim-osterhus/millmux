use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use serde::{de::DeserializeOwned, Serialize};

use crate::error::{MillmuxError, MillmuxResult};

#[cfg(unix)]
pub const PRIVATE_DIR_MODE: u32 = 0o700;
#[cfg(unix)]
pub const PRIVATE_FILE_MODE: u32 = 0o600;

pub fn create_private_dir_all(path: impl AsRef<Path>) -> MillmuxResult<()> {
    let path = path.as_ref();
    fs::create_dir_all(path)?;
    harden_private_dir(path)?;
    Ok(())
}

pub fn write_json_atomic<T: Serialize>(path: impl AsRef<Path>, value: &T) -> MillmuxResult<()> {
    let path = path.as_ref();
    let parent = path
        .parent()
        .ok_or_else(|| MillmuxError::Storage(format!("missing parent for {}", path.display())))?;
    create_private_dir_all(parent)?;

    let temp_path = temp_path_for(path);
    let result = (|| -> MillmuxResult<()> {
        let mut file = create_private_file(&temp_path)?;
        serde_json::to_writer_pretty(&mut file, value)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp_path, path)?;
        harden_private_file(path)?;
        sync_dir(parent)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

pub fn read_json<T: DeserializeOwned>(path: impl AsRef<Path>) -> MillmuxResult<T> {
    let file = File::open(path)?;
    Ok(serde_json::from_reader(file)?)
}

pub fn append_json_line<T: Serialize>(path: impl AsRef<Path>, value: &T) -> MillmuxResult<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        create_private_dir_all(parent)?;
    }
    let mut file = open_private_append_file(path)?;
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    file.write_all(&line)?;
    file.sync_all()?;
    Ok(())
}

pub fn read_json_lines<T: DeserializeOwned>(path: impl AsRef<Path>) -> MillmuxResult<Vec<T>> {
    let file = File::open(path)?;
    let mut values = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        values.push(serde_json::from_str(&line)?);
    }
    Ok(values)
}

pub fn append_raw_pty_log(path: impl AsRef<Path>, bytes: &[u8]) -> MillmuxResult<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        create_private_dir_all(parent)?;
    }
    let mut file = open_private_append_file(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

pub fn write_private_bytes_atomic(path: impl AsRef<Path>, bytes: &[u8]) -> MillmuxResult<()> {
    let path = path.as_ref();
    let parent = path
        .parent()
        .ok_or_else(|| MillmuxError::Storage(format!("missing parent for {}", path.display())))?;
    create_private_dir_all(parent)?;

    let temp_path = temp_path_for(path);
    let result = (|| -> MillmuxResult<()> {
        let mut file = create_private_file(&temp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp_path, path)?;
        harden_private_file(path)?;
        sync_dir(parent)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn create_private_file(path: &Path) -> MillmuxResult<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(PRIVATE_FILE_MODE);
    }
    Ok(options.open(path)?)
}

fn open_private_append_file(path: &Path) -> MillmuxResult<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(PRIVATE_FILE_MODE);
    }
    let file = options.open(path)?;
    harden_private_file(path)?;
    Ok(file)
}

#[cfg(unix)]
pub fn harden_private_dir(path: impl AsRef<Path>) -> MillmuxResult<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIR_MODE))?;
    Ok(())
}

#[cfg(not(unix))]
pub fn harden_private_dir(_path: impl AsRef<Path>) -> MillmuxResult<()> {
    Ok(())
}

#[cfg(unix)]
pub fn harden_private_file(path: impl AsRef<Path>) -> MillmuxResult<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
    Ok(())
}

#[cfg(not(unix))]
pub fn harden_private_file(_path: impl AsRef<Path>) -> MillmuxResult<()> {
    Ok(())
}

fn temp_path_for(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("metadata");
    path.with_file_name(format!(
        ".{}.tmp.{}.{}",
        file_name,
        std::process::id(),
        uuid::Uuid::new_v4()
    ))
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> MillmuxResult<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> MillmuxResult<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct Sample {
        value: String,
    }

    #[test]
    fn storage_writes_and_reads_json_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("meta.json");
        write_json_atomic(
            &path,
            &Sample {
                value: "one".into(),
            },
        )
        .unwrap();
        write_json_atomic(
            &path,
            &Sample {
                value: "two".into(),
            },
        )
        .unwrap();
        assert_eq!(
            read_json::<Sample>(&path).unwrap(),
            Sample {
                value: "two".into()
            }
        );
    }

    #[test]
    fn storage_appends_json_lines_and_raw_logs() {
        let temp = tempfile::tempdir().unwrap();
        let events = temp.path().join("events.jsonl");
        append_json_line(&events, &Sample { value: "a".into() }).unwrap();
        append_json_line(&events, &Sample { value: "b".into() }).unwrap();
        let values: Vec<Sample> = read_json_lines(&events).unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].value, "a");
        assert_eq!(values[1].value, "b");

        let log = temp.path().join("pty.log");
        append_raw_pty_log(&log, b"hello").unwrap();
        append_raw_pty_log(&log, b" world").unwrap();
        assert_eq!(fs::read(&log).unwrap(), b"hello world");

        let ring = temp.path().join("pty.replay");
        write_private_bytes_atomic(&ring, &[0x00, 0xff, b'a']).unwrap();
        write_private_bytes_atomic(&ring, b"replacement").unwrap();
        assert_eq!(fs::read(&ring).unwrap(), b"replacement");
    }

    #[cfg(unix)]
    mod unix_private {
        use std::sync::Mutex;

        use nix::sys::stat::{umask, Mode};
        use std::os::unix::fs::PermissionsExt;

        use super::*;

        static UMASK_LOCK: Mutex<()> = Mutex::new(());

        #[test]
        fn storage_private_directory_uses_user_only_mode() {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("state").join("sessions").join("session");

            with_umask(0o000, || create_private_dir_all(&path).unwrap());

            assert_eq!(mode(&path), PRIVATE_DIR_MODE);
        }

        #[test]
        fn storage_private_json_atomic_uses_private_temp_and_final_modes() {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("meta.json");

            with_umask(0o000, || {
                write_json_atomic(
                    &path,
                    &Sample {
                        value: "one".into(),
                    },
                )
                .unwrap()
            });

            assert_eq!(mode(temp.path()), PRIVATE_DIR_MODE);
            assert_eq!(mode(&path), PRIVATE_FILE_MODE);
            assert_no_temp_files(temp.path(), "meta.json");
        }

        #[test]
        fn storage_private_json_atomic_removes_failed_temp_file() {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("meta.json");
            fs::create_dir(&path).unwrap();

            let result = with_umask(0o000, || {
                write_json_atomic(
                    &path,
                    &Sample {
                        value: "one".into(),
                    },
                )
            });

            assert!(result.is_err());
            assert_no_temp_files(temp.path(), "meta.json");
        }

        #[test]
        fn storage_private_append_files_keep_contents_and_user_only_modes() {
            let temp = tempfile::tempdir().unwrap();
            let events = temp.path().join("events.jsonl");
            let log = temp.path().join("pty.log");

            with_umask(0o000, || {
                append_json_line(&events, &Sample { value: "a".into() }).unwrap();
                append_json_line(&events, &Sample { value: "b".into() }).unwrap();
                append_raw_pty_log(&log, b"hello").unwrap();
                append_raw_pty_log(&log, b" world").unwrap();
                write_private_bytes_atomic(temp.path().join("pty.replay"), b"tail").unwrap();
            });

            assert_eq!(mode(&events), PRIVATE_FILE_MODE);
            assert_eq!(mode(&log), PRIVATE_FILE_MODE);
            assert_eq!(mode(&temp.path().join("pty.replay")), PRIVATE_FILE_MODE);
            assert_eq!(mode(temp.path()), PRIVATE_DIR_MODE);
            assert_eq!(
                read_json_lines::<Sample>(&events).unwrap(),
                vec![Sample { value: "a".into() }, Sample { value: "b".into() }]
            );
            assert_eq!(fs::read(&log).unwrap(), b"hello world");
        }

        fn with_umask<R>(mask: u32, operation: impl FnOnce() -> R) -> R {
            let _lock = UMASK_LOCK.lock().unwrap();
            let previous = umask(Mode::from_bits_truncate(mask as _));
            let _restore = UmaskRestore(previous);
            operation()
        }

        struct UmaskRestore(Mode);

        impl Drop for UmaskRestore {
            fn drop(&mut self) {
                let _ = umask(self.0);
            }
        }

        fn mode(path: &Path) -> u32 {
            fs::metadata(path).unwrap().permissions().mode() & 0o777
        }

        fn assert_no_temp_files(parent: &Path, target_name: &str) {
            let temp_prefix = format!(".{target_name}.tmp.");
            let leftovers = fs::read_dir(parent)
                .unwrap()
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with(&temp_prefix))
                .collect::<Vec<_>>();
            assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
        }
    }
}
