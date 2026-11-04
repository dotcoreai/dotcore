//! Runtime backend registry: the canonical backend identifiers, the stopped-site cleanup composed
//! from the Docker, native and simulated backends a site configuration binds, and the set of
//! runtime-specific site cleanups selected by runtime binding kind.
//!
//! Cleanup covers every runtime the site configuration binds, and each of them must be
//! reachable or the operation stops before anything changes. Docker is also cleaned when the
//! site no longer binds it, because an earlier configuration may have left labeled
//! resources behind; an unreachable unbound Docker is skipped and reported to the caller.
//! Native workloads are not cleaned here: removing a site directory or node folder cleans them by
//! their location in it, whatever the configuration binds.

use std::{
    collections::BTreeSet,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
};

use anyhow::Context;
use dotcore_domain::SiteNodeId;
use dotcore_runtime::{
    LocatedWorkloadCleanup, RuntimeBindingKind, SiteRuntimeCleanup, SiteRuntimeCleanupReport,
    SiteRuntimeCleanups, SiteRuntimeInventory, SiteRuntimeResetConfigurationError,
    SiteRuntimeResource, SiteRuntimeScope, SkippedSiteRuntimeCleanup,
};
use tracing::debug;

use crate::site_reset::{SiteRuntimeResetAdapter, SiteRuntimeResetFuture};

/// Canonical runtime backend identifier for Docker-owned workloads.
pub const DOCKER_RUNTIME_BACKEND_ID: &str = crate::docker::BACKEND_ID;
/// Canonical runtime backend identifier for launcher-held native host-process workloads.
pub const NATIVE_RUNTIME_BACKEND_ID: &str = crate::native::BACKEND_ID;
/// Canonical runtime backend identifier for deterministic simulated workloads.
pub const SIMULATED_RUNTIME_BACKEND_ID: &str = crate::simulated::BACKEND_ID;

/// The backends' site cleanups: Docker and simulated stopped-site cleanup, native workloads by
/// their location, and Docker removal of root-owned site data.
#[derive(Clone, Debug)]
pub struct BackendSiteRuntimeCleanups {
    cache_root: Option<PathBuf>,
}

impl BackendSiteRuntimeCleanups {
    /// Creates the cleanups of a node whose recreatable caches live below `cache_root`.
    pub fn new(cache_root: impl Into<PathBuf>) -> Self {
        Self {
            cache_root: Some(cache_root.into()),
        }
    }

    /// Creates cleanups for a node with no resolvable cache root, which therefore holds no
    /// workload caches to remove.
    pub fn without_cache_root() -> Self {
        Self { cache_root: None }
    }
}

impl SiteRuntimeCleanups for BackendSiteRuntimeCleanups {
    fn site_cleanup(
        &self,
        bound: &BTreeSet<RuntimeBindingKind>,
    ) -> Result<Box<dyn SiteRuntimeCleanup>, SiteRuntimeResetConfigurationError> {
        Ok(Box::new(SiteRuntimeReset::new(bound)))
    }

    fn located_workloads(
        &self,
        root: &Path,
        site_id: Option<String>,
        site_node_id: Option<SiteNodeId>,
    ) -> Box<dyn LocatedWorkloadCleanup> {
        Box::new(crate::native::NativeSiteCleanup::new(
            root,
            site_id,
            site_node_id,
        ))
    }

    fn remove_root_owned_site_data<'a>(
        &'a self,
        site_root: &'a Path,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(crate::docker::cleanup_root_owned_site_data(site_root))
    }

    fn remove_workload_caches<'a>(
        &'a self,
        site_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            match &self.cache_root {
                Some(cache_root) => {
                    crate::docker::remove_site_workload_caches(cache_root, site_id).await
                }
                None => Ok(()),
            }
        })
    }
}

/// Exact stopped-site cleanup composed from the runtime adapters a site configuration binds.
/// Adapter selection is inert so local control planes are contacted only while reset holds the site lock.
pub struct SiteRuntimeReset {
    adapters: Vec<SelectedAdapter>,
}

impl SiteRuntimeReset {
    /// Selects cleanup for the runtimes bound with these kinds without contacting them. Bound
    /// adapters are required; Docker is added as optional when unbound. The native backend needs
    /// no adapter, because removing the directory cleans native workloads by their location in it.
    pub fn new(bound: &BTreeSet<RuntimeBindingKind>) -> Self {
        let mut adapters = Vec::new();
        if bound.contains(&RuntimeBindingKind::Simulated) {
            adapters.push(SelectedAdapter::required(Box::new(
                SimulatedSiteRuntimeReset,
            )));
        }

        // Unbound Docker is still cleaned when reachable: the site may have used it earlier.
        let docker = crate::docker::site_reset_adapter();
        adapters.push(if bound.contains(&RuntimeBindingKind::Docker) {
            SelectedAdapter::required(docker)
        } else {
            SelectedAdapter::optional(docker)
        });
        Self { adapters }
    }

    #[cfg(test)]
    fn from_adapters(adapters: Vec<SelectedAdapter>) -> Self {
        Self { adapters }
    }
}

impl SiteRuntimeReset {
    /// Connects every selected adapter, failing on an unreachable required one and reporting an
    /// unreachable optional one as skipped.
    async fn reachable_adapters(
        &self,
        site_id: &str,
    ) -> anyhow::Result<(
        Vec<&dyn SiteRuntimeResetAdapter>,
        Vec<SkippedSiteRuntimeCleanup>,
    )> {
        let mut skipped = Vec::new();
        let mut reachable = Vec::with_capacity(self.adapters.len());
        for selected in &self.adapters {
            let adapter = selected.adapter.as_ref();
            match adapter.connect().await {
                Ok(()) => reachable.push(adapter),
                Err(reason) if !selected.required => {
                    debug!(event_kind = "site_runtime_cleanup_skipped", site_id, runtime = adapter.name(), error = %format!("{reason:#}"), "unbound runtime is unreachable; site cleanup skipped");
                    skipped.push(SkippedSiteRuntimeCleanup {
                        runtime: adapter.name(),
                        reason,
                    });
                }
                Err(reason) => {
                    return Err(
                        reason.context(format!("reach {} for site cleanup", adapter.name()))
                    );
                }
            }
        }
        Ok((reachable, skipped))
    }

    /// Validates ownership evidence on every reachable adapter and gathers the resources found.
    async fn prevalidated_resources(
        reachable: &[&dyn SiteRuntimeResetAdapter],
        site_id: &str,
        scope: &SiteRuntimeScope,
    ) -> anyhow::Result<Vec<SiteRuntimeResource>> {
        let mut resources = Vec::new();
        for adapter in reachable {
            let descriptions = adapter
                .prevalidate(site_id, scope)
                .await
                .with_context(|| format!("pre-validate {} site cleanup", adapter.name()))?;
            resources.extend(
                descriptions
                    .into_iter()
                    .map(|description| SiteRuntimeResource {
                        runtime: adapter.name(),
                        description,
                    }),
            );
        }
        Ok(resources)
    }
}

impl SiteRuntimeCleanup for SiteRuntimeReset {
    fn inventory<'a>(
        &'a self,
        site_id: &'a str,
        scope: &'a SiteRuntimeScope,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<SiteRuntimeInventory>> + Send + 'a>> {
        Box::pin(async move {
            let (reachable, skipped) = self.reachable_adapters(site_id).await?;
            let resources = Self::prevalidated_resources(&reachable, site_id, scope).await?;
            Ok(SiteRuntimeInventory { resources, skipped })
        })
    }

    fn cleanup<'a>(
        &'a self,
        site_id: &'a str,
        scope: &'a SiteRuntimeScope,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<SiteRuntimeCleanupReport>> + Send + 'a>> {
        Box::pin(async move {
            let (reachable, skipped) = self.reachable_adapters(site_id).await?;
            Self::prevalidated_resources(&reachable, site_id, scope).await?;
            for adapter in &reachable {
                adapter
                    .cleanup(site_id, scope)
                    .await
                    .with_context(|| format!("clean {} site runtime resources", adapter.name()))?;
            }
            Ok(SiteRuntimeCleanupReport { skipped })
        })
    }
}

struct SelectedAdapter {
    adapter: Box<dyn SiteRuntimeResetAdapter>,
    required: bool,
}

impl SelectedAdapter {
    fn required(adapter: Box<dyn SiteRuntimeResetAdapter>) -> Self {
        Self {
            adapter,
            required: true,
        }
    }

    fn optional(adapter: Box<dyn SiteRuntimeResetAdapter>) -> Self {
        Self {
            adapter,
            required: false,
        }
    }
}

struct SimulatedSiteRuntimeReset;

impl SiteRuntimeResetAdapter for SimulatedSiteRuntimeReset {
    fn name(&self) -> &'static str {
        "Simulated runtime"
    }

    fn connect(&self) -> SiteRuntimeResetFuture<'_> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn prevalidate<'a>(
        &'a self,
        _site_id: &'a str,
        _scope: &'a SiteRuntimeScope,
    ) -> SiteRuntimeResetFuture<'a, Vec<String>> {
        Box::pin(std::future::ready(Ok(Vec::new())))
    }

    fn cleanup<'a>(
        &'a self,
        _site_id: &'a str,
        _scope: &'a SiteRuntimeScope,
    ) -> SiteRuntimeResetFuture<'a> {
        Box::pin(std::future::ready(Ok(())))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn configured_adapters_are_required_native_needs_none_and_unbound_docker_is_optional() {
        for (configured, expected) in [
            (
                vec![RuntimeBindingKind::Simulated],
                vec![("Simulated runtime", true), ("Docker runtime", false)],
            ),
            (
                vec![RuntimeBindingKind::Docker, RuntimeBindingKind::Simulated],
                vec![("Simulated runtime", true), ("Docker runtime", true)],
            ),
            (
                vec![RuntimeBindingKind::Native],
                vec![("Docker runtime", false)],
            ),
            (Vec::new(), vec![("Docker runtime", false)]),
        ] {
            let reset = SiteRuntimeReset::new(&configured.iter().copied().collect());

            assert_eq!(
                reset
                    .adapters
                    .iter()
                    .map(|selected| (selected.adapter.name(), selected.required))
                    .collect::<Vec<_>>(),
                expected,
                "configured {configured:?}"
            );
        }
    }

    #[tokio::test]
    async fn every_adapter_connects_and_prevalidates_before_any_adapter_cleanup() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let reset = SiteRuntimeReset::from_adapters(vec![
            SelectedAdapter::required(Box::new(RecordingAdapter::new("first", Arc::clone(&calls)))),
            SelectedAdapter::optional(Box::new(RecordingAdapter::new(
                "second",
                Arc::clone(&calls),
            ))),
        ]);

        let report = reset
            .cleanup("site-one", &SiteRuntimeScope::Site)
            .await
            .expect("composed cleanup");

        assert!(report.skipped.is_empty());
        assert_eq!(
            *calls.lock().expect("calls"),
            [
                "connect:first",
                "connect:second",
                "prevalidate:first:site-one",
                "prevalidate:second:site-one",
                "cleanup:first:site-one",
                "cleanup:second:site-one",
            ]
        );
    }

    #[tokio::test]
    async fn prevalidation_failure_prevents_every_adapter_cleanup() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let reset = SiteRuntimeReset::from_adapters(vec![
            SelectedAdapter::required(Box::new(RecordingAdapter::new("first", Arc::clone(&calls)))),
            SelectedAdapter::required(Box::new(
                RecordingAdapter::new("second", Arc::clone(&calls)).failing_prevalidation(),
            )),
        ]);

        let error = reset
            .cleanup("site-one", &SiteRuntimeScope::Site)
            .await
            .expect_err("unsafe adapter");

        assert_eq!(
            *calls.lock().expect("calls"),
            [
                "connect:first",
                "connect:second",
                "prevalidate:first:site-one",
                "prevalidate:second:site-one",
            ]
        );
        assert_eq!(
            format!("{error:#}"),
            "pre-validate second site cleanup: injected invalid evidence"
        );
    }

    #[tokio::test]
    async fn unreachable_required_adapter_fails_before_any_discovery() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let reset = SiteRuntimeReset::from_adapters(vec![
            SelectedAdapter::required(Box::new(RecordingAdapter::new("first", Arc::clone(&calls)))),
            SelectedAdapter::required(Box::new(
                RecordingAdapter::new("second", Arc::clone(&calls)).unreachable(),
            )),
        ]);

        let error = reset
            .cleanup("site-one", &SiteRuntimeScope::Site)
            .await
            .expect_err("unreachable required adapter");

        assert_eq!(
            *calls.lock().expect("calls"),
            ["connect:first", "connect:second"]
        );
        assert_eq!(
            format!("{error:#}"),
            "reach second for site cleanup: injected unreachable control plane"
        );
    }

    #[tokio::test]
    async fn unreachable_optional_adapter_is_skipped_and_reported() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let reset = SiteRuntimeReset::from_adapters(vec![
            SelectedAdapter::required(Box::new(RecordingAdapter::new("first", Arc::clone(&calls)))),
            SelectedAdapter::optional(Box::new(
                RecordingAdapter::new("second", Arc::clone(&calls)).unreachable(),
            )),
        ]);

        let report = reset
            .cleanup("site-one", &SiteRuntimeScope::Site)
            .await
            .expect("optional adapter skipped");

        assert_eq!(
            *calls.lock().expect("calls"),
            [
                "connect:first",
                "connect:second",
                "prevalidate:first:site-one",
                "cleanup:first:site-one",
            ]
        );
        assert_eq!(
            report
                .skipped
                .iter()
                .map(|skipped| (skipped.runtime, format!("{:#}", skipped.reason)))
                .collect::<Vec<_>>(),
            [("second", "injected unreachable control plane".to_owned())]
        );
    }

    #[tokio::test]
    async fn inventory_describes_resources_and_never_cleans() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let reset = SiteRuntimeReset::from_adapters(vec![
            SelectedAdapter::required(Box::new(RecordingAdapter::new("first", Arc::clone(&calls)))),
            SelectedAdapter::optional(Box::new(
                RecordingAdapter::new("second", Arc::clone(&calls)).unreachable(),
            )),
            SelectedAdapter::required(Box::new(RecordingAdapter::new("third", Arc::clone(&calls)))),
        ]);

        let inventory = reset
            .inventory("site-one", &SiteRuntimeScope::Site)
            .await
            .expect("inventory");

        assert_eq!(
            inventory
                .resources
                .iter()
                .map(|resource| (resource.runtime, resource.description.as_str()))
                .collect::<Vec<_>>(),
            [("first", "first-resource"), ("third", "third-resource")]
        );
        assert_eq!(
            inventory
                .skipped
                .iter()
                .map(|skipped| skipped.runtime)
                .collect::<Vec<_>>(),
            ["second"]
        );
        assert_eq!(
            *calls.lock().expect("calls"),
            [
                "connect:first",
                "connect:second",
                "connect:third",
                "prevalidate:first:site-one",
                "prevalidate:third:site-one",
            ]
        );
    }

    #[tokio::test]
    async fn inventory_fails_closed_on_invalid_evidence_or_unreachable_required_adapter() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let invalid = SiteRuntimeReset::from_adapters(vec![SelectedAdapter::required(Box::new(
            RecordingAdapter::new("first", Arc::clone(&calls)).failing_prevalidation(),
        ))]);
        let unreachable = SiteRuntimeReset::from_adapters(vec![SelectedAdapter::required(
            Box::new(RecordingAdapter::new("second", Arc::clone(&calls)).unreachable()),
        )]);

        assert_eq!(
            format!(
                "{:#}",
                invalid
                    .inventory("site-one", &SiteRuntimeScope::Site)
                    .await
                    .expect_err("invalid")
            ),
            "pre-validate first site cleanup: injected invalid evidence"
        );
        assert_eq!(
            format!(
                "{:#}",
                unreachable
                    .inventory("site-one", &SiteRuntimeScope::Site)
                    .await
                    .expect_err("unreachable")
            ),
            "reach second for site cleanup: injected unreachable control plane"
        );
    }

    #[tokio::test]
    async fn simulated_cleanup_is_an_idempotent_no_op() {
        let reset = SiteRuntimeReset::from_adapters(vec![SelectedAdapter::required(Box::new(
            SimulatedSiteRuntimeReset,
        ))]);

        reset
            .cleanup("site-one", &SiteRuntimeScope::Site)
            .await
            .expect("first cleanup");
        reset
            .cleanup("site-one", &SiteRuntimeScope::Site)
            .await
            .expect("repeat cleanup");
    }

    struct RecordingAdapter {
        name: &'static str,
        calls: Arc<Mutex<Vec<String>>>,
        unreachable: bool,
        fail_prevalidation: bool,
    }

    impl RecordingAdapter {
        fn new(name: &'static str, calls: Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                name,
                calls,
                unreachable: false,
                fail_prevalidation: false,
            }
        }

        fn unreachable(mut self) -> Self {
            self.unreachable = true;
            self
        }

        fn failing_prevalidation(mut self) -> Self {
            self.fail_prevalidation = true;
            self
        }
    }

    impl SiteRuntimeResetAdapter for RecordingAdapter {
        fn name(&self) -> &'static str {
            self.name
        }

        fn connect(&self) -> SiteRuntimeResetFuture<'_> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .expect("calls")
                    .push(format!("connect:{}", self.name));
                if self.unreachable {
                    anyhow::bail!("injected unreachable control plane");
                }
                Ok(())
            })
        }

        fn prevalidate<'a>(
            &'a self,
            site_id: &'a str,
            _scope: &'a SiteRuntimeScope,
        ) -> SiteRuntimeResetFuture<'a, Vec<String>> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .expect("calls")
                    .push(format!("prevalidate:{}:{site_id}", self.name));
                if self.fail_prevalidation {
                    anyhow::bail!("injected invalid evidence");
                }
                Ok(vec![format!("{}-resource", self.name)])
            })
        }

        fn cleanup<'a>(
            &'a self,
            site_id: &'a str,
            _scope: &'a SiteRuntimeScope,
        ) -> SiteRuntimeResetFuture<'a> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .expect("calls")
                    .push(format!("cleanup:{}:{site_id}", self.name));
                Ok(())
            })
        }
    }
}
