//! Stable inference-controller identity.

/// Identifies one controller-owned model deployment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelDeploymentId(pub String);
