//! Lists this machine's specific IPv4 addresses by interface kind.
//!
//! Classifies them so the most useful hosts can be offered first. Ordinary
//! interfaces are told apart from loopback, virtual interfaces and link-local
//! addresses. Each list is ordered by kind, then interface name, then address.

use std::{
    io,
    net::{IpAddr, Ipv4Addr},
};

/// Returns the specific IPv4 addresses among `addresses`, classified, ordered by kind, then
/// interface name, then address, each once.
pub(super) fn classify(addresses: Vec<InterfaceAddress>) -> Vec<LocalIpv4Candidate> {
    let mut candidates = addresses
        .into_iter()
        .filter_map(LocalIpv4Candidate::from_interface_address)
        .collect::<Vec<_>>();
    candidates.sort_unstable();
    candidates.dedup();
    candidates
}

/// Reads all local interface addresses through the platform's interface API.
pub(super) fn system_interface_addresses() -> io::Result<Vec<InterfaceAddress>> {
    let addresses = getifaddrs::getifaddrs()?
        .filter_map(|interface| {
            let loopback_flag = interface
                .flags
                .contains(getifaddrs::InterfaceFlags::LOOPBACK);
            interface.address.ip_addr().map(|address| {
                InterfaceAddress::new(interface.name, address).with_loopback_flag(loopback_flag)
            })
        })
        .collect();
    Ok(addresses)
}

/// One address observed on one named local network interface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InterfaceAddress {
    interface_name: String,
    address: IpAddr,
    loopback_flag: bool,
}

impl InterfaceAddress {
    /// Creates an interface-address observation whose interface is not flagged
    /// as loopback.
    pub fn new(interface_name: impl Into<String>, address: IpAddr) -> Self {
        Self {
            interface_name: interface_name.into(),
            address,
            loopback_flag: false,
        }
    }

    /// Records whether the operating system flags the interface as loopback.
    pub fn with_loopback_flag(mut self, loopback_flag: bool) -> Self {
        self.loopback_flag = loopback_flag;
        self
    }

    /// Returns the observed address.
    pub fn address(&self) -> IpAddr {
        self.address
    }
}

/// How an operator should read a candidate address. Variants are declared in
/// presentation order, so the derived ordering puts the most useful kinds first.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum LocalIpv4Kind {
    /// An address on a loopback interface, reachable only from this host.
    Loopback,

    /// An address on an ordinary interface, including VPN overlays and
    /// interfaces with names this module does not recognize.
    Physical,

    /// An address on a container, bridge, or hypervisor interface that other
    /// machines usually cannot reach.
    Virtual,

    /// A link-local address, assigned without DHCP or configuration.
    LinkLocal,
}

impl LocalIpv4Kind {
    /// Returns the short operator-facing label, or `None` for ordinary
    /// interfaces that need no annotation.
    pub(super) fn label(self) -> Option<&'static str> {
        match self {
            Self::Loopback => Some("loopback"),
            Self::Physical => None,
            Self::Virtual => Some("virtual"),
            Self::LinkLocal => Some("link-local"),
        }
    }
}

/// A specific IPv4 address presented with its local interface name and kind.
/// The field order defines the candidate ordering.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct LocalIpv4Candidate {
    kind: LocalIpv4Kind,
    interface_name: String,
    address: Ipv4Addr,
}

impl LocalIpv4Candidate {
    /// Creates a named specific IPv4 candidate whose kind is classified from
    /// its address and interface name.
    #[cfg(test)]
    fn new(interface_name: impl Into<String>, address: Ipv4Addr) -> Self {
        Self::classified(interface_name.into(), address, false)
    }

    /// Returns the interface that owns this candidate address.
    pub(super) fn interface_name(&self) -> &str {
        &self.interface_name
    }

    /// Returns the candidate's specific IPv4 address.
    pub(super) fn address(&self) -> Ipv4Addr {
        self.address
    }

    /// Returns how the candidate should be presented and ordered.
    pub(super) fn kind(&self) -> LocalIpv4Kind {
        self.kind
    }

    fn from_interface_address(address: InterfaceAddress) -> Option<Self> {
        let InterfaceAddress {
            interface_name,
            address,
            loopback_flag,
        } = address;
        let IpAddr::V4(address) = address else {
            return None;
        };
        if address.is_unspecified() {
            return None;
        }
        Some(Self::classified(interface_name, address, loopback_flag))
    }

    fn classified(interface_name: String, address: Ipv4Addr, loopback_flag: bool) -> Self {
        let kind = if loopback_flag || address.is_loopback() {
            LocalIpv4Kind::Loopback
        } else if address.is_link_local() {
            LocalIpv4Kind::LinkLocal
        } else if is_virtual_interface_name(&interface_name) {
            LocalIpv4Kind::Virtual
        } else {
            LocalIpv4Kind::Physical
        };
        Self {
            kind,
            interface_name,
            address,
        }
    }
}

/// Guesses whether an interface is virtual from its name. This is a heuristic:
/// the operating system exposes no portable "virtual" flag, so it matches the
/// default prefixes of common container, bridge, and hypervisor tooling, and
/// any other interface is treated as ordinary. VPN overlays are
/// deliberately not matched because other nodes may reach this host through
/// them. A plain bridge is also not matched because it is often the physical LAN
/// bridge, unlike the bridges container tooling creates.
fn is_virtual_interface_name(name: &str) -> bool {
    const VIRTUAL_PREFIXES: &[&str] = &[
        "docker", "podman", "veth", "br-", "virbr", "cni", "flannel", "cali", "lxcbr", "lxdbr",
        "vmnet", "vboxnet",
    ];
    VIRTUAL_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::{InterfaceAddress, LocalIpv4Candidate, classify};

    #[test]
    fn discovers_one_specific_ipv4_candidate() {
        let addresses = vec![address("en0", IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)))];

        let candidates = classify(addresses);
        assert_eq!(candidates, [candidate("en0", Ipv4Addr::new(192, 0, 2, 10))]);
        assert_eq!(candidates[0].interface_name(), "en0");
        assert_eq!(candidates[0].address(), Ipv4Addr::new(192, 0, 2, 10));
    }

    #[test]
    fn orders_and_labels_candidates_by_kind_then_interface_name_then_address() {
        let addresses = vec![
            address("veth1a2b", IpAddr::V4(Ipv4Addr::new(172, 18, 0, 2))),
            address("tun0", IpAddr::V4(Ipv4Addr::new(10, 8, 0, 6))),
            address("br-3f4e5d", IpAddr::V4(Ipv4Addr::new(172, 19, 0, 1))),
            address("docker0", IpAddr::V4(Ipv4Addr::new(172, 17, 0, 1))),
            address("wlan0", IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7))),
            address("enp5s0", IpAddr::V4(Ipv4Addr::new(169, 254, 3, 4))),
            address("lo", IpAddr::V4(Ipv4Addr::new(127, 0, 1, 1))),
            address("podman0", IpAddr::V4(Ipv4Addr::new(10, 88, 0, 1))),
            address("wg0", IpAddr::V4(Ipv4Addr::new(10, 100, 0, 2))),
            address("enp5s0", IpAddr::V4(Ipv4Addr::new(192, 168, 1, 21))),
            address("lo", IpAddr::V4(Ipv4Addr::LOCALHOST)),
            address("tailscale0", IpAddr::V4(Ipv4Addr::new(100, 64, 0, 9))),
            address("enp5s0", IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20))),
            address("mystery7", IpAddr::V4(Ipv4Addr::new(192, 0, 2, 77))),
            address("lo", IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2))),
        ];

        let candidates = classify(addresses);

        assert_eq!(
            summaries(&candidates),
            [
                "lo 127.0.0.1 loopback",
                "lo 127.0.0.2 loopback",
                "lo 127.0.1.1 loopback",
                "enp5s0 192.168.1.20",
                "enp5s0 192.168.1.21",
                "mystery7 192.0.2.77",
                "tailscale0 100.64.0.9",
                "tun0 10.8.0.6",
                "wg0 10.100.0.2",
                "wlan0 10.0.0.7",
                "br-3f4e5d 172.19.0.1 virtual",
                "docker0 172.17.0.1 virtual",
                "podman0 10.88.0.1 virtual",
                "veth1a2b 172.18.0.2 virtual",
                "enp5s0 169.254.3.4 link-local",
            ]
        );
        assert_eq!(candidates[0], candidate("lo", Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn link_local_addresses_are_labeled_regardless_of_interface_name() {
        let addresses = vec![
            address("veth0", IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1))),
            address("wlan0", IpAddr::V4(Ipv4Addr::new(169, 254, 9, 9))),
        ];

        assert_eq!(
            summaries(&classify(addresses)),
            [
                "veth0 169.254.1.1 link-local",
                "wlan0 169.254.9.9 link-local",
            ]
        );
    }

    #[test]
    fn loopback_flag_marks_a_non_127_address_as_loopback() {
        let addresses = vec![
            address("enp5s0", IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20))),
            address("lo0", IpAddr::V4(Ipv4Addr::new(10, 255, 0, 1))).with_loopback_flag(true),
        ];

        assert_eq!(
            summaries(&classify(addresses)),
            ["lo0 10.255.0.1 loopback", "enp5s0 192.168.1.20",]
        );
    }

    #[test]
    fn interface_names_alone_do_not_mark_loopback() {
        let addresses = vec![address("lo", IpAddr::V4(Ipv4Addr::new(192, 0, 2, 5)))];

        assert_eq!(summaries(&classify(addresses)), ["lo 192.0.2.5"]);
    }

    #[test]
    fn removes_exact_duplicates_and_filters_non_specific_or_non_ipv4_addresses() {
        let addresses = vec![
            address("en0", IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))),
            address("en0", IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))),
            address("en1", IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))),
            address("lo", IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
            address("en0", IpAddr::V6(Ipv6Addr::LOCALHOST)),
        ];

        assert_eq!(
            classify(addresses),
            [
                candidate("en0", Ipv4Addr::new(192, 0, 2, 10)),
                candidate("en1", Ipv4Addr::new(192, 0, 2, 10)),
            ]
        );
    }

    /// Renders each candidate as `<interface> <address> [<label>]`.
    fn summaries(candidates: &[LocalIpv4Candidate]) -> Vec<String> {
        candidates
            .iter()
            .map(|candidate| {
                format!(
                    "{} {} {}",
                    candidate.interface_name(),
                    candidate.address(),
                    candidate.kind().label().unwrap_or("")
                )
                .trim_end()
                .to_owned()
            })
            .collect()
    }

    fn address(interface_name: &str, address: IpAddr) -> InterfaceAddress {
        InterfaceAddress::new(interface_name, address)
    }

    fn candidate(interface_name: &str, address: Ipv4Addr) -> LocalIpv4Candidate {
        LocalIpv4Candidate::new(interface_name, address)
    }
}
