//! Node join: the one-time token an operator hands to a new machine, the joining node's side
//! that authenticates the manager and installs its node folder, and the manager's join host that
//! approves, signs, and records the node. The offline node addition on the manager and the join
//! on the new machine drive it.
//!
//! The joining node keeps its private key and sends only a signing request; the manager side
//! signs it with the site CA and records the node only when the operator has approved it and the
//! joining node has committed the offer. A declared node whose folder is intact joins again the
//! same way to change its addresses, proving it holds its pinned identity instead of getting a
//! new one.

mod admission;
mod client;
mod connection;
mod host;
mod installation;
mod node_key;
mod proposal;
mod publication;
#[cfg(test)]
pub(crate) mod test_fixtures;
#[cfg(test)]
mod tests;
mod token;
mod verifier;

pub use client::{JoinConnection, JoinedNodeFolder, NodeJoinError, default_join_host};
pub use host::{
    JoinApproval, JoinDecision, JoinHost, JoinHostError, JoinHostIdentity, JoinHostLimits,
    JoinHostSettings, JoinHostSource, JoinReview, JoinSiteError, JoinSiteWriter, JoinedNode,
    LocalPeers, NodeRejoin, ReviewedJoin, blocking_review,
};
pub use installation::{NodeFolderError, RejoiningNodeFolder};
pub use node_key::{NodeKey, NodeKeyError, csr_key_fingerprint};
pub use proposal::{JoinProposal, JoinProposalError, RejoinProposal};
pub(super) use publication::{publish_node, read_node_pin, replace_node_hosts};
pub use token::{JOIN_SECRET_BYTES, JoinSecret, JoinToken, JoinTokenError};
