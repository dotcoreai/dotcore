//! Node host selection for a new site.
//!
//! Chooses the host other machines reach this node at by a fixed precedence: a named host, a host
//! the server listener requires, a host an existing site already records, then the most useful host
//! found on this machine, else loopback. Finding hosts only observes this machine, is bounded in
//! time and never fails; the chosen host is also checked against this machine.

mod candidates;
mod check;
mod environment;
mod interfaces;
mod listener;
mod selection;
mod tailnet;
#[cfg(test)]
pub(crate) mod test_support;

pub use candidates::HostCandidate;
pub use check::{host_warning, inference_address_remedy, private_address_remedy};
pub use environment::{HostEnvironment, SystemHostEnvironment};
pub use interfaces::InterfaceAddress;
pub use listener::ListenerHost;
pub use selection::{HostSource, SelectedHost, select_host};
pub use tailnet::TailnetStatus;
