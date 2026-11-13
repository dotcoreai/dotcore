//! The site manager's state authority (crate `dotcore-site`): desired site configuration with its
//! site store and node store, site membership, initial site creation and reset, node-scoped desired
//! views with their fencing, and the node agent and private transport that connect SiteNodes.
//!
//! The site owns declarations and their validation, never owner product state: the model and
//! application controllers above it act on the site through injected rules and owner seams, and
//! runtime work reaches it only through the owner-neutral runtime contracts. Every durable write is
//! atomic, owner-only and flushed before it becomes visible.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Dotcore supports only Linux and macOS targets");

pub mod atomic;
pub mod console_settings;
pub mod initial;
pub mod manager_trust;
pub mod membership;
pub mod node;
pub mod persistence;
pub mod private_transport;
pub mod repository;
pub mod reset;
pub mod service;
pub mod store;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod validation;
pub mod view;
pub mod worker_channels;

/// File name of the desired site declaration in a site directory.
pub const SITE_FILE: &str = "site.yaml";
/// File name for the server-local binding to an already declared SiteNode.
pub const NODE_FILE: &str = "node.yaml";
