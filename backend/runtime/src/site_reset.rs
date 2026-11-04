//! Runtime inventory and cleanup contracts for an exclusively locked stopped site, used by site
//! reset, site deletion, and a removed node leaving its site.
//!
//! A cleanup lists or removes the exact runtime resources a site owns within a scope, and reports
//! optional runtimes it skipped because their control plane was unreachable. The backends supply
//! the set of runtime-specific cleanups, selected by the kinds of runtime binding a site
//! configuration uses, so the site code that orchestrates removal names no backend.

use std::{collections::BTreeSet, fmt, future::Future, path::Path, pin::Pin};

use dotcore_domain::SiteNodeId;

/// Runtime-specific cleanups of a stopped site, selected by the runtime binding kinds it uses.
pub trait SiteRuntimeCleanups: Send + Sync + fmt::Debug {
    /// Selects the stopped-site cleanup of the runtimes bound with these kinds, without contacting
    /// them. A kind whose resources need no site-wide cleanup selects nothing.
    fn site_cleanup(
        &self,
        bound: &BTreeSet<RuntimeBindingKind>,
    ) -> Result<Box<dyn SiteRuntimeCleanup>, SiteRuntimeResetConfigurationError>;

    /// Selects the cleanup of the workloads held by their location in one site directory or node
    /// folder about to be deleted. Workloads must record the site and SiteNode when they are known.
    fn located_workloads(
        &self,
        root: &Path,
        site_id: Option<String>,
        site_node_id: Option<SiteNodeId>,
    ) -> Box<dyn LocatedWorkloadCleanup>;

    /// Removes site data a runtime left owned by root beneath the site root, after the host could
    /// not remove it.
    fn remove_root_owned_site_data<'a>(
        &'a self,
        site_root: &'a Path,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    /// Removes the node-local workload caches of one site, which live outside the site root.
    /// Other sites' caches are left alone, and absent caches are already removed.
    fn remove_workload_caches<'a>(
        &'a self,
        site_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;
}

/// Kind of runtime binding an execution target declares, the key site cleanups are selected by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RuntimeBindingKind {
    Docker,

    Native,

    Simulated,
}

impl RuntimeBindingKind {
    /// Every binding kind, the cleanup selection for a site whose declarations cannot be read.
    pub const ALL: [Self; 3] = [Self::Docker, Self::Native, Self::Simulated];
}

/// Workloads found by their location in one directory about to be deleted, with ownership
/// evidence checked before anything stops, so untrusted evidence refuses the removal.
pub trait LocatedWorkloadCleanup: Send + Sync + fmt::Debug {
    /// Human-readable runtime name, such as `native runtime`.
    fn runtime_name(&self) -> &'static str;

    /// Describes the owned workloads without changing anything.
    fn inventory(&self) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<String>>> + Send + '_>>;

    /// Stops and removes every owned workload.
    fn cleanup(&self) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>>;
}

/// Removes exact runtime residue for one validated site identity.
/// Implementations fail closed unless adapter-owned resources can be conclusively classified.
pub trait SiteRuntimeCleanup: Send + Sync {
    /// Lists the exact runtime resources owned by the site within `scope` without changing anything.
    /// The inventory names optional runtimes that were unreachable and therefore skipped.
    fn inventory<'a>(
        &'a self,
        site_id: &'a str,
        scope: &'a SiteRuntimeScope,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<SiteRuntimeInventory>> + Send + 'a>>;

    /// Ensures that exact runtime resources owned by the site within `scope` are absent.
    /// The report names optional runtimes that were unreachable and therefore skipped.
    fn cleanup<'a>(
        &'a self,
        site_id: &'a str,
        scope: &'a SiteRuntimeScope,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<SiteRuntimeCleanupReport>> + Send + 'a>>;
}

/// Which of a site's runtime resources one cleanup acts on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SiteRuntimeScope {
    /// Every resource labelled with the site, as site deletion and site reset remove.
    Site,

    /// Only resources of the site labelled with one of these execution target ids, as a removed
    /// node leaving its site removes. Resources of the site labelled with another well-formed
    /// target are left alone, but a site resource with a missing or malformed target label fails
    /// the cleanup because it cannot be attributed.
    ExecutionTargets(BTreeSet<String>),
}

/// Runtime resources a site owns, as found by a read-only pass.
#[derive(Debug, Default)]
pub struct SiteRuntimeInventory {
    /// Resources a cleanup would remove, grouped by runtime in discovery order.
    pub resources: Vec<SiteRuntimeResource>,

    /// Optional runtimes whose control plane was unreachable, so they were not inventoried.
    pub skipped: Vec<SkippedSiteRuntimeCleanup>,
}

/// One runtime resource owned by a site, described for the operator.
#[derive(Debug)]
pub struct SiteRuntimeResource {
    /// Human-readable runtime name, such as `Docker runtime`.
    pub runtime: &'static str,

    /// Operator-facing description, such as a model container with its owner identity.
    pub description: String,
}

/// Outcome of a successful stopped-site runtime cleanup.
#[derive(Debug, Default)]
pub struct SiteRuntimeCleanupReport {
    /// Optional runtimes whose control plane was unreachable, so their cleanup was skipped.
    pub skipped: Vec<SkippedSiteRuntimeCleanup>,
}

/// One optional runtime cleanup skipped because its control plane was unreachable.
#[derive(Debug)]
pub struct SkippedSiteRuntimeCleanup {
    /// Human-readable runtime name, such as `Docker runtime`.
    pub runtime: &'static str,

    /// Connection failure that made the runtime unreachable.
    pub reason: anyhow::Error,
}

/// Invalid runtime adapter configuration for stopped-site cleanup.
/// Configuration fails before an adapter connects or performs resource discovery.
#[derive(Debug, thiserror::Error)]
pub enum SiteRuntimeResetConfigurationError {
    /// An existing execution target binds a runtime adapter this version does not know, so the
    /// resources it may own cannot be classified.
    #[error(
        "an execution target binds unknown runtime adapter {adapter:?}, whose resources site cleanup cannot classify"
    )]
    UnknownBinding {
        /// Adapter key as written in the site configuration, such as `futureRuntime`.
        adapter: String,
    },
}
