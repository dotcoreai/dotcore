//! The source of what every lane's deployments hold of each site node's memory, as the capacity
//! component counts it.
//!
//! Each workload controller answers its own deployments as capacity workloads; a source joins
//! them so a node's memory and a placement preview name every deployment, whatever its lane. Its
//! answer is advisory and read without serializing with lifecycle operations, so a deployment
//! created or removed a moment ago may be missing or counted as other use until the next read.

use dotcore_site::node::target_inventory::TargetCapabilityInventory;

use super::capacity::NodeWorkloads;

/// Answers the workloads of every site node across lanes.
pub(crate) trait SiteWorkloadSource: Send + Sync {
    /// Returns what the deployments of every lane hold of each node's memory: the inference lane's
    /// first, then the applications lane's, each in its list order. `inventory` resolves each
    /// deployment's target devices; without it, devices don't resolve.
    fn node_workloads(
        &self,
        inventory: Option<&TargetCapabilityInventory>,
    ) -> Result<NodeWorkloads, SiteWorkloadsError>;
}

/// Joins each lane's workloads node by node, keeping the lanes in the order given and each lane's
/// workloads in its own order.
pub(crate) fn join_lanes(lanes: impl IntoIterator<Item = NodeWorkloads>) -> NodeWorkloads {
    let mut joined = NodeWorkloads::new();
    for lane in lanes {
        for (node, workloads) in lane {
            joined.entry(node).or_default().extend(workloads);
        }
    }
    joined
}

/// A lane's deployments could not be read.
#[derive(Debug, thiserror::Error)]
#[error("site workloads could not be read")]
pub(crate) struct SiteWorkloadsError {
    #[source]
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl SiteWorkloadsError {
    pub(crate) fn new(source: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self {
            source: source.into(),
        }
    }
}
