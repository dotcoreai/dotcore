//! Durable file writes shared by site storage: atomic replacement of a file, such as the
//! `state.pb` stores and `site.yaml`, and owner-only creation of files and directories that must
//! not exist yet, such as trust artifacts and node pins.
//!
//! A replacement either leaves the previous file or the complete new one, never a partial file;
//! filesystem operations are injectable so tests can fail each step.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::anyhow;
use prost::Message;

const TEMP_ATTEMPTS: usize = 32;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
pub trait FileSystem: Send + Sync {
    fn create_temp(&self, path: &Path) -> io::Result<(PathBuf, File)>;
    fn write_all(&self, file: &mut File, bytes: &[u8]) -> io::Result<()>;
    fn sync_file(&self, file: &File) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn sync_directory(&self, path: &Path) -> io::Result<()>;
}

pub struct StdFileSystem;

impl FileSystem for StdFileSystem {
    fn create_temp(&self, path: &Path) -> io::Result<(PathBuf, File)> {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("state path has no parent"))?;
        for _ in 0..TEMP_ATTEMPTS {
            let temp = parent.join(format!(
                ".{}.{}.{}.tmp",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("state.pb"),
                std::process::id(),
                TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            options.mode(0o600);
            match options.open(&temp) {
                Ok(file) => return Ok((temp, file)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create state temporary file",
        ))
    }

    fn write_all(&self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
        file.write_all(bytes)
    }

    fn sync_file(&self, file: &File) -> io::Result<()> {
        dotcore_domain::fs_sync::sync(file)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }

    fn sync_directory(&self, path: &Path) -> io::Result<()> {
        dotcore_domain::fs_sync::sync_directory(path)
    }
}

pub fn persist<M: Message>(
    filesystem: &dyn FileSystem,
    path: &Path,
    message: &M,
) -> Result<(), AtomicError> {
    persist_bytes(filesystem, path, &message.encode_to_vec())
}

/// Atomically replaces or creates `path` with `bytes` as an owner-only file: the bytes are synced
/// under a temporary name, renamed over `path`, and the parent directory is synced.
pub fn persist_bytes(
    filesystem: &dyn FileSystem,
    path: &Path,
    bytes: &[u8],
) -> Result<(), AtomicError> {
    let parent = state_parent(path)?;
    let (temp_path, mut file) = create_temporary_file(filesystem, path)?;
    if let Err(error) = write_temporary_file(filesystem, path, &mut file, bytes) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    if let Err(error) = sync_temporary_file(filesystem, path, &file) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    drop(file);
    if let Err(error) = replace_file(filesystem, path, &temp_path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    confirm_replacement(filesystem, parent).map_err(|source| {
        AtomicError::OutcomeUncertain(anyhow::Error::new(source).context(format!(
            "parent directory sync failed after replacement for {}",
            parent.display()
        )))
    })
}

/// Creates `path` as an owner-only file that must not exist yet, writes and syncs `bytes`, and
/// removes the file again when writing fails.
pub fn write_new_owner_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    if let Err(error) = file
        .write_all(bytes)
        .and_then(|_| dotcore_domain::fs_sync::sync(&file))
    {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(())
}

/// Creates `path` as an owner-only directory that must not exist yet.
pub fn create_owner_directory(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(path)
}

#[derive(Debug, thiserror::Error)]
pub enum AtomicError {
    #[error("Atomic persistence storage failed: {0}")]
    Storage(#[source] anyhow::Error),

    #[error("Atomic persistence outcome is uncertain: {0}")]
    OutcomeUncertain(#[source] anyhow::Error),
}

fn state_parent(path: &Path) -> Result<&Path, AtomicError> {
    path.parent()
        .ok_or_else(|| AtomicError::Storage(anyhow!("path has no parent: {}", path.display())))
}

fn create_temporary_file(
    filesystem: &dyn FileSystem,
    path: &Path,
) -> Result<(PathBuf, File), AtomicError> {
    filesystem.create_temp(path).map_err(|source| {
        AtomicError::Storage(anyhow::Error::new(source).context(format!(
            "temporary file create failed for {}",
            path.display()
        )))
    })
}

fn write_temporary_file(
    filesystem: &dyn FileSystem,
    path: &Path,
    file: &mut File,
    bytes: &[u8],
) -> Result<(), AtomicError> {
    filesystem.write_all(file, bytes).map_err(|source| {
        AtomicError::Storage(anyhow::Error::new(source).context(format!(
            "temporary file write failed for {}",
            path.display()
        )))
    })
}

fn sync_temporary_file(
    filesystem: &dyn FileSystem,
    path: &Path,
    file: &File,
) -> Result<(), AtomicError> {
    // The temporary contents must be durable before their name replaces the target.
    filesystem.sync_file(file).map_err(|source| {
        AtomicError::Storage(
            anyhow::Error::new(source)
                .context(format!("temporary file sync failed for {}", path.display())),
        )
    })
}

fn replace_file(
    filesystem: &dyn FileSystem,
    path: &Path,
    temporary: &Path,
) -> Result<(), AtomicError> {
    filesystem.rename(temporary, path).map_err(|source| {
        AtomicError::Storage(
            anyhow::Error::new(source)
                .context(format!("atomic rename failed for {}", path.display())),
        )
    })
}

fn confirm_replacement(filesystem: &dyn FileSystem, parent: &Path) -> std::io::Result<()> {
    // The renamed directory entry is durable only after syncing its parent directory.
    filesystem.sync_directory(parent)
}

#[cfg(test)]
mod tests {
    use std::{
        fs, io,
        path::{Path, PathBuf},
        sync::Mutex,
    };

    use anyhow::anyhow;
    use prost::Message;

    use super::{AtomicError, FileSystem, StdFileSystem, persist};

    #[test]
    fn atomic_errors_name_persistence_and_the_outcome_state() {
        let storage = AtomicError::Storage(anyhow!("write failed for /site/state.pb"));
        let uncertain = AtomicError::OutcomeUncertain(anyhow!(
            "directory sync failed for /site after replacement"
        ));

        assert_eq!(
            storage.to_string(),
            "Atomic persistence storage failed: write failed for /site/state.pb"
        );
        assert_eq!(
            uncertain.to_string(),
            "Atomic persistence outcome is uncertain: directory sync failed for /site after replacement"
        );
    }

    #[test]
    fn atomic_filesystem_leaves_retain_context_io_sources() {
        let directory = test_directory();
        let path = directory.join("state.pb");
        let message = TestMessage {
            payload: b"generic encoded state".to_vec(),
        };
        for (failure, outcome, context, diagnostic) in [
            (
                FailurePoint::Create,
                "Atomic persistence storage failed",
                "temporary file create failed",
                "injected temporary-file creation failure",
            ),
            (
                FailurePoint::Write,
                "Atomic persistence storage failed",
                "temporary file write failed",
                "injected temporary-file write failure",
            ),
            (
                FailurePoint::FileSync,
                "Atomic persistence storage failed",
                "temporary file sync failed",
                "injected temporary-file sync failure",
            ),
            (
                FailurePoint::Rename,
                "Atomic persistence storage failed",
                "atomic rename failed",
                "injected atomic rename failure",
            ),
            (
                FailurePoint::DirectorySync,
                "Atomic persistence outcome is uncertain",
                "parent directory sync failed after replacement",
                "injected parent directory sync failure",
            ),
        ] {
            let error = persist(&FailingFileSystem::new(failure), &path, &message)
                .expect_err("injected filesystem failure");
            let contextual_path = if matches!(failure, FailurePoint::DirectorySync) {
                directory.as_path()
            } else {
                path.as_path()
            };
            let expected_context = format!("{context} for {}", contextual_path.display());

            assert_eq!(error.to_string(), format!("{outcome}: {expected_context}"));
            assert!(!error.to_string().contains(diagnostic));
            let source = std::error::Error::source(&error).expect("filesystem context");
            assert_eq!(source.to_string(), expected_context);
            assert!(!source.to_string().contains(diagnostic));
            assert_eq!(
                source
                    .source()
                    .and_then(|source| source.downcast_ref::<io::Error>())
                    .map(ToString::to_string),
                Some(diagnostic.to_owned())
            );
        }

        fs::remove_dir_all(directory).expect("test directory cleanup");
    }

    #[test]
    fn atomic_persistence_syncs_contents_before_replacement_and_parent_metadata_afterwards() {
        let directory = test_directory();
        let path = directory.join("state.pb");
        let filesystem = RecordingFileSystem::new();

        let message = TestMessage {
            payload: b"generic encoded state".to_vec(),
        };
        persist(&filesystem, &path, &message).expect("atomic persistence");

        assert_eq!(
            *filesystem.operations.lock().expect("operation log"),
            ["create", "write", "sync file", "rename", "sync directory"]
        );
        assert_eq!(
            fs::read(&path).expect("published bytes"),
            b"\x0a\x15generic encoded state"
        );
        fs::remove_dir_all(directory).expect("fixture cleanup");
    }

    #[derive(Clone, PartialEq, Message)]
    struct TestMessage {
        #[prost(bytes, tag = "1")]
        payload: Vec<u8>,
    }

    #[derive(Clone, Copy)]
    enum FailurePoint {
        Create,

        Write,

        FileSync,

        Rename,

        DirectorySync,
    }

    struct FailingFileSystem {
        failure: FailurePoint,
        inner: StdFileSystem,
    }

    impl FailingFileSystem {
        fn new(failure: FailurePoint) -> Self {
            Self {
                failure,
                inner: StdFileSystem,
            }
        }
    }

    impl FileSystem for FailingFileSystem {
        fn create_temp(&self, path: &Path) -> io::Result<(PathBuf, fs::File)> {
            if matches!(self.failure, FailurePoint::Create) {
                return Err(io::Error::other("injected temporary-file creation failure"));
            }
            self.inner.create_temp(path)
        }

        fn write_all(&self, file: &mut fs::File, bytes: &[u8]) -> io::Result<()> {
            if matches!(self.failure, FailurePoint::Write) {
                return Err(io::Error::other("injected temporary-file write failure"));
            }
            self.inner.write_all(file, bytes)
        }

        fn sync_file(&self, file: &fs::File) -> io::Result<()> {
            if matches!(self.failure, FailurePoint::FileSync) {
                return Err(io::Error::other("injected temporary-file sync failure"));
            }
            self.inner.sync_file(file)
        }

        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            if matches!(self.failure, FailurePoint::Rename) {
                return Err(io::Error::other("injected atomic rename failure"));
            }
            self.inner.rename(from, to)
        }

        fn sync_directory(&self, path: &Path) -> io::Result<()> {
            if matches!(self.failure, FailurePoint::DirectorySync) {
                return Err(io::Error::other("injected parent directory sync failure"));
            }
            self.inner.sync_directory(path)
        }
    }

    struct RecordingFileSystem {
        inner: StdFileSystem,
        operations: Mutex<Vec<&'static str>>,
    }

    impl RecordingFileSystem {
        fn new() -> Self {
            Self {
                inner: StdFileSystem,
                operations: Mutex::new(Vec::new()),
            }
        }
    }

    impl FileSystem for RecordingFileSystem {
        fn create_temp(&self, path: &Path) -> io::Result<(PathBuf, fs::File)> {
            self.operations
                .lock()
                .expect("operation log")
                .push("create");
            self.inner.create_temp(path)
        }

        fn write_all(&self, file: &mut fs::File, bytes: &[u8]) -> io::Result<()> {
            self.operations.lock().expect("operation log").push("write");
            self.inner.write_all(file, bytes)
        }

        fn sync_file(&self, file: &fs::File) -> io::Result<()> {
            self.operations
                .lock()
                .expect("operation log")
                .push("sync file");
            self.inner.sync_file(file)
        }

        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.operations
                .lock()
                .expect("operation log")
                .push("rename");
            self.inner.rename(from, to)
        }

        fn sync_directory(&self, path: &Path) -> io::Result<()> {
            self.operations
                .lock()
                .expect("operation log")
                .push("sync directory");
            self.inner.sync_directory(path)
        }
    }

    fn test_directory() -> PathBuf {
        let path = dotcore_domain::test_support::canonical_temp_dir().join(format!(
            "dotcore-atomic-persistence-{}",
            dotcore_domain::test_support::unique_suffix()
        ));
        fs::create_dir(&path).expect("fixture directory");
        path
    }
}
