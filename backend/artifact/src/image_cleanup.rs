//! Reclaims the Docker image disk that no Docker-delivered Nix artifact on the machine uses: the
//! names loaders applied whose loader no Nix root keeps, the upstream digest references left on
//! such images, and untagged local builds of Dockerfile recipes.
//!
//! The Nix roots of every site, node, and developer on the machine act as its reference count, so
//! one daemon shared by several sites stays safe: an image keeps every name a rooted loader
//! needs, and an image any container uses keeps all of its names. Names are removed one at a
//! time and never forced, so Docker frees an image's bytes only once its last name is gone.
//! Cleanup waits for image loads in progress and holds them off while it removes names.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use tokio_util::sync::CancellationToken;
use tracing::info;

use super::{
    ArtifactCacheError,
    docker_image::{CLEANUP_LOCK_PATH, METADATA_PATH, RECIPE_LABEL},
    model::acquire_lock,
    nix::NixCommand,
};

/// Names every Docker-delivered Nix artifact's image carries, launch and source tags alike.
const LOADER_NAME_PREFIX: &str = "dotcore-nix/";

/// Longest wait for image loads in progress to finish before cleanup gives up.
const LOCK_WAIT: Duration = Duration::from_secs(30);

/// Docker image cleanup over the machine's daemon and Nix roots.
pub struct ImageCleanup {
    docker: Arc<dyn ImageCleanupDocker>,
    roots: Arc<dyn NixRootScan>,
    lock: PathBuf,
    store_root: PathBuf,
    name_prefix: String,
    lock_wait: Duration,
}

impl ImageCleanup {
    /// Cleans up through `docker`, coordinating with the image loads of the artifact cache at
    /// `cache_root` and reading the host's Nix roots.
    pub fn new(cache_root: &Path, docker: Arc<dyn ImageCleanupDocker>) -> Self {
        Self {
            docker,
            roots: Arc::new(CommandNixRootScan {
                command: NixCommand::Discover,
            }),
            lock: cache_root.join(CLEANUP_LOCK_PATH),
            store_root: PathBuf::from("/"),
            name_prefix: LOADER_NAME_PREFIX.to_owned(),
            lock_wait: LOCK_WAIT,
        }
    }

    /// Returns what cleanup would remove now, without removing anything.
    pub async fn plan(&self) -> Result<Vec<ImageRemoval>, ImageCleanupError> {
        let keep = self.keep().await?;
        let images = self
            .docker
            .images()
            .await
            .map_err(ImageCleanupError::Docker)?;
        let in_use = self
            .docker
            .container_images()
            .await
            .map_err(ImageCleanupError::Docker)?;
        Ok(images
            .iter()
            .filter(|image| !in_use.contains(&image.id))
            .flat_map(|image| self.removals(image, &keep))
            .collect())
    }

    /// Removes what [`Self::plan`] finds, while no image load runs, and returns each removal with
    /// its outcome. Images a container started using since planning keep their names.
    pub async fn apply(&self) -> Result<Vec<(ImageRemoval, RemovalOutcome)>, ImageCleanupError> {
        let _lock = tokio::time::timeout(
            self.lock_wait,
            acquire_lock(self.lock.clone(), &CancellationToken::new(), None),
        )
        .await
        .map_err(|_| ImageCleanupError::LoadsInProgress)?
        .map_err(ImageCleanupError::Lock)?;
        let removals = self.plan().await?;
        let in_use = self
            .docker
            .container_images()
            .await
            .map_err(ImageCleanupError::Docker)?;
        let mut outcomes = Vec::with_capacity(removals.len());
        for removal in removals {
            let outcome = if in_use.contains(&removal.image_id) {
                RemovalOutcome::InUse
            } else {
                match self.docker.remove(removal.reference.clone()).await {
                    Ok(()) => RemovalOutcome::Removed,
                    Err(error) => RemovalOutcome::Failed(format!("{error:#}")),
                }
            };
            info!(
                event_kind = "docker_image_cleanup",
                image_id = %removal.image_id,
                reference = %removal.reference,
                reason = ?removal.reason,
                outcome = ?outcome,
                "Docker image cleanup step"
            );
            outcomes.push((removal, outcome));
        }
        Ok(outcomes)
    }

    /// Returns the names rooted loaders need: each loader's launch tag and the source tag its
    /// metadata names, and the launch tags whose source tag is unknown.
    async fn keep(&self) -> Result<Keep, ImageCleanupError> {
        let mut keep = Keep::default();
        for store_path in self.roots.rooted_store_paths().await? {
            let Some(launch) = dotcore_domain::nix::docker_launch_reference(&store_path) else {
                continue;
            };
            let metadata = self
                .store_root
                .join(store_path.trim_start_matches('/'))
                .join(METADATA_PATH);
            match source_reference(&metadata).await {
                Some(source) => {
                    keep.names.insert(source);
                }
                // Another Dotcore version may write other metadata; its image stays whole.
                None => {
                    keep.whole.insert(launch.clone());
                }
            }
            keep.names.insert(launch);
        }
        Ok(keep)
    }

    /// Returns the removals of one image no container uses.
    fn removals(&self, image: &LocalImage, keep: &Keep) -> Vec<ImageRemoval> {
        let removal = |target: &str, reason| ImageRemoval {
            image_id: image.id.clone(),
            reference: target.to_owned(),
            reason,
        };
        let loader_names: Vec<&String> = image
            .tags
            .iter()
            .filter(|tag| tag.starts_with(&self.name_prefix))
            .collect();
        if loader_names.is_empty() {
            return if image.tags.is_empty() && image.labels.contains_key(RECIPE_LABEL) {
                vec![removal(&image.id, RemovalReason::UntaggedBuild)]
            } else {
                Vec::new()
            };
        }
        if loader_names.iter().any(|name| keep.whole.contains(*name)) {
            return Vec::new();
        }
        let unused: Vec<&String> = loader_names
            .into_iter()
            .filter(|name| !keep.names.contains(*name))
            .collect();
        let mut removals: Vec<ImageRemoval> = unused
            .iter()
            .map(|name| removal(name, RemovalReason::UnusedName))
            .collect();
        // Once its last name goes, an image held only by digest references would stay forever.
        if unused.len() == image.tags.len() {
            removals.extend(
                image
                    .digests
                    .iter()
                    .map(|digest| removal(digest, RemovalReason::DigestReference)),
            );
        }
        removals
    }

    /// Resolves loader store paths under `store_root` and reads roots through `roots`, for fake
    /// stores in tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_nix(mut self, store_root: PathBuf, roots: Arc<dyn NixRootScan>) -> Self {
        self.store_root = store_root;
        self.roots = roots;
        self
    }

    /// Considers only names under `prefix`, so tests on a shared daemon touch only their own.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_name_prefix(mut self, prefix: &str) -> Self {
        self.name_prefix = prefix.to_owned();
        self
    }

    /// Waits at most `wait` for image loads in progress.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_lock_wait(mut self, wait: Duration) -> Self {
        self.lock_wait = wait;
        self
    }
}

/// One name or untagged image cleanup removes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageRemoval {
    /// Image the removal applies to.
    pub image_id: String,

    /// What Docker is asked to remove: a name, or the image ID of an untagged build.
    pub reference: String,

    pub reason: RemovalReason,
}

/// Why cleanup removes a name or an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemovalReason {
    /// A loader name whose loader no Nix root keeps.
    UnusedName,

    /// An upstream digest reference of an image losing its last name.
    DigestReference,

    /// An untagged local build of a Dockerfile recipe.
    UntaggedBuild,
}

/// What happened to one planned removal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemovalOutcome {
    Removed,

    /// A container started using the image after planning, so it kept the name.
    InUse,

    /// Docker refused or failed the removal.
    Failed(String),
}

/// A local Docker image as cleanup sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalImage {
    pub id: String,
    pub tags: Vec<String>,
    pub digests: Vec<String>,
    pub labels: BTreeMap<String, String>,
}

/// The local Docker daemon operations cleanup needs.
pub trait ImageCleanupDocker: Send + Sync {
    /// Lists every local image with its names and labels.
    fn images(&self) -> CleanupFuture<'_, Vec<LocalImage>>;

    /// Returns the image IDs every container, running or stopped, was created from.
    fn container_images(&self) -> CleanupFuture<'_, BTreeSet<String>>;

    /// Removes one name, or an image by ID, without forcing it.
    fn remove(&self, reference: String) -> CleanupFuture<'_, ()>;
}

/// The store paths the host's Nix garbage collector counts as roots.
pub trait NixRootScan: Send + Sync {
    /// Returns every store path a root names directly.
    fn rooted_store_paths(&self) -> CleanupFuture<'_, BTreeSet<String>, ImageCleanupError>;
}

pub type CleanupFuture<'a, T, E = anyhow::Error> =
    Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>>;

/// Failures that stop cleanup before it removes anything more.
#[derive(Debug, thiserror::Error)]
pub enum ImageCleanupError {
    #[error(
        "Nix is not installed on this machine: no nix executable in {}; cleanup needs it to read Nix roots",
        dotcore_domain::nix::search_locations()
    )]
    NixMissing,

    #[error("could not read the Nix garbage-collector roots: {detail}")]
    Roots { detail: String },

    #[error("Docker image loads are still in progress; retry once they finish")]
    LoadsInProgress,

    #[error("Docker image cleanup lock failed")]
    Lock(#[source] ArtifactCacheError),

    #[error("Docker daemon operation failed")]
    Docker(#[source] anyhow::Error),
}

#[derive(Default)]
struct Keep {
    names: BTreeSet<String>,

    /// Launch tags whose images keep every name, since their source tag is unknown.
    whole: BTreeSet<String>,
}

/// Reads roots with `nix-store --gc --print-roots`, which lists them without collecting.
struct CommandNixRootScan {
    command: NixCommand,
}

impl NixRootScan for CommandNixRootScan {
    fn rooted_store_paths(&self) -> CleanupFuture<'_, BTreeSet<String>, ImageCleanupError> {
        Box::pin(async move {
            let nix = self
                .command
                .executable()
                .ok_or(ImageCleanupError::NixMissing)?;
            let output = tokio::process::Command::new(nix.with_file_name("nix-store"))
                .args(["--gc", "--print-roots"])
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|error| ImageCleanupError::Roots {
                    detail: error.to_string(),
                })?;
            if !output.status.success() {
                return Err(ImageCleanupError::Roots {
                    detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                });
            }
            Ok(root_store_paths(&String::from_utf8_lossy(&output.stdout)))
        })
    }
}

/// Returns the top-level store paths of `nix-store --gc --print-roots` lines
/// (`<root> -> <path>`), whose root may be a link, a process, or a censored location.
fn root_store_paths(listing: &str) -> BTreeSet<String> {
    listing
        .lines()
        .filter_map(|line| line.rsplit_once(" -> "))
        .filter_map(|(_, path)| {
            let entry = path.trim().strip_prefix("/nix/store/")?.split('/').next()?;
            Some(format!("/nix/store/{entry}"))
        })
        .filter(|store_path| dotcore_domain::nix::valid_store_path(store_path))
        .collect()
}

/// Returns the source tag a loader's metadata names, or `None` when it is unreadable.
async fn source_reference(metadata: &Path) -> Option<String> {
    let text = tokio::fs::read(metadata).await.ok()?;
    let value: serde_json::Value = serde_json::from_slice(&text).ok()?;
    value.get("reference")?.as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::model::acquire_shared_lock;

    /// Loader store paths: `ROOTED` is rooted, `UNROOTED` is not, `FOREIGN` is rooted with
    /// metadata this version cannot read.
    const ROOTED: &str = "/nix/store/00000000000000000000000000000001-docker-image-n8n";
    const OTHER_ROOTED: &str = "/nix/store/00000000000000000000000000000002-docker-image-n8n";
    const UNROOTED: &str = "/nix/store/00000000000000000000000000000003-docker-image-n8n";
    const FOREIGN: &str = "/nix/store/00000000000000000000000000000004-docker-image-vllm";
    const SOURCE: &str = "dotcore-nix/n8n:sha256-0123";

    #[tokio::test]
    async fn plan_keeps_what_rooted_loaders_and_containers_use() {
        let fixture = Fixture::new();
        let launch = |store_path| dotcore_domain::nix::docker_launch_reference(store_path).unwrap();
        fixture
            .docker
            .add("shared", &[&launch(ROOTED), &launch(UNROOTED), SOURCE], &[]);
        fixture.docker.add(
            "stale",
            &[
                "dotcore-nix/docker-image-old:0123",
                "dotcore-nix/old:sha256-0",
            ],
            &["docker.io/library/old@sha256:aaaa"],
        );
        fixture.docker.add(
            "used",
            &["dotcore-nix/docker-image-used:0123"],
            &["docker.io/library/used@sha256:bbbb"],
        );
        fixture.docker.add(
            "mixed",
            &["dotcore-nix/docker-image-mixed:0123", "dotcore/hermes:dev"],
            &["dotcore/hermes@sha256:cccc"],
        );
        fixture.docker.add(
            "foreign",
            &[&launch(FOREIGN), "dotcore-nix/vllm:unknown"],
            &[],
        );
        fixture.docker.add("unrelated", &["postgres:16"], &[]);
        fixture.docker.add_labeled("dangling-build", RECIPE_LABEL);
        fixture.docker.add_labeled("dangling-other", "other.label");
        fixture.docker.use_image("used");

        let plan = fixture.cleanup().plan().await.expect("plan");

        assert_eq!(
            plan,
            [
                removal(
                    "dangling-build",
                    "dangling-build",
                    RemovalReason::UntaggedBuild
                ),
                removal(
                    "mixed",
                    "dotcore-nix/docker-image-mixed:0123",
                    RemovalReason::UnusedName
                ),
                removal("shared", &launch(UNROOTED), RemovalReason::UnusedName),
                removal(
                    "stale",
                    "dotcore-nix/docker-image-old:0123",
                    RemovalReason::UnusedName
                ),
                removal(
                    "stale",
                    "dotcore-nix/old:sha256-0",
                    RemovalReason::UnusedName
                ),
                removal(
                    "stale",
                    "docker.io/library/old@sha256:aaaa",
                    RemovalReason::DigestReference
                ),
            ]
        );
        assert!(
            fixture.docker.removed().is_empty(),
            "planning removes nothing"
        );
    }

    #[tokio::test]
    async fn apply_removes_the_plan_but_spares_images_a_container_started_using() {
        let fixture = Fixture::new();
        fixture
            .docker
            .add("stale", &["dotcore-nix/docker-image-old:0123"], &[]);
        fixture
            .docker
            .add("late", &["dotcore-nix/docker-image-late:0123"], &[]);
        fixture.docker.use_image_after_listing("late");

        let outcomes = fixture.cleanup().apply().await.expect("apply");

        assert_eq!(
            outcomes,
            [
                (
                    removal(
                        "late",
                        "dotcore-nix/docker-image-late:0123",
                        RemovalReason::UnusedName
                    ),
                    RemovalOutcome::InUse
                ),
                (
                    removal(
                        "stale",
                        "dotcore-nix/docker-image-old:0123",
                        RemovalReason::UnusedName
                    ),
                    RemovalOutcome::Removed
                ),
            ]
        );
        assert_eq!(
            fixture.docker.removed(),
            ["dotcore-nix/docker-image-old:0123"]
        );
    }

    #[tokio::test]
    async fn apply_waits_for_image_loads_and_gives_up_after_its_bound() {
        let fixture = Fixture::new();
        fixture
            .docker
            .add("stale", &["dotcore-nix/docker-image-old:0123"], &[]);
        let load = acquire_shared_lock(
            fixture.cache().join(CLEANUP_LOCK_PATH),
            &CancellationToken::new(),
        )
        .await
        .expect("load lock");

        let error = fixture
            .cleanup()
            .with_lock_wait(Duration::from_millis(50))
            .apply()
            .await
            .expect_err("a load holds the lock");

        assert!(
            matches!(error, ImageCleanupError::LoadsInProgress),
            "{error:?}"
        );
        assert!(fixture.docker.removed().is_empty());
        drop(load);
        fixture
            .cleanup()
            .apply()
            .await
            .expect("apply after the load");
        assert_eq!(
            fixture.docker.removed(),
            ["dotcore-nix/docker-image-old:0123"]
        );
    }

    /// Builds tiny label-only images under a test prefix on the real daemon: one kept by a rooted
    /// loader's source tag but also carrying a stale name, one no root keeps, and one a stopped
    /// container uses. Cleanup removes exactly the stale and unrooted names, which deletes the
    /// unrooted image. Other images on the daemon are planned but never touched.
    #[tokio::test]
    #[ignore = "hardware: just backend::test-docker"]
    async fn hw_docker_cleanup_removes_only_names_no_root_keeps() {
        let docker = |arguments: &[&str]| {
            std::process::Command::new("docker")
                .args(arguments)
                .output()
                .expect("docker command")
        };
        let suffix = dotcore_domain::test_support::unique_suffix();
        let prefix = format!("dotcore-nix-test-{suffix}/");
        let build = |name: &str| {
            let tag = format!("{prefix}{name}:1");
            let mut child = std::process::Command::new("docker")
                .args(["build", "--quiet", "--tag", &tag, "-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .expect("docker build");
            std::io::Write::write_all(
                &mut child.stdin.take().expect("build input"),
                format!("FROM scratch\nLABEL dotcore.test={suffix}-{name}\n").as_bytes(),
            )
            .expect("write Dockerfile");
            assert!(child.wait().expect("docker build").success(), "build {tag}");
            tag
        };
        let present = |reference: &str| docker(&["image", "inspect", reference]).status.success();
        let kept = build("kept");
        let stale = format!("{prefix}stale:1");
        assert!(
            docker(&["tag", &kept, &stale]).status.success(),
            "tag {stale}"
        );
        let unrooted = build("unrooted");
        let used = build("used");
        let container = format!("dotcore-test-cleanup-{suffix}");
        assert!(
            docker(&["create", "--name", &container, &used, "/absent"])
                .status
                .success(),
            "create {container}"
        );
        let fixture = Fixture::new();
        fixture.metadata(
            ROOTED,
            &serde_json::json!({ "reference": kept }).to_string(),
        );
        let client = Arc::new(
            crate::DockerOciClient::connect()
                .await
                .expect("Docker client"),
        );
        let cleanup = ImageCleanup::new(&fixture.cache(), client.clone())
            .with_nix(
                fixture.root.path().join("store"),
                Arc::new(FixedRoots([ROOTED].map(str::to_owned).into())),
            )
            .with_name_prefix(&prefix);

        let mine: Vec<ImageRemoval> = cleanup
            .plan()
            .await
            .expect("plan")
            .into_iter()
            .filter(|removal| removal.reference.starts_with(&prefix))
            .collect();
        let mut names: Vec<&str> = mine
            .iter()
            .filter(|removal| removal.reason == RemovalReason::UnusedName)
            .map(|removal| removal.reference.as_str())
            .collect();
        names.sort();
        assert_eq!(names, [stale.as_str(), &format!("{prefix}unrooted:1")]);
        // The containerd image store reports a local build's own digest reference per name.
        assert!(
            mine.iter()
                .filter(|removal| removal.reason == RemovalReason::DigestReference)
                .all(|removal| removal.reference.starts_with(&format!("{prefix}unrooted@"))),
            "{mine:?}"
        );
        for removal in &mine {
            client
                .remove(removal.reference.clone())
                .await
                .expect("remove");
        }

        assert!(present(&kept), "the rooted loader's name stays");
        assert!(!present(&stale), "the stale name goes");
        assert!(!present(&unrooted), "the unrooted image goes");
        assert!(present(&used), "an image a container uses keeps its name");
        docker(&["rm", &container]);
        docker(&["image", "rm", &kept, &used]);
    }

    #[test]
    fn roots_are_read_as_top_level_store_paths() {
        let listing = format!(
            "/home/u/site/nix-roots/x -> {ROOTED}\n{{censored}} -> {OTHER_ROOTED}\n/proc/12/maps -> {FOREIGN}/lib/libfoo.so\n/proc/13/environ -> /not/a/store/path\n"
        );

        assert_eq!(
            root_store_paths(&listing),
            [ROOTED, OTHER_ROOTED, FOREIGN].map(str::to_owned).into()
        );
    }

    fn removal(image_id: &str, reference: &str, reason: RemovalReason) -> ImageRemoval {
        ImageRemoval {
            image_id: image_id.to_owned(),
            reference: reference.to_owned(),
            reason,
        }
    }

    /// Fake store holding the loaders' metadata, fake roots, fake daemon and cache.
    struct Fixture {
        root: tempfile::TempDir,
        docker: Arc<FakeDocker>,
    }

    impl Fixture {
        fn new() -> Self {
            let fixture = Self {
                root: tempfile::tempdir().expect("fixture root"),
                docker: Arc::new(FakeDocker::default()),
            };
            for store_path in [ROOTED, OTHER_ROOTED, UNROOTED] {
                fixture.metadata(
                    store_path,
                    &serde_json::json!({ "reference": SOURCE }).to_string(),
                );
            }
            fixture.metadata(FOREIGN, "not metadata");
            fixture
        }

        fn metadata(&self, store_path: &str, content: &str) {
            let path = self
                .root
                .path()
                .join("store")
                .join(store_path.trim_start_matches('/'))
                .join(METADATA_PATH);
            std::fs::create_dir_all(path.parent().expect("metadata folder"))
                .expect("metadata folder");
            std::fs::write(path, content).expect("metadata");
        }

        fn cache(&self) -> PathBuf {
            self.root.path().join("cache")
        }

        fn cleanup(&self) -> ImageCleanup {
            ImageCleanup::new(&self.cache(), self.docker.clone()).with_nix(
                self.root.path().join("store"),
                Arc::new(FixedRoots(
                    [ROOTED, OTHER_ROOTED, FOREIGN].map(str::to_owned).into(),
                )),
            )
        }
    }

    struct FixedRoots(BTreeSet<String>);

    impl NixRootScan for FixedRoots {
        fn rooted_store_paths(&self) -> CleanupFuture<'_, BTreeSet<String>, ImageCleanupError> {
            Box::pin(async move { Ok(self.0.clone()) })
        }
    }

    /// Fake daemon: images by ID, the images containers use, and the removals asked of it.
    #[derive(Default)]
    struct FakeDocker {
        images: Mutex<Vec<LocalImage>>,
        in_use: Mutex<BTreeSet<String>>,
        in_use_after_listing: Mutex<BTreeSet<String>>,
        removed: Mutex<Vec<String>>,
    }

    impl FakeDocker {
        fn add(&self, id: &str, tags: &[&str], digests: &[&str]) {
            self.images.lock().expect("images").push(LocalImage {
                id: id.to_owned(),
                tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
                digests: digests.iter().map(|digest| (*digest).to_owned()).collect(),
                labels: BTreeMap::new(),
            });
        }

        fn add_labeled(&self, id: &str, label: &str) {
            self.images.lock().expect("images").push(LocalImage {
                id: id.to_owned(),
                labels: BTreeMap::from([(label.to_owned(), "0123".to_owned())]),
                ..LocalImage::default()
            });
        }

        fn use_image(&self, id: &str) {
            self.in_use.lock().expect("in use").insert(id.to_owned());
        }

        /// Makes the image in use from the second container listing on, as when a container is
        /// created between planning and removal.
        fn use_image_after_listing(&self, id: &str) {
            self.in_use_after_listing
                .lock()
                .expect("in use")
                .insert(id.to_owned());
        }

        fn removed(&self) -> Vec<String> {
            self.removed.lock().expect("removed").clone()
        }
    }

    impl ImageCleanupDocker for FakeDocker {
        fn images(&self) -> CleanupFuture<'_, Vec<LocalImage>> {
            let mut images = self.images.lock().expect("images").clone();
            images.sort_by(|left, right| left.id.cmp(&right.id));
            Box::pin(async move { Ok(images) })
        }

        fn container_images(&self) -> CleanupFuture<'_, BTreeSet<String>> {
            let mut in_use = self.in_use.lock().expect("in use");
            let listed = in_use.clone();
            in_use.extend(std::mem::take(
                &mut *self.in_use_after_listing.lock().expect("in use"),
            ));
            Box::pin(async move { Ok(listed) })
        }

        fn remove(&self, reference: String) -> CleanupFuture<'_, ()> {
            self.removed.lock().expect("removed").push(reference);
            Box::pin(async { Ok(()) })
        }
    }
}
