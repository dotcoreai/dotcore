//! The machine facts host selection reads.
//!
//! Exposes interface addresses, tailnet status, host-name resolution and process variables through
//! one interface, so tests describe a whole machine with one fake and never touch the real network,
//! tailnet client or DNS.

use std::io;

use dotcore_domain::{HostResolver, SystemHostResolver};
use futures_util::future::BoxFuture;

use super::{InterfaceAddress, TailnetStatus, interfaces, tailnet};
use crate::config::environment_value;

/// This machine as host selection sees it.
pub trait HostEnvironment: Sync {
    /// Returns every address observed on this machine's network interfaces.
    fn interface_addresses(&self) -> io::Result<Vec<InterfaceAddress>>;

    /// Returns this machine's tailnet status, or `None` without a running tailnet.
    fn tailnet_status(&self) -> BoxFuture<'_, Option<TailnetStatus>>;

    /// Returns the resolver host names are looked up with.
    fn resolver(&self) -> &dyn HostResolver;

    /// Returns the process variable `name`, or `None` when it is unset.
    fn variable(&self, name: &'static str) -> anyhow::Result<Option<String>>;
}

/// The real machine: its network interfaces, tailnet client, system resolver and process
/// environment.
pub struct SystemHostEnvironment;

impl HostEnvironment for SystemHostEnvironment {
    fn interface_addresses(&self) -> io::Result<Vec<InterfaceAddress>> {
        interfaces::system_interface_addresses()
    }

    fn tailnet_status(&self) -> BoxFuture<'_, Option<TailnetStatus>> {
        Box::pin(tailnet::system_tailnet_status())
    }

    fn resolver(&self) -> &dyn HostResolver {
        &SystemHostResolver
    }

    fn variable(&self, name: &'static str) -> anyhow::Result<Option<String>> {
        environment_value(name)
    }
}
