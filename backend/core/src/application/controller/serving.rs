//! Apps-controller port for stable inference route links.
//!
//! An application deployment may link one route by its stable identity. The port projects the
//! route's current name without checking whether the route is enabled or currently serving, so
//! route availability never gates application creation or launch.

/// Current launch projection for one linked inference route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedInferenceRoute {
    pub inference_route_id: String,
    pub route_name: String,
}

/// A linked inference route cannot be projected for application use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InferenceRouteResolutionError {
    #[error("Inference route is unknown")]
    Unknown,

    #[error("Inference route does not serve chat requests")]
    NotChat,

    #[error("Inference route generation is stale")]
    Stale,

    #[error("Inference routes are unavailable on this node")]
    Unavailable,
}

/// Projects linked inference routes through route-manager-owned state.
pub trait InferenceRouteResolver: Send + Sync {
    /// Projects a route being linked at creation, requiring its exact current generation.
    fn resolve_for_create(
        &self,
        inference_route_id: &str,
        expected_generation: u64,
    ) -> Result<ResolvedInferenceRoute, InferenceRouteResolutionError>;

    /// Projects the current declaration of a retained route link.
    fn project_current(
        &self,
        inference_route_id: &str,
    ) -> Result<ResolvedInferenceRoute, InferenceRouteResolutionError>;

    #[cfg(test)]
    fn composition_route_manager_pointer(&self) -> Option<usize> {
        None
    }
}
