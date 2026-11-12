//! Exclusive lock boundary for destructive stopped-site administration: resetting a site to fresh
//! inputs and deleting a site directory.
//!
//! Removal stops the site's runtime resources before deleting its directory. Native workloads are
//! found by their location in the directory, so they are cleaned even when the site identity is
//! unknown, as for a joined worker's folder; other runtimes need the site identity.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use dotcore_domain::SiteNodeId;
use dotcore_runtime::{
    RuntimeBindingKind, SiteRuntimeCleanup, SiteRuntimeCleanupReport, SiteRuntimeCleanups,
    SiteRuntimeInventory, SiteRuntimeResetConfigurationError,
};
use serde::{Deserialize, de::IgnoredAny};
use tracing::{error, info};

use crate::{
    NODE_FILE, SITE_FILE,
    initial::{
        input_store::{InitialSiteInputWriter, InitialSiteWriteError},
        inputs::InitialSiteInputs,
    },
    node::folder::{WORKER_MANIFEST_FILE, WorkerNodeCustody, WorkerNodeCustodyError},
    reset::runtime_removal::{RuntimeRemoval, RuntimeRemovalError},
};

/// Orchestrates one exclusive stopped-site cleanup and fresh initialization.
pub struct SiteResetTransaction {
    runtime: Box<dyn SiteRuntimeCleanup>,
    persistence: Box<dyn SitePersistenceCleanup>,
    publisher: Box<dyn SiteResetPublisher>,
}

impl SiteResetTransaction {
    /// Creates a reset transaction from exact runtime and persistence cleanup capabilities and an
    /// input writer configured with the caller's complete site validator.
    pub fn new(
        runtime: Box<dyn SiteRuntimeCleanup>,
        persistence: Box<dyn SitePersistenceCleanup>,
        writer: InitialSiteInputWriter,
    ) -> Self {
        Self {
            runtime,
            persistence,
            publisher: Box::new(writer),
        }
    }

    /// Lists the runtime resources the session's site owns without changing anything.
    /// A session with no site identity lists only its native workloads.
    pub async fn inventory(
        &self,
        session: &SiteResetSession,
    ) -> Result<SiteRuntimeInventory, SiteResetTransactionError> {
        session.inventory(self.runtime.as_ref()).await
    }

    /// Cleans an existing stopped site and publishes the requested fresh inputs under one lock.
    /// The report names optional runtime cleanups that were skipped as unreachable.
    pub async fn reset(
        &self,
        site_root: &Path,
        cleanups: Arc<dyn SiteRuntimeCleanups>,
        inputs: &InitialSiteInputs,
    ) -> Result<SiteRuntimeCleanupReport, SiteResetTransactionError> {
        info!(event_kind = "site_reset_requested", site_root = %site_root.display(), "Site reset requested");
        let session = match SiteResetSession::begin(site_root, cleanups) {
            Ok(session) => session,
            Err(error) => {
                error!(event_kind = "site_reset_failed", site_root = %site_root.display(), error = %error, "Site reset failed");
                return Err(error);
            }
        };
        let result = self.reset_locked(session, inputs).await;
        match &result {
            Ok(_) => {
                info!(event_kind = "site_reset_completed", site_root = %site_root.display(), "Site reset completed")
            }
            Err(error) => {
                error!(event_kind = "site_reset_failed", site_root = %site_root.display(), error = %error, "Site reset failed")
            }
        }
        result
    }

    /// Executes cleanup and publication using a reset session acquired before preparation.
    /// Cleanup other than native needs the site identity, so an unidentified session cleans only
    /// native workloads before removing any existing root. The report names optional runtime
    /// cleanups that were skipped as unreachable.
    pub async fn reset_locked(
        &self,
        session: SiteResetSession,
        inputs: &InitialSiteInputs,
    ) -> Result<SiteRuntimeCleanupReport, SiteResetTransactionError> {
        let removed = self.remove_locked(session).await?;
        self.publish_removed(removed, inputs)
    }

    /// Removes the site's runtime resources, workload caches and persisted state, keeping the
    /// exclusion in the returned [`RemovedSite`] so the fresh inputs can be collected and
    /// published without another process taking the root in between.
    pub async fn remove_locked(
        &self,
        session: SiteResetSession,
    ) -> Result<RemovedSite, SiteResetTransactionError> {
        let SiteResetSession {
            site_root,
            identity,
            cleanups,
            lock,
            _custody,
        } = session;
        let report = remove_site(
            self.runtime.as_ref(),
            self.persistence.as_ref(),
            cleanups.as_ref(),
            &site_root,
            identity,
        )
        .await?;
        Ok(RemovedSite {
            site_root,
            lock,
            _custody,
            report,
        })
    }

    /// Publishes the requested fresh inputs into a removed site and releases its exclusion.
    pub fn publish_removed(
        &self,
        removed: RemovedSite,
        inputs: &InitialSiteInputs,
    ) -> Result<SiteRuntimeCleanupReport, SiteResetTransactionError> {
        self.publish(&removed.site_root, inputs, &removed.lock)?;
        Ok(removed.report)
    }

    fn publish(
        &self,
        site_root: &Path,
        inputs: &InitialSiteInputs,
        lock: &SiteResetLock,
    ) -> Result<(), SiteResetTransactionError> {
        self.publisher
            .publish(site_root, inputs, lock)
            .map_err(SiteResetTransactionError::FreshInitialization)
    }

    #[cfg(test)]
    fn with_publisher(mut self, publisher: Box<dyn SiteResetPublisher>) -> Self {
        self.publisher = publisher;
        self
    }
}

/// A site root whose runtime resources, caches and state were removed and that is still excluded
/// from other processes, waiting for fresh inputs to be published.
#[derive(Debug)]
pub struct RemovedSite {
    site_root: PathBuf,
    lock: SiteResetLock,
    /// Custody of a joined node folder, held until the fresh inputs are published.
    _custody: Option<WorkerNodeCustody>,
    report: SiteRuntimeCleanupReport,
}

/// Orchestrates one exclusive stopped-site deletion: runtime cleanup, then persistence removal.
/// Unlike a reset it publishes nothing, so the site root is absent on success.
pub struct SiteDeleteTransaction {
    runtime: Box<dyn SiteRuntimeCleanup>,
    persistence: Box<dyn SitePersistenceCleanup>,
}

impl SiteDeleteTransaction {
    /// Creates a deletion from exact runtime and persistence cleanup capabilities.
    pub fn new(
        runtime: Box<dyn SiteRuntimeCleanup>,
        persistence: Box<dyn SitePersistenceCleanup>,
    ) -> Self {
        Self {
            runtime,
            persistence,
        }
    }

    /// Lists the runtime resources the session's site owns without changing anything.
    /// A session with no site identity lists only its native workloads.
    pub async fn inventory(
        &self,
        session: &SiteResetSession,
    ) -> Result<SiteRuntimeInventory, SiteResetTransactionError> {
        session.inventory(self.runtime.as_ref()).await
    }

    /// Removes the site's runtime resources and then its complete persistence root under the lock.
    /// Persistence is untouched when runtime cleanup fails; an unidentified session cleans only
    /// native workloads, because other runtimes classify resources by the site id.
    pub async fn delete(
        &self,
        session: SiteResetSession,
    ) -> Result<SiteRuntimeCleanupReport, SiteResetTransactionError> {
        let SiteResetSession {
            site_root,
            identity,
            cleanups,
            lock: _lock,
            _custody,
        } = session;
        info!(event_kind = "site_delete_requested", site_root = %site_root.display(), "Site delete requested");
        let result = remove_site(
            self.runtime.as_ref(),
            self.persistence.as_ref(),
            cleanups.as_ref(),
            &site_root,
            identity,
        )
        .await;
        match &result {
            Ok(_) => {
                info!(event_kind = "site_delete_completed", site_root = %site_root.display(), "Site delete completed")
            }
            Err(error) => {
                error!(event_kind = "site_delete_failed", site_root = %site_root.display(), error = %error, "Site delete failed")
            }
        }
        result
    }
}

/// Removes all persistence beneath one validated site root.
pub trait SitePersistenceCleanup: Send + Sync {
    /// Checks without changing anything that the root is safe to remove, so a removal can refuse
    /// before it stops any workload rather than after.
    fn preflight<'a>(
        &'a self,
        site_root: &'a Path,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    /// Ensures that the site persistence root is absent.
    fn cleanup<'a>(
        &'a self,
        site_root: &'a Path,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;
}

trait SiteResetPublisher: Send + Sync {
    fn publish(
        &self,
        site_root: &Path,
        inputs: &InitialSiteInputs,
        lock: &SiteResetLock,
    ) -> Result<(), InitialSiteWriteError>;
}

impl SiteResetPublisher for InitialSiteInputWriter {
    fn publish(
        &self,
        site_root: &Path,
        inputs: &InitialSiteInputs,
        lock: &SiteResetLock,
    ) -> Result<(), InitialSiteWriteError> {
        self.create_under_reset_lock(site_root, inputs, lock)
    }
}

/// Pre-acquired stopped-site reset state retained across input collection and execution.
#[derive(Debug)]
pub struct SiteResetSession {
    site_root: PathBuf,
    identity: Option<ExistingSiteIdentity>,
    /// Runtime-specific cleanups, selected by the runtime binding kinds the site uses.
    cleanups: Arc<dyn SiteRuntimeCleanups>,
    lock: SiteResetLock,
    /// Custody of a joined node folder, held so a running node server excludes the session.
    _custody: Option<WorkerNodeCustody>,
}

impl SiteResetSession {
    /// Acquires exclusion before observing existing site identity or contacting external systems.
    /// Runtime cleanups come from `cleanups`, selected by the runtime bindings the site uses.
    pub fn begin(
        site_root: &Path,
        cleanups: Arc<dyn SiteRuntimeCleanups>,
    ) -> Result<Self, SiteResetTransactionError> {
        let lock = SiteResetLock::try_acquire(site_root)?;
        let custody = acquire_node_custody(site_root)?;
        let identity = existing_site_identity(site_root)?;
        Ok(Self {
            site_root: site_root.to_owned(),
            identity,
            cleanups,
            lock,
            _custody: custody,
        })
    }

    /// Acquires exclusion without reading the site identity, for deleting a site or node folder
    /// whose configuration is missing or invalid. Such a session classifies only native workloads,
    /// by their location in the root. The root must still look like a site or node folder (or be
    /// empty or absent), so a wrong path never selects an unrelated directory for removal.
    pub fn begin_unidentified(
        site_root: &Path,
        cleanups: Arc<dyn SiteRuntimeCleanups>,
    ) -> Result<Self, SiteResetTransactionError> {
        Self::begin_unidentified_with(
            site_root,
            cleanups,
            &[SITE_FILE, "node.yaml", "worker.yaml"],
        )
    }

    /// Like [`Self::begin_unidentified`], but for replacing a manager site: the root must hold
    /// `site.yaml` (or be empty or absent), so a node folder is never overwritten.
    pub fn begin_unidentified_site(
        site_root: &Path,
        cleanups: Arc<dyn SiteRuntimeCleanups>,
    ) -> Result<Self, SiteResetTransactionError> {
        Self::begin_unidentified_with(site_root, cleanups, &[SITE_FILE])
    }

    fn begin_unidentified_with(
        site_root: &Path,
        cleanups: Arc<dyn SiteRuntimeCleanups>,
        markers: &[&str],
    ) -> Result<Self, SiteResetTransactionError> {
        let lock = SiteResetLock::try_acquire(site_root)?;
        let custody = acquire_node_custody(site_root)?;
        require_recognizable_root(site_root, markers)?;
        Ok(Self {
            site_root: site_root.to_owned(),
            identity: None,
            cleanups,
            lock,
            _custody: custody,
        })
    }

    /// Counts the workers provisioned against the site's current trust; a reset invalidates them.
    pub fn provisioned_workers(&self) -> std::io::Result<usize> {
        let pins = self
            .site_root
            .join(crate::manager_trust::SITE_TRUST_DIRECTORY)
            .join(crate::manager_trust::WORKER_PIN_DIRECTORY);
        let entries = match fs::read_dir(&pins) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        let mut count = 0;
        for entry in entries {
            if entry?.file_name().to_string_lossy().ends_with(".sha256") {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Returns the exact site root protected by this session.
    pub fn site_root(&self) -> &Path {
        &self.site_root
    }

    /// Returns the runtime cleanups this session removes the site with.
    pub fn cleanups(&self) -> Arc<dyn SiteRuntimeCleanups> {
        Arc::clone(&self.cleanups)
    }

    /// Lists the runtime resources this site owns through `runtime`, then its native workloads,
    /// without changing anything. A session with no site identity lists only native workloads.
    pub async fn inventory(
        &self,
        runtime: &dyn SiteRuntimeCleanup,
    ) -> Result<SiteRuntimeInventory, SiteResetTransactionError> {
        runtime_removal(
            self.cleanups.as_ref(),
            &self.site_root,
            self.identity.as_ref(),
        )
        .inventory(runtime)
        .await
        .map_err(SiteResetTransactionError::from)
    }

    /// Selects runtime cleanup from the runtime bindings of the existing site configuration.
    /// Damaged declarations require cleanup of every supported runtime; a binding to an unknown
    /// adapter is refused, because its resources cannot be classified.
    pub fn runtime_cleanup(
        &self,
    ) -> Result<Box<dyn SiteRuntimeCleanup>, SiteRuntimeResetConfigurationError> {
        let bound = match &self.identity {
            None => BTreeSet::new(),
            Some(identity) => match identity.runtime_binding_kinds() {
                Ok(Some(bound)) => bound,
                Ok(None) => RuntimeBindingKind::ALL.into_iter().collect(),
                Err(adapter) => {
                    return Err(SiteRuntimeResetConfigurationError::UnknownBinding { adapter });
                }
            },
        };
        self.cleanups.site_cleanup(&bound)
    }
}

/// Holds the exclusive site reset lock until dropped.
#[derive(Debug)]
pub struct SiteResetLock {
    _lock: crate::initial::input_store::SiteInputLock,
}

impl SiteResetLock {
    /// Attempts to exclude every backend startup without waiting.
    pub fn try_acquire(site_root: &Path) -> Result<Self, SiteResetLockError> {
        let lock = crate::initial::input_store::try_acquire_exclusive_site_lock(site_root)
            .map_err(reset_lock_error)?
            .ok_or_else(|| SiteResetLockError::SiteRunning {
                site_root: site_root.to_owned(),
            })?;
        Ok(Self { _lock: lock })
    }
}

/// Failure while acquiring the exclusive site reset lock.
#[derive(Debug, thiserror::Error)]
pub enum SiteResetLockError {
    /// A backend currently retains the shared site lock.
    #[error("site is running at {site_root}")]
    SiteRunning {
        /// Site root whose shared lock is held.
        site_root: PathBuf,
    },

    /// A running node server holds custody of the joined node folder.
    #[error("node folder at {site_root} is held by a running node")]
    NodeRunning {
        /// Node folder whose custody is held.
        site_root: PathBuf,
    },

    /// The stable sibling lock could not be inspected, opened, or acquired.
    #[error("site reset lock failed: {0}")]
    Storage(#[source] anyhow::Error),
}

/// Failure while resetting and freshly initializing one stopped site.
#[derive(Debug, thiserror::Error)]
pub enum SiteResetTransactionError {
    /// The exclusive stopped-site lock could not be acquired.
    #[error(transparent)]
    Lock(#[from] SiteResetLockError),

    /// Existing site identity could not be safely established before mutation.
    #[error("existing site identity is invalid: {0}")]
    ExistingSiteIdentity(#[from] SiteResetIdentityError),

    /// Exact runtime residue could not be completely removed.
    #[error("site runtime cleanup failed: {0}")]
    RuntimeCleanup(#[source] anyhow::Error),

    /// Native workloads in the site directory could not be classified, stopped, or removed.
    #[error("Site native workload cleanup failed: {0}")]
    NativeCleanup(#[source] anyhow::Error),

    /// Existing site persistence could not be completely removed.
    #[error("site persistence cleanup failed: {0}")]
    PersistenceCleanup(#[source] anyhow::Error),

    /// Fresh site inputs could not be conclusively published.
    #[error("fresh site initialization failed: {0}")]
    FreshInitialization(#[source] InitialSiteWriteError),
}

impl From<RuntimeRemovalError> for SiteResetTransactionError {
    fn from(error: RuntimeRemovalError) -> Self {
        match error {
            RuntimeRemovalError::Native(source) => Self::NativeCleanup(source),
            RuntimeRemovalError::Runtime(source) => Self::RuntimeCleanup(source),
        }
    }
}

/// Failure while reading the minimal pre-reset identity from an existing site.
#[derive(Debug, thiserror::Error)]
pub enum SiteResetIdentityError {
    /// The existing site root is not a real directory.
    #[error("existing site root must be a directory: {path}")]
    UnexpectedRoot {
        /// Rejected existing site root.
        path: PathBuf,
    },

    /// The existing site YAML could not be read through the restricted YAML boundary.
    #[error("site.yaml could not be read for site identity")]
    SiteYaml(#[source] dotcore_proto::yaml::RestrictedYamlError),

    /// The persisted site identity does not use the stable identifier syntax.
    #[error("site.yaml siteId must match [a-z][a-z0-9-]{{0,62}}")]
    InvalidSiteId,

    /// The root is a non-empty directory with no site or node folder input file.
    #[error("{path} is not empty and has no site.yaml, node.yaml, or worker.yaml")]
    UnrecognizedRoot {
        /// Rejected root.
        path: PathBuf,
    },
}

/// Minimal pre-reset view of an existing site: its identity and its raw execution targets.
/// Execution targets stay untyped so a damaged declaration still yields the site identity.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExistingSiteIdentity {
    site_id: String,

    #[serde(default)]
    execution_targets: serde_json::Value,
}

impl ExistingSiteIdentity {
    fn site_id(&self) -> &str {
        &self.site_id
    }

    /// Returns the runtime binding kinds of the execution targets, `None` when a damaged
    /// declaration cannot be classified to exactly one adapter, or the first adapter key this
    /// version does not know.
    fn runtime_binding_kinds(&self) -> Result<Option<BTreeSet<RuntimeBindingKind>>, String> {
        if self.execution_targets.is_null() {
            return Ok(Some(BTreeSet::new()));
        }
        let Ok(targets) =
            serde_json::from_value::<Vec<ExistingExecutionTarget>>(self.execution_targets.clone())
        else {
            return Ok(None);
        };
        let mut kinds = Some(BTreeSet::new());
        for target in &targets {
            match target.runtime_binding_kind()? {
                Some(kind) => {
                    if let Some(kinds) = &mut kinds {
                        kinds.insert(kind);
                    }
                }
                None => kinds = None,
            }
        }
        Ok(kinds)
    }
}

/// Runtime binding of one existing execution target, keyed by its adapter name.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExistingExecutionTarget {
    #[serde(default)]
    model_runtime: Option<BTreeMap<String, IgnoredAny>>,

    #[serde(default)]
    application_runtime: Option<BTreeMap<String, IgnoredAny>>,
}

impl ExistingExecutionTarget {
    /// Returns the bound runtime binding kind, `None` when the target binds no single adapter, or
    /// the adapter key this version does not know.
    fn runtime_binding_kind(&self) -> Result<Option<RuntimeBindingKind>, String> {
        let binding = match (&self.model_runtime, &self.application_runtime) {
            (Some(binding), None) | (None, Some(binding)) => binding,
            _ => return Ok(None),
        };
        let mut adapters = binding.keys();
        let (Some(adapter), None) = (adapters.next(), adapters.next()) else {
            return Ok(None);
        };
        match adapter.as_str() {
            "dockerRuntime" => Ok(Some(RuntimeBindingKind::Docker)),
            "nativeRuntime" => Ok(Some(RuntimeBindingKind::Native)),
            "simulatedRuntime" => Ok(Some(RuntimeBindingKind::Simulated)),
            unknown => Err(unknown.to_owned()),
        }
    }
}

/// Removes the site's runtime resources, then ensures its persistence root is absent. Native
/// workloads are cleaned by their location whatever the identity; other runtime cleanup and the
/// removal of the site's workload caches need the site identity, so an unidentified session skips
/// them. A root unsafe to remove and untrusted
/// native evidence both refuse before any runtime changes, and persistence is untouched when any
/// cleanup fails.
async fn remove_site(
    runtime: &dyn SiteRuntimeCleanup,
    persistence: &dyn SitePersistenceCleanup,
    cleanups: &dyn SiteRuntimeCleanups,
    site_root: &Path,
    identity: Option<ExistingSiteIdentity>,
) -> Result<SiteRuntimeCleanupReport, SiteResetTransactionError> {
    persistence
        .preflight(site_root)
        .await
        .map_err(SiteResetTransactionError::PersistenceCleanup)?;
    let report = runtime_removal(cleanups, site_root, identity.as_ref())
        .cleanup(runtime)
        .await?;
    if let Some(identity) = &identity {
        // Workload caches sit outside the site root and are keyed by site id, and no workload
        // runs any more, so they go once the runtimes are clean.
        cleanups
            .remove_workload_caches(identity.site_id())
            .await
            .map_err(SiteResetTransactionError::RuntimeCleanup)?;
    }
    persistence
        .cleanup(site_root)
        .await
        .map_err(SiteResetTransactionError::PersistenceCleanup)?;
    Ok(report)
}

/// Selects the runtime cleanup of the site root, expecting the site identity when known and the
/// `SiteNode` its `node.yaml` binds when readable.
fn runtime_removal(
    cleanups: &dyn SiteRuntimeCleanups,
    site_root: &Path,
    identity: Option<&ExistingSiteIdentity>,
) -> RuntimeRemoval {
    RuntimeRemoval::site_directory(
        cleanups,
        site_root,
        identity.map(ExistingSiteIdentity::site_id),
        bound_site_node_id(site_root),
    )
}

/// `SiteNode` identity bound by the root's `node.yaml`, or `None` when it is missing or unreadable.
fn bound_site_node_id(site_root: &Path) -> Option<SiteNodeId> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ExistingNodeBinding {
        node_id: String,
    }

    let binding =
        dotcore_proto::yaml::from_file::<ExistingNodeBinding>(&site_root.join(NODE_FILE)).ok()?;
    SiteNodeId::parse(&binding.node_id).ok()
}

/// Accepts an absent or empty root, or one holding at least one of the `markers` input files.
fn require_recognizable_root(
    site_root: &Path,
    markers: &[&str],
) -> Result<(), SiteResetIdentityError> {
    let mut entries = match fs::read_dir(site_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {
            return Err(SiteResetIdentityError::UnexpectedRoot {
                path: site_root.to_owned(),
            });
        }
    };
    let recognizable =
        entries.next().is_none() || markers.iter().any(|name| site_root.join(name).is_file());
    if recognizable {
        Ok(())
    } else {
        Err(SiteResetIdentityError::UnrecognizedRoot {
            path: site_root.to_owned(),
        })
    }
}

fn existing_site_identity(
    site_root: &Path,
) -> Result<Option<ExistingSiteIdentity>, SiteResetIdentityError> {
    match fs::symlink_metadata(site_root) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(SiteResetIdentityError::UnexpectedRoot {
                path: site_root.to_owned(),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(SiteResetIdentityError::SiteYaml(
                dotcore_proto::yaml::RestrictedYamlError::FileReadError(error),
            ));
        }
    }
    let identity =
        dotcore_proto::yaml::from_file::<ExistingSiteIdentity>(&site_root.join(SITE_FILE))
            .map_err(SiteResetIdentityError::SiteYaml)?;
    if !dotcore_domain::valid_id(&identity.site_id) {
        return Err(SiteResetIdentityError::InvalidSiteId);
    }
    Ok(Some(identity))
}

/// Takes custody of a joined node folder (a regular `worker.yaml`), so a running node server
/// excludes the session. Manager, empty, missing and symlinked-manifest roots take none, and the
/// custody lock file is never created for them. Callers take the sibling `<root>.lock` first, then
/// custody; both are non-blocking, so the order cannot deadlock.
fn acquire_node_custody(site_root: &Path) -> Result<Option<WorkerNodeCustody>, SiteResetLockError> {
    let manifest = site_root.join(WORKER_MANIFEST_FILE);
    match fs::symlink_metadata(&manifest) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(SiteResetLockError::Storage(
                anyhow::Error::new(source)
                    .context(format!("inspect node manifest {}", manifest.display())),
            ));
        }
    }
    match WorkerNodeCustody::try_acquire(site_root) {
        Ok(custody) => Ok(Some(custody)),
        Err(WorkerNodeCustodyError::AlreadyHeld) => Err(SiteResetLockError::NodeRunning {
            site_root: site_root.to_owned(),
        }),
        Err(error @ WorkerNodeCustodyError::Storage { .. }) => {
            Err(SiteResetLockError::Storage(anyhow::Error::new(error)))
        }
    }
}

fn reset_lock_error(error: crate::initial::input_store::SiteInputLockError) -> SiteResetLockError {
    match error {
        crate::initial::input_store::SiteInputLockError::Storage(source) => {
            SiteResetLockError::Storage(source)
        }
        error @ crate::initial::input_store::SiteInputLockError::Held { .. } => {
            SiteResetLockError::Storage(anyhow::Error::new(error))
        }
    }
}

#[cfg(test)]
mod tests;
