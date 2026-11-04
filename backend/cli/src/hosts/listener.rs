//! The node host the server's listener configuration requires of a new site.
//!
//! Reads the listener settings the server reads, so the site never publishes a host the server
//! does not listen on. Unset and wildcard listeners leave the host free; specific ones must agree
//! on one address, which then becomes the only accepted host.

use std::net::Ipv4Addr;

use anyhow::{Context, bail};
use dotcore_backend::composition::{INFERENCE_ADDR_ENV, MANAGEMENT_ADDR_ENV, PRIVATE_ADDR_ENV};
use dotcore_domain::{BindAddress, HostResolver, SiteNodeHost};

use super::HostEnvironment;

/// The node host the server's specific listener addresses require.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListenerHost {
    /// Host as the first specific listener setting names it, an address or a host name.
    host: SiteNodeHost,

    /// IPv4 address the listeners bind.
    address: Ipv4Addr,

    /// Listener settings naming a specific address, in server reading order.
    variables: Vec<&'static str>,
}

impl ListenerHost {
    /// Returns the host the listener settings require, or `None` when they leave it free, after
    /// checking that an explicitly requested host is that host or a name resolving to its address.
    pub async fn constrain(
        requested: Option<&SiteNodeHost>,
        environment: &dyn HostEnvironment,
    ) -> anyhow::Result<Option<Self>> {
        let Some(listener) = Self::from_environment(environment).await? else {
            return Ok(None);
        };
        if let Some(host) = requested {
            listener.check(host, environment.resolver()).await?;
        }
        Ok(Some(listener))
    }

    /// Returns the required host.
    pub fn host(&self) -> &SiteNodeHost {
        &self.host
    }

    /// Names the listener settings that require the host, comma separated.
    pub fn variables(&self) -> String {
        self.variables.join(", ")
    }

    async fn from_environment(environment: &dyn HostEnvironment) -> anyhow::Result<Option<Self>> {
        let mut listeners = Vec::new();
        for variable in [MANAGEMENT_ADDR_ENV, INFERENCE_ADDR_ENV, PRIVATE_ADDR_ENV] {
            let Some(value) = environment.variable(variable)? else {
                continue;
            };
            let bind_address = value
                .parse::<BindAddress>()
                .with_context(|| format!("invalid {variable} value {value:?}"))?;
            let Some(host) = bind_address.host() else {
                continue;
            };
            let address = host
                .resolve(environment.resolver())
                .await
                .with_context(|| format!("resolve the {variable} host {host}"))?;
            listeners.push(SpecificListener {
                variable,
                value,
                host: host.clone(),
                address,
            });
        }

        let Some(first) = listeners.first() else {
            return Ok(None);
        };
        if listeners
            .iter()
            .any(|listener| listener.address != first.address)
        {
            let settings = listeners
                .iter()
                .map(|listener| format!("{}={}", listener.variable, listener.value))
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "the server listener variables name different addresses ({settings}); set them \
                 to one address, or to 0.0.0.0, before initializing the site"
            );
        }
        Ok(Some(Self {
            host: first.host.clone(),
            address: first.address,
            variables: listeners.iter().map(|listener| listener.variable).collect(),
        }))
    }

    async fn check(&self, host: &SiteNodeHost, resolver: &dyn HostResolver) -> anyhow::Result<()> {
        if *host == self.host {
            return Ok(());
        }
        let addresses = host.resolve_all(resolver).await.with_context(|| {
            format!(
                "resolve --host {host} to compare it with the server listener address {}",
                self.host
            )
        })?;
        if addresses.contains(&self.address) {
            return Ok(());
        }
        let change = if self.variables.len() == 1 {
            "that variable"
        } else {
            "those variables"
        };
        bail!(
            "the server listens on {} ({}); use --host {} or change {change}",
            self.host,
            self.variables(),
            self.host
        )
    }
}

/// One listener variable naming a specific address.
struct SpecificListener {
    variable: &'static str,
    value: String,
    host: SiteNodeHost,
    address: Ipv4Addr,
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use dotcore_domain::SiteNodeHost;

    use super::ListenerHost;
    use crate::hosts::test_support::FakeHostEnvironment;

    #[tokio::test]
    async fn unset_and_wildcard_listeners_leave_any_host_free() {
        let requested = host("100.64.0.1");
        for environment in [
            vec![],
            vec![
                ("DOTCORE_MANAGEMENT_ADDR", "0.0.0.0:6202"),
                ("DOTCORE_INFERENCE_ADDR", "0.0.0.0:6402"),
            ],
        ] {
            assert_eq!(
                constrain(Some(&requested), &environment, &[])
                    .await
                    .expect("free host"),
                None
            );
        }
    }

    #[tokio::test]
    async fn specific_listeners_require_their_address() {
        let environment = loopback_listeners();

        let listener = constrain(None, &environment, &[])
            .await
            .expect("constraint")
            .expect("required host");

        assert_eq!(listener.host(), &SiteNodeHost::LOOPBACK);
        assert_eq!(
            listener.variables(),
            "DOTCORE_MANAGEMENT_ADDR, DOTCORE_INFERENCE_ADDR, DOTCORE_PRIVATE_ADDR"
        );
        assert_eq!(
            constrain(Some(&host("127.0.0.1")), &environment, &[])
                .await
                .expect("matching host"),
            Some(listener)
        );
    }

    #[tokio::test]
    async fn another_requested_address_is_rejected_naming_the_variables_and_the_fix() {
        let error = constrain(Some(&host("100.64.0.1")), &loopback_listeners(), &[])
            .await
            .expect_err("mismatched host");

        assert_eq!(
            format!("{error:#}"),
            "the server listens on 127.0.0.1 (DOTCORE_MANAGEMENT_ADDR, DOTCORE_INFERENCE_ADDR, \
             DOTCORE_PRIVATE_ADDR); use --host 127.0.0.1 or change those variables"
        );
    }

    #[tokio::test]
    async fn a_wildcard_listener_does_not_relax_a_specific_one() {
        let environment = vec![
            ("DOTCORE_MANAGEMENT_ADDR", "0.0.0.0:6202"),
            ("DOTCORE_INFERENCE_ADDR", "127.0.0.1:6402"),
        ];

        let error = constrain(Some(&host("100.64.0.1")), &environment, &[])
            .await
            .expect_err("mismatched host");

        assert_eq!(
            format!("{error:#}"),
            "the server listens on 127.0.0.1 (DOTCORE_INFERENCE_ADDR); use --host 127.0.0.1 or \
             change that variable"
        );
    }

    #[tokio::test]
    async fn listeners_naming_different_addresses_are_rejected() {
        let environment = vec![
            ("DOTCORE_MANAGEMENT_ADDR", "127.0.0.1:6202"),
            ("DOTCORE_INFERENCE_ADDR", "100.64.0.1:6402"),
        ];

        let error = constrain(None, &environment, &[])
            .await
            .expect_err("disagreeing listeners");

        assert_eq!(
            format!("{error:#}"),
            "the server listener variables name different addresses \
             (DOTCORE_MANAGEMENT_ADDR=127.0.0.1:6202, DOTCORE_INFERENCE_ADDR=100.64.0.1:6402); \
             set them to one address, or to 0.0.0.0, before initializing the site"
        );
    }

    #[tokio::test]
    async fn host_names_compare_by_the_address_they_resolve_to() {
        let names = [
            ("manager.example.ts.net", Ipv4Addr::new(100, 64, 0, 1)),
            ("other.example.ts.net", Ipv4Addr::new(100, 64, 0, 2)),
        ];
        let environment = vec![
            ("DOTCORE_MANAGEMENT_ADDR", "manager.example.ts.net:6202"),
            ("DOTCORE_INFERENCE_ADDR", "100.64.0.1:6402"),
        ];

        let listener = constrain(Some(&host("100.64.0.1")), &environment, &names)
            .await
            .expect("address of the listener name")
            .expect("required host");
        assert_eq!(listener.host(), &host("manager.example.ts.net"));

        let error = constrain(Some(&host("other.example.ts.net")), &environment, &names)
            .await
            .expect_err("name resolving elsewhere");
        assert_eq!(
            format!("{error:#}"),
            "the server listens on manager.example.ts.net (DOTCORE_MANAGEMENT_ADDR, \
             DOTCORE_INFERENCE_ADDR); use --host manager.example.ts.net or change those variables"
        );
    }

    #[tokio::test]
    async fn a_requested_name_resolving_to_the_listener_address_passes() {
        let names = [("localhost", Ipv4Addr::LOCALHOST)];

        let listener = constrain(Some(&host("localhost")), &loopback_listeners(), &names)
            .await
            .expect("name resolving to the listener address")
            .expect("required host");
        assert_eq!(listener.host(), &SiteNodeHost::LOOPBACK);

        let error = constrain(
            Some(&host("unknown.example")),
            &loopback_listeners(),
            &names,
        )
        .await
        .expect_err("unresolvable name");
        assert_eq!(
            format!("{error:#}"),
            "resolve --host unknown.example to compare it with the server listener address \
             127.0.0.1: could not resolve host name unknown.example; check that the network \
             providing it is up: unknown.example is unknown"
        );
    }

    #[tokio::test]
    async fn a_malformed_listener_variable_fails_like_the_server() {
        let environment = vec![("DOTCORE_INFERENCE_ADDR", "not-an-address")];

        let error = constrain(None, &environment, &[])
            .await
            .expect_err("malformed listener");

        assert_eq!(
            format!("{error:#}"),
            "invalid DOTCORE_INFERENCE_ADDR value \"not-an-address\": \"not-an-address\" must be \
             an IPv4 address or a DNS host name with a port, for example 10.0.0.7:7443"
        );
    }

    async fn constrain(
        requested: Option<&SiteNodeHost>,
        variables: &[(&'static str, &'static str)],
        names: &[(&str, Ipv4Addr)],
    ) -> anyhow::Result<Option<ListenerHost>> {
        let environment = variables.iter().fold(
            names.iter().fold(
                FakeHostEnvironment::default(),
                |environment, (name, address)| environment.with_name(name, [*address]),
            ),
            |environment, (variable, value)| environment.with_variable(variable, value),
        );
        ListenerHost::constrain(requested, &environment).await
    }

    fn loopback_listeners() -> Vec<(&'static str, &'static str)> {
        vec![
            ("DOTCORE_MANAGEMENT_ADDR", "127.0.0.1:6202"),
            ("DOTCORE_INFERENCE_ADDR", "127.0.0.1:6402"),
            ("DOTCORE_PRIVATE_ADDR", "127.0.0.1:6602"),
        ]
    }

    fn host(value: &str) -> SiteNodeHost {
        SiteNodeHost::parse(value).expect("host")
    }
}
