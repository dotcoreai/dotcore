//! Node-scoped desired views: how the site manager projects its accepted site into one complete
//! view per node, how a node validates and installs that view, and the fencing that keeps every
//! node action tied to the view and manager session that authorized it.
//!
//! Owners supply the content of their sections through injected seams; this boundary owns the view
//! envelope, its wire contract, cleanup evidence classification and lifecycle fencing.

pub mod cleanup_evidence;
pub mod contract;
pub mod desired;
pub mod fencing;
