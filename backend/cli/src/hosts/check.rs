//! Checks a node host against this machine before a site records it.
//!
//! Warns when the host is reachable only from this machine, or when the listeners that default to
//! the node host cannot bind it unless they are configured separately, so the operator learns
//! before anything is written. A host name that does not resolve here, or interfaces that cannot be
//! read, give no warning.

use dotcore::composition::{
    DEFAULT_INFERENCE_PORT, DEFAULT_PRIVATE_PORT, INFERENCE_ADDR_ENV, PRIVATE_ADDR_ENV,
};
use dotcore_domain::SiteNodeHost;

use super::{HostEnvironment, interfaces::classify};

/// Describes what breaks with `host` on this machine: only this machine reaches a loopback
/// host, and the console, inference and private listeners cannot bind a host that is not an
/// address of this machine unless their addresses are configured. `None` when neither applies or
/// the host cannot be checked here.
pub async fn host_warning(
    host: &SiteNodeHost,
    environment: &dyn HostEnvironment,
) -> Option<String> {
    let resolved = host
        .resolve(environment.resolver())
        .await
        .inspect_err(|error| tracing::debug!(%host, %error, "node host is not checked"))
        .ok()?;

    if resolved.is_loopback() {
        return Some(format!(
            "only this machine can reach node host {host}; workers and remote apps need another \
             host"
        ));
    }

    // A listener variable that cannot be read counts as set, since the server then fails on it
    // rather than on the host. The inference address also places the packaged console.
    let inference_set = !matches!(environment.variable(INFERENCE_ADDR_ENV), Ok(None));
    let private_set = !matches!(environment.variable(PRIVATE_ADDR_ENV), Ok(None));
    if inference_set && private_set {
        return None;
    }
    let interfaces = environment
        .interface_addresses()
        .inspect_err(|error| tracing::debug!(%error, "local interface addresses unavailable"))
        .ok()?;
    let local = classify(interfaces)
        .iter()
        .any(|interface| interface.address() == resolved);
    (!local).then(|| not_local_warning(host, inference_set, private_set))
}

/// Describes how to start the manager with its private listener on `address`, offering both
/// ways the server and its launcher accept that setting.
pub fn private_address_remedy(address: &str) -> String {
    format!("{PRIVATE_ADDR_ENV}={address} (or dotcore --private-addr {address})")
}

/// Describes how to start the manager with its inference listener, and the packaged console with
/// it, on `address`.
pub fn inference_address_remedy(address: &str) -> String {
    format!("{INFERENCE_ADDR_ENV}={address}")
}

/// Describes which listeners left at their node host default cannot bind `host`, and the
/// settings that move them to every interface.
fn not_local_warning(host: &SiteNodeHost, inference_set: bool, private_set: bool) -> String {
    let inference_remedy = inference_address_remedy(&format!("0.0.0.0:{DEFAULT_INFERENCE_PORT}"));
    let private_remedy = private_address_remedy(&format!("0.0.0.0:{DEFAULT_PRIVATE_PORT}"));
    let (failure, remedy) = match (inference_set, private_set) {
        (false, false) => (
            "the console and inference cannot start on this host, and the private listener \
             cannot once nodes join"
                .to_owned(),
            format!("{inference_remedy} and {private_remedy}"),
        ),
        (false, true) => (
            "the console and inference cannot start on this host".to_owned(),
            inference_remedy,
        ),
        _ => (
            "server start will fail to bind the private listener on this host once nodes join"
                .to_owned(),
            private_remedy,
        ),
    };
    format!(
        "node host {host} is not an address of this machine: {failure}; use an address of this \
         machine, or start the manager with {remedy}"
    )
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use dotcore_domain::SiteNodeHost;

    use super::host_warning;
    use crate::hosts::test_support::FakeHostEnvironment;

    const LAN_IPV4: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 20);

    #[tokio::test]
    async fn a_loopback_address_or_a_name_resolving_to_loopback_warns_alike() {
        let environment = FakeHostEnvironment::default()
            .with_interface("lo", Ipv4Addr::LOCALHOST)
            .with_name("manager", [Ipv4Addr::new(127, 0, 0, 2)]);

        for host in ["127.0.0.1", "manager"] {
            assert_eq!(
                warning(&environment, host).await,
                Some(format!(
                    "only this machine can reach node host {host}; workers and remote apps need \
                     another host"
                ))
            );
        }
    }

    #[tokio::test]
    async fn a_host_whose_first_address_is_not_local_warns_that_no_listener_can_bind() {
        let elsewhere = Ipv4Addr::new(192, 0, 2, 7);
        let environment = FakeHostEnvironment::default()
            .with_interface("enp5s0", LAN_IPV4)
            .with_name("first-elsewhere.example", [elsewhere, LAN_IPV4])
            .with_name("first-local.example", [LAN_IPV4, elsewhere]);

        for host in ["192.0.2.10", "first-elsewhere.example"] {
            assert_eq!(
                warning(&environment, host).await,
                Some(format!(
                    "node host {host} is not an address of this machine: the console and \
                     inference cannot start on this host, and the private listener cannot once \
                     nodes join; use an address of this machine, or start the manager with \
                     DOTCORE_INFERENCE_ADDR=0.0.0.0:50052 and DOTCORE_PRIVATE_ADDR=0.0.0.0:50053 \
                     (or dotcore --private-addr 0.0.0.0:50053)"
                )),
                "{host}"
            );
        }
        for host in ["192.168.1.20", "first-local.example", "unknown.example"] {
            assert_eq!(warning(&environment, host).await, None, "{host}");
        }
    }

    #[tokio::test]
    async fn a_configured_listener_address_drops_its_part_of_the_bind_warning() {
        let inference_only = FakeHostEnvironment::default()
            .with_interface("enp5s0", LAN_IPV4)
            .with_variable("DOTCORE_INFERENCE_ADDR", "0.0.0.0:50052");
        let private_only = FakeHostEnvironment::default()
            .with_interface("enp5s0", LAN_IPV4)
            .with_variable("DOTCORE_PRIVATE_ADDR", "0.0.0.0:50053");

        assert_eq!(
            warning(&inference_only, "192.0.2.10").await,
            Some(
                "node host 192.0.2.10 is not an address of this machine: server start will fail \
                 to bind the private listener on this host once nodes join; use an address of \
                 this machine, or start the manager with DOTCORE_PRIVATE_ADDR=0.0.0.0:50053 (or \
                 dotcore --private-addr 0.0.0.0:50053)"
                    .to_owned()
            )
        );
        assert_eq!(
            warning(&private_only, "192.0.2.10").await,
            Some(
                "node host 192.0.2.10 is not an address of this machine: the console and \
                 inference cannot start on this host; use an address of this machine, or start \
                 the manager with DOTCORE_INFERENCE_ADDR=0.0.0.0:50052"
                    .to_owned()
            )
        );
    }

    #[tokio::test]
    async fn configured_inference_and_private_addresses_skip_the_bind_warning_but_not_loopback() {
        let environment = FakeHostEnvironment::default()
            .with_interface("enp5s0", LAN_IPV4)
            .with_variable("DOTCORE_INFERENCE_ADDR", "0.0.0.0:50052")
            .with_variable("DOTCORE_PRIVATE_ADDR", "0.0.0.0:50053");

        assert_eq!(warning(&environment, "192.0.2.10").await, None);
        assert!(warning(&environment, "127.0.0.1").await.is_some());
    }

    async fn warning(environment: &FakeHostEnvironment, host: &str) -> Option<String> {
        host_warning(&SiteNodeHost::parse(host).expect("host"), environment).await
    }
}
