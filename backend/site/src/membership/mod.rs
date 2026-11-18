//! Changes to which nodes belong to a site: joining a new node or letting a declared one join
//! again while the site manager runs, removing one from the running site or from a stopped one,
//! letting a node leave, and listing the declared nodes, with the management API that drives
//! joins and removals.
//!
//! The running manager records a join or a removal so that it is accepted only once it is
//! written, and withdraws a removed node's trust only then. A stopped site's removal edits it
//! through an offline site opened with the exclusive site lock. A node that leaves cleans up its
//! own node folder and workloads without contacting the manager.

pub mod join;
mod leave;
mod list;
mod live_removal;
mod management;
mod offline;
mod recorder;
mod removal;

pub use leave::{NodeLeave, NodeLeaveError};
pub use list::{ListedNode, ListedTarget, NodeListError, SiteNodeListing, list_site_nodes};
pub use live_removal::{
    MachineRemoval, MachineRemovalError, MachineRemovalOutcome, NodeConnectivity,
};
pub use management::MembershipManagementService;
pub use offline::{OfflineSite, OfflineSiteError};
pub use recorder::MembershipRecorder;
pub use removal::{
    DeletedRoute, DeploymentKind, NodeRemoval, NodeRemovalError, NodeRemovalPlan, PlacedDeployment,
    looks_like_node_id,
};

#[cfg(test)]
pub(crate) mod test_support {
    /// Connectivity of a site whose nodes are never connected.
    pub(crate) struct Disconnected;

    impl super::NodeConnectivity for Disconnected {
        fn is_connected(&self, _node_id: &str) -> bool {
            false
        }
    }
}
