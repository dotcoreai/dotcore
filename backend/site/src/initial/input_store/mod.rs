//! Guarded offline creation and replacement of initial site input files.

use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use anyhow::anyhow;
use dotcore_proto::dotcore::site::v1::{Site, SiteNode};
use fs4::fs_std::FileExt;

use crate::{
    NODE_FILE, SITE_FILE,
    initial::{
        inputs::{InitialSiteInputLoadError, InitialSiteInputs},
        request::InitialSiteRequest,
        validation::{SiteInputBundleValidator, SiteInputValidationError},
    },
    manager_trust::{ManagerTrust, SITE_TRUST_DIRECTORY, WORKER_PIN_DIRECTORY},
    node::observation_configuration::node_observation_configuration,
    repository::SiteRepositoryError,
    validation::decode_site,
};

static TRANSACTION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// How often a waiting acquisition retries the site input lock.
// Not a behavior duration: poll of a file lock another process may hold.
const SITE_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Longest time a writer waits for another writer's transaction to release the lock.
// Not a behavior duration: bound on a wait for a file lock another process holds.
const SITE_LOCK_WRITER_WAIT: Duration = Duration::from_secs(30);

/// Holds an advisory sibling site-input lock until dropped.
#[derive(Debug)]
pub struct SiteInputLock {
    file: File,
}

impl Drop for SiteInputLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Failure while acquiring the advisory site-input lock.
#[derive(Debug, thiserror::Error)]
pub enum SiteInputLockError {
    #[error("site input lock failed: {0}")]
    Storage(#[source] anyhow::Error),

    /// A site writer still held the lock when the bounded wait ended.
    #[error("site input lock {} still held after {waited:?}; another site command is writing the site", path.display())]
    Held {
        /// Contended lock file.
        path: PathBuf,
        /// How long acquisition waited.
        waited: Duration,
    },
}

/// Acquires the shared lock held by a running backend for its entire lifetime.
/// Site writers hold the exclusive lock only for one short transaction, so this waits at most
/// `wait` for one to finish and then fails instead of blocking startup indefinitely.
pub fn acquire_shared_site_lock(
    site_root: &Path,
    wait: Duration,
) -> Result<SiteInputLock, SiteInputLockError> {
    let (file, lock_path) = open_site_lock(site_root, false)?;
    let deadline = Instant::now() + wait;
    loop {
        let acquired = FileExt::try_lock_shared(&file)
            .map_err(|error| lock_error(&lock_path, "acquire", error))?;
        if acquired {
            return Ok(SiteInputLock { file });
        }

        let now = Instant::now();
        if now >= deadline {
            return Err(SiteInputLockError::Held {
                path: lock_path,
                waited: wait,
            });
        }
        thread::sleep(SITE_LOCK_POLL_INTERVAL.min(deadline - now));
    }
}

pub fn try_acquire_exclusive_site_lock(
    site_root: &Path,
) -> Result<Option<SiteInputLock>, SiteInputLockError> {
    let (file, lock_path) = open_site_lock(site_root, true)?;
    let acquired = file
        .try_lock_exclusive()
        .map_err(|error| lock_error(&lock_path, "acquire", error))?;
    Ok(acquired.then_some(SiteInputLock { file }))
}

/// Takes the exclusive lock for one writer transaction. A running backend holds its shared lock
/// until it stops, so a running site is refused at once; another writer holds the lock only for
/// one short transaction, so this waits a bounded time for it.
fn acquire_exclusive_site_lock(site_root: &Path) -> Result<SiteInputLock, InitialSiteWriteError> {
    let (file, lock_path) = open_site_lock(site_root, true)?;
    let deadline = Instant::now() + SITE_LOCK_WRITER_WAIT;
    loop {
        let acquired = file
            .try_lock_exclusive()
            .map_err(|error| lock_error(&lock_path, "acquire", error))?;
        if acquired {
            return Ok(SiteInputLock { file });
        }

        // Only a running backend holds the lock shared, so a successful shared probe means the
        // holder is not another writer.
        let shared = FileExt::try_lock_shared(&file)
            .map_err(|error| lock_error(&lock_path, "probe", error))?;
        if shared {
            FileExt::unlock(&file).map_err(|error| lock_error(&lock_path, "release", error))?;
            return Err(InitialSiteWriteError::SiteRunning);
        }

        let now = Instant::now();
        if now >= deadline {
            return Err(SiteInputLockError::Held {
                path: lock_path,
                waited: SITE_LOCK_WRITER_WAIT,
            }
            .into());
        }
        thread::sleep(SITE_LOCK_POLL_INTERVAL.min(deadline - now));
    }
}

fn ensure_existing_site_lock_available(site_root: &Path) -> Result<(), InitialSiteWriteError> {
    let lock_path = PathBuf::from(format!("{}.lock", site_root.display()));
    reject_symlink(&lock_path).map_err(InitialSiteWriteError::Storage)?;
    let file = match OpenOptions::new().read(true).write(true).open(&lock_path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(storage_error("open site input lock", &lock_path, error)),
    };
    let acquired = file
        .try_lock_exclusive()
        .map_err(|error| InitialSiteWriteError::Lock(lock_error(&lock_path, "acquire", error)))?;
    if !acquired {
        return Err(InitialSiteWriteError::SiteRunning);
    }
    FileExt::unlock(&file)
        .map_err(|error| InitialSiteWriteError::Lock(lock_error(&lock_path, "release", error)))
}

fn open_site_lock(
    site_root: &Path,
    create_missing_parent: bool,
) -> Result<(File, PathBuf), SiteInputLockError> {
    prepare_lock_parent(site_root, create_missing_parent).map_err(SiteInputLockError::Storage)?;
    let lock_path = PathBuf::from(format!("{}.lock", site_root.display()));
    reject_symlink(&lock_path).map_err(SiteInputLockError::Storage)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    options.custom_flags(libc::O_NOFOLLOW);
    let file = options
        .open(&lock_path)
        .map_err(|error| lock_error(&lock_path, "open", error))?;
    Ok((file, lock_path))
}

trait DirectoryTransactionFileSystem: Send + Sync {
    fn create_directory(&self, path: &Path) -> io::Result<()>;
    fn create_file(&self, path: &Path) -> io::Result<File>;
    fn write_all(&self, file: &mut File, contents: &[u8]) -> io::Result<()>;
    fn sync_file(&self, file: &File) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove_directory_all(&self, path: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn sync_directory(&self, path: &Path) -> io::Result<()>;
    fn same_filesystem(&self, target: &Path, parent: &Path) -> io::Result<bool>;
}

struct StdDirectoryTransactionFileSystem;

impl DirectoryTransactionFileSystem for StdDirectoryTransactionFileSystem {
    fn create_directory(&self, path: &Path) -> io::Result<()> {
        fs::create_dir(path)
    }

    fn create_file(&self, path: &Path) -> io::Result<File> {
        OpenOptions::new().create_new(true).write(true).open(path)
    }

    fn write_all(&self, file: &mut File, contents: &[u8]) -> io::Result<()> {
        file.write_all(contents)
    }

    fn sync_file(&self, file: &File) -> io::Result<()> {
        dotcore_domain::fs_sync::sync(file)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }

    fn remove_directory_all(&self, path: &Path) -> io::Result<()> {
        fs::remove_dir_all(path)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }

    fn sync_directory(&self, path: &Path) -> io::Result<()> {
        dotcore_domain::fs_sync::sync_directory(path)
    }

    fn same_filesystem(&self, target: &Path, parent: &Path) -> io::Result<bool> {
        Ok(fs::metadata(target)?.dev() == fs::metadata(parent)?.dev())
    }
}

/// Guarded offline writer for the two initializer-owned site input files.
///
/// Replacement is failure-safe but not atomic: it preserves a sibling backup
/// when a completed swap cannot be confirmed or cleaned up.
#[derive(Clone)]
pub struct InitialSiteInputWriter {
    validator: SiteInputBundleValidator,
    filesystem: Arc<dyn DirectoryTransactionFileSystem>,
}

impl InitialSiteInputWriter {
    /// Creates a writer that runs the given complete site input validator before an input bundle
    /// becomes visible.
    pub fn new(validator: SiteInputBundleValidator) -> Self {
        Self {
            validator,
            filesystem: Arc::new(StdDirectoryTransactionFileSystem),
        }
    }

    #[cfg(test)]
    fn with_filesystem(mut self, filesystem: Arc<dyn DirectoryTransactionFileSystem>) -> Self {
        self.filesystem = filesystem;
        self
    }

    /// Inspects whether a site is absent or already valid without mutating filesystem state.
    pub fn inspect_if_missing(
        &self,
        site_root: &Path,
    ) -> Result<InitialSitePresence, InitialSiteWriteError> {
        self.reject_recoverable_backup(site_root)?;
        match fs::symlink_metadata(site_root) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                Err(InitialSiteWriteError::Symlink {
                    path: site_root.to_owned(),
                })
            }
            Ok(_) => {
                self.reject_input_symlinks(site_root)?;
                ensure_existing_site_lock_available(site_root)?;
                self.validator
                    .validate_without_runtime_declarations(site_root)
                    .map_err(InitialSiteWriteError::ExistingInputsInvalid)?;
                Ok(InitialSitePresence::Initialized)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Ok(InitialSitePresence::Missing)
            }
            Err(error) => Err(storage_error("inspect site target", site_root, error)),
        }
    }

    /// Inspects and publishes one initialization request under one exclusive transaction.
    /// Replacement rewrites the inputs in place and keeps site trust, so it refuses a site with
    /// deployment declarations or retained state; removing those is a site reset.
    pub fn initialize(
        &self,
        site_root: &Path,
        inputs: &InitialSiteInputs,
        request: &InitialSiteRequest,
        replace: bool,
    ) -> Result<InitialSiteWriteOutcome, InitialSiteWriteError> {
        let _lock = acquire_exclusive_site_lock(site_root)?;
        self.reject_recoverable_backup(site_root)?;
        match fs::symlink_metadata(site_root) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                Err(InitialSiteWriteError::Symlink {
                    path: site_root.to_owned(),
                })
            }
            Ok(_) => {
                self.reject_input_symlinks(site_root)?;
                let has_declarations = existing_runtime_declarations(site_root)?;
                if has_declarations && !replace {
                    return Err(InitialSiteWriteError::DeploymentDeclarations);
                }
                if has_declarations {
                    self.validator
                        .validate_without_runtime_declarations(site_root)
                        .map_err(InitialSiteWriteError::ExistingInputsInvalid)?;
                } else {
                    self.validator
                        .validate(site_root)
                        .map_err(InitialSiteWriteError::ExistingInputsInvalid)?;
                }
                let existing =
                    InitialSiteInputs::load(site_root, &self.validator.declaration_rules())
                        .map_err(InitialSiteWriteError::ExistingInputsUnreadable)?;
                ManagerTrust::load(site_root, existing.site()).map_err(|error| {
                    InitialSiteWriteError::ExistingTrustInvalid(anyhow::Error::new(error))
                })?;
                if !replace && existing.matches_requested_intent(request, inputs) {
                    return Ok(InitialSiteWriteOutcome::AlreadyInitialized(existing));
                }
                if !replace {
                    return Err(InitialSiteWriteError::IntentConflict);
                }
                if has_declarations || has_retained_state(site_root) {
                    return Err(InitialSiteWriteError::RetainedStateResetRequired);
                }
                reject_provisioned_worker_topology_change(
                    site_root,
                    existing.site(),
                    inputs.site(),
                )?;
                ManagerTrust::load(site_root, inputs.site()).map_err(|error| {
                    InitialSiteWriteError::ExistingTrustInvalid(anyhow::Error::new(error))
                })?;
                self.replace_locked(site_root, inputs)?;
                Ok(InitialSiteWriteOutcome::Published)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.create_locked(site_root, inputs)?;
                Ok(InitialSiteWriteOutcome::Published)
            }
            Err(error) => Err(storage_error("inspect site target", site_root, error)),
        }
    }

    /// Atomically creates a previously absent site input directory.
    pub fn create(
        &self,
        site_root: &Path,
        inputs: &InitialSiteInputs,
    ) -> Result<(), InitialSiteWriteError> {
        let _lock = acquire_exclusive_site_lock(site_root)?;
        self.create_while_locked(site_root, inputs)
    }

    pub(crate) fn create_under_reset_lock(
        &self,
        site_root: &Path,
        inputs: &InitialSiteInputs,
        _lock: &crate::reset::transaction::SiteResetLock,
    ) -> Result<(), InitialSiteWriteError> {
        self.create_while_locked(site_root, inputs)
    }

    fn create_while_locked(
        &self,
        site_root: &Path,
        inputs: &InitialSiteInputs,
    ) -> Result<(), InitialSiteWriteError> {
        self.reject_recoverable_backup(site_root)?;
        match fs::symlink_metadata(site_root) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(InitialSiteWriteError::Symlink {
                    path: site_root.to_owned(),
                });
            }
            Ok(_) => return Err(InitialSiteWriteError::TargetExists),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(storage_error("inspect site target", site_root, error)),
        }
        self.create_locked(site_root, inputs)
    }

    fn create_locked(
        &self,
        site_root: &Path,
        inputs: &InitialSiteInputs,
    ) -> Result<(), InitialSiteWriteError> {
        let staging = self.stage_inputs(site_root, inputs, true)?;
        if let Err(error) = self.filesystem.rename(&staging, site_root) {
            let _ = self.filesystem.remove_directory_all(&staging);
            return Err(storage_error(
                "publish initial site inputs",
                site_root,
                error,
            ));
        }
        self.filesystem
            .sync_directory(parent_directory(site_root)?)
            .map_err(|error| outcome_error("confirm initial site input creation", site_root, error))
    }

    /// Guardedly replaces only a complete initializer-owned offline site input directory.
    pub fn replace(
        &self,
        site_root: &Path,
        inputs: &InitialSiteInputs,
    ) -> Result<(), InitialSiteWriteError> {
        let _lock = acquire_exclusive_site_lock(site_root)?;
        self.reject_recoverable_backup(site_root)?;
        self.reject_input_symlinks(site_root)?;
        if existing_runtime_declarations(site_root)? {
            return Err(InitialSiteWriteError::DeploymentDeclarations);
        }
        let existing = InitialSiteInputs::load(site_root, &self.validator.declaration_rules())
            .map_err(InitialSiteWriteError::ExistingInputsUnreadable)?;
        ManagerTrust::load(site_root, existing.site()).map_err(|error| {
            InitialSiteWriteError::ExistingTrustInvalid(anyhow::Error::new(error))
        })?;
        reject_provisioned_worker_topology_change(site_root, existing.site(), inputs.site())?;
        self.replace_locked(site_root, inputs)
    }

    fn replace_locked(
        &self,
        site_root: &Path,
        inputs: &InitialSiteInputs,
    ) -> Result<(), InitialSiteWriteError> {
        ManagerTrust::load(site_root, inputs.site()).map_err(|error| {
            InitialSiteWriteError::ExistingTrustInvalid(anyhow::Error::new(error))
        })?;
        self.require_owned_target(site_root)?;
        let staging = self.stage_inputs(site_root, inputs, false)?;
        let backup_site = transaction_path(site_root, "init-backup-site")?;
        let backup_node = transaction_path(site_root, "init-backup-node")?;
        for (name, backup) in [(SITE_FILE, &backup_site), (NODE_FILE, &backup_node)] {
            if let Err(error) = self.filesystem.rename(&site_root.join(name), backup) {
                if name == NODE_FILE {
                    let _ = self
                        .filesystem
                        .rename(&backup_site, &site_root.join(SITE_FILE));
                }
                let _ = self.filesystem.remove_directory_all(&staging);
                return Err(storage_error(
                    "create guarded site input backup",
                    backup,
                    error,
                ));
            }
        }
        let result = (|| {
            self.filesystem
                .rename(&staging.join(SITE_FILE), &site_root.join(SITE_FILE))
                .map_err(|error| storage_error("publish guarded site input", site_root, error))?;
            self.filesystem
                .rename(&staging.join(NODE_FILE), &site_root.join(NODE_FILE))
                .map_err(|error| storage_error("publish guarded site input", site_root, error))?;
            self.filesystem
                .sync_directory(parent_directory(site_root)?)
                .map_err(|error| {
                    outcome_error("confirm guarded site input replacement", site_root, error)
                })
        })();
        if let Err(error) = result {
            let _ = self.filesystem.remove_file(&site_root.join(SITE_FILE));
            let _ = self.filesystem.remove_file(&site_root.join(NODE_FILE));
            let _ = self
                .filesystem
                .rename(&backup_site, &site_root.join(SITE_FILE));
            let _ = self
                .filesystem
                .rename(&backup_node, &site_root.join(NODE_FILE));
            let _ = self.filesystem.remove_directory_all(&staging);
            return Err(error);
        }
        let _ = self.filesystem.remove_directory_all(&staging);
        self.filesystem.remove_file(&backup_site).map_err(|error| {
            InitialSiteWriteError::RecoverableBackup {
                path: backup_site,
                source: anyhow::Error::new(error),
            }
        })?;
        self.filesystem.remove_file(&backup_node).map_err(|error| {
            InitialSiteWriteError::RecoverableBackup {
                path: backup_node,
                source: anyhow::Error::new(error),
            }
        })
    }

    fn stage_inputs(
        &self,
        site_root: &Path,
        inputs: &InitialSiteInputs,
        with_trust: bool,
    ) -> Result<PathBuf, InitialSiteWriteError> {
        let staging = staging_path(site_root)?;
        self.filesystem
            .create_directory(&staging)
            .map_err(|error| {
                storage_error("create initial site staging directory", &staging, error)
            })?;
        let result = (|| {
            self.write_file(
                &staging.join(SITE_FILE),
                &inputs.site_yaml().map_err(|error| {
                    InitialSiteWriteError::InvalidInputs(anyhow::Error::new(error))
                })?,
            )?;
            self.write_file(
                &staging.join(NODE_FILE),
                &inputs.node_yaml().map_err(|error| {
                    InitialSiteWriteError::InvalidInputs(anyhow::Error::new(error))
                })?,
            )?;
            if with_trust {
                ManagerTrust::create(&staging, inputs.site()).map_err(|error| {
                    InitialSiteWriteError::Storage(
                        anyhow::Error::new(error).context("stage site manager trust"),
                    )
                })?;
            }
            self.validator
                .validate(&staging)
                .map_err(|error| InitialSiteWriteError::InvalidInputs(anyhow::Error::new(error)))?;
            self.filesystem.sync_directory(&staging).map_err(|error| {
                storage_error("sync initial site staging directory", &staging, error)
            })
        })();
        if result.is_err() {
            let _ = self.filesystem.remove_directory_all(&staging);
        }
        result.map(|_| staging)
    }

    fn write_file(&self, path: &Path, contents: &str) -> Result<(), InitialSiteWriteError> {
        let mut file = self
            .filesystem
            .create_file(path)
            .map_err(|error| storage_error("create initial site input", path, error))?;
        self.filesystem
            .write_all(&mut file, contents.as_bytes())
            .and_then(|_| self.filesystem.sync_file(&file))
            .map_err(|error| storage_error("write initial site input", path, error))
    }

    fn reject_input_symlinks(&self, site_root: &Path) -> Result<(), InitialSiteWriteError> {
        for name in [SITE_FILE, NODE_FILE] {
            let path = site_root.join(name);
            if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
                return Err(InitialSiteWriteError::Symlink { path });
            }
        }
        Ok(())
    }

    fn require_owned_target(&self, site_root: &Path) -> Result<(), InitialSiteWriteError> {
        let metadata = fs::symlink_metadata(site_root)
            .map_err(|error| storage_error("inspect site replacement target", site_root, error))?;
        if metadata.file_type().is_symlink() {
            return Err(InitialSiteWriteError::Symlink {
                path: site_root.to_owned(),
            });
        }
        if !metadata.is_dir() {
            return Err(InitialSiteWriteError::UnexpectedEntry {
                path: site_root.to_owned(),
            });
        }
        let parent = parent_directory(site_root)?;
        if !self
            .filesystem
            .same_filesystem(site_root, parent)
            .map_err(|error| {
                storage_error("inspect site replacement filesystem", site_root, error)
            })?
        {
            return Err(InitialSiteWriteError::CrossFilesystem);
        }
        let mut entries = BTreeSet::new();
        for entry in fs::read_dir(site_root)
            .map_err(|error| storage_error("read site replacement target", site_root, error))?
        {
            let entry = entry
                .map_err(|error| storage_error("read site replacement entry", site_root, error))?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| storage_error("inspect site replacement entry", &path, error))?;
            if metadata.file_type().is_symlink() {
                return Err(InitialSiteWriteError::Symlink { path });
            }
            if name == "catalog" {
                if !metadata.is_dir() {
                    return Err(InitialSiteWriteError::UnexpectedEntry { path });
                }
                continue;
            }
            if name == SITE_TRUST_DIRECTORY {
                if !metadata.is_dir() {
                    return Err(InitialSiteWriteError::UnexpectedEntry { path });
                }
                continue;
            }
            if name == "state.pb" {
                if !metadata.is_file() {
                    return Err(InitialSiteWriteError::UnexpectedEntry { path });
                }
                continue;
            }
            if !metadata.is_file() || !matches!(name.as_str(), SITE_FILE | NODE_FILE) {
                return Err(InitialSiteWriteError::UnexpectedEntry { path });
            }
            entries.insert(name);
        }
        let required = BTreeSet::from([SITE_FILE.to_owned(), NODE_FILE.to_owned()]);
        if entries.is_superset(&required)
            && entries
                .difference(&required)
                .all(|name| name == "catalog" || name == "state.pb")
        {
            Ok(())
        } else {
            Err(InitialSiteWriteError::UnexpectedEntry {
                path: site_root.to_owned(),
            })
        }
    }

    fn reject_recoverable_backup(&self, site_root: &Path) -> Result<(), InitialSiteWriteError> {
        let parent = parent_directory(site_root)?;
        let prefix = backup_prefix(site_root)?;
        let entries = match fs::read_dir(parent) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(storage_error("scan site backup directory", parent, error)),
        };
        for entry in entries {
            let entry =
                entry.map_err(|error| storage_error("scan site backup entry", parent, error))?;
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                return Err(InitialSiteWriteError::RecoverableBackup {
                    path: entry.path(),
                    source: anyhow!("guarded replacement did not complete"),
                });
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn rollback_backup(
        &self,
        site_root: &Path,
        backup: &Path,
        replacement_error: std::io::Error,
    ) -> Result<(), InitialSiteWriteError> {
        match self.filesystem.rename(backup, site_root) {
            Ok(()) => {
                self.filesystem
                    .sync_directory(parent_directory(site_root)?)
                    .map_err(|error| {
                        outcome_error("confirm guarded site input rollback", site_root, error)
                    })?;
                Err(storage_error(
                    "publish guarded site input replacement",
                    site_root,
                    replacement_error,
                ))
            }
            Err(rollback_error) => Err(InitialSiteWriteError::RecoverableBackup {
                path: backup.to_owned(),
                source: anyhow::Error::new(rollback_error).context(format!(
                    "replacement failed: {replacement_error}; backup restoration failed"
                )),
            }),
        }
    }
}

fn reject_provisioned_worker_topology_change(
    site_root: &Path,
    existing: &Site,
    replacement: &Site,
) -> Result<(), InitialSiteWriteError> {
    let pins = site_root
        .join(SITE_TRUST_DIRECTORY)
        .join(WORKER_PIN_DIRECTORY);
    let entries = fs::read_dir(&pins)
        .map_err(|error| storage_error("read provisioned worker pins", &pins, error))?;
    let mut worker_ids = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|error| storage_error("read provisioned worker pin", &pins, error))?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            return Err(InitialSiteWriteError::ProvisionedWorkerTopology);
        };
        let Some(worker_id) = file_name.strip_suffix(".sha256") else {
            continue;
        };
        worker_ids.push(worker_id.to_owned());
    }
    if worker_ids.is_empty() {
        return Ok(());
    }

    let existing_manager = existing
        .nodes
        .iter()
        .find(|node| node.site_manager.is_some())
        .map(|node| (&node.node_id, &node.host));
    let replacement_manager = replacement
        .nodes
        .iter()
        .find(|node| node.site_manager.is_some())
        .map(|node| (&node.node_id, &node.host));
    if existing_manager != replacement_manager {
        return Err(InitialSiteWriteError::ProvisionedWorkerTopology);
    }

    for worker_id in worker_ids {
        let existing_node = existing.nodes.iter().find(|node| node.node_id == worker_id);
        let replacement_node = replacement
            .nodes
            .iter()
            .find(|node| node.node_id == worker_id);
        if existing_node.map(node_topology) != replacement_node.map(node_topology) {
            return Err(InitialSiteWriteError::ProvisionedWorkerTopology);
        }
        let existing_configuration = node_observation_configuration(existing, &worker_id)
            .map_err(|_| InitialSiteWriteError::ProvisionedWorkerTopology)?;
        let replacement_configuration = node_observation_configuration(replacement, &worker_id)
            .map_err(|_| InitialSiteWriteError::ProvisionedWorkerTopology)?;
        if existing_configuration != replacement_configuration {
            return Err(InitialSiteWriteError::ProvisionedWorkerTopology);
        }
    }
    Ok(())
}

/// The membership facts of a pinned node that a guarded replacement must keep.
fn node_topology(node: &SiteNode) -> (&str, &str, &str, bool) {
    (
        &node.node_id,
        &node.host,
        &node.manager_host,
        node.site_manager.is_some(),
    )
}

/// Failure while creating or guardedly replacing initial site inputs.
#[derive(Debug, thiserror::Error)]
pub enum InitialSiteWriteError {
    /// Creation leaves an existing site target unchanged.
    #[error("initial site target already exists")]
    TargetExists,

    /// Existing inputs failed complete validation under the transaction lock.
    #[error("existing site inputs are invalid: {0}")]
    ExistingInputsInvalid(#[source] SiteInputValidationError),

    /// Existing inputs could not be loaded for intent and ownership checks.
    #[error("existing site inputs could not be inspected: {0}")]
    ExistingInputsUnreadable(#[source] InitialSiteInputLoadError),

    /// Existing site manager credentials are missing or do not match its identity.
    #[error("existing site manager trust is invalid: {0}")]
    ExistingTrustInvalid(#[source] anyhow::Error),

    /// Existing site YAML could not be decoded for replacement ownership checks.
    #[error("existing site deployment declarations could not be inspected: {0}")]
    DeploymentInspection(#[source] SiteRepositoryError),

    /// Existing valid inputs do not match the requested initialization intent.
    #[error(
        "existing site inputs conflict with requested initialization; request their replacement"
    )]
    IntentConflict,

    /// Runtime-owned desired declarations make generic replacement unsafe.
    #[error(
        "guarded site input replacement refuses runtime-owned deployment or inference route declarations"
    )]
    DeploymentDeclarations,

    /// Deployment declarations or state a backend run left behind can only be removed by a site
    /// reset.
    #[error("retained site state requires explicit reset confirmation")]
    RetainedStateResetRequired,

    /// Provisioned worker topology can only be replaced by resetting site trust.
    #[error("provisioned worker topology requires explicit site reset")]
    ProvisionedWorkerTopology,

    /// Generated inputs failed complete validation before they could become visible.
    #[error("generated initial site inputs are invalid: {0}")]
    InvalidInputs(#[source] anyhow::Error),

    /// A replacement target contains the non-owned site-local catalog.
    #[error("guarded site input replacement refuses an existing non-owned catalog/")]
    NonOwnedCatalog,

    /// A target or entry is a symbolic link.
    #[error("guarded site input operation refuses symbolic link {path}")]
    Symlink {
        /// Rejected symbolic-link path.
        path: PathBuf,
    },

    /// A target contains unknown, retained, generated, or non-owned content.
    #[error("guarded site input replacement refuses unexpected entry {path}")]
    UnexpectedEntry {
        /// Rejected path.
        path: PathBuf,
    },

    /// Replacement target and its sibling transaction directory differ in filesystem device.
    #[error("guarded site input replacement requires one filesystem")]
    CrossFilesystem,

    /// A previous guarded replacement retained a backup requiring explicit recovery.
    #[error("guarded site input replacement requires backup recovery at {path}: {source}")]
    RecoverableBackup {
        /// Retained backup path.
        path: PathBuf,
        /// Failure that left recovery material.
        #[source]
        source: anyhow::Error,
    },

    /// Filesystem operation failed before a durable outcome was established.
    #[error("initial site input storage failed: {0}")]
    Storage(#[source] anyhow::Error),

    /// A visible transaction completed but its durable outcome could not be confirmed.
    #[error("initial site input outcome is uncertain: {0}")]
    OutcomeUncertain(#[source] anyhow::Error),

    /// A running backend holds the shared site lock.
    #[error("site is running; stop the backend and retry initialization")]
    SiteRunning,

    /// Advisory lock acquisition failed.
    #[error(transparent)]
    Lock(#[from] SiteInputLockError),
}

/// Result of a read-only initialization-if-missing inspection.
#[derive(Debug)]
pub enum InitialSitePresence {
    /// The site target does not exist and can enter guided initialization.
    Missing,

    /// The site contains a complete valid operational input bundle.
    Initialized,
}

/// Result of one locked initial site transaction.
#[expect(
    clippy::large_enum_variant,
    reason = "the successful initialization outcome returns the complete validated input bundle"
)]
pub enum InitialSiteWriteOutcome {
    /// The requested inputs were created or guardedly replaced.
    Published,

    /// Existing valid inputs already matched and were left unchanged.
    AlreadyInitialized(InitialSiteInputs),
}

fn prepare_lock_parent(site_root: &Path, create_missing: bool) -> anyhow::Result<()> {
    let parent = site_root
        .parent()
        .ok_or_else(|| anyhow!("initial site target has no parent directory"))?;
    let mut current = PathBuf::new();
    for component in parent.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(anyhow!(
                    "Site input lock ancestor {} must not be a symbolic link",
                    current.display()
                ));
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(anyhow!(
                    "Site input lock ancestor {} must be a directory",
                    current.display()
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !create_missing {
                    return Err(anyhow!(
                        "Site input lock parent {} does not exist",
                        current.display()
                    ));
                }
                let containing = current
                    .parent()
                    .ok_or_else(|| anyhow!("Site input lock parent has no containing directory"))?;
                fs::create_dir(&current).map_err(|error| {
                    anyhow::Error::new(error).context(format!(
                        "create site input lock parent {}",
                        current.display()
                    ))
                })?;
                File::open(&current)
                    .and_then(|directory| dotcore_domain::fs_sync::sync(&directory))
                    .map_err(|error| {
                        anyhow::Error::new(error).context(format!(
                            "sync created site input lock parent {}",
                            current.display()
                        ))
                    })?;
                File::open(containing)
                    .and_then(|directory| dotcore_domain::fs_sync::sync(&directory))
                    .map_err(|error| {
                        anyhow::Error::new(error).context(format!(
                            "sync site input lock parent {}",
                            containing.display()
                        ))
                    })?;
            }
            Err(error) => {
                return Err(anyhow::Error::new(error).context(format!(
                    "inspect site input lock ancestor {}",
                    current.display()
                )));
            }
        }
    }
    Ok(())
}

/// Reports whether a backend run left state beside the site inputs: owner state, the node store,
/// application data, logs or telemetry. Runtime resources only exist after such a run.
fn has_retained_state(site_root: &Path) -> bool {
    ["state.pb", "node", "applications-data", "logs", "telemetry"]
        .iter()
        .any(|name| fs::symlink_metadata(site_root.join(name)).is_ok())
}

fn existing_runtime_declarations(site_root: &Path) -> Result<bool, InitialSiteWriteError> {
    let site = decode_site(&site_root.join(SITE_FILE))
        .map_err(InitialSiteWriteError::DeploymentInspection)?;
    Ok(!site.model_deployments.is_empty()
        || !site.application_deployments.is_empty()
        || !site.inference_routes.is_empty())
}

fn reject_symlink(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(anyhow!(
            "Site input lock {} must not be a symbolic link",
            path.display()
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(anyhow::Error::new(error)
            .context(format!("inspect site input lock {}", path.display()))),
    }
}

fn staging_path(site_root: &Path) -> Result<PathBuf, InitialSiteWriteError> {
    transaction_path(site_root, "init-stage")
}

#[cfg(test)]
fn backup_path(site_root: &Path) -> Result<PathBuf, InitialSiteWriteError> {
    transaction_path(site_root, "init-backup")
}

fn transaction_path(site_root: &Path, purpose: &str) -> Result<PathBuf, InitialSiteWriteError> {
    let parent = parent_directory(site_root)?;
    let name = site_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            InitialSiteWriteError::Storage(anyhow!("initial site target has no file name"))
        })?;
    Ok(parent.join(format!(
        ".{name}.{purpose}-{}",
        TRANSACTION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )))
}

fn backup_prefix(site_root: &Path) -> Result<String, InitialSiteWriteError> {
    let name = site_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            InitialSiteWriteError::Storage(anyhow!("initial site target has no file name"))
        })?;
    Ok(format!(".{name}.init-backup-"))
}

fn parent_directory(path: &Path) -> Result<&Path, InitialSiteWriteError> {
    path.parent().ok_or_else(|| {
        InitialSiteWriteError::Storage(anyhow!("initial site target has no parent directory"))
    })
}

fn lock_error(path: &Path, operation: &str, error: std::io::Error) -> SiteInputLockError {
    SiteInputLockError::Storage(anyhow::Error::new(error).context(format!(
        "{operation} sibling site input lock {}",
        path.display()
    )))
}

fn storage_error(operation: &str, path: &Path, error: std::io::Error) -> InitialSiteWriteError {
    InitialSiteWriteError::Storage(
        anyhow::Error::new(error).context(format!("{operation} {}", path.display())),
    )
}

fn outcome_error(operation: &str, path: &Path, error: std::io::Error) -> InitialSiteWriteError {
    InitialSiteWriteError::OutcomeUncertain(
        anyhow::Error::new(error).context(format!("{operation} {}", path.display())),
    )
}

#[cfg(test)]
mod tests;
