//! Shared single-file durability for the owner and node current-state stores.
//!
//! Each store keeps one protobuf snapshot in memory, holds an exclusive lock file beside it for
//! its whole lifetime, and replaces the file atomically on every committed change. A write whose
//! outcome is unknown poisons the store so no later read can expose state that may not be durable.

use std::{
    fs::{File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use fs4::fs_std::FileExt;

use crate::{
    atomic::{AtomicError, FileSystem, persist},
    store::{MAX_STATE_BYTES, StoreError, StoredCurrentState, validate_encoded_size},
};

/// One locked, file-backed snapshot shared by every clone of its store.
pub(super) struct StateFile<M> {
    /// Current-state lock: innermost after the node-control coordinator and site locks. No path
    /// takes the site lock or the coordinator while holding it.
    state: Arc<Mutex<FileState<M>>>,
    lock: Arc<CurrentStateFileLock>,
}

impl<M: StoredCurrentState> StateFile<M> {
    /// Wraps an already validated snapshot whose file custody `lock` holds.
    pub(super) fn new(
        state: M,
        path: PathBuf,
        filesystem: Arc<dyn FileSystem>,
        lock: CurrentStateFileLock,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(FileState {
                state,
                path,
                filesystem,
                poisoned: false,
            })),
            lock: Arc::new(lock),
        }
    }

    pub(super) fn path(&self) -> Result<PathBuf, StoreError> {
        Ok(self.lock_state()?.path.clone())
    }

    /// Reads the committed snapshot unless an uncertain write poisoned it.
    pub(super) fn read<T>(&self, read: impl FnOnce(&M) -> T) -> Result<T, StoreError> {
        let state = self.lock_state()?;
        if state.poisoned {
            return Err(StoreError::Poisoned);
        }
        Ok(read(&state.state))
    }

    /// Applies `operation` to a candidate copy, validates its envelope and size, and commits it
    /// only once the replacement file is durable. The operation validates the records it owns.
    pub(super) fn transact<T, E: From<StoreError>>(
        &self,
        operation: impl FnOnce(&mut M) -> Result<T, E>,
    ) -> Result<T, E> {
        let mut state = self.lock_state()?;
        if state.poisoned {
            return Err(StoreError::Poisoned.into());
        }
        let mut candidate = state.state.clone();
        let result = operation(&mut candidate)?;
        candidate.validate_stored()?;
        validate_encoded_size(&candidate)?;
        match persist(&*state.filesystem, &state.path, &candidate) {
            Ok(()) => {
                state.state = candidate;
                Ok(result)
            }
            Err(AtomicError::OutcomeUncertain(source)) => {
                state.poisoned = true;
                Err(StoreError::OutcomeUncertain(source).into())
            }
            Err(AtomicError::Storage(source)) => Err(StoreError::Storage(source).into()),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn state_pointer(&self) -> usize {
        Arc::as_ptr(&self.state) as usize
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(super) fn lock_file(&self) -> &CurrentStateFileLock {
        &self.lock
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, FileState<M>>, StoreError> {
        self.state.lock().map_err(|_| StoreError::Unavailable)
    }
}

impl<M> Clone for StateFile<M> {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
            lock: Arc::clone(&self.lock),
        }
    }
}

struct FileState<M> {
    state: M,
    path: PathBuf,
    filesystem: Arc<dyn FileSystem>,
    poisoned: bool,
}

/// Exclusive custody of one state file, held through a sibling `<file>.lock`.
pub(super) struct CurrentStateFileLock {
    pub(super) file: File,
    pub(super) path: PathBuf,
}

impl Drop for CurrentStateFileLock {
    fn drop(&mut self) {
        // Explicit unlock prevents a pre-exec child descriptor from extending parent custody.
        if let Err(source) = FileExt::unlock(&self.file) {
            tracing::error!(
                lock_path = %self.path.display(),
                error = %source,
                "could not explicitly release current-state custody"
            );
        }
    }
}

/// Acquires exclusive custody of the state file at `path` without waiting.
pub(super) fn acquire_lock(path: &Path) -> Result<CurrentStateFileLock, StoreError> {
    let lock_path = PathBuf::from(format!("{}.lock", path.display()));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|source| StoreError::storage("lock file open", lock_path.display(), source))?;
    if !file
        .try_lock_exclusive()
        .map_err(|source| StoreError::storage("exclusive lock", lock_path.display(), source))?
    {
        let source = anyhow::Error::new(io::Error::new(
            io::ErrorKind::WouldBlock,
            "current state is already locked",
        ))
        .context("current state is already locked");
        return Err(StoreError::Storage(source));
    }
    Ok(CurrentStateFileLock {
        file,
        path: lock_path,
    })
}

/// Decodes the snapshot stored at `path` without validating it; a missing file reads as `None`.
pub(super) fn read_stored<M: StoredCurrentState>(path: &Path) -> Result<Option<M>, StoreError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(StoreError::storage("open", path.display(), error)),
    };
    let length = file
        .metadata()
        .map_err(|source| StoreError::storage("metadata read", path.display(), source))?
        .len();
    if length > MAX_STATE_BYTES {
        return Err(StoreError::LimitExceeded);
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.read_to_end(&mut bytes)
        .map_err(|source| StoreError::storage("read", path.display(), source))?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(StoreError::LimitExceeded);
    }
    M::decode(bytes.as_slice())
        .map(Some)
        .map_err(StoreError::Decode)
}
