//! Node join: the one-time token an operator hands to a new machine, the joining node's side
//! that authenticates the manager and installs its node folder, and the site manager's join
//! registry that hosts each join on the private listener, waits for the operator's decision,
//! signs, and records the node, with the management API that drives it.
//!
//! The joining node keeps its private key and sends only a signing request, and listens on its
//! private port while it joins. The manager side dials it back there and proves it holds that
//! key, then signs it with the site CA and records the node only when the operator has approved
//! it and the joining node has committed the offer. A declared node whose folder is intact joins
//! again the same way to change its addresses, proving it holds its pinned identity instead of
//! getting a new one.

mod admission;
mod client;
mod dial_back;
mod dial_back_listener;
mod fingerprint_pattern;
mod host;
mod installation;
mod management;
mod node_key;
mod proposal;
mod registry;
#[cfg(test)]
pub(crate) mod test_fixtures;
#[cfg(test)]
mod tests;
mod token;
mod verifier;

pub use client::{JoinConnection, JoinedNodeFolder, NodeJoinError};
pub use dial_back::{DialBack, DialBackError};
pub use fingerprint_pattern::{FINGERPRINT_PATTERN_SIZE, FingerprintColor, FingerprintPattern};
pub use host::{
    JoinHostIdentity, JoinHostSource, JoinReview, JoinService, JoinSiteError, JoinSiteWriter,
    JoinedNode, NodeRejoin, ReviewedJoin,
};
pub use installation::{NodeFolderError, RejoiningNodeFolder};
pub use management::JoinManagementService;
pub use node_key::{NodeKey, NodeKeyError, csr_key_fingerprint};
pub use proposal::{JoinProposal, JoinProposalError, RejoinProposal};
pub use registry::{
    JoinAttempt, JoinAttemptOutcome, JoinLimits, JoinRegistry, JoinRegistryError, JoinStatus,
    JoinWatch, StartedJoin,
};
pub use token::{JOIN_SECRET_BYTES, JoinSecret, JoinToken, JoinTokenError};
