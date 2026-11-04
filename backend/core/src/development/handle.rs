//! Local ownership handles that fence recovery and cleanup of one exact
//! transient development workload.
//!
//! A handle is a versioned JSON file recording the workload's identity, such
//! as its runtime scope, owner, generation, and launch fingerprint, plus its
//! workload, runtime process (container or native workload), and process IDs and recovery state. Custody is
//! exclusive and atomic, and fails closed on unsafe paths, symlinks, or broad
//! permissions, so a crashed development workload run is always matched to the
//! exact workload it created and never to another one.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use dotcore_proto::dotcore::telemetry::v1::WorkloadResourceInputs;
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

const HANDLE_FORMAT_VERSION: u32 = 3;
const MAX_HANDLE_BYTES: u64 = 64 * 1024;
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(10);
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    static FAIL_BEFORE_RENAME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_PARENT_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Versioned local custody record for one exact transient development workload.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DevelopmentOwnershipHandle {
    pub format_version: u32,
    pub state: DevelopmentOwnershipState,
    pub runtime_scope: String,
    pub runtime_backend_id: String,
    pub owner_id: String,
    pub generation: u64,
    pub launch_fingerprint: String,
    pub workload_id: Option<String>,
    pub runtime_process_id: Option<String>,
    pub process_id: Option<u32>,
    pub dirty_reason: Option<String>,
    /// Resolved launch inputs recorded with the workload's telemetry.
    /// Defaulted on read so an older handle reaches the version check instead of a parse error.
    #[serde(default)]
    pub launch_inputs: WorkloadResourceInputs,
}

impl DevelopmentOwnershipHandle {
    pub(super) fn prepared(
        runtime_scope: String,
        runtime_backend_id: String,
        owner_id: String,
        generation: u64,
        launch_fingerprint: String,
        launch_inputs: WorkloadResourceInputs,
    ) -> Self {
        Self {
            format_version: HANDLE_FORMAT_VERSION,
            state: DevelopmentOwnershipState::Prepared,
            runtime_scope,
            runtime_backend_id,
            owner_id,
            generation,
            launch_fingerprint,
            workload_id: None,
            runtime_process_id: None,
            process_id: None,
            dirty_reason: None,
            launch_inputs,
        }
    }

    pub(super) fn validate(&self) -> Result<(), OwnershipHandleError> {
        if self.format_version != HANDLE_FORMAT_VERSION {
            return Err(OwnershipHandleError::UnsupportedVersion(
                self.format_version,
            ));
        }
        if self.runtime_scope.is_empty()
            || self.runtime_backend_id.is_empty()
            || self.owner_id.is_empty()
            || self.runtime_scope.len() > 128
            || self.runtime_backend_id.len() > 128
            || self.owner_id.len() > 128
            || self.generation == 0
            || self.launch_fingerprint.len() != 64
            || !self
                .launch_fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(OwnershipHandleError::InvalidContents);
        }
        if self
            .workload_id
            .as_ref()
            .is_some_and(|value| value.len() > 512)
            || self
                .runtime_process_id
                .as_ref()
                .is_some_and(|value| value.len() > 512)
            || self
                .dirty_reason
                .as_ref()
                .is_some_and(|value| value.len() > 1024)
            || self.process_id == Some(0)
            || self.runtime_process_id.is_some() && self.workload_id.is_none()
            || self.process_id.is_some() && self.runtime_process_id.is_none()
            || matches!(
                self.state,
                DevelopmentOwnershipState::Starting
                    | DevelopmentOwnershipState::Started
                    | DevelopmentOwnershipState::Running
                    | DevelopmentOwnershipState::Stopping
                    | DevelopmentOwnershipState::Removing
            ) && (self.workload_id.is_none() || self.runtime_process_id.is_none())
            || self.state == DevelopmentOwnershipState::Running && self.process_id.is_none()
            || self.state == DevelopmentOwnershipState::Dirty && self.dirty_reason.is_none()
            || self.state != DevelopmentOwnershipState::Dirty && self.dirty_reason.is_some()
        {
            return Err(OwnershipHandleError::InvalidContents);
        }
        Ok(())
    }
}

/// Durable recovery phase for one development workload.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DevelopmentOwnershipState {
    Prepared,

    Creating,

    Created,

    Starting,

    Started,

    Running,

    Stopping,

    Removing,

    Dirty,

    Removed,
}

#[cfg(test)]
pub(super) fn fail_next_parent_sync() {
    FAIL_PARENT_SYNC.with(|failure| failure.set(true));
}

/// Host-wide scope custody acquired before any per-handle custody.
pub(super) struct ScopeLock {
    file: File,
}

impl ScopeLock {
    pub(super) async fn acquire(runtime_scope: &str) -> Result<Self, OwnershipHandleError> {
        let lock_path = scope_lock_path(runtime_scope);
        tokio::task::spawn_blocking(move || acquire_scope_lock(&lock_path))
            .await
            .map_err(|source| OwnershipHandleError::LockTask(source.to_string()))?
    }

    pub(super) async fn acquire_cancellable(
        runtime_scope: &str,
        cancellation: CancellationToken,
    ) -> Result<Self, OwnershipHandleError> {
        let lock_path = scope_lock_path(runtime_scope);
        tokio::task::spawn_blocking(move || {
            acquire_scope_lock_cancellable(&lock_path, &cancellation)
        })
        .await
        .map_err(|source| OwnershipHandleError::LockTask(source.to_string()))?
    }
}

impl Drop for ScopeLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

pub(super) struct OwnershipHandleFile {
    path: PathBuf,
    parent: PathBuf,
    lock: File,
    handle: DevelopmentOwnershipHandle,
}

impl OwnershipHandleFile {
    pub(super) fn create(
        path: &Path,
        handle: DevelopmentOwnershipHandle,
    ) -> Result<Self, OwnershipHandleError> {
        let (parent, lock) = lock_path(path)?;
        let replace = replaceable_existing_target(path, &handle)?;
        write_atomic(path, &parent, &handle, replace)?;
        Ok(Self {
            path: path.to_owned(),
            parent,
            lock,
            handle,
        })
    }

    pub(super) fn open(path: &Path) -> Result<Self, OwnershipHandleError> {
        let (parent, lock) = lock_path(path)?;
        let handle = read_handle(path)?;
        Ok(Self {
            path: path.to_owned(),
            parent,
            lock,
            handle,
        })
    }

    pub(super) fn handle(&self) -> &DevelopmentOwnershipHandle {
        &self.handle
    }

    pub(super) fn replace(
        &mut self,
        handle: DevelopmentOwnershipHandle,
    ) -> Result<(), OwnershipHandleError> {
        handle.validate()?;
        validate_handle_file(&self.path)?;
        if let Err(error) = write_atomic(&self.path, &self.parent, &handle, true) {
            if read_handle(&self.path).is_ok_and(|persisted| persisted == handle) {
                self.handle = handle;
            }
            return Err(error);
        }
        self.handle = handle;
        Ok(())
    }
}

impl Drop for OwnershipHandleFile {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.lock);
    }
}

/// Failures that make ownership custody unsafe or inconclusive.
#[derive(Debug, thiserror::Error)]
pub enum OwnershipHandleError {
    #[error("development ownership handle path must be an absolute normal path with a file name")]
    UnsafePath,

    #[error("development ownership handle path has an unsafe symlink or non-directory ancestor")]
    UnsafeAncestor,

    #[error(
        "development ownership handle file is unsafe or has permissions broader than owner-only"
    )]
    UnsafeFile,

    #[error("development ownership handle already exists")]
    AlreadyExists,

    #[error(
        "development ownership handle format version {0} is unsupported; tear the workload down with the dotcore build that launched it, or remove the handle and its container"
    )]
    UnsupportedVersion(u32),

    #[error("development ownership handle contents are incomplete or malformed")]
    InvalidContents,

    #[error("development ownership handle does not belong to this runtime namespace")]
    NamespaceMismatch,

    #[error("development ownership handle JSON is invalid: {0}")]
    Json(#[source] serde_json::Error),

    #[error("development scope custody was cancelled")]
    Cancelled,

    #[error("development scope lock task failed: {0}")]
    LockTask(String),

    #[error("development ownership handle {operation} failed for {path}: {source}")]
    Storage {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn scope_lock_path(runtime_scope: &str) -> PathBuf {
    Path::new("/tmp")
        .join("dotcore-development-workload-locks")
        .join(format!("{runtime_scope}.lock"))
}

fn acquire_scope_lock(path: &Path) -> Result<ScopeLock, OwnershipHandleError> {
    let file = open_scope_lock(path)?;
    file.lock_exclusive()
        .map_err(|source| scope_lock_error(path, source))?;
    Ok(ScopeLock { file })
}

fn acquire_scope_lock_cancellable(
    path: &Path,
    cancellation: &CancellationToken,
) -> Result<ScopeLock, OwnershipHandleError> {
    let file = open_scope_lock(path)?;
    loop {
        if cancellation.is_cancelled() {
            return Err(OwnershipHandleError::Cancelled);
        }
        match file.try_lock_exclusive() {
            Ok(true) => return Ok(ScopeLock { file }),
            Ok(false) => std::thread::sleep(LOCK_RETRY_INTERVAL),
            Err(error) => return Err(scope_lock_error(path, error)),
        }
    }
}

fn open_scope_lock(path: &Path) -> Result<File, OwnershipHandleError> {
    let parent = path.parent().ok_or(OwnershipHandleError::UnsafePath)?;
    fs::create_dir_all(parent).map_err(|source| OwnershipHandleError::Storage {
        operation: "create scope lock directory",
        path: parent.to_owned(),
        source,
    })?;
    reject_symlink_or_non_file_if_present(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let file = options
        .open(path)
        .map_err(|source| OwnershipHandleError::Storage {
            operation: "open scope lock",
            path: path.to_owned(),
            source,
        })?;
    validate_private_regular_file(
        path,
        &file
            .metadata()
            .map_err(|source| OwnershipHandleError::Storage {
                operation: "inspect scope lock",
                path: path.to_owned(),
                source,
            })?,
    )?;
    Ok(file)
}

fn scope_lock_error(path: &Path, source: io::Error) -> OwnershipHandleError {
    OwnershipHandleError::Storage {
        operation: "acquire scope lock",
        path: path.to_owned(),
        source,
    }
}

fn lock_path(path: &Path) -> Result<(PathBuf, File), OwnershipHandleError> {
    let parent = validate_path(path)?;
    let lock_path = PathBuf::from(format!("{}.lock", path.display()));
    reject_symlink_or_non_file_if_present(&lock_path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let lock = options
        .open(&lock_path)
        .map_err(|source| OwnershipHandleError::Storage {
            operation: "open lock",
            path: lock_path.clone(),
            source,
        })?;
    validate_private_regular_file(
        &lock_path,
        &lock
            .metadata()
            .map_err(|source| OwnershipHandleError::Storage {
                operation: "inspect lock",
                path: lock_path.clone(),
                source,
            })?,
    )?;
    lock.lock_exclusive()
        .map_err(|source| OwnershipHandleError::Storage {
            operation: "acquire lock",
            path: lock_path,
            source,
        })?;
    Ok((parent, lock))
}

fn validate_path(path: &Path) -> Result<PathBuf, OwnershipHandleError> {
    if !path.is_absolute()
        || path.file_name().is_none()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(OwnershipHandleError::UnsafePath);
    }
    let parent = path.parent().ok_or(OwnershipHandleError::UnsafePath)?;
    let mut current = PathBuf::new();
    for component in parent.components() {
        current.push(component);
        let metadata =
            fs::symlink_metadata(&current).map_err(|source| OwnershipHandleError::Storage {
                operation: "inspect ancestor",
                path: current.clone(),
                source,
            })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(OwnershipHandleError::UnsafeAncestor);
        }
    }
    Ok(parent.to_owned())
}

/// Returns whether `path` holds a removed handle of `handle`'s runtime namespace, which a new
/// handle may replace because it records no workload. Any other existing file fails as already
/// existing, so custody of a live, foreign or unreadable handle is never overwritten.
fn replaceable_existing_target(
    path: &Path,
    handle: &DevelopmentOwnershipHandle,
) -> Result<bool, OwnershipHandleError> {
    match fs::symlink_metadata(path) {
        Ok(_) => match read_handle(path) {
            Ok(existing)
                if existing.state == DevelopmentOwnershipState::Removed
                    && existing.runtime_scope == handle.runtime_scope
                    && existing.runtime_backend_id == handle.runtime_backend_id =>
            {
                Ok(true)
            }
            _ => Err(OwnershipHandleError::AlreadyExists),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(OwnershipHandleError::Storage {
            operation: "inspect",
            path: path.to_owned(),
            source,
        }),
    }
}

fn reject_existing_target(path: &Path) -> Result<(), OwnershipHandleError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(OwnershipHandleError::AlreadyExists),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(OwnershipHandleError::Storage {
            operation: "inspect",
            path: path.to_owned(),
            source,
        }),
    }
}

fn validate_handle_file(path: &Path) -> Result<(), OwnershipHandleError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| OwnershipHandleError::Storage {
        operation: "inspect",
        path: path.to_owned(),
        source,
    })?;
    validate_private_regular_file(path, &metadata)?;
    if metadata.len() > MAX_HANDLE_BYTES {
        return Err(OwnershipHandleError::InvalidContents);
    }
    Ok(())
}

fn read_handle(path: &Path) -> Result<DevelopmentOwnershipHandle, OwnershipHandleError> {
    validate_handle_file(path)?;
    let bytes = fs::read(path).map_err(|source| OwnershipHandleError::Storage {
        operation: "read",
        path: path.to_owned(),
        source,
    })?;
    let handle = serde_json::from_slice::<DevelopmentOwnershipHandle>(&bytes)
        .map_err(OwnershipHandleError::Json)?;
    handle.validate()?;
    Ok(handle)
}

fn reject_symlink_or_non_file_if_present(path: &Path) -> Result<(), OwnershipHandleError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_private_regular_file(path, &metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(OwnershipHandleError::Storage {
            operation: "inspect",
            path: path.to_owned(),
            source,
        }),
    }
}

fn validate_private_regular_file(
    _path: &Path,
    metadata: &fs::Metadata,
) -> Result<(), OwnershipHandleError> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(OwnershipHandleError::UnsafeFile);
    }
    if metadata.mode() & 0o077 != 0 || metadata.nlink() != 1 {
        return Err(OwnershipHandleError::UnsafeFile);
    }
    Ok(())
}

fn write_atomic(
    path: &Path,
    parent: &Path,
    handle: &DevelopmentOwnershipHandle,
    replace: bool,
) -> Result<(), OwnershipHandleError> {
    let bytes = serde_json::to_vec_pretty(handle).map_err(OwnershipHandleError::Json)?;
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .ok_or(OwnershipHandleError::UnsafePath)?,
        std::process::id(),
        TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let mut file = options
        .open(&temporary)
        .map_err(|source| OwnershipHandleError::Storage {
            operation: "create temporary file",
            path: temporary.clone(),
            source,
        })?;
    let result = (|| {
        file.write_all(&bytes)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| dotcore_domain::fs_sync::sync(&file))
            .map_err(|source| OwnershipHandleError::Storage {
                operation: "write temporary file",
                path: temporary.clone(),
                source,
            })?;
        if !replace {
            reject_existing_target(path)?;
        }
        #[cfg(test)]
        FAIL_BEFORE_RENAME.with(|failure| {
            if failure.replace(false) {
                return Err(OwnershipHandleError::Storage {
                    operation: "injected pre-rename",
                    path: path.to_owned(),
                    source: io::Error::other("injected ownership handle persistence failure"),
                });
            }
            Ok(())
        })?;
        fs::rename(&temporary, path).map_err(|source| OwnershipHandleError::Storage {
            operation: "replace",
            path: path.to_owned(),
            source,
        })?;
        #[cfg(test)]
        FAIL_PARENT_SYNC.with(|failure| {
            if failure.replace(false) {
                return Err(OwnershipHandleError::Storage {
                    operation: "sync parent directory",
                    path: parent.to_owned(),
                    source: io::Error::other("injected parent directory sync failure"),
                });
            }
            Ok(())
        })?;
        File::open(parent)
            .and_then(|directory| dotcore_domain::fs_sync::sync(&directory))
            .map_err(|source| OwnershipHandleError::Storage {
                operation: "sync parent directory",
                path: parent.to_owned(),
                source,
            })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    #[test]
    fn handle_create_and_each_replacement_are_exact_private_atomic_documents() {
        let root = test_root("atomic");
        fs::create_dir_all(&root).expect("test root");
        let path = root.join("owner.json");
        let mut transaction =
            OwnershipHandleFile::create(&path, handle()).expect("create ownership handle");
        let mut running = transaction.handle().clone();
        running.state = DevelopmentOwnershipState::Running;
        running.workload_id = Some("workload-1".to_owned());
        running.runtime_process_id = Some("container-1".to_owned());
        running.process_id = Some(42);
        transaction
            .replace(running.clone())
            .expect("replace handle");
        drop(transaction);

        let reopened = OwnershipHandleFile::open(&path).expect("open handle");
        assert_eq!(reopened.handle(), &running);
        assert_eq!(reopened.handle().format_version, HANDLE_FORMAT_VERSION);
        assert_eq!(fs::metadata(&path).expect("metadata").mode() & 0o777, 0o600);
        assert_eq!(
            fs::read_dir(&root)
                .expect("directory")
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
                .count(),
            0
        );
        drop(reopened);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn pre_rename_failure_preserves_the_complete_previous_handle() {
        let root = test_root("failed-transition");
        fs::create_dir_all(&root).expect("test root");
        let path = root.join("owner.json");
        let mut transaction = OwnershipHandleFile::create(&path, handle()).expect("create handle");
        let previous = transaction.handle().clone();
        let mut next = previous.clone();
        next.state = DevelopmentOwnershipState::Creating;
        FAIL_BEFORE_RENAME.with(|failure| failure.set(true));

        transaction
            .replace(next)
            .expect_err("injected persistence failure");

        assert_eq!(transaction.handle(), &previous);
        drop(transaction);
        let reopened = OwnershipHandleFile::open(&path).expect("reopen previous handle");
        assert_eq!(reopened.handle(), &previous);
        drop(reopened);
        assert_eq!(
            fs::read_dir(&root)
                .expect("directory")
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
                .count(),
            0
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn post_rename_sync_failure_reconciles_the_committed_handle_for_recovery() {
        let root = test_root("failed-parent-sync");
        fs::create_dir_all(&root).expect("test root");
        let path = root.join("owner.json");
        let mut transaction = OwnershipHandleFile::create(&path, handle()).expect("create handle");
        let mut next = transaction.handle().clone();
        next.state = DevelopmentOwnershipState::Creating;
        FAIL_PARENT_SYNC.with(|failure| failure.set(true));

        transaction
            .replace(next.clone())
            .expect_err("injected parent sync failure");

        assert_eq!(transaction.handle(), &next);
        drop(transaction);
        let reopened = OwnershipHandleFile::open(&path).expect("reopen committed handle");
        assert_eq!(reopened.handle(), &next);
        drop(reopened);
        assert_eq!(
            fs::read_dir(&root)
                .expect("directory")
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
                .count(),
            0
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn handle_paths_symlinks_permissions_versions_and_partial_identities_fail_closed() {
        let root = test_root("unsafe");
        fs::create_dir_all(&root).expect("test root");
        assert!(matches!(
            OwnershipHandleFile::create(Path::new("relative.json"), handle()),
            Err(OwnershipHandleError::UnsafePath)
        ));
        let path = root.join("owner.json");
        fs::write(&path, b"foreign").expect("foreign file");
        assert!(matches!(
            OwnershipHandleFile::create(&path, handle()),
            Err(OwnershipHandleError::AlreadyExists)
        ));
        fs::remove_file(&path).expect("remove foreign file");

        let mut invalid = handle();
        // A handle of the previous format, which named a container instead of a runtime process.
        invalid.format_version = 2;
        let bytes = serde_json::to_vec(&invalid).expect("JSON");
        fs::write(&path, bytes).expect("unsupported handle");
        set_private(&path);
        assert!(matches!(
            OwnershipHandleFile::open(&path),
            Err(OwnershipHandleError::UnsupportedVersion(2))
        ));
        let mut previous = serde_json::to_value(&invalid).expect("JSON value");
        previous["format_version"] = 1.into();
        previous
            .as_object_mut()
            .expect("handle object")
            .remove("launch_inputs");
        fs::write(&path, serde_json::to_vec(&previous).expect("JSON")).expect("previous handle");
        let Err(previous_error) = OwnershipHandleFile::open(&path) else {
            panic!("previous handle format must be rejected");
        };
        assert!(matches!(
            previous_error,
            OwnershipHandleError::UnsupportedVersion(1)
        ));
        assert!(
            previous_error
                .to_string()
                .contains("tear the workload down")
        );
        invalid.format_version = HANDLE_FORMAT_VERSION;
        invalid.runtime_process_id = Some("container".to_owned());
        fs::write(&path, serde_json::to_vec(&invalid).expect("JSON")).expect("invalid handle");
        assert!(matches!(
            OwnershipHandleFile::open(&path),
            Err(OwnershipHandleError::InvalidContents)
        ));

        invalid.runtime_process_id = None;
        fs::write(&path, serde_json::to_vec(&invalid).expect("JSON")).expect("broad handle");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("permissions");
        assert!(matches!(
            OwnershipHandleFile::open(&path),
            Err(OwnershipHandleError::UnsafeFile)
        ));
        fs::remove_file(&path).expect("remove broad handle");
        let outside = root.join("outside");
        fs::write(&outside, b"outside").expect("outside");
        symlink(&outside, &path).expect("symlink");
        assert!(matches!(
            OwnershipHandleFile::open(&path),
            Err(OwnershipHandleError::UnsafeFile)
        ));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn create_replaces_only_a_removed_handle_of_the_same_namespace() {
        let root = test_root("reuse");
        fs::create_dir_all(&root).expect("test root");
        let path = root.join("owner.json");
        let persist = |state, runtime_scope: &str| {
            let mut persisted = handle();
            persisted.state = state;
            persisted.runtime_scope = runtime_scope.to_owned();
            fs::write(&path, serde_json::to_vec(&persisted).expect("JSON")).expect("handle");
            set_private(&path);
            fs::read(&path).expect("persisted handle")
        };

        for state in [
            DevelopmentOwnershipState::Prepared,
            DevelopmentOwnershipState::Running,
            DevelopmentOwnershipState::Dirty,
        ] {
            let before = persist(state, "development-scope");
            assert!(matches!(
                OwnershipHandleFile::create(&path, handle()),
                Err(OwnershipHandleError::AlreadyExists)
            ));
            assert_eq!(fs::read(&path).expect("unchanged handle"), before);
        }
        let before = persist(DevelopmentOwnershipState::Removed, "other-scope");
        assert!(matches!(
            OwnershipHandleFile::create(&path, handle()),
            Err(OwnershipHandleError::AlreadyExists)
        ));
        assert_eq!(fs::read(&path).expect("unchanged handle"), before);

        persist(DevelopmentOwnershipState::Removed, "development-scope");
        let created = OwnershipHandleFile::create(&path, handle()).expect("replace removed handle");
        assert_eq!(created.handle(), &handle());
        drop(created);
        let reopened = OwnershipHandleFile::open(&path).expect("open handle");
        assert_eq!(reopened.handle().state, DevelopmentOwnershipState::Prepared);
        assert_eq!(reopened.handle(), &handle());
        fs::remove_dir_all(root).expect("cleanup");
    }

    fn handle() -> DevelopmentOwnershipHandle {
        DevelopmentOwnershipHandle::prepared(
            "development-scope".to_owned(),
            "runtime/docker".to_owned(),
            "runner-owner".to_owned(),
            7,
            "a".repeat(64),
            WorkloadResourceInputs {
                model_id: "model".to_owned(),
                ..WorkloadResourceInputs::default()
            },
        )
    }

    fn test_root(case: &str) -> PathBuf {
        dotcore_domain::test_support::canonical_temp_dir().join(format!(
            "dotcore-development-handle-{case}-{}-{}",
            std::process::id(),
            TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn set_private(path: &Path) {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("permissions");
    }
}
