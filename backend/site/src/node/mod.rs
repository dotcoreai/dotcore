//! SiteNodes from both sides of private transport: on each node, its binding, folder, trust
//! identity and the agent that observes the host and reports to the site manager; on the manager,
//! the registry, control coordinator and capability inventory built from those reports.
//!
//! Host hardware is collected through the node hardware crate and runtime prerequisites through
//! injected capability probes, so this boundary calls no runtime backend. The manager fences every
//! report and published view by node process incarnation and freshness.

pub mod agent;
pub mod binding;
pub mod capabilities;
pub mod control;
pub mod folder;
pub mod hardware_inventory;
pub mod hostname;
pub mod identity;
pub mod metrics;
pub mod observation_configuration;
pub mod registry;
pub mod session;
pub mod stream;
pub mod target_inventory;
