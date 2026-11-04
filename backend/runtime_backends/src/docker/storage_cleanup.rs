//! Machine cleanup of workload storage that expires once unused.
//!
//! The Docker runtime stamps the last use of every storage key that its owner marked as expiring
//! when unused. Cleanup lists the key folders on this machine whose stamp is older than a retention
//! period and that no container mounts, and removes the ones the operator confirmed after checking
//! both conditions again. Storage without a stamp, such as storage owned by a single deployment,
//! is never listed.

use std::{
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::Context as _;
use bollard::{Docker, query_parameters::ListContainersOptionsBuilder};
use dotcore_domain::CacheLayout;
use dotcore_runtime::WorkloadStorageKind;

use super::{site_data::remove_folder_tree, storage::LAST_USED_MARKER};

/// Name prefix of a key folder moved aside while it is removed.
const REMOVING_PREFIX: &str = ".removing-";

/// Plans and applies the removal of expired storage in a set of storage areas.
pub struct StorageCleanup {
    areas: Vec<StorageArea>,
    retention: Duration,
    mounts: Arc<dyn MountedFolders>,
}

impl StorageCleanup {
    /// Creates a cleanup of `areas` that expires storage unused for longer than `retention`.
    pub fn new(
        areas: Vec<StorageArea>,
        retention: Duration,
        mounts: Arc<dyn MountedFolders>,
    ) -> Self {
        Self {
            areas,
            retention,
            mounts,
        }
    }

    /// Lists the storage keys that expired at `now` and that no container mounts.
    pub async fn plan(&self, now: SystemTime) -> anyhow::Result<Vec<StorageRemoval>> {
        let mounted = self.mounts.mounted_sources().await?;
        let mut planned = Vec::new();
        for area in &self.areas {
            for removal in area.stamped_keys()? {
                if self.expired(removal.last_used, now) && !is_mounted(&removal.path, &mounted) {
                    planned.push(removal);
                }
            }
        }
        Ok(planned)
    }

    /// Removes each planned key that is still expired and unmounted at `now`, and reports what
    /// happened to every one of them.
    pub async fn apply(
        &self,
        planned: Vec<StorageRemoval>,
        now: SystemTime,
    ) -> Vec<(StorageRemoval, StorageRemovalOutcome)> {
        let mut results = Vec::with_capacity(planned.len());
        for removal in planned {
            let outcome = self.remove(&removal, now).await;
            results.push((removal, outcome));
        }
        results
    }

    async fn remove(&self, removal: &StorageRemoval, now: SystemTime) -> StorageRemovalOutcome {
        match last_used(&removal.path) {
            Ok(Some(last_used)) if !self.expired(last_used, now) => {
                return StorageRemovalOutcome::UsedSincePlanning;
            }
            Ok(Some(_)) => {}
            Ok(None) => return StorageRemovalOutcome::Removed,
            Err(error) => return StorageRemovalOutcome::Failed(format!("{error:#}")),
        }
        match self.mounts.mounted_sources().await {
            Ok(mounted) if is_mounted(&removal.path, &mounted) => {
                return StorageRemovalOutcome::InUse;
            }
            Ok(_) => {}
            Err(error) => return StorageRemovalOutcome::Failed(format!("{error:#}")),
        }

        // A workload starting from here on finds the key folder gone and gets a new empty one,
        // instead of a folder that is half removed.
        let removing = removing_path(&removal.path);
        if removing != removal.path
            && let Err(error) = std::fs::rename(&removal.path, &removing)
        {
            return StorageRemovalOutcome::Failed(format!(
                "move {} aside: {error}",
                removal.path.display()
            ));
        }
        match remove_folder_tree(&removing).await {
            Ok(()) => StorageRemovalOutcome::Removed,
            Err(error) => StorageRemovalOutcome::Failed(format!("{error:#}")),
        }
    }

    fn expired(&self, last_used: SystemTime, now: SystemTime) -> bool {
        now.duration_since(last_used)
            .is_ok_and(|unused| unused > self.retention)
    }
}

/// One folder holding storage keys as `<namespace>/<key>`, with the label shown for its keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageArea {
    pub kind: WorkloadStorageKind,
    pub label: String,
    pub root: PathBuf,
}

impl StorageArea {
    /// Returns the storage areas of this machine that cleanup can reach: every site's workload
    /// caches below `cache_root`, the development application data, and the application data of
    /// `site_root` when given. Absent folders hold nothing and are left out.
    pub fn on_machine(cache_root: &Path, site_root: Option<&Path>) -> anyhow::Result<Vec<Self>> {
        let cache = CacheLayout(cache_root);
        let mut areas = Vec::new();
        for entry in read_dir_if_present(&cache.workloads())? {
            let entry = entry.context("read workload cache root")?;
            let Some(site) = visible_name(&entry.file_name()) else {
                continue;
            };
            if entry.file_type()?.is_dir() {
                areas.push(Self::resolved(
                    WorkloadStorageKind::Cache,
                    site,
                    &entry.path(),
                )?);
            }
        }
        let data = [
            Some(("development", cache.development_applications())),
            site_root.map(|root| ("site", root.join("applications-data"))),
        ];
        for (label, root) in data.into_iter().flatten() {
            if root.is_dir() {
                areas.push(Self::resolved(
                    WorkloadStorageKind::Data,
                    label.to_owned(),
                    &root,
                )?);
            }
        }
        Ok(areas)
    }

    /// Resolves the area's root the way the runtime resolves storage roots, so key folders
    /// compare equal to the mount sources it gave Docker.
    fn resolved(kind: WorkloadStorageKind, label: String, root: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            kind,
            label,
            root: root
                .canonicalize()
                .with_context(|| format!("resolve {}", root.display()))?,
        })
    }

    /// Returns every key folder in this area that records its last use.
    fn stamped_keys(&self) -> anyhow::Result<Vec<StorageRemoval>> {
        let mut keys = Vec::new();
        for namespace in read_dir_if_present(&self.root)? {
            let namespace = namespace.with_context(|| format!("read {}", self.root.display()))?;
            let Some(namespace_name) = visible_name(&namespace.file_name()) else {
                continue;
            };
            if !namespace.file_type()?.is_dir() {
                continue;
            }
            for key in read_dir_if_present(&namespace.path())? {
                let key = key.with_context(|| format!("read {}", namespace.path().display()))?;
                // A key folder left aside by an interrupted removal is listed again to finish it.
                let Some(key_name) = key
                    .file_name()
                    .to_str()
                    .filter(|name| !name.starts_with('.') || name.starts_with(REMOVING_PREFIX))
                    .map(str::to_owned)
                else {
                    continue;
                };
                if !key.file_type()?.is_dir() {
                    continue;
                }
                let path = key.path();
                if let Some(last_used) = last_used(&path)? {
                    keys.push(StorageRemoval {
                        kind: self.kind,
                        area: self.label.clone(),
                        namespace: namespace_name.clone(),
                        key: key_name,
                        path,
                        last_used,
                    });
                }
            }
        }
        keys.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(keys)
    }
}

/// One storage key folder that cleanup lists for removal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageRemoval {
    pub kind: WorkloadStorageKind,
    pub area: String,
    pub namespace: String,
    pub key: String,
    pub path: PathBuf,
    pub last_used: SystemTime,
}

/// What happened to one planned storage removal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StorageRemovalOutcome {
    Removed,

    /// A container mounted the storage after planning, so it was kept.
    InUse,

    /// A workload used the storage after planning, so it was kept.
    UsedSincePlanning,

    /// The folder could not be removed.
    Failed(String),
}

/// Host folders that containers on this machine mount.
pub trait MountedFolders: Send + Sync {
    /// Returns the host source of every mount of every container, in any state.
    fn mounted_sources(&self) -> MountedSourcesFuture<'_>;
}

/// The future returned by [`MountedFolders::mounted_sources`].
pub type MountedSourcesFuture<'a> =
    Pin<Box<dyn Future<Output = anyhow::Result<Vec<PathBuf>>> + Send + 'a>>;

/// Mounted folders read from the local Docker daemon, across every container rather than only
/// Dotcore's, since any container may use a storage folder.
pub struct DockerMountedFolders {
    docker: Docker,
}

impl DockerMountedFolders {
    /// Connects to the local Docker daemon.
    pub async fn connect() -> anyhow::Result<Self> {
        let docker = Docker::connect_with_unix(
            &dotcore_domain::docker::local_endpoint(),
            dotcore_domain::docker::DOCKER_CLIENT_TIMEOUT_SECONDS,
            bollard::API_DEFAULT_VERSION,
        )
        .context("connect to Docker")?
        .negotiate_version()
        .await
        .context("negotiate the Docker API version")?;
        Ok(Self { docker })
    }
}

impl MountedFolders for DockerMountedFolders {
    fn mounted_sources(&self) -> MountedSourcesFuture<'_> {
        Box::pin(async move {
            let options = ListContainersOptionsBuilder::default().all(true).build();
            let containers = self
                .docker
                .list_containers(Some(options))
                .await
                .context("list Docker containers")?;
            Ok(containers
                .into_iter()
                .flat_map(|container| container.mounts.unwrap_or_default())
                .filter_map(|mount| mount.source.map(PathBuf::from))
                .collect())
        })
    }
}

/// Returns the last use recorded in a key folder, or `None` when it records none.
fn last_used(key_folder: &Path) -> anyhow::Result<Option<SystemTime>> {
    match std::fs::metadata(key_folder.join(LAST_USED_MARKER)) {
        Ok(metadata) => Ok(Some(metadata.modified().with_context(|| {
            format!("read the last use of {}", key_folder.display())
        })?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("read the last use of {}", key_folder.display()))
        }
    }
}

/// Tells whether any mount source is the key folder or a folder below it.
fn is_mounted(key_folder: &Path, mounted: &[PathBuf]) -> bool {
    mounted.iter().any(|source| source.starts_with(key_folder))
}

/// Returns the hidden sibling a key folder moves to while it is removed; a folder already aside
/// stays where it is.
fn removing_path(key_folder: &Path) -> PathBuf {
    let name = key_folder
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name.starts_with(REMOVING_PREFIX) {
        key_folder.to_owned()
    } else {
        key_folder.with_file_name(format!("{REMOVING_PREFIX}{name}"))
    }
}

/// Returns a directory entry's name unless it is hidden or not valid UTF-8.
fn visible_name(name: &std::ffi::OsStr) -> Option<String> {
    name.to_str()
        .filter(|name| !name.starts_with('.'))
        .map(str::to_owned)
}

fn read_dir_if_present(
    folder: &Path,
) -> anyhow::Result<Box<dyn Iterator<Item = io::Result<std::fs::DirEntry>>>> {
    match std::fs::read_dir(folder) {
        Ok(entries) => Ok(Box::new(entries)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Box::new(std::iter::empty())),
        Err(error) => Err(error).with_context(|| format!("read {}", folder.display())),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    #[tokio::test]
    async fn plans_only_stamped_unmounted_keys_unused_past_the_retention() {
        let machine = Machine::new();
        let expired = machine.cache_key("site-a", "engine-artifact", "build-one", 9);
        let fresh = machine.cache_key("site-a", "engine-artifact", "build-two", 1);
        let mounted = machine.cache_key("site-b", "engine-artifact", "build-one", 9);
        machine.unstamped_key("workload-cache/v2/site-a/deployment/model-one");
        let development = machine.data_key("development-applications", "application", "app-one", 9);
        let site = machine.data_key("site/applications-data", "application", "app-two", 9);
        let mounts = FakeMounts::with([mounted.join("engine-cache")]);

        let planned = machine
            .cleanup(&mounts)
            .plan(machine.now)
            .await
            .expect("plan");

        assert_eq!(
            planned,
            [
                machine.removal(
                    WorkloadStorageKind::Cache,
                    "site-a",
                    "engine-artifact",
                    &expired
                ),
                machine.removal(
                    WorkloadStorageKind::Data,
                    "development",
                    "application",
                    &development
                ),
                machine.removal(WorkloadStorageKind::Data, "site", "application", &site),
            ]
        );
        assert!(fresh.is_dir());
    }

    #[tokio::test]
    async fn applies_only_removals_still_expired_and_unmounted() {
        let machine = Machine::new();
        let removed = machine.cache_key("site-a", "engine-artifact", "build-one", 9);
        let used = machine.cache_key("site-a", "engine-artifact", "build-two", 9);
        let mounted = machine.data_key("site/applications-data", "application", "app-one", 9);
        let mounts = FakeMounts::with([]);
        let cleanup = machine.cleanup(&mounts);
        let planned = cleanup.plan(machine.now).await.expect("plan");
        assert_eq!(planned.len(), 3);

        stamp(&used, machine.now);
        mounts.set([mounted.join("models")]);
        let results = cleanup.apply(planned, machine.now).await;

        assert_eq!(
            results
                .iter()
                .map(|(removal, outcome)| (removal.path.clone(), outcome.clone()))
                .collect::<Vec<_>>(),
            [
                (removed.clone(), StorageRemovalOutcome::Removed),
                (used.clone(), StorageRemovalOutcome::UsedSincePlanning),
                (mounted.clone(), StorageRemovalOutcome::InUse),
            ]
        );
        assert!(!removed.exists());
        assert!(!removing_path(&removed).exists());
        assert!(used.join("engine-cache/entry").is_file());
        assert!(mounted.join("models/entry").is_file());
    }

    #[tokio::test]
    async fn finishes_a_removal_left_aside() {
        let machine = Machine::new();
        let key = machine.cache_key("site-a", "engine-artifact", "build-one", 9);
        let aside = removing_path(&key);
        std::fs::rename(&key, &aside).expect("move aside");
        let mounts = FakeMounts::with([]);
        let cleanup = machine.cleanup(&mounts);

        let planned = cleanup.plan(machine.now).await.expect("plan");
        assert_eq!(
            planned
                .iter()
                .map(|removal| &removal.path)
                .collect::<Vec<_>>(),
            [&aside]
        );
        let results = cleanup.apply(planned, machine.now).await;

        assert_eq!(results[0].1, StorageRemovalOutcome::Removed);
        assert!(!aside.exists());
    }

    #[tokio::test]
    #[ignore = "hardware: just backend::test-docker"]
    async fn hw_docker_removes_root_owned_expired_storage_and_keeps_storage_a_container_mounts() {
        let machine = Machine::new();
        let root_owned = machine.cache_key("site-a", "engine-artifact", "build-one", 9);
        let mounted = machine.data_key("site/applications-data", "application", "app-one", 9);
        docker(&[
            "run",
            "--rm",
            "--network",
            "none",
            "--volume",
            &format!("{}:/cache", root_owned.join("engine-cache").display()),
            super::super::SITE_DATA_CLEANUP_IMAGE,
            "sh",
            "-c",
            "mkdir /cache/private && echo compiled > /cache/private/entry && chmod 700 /cache/private",
        ]);
        // Docker Desktop's file sharing maps container-root files to the host user, so only native
        // Linux bind mounts leave content this process cannot delete.
        if cfg!(target_os = "linux") {
            assert!(
                std::fs::read_dir(root_owned.join("engine-cache/private")).is_err(),
                "this process cannot read root-owned content"
            );
        }
        let container = format!("dotcore-storage-cleanup-test-{}", std::process::id());
        docker(&[
            "create",
            "--name",
            &container,
            "--network",
            "none",
            "--volume",
            &format!("{}:/models", mounted.join("models").display()),
            super::super::SITE_DATA_CLEANUP_IMAGE,
            "true",
        ]);
        let mounts = Arc::new(DockerMountedFolders::connect().await.expect("Docker"));
        let areas = StorageArea::on_machine(&machine.root, Some(&machine.root.join("site")))
            .expect("areas");
        let cleanup = StorageCleanup::new(areas, 7 * DAY, mounts);

        let planned = cleanup.plan(machine.now).await;
        let results = match planned {
            Ok(planned) => Ok(cleanup.apply(planned, machine.now).await),
            Err(error) => Err(error),
        };
        docker(&["rm", &container]);

        assert_eq!(
            results
                .expect("plan")
                .into_iter()
                .map(|(removal, outcome)| (removal.path, outcome))
                .collect::<Vec<_>>(),
            [(root_owned.clone(), StorageRemovalOutcome::Removed)]
        );
        assert!(!root_owned.exists());
        assert!(mounted.join("models/entry").is_file());
    }

    fn docker(arguments: &[&str]) {
        let status = std::process::Command::new("docker")
            .args(arguments)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("docker");
        assert!(status.success(), "docker {arguments:?}");
    }

    /// A machine's cache root and site folder in a temporary directory, with a fixed clock.
    struct Machine {
        _directory: tempfile::TempDir,
        root: PathBuf,
        now: SystemTime,
    }

    impl Machine {
        fn new() -> Self {
            let directory =
                tempfile::tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
                    .expect("machine root");
            let root = directory.path().to_owned();
            Self {
                _directory: directory,
                root,
                now: SystemTime::UNIX_EPOCH + 100 * DAY,
            }
        }

        fn cleanup(&self, mounts: &Arc<FakeMounts>) -> StorageCleanup {
            let areas =
                StorageArea::on_machine(&self.root, Some(&self.root.join("site"))).expect("areas");
            StorageCleanup::new(areas, 7 * DAY, mounts.clone())
        }

        fn cache_key(&self, site: &str, namespace: &str, key: &str, unused_days: u32) -> PathBuf {
            self.stamped_key(
                &format!("workload-cache/v2/{site}/{namespace}/{key}"),
                "engine-cache",
                unused_days,
            )
        }

        fn data_key(&self, area: &str, namespace: &str, key: &str, unused_days: u32) -> PathBuf {
            self.stamped_key(&format!("{area}/{namespace}/{key}"), "models", unused_days)
        }

        fn stamped_key(&self, relative: &str, name: &str, unused_days: u32) -> PathBuf {
            let key = self.unstamped_key(relative);
            std::fs::create_dir_all(key.join(name)).expect("storage folder");
            std::fs::write(key.join(name).join("entry"), "content").expect("storage file");
            stamp(&key, self.now - unused_days * DAY);
            key
        }

        fn unstamped_key(&self, relative: &str) -> PathBuf {
            let key = self.root.join(relative);
            std::fs::create_dir_all(&key).expect("key folder");
            key
        }

        fn removal(
            &self,
            kind: WorkloadStorageKind,
            area: &str,
            namespace: &str,
            key: &Path,
        ) -> StorageRemoval {
            StorageRemoval {
                kind,
                area: area.to_owned(),
                namespace: namespace.to_owned(),
                key: key
                    .file_name()
                    .and_then(|name| name.to_str())
                    .expect("key name")
                    .to_owned(),
                path: key.to_owned(),
                last_used: last_used(key).expect("read").expect("stamped"),
            }
        }
    }

    /// Mount sources that tests change between planning and applying.
    struct FakeMounts(Mutex<Vec<PathBuf>>);

    impl FakeMounts {
        fn with<const N: usize>(sources: [PathBuf; N]) -> Arc<Self> {
            Arc::new(Self(Mutex::new(sources.to_vec())))
        }

        fn set<const N: usize>(&self, sources: [PathBuf; N]) {
            *self.0.lock().expect("mounts") = sources.to_vec();
        }
    }

    impl MountedFolders for FakeMounts {
        fn mounted_sources(&self) -> MountedSourcesFuture<'_> {
            let sources = self.0.lock().expect("mounts").clone();
            Box::pin(async move { Ok(sources) })
        }
    }

    fn stamp(key: &Path, at: SystemTime) {
        std::fs::File::options()
            .create(true)
            .append(true)
            .open(key.join(LAST_USED_MARKER))
            .and_then(|file| file.set_modified(at))
            .expect("stamp");
    }
}
