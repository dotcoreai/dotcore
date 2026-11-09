//! Node-local storage folders that the Docker runtime provisions and mounts into workloads.
//!
//! Each attachment lives at a fixed host folder chosen from its kind, namespace, key and name,
//! so every workload naming the same four values binds the same contents and a recreated
//! container finds them again. Caches live below the site's folder in the workload cache root and
//! data lives below the application data root, both as `<namespace>/<key>/<name>`. Folders are
//! created before each container create and start, and symlinks or other escapes from the root
//! are rejected. On restart, the folders to recreate are read back from the container's own bind
//! mounts.
//!
//! Storage that expires when unused carries a last-use marker in its key folder, refreshed each
//! time a workload is created or started with it, so machine cleanup can tell how long it has
//! been unused.

use std::path::{Component, Path, PathBuf};

use dotcore_domain::real_time;
use dotcore_runtime::{
    RuntimeBackendError, WorkloadStorageAttachment, WorkloadStorageKey, WorkloadStorageKind,
};
use tracing::debug;

use super::{
    adapter::DockerRuntimeConfig, runtime_error, runtime_error_with_source,
    validation::validate_name,
};

/// Name of the file in a key folder whose modification time records the storage's last use.
///
/// Storage names never start with a dot, so the marker never collides with a storage folder.
pub(super) const LAST_USED_MARKER: &str = ".last-used";

/// One host storage folder, identified by the attachment parts that choose its path, and whether
/// its key folder records its last use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StorageFolder {
    pub(super) kind: WorkloadStorageKind,
    pub(super) namespace: String,
    pub(super) key: String,
    pub(super) name: String,
    pub(super) expires_when_unused: bool,
}

impl From<&WorkloadStorageAttachment> for StorageFolder {
    fn from(storage: &WorkloadStorageAttachment) -> Self {
        Self {
            kind: storage.kind,
            namespace: storage.namespace.clone(),
            key: storage.key.clone(),
            name: storage.name.clone(),
            expires_when_unused: storage.expires_when_unused,
        }
    }
}

/// Returns the host folder of one storage attachment, below the resolved root that escape checks
/// also use, so the folder Docker mounts is the folder that was checked.
pub(super) fn storage_path(
    config: &DockerRuntimeConfig,
    folder: &StorageFolder,
) -> Result<PathBuf, RuntimeBackendError> {
    Ok(storage_root(config, folder.kind)?
        .join(&folder.namespace)
        .join(&folder.key)
        .join(&folder.name))
}

/// Returns the host folder holding every attachment of one owner key.
pub(super) fn storage_key_path(
    config: &DockerRuntimeConfig,
    storage: &WorkloadStorageKey,
) -> Result<PathBuf, RuntimeBackendError> {
    if !(safe_part(&storage.namespace) && safe_part(&storage.key)) {
        return Err(runtime_error(
            "Docker runtime rejected an unsafe storage key",
        ));
    }
    Ok(storage_root(config, storage.kind)?
        .join(&storage.namespace)
        .join(&storage.key))
}

/// Rejects attachment parts that are not one safe host path component each.
pub(super) fn validate_storage(
    storage: &WorkloadStorageAttachment,
) -> Result<(), RuntimeBackendError> {
    if [&storage.name, &storage.namespace, &storage.key]
        .into_iter()
        .all(|part| safe_part(part))
    {
        Ok(())
    } else {
        Err(runtime_error(
            "Docker runtime rejected an unsafe storage attachment",
        ))
    }
}

/// Returns the storage folders among bind-mount sources that sit at this runtime's storage layout.
/// A folder records its last use when its key folder already has a marker, since bind mounts do
/// not carry the owner's choice.
pub(super) fn retained_storage_folders(
    config: &DockerRuntimeConfig,
    bind_sources: &[PathBuf],
) -> Result<Vec<StorageFolder>, RuntimeBackendError> {
    let roots = [WorkloadStorageKind::Cache, WorkloadStorageKind::Data]
        .into_iter()
        .map(|kind| Ok((kind, storage_root(config, kind)?)))
        .collect::<Result<Vec<_>, RuntimeBackendError>>()?;
    Ok(bind_sources
        .iter()
        .filter_map(|source| {
            roots
                .iter()
                .find_map(|(kind, root)| folder_below(*kind, root, source))
        })
        .collect())
}

/// Creates every storage folder, rejecting paths that escape their root, and stamps the last use
/// of each folder that expires when unused.
pub(super) fn materialize_storage(
    config: &DockerRuntimeConfig,
    folders: impl IntoIterator<Item = StorageFolder>,
) -> Result<(), RuntimeBackendError> {
    for folder in folders {
        let mut path = storage_root(config, folder.kind)?;
        for part in [&folder.namespace, &folder.key, &folder.name] {
            path.push(part);
            reject_unsafe_component(&path)?;
        }
        std::fs::create_dir_all(&path).map_err(|source| {
            runtime_error_with_source("Docker runtime could not create storage directory", source)
        })?;
        let resolved = path.canonicalize().map_err(|source| {
            runtime_error_with_source("Docker runtime could not resolve storage directory", source)
        })?;
        if resolved != path {
            return Err(runtime_error(
                "Docker runtime rejected storage outside its root",
            ));
        }
        if folder.expires_when_unused {
            let marker = resolved
                .parent()
                .ok_or_else(|| runtime_error("Docker runtime storage folder has no key folder"))?
                .join(LAST_USED_MARKER);
            stamp_last_use(&marker)?;
        }
        debug!(
            event_kind = "workload_storage_prepared",
            kind = folder.kind.as_str(),
            namespace = folder.namespace,
            key = folder.key,
            storage = folder.name,
            path = %path.display(),
            "workload storage directory prepared"
        );
    }
    Ok(())
}

/// Resolves the kind's root; a cache root is this site's folder below the workload cache root.
fn storage_root(
    config: &DockerRuntimeConfig,
    kind: WorkloadStorageKind,
) -> Result<PathBuf, RuntimeBackendError> {
    let base = match kind {
        WorkloadStorageKind::Cache => &config.workload_cache_root,
        WorkloadStorageKind::Data => &config.application_data_root,
    };
    let base = base.canonicalize().map_err(|source| {
        runtime_error_with_source("Docker runtime storage root is unavailable", source)
    })?;
    Ok(match kind {
        WorkloadStorageKind::Cache => {
            let site = base.join(&config.site_id);
            reject_unsafe_component(&site)?;
            site
        }
        WorkloadStorageKind::Data => base,
    })
}

fn folder_below(kind: WorkloadStorageKind, root: &Path, source: &Path) -> Option<StorageFolder> {
    let mut parts = source
        .strip_prefix(root)
        .ok()?
        .components()
        .map(|part| match part {
            Component::Normal(part) => part.to_str(),
            _ => None,
        });
    let (Some(Some(namespace)), Some(Some(key)), Some(Some(name)), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    [namespace, key, name]
        .into_iter()
        .all(safe_part)
        .then(|| StorageFolder {
            kind,
            namespace: namespace.to_owned(),
            key: key.to_owned(),
            name: name.to_owned(),
            expires_when_unused: root
                .join(namespace)
                .join(key)
                .join(LAST_USED_MARKER)
                .is_file(),
        })
}

/// Creates the last-use marker, or moves its modification time to now.
fn stamp_last_use(marker: &Path) -> Result<(), RuntimeBackendError> {
    std::fs::File::options()
        .create(true)
        .append(true)
        .open(marker)
        .and_then(|file| file.set_modified(real_time::wall_now()))
        .map_err(|source| {
            runtime_error_with_source("Docker runtime could not stamp storage last use", source)
        })
}

fn safe_part(part: &str) -> bool {
    validate_name(part, "storage attachment").is_ok() && !matches!(part, "." | "..")
}

fn reject_unsafe_component(path: &Path) -> Result<(), RuntimeBackendError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => Err(
            runtime_error("Docker runtime rejected unsafe storage path component"),
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(runtime_error_with_source(
            "Docker runtime could not inspect storage path component",
            source,
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use crate::docker::{DockerAcceleratorTransport, configuration::DockerLogSourcePermits};

    #[test]
    fn places_each_kind_at_its_exact_layout_and_shares_identical_folders() {
        let root = root("layout");
        let config = config(root.clone());
        let cache = folder(
            WorkloadStorageKind::Cache,
            "deployment",
            "deployment-a",
            "engine-cache",
        );
        let data = folder(
            WorkloadStorageKind::Data,
            "deployment",
            "deployment-a",
            "engine-cache",
        );
        let path = |folder: &StorageFolder| storage_path(&config, folder).expect("storage path");

        assert_eq!(
            path(&cache),
            root.join("cache/site-a/deployment/deployment-a/engine-cache")
        );
        assert_eq!(
            path(&data),
            root.join("data/deployment/deployment-a/engine-cache")
        );
        assert_eq!(path(&cache), path(&cache.clone()));
        for other in [
            folder(
                WorkloadStorageKind::Cache,
                "application",
                "deployment-a",
                "engine-cache",
            ),
            folder(
                WorkloadStorageKind::Cache,
                "deployment",
                "deployment-b",
                "engine-cache",
            ),
            folder(
                WorkloadStorageKind::Cache,
                "deployment",
                "deployment-a",
                "other-cache",
            ),
        ] {
            assert_ne!(path(&other), path(&cache));
        }
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn mounts_the_resolved_folder_below_a_symlinked_root() {
        let root = root("linked");
        let linked = root.with_extension("link");
        let _ = std::fs::remove_file(&linked);
        std::os::unix::fs::symlink(&root, &linked).expect("root link");
        let config = config(linked.clone());
        let data = folder(
            WorkloadStorageKind::Data,
            "application",
            "open-webui",
            "web-data",
        );

        materialize_storage(&config, [data.clone()]).expect("create");
        let source = storage_path(&config, &data).expect("storage path");

        assert_eq!(source, root.join("data/application/open-webui/web-data"));
        assert!(source.is_dir());
        assert_eq!(
            retained_storage_folders(&config, std::slice::from_ref(&source)).expect("read back"),
            [data]
        );
        std::fs::remove_file(linked).expect("cleanup link");
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn creates_stable_folders_and_recreates_deleted_ones() {
        let root = root("stable");
        let config = config(root.clone());
        let cache = folder(
            WorkloadStorageKind::Cache,
            "deployment",
            "deployment-a",
            "engine-cache",
        );
        let data = folder(
            WorkloadStorageKind::Data,
            "application",
            "open-webui",
            "web-data",
        );

        materialize_storage(&config, [cache.clone(), data.clone()]).expect("create");
        let cache_path = root.join("cache/site-a/deployment/deployment-a/engine-cache");
        let data_path = root.join("data/application/open-webui/web-data");
        assert!(cache_path.is_dir());
        assert!(data_path.is_dir());

        std::fs::remove_dir_all(&cache_path).expect("delete cache");
        materialize_storage(&config, [cache]).expect("recreate");
        assert!(cache_path.is_dir());
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn stamps_the_last_use_of_expiring_storage_only_and_reads_it_back() {
        let root = root("last-use");
        let config = config(root.clone());
        let shared = StorageFolder {
            expires_when_unused: true,
            ..folder(
                WorkloadStorageKind::Data,
                "application",
                "open-webui",
                "web-data",
            )
        };
        let owned = folder(
            WorkloadStorageKind::Cache,
            "deployment",
            "deployment-a",
            "engine-cache",
        );
        let marker = root
            .join("data/application/open-webui")
            .join(LAST_USED_MARKER);

        materialize_storage(&config, [shared.clone(), owned.clone()]).expect("create");
        assert!(marker.is_file());
        assert!(
            !root
                .join("cache/site-a/deployment/deployment-a")
                .join(LAST_USED_MARKER)
                .exists()
        );

        let earlier = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        std::fs::File::options()
            .append(true)
            .open(&marker)
            .and_then(|file| file.set_modified(earlier))
            .expect("age marker");
        materialize_storage(&config, [shared.clone()]).expect("start");
        let stamped = std::fs::metadata(&marker)
            .and_then(|metadata| metadata.modified())
            .expect("marker time");
        assert!(stamped > earlier);

        let sources = [
            root.join("data/application/open-webui/web-data"),
            root.join("cache/site-a/deployment/deployment-a/engine-cache"),
        ];
        assert_eq!(
            retained_storage_folders(&config, &sources).expect("read back"),
            [shared, owned]
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn reads_back_only_folders_at_the_storage_layout() {
        let root = root("names");
        let config = config(root.clone());
        let sources = [
            root.join("cache/site-a/deployment/deployment-a/engine-cache"),
            root.join("data/application/open-webui/web-data"),
            root.join("cache/site-b/deployment/deployment-a/engine-cache"),
            root.join("cache/site-a/deployment/deployment-a/nested/engine-cache"),
            root.join("cache/site-a/deployment/engine-cache"),
            PathBuf::from("/artifacts/model-snapshot"),
        ];

        assert_eq!(
            retained_storage_folders(&config, &sources).expect("read back"),
            [
                folder(
                    WorkloadStorageKind::Cache,
                    "deployment",
                    "deployment-a",
                    "engine-cache"
                ),
                folder(
                    WorkloadStorageKind::Data,
                    "application",
                    "open-webui",
                    "web-data"
                ),
            ]
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn rejects_unsafe_parts_and_symlinks_that_escape_the_root() {
        let root = root("symlink");
        let config = config(root.clone());
        let outside = root.with_extension("outside");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::create_dir_all(root.join("cache/site-a")).expect("site folder");
        std::os::unix::fs::symlink(&outside, root.join("cache/site-a/deployment"))
            .expect("escape link");

        let error = materialize_storage(
            &config,
            [folder(
                WorkloadStorageKind::Cache,
                "deployment",
                "deployment-a",
                "engine-cache",
            )],
        )
        .expect_err("escaping symlink");

        assert_eq!(
            error.to_string(),
            "Invalid runtime launch: Docker runtime rejected unsafe storage path component"
        );
        assert!(!outside.join("deployment-a").exists());
        for (namespace, key, name) in [
            ("..", "key", "name"),
            ("namespace", ".", "name"),
            ("namespace", "key", "a/b"),
            ("", "key", "name"),
        ] {
            let storage = WorkloadStorageAttachment {
                name: name.to_owned(),
                destination: "/cache".to_owned(),
                kind: WorkloadStorageKind::Cache,
                namespace: namespace.to_owned(),
                key: key.to_owned(),
                read_only: false,
                expires_when_unused: false,
            };
            assert_eq!(
                validate_storage(&storage)
                    .expect_err("unsafe part")
                    .to_string(),
                "Invalid runtime launch: Docker runtime rejected an unsafe storage attachment"
            );
        }
        std::fs::remove_dir_all(root).expect("cleanup");
        std::fs::remove_dir_all(outside).expect("cleanup");
    }

    fn folder(kind: WorkloadStorageKind, namespace: &str, key: &str, name: &str) -> StorageFolder {
        StorageFolder {
            kind,
            namespace: namespace.to_owned(),
            key: key.to_owned(),
            name: name.to_owned(),
            expires_when_unused: false,
        }
    }

    fn root(name: &str) -> PathBuf {
        let root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
            "dotcore-workload-storage-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("cache")).expect("cache root");
        std::fs::create_dir_all(root.join("data")).expect("data root");
        root.canonicalize().expect("canonical root")
    }

    fn config(root: PathBuf) -> DockerRuntimeConfig {
        DockerRuntimeConfig {
            site_id: "site-a".to_owned(),
            execution_target_id: "target-a".to_owned(),
            approved_mount_root: dotcore_domain::test_support::canonical_temp_dir(),
            application_data_root: root.join("data"),
            workload_cache_root: root.join("cache"),
            accelerator_transport: DockerAcceleratorTransport::Cdi,
            source_stream_permits: DockerLogSourcePermits::new(),
        }
    }
}
