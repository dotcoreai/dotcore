//! Realizes the Docker image of a Docker-delivered Nix artifact on the node that hosts it.
//!
//! The artifact's package is a Docker image derivation: a loader program plus the image's
//! metadata. Preparation runs the loader against the node's Docker daemon, which makes the image
//! available under a reference derived from the package's store path, then verifies the realized
//! image against the metadata itself instead of trusting the loader. An image that already
//! verifies is not loaded again. Runs of one image are serialized across processes, bounded in
//! time, and stop with every process they started; each line a run prints reaches the caller as
//! it arrives.

use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    time::Duration,
};

use dotcore_domain::real_time;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt as _, AsyncRead, BufReader};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::{
    ArtifactCacheError, NixPreparationError, OciPlatform,
    model::{acquire_shared_lock, acquire_step_lock},
    output::{OutputStream, StepOutput},
};

/// Longest loader run: sized for the slowest realization, a pull of a large CUDA image or a local
/// build of a CUDA engine.
pub(crate) const LOADER_TIME_LIMIT: Duration = Duration::from_secs(2 * 60 * 60);

/// Loader command that makes the image available and prints its result.
const LOAD_COMMAND: &str = "load";

/// Image metadata file of a Docker image derivation package.
pub(crate) const METADATA_PATH: &str = "share/dotcore/image.json";

/// Label a local build of a Dockerfile recipe carries, naming the recipe hash. The loader in
/// `nix/docker-images/loader.sh` applies it.
pub(crate) const RECIPE_LABEL: &str = "dotcore.recipe";

/// Bounded number of loader stderr lines kept with a failure.
const STDERR_LINE_LIMIT: usize = 20;

/// Directory under the artifact cache holding one cross-process lock per image.
const LOCK_DIRECTORY: &str = ".dotcore-docker-images/.locks";

/// Lock under the artifact cache that image loads hold shared and image cleanup exclusively.
pub(crate) const CLEANUP_LOCK_PATH: &str = ".dotcore-docker-images/cleanup.lock";

/// Injectable Docker image inspection the artifact manager verifies realized images with.
pub trait DockerImageInspector: Send + Sync {
    /// Inspects the image a reference names, or `None` when the daemon has no such image.
    fn inspect_docker_image(&self, reference: String) -> DockerImageInspectionFuture;
}

pub type DockerImageInspectionFuture =
    Pin<Box<dyn Future<Output = Result<Option<DockerImageFacts>, anyhow::Error>> + Send>>;

/// Facts of one locally present Docker image that verification compares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DockerImageFacts {
    pub image_id: Option<String>,
    pub platform: OciPlatform,
    pub diff_ids: Vec<String>,
    pub labels: BTreeMap<String, String>,
}

/// Runs Docker image derivation loaders and verifies the images they realize.
#[derive(Clone)]
pub(crate) struct DockerImageLoader {
    locks: PathBuf,
    cleanup_lock: PathBuf,
    docker_endpoint: String,
    time_limit: Duration,
}

impl DockerImageLoader {
    /// Loads under `cache_root`, against the backend's Docker daemon.
    pub(crate) fn new(cache_root: &Path) -> Self {
        Self {
            locks: cache_root.join(LOCK_DIRECTORY),
            cleanup_lock: cache_root.join(CLEANUP_LOCK_PATH),
            docker_endpoint: dotcore_domain::docker::local_endpoint(),
            time_limit: LOADER_TIME_LIMIT,
        }
    }

    /// Runs the loader of the package at `package` (the built store path, under the store root)
    /// for `launch_reference`, then verifies the image under that reference runs on `platform`
    /// and has the identity the package's metadata records. `nix` is put on the loader's `PATH`
    /// so it can find stored images. An image that already verifies is used as is, without
    /// running the loader or reporting to `step`, so the step ends as already prepared.
    pub(crate) async fn load(
        &self,
        request: DockerImageLoad<'_>,
        inspector: &dyn DockerImageInspector,
        cancellation: &CancellationToken,
        step: &StepOutput,
    ) -> Result<(), DockerImagePreparationError> {
        let metadata = ImageMetadata::read(&request.package.join(METADATA_PATH)).await?;
        // The loader program is derived from the store path, so a package built under another
        // name fails here rather than as an unexplained spawn error.
        let loader = request.package.join("bin").join(request.loader);
        if !tokio::fs::try_exists(&loader).await.unwrap_or(false) {
            return Err(DockerImagePreparationError::LoaderMissing {
                path: loader.display().to_string(),
            });
        }
        let lock_error = |error| match error {
            ArtifactCacheError::Cancelled => DockerImagePreparationError::Cancelled,
            error => DockerImagePreparationError::Lock(error),
        };
        // Held until verification ends, so image cleanup never removes a name mid-load or while
        // a previous load is reused.
        let _cleanup = acquire_shared_lock(self.cleanup_lock.clone(), cancellation)
            .await
            .map_err(lock_error)?;
        if self
            .loaded(&request, &metadata, inspector, cancellation)
            .await?
        {
            return Ok(());
        }
        step.start();
        let _lock = acquire_step_lock(
            self.locks
                .join(format!("{}.lock", lock_name(&metadata.reference))),
            cancellation,
            step,
        )
        .await
        .map_err(lock_error)?;
        if self
            .loaded(&request, &metadata, inspector, cancellation)
            .await?
        {
            return Ok(());
        }

        info!(
            event_kind = "docker_image_loading",
            reference = request.launch_reference,
            loader = %request.package.join("bin").join(request.loader).display(),
            docker_endpoint = %self.docker_endpoint,
            "running the Docker image derivation loader"
        );
        let output = self.run(&request, cancellation, step).await?;
        let result = LoadResult::parse(&output)?;
        if result.reference != request.launch_reference {
            return Err(DockerImagePreparationError::ReferenceMismatch {
                expected: request.launch_reference.to_owned(),
                reported: result.reference,
            });
        }
        verify(&request, &metadata, inspector, cancellation).await?;
        info!(
            event_kind = "docker_image_loaded",
            reference = request.launch_reference,
            image_id = %result.image_id,
            path = %result.path,
            "Docker image derivation loaded and verified"
        );
        Ok(())
    }

    /// Whether the image under the launch reference already verifies, as a previous load left
    /// it. Only cancellation fails the check; any other failure means the image needs loading.
    async fn loaded(
        &self,
        request: &DockerImageLoad<'_>,
        metadata: &ImageMetadata,
        inspector: &dyn DockerImageInspector,
        cancellation: &CancellationToken,
    ) -> Result<bool, DockerImagePreparationError> {
        match verify(request, metadata, inspector, cancellation).await {
            Ok(()) => {
                debug!(
                    event_kind = "docker_image_reused",
                    reference = request.launch_reference,
                    "Docker image derivation already loaded"
                );
                Ok(true)
            }
            Err(DockerImagePreparationError::Cancelled) => {
                Err(DockerImagePreparationError::Cancelled)
            }
            Err(_) => Ok(false),
        }
    }

    async fn run(
        &self,
        request: &DockerImageLoad<'_>,
        cancellation: &CancellationToken,
        step: &StepOutput,
    ) -> Result<String, DockerImagePreparationError> {
        // Without `nix` the loader cannot look for a stored image and would quietly fall back to
        // the source, losing binary cache delivery.
        let nix = request
            .nix
            .as_deref()
            .ok_or(DockerImagePreparationError::Nix(
                NixPreparationError::NixMissing,
            ))?;
        let mut command =
            tokio::process::Command::new(request.package.join("bin").join(request.loader));
        command
            .arg(LOAD_COMMAND)
            .env(
                dotcore_domain::docker::DOCKER_HOST_ENV,
                &self.docker_endpoint,
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own process group, so stopping it also stops the pulls and builds it started.
            .process_group(0)
            .kill_on_drop(true);
        if let Some(path) = loader_path(nix) {
            command.env("PATH", path);
        }
        let mut child = command
            .spawn()
            .map_err(DockerImagePreparationError::Spawn)?;
        let group = ProcessGroup(child.id());
        let stdout = child.stdout.take().expect("piped loader stdout");
        let stderr = child.stderr.take().expect("piped loader stderr");
        let run = async {
            let (output, errors, status) = tokio::join!(
                read_lines(stdout, usize::MAX, step, OutputStream::Stdout),
                read_lines(stderr, STDERR_LINE_LIMIT, step, OutputStream::Stderr),
                child.wait()
            );
            let status = status.map_err(DockerImagePreparationError::Wait)?;
            Ok::<_, DockerImagePreparationError>((output, errors, status))
        };
        let (output, errors, status) = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(DockerImagePreparationError::Cancelled),
            result = real_time::bound(self.time_limit, run) => result.map_err(|_| {
                DockerImagePreparationError::TimedOut {
                    time_limit: self.time_limit,
                }
            })??,
        };
        group.disarm();
        if !status.success() {
            let stderr = errors.into_iter().collect::<Vec<_>>().join("\n");
            warn!(
                event_kind = "docker_image_loader_failed",
                reference = request.launch_reference,
                %status,
                stderr = %stderr,
                "Docker image derivation loader failed"
            );
            return Err(DockerImagePreparationError::LoaderFailed {
                status: status.to_string(),
                stderr,
            });
        }
        Ok(output.into_iter().collect::<Vec<_>>().join("\n"))
    }

    /// Sends the loader's Docker calls to `endpoint` instead of the backend's daemon.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn with_docker_endpoint(mut self, endpoint: String) -> Self {
        self.docker_endpoint = endpoint;
        self
    }

    /// Bounds loader runs by `time_limit` instead of the production limit.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn with_time_limit(mut self, time_limit: Duration) -> Self {
        self.time_limit = time_limit;
        self
    }
}

/// One loader run: the built package, its loader program, the reference the deployment launches,
/// the platform it runs on, and the host's `nix`, which the loader needs to find stored images.
pub(crate) struct DockerImageLoad<'a> {
    pub(crate) package: PathBuf,
    pub(crate) loader: &'a str,
    pub(crate) launch_reference: &'a str,
    pub(crate) platform: &'a OciPlatform,
    pub(crate) nix: Option<PathBuf>,
}

/// Failures while realizing and verifying a Docker-delivered Nix artifact's image.
#[derive(Debug, thiserror::Error)]
pub enum DockerImagePreparationError {
    #[error("Docker image verification is not configured on this node")]
    InspectorNotConfigured,

    #[error("this node's host has no Nix system that Nix artifacts target")]
    NixSystemUnavailable,

    #[error(transparent)]
    Nix(NixPreparationError),

    #[error(
        "Docker-delivered Nix artifact store path {store_path:?} names no Docker image derivation"
    )]
    InvalidStorePath { store_path: String },

    #[error("Docker image derivation metadata {path:?} could not be read")]
    Metadata {
        path: String,
        #[source]
        source: anyhow::Error,
    },

    #[error(
        "Docker image derivation has no loader program {path:?}; its package must name its loader after its store entry"
    )]
    LoaderMissing { path: String },

    #[error("Docker image lock could not be acquired")]
    Lock(#[source] ArtifactCacheError),

    #[error("Docker image preparation was cancelled")]
    Cancelled,

    #[error("Docker image derivation loader could not be started")]
    Spawn(#[source] std::io::Error),

    #[error("Docker image derivation loader could not be awaited")]
    Wait(#[source] std::io::Error),

    #[error("Docker image derivation loader exited with {status}")]
    LoaderFailed { status: String, stderr: String },

    #[error("Docker image derivation loader did not finish within {time_limit:?}")]
    TimedOut { time_limit: Duration },

    #[error("Docker image derivation loader printed no valid result")]
    MalformedResult {
        output: String,
        #[source]
        source: serde_json::Error,
    },

    #[error(
        "Docker image derivation loader reported {reported:?}, not the expected launch reference {expected:?}"
    )]
    ReferenceMismatch { expected: String, reported: String },

    #[error("Docker image inspection failed")]
    Inspection(#[source] anyhow::Error),

    #[error("Docker image {reference:?} is not in the Docker daemon after loading")]
    ImageMissing { reference: String },

    #[error(
        "Docker image {reference:?} is not the image its source tag {source_reference:?} names"
    )]
    TagMismatch {
        reference: String,
        source_reference: String,
    },

    #[error("Docker image {reference:?} is for platform {realized}, not {expected}")]
    PlatformMismatch {
        reference: String,
        expected: String,
        realized: String,
    },

    #[error("Docker image {reference:?} layers differ from the identity its derivation records")]
    IdentityMismatch { reference: String },

    #[error("Docker image {reference:?} is not a build of the recipe its derivation records")]
    RecipeMismatch { reference: String },
}

impl DockerImagePreparationError {
    pub(crate) fn is_cancelled(&self) -> bool {
        matches!(
            self,
            Self::Cancelled | Self::Nix(NixPreparationError::Cancelled)
        )
    }

    /// Returns the last lines the loader wrote to stderr, for failure diagnostics.
    pub fn loader_stderr(&self) -> Option<&str> {
        match self {
            Self::LoaderFailed { stderr, .. } if !stderr.is_empty() => Some(stderr),
            _ => None,
        }
    }
}

/// Docker image derivation metadata, as `share/dotcore/image.json` records it.
#[derive(Debug, Deserialize)]
struct ImageMetadata {
    /// Source tag the loader reuses images under.
    reference: String,
    platform: String,
    identity: Option<ImageIdentity>,
    dockerfile: Option<DockerfileRecipe>,
}

#[derive(Debug, Deserialize)]
struct ImageIdentity {
    #[serde(rename = "diffIds")]
    diff_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct DockerfileRecipe {
    #[serde(rename = "recipeHash")]
    recipe_hash: String,
}

impl ImageMetadata {
    async fn read(path: &Path) -> Result<Self, DockerImagePreparationError> {
        let error = |source: anyhow::Error| DockerImagePreparationError::Metadata {
            path: path.display().to_string(),
            source,
        };
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|source| error(anyhow::Error::new(source)))?;
        serde_json::from_slice(&bytes).map_err(|source| error(anyhow::Error::new(source)))
    }

    fn platform(&self) -> Option<OciPlatform> {
        let (operating_system, architecture) = self.platform.split_once('/')?;
        Some(OciPlatform {
            operating_system: operating_system.to_owned(),
            architecture: architecture.to_owned(),
            variant: None,
        })
    }
}

/// The loader's result line.
#[derive(Debug, Deserialize)]
struct LoadResult {
    reference: String,
    #[serde(rename = "imageId")]
    image_id: String,
    path: String,
}

impl LoadResult {
    /// Parses the last stdout line, where the loader prints its result.
    fn parse(output: &str) -> Result<Self, DockerImagePreparationError> {
        let line = output
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("");
        serde_json::from_str(line).map_err(|source| DockerImagePreparationError::MalformedResult {
            output: output.to_owned(),
            source,
        })
    }
}

/// Verifies the image under the launch reference, with registry images held to their locked layer
/// diff IDs and Dockerfile images to their recipe label, since a node's local build of a recipe
/// has node-specific bytes.
async fn verify(
    request: &DockerImageLoad<'_>,
    metadata: &ImageMetadata,
    inspector: &dyn DockerImageInspector,
    cancellation: &CancellationToken,
) -> Result<(), DockerImagePreparationError> {
    let reference = request.launch_reference;
    let platform_mismatch = |realized: String| DockerImagePreparationError::PlatformMismatch {
        reference: reference.to_owned(),
        expected: render_platform(request.platform),
        realized,
    };
    if !metadata
        .platform()
        .is_some_and(|platform| request.platform.is_satisfied_by(&platform))
    {
        return Err(platform_mismatch(metadata.platform.clone()));
    }

    let image = inspect(inspector, reference, cancellation)
        .await?
        .ok_or_else(|| DockerImagePreparationError::ImageMissing {
            reference: reference.to_owned(),
        })?;
    let source = inspect(inspector, &metadata.reference, cancellation).await?;
    if image.image_id.is_none() || source.is_none_or(|source| source.image_id != image.image_id) {
        return Err(DockerImagePreparationError::TagMismatch {
            reference: reference.to_owned(),
            source_reference: metadata.reference.clone(),
        });
    }
    if !request.platform.is_satisfied_by(&image.platform) {
        return Err(platform_mismatch(render_platform(&image.platform)));
    }
    match &metadata.dockerfile {
        Some(recipe) => {
            if image.labels.get(RECIPE_LABEL) != Some(&recipe.recipe_hash) {
                return Err(DockerImagePreparationError::RecipeMismatch {
                    reference: reference.to_owned(),
                });
            }
        }
        None => {
            if metadata
                .identity
                .as_ref()
                .is_none_or(|identity| identity.diff_ids != image.diff_ids)
            {
                return Err(DockerImagePreparationError::IdentityMismatch {
                    reference: reference.to_owned(),
                });
            }
        }
    }
    Ok(())
}

async fn inspect(
    inspector: &dyn DockerImageInspector,
    reference: &str,
    cancellation: &CancellationToken,
) -> Result<Option<DockerImageFacts>, DockerImagePreparationError> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(DockerImagePreparationError::Cancelled),
        result = inspector.inspect_docker_image(reference.to_owned()) => {
            result.map_err(DockerImagePreparationError::Inspection)
        }
    }
}

/// Kills a loader's whole process group unless disarmed after the loader exited by itself.
struct ProcessGroup(Option<u32>);

impl ProcessGroup {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if let Some(group) = self.0.and_then(|id| libc::pid_t::try_from(id).ok()) {
            // SAFETY: `kill` takes no pointers; a negative pid signals the loader's own group.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
    }
}

/// Reads lines until end of file, reporting each to `step` as printed to `kind` and keeping the
/// last `limit`. Bytes that are not UTF-8, such as compiler output of an image build, are replaced
/// rather than ending the read, since a closed pipe would fail the loader's next write.
async fn read_lines(
    stream: impl AsyncRead + Unpin,
    limit: usize,
    step: &StepOutput,
    kind: OutputStream,
) -> VecDeque<String> {
    let mut reader = BufReader::new(stream);
    let mut kept = VecDeque::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if kept.len() == limit {
                    kept.pop_front();
                }
                let text = String::from_utf8_lossy(&line);
                let text = text.trim_end_matches(['\n', '\r']);
                step.line(kind, text);
                kept.push_back(text.to_owned());
            }
        }
    }
    kept
}

/// Returns the loader's `PATH`: the directory holding `nix` ahead of the inherited `PATH`.
fn loader_path(nix: &Path) -> Option<std::ffi::OsString> {
    let directory = nix.parent()?.to_path_buf();
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    std::env::join_paths(std::iter::once(directory).chain(std::env::split_paths(&inherited))).ok()
}

/// One lock file name per image source tag.
fn lock_name(reference: &str) -> String {
    reference
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn render_platform(platform: &OciPlatform) -> String {
    let base = format!("{}/{}", platform.operating_system, platform.architecture);
    platform
        .variant
        .as_ref()
        .map_or(base.clone(), |variant| format!("{base}/{variant}"))
}

#[cfg(test)]
mod tests {
    use dotcore_domain::test_support::nix::{
        DOCKER_IMAGE_LAUNCH_REFERENCE, DOCKER_IMAGE_STORE_PATH,
    };
    use dotcore_proto::dotcore::{
        application::v1::{
            ResolvedApplicationDeploymentSnapshot, ResolvedApplicationServiceSelection,
        },
        catalog::v1::{
            ApplicationGraph, ApplicationService, ApplicationServiceArtifact, NixArtifact,
            NixDockerImage, application_service_artifact::Kind as ServiceArtifactKind,
            nix_artifact::Provides,
        },
        deployment::v1::ArtifactPreparationPolicy,
    };

    use super::*;
    use crate::{
        ApplicationArtifactPreparationError, ArtifactManager, ArtifactPreparationError,
        DockerImageFixture, EngineArtifactPreparation, EngineArtifactSource, PreparationOutput,
        output::{PreparationEventKind, PreparationStep, test_support::RecordedOutput},
    };

    const SOURCE_REFERENCE: &str = "dotcore-nix/n8n:0123";
    const IMAGE_ID: &str =
        "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const DOCKER_ENDPOINT: &str = "unix:///run/dotcore-test/docker.sock";
    const LOADER: &str = "docker-image-n8n";

    #[tokio::test]
    async fn a_verified_registry_image_is_prepared_with_the_backend_docker_endpoint() {
        let fixture = Fixture::registry(&success(DOCKER_IMAGE_LAUNCH_REFERENCE));
        fixture.loads_image(image(IMAGE_ID, "amd64", &["sha256:layer"], None));

        fixture.prepare().await.expect("prepared image");

        let environment = fixture.loader_environment();
        assert_eq!(
            environment.get("DOCKER_HOST").map(String::as_str),
            Some(DOCKER_ENDPOINT)
        );
        assert_eq!(
            environment.get("ARGUMENTS").map(String::as_str),
            Some("load")
        );
        assert!(
            environment.get("PATH").is_some_and(
                |path| path.starts_with(&format!("{}:", fixture.nix_directory().display()))
            ),
            "{environment:?}"
        );
        // The image is checked before and after the lock, then verified once loaded.
        assert_eq!(
            fixture.images.inspected(),
            [
                DOCKER_IMAGE_LAUNCH_REFERENCE,
                DOCKER_IMAGE_LAUNCH_REFERENCE,
                DOCKER_IMAGE_LAUNCH_REFERENCE,
                SOURCE_REFERENCE
            ]
        );
    }

    #[tokio::test]
    async fn loader_output_is_reported_as_it_runs_and_a_loaded_image_is_reported_already_prepared()
    {
        let fixture = Fixture::registry(&format!(
            "echo 'docker-image-n8n: stored: copying from the Nix store' >&2\necho 'Loaded image: {SOURCE_REFERENCE}'\necho 'docker-image-n8n: tagged' >&2\n{}",
            success(DOCKER_IMAGE_LAUNCH_REFERENCE)
        ));
        fixture.loads_image(image(IMAGE_ID, "amd64", &["sha256:layer"], None));
        let loaded = RecordedOutput::new();
        let reused = RecordedOutput::new();

        fixture
            .prepare_reporting(Duration::from_secs(60), CancellationToken::new(), &loaded)
            .await
            .expect("loaded image");
        fixture
            .prepare_reporting(Duration::from_secs(60), CancellationToken::new(), &reused)
            .await
            .expect("reused image");

        let events = loaded.kinds();
        assert_eq!(
            events[..2],
            [
                already_built(),
                (
                    PreparationStep::EngineImage,
                    PreparationEventKind::Started {
                        artifact: DOCKER_IMAGE_LAUNCH_REFERENCE.to_owned()
                    }
                )
            ]
        );
        assert_eq!(
            events.last(),
            Some(&(
                PreparationStep::EngineImage,
                PreparationEventKind::Finished {
                    duration: Duration::ZERO
                }
            ))
        );
        // The loader's two streams are read concurrently, so only each stream's order is fixed.
        assert_eq!(
            lines(&events, OutputStream::Stderr),
            [
                "docker-image-n8n: stored: copying from the Nix store",
                "docker-image-n8n: tagged"
            ]
        );
        assert_eq!(
            lines(&events, OutputStream::Stdout),
            [
                format!("Loaded image: {SOURCE_REFERENCE}"),
                result_line(DOCKER_IMAGE_LAUNCH_REFERENCE)
            ]
        );
        assert_eq!(events.len(), 7, "{events:?}");
        assert_eq!(
            reused.kinds(),
            [
                already_built(),
                (
                    PreparationStep::EngineImage,
                    PreparationEventKind::AlreadyPrepared {
                        artifact: DOCKER_IMAGE_LAUNCH_REFERENCE.to_owned()
                    }
                )
            ]
        );
        assert_eq!(fixture.loader_runs(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_out_loader_reports_its_output_up_to_the_time_limit() {
        let fixture = Fixture::registry(&format!(
            "echo 'docker-image-n8n: building from source' >&2\n{}",
            sleeping()
        ));
        let recorded = std::sync::Arc::new(RecordedOutput::new());
        let seen = recorded.clone();
        let pid_file = fixture.root.path().join("child.pid");

        let error = fixture
            .prepare_timing_out(Duration::from_secs(2), &recorded, move || {
                seen.kinds().len() == 3 && pid_file.exists()
            })
            .await;

        assert!(
            matches!(error, DockerImagePreparationError::TimedOut { .. }),
            "{error:?}"
        );
        assert_eq!(
            recorded.kinds(),
            [
                already_built(),
                (
                    PreparationStep::EngineImage,
                    PreparationEventKind::Started {
                        artifact: DOCKER_IMAGE_LAUNCH_REFERENCE.to_owned()
                    }
                ),
                (
                    PreparationStep::EngineImage,
                    PreparationEventKind::Line {
                        stream: OutputStream::Stderr,
                        text: "docker-image-n8n: building from source".to_owned()
                    }
                ),
                (
                    PreparationStep::EngineImage,
                    PreparationEventKind::Failed {
                        error: error.to_string()
                    }
                ),
            ]
        );
        fixture.assert_child_stopped().await;
    }

    #[tokio::test]
    async fn a_package_without_the_loader_its_store_path_names_is_rejected() {
        let fixture = Fixture::registry(&success(DOCKER_IMAGE_LAUNCH_REFERENCE));
        std::fs::remove_file(fixture.loader_program()).expect("remove loader");

        let error = fixture.prepare().await.expect_err("missing loader");

        let DockerImagePreparationError::LoaderMissing { path } = &error else {
            panic!("unexpected error {error:?}");
        };
        assert_eq!(path, &fixture.loader_program().display().to_string());
        assert_eq!(fixture.loader_runs(), 0);
    }

    // The loader tests write these samples (`checks.docker-images-samples` compares their fields),
    // so a field renamed on either side fails one of the two.
    #[test]
    fn loader_formats_read_from_the_committed_samples() {
        let raw = |sample: &str| -> serde_json::Value {
            serde_json::from_str(sample).expect("sample JSON")
        };
        let registry_sample = include_str!("../../fixtures/docker-image/registry-image.json");
        let dockerfile_sample = include_str!("../../fixtures/docker-image/dockerfile-image.json");
        let result_sample = include_str!("../../fixtures/docker-image/load-result.json");

        let registry: ImageMetadata = serde_json::from_str(registry_sample).expect("registry");
        let registry_raw = raw(registry_sample);
        assert_eq!(registry.reference, registry_raw["reference"]);
        assert_eq!(registry.platform, registry_raw["platform"]);
        assert_eq!(
            registry.identity.expect("registry identity").diff_ids,
            registry_raw["identity"]["diffIds"]
                .as_array()
                .expect("diff IDs")
                .iter()
                .map(|id| id.as_str().expect("diff ID").to_owned())
                .collect::<Vec<_>>()
        );
        let dockerfile: ImageMetadata =
            serde_json::from_str(dockerfile_sample).expect("dockerfile");
        assert_eq!(
            dockerfile
                .dockerfile
                .expect("dockerfile recipe")
                .recipe_hash,
            raw(dockerfile_sample)["dockerfile"]["recipeHash"]
        );
        let result = LoadResult::parse(result_sample).expect("result");
        let result_raw = raw(result_sample);
        assert_eq!(
            (result.reference, result.image_id, result.path),
            (
                result_raw["reference"]
                    .as_str()
                    .expect("reference")
                    .to_owned(),
                result_raw["imageId"].as_str().expect("image ID").to_owned(),
                result_raw["path"].as_str().expect("path").to_owned(),
            )
        );
    }

    #[tokio::test]
    async fn loading_without_nix_fails_instead_of_skipping_the_stored_image() {
        let fixture = Fixture::registry(&success(DOCKER_IMAGE_LAUNCH_REFERENCE));
        let package = fixture
            .loader_program()
            .parent()
            .and_then(Path::parent)
            .expect("package")
            .to_path_buf();

        let error = DockerImageLoader::new(&fixture.root.path().join("cache"))
            .load(
                DockerImageLoad {
                    package,
                    loader: LOADER,
                    launch_reference: DOCKER_IMAGE_LAUNCH_REFERENCE,
                    platform: &linux_amd64(),
                    nix: None,
                },
                &NoImages,
                &CancellationToken::new(),
                &StepOutput::discard(),
            )
            .await
            .expect_err("loading without nix");

        assert!(
            matches!(
                error,
                DockerImagePreparationError::Nix(NixPreparationError::NixMissing)
            ),
            "{error:?}"
        );
        assert_eq!(fixture.loader_runs(), 0);
    }

    #[tokio::test]
    async fn loader_failures_keep_the_tail_of_its_stderr() {
        let fixture = Fixture::registry(
            "for line in $(seq 1 30); do echo \"line $line\" >&2; done\necho 'docker-image-n8n: error: pull failed' >&2\nexit 3",
        );

        let error = fixture.prepare().await.expect_err("loader failure");

        let DockerImagePreparationError::LoaderFailed { status, stderr } = &error else {
            panic!("unexpected error {error:?}");
        };
        assert!(status.contains('3'), "{status}");
        assert_eq!(stderr.lines().count(), STDERR_LINE_LIMIT);
        assert_eq!(
            stderr.lines().last(),
            Some("docker-image-n8n: error: pull failed")
        );
        assert_eq!(error.loader_stderr(), Some(stderr.as_str()));
        // The image is only checked before loading, never verified after a failed load.
        assert_eq!(
            fixture.images.inspected(),
            [DOCKER_IMAGE_LAUNCH_REFERENCE, DOCKER_IMAGE_LAUNCH_REFERENCE]
        );
    }

    #[tokio::test]
    async fn loader_output_that_is_not_utf8_is_read_to_the_end() {
        let fixture = Fixture::registry(&format!(
            "for line in $(seq 1 200); do printf 'build \\377\\376 output\\n' >&2; done\n{}",
            success(DOCKER_IMAGE_LAUNCH_REFERENCE)
        ));
        fixture.loads_image(image(IMAGE_ID, "amd64", &["sha256:layer"], None));

        fixture
            .prepare()
            .await
            .expect("prepared despite non-UTF-8 output");
    }

    #[tokio::test(start_paused = true)]
    async fn a_loader_past_its_time_limit_is_stopped_with_the_processes_it_started() {
        let fixture = Fixture::registry(&sleeping());
        let time_limit = Duration::from_millis(300);
        let pid_file = fixture.root.path().join("child.pid");

        let error = fixture
            .prepare_timing_out(time_limit, &RecordedOutput::new(), move || {
                pid_file.exists()
            })
            .await;

        assert!(
            matches!(error, DockerImagePreparationError::TimedOut { time_limit: limit } if limit == time_limit),
            "{error:?}"
        );
        fixture.assert_child_stopped().await;
    }

    #[tokio::test]
    async fn a_cancelled_preparation_stops_the_loader_and_its_processes() {
        let fixture = Fixture::registry(&sleeping());
        let cancellation = CancellationToken::new();
        let cancel = cancellation.clone();
        let pid_file = fixture.root.path().join("child.pid");
        tokio::spawn(async move {
            wait_for(|| pid_file.exists()).await;
            cancel.cancel();
        });

        let error = fixture
            .prepare_with(Duration::from_secs(60), cancellation)
            .await
            .expect_err("cancelled");

        assert!(
            matches!(error, DockerImagePreparationError::Cancelled),
            "{error:?}"
        );
        fixture.assert_child_stopped().await;
    }

    #[tokio::test]
    async fn malformed_or_foreign_loader_results_are_rejected() {
        let malformed = Fixture::registry("echo 'loaded'");
        assert!(matches!(
            malformed.prepare().await,
            Err(DockerImagePreparationError::MalformedResult { output, .. }) if output == "loaded"
        ));

        let foreign = Fixture::registry(&success("dotcore-nix/docker-image-other:0123"));
        assert!(matches!(
            foreign.prepare().await,
            Err(DockerImagePreparationError::ReferenceMismatch { expected, reported })
                if expected == DOCKER_IMAGE_LAUNCH_REFERENCE && reported == "dotcore-nix/docker-image-other:0123"
        ));
    }

    #[tokio::test]
    async fn realized_images_are_verified_against_the_derivation_metadata() {
        let layers = ["sha256:layer"];
        let cases: [(
            &str,
            Option<DockerImageFacts>,
            Option<DockerImageFacts>,
            ErrorCheck,
        ); 4] = [
            (
                "missing",
                None,
                Some(image(IMAGE_ID, "amd64", &layers, None)),
                |error| matches!(error, DockerImagePreparationError::ImageMissing { .. }),
            ),
            (
                "tag points elsewhere",
                Some(image(IMAGE_ID, "amd64", &layers, None)),
                Some(image("sha256:other", "amd64", &layers, None)),
                |error| matches!(error, DockerImagePreparationError::TagMismatch { .. }),
            ),
            (
                "other platform",
                Some(image(IMAGE_ID, "arm64", &layers, None)),
                Some(image(IMAGE_ID, "arm64", &layers, None)),
                |error| {
                    matches!(error, DockerImagePreparationError::PlatformMismatch { expected, realized, .. }
                        if expected == "linux/amd64" && realized == "linux/arm64")
                },
            ),
            (
                "other layers",
                Some(image(IMAGE_ID, "amd64", &["sha256:other"], None)),
                Some(image(IMAGE_ID, "amd64", &["sha256:other"], None)),
                |error| matches!(error, DockerImagePreparationError::IdentityMismatch { .. }),
            ),
        ];
        for (case, launched, source, expected) in cases {
            let fixture = Fixture::registry(&success(DOCKER_IMAGE_LAUNCH_REFERENCE));
            if let Some(launched) = launched {
                fixture
                    .images
                    .present(DOCKER_IMAGE_LAUNCH_REFERENCE, launched);
            }
            if let Some(source) = source {
                fixture.images.present(SOURCE_REFERENCE, source);
            }
            let error = fixture.prepare().await.expect_err(case);
            assert!(expected(&error), "{case}: {error:?}");
        }
    }

    #[tokio::test]
    async fn dockerfile_images_are_held_to_their_recipe_label_not_their_bytes() {
        let local_build = image(IMAGE_ID, "amd64", &["sha256:local"], Some("recipe-hash"));
        let fixture = Fixture::dockerfile(&success(DOCKER_IMAGE_LAUNCH_REFERENCE));
        fixture.loads_image(local_build);
        fixture.prepare().await.expect("local build of the recipe");

        let other_recipe = image(IMAGE_ID, "amd64", &["sha256:local"], Some("other"));
        let fixture = Fixture::dockerfile(&success(DOCKER_IMAGE_LAUNCH_REFERENCE));
        fixture
            .images
            .present(DOCKER_IMAGE_LAUNCH_REFERENCE, other_recipe.clone());
        fixture.images.present(SOURCE_REFERENCE, other_recipe);
        assert!(matches!(
            fixture.prepare().await,
            Err(DockerImagePreparationError::RecipeMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn engines_and_application_services_prepare_through_the_loader() {
        let fixture = Fixture::registry(&success(DOCKER_IMAGE_LAUNCH_REFERENCE));
        fixture.loads_image(image(IMAGE_ID, "amd64", &["sha256:layer"], None));
        let manager = fixture.manager(Duration::from_secs(60));

        manager
            .prepare_engine(
                &EngineArtifactPreparation {
                    source: EngineArtifactSource::NixDockerImage {
                        flake: dotcore_domain::test_support::nix::locked_flake(),
                        package: LOADER.to_owned(),
                    },
                    platform: linux_amd64(),
                    preparation_policy: ArtifactPreparationPolicy::ImmutableRemote,
                    launch_identity: DOCKER_IMAGE_STORE_PATH.to_owned(),
                },
                CancellationToken::new(),
                &PreparationOutput::discard(),
            )
            .await
            .expect("prepared engine");
        manager
            .prepare_application(
                &application_snapshot(),
                CancellationToken::new(),
                &PreparationOutput::discard(),
            )
            .await
            .expect("prepared application");
        // The application service reuses the image the engine loaded.
        assert_eq!(fixture.loader_runs(), 1);

        for launch_identity in [dotcore_domain::test_support::nix::STORE_PATH, ""] {
            let mut unusable = application_snapshot();
            unusable.service_selections[0].launch_identity = launch_identity.to_owned();
            assert!(
                matches!(
                    manager.prepare_application(&unusable, CancellationToken::new(), &PreparationOutput::discard()).await,
                    Err(ApplicationArtifactPreparationError::DockerImageService { service_name, source: DockerImagePreparationError::InvalidStorePath { .. } })
                        if service_name == "web"
                ),
                "{launch_identity:?}"
            );
        }
        let without_inspector =
            ArtifactManager::new(fixture.root.path().join("cache"), None).expect("manager");
        assert!(matches!(
            without_inspector
                .prepare_engine(
                    &EngineArtifactPreparation {
                        source: EngineArtifactSource::NixDockerImage {
                            flake: dotcore_domain::test_support::nix::locked_flake(),
                            package: LOADER.to_owned(),
                        },
                        platform: linux_amd64(),
                        preparation_policy: ArtifactPreparationPolicy::ImmutableRemote,
                        launch_identity: DOCKER_IMAGE_STORE_PATH.to_owned(),
                    },
                    CancellationToken::new(),
                    &PreparationOutput::discard(),
                )
                .await,
            Err(ArtifactPreparationError::DockerImage(
                DockerImagePreparationError::InspectorNotConfigured
            ))
        ));
    }

    type ErrorCheck = fn(&DockerImagePreparationError) -> bool;

    /// A fake store holding one Docker image derivation package, already rooted in the artifact
    /// cache so preparation never runs Nix, whose loader runs `body` after recording its
    /// environment.
    struct Fixture {
        root: tempfile::TempDir,
        images: DockerImageFixture,
    }

    impl Fixture {
        fn registry(body: &str) -> Self {
            Self::new(
                body,
                serde_json::json!({
                    "reference": SOURCE_REFERENCE,
                    "platform": "linux/amd64",
                    "kind": "registry",
                    "identity": { "diffIds": ["sha256:layer"] },
                }),
            )
        }

        fn dockerfile(body: &str) -> Self {
            Self::new(
                body,
                serde_json::json!({
                    "reference": SOURCE_REFERENCE,
                    "platform": "linux/amd64",
                    "kind": "dockerfile",
                    "identity": { "diffIds": ["sha256:recorded"] },
                    "dockerfile": { "recipeHash": "recipe-hash" },
                }),
            )
        }

        fn new(body: &str, metadata: serde_json::Value) -> Self {
            let root = tempfile::tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
                .expect("fixture root");
            let images = DockerImageFixture::new(root.path());
            images.add_package(
                DOCKER_IMAGE_STORE_PATH,
                &metadata,
                &format!(
                    "ROOT='{}'\n{{ echo \"DOCKER_HOST=$DOCKER_HOST\"; echo \"PATH=$PATH\"; echo \"ARGUMENTS=$*\"; }} > '{}'\n{body}",
                    root.path().display(),
                    root.path().join("environment").display(),
                ),
            );
            Self { root, images }
        }

        fn manager(&self, time_limit: Duration) -> ArtifactManager {
            self.images
                .manager_with("x86_64-linux", DOCKER_ENDPOINT, time_limit)
        }

        async fn prepare(&self) -> Result<(), DockerImagePreparationError> {
            self.prepare_with(Duration::from_secs(60), CancellationToken::new())
                .await
        }

        async fn prepare_with(
            &self,
            time_limit: Duration,
            cancellation: CancellationToken,
        ) -> Result<(), DockerImagePreparationError> {
            self.prepare_reporting(time_limit, cancellation, &RecordedOutput::new())
                .await
        }

        /// Starts a preparation whose loader must outlive `time_limit`, holds the paused clock
        /// until `ready` reports the loader's progress, then lets the limit expire and returns
        /// the resulting error. Runs on a paused tokio clock, so scheduling delays cannot
        /// expire the limit early.
        async fn prepare_timing_out(
            &self,
            time_limit: Duration,
            recorded: &RecordedOutput,
            ready: impl Fn() -> bool + Send + 'static,
        ) -> DockerImagePreparationError {
            let prepare = self.prepare_reporting(time_limit, CancellationToken::new(), recorded);
            tokio::pin!(prepare);

            // A running blocking task keeps the paused clock from auto-advancing.
            let progressed = tokio::task::spawn_blocking(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(30);
                while !ready() && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                ready()
            });
            tokio::select! {
                result = &mut prepare => panic!("preparation ended before the loader progressed: {result:?}"),
                progressed = progressed => assert!(progressed.expect("wait for loader"), "loader made no progress"),
            }

            tokio::time::advance(time_limit).await;
            prepare.await.expect_err("timed out")
        }

        async fn prepare_reporting(
            &self,
            time_limit: Duration,
            cancellation: CancellationToken,
            recorded: &RecordedOutput,
        ) -> Result<(), DockerImagePreparationError> {
            let engine = EngineArtifactPreparation {
                source: EngineArtifactSource::NixDockerImage {
                    flake: dotcore_domain::test_support::nix::locked_flake(),
                    package: LOADER.to_owned(),
                },
                platform: linux_amd64(),
                preparation_policy: ArtifactPreparationPolicy::ImmutableRemote,
                launch_identity: DOCKER_IMAGE_STORE_PATH.to_owned(),
            };
            match self
                .manager(time_limit)
                .prepare_engine(&engine, cancellation, &recorded.output())
                .await
            {
                Ok(()) => Ok(()),
                Err(ArtifactPreparationError::DockerImage(error)) => Err(error),
                Err(error) => panic!("unexpected engine preparation error {error:?}"),
            }
        }

        /// Makes the fake daemon hold `image` under the launch and source references once the
        /// loader ran.
        fn loads_image(&self, image: DockerImageFacts) {
            for reference in [DOCKER_IMAGE_LAUNCH_REFERENCE, SOURCE_REFERENCE] {
                self.images
                    .present_after_load(DOCKER_IMAGE_STORE_PATH, reference, image.clone());
            }
        }

        fn loader_program(&self) -> PathBuf {
            self.images.loader_program(DOCKER_IMAGE_STORE_PATH)
        }

        fn nix_directory(&self) -> PathBuf {
            self.images.nix_directory()
        }

        fn loader_environment(&self) -> BTreeMap<String, String> {
            std::fs::read_to_string(self.root.path().join("environment"))
                .expect("loader environment")
                .lines()
                .filter_map(|line| line.split_once('='))
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect()
        }

        fn loader_runs(&self) -> usize {
            self.images.loader_runs(DOCKER_IMAGE_STORE_PATH)
        }

        /// Waits until the process the sleeping loader started is gone.
        async fn assert_child_stopped(&self) {
            let pid: libc::pid_t = std::fs::read_to_string(self.root.path().join("child.pid"))
                .expect("child pid")
                .trim()
                .parse()
                .expect("pid");
            // SAFETY: `kill` with signal 0 only checks that the process exists.
            let alive = || unsafe { libc::kill(pid, 0) } == 0;
            // Real time: callers may run with a paused tokio clock.
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while alive() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(!alive(), "loader child {pid} still runs");
        }
    }

    fn success(reference: &str) -> String {
        format!("echo '{}'", result_line(reference))
    }

    fn result_line(reference: &str) -> String {
        serde_json::json!({
            "reference": reference,
            "imageId": IMAGE_ID,
            "path": "present",
            "identityVerified": true,
            "platform": "linux/amd64",
        })
        .to_string()
    }

    /// The engine build event of a loader package the fixture store already holds.
    fn already_built() -> (PreparationStep, PreparationEventKind) {
        (
            PreparationStep::EngineBuild,
            PreparationEventKind::AlreadyPrepared {
                artifact: DOCKER_IMAGE_STORE_PATH.to_owned(),
            },
        )
    }

    /// Returns the text of each line `events` report on `stream`, in order.
    fn lines(
        events: &[(PreparationStep, PreparationEventKind)],
        stream: OutputStream,
    ) -> Vec<String> {
        events
            .iter()
            .filter_map(|(_, kind)| match kind {
                PreparationEventKind::Line {
                    stream: printed,
                    text,
                } if *printed == stream => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Loader body that starts a long child, records its pid in the fixture root, and waits for
    /// it.
    fn sleeping() -> String {
        "sleep 30 &\necho $! > \"$ROOT/child.pid.tmp\"\nmv \"$ROOT/child.pid.tmp\" \"$ROOT/child.pid\"\nwait"
            .to_owned()
    }

    fn image(
        id: &str,
        architecture: &str,
        layers: &[&str],
        recipe: Option<&str>,
    ) -> DockerImageFacts {
        DockerImageFacts {
            image_id: Some(id.to_owned()),
            platform: OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: architecture.to_owned(),
                variant: None,
            },
            diff_ids: layers.iter().map(|layer| (*layer).to_owned()).collect(),
            labels: recipe
                .map(|recipe| BTreeMap::from([(RECIPE_LABEL.to_owned(), recipe.to_owned())]))
                .unwrap_or_default(),
        }
    }

    fn linux_amd64() -> OciPlatform {
        OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "amd64".to_owned(),
            variant: None,
        }
    }

    fn application_snapshot() -> ResolvedApplicationDeploymentSnapshot {
        ResolvedApplicationDeploymentSnapshot {
            graph: Some(ApplicationGraph {
                services: vec![ApplicationService {
                    service_name: "web".to_owned(),
                    artifact: Some(ApplicationServiceArtifact {
                        kind: Some(ServiceArtifactKind::Nix(NixArtifact {
                            flake: dotcore_domain::test_support::nix::locked_flake(),
                            package: Some(LOADER.to_owned()),
                            provides: Some(Provides::DockerImage(NixDockerImage {})),
                            store_paths: Vec::new(),
                        })),
                        supported_platforms: Vec::new(),
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            platform: Some(dotcore_proto::dotcore::catalog::v1::OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                variant: None,
            }),
            service_selections: vec![ResolvedApplicationServiceSelection {
                service_name: "web".to_owned(),
                preparation_policy: ArtifactPreparationPolicy::ImmutableRemote as i32,
                launch_identity: DOCKER_IMAGE_STORE_PATH.to_owned(),
            }],
            ..Default::default()
        }
    }

    async fn wait_for(condition: impl Fn() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !condition() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Docker daemon holding no image.
    struct NoImages;

    impl DockerImageInspector for NoImages {
        fn inspect_docker_image(&self, _reference: String) -> DockerImageInspectionFuture {
            Box::pin(async { Ok(None) })
        }
    }
}
