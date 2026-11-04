//! Ranked hosts that other machines may reach this machine at.
//!
//! Lists each host with a short label, most useful first: tailnet name, tailnet addresses, LAN
//! addresses, then virtual, loopback and link-local addresses. Only a tailnet name or address this
//! machine holds, or a LAN address, can be recommended; the rest are listed so the operator can
//! pick knowingly.

use std::net::Ipv4Addr;

use dotcore_domain::SiteNodeHost;

use super::{
    HostEnvironment, TailnetStatus,
    interfaces::{LocalIpv4Candidate, LocalIpv4Kind, classify},
};

/// One host other machines may reach this machine at, with a short operator-facing label such
/// naming its kind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostCandidate {
    host: SiteNodeHost,
    label: String,
    recommendable: bool,
}

impl HostCandidate {
    /// Creates a candidate for `host` shown with `label`.
    pub fn new(host: SiteNodeHost, label: impl Into<String>, recommendable: bool) -> Self {
        Self {
            host,
            label: label.into(),
            recommendable,
        }
    }

    /// Returns the candidate host, an IPv4 address or the tailnet name.
    pub fn host(&self) -> &SiteNodeHost {
        &self.host
    }

    /// Returns the short label shown next to the host.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Reports whether the candidate may be the default: other machines can usually reach it
    /// and the server can listen on it.
    pub fn is_recommendable(&self) -> bool {
        self.recommendable
    }

    fn ipv4(address: Ipv4Addr, label: impl Into<String>, recommendable: bool) -> Option<Self> {
        SiteNodeHost::from_ipv4(address)
            .ok()
            .map(|host| Self::new(host, label, recommendable))
    }
}

/// Returns this machine's host candidates in rank order, each host listed once.
pub(super) async fn discover_host_candidates(
    environment: &dyn HostEnvironment,
) -> Vec<HostCandidate> {
    rank(&read_host_facts(environment).await)
}

/// What ranking needs to know about this machine.
#[derive(Debug, Default)]
struct HostFacts {
    /// Interface addresses; empty when they cannot be read.
    interfaces: Vec<LocalIpv4Candidate>,
    tailnet: Option<TailnetStatus>,

    /// Addresses the tailnet name resolves to here.
    name_addresses: Vec<Ipv4Addr>,
}

async fn read_host_facts(environment: &dyn HostEnvironment) -> HostFacts {
    let interfaces = environment
        .interface_addresses()
        .map(classify)
        .inspect_err(|error| tracing::debug!(%error, "local interface addresses unavailable"))
        .unwrap_or_default();
    let tailnet = environment.tailnet_status().await;
    let name = tailnet.as_ref().and_then(|status| status.dns_name.as_ref());
    let name_addresses = match name {
        Some(name) => name
            .resolve_all(environment.resolver())
            .await
            .inspect_err(|error| tracing::debug!(%name, %error, "tailnet name does not resolve"))
            .unwrap_or_default(),
        None => Vec::new(),
    };
    HostFacts {
        interfaces,
        tailnet,
        name_addresses,
    }
}

/// Ranks the tailnet name first only when it resolves to one of the tailnet addresses, then
/// the tailnet addresses, then a name that does not resolve here, then the interfaces by kind.
fn rank(facts: &HostFacts) -> Vec<HostCandidate> {
    // Unreadable interfaces leave every tailnet address trusted rather than none.
    let held = |address: &Ipv4Addr| {
        facts.interfaces.is_empty()
            || facts
                .interfaces
                .iter()
                .any(|interface| interface.address() == *address)
    };
    let mut candidates = Vec::new();

    if let Some(tailnet) = &facts.tailnet {
        let name_target = facts
            .name_addresses
            .iter()
            .find(|address| tailnet.ipv4.contains(address));
        let name = tailnet.dns_name.as_ref();
        if let (Some(name), Some(target)) = (name, name_target) {
            candidates.push(holdable(name.clone(), "tailnet name", held(target)));
        }
        for address in &tailnet.ipv4 {
            if let Ok(host) = SiteNodeHost::from_ipv4(*address) {
                candidates.push(holdable(host, "tailnet ip", held(address)));
            }
        }
        if let (Some(name), None) = (name.cloned(), name_target) {
            candidates.push(HostCandidate::new(
                name,
                "tailnet name, does not resolve here",
                false,
            ));
        }
    }

    for kind in [
        LocalIpv4Kind::Physical,
        LocalIpv4Kind::Virtual,
        LocalIpv4Kind::Loopback,
        LocalIpv4Kind::LinkLocal,
    ] {
        candidates.extend(
            facts
                .interfaces
                .iter()
                .filter(|interface| interface.kind() == kind)
                .filter_map(|interface| match kind.label() {
                    Some(label) => HostCandidate::ipv4(interface.address(), label, false),
                    None => HostCandidate::ipv4(
                        interface.address(),
                        format!("address on {}", interface.interface_name()),
                        true,
                    ),
                }),
        );
    }

    // A tailnet address also seen on its interface stays listed as a tailnet address.
    let mut unique: Vec<HostCandidate> = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if !unique.iter().any(|kept| kept.host == candidate.host) {
            unique.push(candidate);
        }
    }
    unique
}

/// A tailnet candidate, recommendable only when this machine holds its address, as it does not
/// with userspace networking.
fn holdable(host: SiteNodeHost, label: &str, held: bool) -> HostCandidate {
    if held {
        HostCandidate::new(host, label, true)
    } else {
        HostCandidate::new(host, format!("{label}, not on this machine"), false)
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use dotcore_domain::SiteNodeHost;

    use super::{HostCandidate, HostFacts, rank};
    use crate::hosts::{
        TailnetStatus,
        interfaces::{InterfaceAddress, classify},
    };

    const NAME: &str = "gpu1.example.ts.net";
    const TAILNET_IPV4: Ipv4Addr = Ipv4Addr::new(100, 101, 102, 103);
    const LAN_IPV4: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 20);

    #[test]
    fn a_resolving_tailnet_name_leads_then_the_tailnet_ip_then_the_interfaces_by_kind() {
        let facts = facts(
            &[
                ("lo", Ipv4Addr::LOCALHOST),
                ("enp5s0", Ipv4Addr::new(169, 254, 3, 4)),
                ("docker0", Ipv4Addr::new(172, 17, 0, 1)),
                ("tailscale0", TAILNET_IPV4),
                ("wlan0", Ipv4Addr::new(10, 0, 0, 7)),
                ("enp5s0", LAN_IPV4),
            ],
            Some(tailnet(Some(NAME))),
            &[TAILNET_IPV4],
        );

        assert_eq!(
            summaries(&rank(&facts)),
            [
                "gpu1.example.ts.net tailnet name *",
                "100.101.102.103 tailnet ip *",
                "192.168.1.20 address on enp5s0 *",
                "10.0.0.7 address on wlan0 *",
                "172.17.0.1 virtual",
                "127.0.0.1 loopback",
                "169.254.3.4 link-local",
            ]
        );
    }

    #[test]
    fn a_tailnet_name_that_does_not_resolve_to_the_tailnet_follows_the_tailnet_ip() {
        for name_addresses in [&[][..], &[Ipv4Addr::new(127, 0, 0, 2)]] {
            let facts = facts(
                &[("tailscale0", TAILNET_IPV4), ("enp5s0", LAN_IPV4)],
                Some(tailnet(Some(NAME))),
                name_addresses,
            );

            assert_eq!(
                summaries(&rank(&facts)),
                [
                    "100.101.102.103 tailnet ip *",
                    "gpu1.example.ts.net tailnet name, does not resolve here",
                    "192.168.1.20 address on enp5s0 *",
                ],
                "{name_addresses:?}"
            );
        }
    }

    #[test]
    fn a_tailnet_address_no_interface_holds_is_listed_but_not_recommendable() {
        let facts = facts(
            &[("lo", Ipv4Addr::LOCALHOST), ("enp5s0", LAN_IPV4)],
            Some(tailnet(Some(NAME))),
            &[TAILNET_IPV4],
        );

        assert_eq!(
            summaries(&rank(&facts)),
            [
                "gpu1.example.ts.net tailnet name, not on this machine",
                "100.101.102.103 tailnet ip, not on this machine",
                "192.168.1.20 address on enp5s0 *",
                "127.0.0.1 loopback",
            ]
        );
    }

    #[test]
    fn without_a_tailnet_the_lan_leads_in_interface_order() {
        let facts = facts(
            &[
                ("lo", Ipv4Addr::LOCALHOST),
                ("wlan0", Ipv4Addr::new(10, 0, 0, 7)),
                ("utun3", Ipv4Addr::new(100, 100, 1, 2)),
                ("enp5s0", LAN_IPV4),
            ],
            None,
            &[],
        );

        assert_eq!(
            summaries(&rank(&facts)),
            [
                "192.168.1.20 address on enp5s0 *",
                "100.100.1.2 address on utun3 *",
                "10.0.0.7 address on wlan0 *",
                "127.0.0.1 loopback",
            ]
        );
    }

    #[test]
    fn only_loopback_lists_nothing_recommendable() {
        let facts = facts(
            &[
                ("lo", Ipv4Addr::new(127, 0, 0, 2)),
                ("lo", Ipv4Addr::LOCALHOST),
            ],
            None,
            &[],
        );

        assert_eq!(
            summaries(&rank(&facts)),
            ["127.0.0.1 loopback", "127.0.0.2 loopback"]
        );
    }

    #[test]
    fn unreadable_interfaces_still_list_the_tailnet() {
        let facts = facts(&[], Some(tailnet(Some(NAME))), &[TAILNET_IPV4]);

        assert_eq!(
            summaries(&rank(&facts)),
            [
                "gpu1.example.ts.net tailnet name *",
                "100.101.102.103 tailnet ip *",
            ]
        );
    }

    fn facts(
        interfaces: &[(&str, Ipv4Addr)],
        tailnet: Option<TailnetStatus>,
        name_addresses: &[Ipv4Addr],
    ) -> HostFacts {
        HostFacts {
            interfaces: classify(
                interfaces
                    .iter()
                    .map(|(name, address)| InterfaceAddress::new(*name, (*address).into()))
                    .collect(),
            ),
            tailnet,
            name_addresses: name_addresses.to_vec(),
        }
    }

    fn tailnet(name: Option<&str>) -> TailnetStatus {
        TailnetStatus {
            dns_name: name.map(|name| SiteNodeHost::parse(name).expect("name")),
            ipv4: vec![TAILNET_IPV4],
        }
    }

    /// Renders each candidate as `<host> <label>`, with ` *` when recommendable.
    fn summaries(candidates: &[HostCandidate]) -> Vec<String> {
        candidates
            .iter()
            .map(|candidate| {
                let mark = if candidate.is_recommendable() {
                    " *"
                } else {
                    ""
                };
                format!("{} {}{mark}", candidate.host(), candidate.label())
            })
            .collect()
    }
}
