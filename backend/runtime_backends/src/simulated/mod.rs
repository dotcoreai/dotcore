//! Deterministic in-memory runtime backend for execution-target lifecycle behavior, and the
//! declared machine its model workloads take memory on.

mod adapter;
mod configuration;
mod logs;
mod machine;
#[cfg(test)]
mod tests;

pub use adapter::BACKEND_ID;
pub use adapter::InMemoryRuntimeBackend;
pub use configuration::{
    SimulatedRuntimeCallCounts, SimulatedRuntimeGroupCall, SimulatedRuntimeGroupDrift,
    SimulatedRuntimeTestSupport,
};
pub use configuration::{SimulatedRuntimeConfig, SimulatedStartupConfig};
pub use machine::DeclaredMachine;
