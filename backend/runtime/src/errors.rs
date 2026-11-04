//! Shared typed runtime operation errors and futures.

use std::{future::Future, pin::Pin};

use anyhow::anyhow;
use dotcore_domain::{ExecutionPlatform, WorkloadArtifactKind};

/// Boxed future returned by one runtime operation.
pub type RuntimeFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, RuntimeBackendError>> + Send + 'a>>;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeBackendError {
    #[error("Runtime execution-platform capability is unavailable")]
    ExecutionPlatformUnavailable,

    #[error("Invalid runtime execution-platform capability: {0}")]
    ExecutionPlatform(#[source] dotcore_domain::ExecutionPlatformError),

    #[error(
        "Runtime destination platform {observed_operating_system}/{observed_architecture} does not match retained workload platform {required_operating_system}/{required_architecture}"
    )]
    TargetPlatformMismatch {
        required_operating_system: String,
        required_architecture: String,
        observed_operating_system: String,
        observed_architecture: String,
    },

    #[error(
        "Runtime execution target {observed_execution_target_id:?} does not match retained execution target {required_execution_target_id:?}"
    )]
    ExecutionTargetMismatch {
        required_execution_target_id: String,
        observed_execution_target_id: String,
    },

    #[error(
        "Runtime image {image:?} platform {observed} does not match retained workload platform {required}"
    )]
    ImagePlatformMismatch {
        image: String,
        required: ExecutionPlatform,
        observed: ExecutionPlatform,
    },

    #[error("Docker runtime failure: {0}")]
    Docker(#[source] anyhow::Error),

    #[error("Native runtime failure: {0}")]
    Native(#[source] anyhow::Error),

    #[error("Runtime local image is missing: {0}")]
    ImageMissing(#[source] anyhow::Error),

    #[error("Runtime published host port is in use on this node")]
    PublishedPortInUse(#[source] anyhow::Error),

    #[error("Invalid runtime launch: {0}")]
    InvalidLaunch(#[source] anyhow::Error),

    #[error("Runtime {backend_id} cannot launch {kind} executables")]
    UnsupportedExecutableArtifact {
        backend_id: &'static str,
        kind: WorkloadArtifactKind,
    },

    #[error("Invalid runtime workload ownership: {0}")]
    Ownership(#[source] anyhow::Error),

    #[error("Runtime realization preparation was cancelled")]
    PreparationCancelled,

    #[error("Runtime host-shared IPC capability is unavailable")]
    HostSharedIpcUnavailable,

    #[error("Runtime direct diagnostics capability is unavailable")]
    LogCapabilityUnavailable,

    #[error("Runtime workload has not been created yet")]
    WorkloadNotCreated,

    #[error("Runtime direct diagnostics request was cancelled")]
    LogCancelled,

    #[error("Runtime diagnostics on the hosting worker are unavailable")]
    RemoteLogUnavailable,

    #[error("Runtime direct diagnostics source capacity is exhausted")]
    LogSourceLimitReached,

    #[error("Invalid runtime workload observation: {0}")]
    Observation(#[source] anyhow::Error),
}

impl RuntimeBackendError {
    /// A launch the backend refuses before acting, with a source-less reason.
    pub fn invalid_launch(message: impl Into<String>) -> Self {
        Self::InvalidLaunch(anyhow!(message.into()))
    }

    /// A required image that is absent locally and must not be pulled.
    pub fn local_image_missing(message: impl Into<String>) -> Self {
        Self::ImageMissing(anyhow!(message.into()))
    }

    pub fn is_local_image_missing(&self) -> bool {
        matches!(self, Self::ImageMissing(_))
    }

    /// A requested published host port that something else on the node already holds.
    pub fn published_port_in_use_with_source(
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::PublishedPortInUse(anyhow::Error::new(source).context(message.into()))
    }

    pub fn is_published_port_in_use(&self) -> bool {
        matches!(self, Self::PublishedPortInUse(_))
    }

    /// A preparation stopped by its caller's cancellation.
    pub fn preparation_cancelled() -> Self {
        Self::PreparationCancelled
    }

    /// A host-shared IPC request on a backend that cannot provide it.
    pub fn host_shared_ipc_unavailable() -> Self {
        Self::HostSharedIpcUnavailable
    }

    pub fn is_preparation_cancelled(&self) -> bool {
        matches!(self, Self::PreparationCancelled)
    }

    pub fn is_log_cancelled(&self) -> bool {
        matches!(self, Self::LogCancelled)
    }

    /// A diagnostics read on a backend without direct log access.
    pub fn log_capability_unavailable() -> Self {
        Self::LogCapabilityUnavailable
    }

    /// A diagnostics read for a workload the backend never created.
    pub fn workload_not_created() -> Self {
        Self::WorkloadNotCreated
    }

    /// A diagnostics read whose hosting node could not be reached.
    pub fn remote_log_unavailable() -> Self {
        Self::RemoteLogUnavailable
    }

    /// A Docker failure with source-independent context over its concrete source.
    pub fn docker_with_source(
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Docker(anyhow::Error::new(source).context(message.into()))
    }

    /// A native runtime failure with source-independent context over its concrete source.
    pub fn native_with_source(
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Native(anyhow::Error::new(source).context(message.into()))
    }

    /// A missing image with source-independent context over its concrete source.
    pub fn image_missing_with_source(
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::ImageMissing(anyhow::Error::new(source).context(message.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_less_runtime_errors_have_stable_capability_and_cancellation_messages() {
        let unavailable = RuntimeBackendError::log_capability_unavailable();
        let cancelled = RuntimeBackendError::LogCancelled;
        let exhausted = RuntimeBackendError::LogSourceLimitReached;

        let host_ipc = RuntimeBackendError::host_shared_ipc_unavailable();
        assert_eq!(
            unavailable.to_string(),
            "Runtime direct diagnostics capability is unavailable"
        );
        assert_eq!(
            cancelled.to_string(),
            "Runtime direct diagnostics request was cancelled"
        );
        assert_eq!(
            exhausted.to_string(),
            "Runtime direct diagnostics source capacity is exhausted"
        );
        assert_eq!(
            host_ipc.to_string(),
            "Runtime host-shared IPC capability is unavailable"
        );
        assert!(!unavailable.is_log_cancelled());
        assert!(cancelled.is_log_cancelled());
    }

    #[test]
    fn sourced_runtime_leaves_keep_context_separate_from_the_concrete_source() {
        for (error, context, concrete_source) in [
            (
                RuntimeBackendError::docker_with_source(
                    "Docker client connection failed",
                    std::io::Error::other("daemon unavailable"),
                ),
                "Docker client connection failed",
                "daemon unavailable",
            ),
            (
                RuntimeBackendError::image_missing_with_source(
                    "Docker image inspection failed",
                    std::io::Error::other("image unavailable"),
                ),
                "Docker image inspection failed",
                "image unavailable",
            ),
        ] {
            let source = std::error::Error::source(&error).expect("contextual anyhow source");
            assert_eq!(source.to_string(), context);
            assert_eq!(
                source.source().expect("concrete source").to_string(),
                concrete_source
            );
            assert!(!source.to_string().contains("unavailable"));
        }
    }
}
