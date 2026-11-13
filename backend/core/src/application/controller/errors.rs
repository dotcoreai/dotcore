//! Apps-controller lifecycle error categories and public diagnostic text.

use super::InferenceRouteResolutionError;

/// The result categories exposed by apps-controller operations.
#[derive(Debug, thiserror::Error)]
pub enum ApplicationLifecycleError {
    #[error("Application lifecycle configuration failed: {0}")]
    Configuration(#[source] anyhow::Error),

    #[error("Application lifecycle outcome is uncertain: {0}")]
    OutcomeUncertain(#[source] anyhow::Error),

    #[error("Application lifecycle request contains an invalid ID")]
    InvalidArgument,

    #[error("Application deployment name must be 1–128 non-control characters after trimming")]
    InvalidName,

    #[error("Application parameter {parameter_id:?} {reason}")]
    InvalidParameter {
        parameter_id: String,
        reason: ParameterRejection,
    },

    #[error("Application storage binding {binding_name:?} of service {service_name:?} {reason}")]
    InvalidStorageChoice {
        service_name: String,
        binding_name: String,
        reason: StorageChoiceRejection,
    },

    #[error("Application deployment was not found")]
    NotFound,

    #[error("Application lifecycle operation is not valid for the current state")]
    FailedPrecondition,

    #[error(
        "Application execution target {execution_target_id:?} on node {site_node_id:?} is not eligible: {reason}"
    )]
    IneligibleTarget {
        site_node_id: String,
        execution_target_id: String,
        reason: TargetIneligibility,
    },

    #[error("Application execution target {execution_target_id:?} has no platform observation")]
    MissingTargetObservation { execution_target_id: String },

    #[error(
        "Application execution target {execution_target_id:?} platform observation is unavailable"
    )]
    UnavailableTargetObservation { execution_target_id: String },

    #[error("Application execution target {execution_target_id:?} platform observation is stale")]
    StaleTargetObservation { execution_target_id: String },

    #[error(
        "Application execution target {execution_target_id:?} declared platform {declared} but runtime observed {observed}"
    )]
    TargetPlatformMismatch {
        execution_target_id: String,
        declared: String,
        observed: String,
    },

    #[error("Application service {service_name:?} does not support selected platform {platform}")]
    UnsupportedServicePlatform {
        service_name: String,
        platform: String,
    },

    #[error("Retained application target is incompatible with current site configuration: {0}")]
    RetainedTargetIncompatible(RetainedTargetMismatch),

    #[error("Every application published host port is taken; tear down a deployment to free one")]
    PublishedPortsExhausted,

    #[error("{0}")]
    InferenceRoute(InferenceRouteResolutionError),
}

/// Why a deployment creation request's parameter value was rejected.
#[derive(Debug, thiserror::Error)]
pub enum ParameterRejection {
    #[error("is not declared by the application")]
    Unknown,

    #[error("is supplied more than once")]
    Duplicate,

    #[error("has no value")]
    MissingValue,

    // The schema reason is the leaf of this message rather than a source, so the API message
    // carries it and full error chains do not repeat it.
    #[error("has an invalid value: {0}")]
    InvalidValue(anyhow::Error),
}

/// Why a deployment creation request's storage choice was rejected.
#[derive(Debug, thiserror::Error)]
pub enum StorageChoiceRejection {
    #[error("is not declared by the application")]
    Unknown,

    #[error("is chosen more than once")]
    Duplicate,

    #[error("does not allow placement {0}")]
    NotAllowed(String),
}

/// What differs between a deployment's retained target facts and the current site configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RetainedTargetMismatch {
    #[error("the retained placement no longer names a declared target on the same node")]
    Placement,

    #[error("the target's platform differs from the retained platform")]
    Platform,

    #[error("the target's accelerator devices differ from the retained devices")]
    Devices,
}

/// Why an exact placement names no eligible application execution target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TargetIneligibility {
    #[error("the site declares no such execution target")]
    Undeclared,

    #[error("the execution target belongs to another node")]
    OwnedByAnotherNode,

    #[error("the execution target does not run applications")]
    NotApplicationTarget,
}

pub(super) struct DeploymentReconcileFailure {
    pub(super) stage: &'static str,
    pub(super) failure: Option<DeploymentFailureCode>,
    pub(super) source: Box<dyn std::error::Error + Send + Sync>,
}

impl DeploymentReconcileFailure {
    /// Reports whether the node view lease refused a runtime action, so the deployment's
    /// observed state stays as it was instead of becoming a failure.
    pub(super) fn action_refused(&self) -> bool {
        matches!(
            self.source.downcast_ref(),
            Some(crate::dispatch::application::ApplicationRuntimeError::ActionRejected)
        )
    }
}

/// Stable failure categories a realizer records on one deployment's state, beyond its stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DeploymentFailureCode {
    /// Retained node state or unowned application data needs an explicit pause and resume.
    RecoveryRequired,

    /// A local-development tag did not resolve to a local image for the retained platform.
    LaunchIdentityUnresolved,

    /// The image of a pinned launch identity is not available on the node.
    ImageMissing,

    /// A prepared image did not match its pinned identity or the retained platform.
    ArtifactPreparationFailed,

    /// The deployment's published host port is held by something else on the node.
    PublishedPortInUse,
}

impl DeploymentFailureCode {
    /// Stable code stored in the deployment failure summary.
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::RecoveryRequired => "RecoveryRequired",
            Self::LaunchIdentityUnresolved => "LaunchIdentityUnresolved",
            Self::ImageMissing => "ImageMissing",
            Self::ArtifactPreparationFailed => "ArtifactPreparationFailed",
            Self::PublishedPortInUse => "PublishedPortInUse",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::RecoveryRequired => {
                "Application deployment requires recovery; pause and resume it to retry"
            }
            Self::LaunchIdentityUnresolved => {
                "Application development artifact did not resolve on this node"
            }
            Self::ImageMissing => "Application image is not available locally",
            Self::ArtifactPreparationFailed => {
                "Application image did not match its pinned identity or this node's platform"
            }
            Self::PublishedPortInUse => {
                "Application published port is in use on this node; free the port or delete the deployment"
            }
        }
    }

    /// Failure summary recorded on the deployment state.
    pub(super) fn summary(
        self,
    ) -> dotcore_proto::dotcore::application::v1::ApplicationDeploymentFailure {
        dotcore_proto::dotcore::application::v1::ApplicationDeploymentFailure {
            code: self.code().to_owned(),
            message: self.message().to_owned(),
            diagnostics: Vec::new(),
        }
    }
}

pub(super) fn configuration(message: impl Into<String>) -> ApplicationLifecycleError {
    ApplicationLifecycleError::Configuration(anyhow::anyhow!(message.into()))
}

pub(super) fn configuration_with_source(
    context: impl Into<String>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> ApplicationLifecycleError {
    ApplicationLifecycleError::Configuration(anyhow::Error::new(source).context(context.into()))
}

pub(super) fn outcome_uncertain_with_source(
    context: impl Into<String>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> ApplicationLifecycleError {
    ApplicationLifecycleError::OutcomeUncertain(anyhow::Error::new(source).context(context.into()))
}

pub(super) fn deployment_failure(
    stage: &'static str,
    source: impl std::error::Error + Send + Sync + 'static,
) -> DeploymentReconcileFailure {
    DeploymentReconcileFailure {
        stage,
        failure: None,
        source: Box::new(source),
    }
}

pub(super) fn coded_deployment_failure(
    stage: &'static str,
    failure: DeploymentFailureCode,
    source: impl std::error::Error + Send + Sync + 'static,
) -> DeploymentReconcileFailure {
    DeploymentReconcileFailure {
        stage,
        failure: Some(failure),
        source: Box::new(source),
    }
}
