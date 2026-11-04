//! Shared deployment value validation, metadata admission policy, and node-side launch identity
//! pinning.
//!
//! This private capability contains owner-neutral rules only; model and application modules
//! retain deployment records, lifecycle, persistence, projection, and execution authority.

pub mod artifact;
pub mod launch_pin;
pub mod metadata;
pub mod name;

/// Controller that owns a deployment's declarations, lifecycle generations, and teardown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeploymentOwner {
    Model,
    Application,
}
