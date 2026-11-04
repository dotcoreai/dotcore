//! The per-backend half of stopped-site runtime cleanup: each backend that can own site resources
//! offers one adapter that reaches its control plane, lists the resources a site owns within a
//! scope, and removes them.
//!
//! The composed cleanup connects and pre-validates every adapter before any adapter removes
//! anything, so one unreachable or ambiguous runtime stops the whole cleanup unchanged.

use std::{future::Future, pin::Pin};

use dotcore_runtime::SiteRuntimeScope;

/// Boxed future of one stopped-site cleanup adapter step.
pub(crate) type SiteRuntimeResetFuture<'a, T = ()> =
    Pin<Box<dyn Future<Output = anyhow::Result<T>> + Send + 'a>>;

/// One backend's stopped-site cleanup, selected by the composed cleanup.
pub(crate) trait SiteRuntimeResetAdapter: Send + Sync {
    /// Human-readable runtime name, such as `Docker runtime`.
    fn name(&self) -> &'static str;

    /// Reaches the adapter's local control plane; failure means it is unreachable.
    fn connect(&self) -> SiteRuntimeResetFuture<'_>;

    /// Validates ownership evidence and describes the resources the site owns, without changes.
    fn prevalidate<'a>(
        &'a self,
        site_id: &'a str,
        scope: &'a SiteRuntimeScope,
    ) -> SiteRuntimeResetFuture<'a, Vec<String>>;

    /// Removes the resources the site owns within `scope`, succeeding when they are already absent.
    fn cleanup<'a>(
        &'a self,
        site_id: &'a str,
        scope: &'a SiteRuntimeScope,
    ) -> SiteRuntimeResetFuture<'a>;
}
