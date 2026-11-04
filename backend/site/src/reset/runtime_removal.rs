//! Stops and removes the runtime workloads of one stopped site directory or joined node folder
//! before that directory is deleted, for site reset, site deletion, and a node leaving its site.
//!
//! Native workloads are found by their location in the directory and their ownership evidence is
//! checked before any runtime changes, so untrusted evidence refuses the removal with nothing
//! stopped. Other runtimes are cleaned by site identity, either everything labelled with the site
//! or only a node's own execution targets, and are skipped when the site identity is unknown.

use std::{collections::BTreeSet, path::Path};

use dotcore_domain::SiteNodeId;
use dotcore_runtime::{
    LocatedWorkloadCleanup, SiteRuntimeCleanup, SiteRuntimeCleanupReport, SiteRuntimeCleanups,
    SiteRuntimeInventory, SiteRuntimeResource, SiteRuntimeScope,
};

/// Runtime cleanup of one directory about to be deleted: its native workloads by location, then
/// the other runtimes' resources of its site within one scope.
#[derive(Debug)]
pub struct RuntimeRemoval {
    native: Box<dyn LocatedWorkloadCleanup>,
    site: Option<(String, SiteRuntimeScope)>,
}

impl RuntimeRemoval {
    /// Selects the cleanup of a whole site directory. With a known site identity, other runtimes
    /// clean every resource labelled with the site; without one, only native workloads are
    /// cleaned. Native workloads must record the site and SiteNode when they are known.
    pub fn site_directory(
        cleanups: &dyn SiteRuntimeCleanups,
        site_root: &Path,
        site_id: Option<&str>,
        site_node_id: Option<SiteNodeId>,
    ) -> Self {
        Self {
            native: cleanups.located_workloads(site_root, site_id.map(str::to_owned), site_node_id),
            site: site_id.map(|site_id| (site_id.to_owned(), SiteRuntimeScope::Site)),
        }
    }

    /// Selects the cleanup of a joined node folder. Other runtimes clean only the resources of
    /// the node's execution targets, and every native workload in the folder must record exactly
    /// this site and SiteNode.
    pub fn node_folder(
        cleanups: &dyn SiteRuntimeCleanups,
        node_root: &Path,
        site_id: &str,
        site_node_id: SiteNodeId,
        execution_target_ids: BTreeSet<String>,
    ) -> Self {
        Self {
            native: cleanups.located_workloads(
                node_root,
                Some(site_id.to_owned()),
                Some(site_node_id),
            ),
            site: Some((
                site_id.to_owned(),
                SiteRuntimeScope::ExecutionTargets(execution_target_ids),
            )),
        }
    }

    /// Lists the resources `runtime` finds, then the native workloads, without changing
    /// anything. Native evidence is checked first, so untrusted evidence contacts no runtime.
    pub async fn inventory(
        &self,
        runtime: &dyn SiteRuntimeCleanup,
    ) -> Result<SiteRuntimeInventory, RuntimeRemovalError> {
        let native = self
            .native
            .inventory()
            .await
            .map_err(RuntimeRemovalError::Native)?;
        let mut inventory = match &self.site {
            Some((site_id, scope)) => runtime
                .inventory(site_id, scope)
                .await
                .map_err(RuntimeRemovalError::Runtime)?,
            None => SiteRuntimeInventory::default(),
        };
        inventory
            .resources
            .extend(native.into_iter().map(|description| SiteRuntimeResource {
                runtime: self.native.runtime_name(),
                description,
            }));
        Ok(inventory)
    }

    /// Validates the native evidence, cleans the other runtimes, then stops and removes the
    /// native workloads. Untrusted evidence refuses before any runtime changes, but a native stop
    /// or removal failing afterwards leaves the other runtimes already cleaned. Each step is
    /// idempotent, so a failed attempt can simply be repeated.
    pub async fn cleanup(
        &self,
        runtime: &dyn SiteRuntimeCleanup,
    ) -> Result<SiteRuntimeCleanupReport, RuntimeRemovalError> {
        self.native
            .inventory()
            .await
            .map_err(RuntimeRemovalError::Native)?;
        let report = match &self.site {
            Some((site_id, scope)) => runtime
                .cleanup(site_id, scope)
                .await
                .map_err(RuntimeRemovalError::Runtime)?,
            None => SiteRuntimeCleanupReport::default(),
        };
        self.native
            .cleanup()
            .await
            .map_err(RuntimeRemovalError::Native)?;
        Ok(report)
    }
}

/// Runtime cleanup of a directory about to be deleted failed; the directory must be kept.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeRemovalError {
    /// Native workloads in the directory could not be classified, stopped, or removed.
    #[error("native workload cleanup failed")]
    Native(#[source] anyhow::Error),

    /// Another runtime's resources could not be listed or removed.
    #[error("Runtime cleanup failed")]
    Runtime(#[source] anyhow::Error),
}
