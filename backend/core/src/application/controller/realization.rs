//! Node-local application realization input and result boundaries.
//!
//! A realizer reads its assigned declarations only through [`ApplicationRealizationInput`] and
//! reports completed cleanup only through [`ApplicationRealizationResults`], so it never receives
//! declaration mutation or deletion authority. It also keeps durable node records, its ownership
//! evidence and launch identity pins, through [`NodeRealizationRecords`].

use std::{collections::BTreeMap, sync::Arc};

use dotcore_proto::dotcore::{
    application::v1::ApplicationDeploymentDeclaration, site::store::v1::NodeApplicationRealization,
};
use dotcore_site::{
    repository::ApplicationSiteSnapshot,
    view::fencing::{ActionAssignment, ManagerSession},
};
use tokio::sync::Notify;

use super::ApplicationLifecycleError;

/// Read-only application owner input consumed by a node-local realizer.
///
/// Implementations expose the site topology and the declarations assigned to the realizer; they
/// cannot accept management mutations or project public status.
pub(crate) trait ApplicationRealizationInput: Send + Sync {
    /// Returns the assigned declarations with the site nodes and execution targets they name.
    fn realization_snapshot(
        &self,
    ) -> Result<ApplicationRealizationSnapshot, ApplicationLifecycleError>;

    /// Returns the listener woken when assigned intent changes.
    fn register_change_listener(&self) -> Arc<Notify>;
}

/// Input of one reconciliation pass: the assigned declarations with the topology they name, and
/// the accepted assignment each deployment's runtime actions act for.
pub(crate) struct ApplicationRealizationSnapshot {
    pub(crate) site: ApplicationSiteSnapshot,

    /// Action identity of each assigned deployment, keyed by deployment identity.
    pub(crate) assignments: BTreeMap<String, ActionAssignment>,

    /// Installed node view the assignments came from; absent while no view is installed.
    pub(crate) view: Option<RealizedView>,
}

/// Identity of the installed node view one reconciliation pass realized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RealizedView {
    pub(crate) manager_session: ManagerSession,
    pub(crate) revision: u64,
}

/// Durable node-local records of the deployments a node realizes.
///
/// A realization record is the node's ownership evidence for a deployment's workload group and
/// storage and retains its once-resolved launch identities; cleanup evidence proves a withdrawn
/// generation left nothing behind.
pub(crate) trait NodeRealizationRecords: Send + Sync {
    /// Returns the realization record of one deployment, if this node began realizing it.
    fn realization(
        &self,
        deployment_id: &str,
    ) -> Result<Option<NodeApplicationRealization>, ApplicationLifecycleError>;

    /// Creates or replaces one realization record before the runtime action that relies on it.
    fn record_realization(
        &self,
        realization: NodeApplicationRealization,
    ) -> Result<(), ApplicationLifecycleError>;

    /// Reports whether cleanup evidence for this teardown generation is already retained.
    fn cleanup_confirmed(
        &self,
        deployment_id: &str,
        lifecycle_generation: u64,
    ) -> Result<bool, ApplicationLifecycleError>;
}

/// Completion boundary through which a node-local realizer returns cleanup outcomes.
pub(crate) trait ApplicationRealizationResults: Send + Sync {
    /// Confirms that the exact teardown carried by `teardown` left no runtime workload group.
    /// The port removes the deployment's node records and retains cleanup evidence that the
    /// manager finalizes the declaration from.
    fn confirm_cleanup(
        &self,
        teardown: &ApplicationDeploymentDeclaration,
    ) -> Result<(), ApplicationLifecycleError>;
}
