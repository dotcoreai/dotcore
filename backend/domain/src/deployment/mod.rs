//! Shared deployment value validation, metadata admission policy, secret values, node-side
//! launch identity pinning, and the crash rule with its failure diagnostics.
//!
//! This private capability contains owner-neutral rules only; model and application modules
//! retain deployment records, lifecycle, persistence, projection, and execution authority.

pub mod artifact;
pub mod crash;
pub mod launch_pin;
pub mod metadata;
pub mod name;
pub mod secret;

/// Diagnostic lines one deployment failure may carry.
pub const MAX_FAILURE_DIAGNOSTICS: usize = 8;

/// Total bytes of one deployment failure's diagnostics, room for a workload's output tail.
pub const MAX_FAILURE_DIAGNOSTIC_BYTES: usize = 6 * 1024;

/// Controller that owns a deployment's declarations, lifecycle generations, and teardown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeploymentOwner {
    Model,
    Application,
}
