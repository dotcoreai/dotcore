//! Changes to which nodes belong to a site: joining a new node or letting a declared one join
//! again while the site manager runs, removing one from the manager's stopped site, letting a
//! node leave, and listing the declared nodes.
//!
//! The running manager records a join through its membership recorder, which accepts a change
//! only once it is written. Removal edits the stopped site through an offline site opened with
//! the exclusive site lock. A node that leaves cleans up its own node folder and workloads
//! without contacting the manager.

pub mod join;
mod leave;
mod list;
mod offline;
mod recorder;
mod removal;

pub use leave::{NodeLeave, NodeLeaveError};
pub use list::{ListedNode, ListedTarget, NodeListError, SiteNodeListing, list_site_nodes};
pub use offline::{OfflineSite, OfflineSiteError};
pub use recorder::MembershipRecorder;
pub use removal::{
    DeletedRoute, DeploymentKind, NodeRemoval, NodeRemovalError, NodeRemovalPlan, PlacedDeployment,
};
