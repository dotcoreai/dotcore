//! The artifact manager (crate `dotcore-artifact`): prepares the model and engine artifacts a
//! launch needs before it can run, as a platform service below the controllers.
//!
//! The model and auxiliary files named by a launch are placed into a local cache, each retained OCI
//! image is prepared through an injected OCI preparer according to the preparation policy its
//! snapshot retains, and each retained Nix store path is built and rooted in the node's root folder,
//! which keeps it alive while the node pins it and releases it afterwards. A
//! Docker-delivered Nix artifact's loader then runs against the node's Docker daemon, and the image
//! it realizes is verified before any launch uses it. On the
//! SiteNode that hosts a deployment, it pins an engine to the immutable identity that node
//! launches: a local image ID for a tag-only image, the store path a package of the node's
//! development source evaluates to, or a locked Nix artifact's store path for the node's own Nix
//! system, the system it builds every Nix artifact for. It only materializes artifacts, reports
//! each preparation step's output to an observer the caller supplies and reports failures; it
//! retains no output, and cancellation and lifecycle error handling stay with the caller.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Dotcore supports only Linux and macOS targets");

mod docker_image;
#[cfg(any(test, feature = "test-support"))]
mod docker_image_fixture;
mod image_cleanup;
mod model;
mod nix;
mod nix_build;
mod nix_roots;
mod oci;
mod output;
mod progress;

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use dotcore_domain::deployment::launch_pin::PinSource;
use dotcore_domain::nix::DotcoreSources;
use dotcore_proto::dotcore::{
    application::v1::ResolvedApplicationDeploymentSnapshot,
    catalog::v1::{
        application_service_artifact::Kind as ServiceArtifactKind, nix_artifact::Provides,
    },
    deployment::v1::ArtifactPreparationPolicy,
};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub use docker_image::{
    DockerImageFacts, DockerImageInspectionFuture, DockerImageInspector,
    DockerImagePreparationError,
};
#[cfg(any(test, feature = "test-support"))]
pub use docker_image_fixture::{DockerImageFixture, FixedNixEvaluator};
pub use image_cleanup::{
    CleanupFuture, ImageCleanup, ImageCleanupDocker, ImageCleanupError, ImageRemoval, LocalImage,
    NixRootScan, RemovalOutcome, RemovalReason,
};
#[cfg(any(test, feature = "test-support"))]
pub use model::BlockingWorkGate;
pub use model::ModelArtifactSource;
pub use model::{
    ArtifactCacheError, ModelArtifactCache, ModelArtifactPreparation, PreparedModelArtifacts,
};
pub use nix::{CommandNixEvaluator, NixEvaluationError, NixEvaluator};
#[cfg(any(test, feature = "test-support"))]
pub use nix::{NixEvaluation, NixEvaluationFuture};
pub use nix_build::{NixPreparationError, NixPreparationRequest, NixPreparer};
pub use nix_roots::{NIX_ROOTS_DIRECTORY, NixRootKeeper, PinnedStorePaths};
pub use oci::LocalImageIdentityResolutionError;
#[cfg(any(test, feature = "test-support"))]
pub use oci::LocalImageIdentityResolutionFuture;
#[cfg(any(test, feature = "test-support"))]
pub use oci::LocalImageIdentityResolver;
#[cfg(any(test, feature = "test-support"))]
pub use oci::OciPreparationFuture;
pub use oci::{
    DockerOciClient, OciClientError, OciImagePreparer, OciPlatform, OciPreparationError,
    OciPreparationRequest, OciPreparationSource, OciPreparer,
};
pub use output::{
    OutputStream, PreparationEvent, PreparationEventKind, PreparationObserver, PreparationOutput,
    PreparationStep, StepOutput,
};
pub use progress::{
    ModelArtifactProgress, ModelArtifactProgressObserver, ModelArtifactStage,
    ModelArtifactStageObserver,
};

/// Shared artifact-manager facade for a resolved model launch.
///
/// It owns model and auxiliary cache preparation, the injected OCI image preparer, and the Nix
/// development source this node builds engine artifacts from. Workload owners supply cancellation
/// and retain their lifecycle-specific error handling.
#[derive(Clone)]
pub struct ArtifactManager {
    model_cache: ModelArtifactCache,
    oci_preparer: Option<Arc<dyn OciImagePreparer>>,
    nix: NixSources,
    nix_preparer: NixPreparer,
    docker_images: docker_image::DockerImageLoader,
    /// Nix system this node builds Nix artifacts for: the host's, or `None` on a host Nix
    /// artifacts cannot target.
    nix_system: Option<&'static str>,
}

impl ArtifactManager {
    /// Creates the shared preparation facade over one artifact cache and optional OCI capability.
    pub fn new(
        model_cache_path: PathBuf,
        oci_preparer: Option<Arc<dyn OciImagePreparer>>,
    ) -> Result<Self, ArtifactCacheError> {
        Ok(Self::from_parts(
            ModelArtifactCache::new(model_cache_path)?,
            oci_preparer,
        ))
    }

    /// Selects the development source this node pins and builds `self` Nix artifacts from, and
    /// the folder rooting what it builds.
    pub fn with_nix_sources(mut self, nix: NixSources) -> Self {
        if let Some(roots) = &nix.roots {
            self.nix_preparer = self.nix_preparer.with_roots(roots.clone());
        }
        self.nix = nix;
        self
    }

    /// Reuses existing artifact preparation capabilities behind the shared facade.
    pub fn from_parts(
        model_cache: ModelArtifactCache,
        oci_preparer: Option<Arc<dyn OciImagePreparer>>,
    ) -> Self {
        Self {
            nix_preparer: NixPreparer::default(),
            docker_images: docker_image::DockerImageLoader::new(model_cache.root()),
            model_cache,
            oci_preparer,
            nix: NixSources::default(),
            nix_system: dotcore_domain::nix::host_system(),
        }
    }

    /// Materializes every model and auxiliary artifact selected by the resolved launch, reporting
    /// the downloads as the model download step to `output`.
    pub async fn prepare_model(
        &self,
        artifacts: &BTreeMap<String, ModelArtifactSource>,
        cancellation: &CancellationToken,
        progress: Option<ModelArtifactProgressObserver>,
        stage: Option<ModelArtifactStageObserver>,
        output: &PreparationOutput,
    ) -> Result<ModelArtifactPreparation, ArtifactCacheError> {
        let step = output.step(
            PreparationStep::ModelDownload,
            model_artifacts_description(artifacts),
        );
        let result = self
            .model_cache
            .prepare_artifacts_cancellable(artifacts, cancellation, progress, stage, &step)
            .await;
        step.complete(&result, ArtifactCacheError::is_cancelled);
        result
    }

    /// Returns the cache paths selected by the resolved model and auxiliary artifacts.
    pub fn expected_model_paths(
        &self,
        artifacts: &BTreeMap<String, ModelArtifactSource>,
    ) -> Result<PreparedModelArtifacts, ArtifactCacheError> {
        self.model_cache.expected_artifact_paths(artifacts)
    }

    /// Resolves a local-development tag to the immutable Docker image ID retained by a snapshot.
    pub async fn resolve_local_image_identity(
        &self,
        reference: String,
    ) -> Result<String, LocalImageIdentityResolutionError> {
        let resolver = self
            .oci_preparer
            .as_ref()
            .and_then(|preparer| preparer.as_local_image_identity_resolver())
            .ok_or(LocalImageIdentityResolutionError::NotConfigured)?;
        resolver.resolve_local_image_identity(reference).await
    }

    /// Prepares every immutable service image retained by an application snapshot: an OCI image
    /// by its launch identity, or a Docker-delivered Nix artifact of the retained graph by its
    /// pinned loader store path. Each service reports its build and image steps to `output`.
    pub async fn prepare_application(
        &self,
        snapshot: &ResolvedApplicationDeploymentSnapshot,
        cancellation: CancellationToken,
        output: &PreparationOutput,
    ) -> Result<(), ApplicationArtifactPreparationError> {
        let preparer = self
            .oci_preparer
            .as_ref()
            .ok_or(ApplicationArtifactPreparationError::OciPreparerNotConfigured)?;
        let platform = snapshot
            .platform
            .as_ref()
            .ok_or(ApplicationArtifactPreparationError::InvalidSnapshot)?;
        if platform.variant.is_some() {
            return Err(ApplicationArtifactPreparationError::InvalidSnapshot);
        }
        for selection in &snapshot.service_selections {
            let policy = ArtifactPreparationPolicy::try_from(selection.preparation_policy)
                .map_err(|_| ApplicationArtifactPreparationError::InvalidSnapshot)?;
            if let Some(nix) = service_nix_artifact(snapshot, &selection.service_name) {
                let image = DockerDeliveredNix::from_artifact(nix)
                    .ok_or(ApplicationArtifactPreparationError::InvalidSnapshot)?;
                self.prepare_docker_image(
                    &image,
                    &selection.launch_identity,
                    policy,
                    &OciPlatform {
                        operating_system: platform.operating_system.clone(),
                        architecture: platform.architecture.clone(),
                        variant: None,
                    },
                    cancellation.child_token(),
                    output,
                    PreparationStep::ServiceBuild {
                        service: selection.service_name.clone(),
                    },
                    PreparationStep::ServiceImage {
                        service: selection.service_name.clone(),
                    },
                )
                .await
                .map_err(|source| {
                    ApplicationArtifactPreparationError::DockerImageService {
                        service_name: selection.service_name.clone(),
                        source,
                    }
                })?;
                continue;
            }
            let source = ArtifactPreparationPolicy::try_from(selection.preparation_policy)
                .ok()
                .and_then(preparation_source)
                .ok_or(ApplicationArtifactPreparationError::InvalidSnapshot)?;
            let step = output.step(
                PreparationStep::ServiceImage {
                    service: selection.service_name.clone(),
                },
                selection.launch_identity.clone(),
            );
            let prepared = preparer
                .prepare(
                    OciPreparationRequest {
                        reference: selection.launch_identity.clone(),
                        platform: OciPlatform {
                            operating_system: platform.operating_system.clone(),
                            architecture: platform.architecture.clone(),
                            variant: platform.variant.clone(),
                        },
                        source,
                    },
                    cancellation.child_token(),
                    step.clone(),
                )
                .await;
            step.complete(&prepared, OciPreparationError::is_cancelled);
            prepared.map_err(|source| ApplicationArtifactPreparationError::Service {
                service_name: selection.service_name.clone(),
                source,
            })?;
        }
        Ok(())
    }

    /// Prepares the exact engine artifact one launch runs, reporting its engine build and engine
    /// image steps to `output`.
    pub async fn prepare_engine(
        &self,
        engine: &EngineArtifactPreparation,
        cancellation: CancellationToken,
        output: &PreparationOutput,
    ) -> Result<(), ArtifactPreparationError> {
        match &engine.source {
            EngineArtifactSource::OciImage => {
                let step =
                    output.step(PreparationStep::EngineImage, engine.launch_identity.clone());
                let prepared = self.prepare_oci_engine(engine, cancellation, &step).await;
                step.complete(&prepared, ArtifactPreparationError::is_cancelled);
                prepared
            }
            EngineArtifactSource::NixDockerImage { flake, package } => self
                .prepare_docker_image(
                    &DockerDeliveredNix {
                        flake: flake.clone(),
                        package: package.clone(),
                    },
                    &engine.launch_identity,
                    engine.preparation_policy,
                    &engine.platform,
                    cancellation,
                    output,
                    PreparationStep::EngineBuild,
                    PreparationStep::EngineImage,
                )
                .await
                .map_err(ArtifactPreparationError::DockerImage),
            EngineArtifactSource::Nix { flake, package } => {
                let step =
                    output.step(PreparationStep::EngineBuild, engine.launch_identity.clone());
                let prepared = self
                    .prepare_nix_engine(flake, package, engine, cancellation, &step)
                    .await;
                step.complete(&prepared, ArtifactPreparationError::is_cancelled);
                prepared
            }
        }
    }

    async fn prepare_oci_engine(
        &self,
        engine: &EngineArtifactPreparation,
        cancellation: CancellationToken,
        step: &StepOutput,
    ) -> Result<(), ArtifactPreparationError> {
        let oci_preparer = self
            .oci_preparer
            .as_ref()
            .ok_or(ArtifactPreparationError::OciPreparerNotConfigured)?;
        oci_preparer
            .prepare(oci_preparation_request(engine)?, cancellation, step.clone())
            .await
            .map_err(ArtifactPreparationError::Oci)
    }

    async fn prepare_nix_engine(
        &self,
        flake: &str,
        package: &str,
        engine: &EngineArtifactPreparation,
        cancellation: CancellationToken,
        step: &StepOutput,
    ) -> Result<(), ArtifactPreparationError> {
        let system = self
            .nix_system
            .ok_or(ArtifactPreparationError::NixSystemUnavailable)?;
        self.nix_preparer
            .prepare(
                &NixPreparationRequest {
                    flake: flake.to_owned(),
                    package: package.to_owned(),
                    system: system.to_owned(),
                    store_path: engine.launch_identity.clone(),
                    policy: engine.preparation_policy,
                },
                &self.nix.sources,
                cancellation,
                step,
            )
            .await
            .map_err(ArtifactPreparationError::Nix)
    }

    /// Builds a Docker-delivered Nix artifact's loader package at its retained store path, runs
    /// the loader, and verifies the image it realizes under the reference derived from that path.
    /// The build reports to `output` as `build`, naming the store path, and the loader as `load`,
    /// naming the image reference; a failure before the build starts reports to `build`, the
    /// artifact's first step.
    #[expect(
        clippy::too_many_arguments,
        reason = "engines and application services each supply their own steps of one artifact"
    )]
    async fn prepare_docker_image(
        &self,
        image: &DockerDeliveredNix,
        store_path: &str,
        policy: ArtifactPreparationPolicy,
        platform: &OciPlatform,
        cancellation: CancellationToken,
        output: &PreparationOutput,
        build: PreparationStep,
        load: PreparationStep,
    ) -> Result<(), DockerImagePreparationError> {
        let build = output.step(build, store_path);
        let built = async {
            let invalid_store_path = || DockerImagePreparationError::InvalidStorePath {
                store_path: store_path.to_owned(),
            };
            let launch_reference = dotcore_domain::nix::docker_launch_reference(store_path)
                .ok_or_else(invalid_store_path)?;
            let loader = dotcore_domain::nix::docker_image_loader(store_path)
                .ok_or_else(invalid_store_path)?;
            let inspector = self
                .oci_preparer
                .as_ref()
                .and_then(|preparer| preparer.as_docker_image_inspector())
                .ok_or(DockerImagePreparationError::InspectorNotConfigured)?;
            let system = self
                .nix_system
                .ok_or(DockerImagePreparationError::NixSystemUnavailable)?;
            self.nix_preparer
                .prepare(
                    &NixPreparationRequest {
                        flake: image.flake.clone(),
                        package: image.package.clone(),
                        system: system.to_owned(),
                        store_path: store_path.to_owned(),
                        policy,
                    },
                    &self.nix.sources,
                    cancellation.child_token(),
                    &build,
                )
                .await
                .map_err(DockerImagePreparationError::Nix)?;
            Ok((launch_reference, loader, inspector))
        }
        .await;
        build.complete(&built, DockerImagePreparationError::is_cancelled);
        let (launch_reference, loader, inspector) = built?;

        let load = output.step(load, launch_reference.clone());
        let loaded = self
            .docker_images
            .load(
                docker_image::DockerImageLoad {
                    package: self.nix_preparer.store_location(store_path),
                    loader,
                    launch_reference: &launch_reference,
                    platform,
                    nix: self.nix_preparer.nix_executable(),
                },
                inspector,
                &cancellation,
                &load,
            )
            .await;
        load.complete(&loaded, DockerImagePreparationError::is_cancelled);
        loaded
    }

    /// Builds Nix artifacts for `system` instead of the host's, so tests cover every system.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_nix_system(mut self, system: Option<&'static str>) -> Self {
        self.nix_system = system;
        self
    }

    /// Resolves Nix store paths under `store_root` and runs `nix` at `executable`, for fake
    /// stores in tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_nix_store(mut self, store_root: PathBuf, executable: PathBuf) -> Self {
        self.nix_preparer = self
            .nix_preparer
            .with_store_root(store_root)
            .with_executable(executable);
        self
    }

    /// Runs Docker image derivation loaders against `endpoint`, bounded by `time_limit`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_docker_image_loader(
        mut self,
        endpoint: String,
        time_limit: std::time::Duration,
    ) -> Self {
        self.docker_images = self
            .docker_images
            .with_docker_endpoint(endpoint)
            .with_time_limit(time_limit);
        self
    }
}

impl dotcore_domain::deployment::launch_pin::LaunchPinResolver for ArtifactManager {
    /// Resolves a development tag to its current image ID and verifies that exact image exists
    /// for `platform`, without pulling; evaluates a package of this node's development source to
    /// its store path for this node's Nix system, without building it; or selects a locked Nix
    /// artifact's catalog store path for this node's Nix system.
    fn resolve_pin<'a>(
        &'a self,
        source: &'a PinSource,
        platform: &'a dotcore_proto::dotcore::catalog::v1::OciPlatform,
        cancellation: CancellationToken,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, anyhow::Error>> + Send + 'a>,
    > {
        Box::pin(async move {
            match source {
                PinSource::LocalImage { reference } => {
                    self.resolve_local_image_pin(reference, platform, cancellation)
                        .await
                }
                PinSource::NixDevelopmentSource { package, .. } => self
                    .resolve_nix_pin(package, cancellation)
                    .await
                    .map_err(anyhow::Error::new),
                PinSource::NixLocked { .. } => self
                    .resolve_locked_nix_pin(source)
                    .map_err(anyhow::Error::new),
            }
        })
    }
}

impl ArtifactManager {
    async fn resolve_local_image_pin(
        &self,
        reference: &str,
        platform: &dotcore_proto::dotcore::catalog::v1::OciPlatform,
        cancellation: CancellationToken,
    ) -> Result<String, anyhow::Error> {
        let preparer = self
            .oci_preparer
            .as_ref()
            .ok_or(LocalImageIdentityResolutionError::NotConfigured)?;
        let image_id = self
            .resolve_local_image_identity(reference.to_owned())
            .await
            .map_err(|source| {
                anyhow::Error::new(source).context("local image identity resolution failed")
            })?;
        // Pinning only inspects a local image, which is no preparation step of a deployment.
        preparer
            .prepare(
                OciPreparationRequest {
                    reference: image_id.clone(),
                    platform: OciPlatform {
                        operating_system: platform.operating_system.clone(),
                        architecture: platform.architecture.clone(),
                        variant: platform.variant.clone(),
                    },
                    source: OciPreparationSource::LocalImageId,
                },
                cancellation,
                StepOutput::discard(),
            )
            .await
            .map_err(|source| {
                anyhow::Error::new(source)
                    .context("resolved local image does not match the retained platform")
            })?;
        Ok(image_id)
    }

    fn resolve_locked_nix_pin(&self, source: &PinSource) -> Result<String, NixPinError> {
        let system = self.nix_system.ok_or(NixPinError::SystemUnavailable)?;
        source
            .locked_store_path(system)
            .map(str::to_owned)
            .ok_or(NixPinError::StorePathUnavailable { system })
    }

    async fn resolve_nix_pin(
        &self,
        package: &str,
        cancellation: CancellationToken,
    ) -> Result<String, NixPinError> {
        let system = self.nix_system.ok_or(NixPinError::SystemUnavailable)?;
        let development_source = self.nix.sources.development.as_deref().ok_or_else(|| {
            NixPinError::DevelopmentSourceUnavailable {
                package: package.to_owned(),
            }
        })?;
        let installable = dotcore_domain::nix::installable(development_source, system, package);
        info!(
            event_kind = "nix_engine_artifact_evaluating",
            package,
            system,
            installable = %installable,
            "evaluating a development-source Nix artifact on this node"
        );
        let evaluation = match self
            .nix
            .evaluator
            .evaluate(&installable, cancellation)
            .await
        {
            Ok(evaluation) => evaluation,
            Err(source) => {
                warn!(
                    event_kind = "nix_engine_evaluation_failed",
                    package,
                    system,
                    installable = %installable,
                    error = %dotcore_domain::diagnostic::diagnostic_chain(&source),
                    "development-source Nix artifact evaluation failed"
                );
                return Err(NixPinError::Evaluation {
                    package: package.to_owned(),
                    system,
                    source,
                });
            }
        };
        if !dotcore_domain::nix::valid_store_path(&evaluation.store_path) {
            return Err(NixPinError::InvalidStorePath {
                package: package.to_owned(),
                store_path: evaluation.store_path,
            });
        }
        info!(
            event_kind = "nix_engine_artifact_evaluated",
            package,
            system,
            installable = %installable,
            store_path = %evaluation.store_path,
            "development-source Nix artifact evaluated to its store path"
        );
        Ok(evaluation.store_path)
    }
}

/// The Dotcore sources a SiteNode builds Nix artifacts from, the evaluator that pins
/// development-source (`self`) artifacts from its development checkout, and the root folder that
/// keeps what it builds alive. One value is shared by a node's pinning and preparation, so both
/// always use the same sources.
#[derive(Clone)]
pub struct NixSources {
    pub sources: DotcoreSources,
    pub evaluator: Arc<dyn NixEvaluator>,

    /// Folder of the node's garbage-collector roots; without one, nothing built is rooted.
    pub roots: Option<PathBuf>,
}

impl NixSources {
    /// Evaluates with the host `nix` command, with `self` standing for the development checkout,
    /// and roots nothing.
    pub fn new(sources: DotcoreSources) -> Self {
        Self {
            sources,
            evaluator: Arc::new(CommandNixEvaluator::default()),
            roots: None,
        }
    }

    /// Roots every Nix artifact the node prepares in `roots`.
    pub fn with_roots(mut self, roots: PathBuf) -> Self {
        self.roots = Some(roots);
        self
    }
}

impl Default for NixSources {
    /// No local source, so nothing is ever evaluated from a checkout and every locked revision is
    /// fetched.
    fn default() -> Self {
        Self::new(DotcoreSources::default())
    }
}

/// Exact engine artifact one launch runs, as the artifact manager prepares it: its packaging,
/// the platform it was selected for, how it is acquired, and the immutable identity that runs (a
/// digest reference, a local image ID, or a Nix store path).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineArtifactPreparation {
    pub source: EngineArtifactSource,
    pub platform: OciPlatform,
    pub preparation_policy: ArtifactPreparationPolicy,
    pub launch_identity: String,
}

/// Packaging of one engine artifact to prepare.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineArtifactSource {
    /// OCI image, acquired by its launch identity.
    OciImage,

    /// Nix flake package built and rooted in the cache, for example package `default` of `self`.
    Nix { flake: String, package: String },

    /// Docker image derivation package built and rooted in the cache, whose loader realizes the
    /// image the Docker runtime launches.
    NixDockerImage { flake: String, package: String },
}

/// Docker-delivered Nix artifact to prepare: the flake package whose store path names its loader.
struct DockerDeliveredNix {
    flake: String,
    package: String,
}

impl DockerDeliveredNix {
    /// Returns the Docker-delivered parts of a catalog Nix artifact, or `None` for a native one.
    fn from_artifact(nix: &dotcore_proto::dotcore::catalog::v1::NixArtifact) -> Option<Self> {
        let Some(Provides::DockerImage(_)) = &nix.provides else {
            return None;
        };
        Some(Self {
            flake: nix.flake.clone(),
            package: nix.package.clone()?,
        })
    }
}

/// Failures while pinning a Nix artifact, of an engine or an application service, on this node.
#[derive(Debug, thiserror::Error)]
pub enum NixPinError {
    #[error("this node has no development source to evaluate package {package:?} from")]
    DevelopmentSourceUnavailable { package: String },

    #[error("this node's host has no Nix system that Nix artifacts target")]
    SystemUnavailable,

    #[error("locked Nix artifact has no catalog store path for this node's Nix system {system}")]
    StorePathUnavailable { system: &'static str },

    #[error(
        "package {package:?} of this node's development source could not be evaluated for {system:?}"
    )]
    Evaluation {
        package: String,
        system: &'static str,
        #[source]
        source: NixEvaluationError,
    },

    #[error("package {package:?} evaluated to an invalid store path {store_path:?}")]
    InvalidStorePath { package: String, store_path: String },
}

/// Artifact-manager preparation failures whose handling is owned by the caller.
#[derive(Debug, thiserror::Error)]
pub enum ArtifactPreparationError {
    #[error("OCI image preparation is not configured")]
    OciPreparerNotConfigured,

    #[error(transparent)]
    Nix(NixPreparationError),

    #[error(transparent)]
    DockerImage(DockerImagePreparationError),

    #[error("this node's host has no Nix system that Nix engine artifacts target")]
    NixSystemUnavailable,

    #[error("engine artifact retains no preparation policy")]
    PreparationPolicyMissing,

    #[error(transparent)]
    Oci(#[from] OciPreparationError),
}

impl ArtifactPreparationError {
    fn is_cancelled(&self) -> bool {
        match self {
            Self::Nix(error) => error.is_cancelled(),
            Self::DockerImage(error) => error.is_cancelled(),
            Self::Oci(error) => error.is_cancelled(),
            _ => false,
        }
    }
}

/// Artifact-manager failures while preparing one complete application snapshot.
#[derive(Debug, thiserror::Error)]
pub enum ApplicationArtifactPreparationError {
    #[error("OCI image preparation is not configured")]
    OciPreparerNotConfigured,

    #[error("Application artifact snapshot is invalid")]
    InvalidSnapshot,

    #[error("Application service {service_name:?} artifact preparation failed")]
    Service {
        service_name: String,
        #[source]
        source: OciPreparationError,
    },

    #[error("Application service {service_name:?} Docker image preparation failed")]
    DockerImageService {
        service_name: String,
        #[source]
        source: DockerImagePreparationError,
    },
}

/// Describes the model and auxiliary artifacts of one launch, each by its repository, its file
/// when it is a single file, and its revision, in binding order.
fn model_artifacts_description(artifacts: &BTreeMap<String, ModelArtifactSource>) -> String {
    artifacts
        .values()
        .map(|artifact| match artifact {
            ModelArtifactSource::HuggingFaceFile {
                repository,
                revision,
                path,
                ..
            } => format!("{repository}/{path}@{revision}"),
            ModelArtifactSource::HuggingFaceSnapshot {
                repository,
                revision,
                ..
            } => format!("{repository}@{revision}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Returns the Nix artifact the retained graph names for a service, or `None` for an OCI image.
fn service_nix_artifact<'a>(
    snapshot: &'a ResolvedApplicationDeploymentSnapshot,
    service_name: &str,
) -> Option<&'a dotcore_proto::dotcore::catalog::v1::NixArtifact> {
    let service = snapshot
        .graph
        .as_ref()?
        .services
        .iter()
        .find(|service| service.service_name == service_name)?;
    match service.artifact.as_ref()?.kind.as_ref()? {
        ServiceArtifactKind::Nix(nix) => Some(nix),
        ServiceArtifactKind::Oci(_) => None,
    }
}

fn oci_preparation_request(
    engine: &EngineArtifactPreparation,
) -> Result<OciPreparationRequest, ArtifactPreparationError> {
    Ok(OciPreparationRequest {
        reference: engine.launch_identity.clone(),
        platform: engine.platform.clone(),
        source: preparation_source(engine.preparation_policy)
            .ok_or(ArtifactPreparationError::PreparationPolicyMissing)?,
    })
}

/// Maps a retained preparation policy to how the OCI preparer acquires the image.
fn preparation_source(policy: ArtifactPreparationPolicy) -> Option<OciPreparationSource> {
    match policy {
        ArtifactPreparationPolicy::ImmutableRemote => Some(OciPreparationSource::ImmutableRemote),
        ArtifactPreparationPolicy::LocalDevelopment => Some(OciPreparationSource::LocalImageId),
        ArtifactPreparationPolicy::DevelopmentSource | ArtifactPreparationPolicy::Unspecified => {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, sync::Mutex};

    use super::*;

    #[tokio::test]
    #[ignore = "hardware: just backend::test-docker"]
    async fn hw_docker_node_pin_resolves_a_local_tag_only_for_its_retained_platform() {
        use dotcore_domain::deployment::launch_pin::LaunchPinResolver as _;

        let docker = |arguments: &[&str]| {
            let output = std::process::Command::new("docker")
                .args(arguments)
                .output()
                .expect("docker command");
            assert!(output.status.success(), "docker {arguments:?}");
            String::from_utf8(output.stdout)
                .expect("docker output")
                .trim()
                .to_owned()
        };
        // Any small multi-platform image the Docker suite already pulls.
        const PIN_FIXTURE_IMAGE: &str =
            "busybox@sha256:73aaf090f3d85aa34ee199857f03fa3a95c8ede2ffd4cc2cdb5b94e566b11662";
        let tag = format!(
            "dotcore-test/worker-pin:{}",
            dotcore_domain::test_support::unique_suffix()
        );
        docker(&["pull", PIN_FIXTURE_IMAGE]);
        docker(&["tag", PIN_FIXTURE_IMAGE, &tag]);
        let expected = docker(&["image", "inspect", "--format", "{{.Id}}", &tag]);
        let native = docker(&["image", "inspect", "--format", "{{.Architecture}}", &tag]);
        let other = if native == "amd64" { "arm64" } else { "amd64" };
        let root = tempfile::tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
            .expect("cache root");
        let manager = ArtifactManager::new(
            root.path().to_path_buf(),
            Some(Arc::new(OciPreparer::new(Arc::new(
                DockerOciClient::connect().await.expect("Docker client"),
            )))),
        )
        .expect("artifact manager");
        let platform = |architecture: &str| dotcore_proto::dotcore::catalog::v1::OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: architecture.to_owned(),
            variant: None,
        };

        let source = PinSource::LocalImage {
            reference: tag.clone(),
        };

        let pinned = manager
            .resolve_pin(&source, &platform(&native), CancellationToken::new())
            .await;
        let mismatched = manager
            .resolve_pin(&source, &platform(other), CancellationToken::new())
            .await;
        docker(&["image", "rm", &tag]);

        assert_eq!(pinned.expect("native pin"), expected);
        assert!(
            mismatched.is_err(),
            "an image of another platform is never pinned"
        );
    }

    #[tokio::test]
    async fn facade_prepares_selected_model_auxiliary_and_engine_artifacts() {
        let root = dotcore_domain::test_support::canonical_temp_dir()
            .join(format!("dotcore-artifact-manager-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("cache root");
        let oci = Arc::new(RecordingOciPreparer::default());
        let manager = ArtifactManager::new(root.clone(), Some(oci.clone())).expect("manager");
        let artifact = ModelArtifactSource::HuggingFaceFile {
            repository: "dotcore/fixture".to_owned(),
            revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            path: "fixture-Q8_0.gguf".to_owned(),
            provenance_digest: String::new(),
        };
        let artifacts = BTreeMap::from([
            ("model".to_owned(), artifact.clone()),
            ("auxiliary".to_owned(), artifact),
        ]);
        let engine = oci_engine(
            ArtifactPreparationPolicy::ImmutableRemote,
            "registry.dotcore.test/fixture@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        );
        let expected = manager
            .expected_model_paths(&artifacts)
            .expect("cache paths");
        for path in expected.artifacts.values() {
            std::fs::create_dir_all(path.parent().expect("artifact parent"))
                .expect("artifact parent");
            std::fs::write(path, b"artifact").expect("cached artifact");
        }

        let prepared = manager
            .prepare_model(
                &artifacts,
                &CancellationToken::new(),
                None,
                None,
                &PreparationOutput::discard(),
            )
            .await
            .expect("prepared model and auxiliary artifacts");
        manager
            .prepare_engine(
                &engine,
                CancellationToken::new(),
                &PreparationOutput::discard(),
            )
            .await
            .expect("prepared engine image");

        assert_eq!(
            prepared
                .artifacts
                .artifacts
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["auxiliary", "model"])
        );
        assert_eq!(
            oci.requests(),
            vec![OciPreparationRequest {
                reference: "registry.dotcore.test/fixture@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
                platform: linux_amd64(),
                source: OciPreparationSource::ImmutableRemote,
            }]
        );
        std::fs::remove_dir_all(root).expect("cache cleanup");
    }

    #[tokio::test]
    async fn nix_engine_preparation_builds_development_sources_from_this_node_only() {
        let cache_root = tempfile::tempdir().expect("cache root");
        let engine = EngineArtifactPreparation {
            source: EngineArtifactSource::Nix {
                flake: "self".to_owned(),
                package: "engines.llama-cpp.native".to_owned(),
            },
            platform: OciPlatform {
                operating_system: "darwin".to_owned(),
                architecture: "arm64".to_owned(),
                variant: None,
            },
            preparation_policy: ArtifactPreparationPolicy::DevelopmentSource,
            launch_identity: dotcore_domain::test_support::nix::STORE_PATH.to_owned(),
        };
        let manager = ArtifactManager::new(cache_root.path().to_path_buf(), None)
            .expect("manager")
            .with_nix_sources(NixSources::default());

        let error = manager
            .prepare_engine(
                &engine,
                CancellationToken::new(),
                &PreparationOutput::discard(),
            )
            .await
            .expect_err("no development source on this node");

        assert!(matches!(
            error,
            ArtifactPreparationError::Nix(NixPreparationError::DevelopmentSourceUnavailable { .. })
        ));
    }

    #[tokio::test]
    async fn engine_preparation_follows_the_retained_policy_and_launch_identity() {
        const IMAGE_ID: &str =
            "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
        let cache_root = tempfile::tempdir().expect("cache root");
        let oci = Arc::new(RecordingOciPreparer::default());
        let manager = ArtifactManager::new(cache_root.path().to_path_buf(), Some(oci.clone()))
            .expect("manager");
        let mut engine = oci_engine(ArtifactPreparationPolicy::LocalDevelopment, IMAGE_ID);

        manager
            .prepare_engine(
                &engine,
                CancellationToken::new(),
                &PreparationOutput::discard(),
            )
            .await
            .expect("prepared local-development engine image");

        assert_eq!(
            oci.requests(),
            vec![OciPreparationRequest {
                reference: IMAGE_ID.to_owned(),
                platform: linux_amd64(),
                source: OciPreparationSource::LocalImageId,
            }]
        );

        engine.preparation_policy = ArtifactPreparationPolicy::Unspecified;
        assert!(matches!(
            manager
                .prepare_engine(
                    &engine,
                    CancellationToken::new(),
                    &PreparationOutput::discard()
                )
                .await,
            Err(ArtifactPreparationError::PreparationPolicyMissing)
        ));
        assert_eq!(oci.requests().len(), 1);
    }

    #[tokio::test]
    async fn local_image_identity_resolution_requires_a_configured_resolver() {
        let cache_root = tempfile::tempdir().expect("cache root");
        let without_preparer =
            ArtifactManager::new(cache_root.path().to_path_buf(), None).expect("manager");
        let without_resolver = ArtifactManager::new(
            cache_root.path().to_path_buf(),
            Some(Arc::new(RecordingOciPreparer::default())),
        )
        .expect("manager");

        for manager in [without_preparer, without_resolver] {
            assert!(matches!(
                manager
                    .resolve_local_image_identity("dotcore/engine:dev".to_owned())
                    .await,
                Err(LocalImageIdentityResolutionError::NotConfigured)
            ));
        }
    }

    #[tokio::test]
    async fn node_pin_evaluates_a_development_source_package_for_the_node_system() {
        use dotcore_domain::deployment::launch_pin::LaunchPinResolver as _;

        let cache_root = tempfile::tempdir().expect("cache root");
        let evaluator = Arc::new(RecordingNixEvaluator::new(Ok(
            dotcore_domain::test_support::nix::STORE_PATH,
        )));
        let manager = nix_pin_manager(&cache_root, Some("git+file:///src/dotcore"), &evaluator);

        let pinned = manager
            .resolve_pin(&engine_package(), &darwin_arm64(), CancellationToken::new())
            .await
            .expect("pinned store path");

        assert_eq!(pinned, dotcore_domain::test_support::nix::STORE_PATH);
        assert_eq!(
            evaluator.installables(),
            ["git+file:///src/dotcore#legacyPackages.aarch64-darwin.engines.llama-cpp.native"]
        );
    }

    #[tokio::test]
    async fn node_pin_selects_the_locked_store_path_of_the_node_system() {
        use dotcore_domain::deployment::launch_pin::LaunchPinResolver as _;

        const LINUX_STORE_PATH: &str =
            "/nix/store/11111111111111111111111111111111-docker-image-vllm";
        let cache_root = tempfile::tempdir().expect("cache root");
        let evaluator = Arc::new(RecordingNixEvaluator::new(Ok(LINUX_STORE_PATH)));
        let manager = nix_pin_manager(&cache_root, None, &evaluator);
        let locked = |systems: &[&str]| PinSource::NixLocked {
            store_paths: systems
                .iter()
                .map(|system| {
                    let store_path = if *system == "aarch64-darwin" {
                        dotcore_domain::test_support::nix::STORE_PATH
                    } else {
                        LINUX_STORE_PATH
                    };
                    ((*system).to_owned(), store_path.to_owned())
                })
                .collect(),
        };

        let pinned = manager
            .resolve_pin(
                &locked(&["aarch64-linux", "aarch64-darwin"]),
                &darwin_arm64(),
                CancellationToken::new(),
            )
            .await
            .expect("pinned store path");
        assert_eq!(pinned, dotcore_domain::test_support::nix::STORE_PATH);
        assert!(evaluator.installables().is_empty());

        assert!(matches!(
            manager.resolve_locked_nix_pin(&PinSource::NixLocked {
                store_paths: vec![("x86_64-linux".to_owned(), LINUX_STORE_PATH.to_owned())],
            }),
            Err(NixPinError::StorePathUnavailable {
                system: "aarch64-darwin"
            })
        ));
        assert!(matches!(
            manager
                .with_nix_system(None)
                .resolve_locked_nix_pin(&PinSource::NixLocked {
                    store_paths: Vec::new()
                }),
            Err(NixPinError::SystemUnavailable)
        ));
    }

    #[tokio::test]
    async fn node_pin_failures_are_typed_by_cause() {
        let cache_root = tempfile::tempdir().expect("cache root");
        let pin = |development_source: Option<&'static str>,
                   outcome: Result<&'static str, &'static str>| {
            let evaluator = Arc::new(RecordingNixEvaluator::new(outcome));
            let manager = nix_pin_manager(&cache_root, development_source, &evaluator);
            async move {
                let error = manager
                    .resolve_nix_pin("engines.llama-cpp.native", CancellationToken::new())
                    .await
                    .expect_err("unpinned");
                (error, evaluator.installables().len())
            }
        };

        let (without_source, evaluations) =
            pin(None, Ok(dotcore_domain::test_support::nix::STORE_PATH)).await;
        assert!(
            matches!(&without_source, NixPinError::DevelopmentSourceUnavailable { package }
                if package == "engines.llama-cpp.native"),
            "{without_source:?}"
        );
        assert_eq!(evaluations, 0);
        let (failed, _) = pin(Some("git+file:///src"), Err("error: attribute missing")).await;
        assert!(
            matches!(&failed, NixPinError::Evaluation { system: "aarch64-darwin", source, .. }
                if matches!(source, NixEvaluationError::Failed { stderr, .. }
                    if stderr == "error: attribute missing")),
            "{failed:?}"
        );
        let (invalid, _) = pin(Some("git+file:///src"), Ok("/tmp/llama-server")).await;
        assert!(
            matches!(&invalid, NixPinError::InvalidStorePath { store_path, .. }
                if store_path == "/tmp/llama-server"),
            "{invalid:?}"
        );
    }

    #[tokio::test]
    async fn facade_prepares_every_application_service_from_retained_immutable_identities() {
        use dotcore_proto::dotcore::application::v1::ResolvedApplicationServiceSelection;

        let cache_root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
            "dotcore-application-artifact-manager-{}",
            std::process::id()
        ));
        let oci = Arc::new(RecordingOciPreparer::default());
        let manager = ArtifactManager::new(cache_root.clone(), Some(oci.clone())).expect("manager");
        let snapshot = ResolvedApplicationDeploymentSnapshot {
            platform: Some(dotcore_proto::dotcore::catalog::v1::OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: "arm64".to_owned(),
                variant: None,
            }),
            service_selections: vec![
                ResolvedApplicationServiceSelection {
                    service_name: "web".to_owned(),
                    preparation_policy:
                        ArtifactPreparationPolicy::ImmutableRemote as i32,
                    launch_identity: "example.test/web@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                },
                ResolvedApplicationServiceSelection {
                    service_name: "worker".to_owned(),
                    preparation_policy:
                        ArtifactPreparationPolicy::LocalDevelopment as i32,
                    launch_identity:
                        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                            .to_owned(),
                },
            ],
            ..Default::default()
        };

        manager
            .prepare_application(
                &snapshot,
                CancellationToken::new(),
                &PreparationOutput::discard(),
            )
            .await
            .expect("prepared complete application snapshot");

        assert_eq!(
            oci.requests(),
            vec![
                OciPreparationRequest {
                    reference: snapshot.service_selections[0].launch_identity.clone(),
                    platform: OciPlatform {
                        operating_system: "linux".to_owned(),
                        architecture: "arm64".to_owned(),
                        variant: None,
                    },
                    source: OciPreparationSource::ImmutableRemote,
                },
                OciPreparationRequest {
                    reference: snapshot.service_selections[1].launch_identity.clone(),
                    platform: OciPlatform {
                        operating_system: "linux".to_owned(),
                        architecture: "arm64".to_owned(),
                        variant: None,
                    },
                    source: OciPreparationSource::LocalImageId,
                },
            ]
        );
        if cache_root.exists() {
            std::fs::remove_dir_all(cache_root).expect("cache cleanup");
        }
    }

    fn oci_engine(
        preparation_policy: ArtifactPreparationPolicy,
        launch_identity: &str,
    ) -> EngineArtifactPreparation {
        EngineArtifactPreparation {
            source: EngineArtifactSource::OciImage,
            platform: linux_amd64(),
            preparation_policy,
            launch_identity: launch_identity.to_owned(),
        }
    }

    fn linux_amd64() -> OciPlatform {
        OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "amd64".to_owned(),
            variant: None,
        }
    }

    #[derive(Default)]
    struct RecordingOciPreparer {
        requests: Mutex<Vec<OciPreparationRequest>>,
    }

    impl RecordingOciPreparer {
        fn requests(&self) -> Vec<OciPreparationRequest> {
            self.requests.lock().expect("OCI requests").clone()
        }
    }

    impl OciImagePreparer for RecordingOciPreparer {
        fn prepare(
            &self,
            request: OciPreparationRequest,
            _cancellation: CancellationToken,
            _output: StepOutput,
        ) -> OciPreparationFuture {
            self.requests.lock().expect("OCI requests").push(request);
            Box::pin(async { Ok(()) })
        }
    }

    fn nix_pin_manager(
        cache_root: &tempfile::TempDir,
        development_source: Option<&str>,
        evaluator: &Arc<RecordingNixEvaluator>,
    ) -> ArtifactManager {
        ArtifactManager::new(cache_root.path().to_path_buf(), None)
            .expect("manager")
            .with_nix_sources(NixSources {
                sources: DotcoreSources {
                    development: development_source.map(str::to_owned),
                    packaged: None,
                },
                evaluator: evaluator.clone(),
                roots: None,
            })
            .with_nix_system(Some("aarch64-darwin"))
    }

    fn engine_package() -> PinSource {
        PinSource::NixDevelopmentSource {
            package: "engines.llama-cpp.native".to_owned(),
            docker_image: false,
        }
    }

    fn darwin_arm64() -> dotcore_proto::dotcore::catalog::v1::OciPlatform {
        dotcore_proto::dotcore::catalog::v1::OciPlatform {
            operating_system: "darwin".to_owned(),
            architecture: "arm64".to_owned(),
            variant: None,
        }
    }

    /// Fake evaluator that records each installable and answers with one fixed outcome: a store
    /// path, or a failed evaluation with the given stderr.
    struct RecordingNixEvaluator {
        outcome: Result<&'static str, &'static str>,
        installables: Mutex<Vec<String>>,
    }

    impl RecordingNixEvaluator {
        fn new(outcome: Result<&'static str, &'static str>) -> Self {
            Self {
                outcome,
                installables: Mutex::new(Vec::new()),
            }
        }

        fn installables(&self) -> Vec<String> {
            self.installables.lock().expect("installables").clone()
        }
    }

    impl NixEvaluator for RecordingNixEvaluator {
        fn evaluate<'a>(
            &'a self,
            installable: &'a str,
            _cancellation: CancellationToken,
        ) -> NixEvaluationFuture<'a> {
            self.installables
                .lock()
                .expect("installables")
                .push(installable.to_owned());
            let outcome = self
                .outcome
                .map(|store_path| NixEvaluation {
                    store_path: store_path.to_owned(),
                })
                .map_err(|stderr| NixEvaluationError::Failed {
                    installable: installable.to_owned(),
                    stderr: stderr.to_owned(),
                });
            Box::pin(std::future::ready(outcome))
        }
    }
}
