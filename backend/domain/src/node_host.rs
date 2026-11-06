//! The host a SiteNode is reached at, and the listener addresses a node binds, each either an
//! IPv4 address or a DNS host name, for example `10.0.0.7` or `manager.example.ts.net:7443`.
//!
//! Checking a host never queries DNS, so site validation stays deterministic and offline. A name
//! is looked up only when a node uses it, through an injectable resolver that returns IPv4
//! addresses, so each machine reaches the address the name has on its own network.

use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    str::FromStr,
};

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

/// Longest DNS host name, without a trailing dot.
const MAX_NAME_LENGTH: usize = 253;

/// Longest label of a DNS host name.
const MAX_LABEL_LENGTH: usize = 63;

/// Longest wait for the system resolver, so an unreachable DNS server fails startup, a join, or
/// a container creation instead of stalling it.
// Not a behavior duration: bound on a real host-name lookup.
const LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Host of a SiteNode: a specific IPv4 address or a DNS host name in canonical form (lowercase,
/// no trailing dot). Wildcard addresses and IPv6 addresses are never hosts.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SiteNodeHost(Host);

impl SiteNodeHost {
    /// The IPv4 loopback address `127.0.0.1`, the host of a site reached only from its own
    /// machine.
    pub const LOOPBACK: Self = Self(Host::Ipv4(Ipv4Addr::LOCALHOST));

    /// Parses a recorded host, which must already be in canonical form.
    pub fn parse(value: &str) -> Result<Self, SiteNodeHostError> {
        match value.parse::<IpAddr>() {
            Ok(IpAddr::V4(address)) => Self::from_ipv4(address),
            Ok(IpAddr::V6(_)) => Err(SiteNodeHostError::Ipv6),
            Err(_) => {
                validate_name(&value.to_ascii_lowercase())?;
                if value.bytes().any(|byte| byte.is_ascii_uppercase()) || value.ends_with('.') {
                    return Err(SiteNodeHostError::NotCanonical);
                }
                Ok(Self(Host::Name(value.to_owned())))
            }
        }
    }

    /// Returns the host for a specific IPv4 address; the wildcard address is never a host.
    pub fn from_ipv4(address: Ipv4Addr) -> Result<Self, SiteNodeHostError> {
        if address.is_unspecified() {
            return Err(SiteNodeHostError::Unspecified);
        }
        Ok(Self(Host::Ipv4(address)))
    }

    /// Parses an operator or peer input, first lowercasing it and dropping one trailing dot, for
    /// example `Manager.Example.` becomes `manager.example`.
    pub fn normalize(value: &str) -> Result<Self, SiteNodeHostError> {
        let value = value.strip_suffix('.').unwrap_or(value);
        Self::parse(&value.to_ascii_lowercase())
    }

    /// Returns the IPv4 address, when the host is an address rather than a name.
    pub fn ipv4(&self) -> Option<Ipv4Addr> {
        match &self.0 {
            Host::Ipv4(address) => Some(*address),
            Host::Name(_) => None,
        }
    }

    /// Reports whether the host is a DNS name that has to be resolved before use.
    pub fn is_name(&self) -> bool {
        matches!(self.0, Host::Name(_))
    }

    /// Returns the IPv4 address this machine reaches the host at: the address itself, or the
    /// first usable IPv4 address `resolver` returns for a name.
    pub async fn resolve(
        &self,
        resolver: &dyn HostResolver,
    ) -> Result<Ipv4Addr, HostResolutionError> {
        Ok(self.resolve_all(resolver).await?[0])
    }

    /// Returns every IPv4 address this machine reaches the host at, in resolver order: the
    /// address itself, or what `resolver` returns for a name. Wildcard, multicast, and broadcast
    /// answers are dropped, since no host is reached there; at least one address remains.
    pub async fn resolve_all(
        &self,
        resolver: &dyn HostResolver,
    ) -> Result<Vec<Ipv4Addr>, HostResolutionError> {
        let name = match &self.0 {
            Host::Ipv4(address) => return Ok(vec![*address]),
            Host::Name(name) => name,
        };
        let addresses: Vec<_> = resolver
            .lookup_ipv4(name)
            .await
            .map_err(|source| HostResolutionError::Lookup {
                host: name.clone(),
                source,
            })?
            .into_iter()
            .filter(|address| {
                !address.is_unspecified() && !address.is_multicast() && !address.is_broadcast()
            })
            .collect();
        if addresses.is_empty() {
            return Err(HostResolutionError::NoIpv4 { host: name.clone() });
        }
        Ok(addresses)
    }

    /// Resolves the host like [`Self::resolve`], also describing why other machines cannot
    /// reach it when it is a name that resolves to a loopback address on this machine, for
    /// example a NixOS short host name resolving to `127.0.0.2`.
    pub async fn resolve_with_loopback_warning(
        &self,
        resolver: &dyn HostResolver,
    ) -> Result<(Ipv4Addr, Option<String>), HostResolutionError> {
        let address = self.resolve(resolver).await?;
        Ok((address, self.loopback_warning(address)))
    }

    /// Describes why other machines cannot use a name that resolved to a loopback address on
    /// this machine; `None` for an address or a name that resolved elsewhere.
    fn loopback_warning(&self, resolved: Ipv4Addr) -> Option<String> {
        (self.is_name() && resolved.is_loopback()).then(|| {
            format!(
                "host name {self} resolves to the loopback address {resolved} on this machine, \
                 which other machines cannot reach; use its fully qualified name"
            )
        })
    }
}

impl fmt::Display for SiteNodeHost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Host::Ipv4(address) => address.fmt(formatter),
            Host::Name(name) => formatter.write_str(name),
        }
    }
}

impl FromStr for SiteNodeHost {
    type Err = SiteNodeHostError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

/// A value violates the SiteNode host rule.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SiteNodeHostError {
    /// The value is neither an IPv4 address nor a valid DNS host name.
    #[error("must be an IPv4 address or a DNS host name")]
    Invalid,

    /// The value is the wildcard IPv4 address.
    #[error("must be a specific non-wildcard IPv4 address or a DNS host name")]
    Unspecified,

    /// The value is an IPv6 address.
    #[error("must be an IPv4 address or a DNS host name, not an IPv6 address")]
    Ipv6,

    /// The value is a DNS host name that is not lowercase or ends with a dot.
    #[error("must be a lowercase DNS host name without a trailing dot")]
    NotCanonical,
}

/// Address a listener binds, from a process setting or a node folder: an IPv4 address, possibly
/// the wildcard, or a DNS host name, with a port, for example `manager.example.ts.net:7443`.
///
/// A name is resolved when the listener starts, and the listener binds the first IPv4 address.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BindAddress {
    host: BindHost,
    port: u16,
}

impl BindAddress {
    /// Parses a `host:port` setting; a name is lowercased and loses one trailing dot.
    pub fn parse(value: &str) -> Result<Self, BindAddressError> {
        Self::parse_with(value, SiteNodeHost::normalize)
    }

    /// Returns the address binding `host` at `port`.
    pub fn new(host: SiteNodeHost, port: u16) -> Self {
        Self {
            host: BindHost::Host(host),
            port,
        }
    }

    /// Returns the host, or `None` for the wildcard address.
    pub fn host(&self) -> Option<&SiteNodeHost> {
        match &self.host {
            BindHost::Wildcard => None,
            BindHost::Host(host) => Some(host),
        }
    }

    /// Returns the port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Reports whether the address is the IPv4 wildcard, which binds every interface.
    pub fn is_wildcard(&self) -> bool {
        matches!(self.host, BindHost::Wildcard)
    }

    /// Returns the socket address to bind, resolving a name through `resolver`.
    pub async fn resolve(
        &self,
        resolver: &dyn HostResolver,
    ) -> Result<SocketAddr, HostResolutionError> {
        let address = match &self.host {
            BindHost::Wildcard => Ipv4Addr::UNSPECIFIED,
            BindHost::Host(host) => host.resolve(resolver).await?,
        };
        Ok(SocketAddr::V4(SocketAddrV4::new(address, self.port)))
    }

    /// Parses a recorded `host:port`, whose name must already be in canonical form.
    fn parse_canonical(value: &str) -> Result<Self, BindAddressError> {
        Self::parse_with(value, SiteNodeHost::parse)
    }

    fn parse_with(
        value: &str,
        parse_host: fn(&str) -> Result<SiteNodeHost, SiteNodeHostError>,
    ) -> Result<Self, BindAddressError> {
        let invalid = || BindAddressError {
            value: value.to_owned(),
        };
        let (host, port) = value.rsplit_once(':').ok_or_else(invalid)?;
        let port = port.parse::<u16>().map_err(|_| invalid())?;
        let host = match host.parse::<Ipv4Addr>() {
            Ok(address) if address.is_unspecified() => BindHost::Wildcard,
            _ => BindHost::Host(parse_host(host).map_err(|_| invalid())?),
        };
        Ok(Self { host, port })
    }
}

impl fmt::Display for BindAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.host {
            BindHost::Wildcard => write!(formatter, "{}:{}", Ipv4Addr::UNSPECIFIED, self.port),
            BindHost::Host(host) => write!(formatter, "{host}:{}", self.port),
        }
    }
}

impl FromStr for BindAddress {
    type Err = BindAddressError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl From<SocketAddrV4> for BindAddress {
    fn from(address: SocketAddrV4) -> Self {
        Self {
            host: SiteNodeHost::from_ipv4(*address.ip()).map_or(BindHost::Wildcard, BindHost::Host),
            port: address.port(),
        }
    }
}

impl TryFrom<String> for BindAddress {
    type Error = BindAddressError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse_canonical(&value)
    }
}

impl From<BindAddress> for String {
    fn from(address: BindAddress) -> Self {
        address.to_string()
    }
}

/// A listener address is not an IPv4 address or DNS host name with a port.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error(
    "{value:?} must be an IPv4 address or a DNS host name with a port, for example 10.0.0.7:7443"
)]
pub struct BindAddressError {
    value: String,
}

/// Looks up the IPv4 addresses of DNS host names; tests replace the system resolver with a fake.
pub trait HostResolver: Send + Sync {
    /// Returns the IPv4 addresses of `name` in resolver order; an empty list when it has only
    /// other addresses.
    fn lookup_ipv4<'a>(&'a self, name: &'a str) -> BoxFuture<'a, std::io::Result<Vec<Ipv4Addr>>>;
}

/// The operating system's resolver, the same one other programs on the machine use, queried off
/// the async worker threads.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemHostResolver;

impl HostResolver for SystemHostResolver {
    #[expect(clippy::disallowed_methods, reason = "bounds a real host-name lookup")]
    fn lookup_ipv4<'a>(&'a self, name: &'a str) -> BoxFuture<'a, std::io::Result<Vec<Ipv4Addr>>> {
        Box::pin(async move {
            let addresses =
                tokio::time::timeout(LOOKUP_TIMEOUT, tokio::net::lookup_host((name, 0)))
                    .await
                    .map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            format!("the lookup did not answer within {LOOKUP_TIMEOUT:?}"),
                        )
                    })??;
            let mut ipv4 = Vec::new();
            for address in addresses {
                if let SocketAddr::V4(address) = address
                    && !ipv4.contains(address.ip())
                {
                    ipv4.push(*address.ip());
                }
            }
            Ok(ipv4)
        })
    }
}

/// A host name could not be turned into an IPv4 address on this machine.
#[derive(Debug, thiserror::Error)]
pub enum HostResolutionError {
    /// The resolver failed, for example because the network that provides the name is not up.
    #[error("could not resolve host name {host}; check that the network providing it is up")]
    Lookup {
        /// Name being resolved.
        host: String,
        /// Resolver failure.
        #[source]
        source: std::io::Error,
    },

    /// The resolver answered with no IPv4 address.
    #[error("host name {host} has no IPv4 address on this machine")]
    NoIpv4 {
        /// Name being resolved.
        host: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Host {
    Ipv4(Ipv4Addr),
    Name(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum BindHost {
    Wildcard,
    Host(SiteNodeHost),
}

/// Checks an RFC 1123 host name: 1 to 63 character labels of letters, digits, and inner hyphens,
/// at most 253 characters, and a last label that is neither all digits nor `0x` followed by hex
/// digits, so the system resolver never reads it as an address (`0x7f000001` is `127.0.0.1`).
fn validate_name(name: &str) -> Result<(), SiteNodeHostError> {
    let name = name.strip_suffix('.').unwrap_or(name);
    if name.is_empty() || name.len() > MAX_NAME_LENGTH {
        return Err(SiteNodeHostError::Invalid);
    }
    let valid_label = |label: &str| {
        (1..=MAX_LABEL_LENGTH).contains(&label.len())
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    };
    if !name.split('.').all(valid_label) {
        return Err(SiteNodeHostError::Invalid);
    }
    if name.rsplit('.').next().is_some_and(|last| {
        last.bytes().all(|byte| byte.is_ascii_digit())
            || last
                .strip_prefix("0x")
                .is_some_and(|hex| hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
    }) {
        return Err(SiteNodeHostError::Invalid);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;
    use crate::test_support::host_resolver::FakeHostResolver;

    #[test]
    fn hosts_are_ipv4_addresses_or_canonical_names() {
        for valid in [
            "10.0.0.7",
            "localhost",
            "manager.example.ts.net",
            "a1-b.c2",
            "0x0.example",
            "manager.0xg1",
        ] {
            let host = SiteNodeHost::parse(valid).expect(valid);
            assert_eq!(host.to_string(), valid);
        }
        assert_eq!(
            SiteNodeHost::parse("10.0.0.7").expect("address").ipv4(),
            Some(Ipv4Addr::new(10, 0, 0, 7))
        );
        assert!(!SiteNodeHost::parse("10.0.0.7").expect("address").is_name());
        assert!(SiteNodeHost::parse("localhost").expect("name").is_name());
        assert_eq!(SiteNodeHost::parse("localhost").expect("name").ipv4(), None);
    }

    #[test]
    fn inputs_are_normalized_and_recorded_hosts_must_be_canonical() {
        assert_eq!(
            SiteNodeHost::normalize("Manager.Example.TS.net.")
                .expect("input")
                .to_string(),
            "manager.example.ts.net"
        );
        assert_eq!(
            SiteNodeHost::normalize("10.0.0.7.").expect("input").ipv4(),
            Some(Ipv4Addr::new(10, 0, 0, 7))
        );
        for value in ["Manager.example", "manager.example.", "Localhost"] {
            assert_eq!(
                SiteNodeHost::parse(value),
                Err(SiteNodeHostError::NotCanonical),
                "{value}"
            );
        }
    }

    #[test]
    fn invalid_names_wildcards_and_ipv6_addresses_are_rejected() {
        let long_label = "a".repeat(64);
        let long_name = ["a".repeat(63).as_str(); 4].join(".");
        assert_eq!(long_name.len(), 255);
        for invalid in [
            "",
            ".",
            "a..b",
            ".manager",
            "-manager",
            "manager-.example",
            long_label.as_str(),
            long_name.as_str(),
            "desk_app",
            "desk app",
            "1.2.3",
            "10.0.0.256",
            "0x0",
            "0X7F000001",
            "0x7f000001",
            "1.0x2",
            "manager.0xab",
            "manager.example:7443",
        ] {
            assert_eq!(
                SiteNodeHost::normalize(invalid),
                Err(SiteNodeHostError::Invalid),
                "{invalid:?}"
            );
        }
        assert_eq!(
            SiteNodeHost::parse("0.0.0.0"),
            Err(SiteNodeHostError::Unspecified)
        );
        for ipv6 in ["::1", "::", "fd7a:115c:a1e0::1"] {
            assert_eq!(SiteNodeHost::parse(ipv6), Err(SiteNodeHostError::Ipv6));
        }

        let longest = [
            "a".repeat(63).as_str(),
            "b".repeat(63).as_str(),
            "c".repeat(63).as_str(),
            "d".repeat(61).as_str(),
        ]
        .join(".");
        assert_eq!(longest.len(), 253);
        SiteNodeHost::parse(&longest).expect("253-character name");
    }

    #[tokio::test]
    async fn names_resolve_to_their_first_ipv4_address_and_addresses_to_themselves() {
        let resolver = FakeHostResolver::default().with(
            "manager.example",
            [Ipv4Addr::new(100, 64, 0, 7), Ipv4Addr::new(10, 0, 0, 7)],
        );

        let name = SiteNodeHost::parse("manager.example").expect("name");
        assert_eq!(
            name.resolve(&resolver).await.expect("resolved"),
            Ipv4Addr::new(100, 64, 0, 7)
        );
        let address = SiteNodeHost::parse("10.0.0.9").expect("address");
        assert_eq!(
            address.resolve(&resolver).await.expect("literal"),
            Ipv4Addr::new(10, 0, 0, 9)
        );
        assert_eq!(resolver.lookups(), ["manager.example"]);
    }

    #[tokio::test]
    async fn resolution_failures_name_the_host_and_keep_the_cause() {
        let resolver = FakeHostResolver::default().with("ipv6-only.example", []);

        let error = SiteNodeHost::parse("ipv6-only.example")
            .expect("name")
            .resolve(&resolver)
            .await
            .expect_err("no IPv4 address");
        assert_eq!(
            error.to_string(),
            "host name ipv6-only.example has no IPv4 address on this machine"
        );

        let resolver = resolver
            .with("wildcard.example", [Ipv4Addr::UNSPECIFIED])
            .with(
                "mixed.example",
                [
                    Ipv4Addr::UNSPECIFIED,
                    Ipv4Addr::BROADCAST,
                    Ipv4Addr::new(224, 0, 0, 1),
                    Ipv4Addr::new(10, 0, 0, 7),
                ],
            );
        let error = SiteNodeHost::parse("wildcard.example")
            .expect("name")
            .resolve(&resolver)
            .await
            .expect_err("only a wildcard answer");
        assert_eq!(
            error.to_string(),
            "host name wildcard.example has no IPv4 address on this machine"
        );
        assert_eq!(
            SiteNodeHost::parse("mixed.example")
                .expect("name")
                .resolve_all(&resolver)
                .await
                .expect("one usable address"),
            [Ipv4Addr::new(10, 0, 0, 7)]
        );

        let error = SiteNodeHost::parse("missing.example")
            .expect("name")
            .resolve(&resolver)
            .await
            .expect_err("failed lookup");
        assert_eq!(
            error.to_string(),
            "could not resolve host name missing.example; check that the network providing it \
             is up"
        );
        assert_eq!(
            error.source().map(ToString::to_string).as_deref(),
            Some("missing.example is unknown")
        );
    }

    #[tokio::test]
    async fn the_system_resolver_resolves_localhost_to_loopback() {
        let host = SiteNodeHost::parse("localhost").expect("name");
        let address = host
            .resolve(&SystemHostResolver)
            .await
            .expect("localhost resolves");
        assert!(address.is_loopback(), "{address}");
    }

    #[test]
    fn only_names_resolving_to_loopback_warn() {
        let name = SiteNodeHost::parse("manager").expect("name");
        assert_eq!(
            name.loopback_warning(Ipv4Addr::new(127, 0, 0, 2))
                .as_deref(),
            Some(
                "host name manager resolves to the loopback address 127.0.0.2 on this machine, \
                 which other machines cannot reach; use its fully qualified name"
            )
        );
        assert_eq!(name.loopback_warning(Ipv4Addr::new(100, 64, 0, 7)), None);
        let address = SiteNodeHost::parse("127.0.0.1").expect("address");
        assert_eq!(address.loopback_warning(Ipv4Addr::LOCALHOST), None);
    }

    #[tokio::test]
    async fn bind_addresses_keep_names_until_resolved() {
        let resolver = FakeHostResolver::default()
            .with("manager.example", [Ipv4Addr::new(100, 64, 0, 7)])
            .with("localhost", [Ipv4Addr::LOCALHOST]);

        let named = BindAddress::parse("Manager.Example.:7443").expect("named address");
        assert_eq!(named.to_string(), "manager.example:7443");
        assert!(!named.is_wildcard());
        assert_eq!(
            named.resolve(&resolver).await.expect("resolved"),
            "100.64.0.7:7443".parse::<SocketAddr>().expect("socket")
        );

        let literal = BindAddress::parse("10.0.0.7:7443").expect("literal");
        assert_eq!(
            literal.resolve(&resolver).await.expect("literal"),
            "10.0.0.7:7443".parse::<SocketAddr>().expect("socket")
        );

        let wildcard = BindAddress::parse("0.0.0.0:50052").expect("wildcard");
        assert!(wildcard.is_wildcard());
        assert_eq!(wildcard.host(), None);
        assert_eq!(wildcard.to_string(), "0.0.0.0:50052");

        let local = BindAddress::parse("localhost:7443").expect("local name");
        let resolved = local.resolve(&resolver).await.expect("resolved");
        assert_eq!(
            resolved,
            "127.0.0.1:7443".parse::<SocketAddr>().expect("socket")
        );
        assert_eq!(
            local
                .host()
                .and_then(|host| host.loopback_warning(Ipv4Addr::LOCALHOST)),
            Some(
                "host name localhost resolves to the loopback address 127.0.0.1 on this \
                 machine, which other machines cannot reach; use its fully qualified name"
                    .to_owned()
            )
        );

        let error = BindAddress::parse("missing.example:7443")
            .expect("named address")
            .resolve(&resolver)
            .await
            .expect_err("failed lookup");
        assert!(matches!(
            error,
            HostResolutionError::Lookup { ref host, .. } if host == "missing.example"
        ));

        for invalid in [
            "manager.example",
            "[::1]:7443",
            "::1:7443",
            "desk_app:7443",
            "a:70000",
        ] {
            assert_eq!(
                BindAddress::parse(invalid).expect_err(invalid).to_string(),
                format!(
                    "{invalid:?} must be an IPv4 address or a DNS host name with a port, for \
                     example 10.0.0.7:7443"
                )
            );
        }
    }

    #[test]
    fn bind_addresses_serialize_as_text() {
        let address = BindAddress::parse("localhost:7443").expect("address");
        let json = serde_json::to_string(&address).expect("serialize");
        assert_eq!(json, "\"localhost:7443\"");
        assert_eq!(
            serde_json::from_str::<BindAddress>(&json).expect("deserialize"),
            address
        );
        for invalid in ["desk_app:1", "LocalHost:7443", "localhost.:7443"] {
            assert!(
                serde_json::from_str::<BindAddress>(&format!("{invalid:?}")).is_err(),
                "{invalid}"
            );
        }
    }
}
