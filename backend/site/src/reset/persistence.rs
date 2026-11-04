//! Guarded removal of the complete disposable site persistence root.

use std::{
    fs,
    future::Future,
    io,
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

#[cfg(target_os = "linux")]
use anyhow::Context;
use dotcore_runtime::SiteRuntimeCleanups;

use crate::reset::transaction::SitePersistenceCleanup;

/// Removes a validated site root, including a constrained fallback for root-owned entries.
/// The sibling site lock and every path outside the selected root remain outside this capability.
pub struct SitePersistenceReset {
    host: Arc<dyn HostSiteRootRemover>,
    root_owned: Arc<dyn RootOwnedSiteDataCleanup>,
    mounts: Arc<dyn SiteMountInspector>,
}

impl SitePersistenceReset {
    /// Creates the production persistence cleanup capability without contacting any runtime.
    /// The runtime cleanups remove root-owned data only after a permission-denied host removal
    /// under the reset lock.
    pub fn new(cleanups: Arc<dyn SiteRuntimeCleanups>) -> Self {
        Self {
            host: Arc::new(SystemHostSiteRootRemover),
            root_owned: Arc::new(RuntimeRootOwnedSiteDataCleanup(cleanups)),
            mounts: Arc::new(SystemSiteMountInspector),
        }
    }

    #[cfg(test)]
    fn with_adapters(
        host: Arc<dyn HostSiteRootRemover>,
        root_owned: Arc<dyn RootOwnedSiteDataCleanup>,
        mounts: Arc<dyn SiteMountInspector>,
    ) -> Self {
        Self {
            host,
            root_owned,
            mounts,
        }
    }

    async fn cleanup_root(&self, site_root: &Path) -> Result<(), SitePersistenceResetError> {
        let site_root = site_root.to_owned();
        if self.validate(site_root.clone()).await? == ValidatedSiteRoot::Absent {
            return Ok(());
        }

        match self.remove_on_host(site_root.clone()).await {
            Ok(()) => return confirm_root_absent(&site_root),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
            Err(source) => return Err(host_removal_error(&site_root, source)),
        }

        if self.validate(site_root.clone()).await? == ValidatedSiteRoot::Absent {
            return Ok(());
        }
        self.root_owned
            .cleanup(&site_root)
            .await
            .map_err(|source| SitePersistenceResetError::RootOwnedFallback {
                site_root: site_root.clone(),
                source,
            })?;
        if self.validate(site_root.clone()).await? == ValidatedSiteRoot::Absent {
            return Ok(());
        }

        match self.remove_on_host(site_root.clone()).await {
            Ok(()) => confirm_root_absent(&site_root),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(host_removal_error(&site_root, source)),
        }
    }

    async fn validate(
        &self,
        site_root: PathBuf,
    ) -> Result<ValidatedSiteRoot, SitePersistenceResetError> {
        let mounts = Arc::clone(&self.mounts);
        tokio::task::spawn_blocking(move || validate_site_root(&site_root, mounts.as_ref()))
            .await
            .map_err(|source| {
                SitePersistenceResetError::Inspection(
                    anyhow::Error::new(source).context("join site persistence safety inspection"),
                )
            })?
    }

    async fn remove_on_host(&self, site_root: PathBuf) -> io::Result<()> {
        let host = Arc::clone(&self.host);
        tokio::task::spawn_blocking(move || host.remove(&site_root))
            .await
            .unwrap_or_else(|source| Err(io::Error::other(source)))
    }
}

impl SitePersistenceCleanup for SitePersistenceReset {
    fn preflight<'a>(
        &'a self,
        site_root: &'a Path,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            self.validate(site_root.to_owned())
                .await
                .map(|_| ())
                .map_err(anyhow::Error::new)
        })
    }

    fn cleanup<'a>(
        &'a self,
        site_root: &'a Path,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            self.cleanup_root(site_root)
                .await
                .map_err(anyhow::Error::new)
        })
    }
}

/// Failure while validating or removing the selected site persistence root.
/// Errors retain the filesystem or Docker source needed by the local administrator.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SitePersistenceResetError {
    #[error("unsafe site persistence root: {0}")]
    UnsafeRoot(#[source] anyhow::Error),

    #[error("site persistence safety inspection failed: {0}")]
    Inspection(#[source] anyhow::Error),

    #[error("host site persistence removal failed: {0}")]
    HostRemoval(#[source] anyhow::Error),

    #[error("root-owned site persistence cleanup failed at {site_root}: {source}")]
    RootOwnedFallback {
        site_root: PathBuf,
        #[source]
        source: anyhow::Error,
    },

    #[error("site persistence removal outcome is uncertain: {0}")]
    OutcomeUncertain(#[source] anyhow::Error),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ValidatedSiteRoot {
    Present,
    Absent,
}

trait HostSiteRootRemover: Send + Sync {
    fn remove(&self, site_root: &Path) -> io::Result<()>;
}

struct SystemHostSiteRootRemover;

impl HostSiteRootRemover for SystemHostSiteRootRemover {
    fn remove(&self, site_root: &Path) -> io::Result<()> {
        fs::remove_dir_all(site_root)
    }
}

type RootOwnedCleanupFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

trait RootOwnedSiteDataCleanup: Send + Sync {
    fn cleanup<'a>(&'a self, site_root: &'a Path) -> RootOwnedCleanupFuture<'a>;
}

/// Root-owned site data removal through the injected runtime cleanups.
struct RuntimeRootOwnedSiteDataCleanup(Arc<dyn SiteRuntimeCleanups>);

impl RootOwnedSiteDataCleanup for RuntimeRootOwnedSiteDataCleanup {
    fn cleanup<'a>(&'a self, site_root: &'a Path) -> RootOwnedCleanupFuture<'a> {
        self.0.remove_root_owned_site_data(site_root)
    }
}

trait SiteMountInspector: Send + Sync {
    fn first_mount_at_or_beneath(&self, site_root: &Path) -> anyhow::Result<Option<PathBuf>>;
}

struct SystemSiteMountInspector;

impl SiteMountInspector for SystemSiteMountInspector {
    fn first_mount_at_or_beneath(&self, site_root: &Path) -> anyhow::Result<Option<PathBuf>> {
        platform_mount_beneath(site_root)
    }
}

fn validate_site_root(
    site_root: &Path,
    mounts: &dyn SiteMountInspector,
) -> Result<ValidatedSiteRoot, SitePersistenceResetError> {
    validate_lexical_root(site_root)?;
    let parent = site_root
        .parent()
        .ok_or_else(|| unsafe_root("Site root has no parent"))?;
    for ancestor in parent.ancestors() {
        validate_real_directory(ancestor, "Site root ancestor")?;
    }

    let root_metadata = match fs::symlink_metadata(site_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ValidatedSiteRoot::Absent);
        }
        Err(source) => return Err(inspection_error("inspect site root", site_root, source)),
    };
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(unsafe_root(format!(
            "Site root must be a real directory: {}",
            site_root.display()
        )));
    }
    if let Some(mount) = mounts
        .first_mount_at_or_beneath(site_root)
        .map_err(SitePersistenceResetError::Inspection)?
    {
        return Err(unsafe_root(format!(
            "Site root contains a mount boundary at {}",
            mount.display()
        )));
    }

    let root_device = std::os::unix::fs::MetadataExt::dev(&root_metadata);
    let mut pending = vec![site_root.to_owned()];
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory)
            .map_err(|source| inspection_error("read site directory", &directory, source))?;
        for entry in entries {
            let entry = entry.map_err(|source| {
                inspection_error("read site directory entry", &directory, source)
            })?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|source| inspection_error("inspect site entry", &path, source))?;
            // Workload data holds links, such as Hugging Face cache snapshots. Removal unlinks them
            // without following, so the scan neither rejects nor descends into them.
            if metadata.file_type().is_symlink() {
                continue;
            }
            if std::os::unix::fs::MetadataExt::dev(&metadata) != root_device {
                return Err(unsafe_root(format!(
                    "Site root crosses a filesystem boundary at {}",
                    path.display()
                )));
            }
            if metadata.is_dir() {
                pending.push(path);
            }
        }
    }
    Ok(ValidatedSiteRoot::Present)
}

fn validate_lexical_root(site_root: &Path) -> Result<(), SitePersistenceResetError> {
    if !site_root.is_absolute()
        || site_root.to_str().is_none()
        || site_root.parent().is_none()
        || site_root.parent() == Some(Path::new("/"))
        || site_root
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(unsafe_root(format!(
            "Site root must be an absolute normalized non-top-level path: {}",
            site_root.display()
        )));
    }
    Ok(())
}

fn validate_real_directory(
    path: &Path,
    description: &str,
) -> Result<(), SitePersistenceResetError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| inspection_error("inspect site root ancestor", path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(unsafe_root(format!(
            "{description} must be a real directory: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn platform_mount_beneath(site_root: &Path) -> anyhow::Result<Option<PathBuf>> {
    let mount_info = fs::read_to_string("/proc/self/mountinfo")
        .context("read Linux mount table for site cleanup")?;
    for line in mount_info.lines() {
        let Some(encoded) = line.split_whitespace().nth(4) else {
            continue;
        };
        let mount = PathBuf::from(decode_mount_path(encoded)?);
        if mount == site_root || mount.starts_with(site_root) {
            return Ok(Some(mount));
        }
    }
    Ok(None)
}

#[cfg(target_os = "linux")]
fn decode_mount_path(encoded: &str) -> anyhow::Result<String> {
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            if index + 3 >= bytes.len() {
                anyhow::bail!("Linux mount table contains a truncated escape");
            }
            let octal = std::str::from_utf8(&bytes[index + 1..index + 4])
                .context("decode Linux mount-table escape")?;
            let value = u8::from_str_radix(octal, 8).context("parse Linux mount-table escape")?;
            decoded.push(value);
            index += 4;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).context("decode Linux mount-table path")
}

#[cfg(not(target_os = "linux"))]
fn platform_mount_beneath(_site_root: &Path) -> anyhow::Result<Option<PathBuf>> {
    Ok(None)
}

fn confirm_root_absent(site_root: &Path) -> Result<(), SitePersistenceResetError> {
    match fs::symlink_metadata(site_root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(SitePersistenceResetError::OutcomeUncertain(
            anyhow::anyhow!(
                "Site persistence root remains visible at {}",
                site_root.display()
            ),
        )),
        Err(source) => Err(SitePersistenceResetError::OutcomeUncertain(
            anyhow::Error::new(source).context(format!(
                "confirm site root removal at {}",
                site_root.display()
            )),
        )),
    }
}

fn unsafe_root(message: impl Into<String>) -> SitePersistenceResetError {
    SitePersistenceResetError::UnsafeRoot(anyhow::anyhow!(message.into()))
}

fn inspection_error(operation: &str, path: &Path, source: io::Error) -> SitePersistenceResetError {
    SitePersistenceResetError::Inspection(
        anyhow::Error::new(source).context(format!("{operation} at {}", path.display())),
    )
}

fn host_removal_error(site_root: &Path, source: io::Error) -> SitePersistenceResetError {
    SitePersistenceResetError::HostRemoval(anyhow::Error::new(source).context(format!(
        "remove site persistence at {}",
        site_root.display()
    )))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use super::*;
    use crate::initial::{
        input_store::InitialSiteInputWriter, validation::SiteInputBundleValidator,
    };

    #[tokio::test]
    async fn direct_cleanup_removes_the_complete_tree_and_preserves_every_sibling() {
        let fixture = Fixture::new();
        fixture.populate_site();
        let before_siblings = fixture.sibling_snapshot();
        let cleanup = SitePersistenceReset::new(crate::test_support::site_runtime_cleanups());

        cleanup
            .cleanup_root(&fixture.site_root)
            .await
            .expect("cleanup");

        assert!(!fixture.site_root.exists());
        assert_eq!(fixture.sibling_snapshot(), before_siblings);
    }

    #[tokio::test]
    async fn nested_symlinks_are_removed_without_touching_their_targets() {
        let fixture = Fixture::new();
        fixture.populate_site();
        let outside = fixture.root.join("outside");
        fs::create_dir_all(outside.join("directory")).expect("outside directory");
        fs::write(outside.join("file"), "outside file").expect("outside file");
        fs::write(outside.join("directory/inner"), "outside inner").expect("outside inner");
        let cache = fixture.site_root.join("applications-data/service/cache");
        fs::create_dir_all(cache.join("blobs")).expect("cache blobs");
        fs::create_dir_all(cache.join("snapshots/revision")).expect("cache snapshots");
        fs::write(cache.join("blobs/hash"), "blob").expect("cache blob");
        symlink_file(
            Path::new("../../blobs/hash"),
            &cache.join("snapshots/revision/config.json"),
        );
        symlink_file(&outside.join("file"), &cache.join("file-link"));
        symlink_directory(&outside.join("directory"), &cache.join("directory-link"));
        symlink_file(&outside.join("missing"), &cache.join("dangling-link"));
        let before_outside = tree_snapshot(&outside);
        let before_siblings = fixture.sibling_snapshot();
        let cleanup = SitePersistenceReset::new(crate::test_support::site_runtime_cleanups());

        cleanup
            .cleanup_root(&fixture.site_root)
            .await
            .expect("cleanup with nested links");

        assert!(!fixture.site_root.exists());
        assert_eq!(
            before_outside,
            vec![
                ("directory/inner".to_owned(), b"outside inner".to_vec()),
                ("file".to_owned(), b"outside file".to_vec()),
            ]
        );
        assert_eq!(tree_snapshot(&outside), before_outside);
        assert_eq!(fixture.sibling_snapshot(), before_siblings);
    }

    #[tokio::test]
    async fn unsafe_roots_and_mounts_fail_before_any_removal() {
        let fixture = Fixture::new();
        fixture.populate_site();
        let host = Arc::new(RecordingHostRemover::default());
        let fallback = Arc::new(RecordingRootOwnedCleanup::default());
        let safe_mounts = Arc::new(StaticMountInspector(None));

        let root_error = SitePersistenceReset::with_adapters(
            host.clone(),
            fallback.clone(),
            safe_mounts.clone(),
        )
        .cleanup_root(Path::new("/"))
        .await
        .expect_err("filesystem root");
        assert!(matches!(
            root_error,
            SitePersistenceResetError::UnsafeRoot(_)
        ));

        let relative_error = SitePersistenceReset::with_adapters(
            host.clone(),
            fallback.clone(),
            safe_mounts.clone(),
        )
        .cleanup_root(Path::new("relative/site"))
        .await
        .expect_err("relative root");
        assert!(matches!(
            relative_error,
            SitePersistenceResetError::UnsafeRoot(_)
        ));

        let mounted = fixture.site_root.join("applications-data");
        let mount_error = SitePersistenceReset::with_adapters(
            host.clone(),
            fallback.clone(),
            Arc::new(StaticMountInspector(Some(mounted.clone()))),
        )
        .cleanup_root(&fixture.site_root)
        .await
        .expect_err("mounted entry");
        assert!(matches!(
            mount_error,
            SitePersistenceResetError::UnsafeRoot(_)
        ));

        assert_eq!(host.calls.load(Ordering::Relaxed), 0);
        assert!(fallback.calls.lock().expect("fallback calls").is_empty());
        assert!(fixture.site_root.join("site.yaml").is_file());
    }

    #[tokio::test]
    async fn root_and_parent_symlinks_are_rejected_without_following_them() {
        let fixture = Fixture::new();
        fixture.populate_site();
        let real_parent = fixture.root.join("real-parent");
        fs::create_dir(&real_parent).expect("real parent");
        let real_site = real_parent.join("site");
        fs::create_dir(&real_site).expect("real site");
        fs::write(real_site.join("marker"), "real").expect("real marker");
        let parent_link = fixture.root.join("parent-link");
        symlink_directory(&real_parent, &parent_link);
        let nested_parent_path = parent_link.join("nested");
        fs::create_dir(real_parent.join("nested")).expect("real nested parent");
        fs::create_dir(real_parent.join("nested/site")).expect("nested real site");
        let root_link = fixture.root.join("site-link");
        symlink_directory(&fixture.site_root, &root_link);
        let cleanup = SitePersistenceReset::new(crate::test_support::site_runtime_cleanups());

        let parent_error = cleanup
            .cleanup_root(&parent_link.join("site"))
            .await
            .expect_err("parent symlink");
        let nested_parent_error = cleanup
            .cleanup_root(&nested_parent_path.join("site"))
            .await
            .expect_err("nested parent symlink");
        let root_error = cleanup
            .cleanup_root(&root_link)
            .await
            .expect_err("root symlink");

        assert!(matches!(
            parent_error,
            SitePersistenceResetError::UnsafeRoot(_)
        ));
        assert!(matches!(
            nested_parent_error,
            SitePersistenceResetError::UnsafeRoot(_)
        ));
        assert!(matches!(
            root_error,
            SitePersistenceResetError::UnsafeRoot(_)
        ));
        assert_eq!(
            fs::read_to_string(real_site.join("marker")).expect("marker"),
            "real"
        );
        assert!(fixture.site_root.join("site.yaml").is_file());
    }

    #[tokio::test]
    async fn permission_failure_runs_exact_fallback_then_retries_host_removal() {
        let fixture = Fixture::new();
        fixture.populate_site();
        let host = Arc::new(RecordingHostRemover::with_results([
            Err(io::ErrorKind::PermissionDenied),
            Ok(()),
        ]));
        let fallback = Arc::new(RecordingRootOwnedCleanup::removing());
        let cleanup = SitePersistenceReset::with_adapters(
            host.clone(),
            fallback.clone(),
            Arc::new(StaticMountInspector(None)),
        );

        cleanup
            .cleanup_root(&fixture.site_root)
            .await
            .expect("fallback cleanup");

        assert_eq!(host.calls.load(Ordering::Relaxed), 2);
        assert_eq!(
            fallback.calls.lock().expect("fallback calls").as_slice(),
            std::slice::from_ref(&fixture.site_root)
        );
        assert!(!fixture.site_root.exists());
    }

    #[tokio::test]
    async fn failed_host_retry_reports_the_concrete_error_and_leaves_no_fresh_tree() {
        let fixture = Fixture::new();
        fixture.populate_site();
        let cleanup = SitePersistenceReset::with_adapters(
            Arc::new(RecordingHostRemover::with_results([
                Err(io::ErrorKind::PermissionDenied),
                Err(io::ErrorKind::PermissionDenied),
            ])),
            Arc::new(RecordingRootOwnedCleanup::removing()),
            Arc::new(StaticMountInspector(None)),
        );

        let error = cleanup
            .cleanup_root(&fixture.site_root)
            .await
            .expect_err("host retry failure");

        let SitePersistenceResetError::HostRemoval(source) = error else {
            panic!("expected typed host-removal error");
        };
        assert!(source.chain().any(|cause| {
            cause
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::PermissionDenied)
        }));
        assert!(fixture.site_root.is_dir());
        assert!(fixture.site_snapshot().is_empty());
    }

    #[tokio::test]
    async fn fallback_failure_retains_the_exact_tree_and_typed_source() {
        let fixture = Fixture::new();
        fixture.populate_site();
        let before = fixture.site_snapshot();
        let fallback = Arc::new(RecordingRootOwnedCleanup::failing());
        let cleanup = SitePersistenceReset::with_adapters(
            Arc::new(RecordingHostRemover::with_results([Err(
                io::ErrorKind::PermissionDenied,
            )])),
            fallback,
            Arc::new(StaticMountInspector(None)),
        );

        let error = cleanup
            .cleanup_root(&fixture.site_root)
            .await
            .expect_err("fallback failure");

        assert!(matches!(
            error,
            SitePersistenceResetError::RootOwnedFallback { .. }
        ));
        assert!(format!("{error:#}").contains("injected root-owned cleanup failure"));
        assert_eq!(fixture.site_snapshot(), before);
    }

    #[tokio::test]
    async fn fallback_failure_stops_transaction_before_fresh_initialization() {
        use crate::{
            reset::transaction::{SiteResetTransaction, SiteResetTransactionError},
            test_support::initial_site_inputs,
        };

        let fixture = Fixture::new();
        fixture.populate_site();
        fs::write(fixture.site_root.join("site.yaml"), "siteId: site-test\n")
            .expect("valid retained site identity");
        let before = fixture.site_snapshot();
        let cleanup = SitePersistenceReset::with_adapters(
            Arc::new(RecordingHostRemover::with_results([Err(
                io::ErrorKind::PermissionDenied,
            )])),
            Arc::new(RecordingRootOwnedCleanup::failing()),
            Arc::new(StaticMountInspector(None)),
        );
        let transaction = SiteResetTransaction::new(
            Box::new(SuccessfulRuntimeCleanup),
            Box::new(cleanup),
            InitialSiteInputWriter::new(SiteInputBundleValidator::for_test()),
        );

        let error = transaction
            .reset(
                &fixture.site_root,
                crate::test_support::site_runtime_cleanups(),
                &initial_site_inputs(),
            )
            .await
            .expect_err("persistence fallback failure");

        assert!(matches!(
            error,
            SiteResetTransactionError::PersistenceCleanup(_)
        ));
        assert_eq!(fixture.site_snapshot(), before);
        assert_eq!(
            fs::read_to_string(&fixture.lock).expect("retained lock"),
            "stable lock"
        );
    }

    #[tokio::test]
    async fn absent_root_is_idempotent_without_host_or_docker_operations() {
        let fixture = Fixture::new();
        let host = Arc::new(RecordingHostRemover::default());
        let fallback = Arc::new(RecordingRootOwnedCleanup::default());
        let cleanup = SitePersistenceReset::with_adapters(
            host.clone(),
            fallback.clone(),
            Arc::new(StaticMountInspector(None)),
        );

        cleanup
            .cleanup_root(&fixture.site_root)
            .await
            .expect("absent root");

        assert_eq!(host.calls.load(Ordering::Relaxed), 0);
        assert!(fallback.calls.lock().expect("fallback calls").is_empty());
    }

    struct SuccessfulRuntimeCleanup;

    impl dotcore_runtime::SiteRuntimeCleanup for SuccessfulRuntimeCleanup {
        fn inventory<'a>(
            &'a self,
            _site_id: &'a str,
            _scope: &'a dotcore_runtime::SiteRuntimeScope,
        ) -> Pin<
            Box<
                dyn Future<Output = anyhow::Result<dotcore_runtime::SiteRuntimeInventory>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(std::future::ready(Ok(Default::default())))
        }

        fn cleanup<'a>(
            &'a self,
            _site_id: &'a str,
            _scope: &'a dotcore_runtime::SiteRuntimeScope,
        ) -> Pin<
            Box<
                dyn Future<Output = anyhow::Result<dotcore_runtime::SiteRuntimeCleanupReport>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(std::future::ready(Ok(Default::default())))
        }
    }

    struct StaticMountInspector(Option<PathBuf>);

    impl SiteMountInspector for StaticMountInspector {
        fn first_mount_at_or_beneath(&self, _site_root: &Path) -> anyhow::Result<Option<PathBuf>> {
            Ok(self.0.clone())
        }
    }

    #[derive(Default)]
    struct RecordingHostRemover {
        calls: AtomicUsize,
        results: Mutex<VecDeque<Result<(), io::ErrorKind>>>,
    }

    impl RecordingHostRemover {
        fn with_results(results: impl IntoIterator<Item = Result<(), io::ErrorKind>>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                results: Mutex::new(results.into_iter().collect()),
            }
        }
    }

    impl HostSiteRootRemover for RecordingHostRemover {
        fn remove(&self, site_root: &Path) -> io::Result<()> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            match self.results.lock().expect("host results").pop_front() {
                Some(Err(kind)) => Err(io::Error::from(kind)),
                Some(Ok(())) | None => fs::remove_dir_all(site_root),
            }
        }
    }

    #[derive(Default)]
    struct RecordingRootOwnedCleanup {
        calls: Mutex<Vec<PathBuf>>,
        behavior: RootOwnedBehavior,
    }

    impl RecordingRootOwnedCleanup {
        fn removing() -> Self {
            Self {
                behavior: RootOwnedBehavior::RemoveEntries,
                ..Self::default()
            }
        }

        fn failing() -> Self {
            Self {
                behavior: RootOwnedBehavior::Fail,
                ..Self::default()
            }
        }
    }

    impl RootOwnedSiteDataCleanup for RecordingRootOwnedCleanup {
        fn cleanup<'a>(&'a self, site_root: &'a Path) -> RootOwnedCleanupFuture<'a> {
            self.calls
                .lock()
                .expect("fallback calls")
                .push(site_root.to_owned());
            Box::pin(std::future::ready(match self.behavior {
                RootOwnedBehavior::Noop => Ok(()),
                RootOwnedBehavior::RemoveEntries => {
                    for entry in fs::read_dir(site_root).expect("fallback directory") {
                        let path = entry.expect("fallback entry").path();
                        if path.is_dir() {
                            fs::remove_dir_all(path).expect("fallback directory removal");
                        } else {
                            fs::remove_file(path).expect("fallback file removal");
                        }
                    }
                    Ok(())
                }
                RootOwnedBehavior::Fail => {
                    Err(anyhow::anyhow!("injected root-owned cleanup failure"))
                }
            }))
        }
    }

    #[derive(Clone, Copy, Default)]
    enum RootOwnedBehavior {
        #[default]
        Noop,
        RemoveEntries,
        Fail,
    }

    struct Fixture {
        root: PathBuf,
        site_root: PathBuf,
        lock: PathBuf,
        cache: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
            let root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
                "dotcore-site-persistence-reset-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).expect("fixture root");
            let site_root = root.join("nested").join("site");
            fs::create_dir_all(site_root.parent().expect("Site parent")).expect("Site parent");
            let lock = PathBuf::from(format!("{}.lock", site_root.display()));
            let cache = root.join("artifact-cache");
            fs::write(&lock, "stable lock").expect("lock file");
            fs::create_dir(&cache).expect("cache");
            fs::write(cache.join("artifact"), "cache bytes").expect("cache bytes");
            Self {
                root,
                site_root,
                lock,
                cache,
            }
        }

        fn populate_site(&self) {
            fs::create_dir_all(self.site_root.join("catalog/nested")).expect("Site tree");
            fs::create_dir_all(self.site_root.join("applications-data/service")).expect("data");
            for (relative, contents) in [
                ("site.yaml", "site"),
                ("node.yaml", "node"),
                ("state.pb", "state"),
                ("catalog/nested/declaration.yaml", "catalog"),
                ("applications-data/service/data", "application"),
                (".hidden", "hidden"),
                ("unknown", "unknown"),
            ] {
                fs::write(self.site_root.join(relative), contents).expect("Site file");
            }
        }

        fn site_snapshot(&self) -> Vec<(String, Vec<u8>)> {
            tree_snapshot(&self.site_root)
        }

        fn sibling_snapshot(&self) -> Vec<(String, Vec<u8>)> {
            vec![
                (
                    self.lock.to_string_lossy().into_owned(),
                    fs::read(&self.lock).expect("lock bytes"),
                ),
                (
                    self.cache.to_string_lossy().into_owned(),
                    fs::read(self.cache.join("artifact")).expect("cache bytes"),
                ),
            ]
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn tree_snapshot(root: &Path) -> Vec<(String, Vec<u8>)> {
        fn collect(root: &Path, path: &Path, entries: &mut Vec<(String, Vec<u8>)>) {
            if !path.exists() {
                return;
            }
            let mut children = fs::read_dir(path)
                .expect("snapshot directory")
                .map(|entry| entry.expect("snapshot entry").path())
                .collect::<Vec<_>>();
            children.sort();
            for child in children {
                if child.is_dir() {
                    collect(root, &child, entries);
                } else {
                    entries.push((
                        child
                            .strip_prefix(root)
                            .expect("relative snapshot")
                            .to_string_lossy()
                            .into_owned(),
                        fs::read(child).expect("snapshot file"),
                    ));
                }
            }
        }

        let mut entries = Vec::new();
        collect(root, root, &mut entries);
        entries
    }

    fn symlink_file(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).expect("file symlink");
    }

    fn symlink_directory(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).expect("directory symlink");
    }
}
