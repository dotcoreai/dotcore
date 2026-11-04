//! Apps-controller deployment lifecycle, persistence, and management API boundaries.

mod controller;
mod published_port;
pub mod repository;
pub mod service;
pub(crate) mod workloads;

pub use controller::{
    ApplicationConfigurationSources, ApplicationController, ApplicationControllerConfiguration,
    ApplicationDeploymentLogController, ApplicationLifecycleController, ApplicationLifecycleError,
    InferenceRouteResolutionError, InferenceRouteResolver, ParameterRejection,
    ResolvedInferenceRoute, RetainedTargetMismatch, TargetIneligibility,
};
pub(crate) use controller::{
    ApplicationNodeViewSection, NodeApplicationComposition, NodeApplicationRealizer,
    classify_application_assignment,
};
