//! Changes to which nodes belong to a site: joining a new node, removing one from the manager's
//! stopped site, letting a node leave, and listing the declared nodes.
//!
//! Join and removal edit the site only through an offline site opened with the exclusive site
//! lock, so membership never changes while the manager runs. A node that leaves cleans up its own
//! node folder and workloads without contacting the manager.

pub mod join;
mod leave;
mod list;
mod offline;
mod removal;

pub use leave::{NodeLeave, NodeLeaveError};
pub use list::{ListedNode, ListedTarget, NodeListError, SiteNodeListing, list_site_nodes};
pub use offline::{OfflineSite, OfflineSiteError};
pub use removal::{
    DeletedRoute, DeploymentKind, NodeRemoval, NodeRemovalError, NodeRemovalPlan, PlacedDeployment,
};
