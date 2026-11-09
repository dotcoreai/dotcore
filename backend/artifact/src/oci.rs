//! Prepares the OCI images that model and application deployments launch from.
//!
//! An immutable remote image is pulled when absent and verified against its pinned digest and
//! platform. A local-development image is only inspected by its retained image ID, for example
//! `sha256:<64 hex>`, and fails when that exact image is absent; it is never pulled. A pull reports
//! each layer's status changes as lines, and an image already present reports nothing. Callers own
//! cancellation, and a development tag is resolved to its current image ID on request.

use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};

use bollard::{Docker, errors::Error as BollardError, query_parameters::CreateImageOptionsBuilder};
use dotcore_domain::docker::DOCKER_OPERATION_TIMEOUT;
use dotcore_domain::oci::{
    OciReferenceKind, canonical_digest_reference, valid_local_image_id, validate_rendered_reference,
};
use dotcore_domain::real_time;
use tokio_stream::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;

use super::{
    DockerImageFacts, DockerImageInspectionFuture, DockerImageInspector,
    output::{OutputStream, StepOutput},
};

/// Injectable private preparation boundary for one catalog-selected OCI image.
pub trait OciImagePreparer: Send + Sync {
    /// Prepares the image, reporting a pull to `output`. An image already present reports
    /// nothing, so its step ends as already prepared.
    fn prepare(
        &self,
        request: OciPreparationRequest,
        cancellation: CancellationToken,
        output: StepOutput,
    ) -> OciPreparationFuture;

    fn as_local_image_identity_resolver(&self) -> Option<&dyn LocalImageIdentityResolver> {
        None
    }

    fn as_docker_image_inspector(&self) -> Option<&dyn DockerImageInspector> {
        None
    }
}

pub type OciPreparationFuture =
    Pin<Box<dyn Future<Output = Result<(), OciPreparationError>> + Send>>;

/// Artifact-manager boundary that resolves a mutable local tag to its current immutable image ID.
pub trait LocalImageIdentityResolver: Send + Sync {
    fn resolve_local_image_identity(&self, reference: String)
    -> LocalImageIdentityResolutionFuture;
}

pub type LocalImageIdentityResolutionFuture =
    Pin<Box<dyn Future<Output = Result<String, LocalImageIdentityResolutionError>> + Send>>;

/// Artifact-owned OCI preparer backed by a local image client.
pub struct OciPreparer {
    client: Arc<dyn OciClient>,
    pull_mutex: Arc<tokio::sync::Mutex<()>>,
}

impl OciPreparer {
    pub fn new(client: Arc<dyn OciClient>) -> Self {
        Self {
            client,
            pull_mutex: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    async fn prepare_image(
        &self,
        request: OciPreparationRequest,
        cancellation: &CancellationToken,
        output: &StepOutput,
    ) -> Result<(), OciPreparationError> {
        validate_request(&request)?;
        if let Some(image) = inspect_image(&*self.client, &request, cancellation).await? {
            if image_matches(&image, &request) {
                return Ok(());
            }
            if request.source == OciPreparationSource::LocalImageId {
                return Err(OciPreparationError::VerificationMismatch);
            }
        }
        match request.source {
            OciPreparationSource::ImmutableRemote => {
                self.pull_and_verify(&request, cancellation, output).await
            }
            OciPreparationSource::LocalImageId => Err(OciPreparationError::LocalImageMissing),
        }
    }

    async fn pull_and_verify(
        &self,
        request: &OciPreparationRequest,
        cancellation: &CancellationToken,
        output: &StepOutput,
    ) -> Result<(), OciPreparationError> {
        output.start();
        let _guard = match self.pull_mutex.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                output.waiting();
                let lock = self.pull_mutex.lock();
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Err(OciPreparationError::Cancelled),
                    guard = lock => guard,
                }
            }
        };
        if cancellation.is_cancelled() {
            return Err(OciPreparationError::Cancelled);
        }
        if inspect_image(&*self.client, request, cancellation)
            .await?
            .is_some_and(|image| image_matches(&image, request))
        {
            return Ok(());
        }

        let pull = self.client.pull(request);
        let mut stream = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(OciPreparationError::Cancelled),
            result = pull => result.map_err(|error| OciPreparationError::PullStream(error.0))?,
        };
        let mut statuses = PullStatuses::default();
        loop {
            let event = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(OciPreparationError::Cancelled),
                event = stream.next() => event,
            };
            match event {
                Some(Ok(OciPullEvent::Progress { layer, status })) => {
                    if let Some(line) = statuses.change(layer, status) {
                        output.line(OutputStream::Stdout, line);
                    }
                }
                Some(Ok(OciPullEvent::DaemonError { source })) => {
                    return Err(OciPreparationError::Daemon(source));
                }
                Some(Err(error)) => return Err(OciPreparationError::PullStream(error.0)),
                None => break,
            }
        }
        if inspect_image(&*self.client, request, cancellation)
            .await?
            .is_some_and(|image| image_matches(&image, request))
        {
            Ok(())
        } else {
            Err(OciPreparationError::VerificationMismatch)
        }
    }
}

impl LocalImageIdentityResolver for OciPreparer {
    fn resolve_local_image_identity(
        &self,
        reference: String,
    ) -> LocalImageIdentityResolutionFuture {
        let client = Arc::clone(&self.client);
        Box::pin(async move {
            let kind = validate_rendered_reference(&reference)
                .map_err(|_| LocalImageIdentityResolutionError::UnsupportedReference)?;
            if !matches!(kind, OciReferenceKind::TagOnly) {
                return Err(LocalImageIdentityResolutionError::UnsupportedReference);
            }
            let image_id = client
                .resolve_local_image_id(reference)
                .await
                .map_err(|error| LocalImageIdentityResolutionError::Inspection(error.0))?
                .ok_or(LocalImageIdentityResolutionError::LocalImageMissing)?;
            valid_local_image_id(&image_id)
                .then_some(image_id)
                .ok_or(LocalImageIdentityResolutionError::InvalidImageIdentity)
        })
    }
}

impl OciImagePreparer for OciPreparer {
    fn prepare(
        &self,
        request: OciPreparationRequest,
        cancellation: CancellationToken,
        output: StepOutput,
    ) -> OciPreparationFuture {
        let client = Arc::clone(&self.client);
        let pull_mutex = Arc::clone(&self.pull_mutex);
        Box::pin(async move {
            OciPreparer { client, pull_mutex }
                .prepare_image(request, &cancellation, &output)
                .await
        })
    }

    fn as_local_image_identity_resolver(&self) -> Option<&dyn LocalImageIdentityResolver> {
        Some(self)
    }

    fn as_docker_image_inspector(&self) -> Option<&dyn DockerImageInspector> {
        Some(self)
    }
}

impl DockerImageInspector for OciPreparer {
    fn inspect_docker_image(&self, reference: String) -> DockerImageInspectionFuture {
        let client = Arc::clone(&self.client);
        Box::pin(async move {
            client
                .inspect_docker_image(reference)
                .await
                .map_err(|error| error.0)
        })
    }
}

/// Immutable OCI image and concrete platform selected by a deployment snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OciPreparationRequest {
    pub reference: String,
    pub platform: OciPlatform,
    pub source: OciPreparationSource,
}

/// Acquisition policy attached to one immutable OCI preparation request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OciPreparationSource {
    ImmutableRemote,

    LocalImageId,
}

/// Concrete OCI platform used for inspection and pull selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OciPlatform {
    pub operating_system: String,
    pub architecture: String,
    pub variant: Option<String>,
}

impl OciPlatform {
    /// Reports whether an image realized for `realized` runs on this retained platform. The
    /// operating system, architecture, and variant must match, except that arm64 `v8` counts as
    /// the default arm64 variant, as containerd treats it: official arm64 images declare `v8`
    /// while retained platforms carry no variant.
    pub fn is_satisfied_by(&self, realized: &OciPlatform) -> bool {
        let default_arm64_variant = self.architecture == "arm64"
            && self.variant.is_none()
            && realized.variant.as_deref() == Some("v8");
        self.operating_system == realized.operating_system
            && self.architecture == realized.architecture
            && (self.variant == realized.variant || default_arm64_variant)
    }
}

/// Locally inspected OCI image identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OciImage {
    pub reference: String,
    pub platform: OciPlatform,
    pub digest: Option<String>,
    pub image_id: Option<String>,
}

/// One Docker pull-stream event.
#[derive(Debug)]
pub enum OciPullEvent {
    Progress { layer: String, status: String },

    DaemonError { source: anyhow::Error },
}

/// OCI preparation failures that the controller maps to its stable deployment failure.
#[derive(Debug, thiserror::Error)]
pub enum OciPreparationError {
    #[error("OCI image reference must be an exact SHA-256 digest reference or local image ID")]
    UnsupportedReference,

    #[error("retained OCI image ID is not available in the local image store")]
    LocalImageMissing,

    #[error("OCI image platform must name an operating system and architecture")]
    UnsupportedPlatform,

    #[error("OCI image preparation was cancelled")]
    Cancelled,

    #[error("OCI image inspection failed")]
    Inspection(#[source] anyhow::Error),

    #[error("OCI image pull stream failed")]
    PullStream(#[source] anyhow::Error),

    #[error("OCI image pull was rejected by the daemon")]
    Daemon(#[source] anyhow::Error),

    #[error("OCI image identity or platform did not match the deployment snapshot")]
    VerificationMismatch,
}

impl OciPreparationError {
    pub(crate) fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

/// Failure while resolving a local development tag to an immutable Docker image ID.
#[derive(Debug, thiserror::Error)]
pub enum LocalImageIdentityResolutionError {
    #[error("local OCI image identity resolution is not configured on this node")]
    NotConfigured,

    #[error("local OCI image reference must contain an explicit tag")]
    UnsupportedReference,

    #[error("tagged OCI image is not available in the local image store")]
    LocalImageMissing,

    #[error("local OCI image inspection failed")]
    Inspection(#[source] anyhow::Error),

    #[error("local OCI image has no immutable SHA-256 image ID")]
    InvalidImageIdentity,
}

type OciFuture<T> = Pin<Box<dyn Future<Output = Result<T, OciClientError>> + Send>>;
type OciPullStream = Pin<Box<dyn Stream<Item = Result<OciPullEvent, OciClientError>> + Send>>;

/// Private Docker-facing port, isolated so tests require no daemon or registry.
pub trait OciClient: Send + Sync {
    fn inspect(&self, request: &OciPreparationRequest) -> OciFuture<Option<OciImage>>;

    fn resolve_local_image_id(&self, _reference: String) -> OciFuture<Option<String>> {
        Box::pin(async {
            Err(OciClientError(anyhow::anyhow!(
                "local image identity resolution is unavailable"
            )))
        })
    }

    fn pull(&self, request: &OciPreparationRequest) -> OciFuture<OciPullStream>;

    fn inspect_docker_image(&self, _reference: String) -> OciFuture<Option<DockerImageFacts>> {
        Box::pin(async {
            Err(OciClientError(anyhow::anyhow!(
                "Docker image inspection is unavailable"
            )))
        })
    }
}

#[derive(Debug, thiserror::Error)]
#[error("OCI client operation failed")]
pub struct OciClientError(#[source] pub anyhow::Error);

/// Anonymous local Docker adapter for the private OCI preparation port.
pub struct DockerOciClient {
    docker: Docker,
}

impl DockerOciClient {
    /// Connects to the local Docker daemon and negotiates its API version, as the Docker runtime
    /// does, so a daemon older than the client's default API version is still served.
    pub async fn connect() -> Result<Self, OciClientError> {
        let docker = Docker::connect_with_unix(
            &dotcore_domain::docker::local_endpoint(),
            dotcore_domain::docker::DOCKER_CLIENT_TIMEOUT_SECONDS,
            bollard::API_DEFAULT_VERSION,
        )
        .map_err(|source| OciClientError(anyhow::Error::new(source)))?;
        let docker = bounded("Docker API version negotiation", docker.negotiate_version())
            .await?
            .map_err(|source| {
                OciClientError(
                    anyhow::Error::new(source).context("Docker API version negotiation failed"),
                )
            })?;
        Ok(Self { docker })
    }
}

impl OciClient for DockerOciClient {
    fn inspect(&self, request: &OciPreparationRequest) -> OciFuture<Option<OciImage>> {
        let docker = self.docker.clone();
        let request = request.clone();
        Box::pin(async move {
            match bounded(
                "Docker image inspection",
                docker.inspect_image(&request.reference),
            )
            .await?
            {
                Ok(image) => Ok(Some(OciImage {
                    reference: request.reference.clone(),
                    platform: OciPlatform {
                        operating_system: image.os.unwrap_or_default(),
                        architecture: image.architecture.unwrap_or_default(),
                        variant: image.variant,
                    },
                    digest: image.repo_digests.and_then(|digests| {
                        canonical_digest_reference(&request.reference)
                            .ok()
                            .flatten()
                            .filter(|expected| {
                                digests
                                    .iter()
                                    .any(|digest| docker_daemon_digest_matches(expected, digest))
                            })
                    }),
                    image_id: image.id,
                })),
                Err(BollardError::DockerResponseServerError {
                    status_code: 404, ..
                }) => Ok(None),
                Err(source) => Err(OciClientError(anyhow::Error::new(source))),
            }
        })
    }

    fn resolve_local_image_id(&self, reference: String) -> OciFuture<Option<String>> {
        let docker = self.docker.clone();
        Box::pin(async move {
            match bounded("Docker image inspection", docker.inspect_image(&reference)).await? {
                Ok(image) => Ok(image.id),
                Err(BollardError::DockerResponseServerError {
                    status_code: 404, ..
                }) => Ok(None),
                Err(source) => Err(OciClientError(anyhow::Error::new(source))),
            }
        })
    }

    fn inspect_docker_image(&self, reference: String) -> OciFuture<Option<DockerImageFacts>> {
        let docker = self.docker.clone();
        Box::pin(async move {
            match bounded("Docker image inspection", docker.inspect_image(&reference)).await? {
                Ok(image) => Ok(Some(DockerImageFacts {
                    image_id: image.id,
                    platform: OciPlatform {
                        operating_system: image.os.unwrap_or_default(),
                        architecture: image.architecture.unwrap_or_default(),
                        variant: image.variant,
                    },
                    diff_ids: image
                        .root_fs
                        .and_then(|root_fs| root_fs.layers)
                        .unwrap_or_default(),
                    labels: image
                        .config
                        .and_then(|config| config.labels)
                        .unwrap_or_default()
                        .into_iter()
                        .collect(),
                })),
                Err(BollardError::DockerResponseServerError {
                    status_code: 404, ..
                }) => Ok(None),
                Err(source) => Err(OciClientError(anyhow::Error::new(source))),
            }
        })
    }

    fn pull(&self, request: &OciPreparationRequest) -> OciFuture<OciPullStream> {
        let options = CreateImageOptionsBuilder::new()
            .from_image(&request.reference)
            .platform(&docker_platform(&request.platform))
            .build();
        let stream = self
            .docker
            .create_image(Some(options), None, None)
            .map(|event| match event {
                Ok(event) => Ok(match event.status {
                    Some(status) => OciPullEvent::Progress {
                        layer: event.id.unwrap_or_default(),
                        status,
                    },
                    None => OciPullEvent::Progress {
                        layer: String::new(),
                        status: String::new(),
                    },
                }),
                Err(source @ BollardError::DockerStreamError { .. }) => {
                    Ok(OciPullEvent::DaemonError {
                        source: anyhow::Error::new(source),
                    })
                }
                Err(source) => Err(OciClientError(anyhow::Error::new(source))),
            });
        let stream: OciPullStream = Box::pin(stream);
        Box::pin(async move { Ok(stream) })
    }
}

impl crate::ImageCleanupDocker for DockerOciClient {
    fn images(&self) -> crate::CleanupFuture<'_, Vec<crate::LocalImage>> {
        Box::pin(async move {
            let options = bollard::query_parameters::ListImagesOptionsBuilder::new()
                .all(false)
                .build();
            let images = bounded(
                "Docker image listing",
                self.docker.list_images(Some(options)),
            )
            .await?
            .map_err(|source| anyhow::Error::new(source).context("list Docker images"))?;
            Ok(images
                .into_iter()
                .map(|image| crate::LocalImage {
                    id: image.id,
                    // The classic image store reports an untagged image as `<none>:<none>`.
                    tags: image
                        .repo_tags
                        .into_iter()
                        .filter(|tag| tag != "<none>:<none>")
                        .collect(),
                    digests: image
                        .repo_digests
                        .into_iter()
                        .filter(|digest| !digest.starts_with("<none>@"))
                        .collect(),
                    labels: image.labels.into_iter().collect(),
                })
                .collect())
        })
    }

    fn container_images(&self) -> crate::CleanupFuture<'_, std::collections::BTreeSet<String>> {
        Box::pin(async move {
            let options = bollard::query_parameters::ListContainersOptionsBuilder::new()
                .all(true)
                .build();
            let containers = bounded(
                "Docker container listing",
                self.docker.list_containers(Some(options)),
            )
            .await?
            .map_err(|source| anyhow::Error::new(source).context("list Docker containers"))?;
            Ok(containers
                .into_iter()
                .filter_map(|container| container.image_id)
                .collect())
        })
    }

    fn remove(&self, reference: String) -> crate::CleanupFuture<'_, ()> {
        Box::pin(async move {
            let options = bollard::query_parameters::RemoveImageOptionsBuilder::new()
                .force(false)
                .build();
            match bounded(
                "Docker image removal",
                self.docker.remove_image(&reference, Some(options), None),
            )
            .await?
            {
                // The containerd image store drops a name's own digest reference with the name.
                Ok(_)
                | Err(BollardError::DockerResponseServerError {
                    status_code: 404, ..
                }) => Ok(()),
                Err(source) => {
                    Err(anyhow::Error::new(source)
                        .context(format!("remove Docker image {reference}")))
                }
            }
        })
    }
}

/// Last status each layer of one pull reported, so only status changes become lines and the
/// repeated progress ticks of a download or extraction do not.
#[derive(Default)]
struct PullStatuses {
    statuses: HashMap<String, String>,
}

impl PullStatuses {
    /// Records a layer's status and returns its line when the status changed, for example
    /// `0123abcd: Pull complete`, or the bare status of an event naming no layer.
    fn change(&mut self, layer: String, status: String) -> Option<String> {
        if status.is_empty() || self.statuses.get(&layer) == Some(&status) {
            return None;
        }
        let line = if layer.is_empty() {
            status.clone()
        } else {
            format!("{layer}: {status}")
        };
        self.statuses.insert(layer, status);
        Some(line)
    }
}

async fn inspect_image(
    client: &dyn OciClient,
    request: &OciPreparationRequest,
    cancellation: &CancellationToken,
) -> Result<Option<OciImage>, OciPreparationError> {
    let inspect = client.inspect(request);
    let image = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(OciPreparationError::Cancelled),
        result = inspect => result.map_err(|error| OciPreparationError::Inspection(error.0))?,
    };
    Ok(image)
}

fn validate_request(request: &OciPreparationRequest) -> Result<(), OciPreparationError> {
    let valid_reference = match request.source {
        OciPreparationSource::ImmutableRemote => has_digest(&request.reference),
        OciPreparationSource::LocalImageId => valid_local_image_id(&request.reference),
    };
    if !valid_reference {
        return Err(OciPreparationError::UnsupportedReference);
    }
    if request.platform.operating_system.is_empty() || request.platform.architecture.is_empty() {
        return Err(OciPreparationError::UnsupportedPlatform);
    }
    Ok(())
}

fn has_digest(reference: &str) -> bool {
    matches!(
        validate_rendered_reference(reference),
        Ok(OciReferenceKind::DigestOnly | OciReferenceKind::TagAndDigest)
    )
}

fn docker_platform(platform: &OciPlatform) -> String {
    let base = format!("{}/{}", platform.operating_system, platform.architecture);
    platform
        .variant
        .as_ref()
        .map_or(base.clone(), |variant| format!("{base}/{variant}"))
}

fn image_matches(image: &OciImage, request: &OciPreparationRequest) -> bool {
    image.reference == request.reference
        && request.platform.is_satisfied_by(&image.platform)
        && match request.source {
            OciPreparationSource::ImmutableRemote => canonical_digest_reference(&request.reference)
                .ok()
                .flatten()
                .is_some_and(|expected| image.digest.as_deref() == Some(expected.as_str())),
            OciPreparationSource::LocalImageId => {
                image.image_id.as_deref() == Some(request.reference.as_str())
            }
        }
}

#[cfg(test)]
fn image_matches_fields(
    repo_digests: &Option<Vec<String>>,
    operating_system: &Option<String>,
    architecture: &Option<String>,
    variant: &Option<String>,
    request: &OciPreparationRequest,
) -> bool {
    let identity_matches = match canonical_digest_reference(&request.reference) {
        Ok(Some(expected)) => repo_digests.as_ref().is_some_and(|digests| {
            digests
                .iter()
                .any(|digest| docker_daemon_digest_matches(&expected, digest))
        }),
        Ok(None) => true,
        Err(_) => false,
    };
    let realized = OciPlatform {
        operating_system: operating_system.clone().unwrap_or_default(),
        architecture: architecture.clone().unwrap_or_default(),
        variant: variant.clone(),
    };
    identity_matches && request.platform.is_satisfied_by(&realized)
}

/// Bounds one Docker Engine request, so a daemon that stops answering fails the operation instead
/// of hanging it. The image pull stream is not bounded; cancellation stops it.
async fn bounded<F: Future>(
    operation: &'static str,
    request: F,
) -> Result<F::Output, OciClientError> {
    real_time::bound(DOCKER_OPERATION_TIMEOUT, request)
        .await
        .map_err(|source| {
            OciClientError(anyhow::Error::new(source).context(format!("{operation} timed out")))
        })
}

fn docker_daemon_digest_matches(expected: &str, reported: &str) -> bool {
    // Docker omits the Docker Hub registry from RepoDigests even when the inspected
    // reference names docker.io explicitly.
    docker_hub_digest_identity(expected) == docker_hub_digest_identity(reported)
}

fn docker_hub_digest_identity(reference: &str) -> &str {
    reference
        .strip_prefix("docker.io/")
        .or_else(|| reference.strip_prefix("index.docker.io/"))
        .unwrap_or(reference)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, VecDeque},
        error::Error as _,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll},
        time::Duration,
    };

    use super::*;
    use crate::output::{PreparationEventKind, PreparationStep, test_support::RecordedOutput};

    #[tokio::test(start_paused = true)]
    async fn docker_requests_fail_once_their_timeout_elapses() {
        let error = bounded("Docker image inspection", std::future::pending::<()>())
            .await
            .expect_err("pending Docker request times out");

        assert_eq!(
            format!("{:#}", error.0),
            "Docker image inspection timed out: deadline has elapsed"
        );
        assert!(error.0.root_cause().is::<tokio::time::error::Elapsed>());
    }

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const OTHER_DIGEST: &str =
        "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

    #[tokio::test]
    async fn warm_exact_image_inspects_once_without_a_pull_or_network_operation() {
        let request = request();
        let client = FakeOciClient::new([Ok(Some(image(&request)))], no_streams());
        let preparer = OciPreparer::new(Arc::new(client.clone()));

        preparer
            .prepare_image(request.clone(), &CancellationToken::new(), &discarded())
            .await
            .expect("warm image preparation");

        assert_eq!(client.calls(), vec![Call::Inspect(request)]);
        assert_eq!(client.pull_count(), 0);
        assert_eq!(client.network_operations(), 0);
    }

    #[tokio::test]
    async fn cold_exact_image_reinspects_pulls_to_eof_and_verifies_digest_and_platform() {
        let request = request();
        let client = FakeOciClient::new(
            [Ok(None), Ok(None), Ok(Some(image(&request)))],
            [[
                Ok(OciPullEvent::Progress {
                    layer: "layer-a".to_owned(),
                    status: "Downloading".to_owned(),
                }),
                Ok(OciPullEvent::Progress {
                    layer: "layer-a".to_owned(),
                    status: "Pull complete".to_owned(),
                }),
            ]],
        );
        let preparer = OciPreparer::new(Arc::new(client.clone()));

        preparer
            .prepare_image(request.clone(), &CancellationToken::new(), &discarded())
            .await
            .expect("cold image preparation");

        assert_eq!(
            client.calls(),
            vec![
                Call::Inspect(request.clone()),
                Call::Inspect(request.clone()),
                Call::Pull(request.clone()),
                Call::PullEvent("layer-a".to_owned(), "Downloading".to_owned()),
                Call::PullEvent("layer-a".to_owned(), "Pull complete".to_owned()),
                Call::Inspect(request),
            ]
        );
        assert_eq!(client.pull_count(), 1);
        assert_eq!(client.maximum_active_pulls(), 1);
    }

    #[tokio::test]
    async fn a_pull_reports_each_layer_status_change_once_and_a_present_image_already_prepared() {
        let request = request();
        let pull = |layer: &str, status: &str| {
            Ok(OciPullEvent::Progress {
                layer: layer.to_owned(),
                status: status.to_owned(),
            })
        };
        let client = FakeOciClient::new(
            [Ok(None), Ok(None), Ok(Some(image(&request)))],
            [[
                pull("", "Pulling from example/engine"),
                pull("layer-a", "Pulling fs layer"),
                pull("layer-b", "Pulling fs layer"),
                pull("layer-a", "Downloading"),
                pull("layer-a", "Downloading"),
                pull("layer-b", "Downloading"),
                pull("layer-a", "Downloading"),
                pull("layer-a", "Download complete"),
                pull("layer-a", "Extracting"),
                pull("layer-a", "Extracting"),
                pull("layer-a", "Pull complete"),
                pull("layer-b", "Downloading"),
                pull("layer-b", "Pull complete"),
                pull("", ""),
                pull("", &format!("Digest: {DIGEST}")),
            ]],
        );
        let cold = RecordedOutput::new();
        let warm = RecordedOutput::new();
        // Each preparation completes its step as the artifact manager does.
        let prepare = async |client: FakeOciClient, recorded: &RecordedOutput| {
            let step = recorded
                .output()
                .step(PreparationStep::EngineImage, request.reference.clone());
            let result = OciPreparer::new(Arc::new(client))
                .prepare_image(request.clone(), &CancellationToken::new(), &step)
                .await;
            step.complete(&result, OciPreparationError::is_cancelled);
            result
        };

        prepare(client, &cold).await.expect("pulled image");
        prepare(
            FakeOciClient::new([Ok(Some(image(&request)))], no_streams()),
            &warm,
        )
        .await
        .expect("present image");

        let line = |text: &str| {
            (
                PreparationStep::EngineImage,
                PreparationEventKind::Line {
                    stream: OutputStream::Stdout,
                    text: text.to_owned(),
                },
            )
        };
        assert_eq!(
            cold.kinds(),
            [
                (
                    PreparationStep::EngineImage,
                    PreparationEventKind::Started {
                        artifact: request.reference.clone()
                    }
                ),
                line("Pulling from example/engine"),
                line("layer-a: Pulling fs layer"),
                line("layer-b: Pulling fs layer"),
                line("layer-a: Downloading"),
                line("layer-b: Downloading"),
                line("layer-a: Download complete"),
                line("layer-a: Extracting"),
                line("layer-a: Pull complete"),
                line("layer-b: Pull complete"),
                line(&format!("Digest: {DIGEST}")),
                (
                    PreparationStep::EngineImage,
                    PreparationEventKind::Finished {
                        duration: Duration::ZERO
                    }
                ),
            ]
        );
        assert_eq!(
            warm.kinds(),
            [(
                PreparationStep::EngineImage,
                PreparationEventKind::AlreadyPrepared {
                    artifact: request.reference.clone()
                }
            )]
        );
    }

    #[tokio::test]
    async fn preparation_selects_execution_children_from_index_and_manifest_publications() {
        let index = PublicationFixture::index(include_bytes!(
            "../../fixtures/oci/multi-arch-with-attestations.oci-index.json"
        ));
        assert_eq!(
            index.execution_platforms(),
            vec![arm64_platform(), platform()]
        );

        for (publication, selected_platform) in [
            (index.clone(), arm64_platform()),
            (
                PublicationFixture::manifest(arm64_platform()),
                arm64_platform(),
            ),
            (index, platform()),
        ] {
            let mut selected = request();
            selected.platform = selected_platform.clone();
            let client = PublicationFixtureClient::new(publication);
            let preparer = OciPreparer::new(Arc::new(client.clone()));

            preparer
                .prepare_image(selected.clone(), &CancellationToken::new(), &discarded())
                .await
                .expect("selected publication child");

            assert_eq!(client.selected_platforms(), vec![selected_platform]);
            assert_eq!(client.realized_image(), Some(image(&selected)));
        }
    }

    #[tokio::test]
    async fn cold_pull_rejects_realized_platform_mismatch_before_launch() {
        let request = request();
        let mut mismatched = image(&request);
        mismatched.platform.architecture = "arm64".to_owned();
        let client = FakeOciClient::new(
            [
                Ok(Some(mismatched.clone())),
                Ok(Some(mismatched.clone())),
                Ok(Some(mismatched)),
            ],
            [std::iter::empty::<Result<OciPullEvent, OciClientError>>()],
        );
        let preparer = OciPreparer::new(Arc::new(client));

        assert!(matches!(
            preparer
                .prepare_image(request, &CancellationToken::new(), &discarded())
                .await,
            Err(OciPreparationError::VerificationMismatch)
        ));
    }

    #[tokio::test]
    async fn realized_arm64_v8_satisfies_a_variant_free_arm64_platform() {
        let mut request = request();
        request.platform = arm64_platform();
        let mut realized = image(&request);
        realized.platform.variant = Some("v8".to_owned());
        let client = FakeOciClient::new([Ok(Some(realized))], no_streams());
        let preparer = OciPreparer::new(Arc::new(client.clone()));

        preparer
            .prepare_image(request.clone(), &CancellationToken::new(), &discarded())
            .await
            .expect("arm64 v8 image preparation");

        assert_eq!(client.calls(), vec![Call::Inspect(request)]);
        assert_eq!(client.pull_count(), 0);
    }

    #[tokio::test]
    async fn realized_variants_other_than_arm64_v8_are_rejected_after_pull() {
        for (architecture, variant) in [("arm64", "v9"), ("arm", "v7"), ("amd64", "v8")] {
            let mut request = request();
            request.platform.architecture = architecture.to_owned();
            let mut realized = image(&request);
            realized.platform.variant = Some(variant.to_owned());
            let client = FakeOciClient::new(
                [
                    Ok(Some(realized.clone())),
                    Ok(Some(realized.clone())),
                    Ok(Some(realized)),
                ],
                [std::iter::empty::<Result<OciPullEvent, OciClientError>>()],
            );
            let preparer = OciPreparer::new(Arc::new(client.clone()));

            assert!(
                matches!(
                    preparer
                        .prepare_image(request, &CancellationToken::new(), &discarded())
                        .await,
                    Err(OciPreparationError::VerificationMismatch)
                ),
                "{architecture}/{variant}"
            );
            assert_eq!(client.pull_count(), 1, "{architecture}/{variant}");
        }
    }

    #[test]
    fn retained_platform_accepts_only_its_exact_variant_or_default_arm64_v8() {
        let retained = |architecture: &str, variant: Option<&str>| OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: architecture.to_owned(),
            variant: variant.map(str::to_owned),
        };
        for (wanted, realized, satisfied) in [
            (retained("arm64", None), retained("arm64", None), true),
            (retained("arm64", None), retained("arm64", Some("v8")), true),
            (
                retained("arm64", None),
                retained("arm64", Some("v9")),
                false,
            ),
            (retained("arm64", None), retained("amd64", None), false),
            (
                retained("amd64", None),
                retained("amd64", Some("v8")),
                false,
            ),
            (retained("arm", None), retained("arm", Some("v7")), false),
            (
                retained("arm", Some("v7")),
                retained("arm", Some("v7")),
                true,
            ),
            (
                retained("arm64", Some("v8")),
                retained("arm64", None),
                false,
            ),
        ] {
            assert_eq!(
                wanted.is_satisfied_by(&realized),
                satisfied,
                "{wanted:?} by {realized:?}"
            );
        }
        let mut other_os = retained("arm64", Some("v8"));
        other_os.operating_system = "darwin".to_owned();
        assert!(!retained("arm64", None).is_satisfied_by(&other_os));
    }

    #[tokio::test]
    async fn serializes_cold_pulls_process_locally() {
        let first = request();
        let second = OciPreparationRequest {
            reference: format!("example.test/engine@{OTHER_DIGEST}"),
            platform: platform(),
            source: OciPreparationSource::ImmutableRemote,
        };
        let client = FakeOciClient::with_cold_images(
            [image(&first), image(&second)],
            [[Ok(progress())], [Ok(progress())]],
        );
        let preparer = Arc::new(OciPreparer::new(Arc::new(client.clone())));

        let first_cancellation = CancellationToken::new();
        let second_cancellation = CancellationToken::new();
        let (first_output, second_output) = (discarded(), discarded());
        let (first_result, second_result) = tokio::join!(
            preparer.prepare_image(first, &first_cancellation, &first_output),
            preparer.prepare_image(second, &second_cancellation, &second_output)
        );

        first_result.expect("first preparation");
        second_result.expect("second preparation");
        assert_eq!(client.pull_count(), 2);
        assert_eq!(client.maximum_active_pulls(), 1);
    }

    #[tokio::test]
    async fn operational_failures_retain_concrete_causes_under_stable_categories() {
        let request = request();
        let inspection = OciPreparer::new(Arc::new(FakeOciClient::new(
            [Err(client_failure("inspection"))],
            no_streams(),
        )))
        .prepare_image(request.clone(), &CancellationToken::new(), &discarded())
        .await
        .expect_err("inspection failure");
        assert_preparation_failure(&inspection, "OCI image inspection failed", "inspection");

        let pull_stream = OciPreparer::new(Arc::new(FakeOciClient::new(
            [Ok(None), Ok(None)],
            [[Err(client_failure("pull stream"))]],
        )))
        .prepare_image(request.clone(), &CancellationToken::new(), &discarded())
        .await
        .expect_err("pull stream failure");
        assert_preparation_failure(&pull_stream, "OCI image pull stream failed", "pull stream");

        let daemon = OciPreparer::new(Arc::new(FakeOciClient::new(
            [Ok(None), Ok(None)],
            [[Ok(OciPullEvent::DaemonError {
                source: anyhow::Error::new(InjectedOciFailure("daemon")),
            })]],
        )))
        .prepare_image(request, &CancellationToken::new(), &discarded())
        .await
        .expect_err("daemon failure");
        assert_preparation_failure(
            &daemon,
            "OCI image pull was rejected by the daemon",
            "daemon",
        );
    }

    #[tokio::test]
    async fn retained_local_image_id_is_inspected_exactly_without_pulling_its_source_tag() {
        let request = local_id_request();
        let client = FakeOciClient::new([Ok(Some(image(&request)))], no_streams());
        let preparer = OciPreparer::new(Arc::new(client.clone()));

        preparer
            .prepare_image(request.clone(), &CancellationToken::new(), &discarded())
            .await
            .expect("retained local image preparation");

        assert_eq!(client.calls(), vec![Call::Inspect(request)]);
        assert_eq!(client.pull_count(), 0);
        assert_eq!(client.network_operations(), 0);
    }

    #[tokio::test]
    async fn retained_local_image_id_rejects_a_missing_image_without_a_pull() {
        let request = local_id_request();
        let client = FakeOciClient::new([Ok(None)], no_streams());
        let preparer = OciPreparer::new(Arc::new(client.clone()));

        assert!(matches!(
            preparer
                .prepare_image(request.clone(), &CancellationToken::new(), &discarded())
                .await,
            Err(OciPreparationError::LocalImageMissing)
        ));
        assert_eq!(client.calls(), vec![Call::Inspect(request)]);
        assert_eq!(client.pull_count(), 0);
        assert_eq!(client.network_operations(), 0);
    }

    #[tokio::test]
    async fn retained_local_image_id_rejects_another_image_or_platform_without_a_pull() {
        let request = local_id_request();
        let mut other_image = image(&request);
        other_image.image_id = Some(OTHER_DIGEST.to_owned());
        let mut wrong_platform = image(&request);
        wrong_platform.platform.architecture = "arm64".to_owned();
        for inspection in [other_image, wrong_platform] {
            let client = FakeOciClient::new([Ok(Some(inspection))], no_streams());
            let preparer = OciPreparer::new(Arc::new(client.clone()));

            assert!(matches!(
                preparer
                    .prepare_image(request.clone(), &CancellationToken::new(), &discarded())
                    .await,
                Err(OciPreparationError::VerificationMismatch)
            ));
            assert_eq!(client.calls(), vec![Call::Inspect(request.clone())]);
            assert_eq!(client.pull_count(), 0);
            assert_eq!(client.network_operations(), 0);
        }
    }

    #[tokio::test]
    async fn local_tag_resolution_returns_the_current_immutable_docker_image_id() {
        let client = FakeOciClient::new([], no_streams());
        let resolver = OciPreparer::new(Arc::new(client));

        let identity = resolver
            .resolve_local_image_identity("example.test/application:dev".to_owned())
            .await
            .expect("local immutable identity");

        assert_eq!(
            identity,
            "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
        );
    }

    #[test]
    fn inspection_matches_variant_present_and_absent_platforms() {
        let absent_variant = request();
        let digests = Some(vec![absent_variant.reference.clone()]);
        assert!(image_matches_fields(
            &digests,
            &Some("linux".to_owned()),
            &Some("amd64".to_owned()),
            &None,
            &absent_variant,
        ));

        let present_variant = OciPreparationRequest {
            platform: OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: "arm".to_owned(),
                variant: Some("v7".to_owned()),
            },
            ..request()
        };
        assert!(image_matches_fields(
            &digests,
            &Some("linux".to_owned()),
            &Some("arm".to_owned()),
            &Some("v7".to_owned()),
            &present_variant,
        ));
        assert!(!image_matches_fields(
            &digests,
            &Some("linux".to_owned()),
            &Some("arm".to_owned()),
            &None,
            &present_variant,
        ));
    }

    #[test]
    fn docker_pull_platform_includes_an_optional_variant() {
        assert_eq!(docker_platform(&platform()), "linux/amd64");
        assert_eq!(
            docker_platform(&OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: "arm".to_owned(),
                variant: Some("v7".to_owned()),
            }),
            "linux/arm/v7",
        );
    }

    #[test]
    fn digest_verification_uses_exact_repository_digest_for_combined_references() {
        let request = OciPreparationRequest {
            reference: format!("example.test/engine:release@{DIGEST}"),
            platform: platform(),
            source: OciPreparationSource::ImmutableRemote,
        };
        assert!(image_matches_fields(
            &Some(vec![format!("example.test/engine@{DIGEST}")]),
            &Some("linux".to_owned()),
            &Some("amd64".to_owned()),
            &None,
            &request,
        ));
        assert!(!image_matches_fields(
            &Some(vec![format!("example.test/engine@{OTHER_DIGEST}")]),
            &Some("linux".to_owned()),
            &Some("amd64".to_owned()),
            &None,
            &request,
        ));
    }

    #[test]
    fn digest_verification_accepts_docker_hub_daemon_repository_normalization() {
        let request = OciPreparationRequest {
            reference: format!("docker.io/vllm/vllm-openai@{DIGEST}"),
            platform: platform(),
            source: OciPreparationSource::ImmutableRemote,
        };

        assert!(image_matches_fields(
            &Some(vec![format!("vllm/vllm-openai@{DIGEST}")]),
            &Some("linux".to_owned()),
            &Some("amd64".to_owned()),
            &None,
            &request,
        ));
        assert!(!image_matches_fields(
            &Some(vec![format!("other/vllm-openai@{DIGEST}")]),
            &Some("linux".to_owned()),
            &Some("amd64".to_owned()),
            &None,
            &request,
        ));
    }

    #[tokio::test]
    async fn rejects_malformed_reference_and_platform_without_docker_or_network_calls() {
        let client = FakeOciClient::new([], no_streams());
        let preparer = OciPreparer::new(Arc::new(client.clone()));
        let invalid_reference = OciPreparationRequest {
            reference: "example.test/engine".to_owned(),
            platform: platform(),
            source: OciPreparationSource::ImmutableRemote,
        };
        let tag_as_local_image_id = OciPreparationRequest {
            reference: "example.test/engine:dev".to_owned(),
            platform: platform(),
            source: OciPreparationSource::LocalImageId,
        };
        let invalid_platform = OciPreparationRequest {
            reference: format!("example.test/engine@{DIGEST}"),
            platform: OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: String::new(),
                variant: None,
            },
            source: OciPreparationSource::ImmutableRemote,
        };

        for unsupported in [invalid_reference, tag_as_local_image_id] {
            assert!(matches!(
                preparer
                    .prepare_image(unsupported, &CancellationToken::new(), &discarded())
                    .await,
                Err(OciPreparationError::UnsupportedReference)
            ));
        }
        assert!(matches!(
            preparer
                .prepare_image(invalid_platform, &CancellationToken::new(), &discarded())
                .await,
            Err(OciPreparationError::UnsupportedPlatform)
        ));
        assert_eq!(client.calls(), Vec::new());
        assert_eq!(client.network_operations(), 0);
    }

    #[tokio::test]
    async fn cancelled_waiter_does_not_pull() {
        let request = request();
        let client = FakeOciClient::new([Ok(None)], no_streams());
        let preparer = OciPreparer::new(Arc::new(client.clone()));
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        assert!(matches!(
            preparer
                .prepare_image(request, &cancellation, &discarded())
                .await,
            Err(OciPreparationError::Cancelled)
        ));
        assert_eq!(client.calls(), Vec::new());
        assert_eq!(client.pull_count(), 0);
    }

    #[tokio::test]
    async fn active_cancellation_drops_pull_stream_and_skips_post_inspection() {
        let request = request();
        let client = FakeOciClient::with_blocked_stream([Ok(None), Ok(None)]);
        let preparer = Arc::new(OciPreparer::new(Arc::new(client.clone())));
        let cancellation = CancellationToken::new();
        let preparation = tokio::spawn({
            let preparer = preparer.clone();
            let request = request.clone();
            let cancellation = cancellation.clone();
            async move {
                preparer
                    .prepare_image(request, &cancellation, &discarded())
                    .await
            }
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            client.wait_pull_started(),
        )
        .await
        .expect("active pull stream");

        cancellation.cancel();
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(1), preparation)
                .await
                .expect("cancelled preparation")
                .expect("preparation task"),
            Err(OciPreparationError::Cancelled)
        ));
        assert_eq!(
            client.calls(),
            vec![
                Call::Inspect(request.clone()),
                Call::Inspect(request.clone()),
                Call::Pull(request),
            ]
        );
        assert_eq!(client.dropped_streams(), 1);
    }

    fn assert_preparation_failure(
        error: &OciPreparationError,
        expected_display: &str,
        expected_operation: &'static str,
    ) {
        assert_eq!(error.to_string(), expected_display);
        let mut source = error.source();
        let mut failure = None;
        while let Some(current) = source {
            failure = current.downcast_ref::<InjectedOciFailure>();
            if failure.is_some() {
                break;
            }
            source = current.source();
        }
        assert_eq!(
            failure.map(|failure| failure.0),
            Some(expected_operation),
            "concrete OCI failure"
        );
    }

    fn client_failure(operation: &'static str) -> OciClientError {
        OciClientError(anyhow::Error::new(InjectedOciFailure(operation)))
    }

    #[derive(Debug, thiserror::Error)]
    #[error("injected OCI {0} failure")]
    struct InjectedOciFailure(&'static str);

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Call {
        Inspect(OciPreparationRequest),

        Pull(OciPreparationRequest),

        PullEvent(String, String),
    }

    #[derive(Clone)]
    struct PublicationFixture {
        document: Arc<serde_json::Value>,
        manifest_platform: Option<OciPlatform>,
    }

    impl PublicationFixture {
        fn index(document: &[u8]) -> Self {
            Self {
                document: Arc::new(
                    serde_json::from_slice(document).expect("OCI index publication fixture"),
                ),
                manifest_platform: None,
            }
        }

        fn manifest(platform: OciPlatform) -> Self {
            Self {
                document: Arc::new(serde_json::json!({
                    "schemaVersion": 2,
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "config": {
                        "mediaType": "application/vnd.oci.image.config.v1+json",
                        "digest": DIGEST,
                        "size": 1
                    },
                    "layers": []
                })),
                manifest_platform: Some(platform),
            }
        }

        fn execution_platforms(&self) -> Vec<OciPlatform> {
            self.document
                .get("manifests")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|manifest| {
                    let platform = manifest.get("platform")?;
                    let operating_system = platform.get("os")?.as_str()?;
                    let architecture = platform.get("architecture")?.as_str()?;
                    (operating_system != "unknown" && architecture != "unknown").then(|| {
                        OciPlatform {
                            operating_system: operating_system.to_owned(),
                            architecture: architecture.to_owned(),
                            variant: platform
                                .get("variant")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_owned),
                        }
                    })
                })
                .collect()
        }

        fn contains(&self, platform: &OciPlatform) -> bool {
            match self
                .document
                .get("mediaType")
                .and_then(serde_json::Value::as_str)
            {
                Some("application/vnd.oci.image.index.v1+json") => {
                    self.execution_platforms().contains(platform)
                }
                Some("application/vnd.oci.image.manifest.v1+json") => {
                    self.manifest_platform.as_ref() == Some(platform)
                }
                _ => false,
            }
        }
    }

    #[derive(Clone)]
    struct PublicationFixtureClient {
        publication: PublicationFixture,
        selected_platforms: Arc<Mutex<Vec<OciPlatform>>>,
        realized_image: Arc<Mutex<Option<OciImage>>>,
    }

    impl PublicationFixtureClient {
        fn new(publication: PublicationFixture) -> Self {
            Self {
                publication,
                selected_platforms: Arc::new(Mutex::new(Vec::new())),
                realized_image: Arc::new(Mutex::new(None)),
            }
        }

        fn selected_platforms(&self) -> Vec<OciPlatform> {
            self.selected_platforms
                .lock()
                .expect("selected platforms")
                .clone()
        }

        fn realized_image(&self) -> Option<OciImage> {
            self.realized_image.lock().expect("realized image").clone()
        }
    }

    impl OciClient for PublicationFixtureClient {
        fn inspect(&self, _: &OciPreparationRequest) -> OciFuture<Option<OciImage>> {
            let image = self.realized_image();
            Box::pin(async move { Ok(image) })
        }

        fn pull(&self, request: &OciPreparationRequest) -> OciFuture<OciPullStream> {
            self.selected_platforms
                .lock()
                .expect("selected platforms")
                .push(request.platform.clone());
            if self.publication.contains(&request.platform) {
                *self.realized_image.lock().expect("realized image") = Some(image(request));
            }
            let stream: OciPullStream = Box::pin(tokio_stream::iter([Ok(progress())]));
            Box::pin(async move { Ok(stream) })
        }
    }

    #[derive(Clone)]
    struct FakeOciClient {
        state: Arc<Mutex<FakeState>>,
        pull_count: Arc<AtomicUsize>,
        active_pulls: Arc<AtomicUsize>,
        maximum_active_pulls: Arc<AtomicUsize>,
        dropped_streams: Arc<AtomicUsize>,
        blocked_stream: bool,
        pull_started: Arc<tokio::sync::Notify>,
    }

    impl FakeOciClient {
        fn new(
            inspections: impl IntoIterator<Item = Result<Option<OciImage>, OciClientError>>,
            streams: impl IntoIterator<
                Item = impl IntoIterator<Item = Result<OciPullEvent, OciClientError>>,
            >,
        ) -> Self {
            Self {
                state: Arc::new(Mutex::new(FakeState {
                    inspections: inspections.into_iter().collect(),
                    streams: streams
                        .into_iter()
                        .map(|stream| stream.into_iter().collect())
                        .collect(),
                    calls: Vec::new(),
                    cold_images: BTreeMap::new(),
                })),
                pull_count: Arc::new(AtomicUsize::new(0)),
                active_pulls: Arc::new(AtomicUsize::new(0)),
                maximum_active_pulls: Arc::new(AtomicUsize::new(0)),
                dropped_streams: Arc::new(AtomicUsize::new(0)),
                blocked_stream: false,
                pull_started: Arc::new(tokio::sync::Notify::new()),
            }
        }

        fn with_cold_images(
            images: impl IntoIterator<Item = OciImage>,
            streams: impl IntoIterator<
                Item = impl IntoIterator<Item = Result<OciPullEvent, OciClientError>>,
            >,
        ) -> Self {
            let client = Self::new([], streams);
            let mut state = client.state.lock().expect("fake state");
            state.cold_images = images
                .into_iter()
                .map(|image| (image.reference.clone(), (image, 0)))
                .collect();
            drop(state);
            client
        }

        fn with_blocked_stream(
            inspections: impl IntoIterator<Item = Result<Option<OciImage>, OciClientError>>,
        ) -> Self {
            let mut client = Self::new(
                inspections,
                [std::iter::empty::<Result<OciPullEvent, OciClientError>>()],
            );
            client.blocked_stream = true;
            client
        }

        async fn wait_pull_started(&self) {
            self.pull_started.notified().await;
        }

        fn calls(&self) -> Vec<Call> {
            self.state.lock().expect("fake state").calls.clone()
        }

        fn pull_count(&self) -> usize {
            self.pull_count.load(Ordering::SeqCst)
        }

        fn network_operations(&self) -> usize {
            self.pull_count()
        }

        fn maximum_active_pulls(&self) -> usize {
            self.maximum_active_pulls.load(Ordering::SeqCst)
        }

        fn dropped_streams(&self) -> usize {
            self.dropped_streams.load(Ordering::SeqCst)
        }
    }

    impl OciClient for FakeOciClient {
        fn inspect(&self, request: &OciPreparationRequest) -> OciFuture<Option<OciImage>> {
            let state = self.state.clone();
            let request = request.clone();
            Box::pin(async move {
                let mut state = state.lock().expect("fake state");
                state.calls.push(Call::Inspect(request.clone()));
                if let Some((image, inspections)) = state.cold_images.get_mut(&request.reference) {
                    *inspections += 1;
                    return Ok((*inspections >= 3).then(|| image.clone()));
                }
                state.inspections.pop_front().expect("planned inspection")
            })
        }

        fn resolve_local_image_id(&self, _reference: String) -> OciFuture<Option<String>> {
            Box::pin(async {
                Ok(Some(
                    "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
                        .to_owned(),
                ))
            })
        }

        fn pull(&self, request: &OciPreparationRequest) -> OciFuture<OciPullStream> {
            let state = self.state.clone();
            let request = request.clone();
            let pull_count = self.pull_count.clone();
            let active_pulls = self.active_pulls.clone();
            let maximum_active_pulls = self.maximum_active_pulls.clone();
            let dropped_streams = self.dropped_streams.clone();
            let blocked = self.blocked_stream;
            let pull_started = self.pull_started.clone();
            Box::pin(async move {
                let events = {
                    let mut state = state.lock().expect("fake state");
                    state.calls.push(Call::Pull(request));
                    state.streams.pop_front().expect("planned pull stream")
                };
                pull_count.fetch_add(1, Ordering::SeqCst);
                let active = active_pulls.fetch_add(1, Ordering::SeqCst) + 1;
                maximum_active_pulls.fetch_max(active, Ordering::SeqCst);
                Ok(Box::pin(FakePullStream {
                    events,
                    state,
                    active_pulls,
                    dropped_streams,
                    blocked,
                    pull_started,
                    complete: false,
                }) as OciPullStream)
            })
        }
    }

    struct FakeState {
        inspections: VecDeque<Result<Option<OciImage>, OciClientError>>,
        streams: VecDeque<VecDeque<Result<OciPullEvent, OciClientError>>>,
        calls: Vec<Call>,
        cold_images: BTreeMap<String, (OciImage, usize)>,
    }

    struct FakePullStream {
        events: VecDeque<Result<OciPullEvent, OciClientError>>,
        state: Arc<Mutex<FakeState>>,
        active_pulls: Arc<AtomicUsize>,
        dropped_streams: Arc<AtomicUsize>,
        blocked: bool,
        pull_started: Arc<tokio::sync::Notify>,
        complete: bool,
    }

    impl Stream for FakePullStream {
        type Item = Result<OciPullEvent, OciClientError>;

        fn poll_next(
            mut self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            if self.blocked {
                self.pull_started.notify_waiters();
                return Poll::Pending;
            }
            let event = self.events.pop_front();
            if let Some(Ok(OciPullEvent::Progress { layer, status })) = &event {
                self.state
                    .lock()
                    .expect("fake state")
                    .calls
                    .push(Call::PullEvent(layer.clone(), status.clone()));
            }
            if event.is_none() {
                self.complete = true;
            }
            Poll::Ready(event)
        }
    }

    impl Drop for FakePullStream {
        fn drop(&mut self) {
            self.active_pulls.fetch_sub(1, Ordering::SeqCst);
            if !self.complete {
                self.dropped_streams.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    fn local_id_request() -> OciPreparationRequest {
        OciPreparationRequest {
            reference: "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
                .to_owned(),
            platform: platform(),
            source: OciPreparationSource::LocalImageId,
        }
    }

    fn request() -> OciPreparationRequest {
        OciPreparationRequest {
            reference: format!("example.test/engine@{DIGEST}"),
            platform: platform(),
            source: OciPreparationSource::ImmutableRemote,
        }
    }

    fn platform() -> OciPlatform {
        OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "amd64".to_owned(),
            variant: None,
        }
    }

    fn arm64_platform() -> OciPlatform {
        OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "arm64".to_owned(),
            variant: None,
        }
    }

    fn image(request: &OciPreparationRequest) -> OciImage {
        OciImage {
            reference: request.reference.clone(),
            platform: request.platform.clone(),
            digest: canonical_digest_reference(&request.reference)
                .ok()
                .flatten(),
            image_id: (request.source == OciPreparationSource::LocalImageId)
                .then(|| request.reference.clone()),
        }
    }

    fn discarded() -> StepOutput {
        StepOutput::discard()
    }

    fn no_streams() -> Vec<Vec<Result<OciPullEvent, OciClientError>>> {
        Vec::new()
    }

    fn progress() -> OciPullEvent {
        OciPullEvent::Progress {
            layer: "layer-a".to_owned(),
            status: "Pull complete".to_owned(),
        }
    }
}
