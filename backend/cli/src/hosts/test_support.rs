//! A fake machine for host selection tests.
//!
//! Fixes interface addresses, tailnet status, host-name answers and process variables, so tests
//! never read the real machine or network. It counts tailnet reads to show whether the machine was
//! searched.

use std::{
    io,
    net::Ipv4Addr,
    sync::atomic::{AtomicUsize, Ordering},
};

use dotcore::testing::FakeHostResolver;
use dotcore_domain::{HostResolver, SiteNodeHost};
use futures_util::future::BoxFuture;

use super::{HostEnvironment, InterfaceAddress, ListenerHost, TailnetStatus};

/// The MagicDNS name of [`FakeHostEnvironment::on_tailnet`].
pub(crate) const TAILNET_NAME: &str = "gpu1.example.ts.net";

/// A machine described by its interfaces, tailnet, host names and variables.
#[derive(Debug, Default)]
pub(crate) struct FakeHostEnvironment {
    interfaces: Option<Vec<InterfaceAddress>>,
    tailnet: Option<TailnetStatus>,
    resolver: FakeHostResolver,
    variables: Vec<(&'static str, String)>,
    tailnet_reads: AtomicUsize,
}

impl FakeHostEnvironment {
    /// On a tailnet with MagicDNS resolving [`TAILNET_NAME`] to `100.101.102.103`, which
    /// `tailscale0` holds, and on a LAN at `192.168.1.20` on `enp5s0`.
    pub(crate) fn on_tailnet() -> Self {
        let tailnet_ipv4 = Ipv4Addr::new(100, 101, 102, 103);
        Self::default()
            .with_interface("lo", Ipv4Addr::LOCALHOST)
            .with_interface("tailscale0", tailnet_ipv4)
            .with_interface("enp5s0", Ipv4Addr::new(192, 168, 1, 20))
            .with_tailnet(Some(TAILNET_NAME), [tailnet_ipv4])
            .with_name(TAILNET_NAME, [tailnet_ipv4])
    }

    /// Without Tailscale and without a LAN address: only loopback and a container bridge.
    pub(crate) fn without_tailnet_or_lan() -> Self {
        Self::default()
            .with_interface("lo", Ipv4Addr::LOCALHOST)
            .with_interface("docker0", Ipv4Addr::new(172, 17, 0, 1))
    }

    pub(crate) fn with_interface(mut self, name: &str, address: Ipv4Addr) -> Self {
        self.interfaces
            .get_or_insert_default()
            .push(InterfaceAddress::new(name, address.into()));
        self
    }

    /// A running tailnet, with MagicDNS enabled when `name` is given.
    pub(crate) fn with_tailnet(
        mut self,
        name: Option<&str>,
        ipv4: impl IntoIterator<Item = Ipv4Addr>,
    ) -> Self {
        self.tailnet = Some(TailnetStatus {
            dns_name: name.map(|name| SiteNodeHost::parse(name).expect("tailnet name")),
            ipv4: ipv4.into_iter().collect(),
        });
        self
    }

    pub(crate) fn with_name(
        mut self,
        name: &str,
        addresses: impl IntoIterator<Item = Ipv4Addr>,
    ) -> Self {
        self.resolver = self.resolver.with(name, addresses);
        self
    }

    pub(crate) fn with_variable(mut self, name: &'static str, value: &str) -> Self {
        self.variables.push((name, value.to_owned()));
        self
    }

    /// Returns how often the tailnet status was read, which only discovery does.
    pub(crate) fn tailnet_reads(&self) -> usize {
        self.tailnet_reads.load(Ordering::SeqCst)
    }
}

impl HostEnvironment for FakeHostEnvironment {
    fn interface_addresses(&self) -> io::Result<Vec<InterfaceAddress>> {
        self.interfaces
            .clone()
            .ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, "interfaces unread"))
    }

    fn tailnet_status(&self) -> BoxFuture<'_, Option<TailnetStatus>> {
        self.tailnet_reads.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::ready(self.tailnet.clone()))
    }

    fn resolver(&self) -> &dyn HostResolver {
        &self.resolver
    }

    fn variable(&self, name: &'static str) -> anyhow::Result<Option<String>> {
        Ok(self
            .variables
            .iter()
            .find(|(variable, _)| *variable == name)
            .map(|(_, value)| value.clone()))
    }
}

/// The host required by a lone `DOTCORE_INFERENCE_ADDR` listener at `address`.
pub(crate) fn listener_host(address: &str) -> ListenerHost {
    listener_host_for(&[("DOTCORE_INFERENCE_ADDR", address)])
}

/// The host required by the listener variables set to the given IPv4 addresses.
pub(crate) fn listener_host_for(listeners: &[(&'static str, &str)]) -> ListenerHost {
    let environment = listeners.iter().fold(
        FakeHostEnvironment::default(),
        |environment, (name, address)| environment.with_variable(name, address),
    );
    crate::build_runtime()
        .expect("runtime")
        .block_on(ListenerHost::constrain(None, &environment))
        .expect("listener variables")
        .expect("required host")
}
