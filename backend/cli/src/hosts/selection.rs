//! Decides which node host a new site records, and why.
//!
//! A named host wins, then a host the server listener requires, then one an existing site already
//! records, then the recommended host found on this machine, else loopback. This machine is
//! searched only when nothing before decides. Only a named, chosen or required host counts as
//! operator intent; a default is recorded without making a later re-run with another default
//! conflict.

use std::fmt;

use dotcore_domain::SiteNodeHost;

use super::{HostCandidate, HostEnvironment, ListenerHost, candidates::discover_host_candidates};

/// Selects the node host in precedence order, returning it with this machine's candidates when
/// they were searched, for a guided choice; otherwise the candidates are empty.
pub async fn select_host(
    flag: Option<&SiteNodeHost>,
    listener: Option<&ListenerHost>,
    recorded: Option<SiteNodeHost>,
    environment: &dyn HostEnvironment,
) -> (SelectedHost, Vec<HostCandidate>) {
    if let Some(host) = flag {
        return (SelectedHost::explicit(host.clone()), Vec::new());
    }
    if let Some(listener) = listener {
        let source = HostSource::Listener(listener.variables());
        return (
            SelectedHost::new(listener.host().clone(), source),
            Vec::new(),
        );
    }
    if let Some(host) = recorded {
        return (SelectedHost::new(host, HostSource::Recorded), Vec::new());
    }
    let candidates = discover_host_candidates(environment).await;
    (SelectedHost::recommended(&candidates), candidates)
}

/// The node host a new site records, with where it came from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedHost {
    host: SiteNodeHost,
    source: HostSource,
}

impl SelectedHost {
    /// The first recommendable candidate in rank order, else loopback.
    pub fn recommended(candidates: &[HostCandidate]) -> Self {
        candidates
            .iter()
            .find(|candidate| candidate.is_recommendable())
            .map_or_else(Self::loopback, |candidate| {
                Self::new(
                    candidate.host().clone(),
                    HostSource::Default(Some(candidate.label().to_owned())),
                )
            })
    }

    /// A host the operator named.
    pub fn explicit(host: SiteNodeHost) -> Self {
        Self::new(host, HostSource::Flag)
    }

    /// The loopback default when nothing else applies.
    pub fn loopback() -> Self {
        Self::new(SiteNodeHost::LOOPBACK, HostSource::Default(None))
    }

    /// Returns the operator's pick when this default was offered: the default itself when the
    /// pick is the same host, else the picked host as a choice.
    pub(crate) fn picked(self, host: SiteNodeHost) -> Self {
        if host == self.host {
            self
        } else {
            Self::new(host, HostSource::Chosen)
        }
    }

    /// Returns the selected host.
    pub fn host(&self) -> &SiteNodeHost {
        &self.host
    }

    /// Returns where the host came from.
    pub fn source(&self) -> &HostSource {
        &self.source
    }

    /// Returns whether the host is operator intent, which an existing site must match.
    pub fn is_override(&self) -> bool {
        matches!(
            self.source,
            HostSource::Flag | HostSource::Chosen | HostSource::Listener(_)
        )
    }

    fn new(host: SiteNodeHost, source: HostSource) -> Self {
        Self { host, source }
    }
}

/// Where a selected node host came from; displays as a short operator phrase.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostSource {
    /// The operator named it with the host flag.
    Flag,

    /// The operator picked or typed another host than the offered default in the guided host
    /// step.
    Chosen,

    /// The server listener configuration requires it; carries the names of the settings that do.
    Listener(String),

    /// Already recorded by the existing site that is initialized again.
    Recorded,

    /// The recommended candidate's label, or `None` for the loopback fallback when nothing is
    /// recommendable.
    Default(Option<String>),
}

impl fmt::Display for HostSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Flag => formatter.write_str("from --host"),
            Self::Chosen => formatter.write_str("chosen"),
            Self::Listener(variables) => write!(formatter, "from {variables}"),
            Self::Recorded => formatter.write_str("already recorded"),
            Self::Default(Some(label)) => write!(formatter, "recommended {label}"),
            Self::Default(None) => formatter.write_str("default, no tailnet or LAN address found"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use dotcore_domain::SiteNodeHost;

    use super::{HostSource, SelectedHost, select_host};
    use crate::hosts::test_support::{FakeHostEnvironment, TAILNET_NAME, listener_host};

    #[test]
    fn the_flag_then_the_listener_then_the_recorded_host_decide_before_discovery() {
        let flag = host("192.0.2.10");
        let listener = listener_host("100.64.0.1:6402");
        let recorded = host("192.0.2.40");

        for (flag, listener, recorded, expected) in [
            (
                Some(&flag),
                Some(&listener),
                Some(recorded.clone()),
                ("192.0.2.10", HostSource::Flag),
            ),
            (
                None,
                Some(&listener),
                Some(recorded.clone()),
                (
                    "100.64.0.1",
                    HostSource::Listener("DOTCORE_INFERENCE_ADDR".to_owned()),
                ),
            ),
            (
                None,
                None,
                Some(recorded.clone()),
                ("192.0.2.40", HostSource::Recorded),
            ),
        ] {
            let environment = FakeHostEnvironment::on_tailnet();

            let (selected, candidates) = crate::build_runtime()
                .expect("runtime")
                .block_on(select_host(flag, listener, recorded, &environment));

            assert_eq!(selected.host(), &host(expected.0));
            assert_eq!(selected.source(), &expected.1);
            assert!(candidates.is_empty());
            assert_eq!(environment.tailnet_reads(), 0, "{expected:?}");
        }
    }

    #[tokio::test]
    async fn otherwise_the_recommended_candidate_or_loopback_is_the_default() {
        for (environment, expected_host, expected_source) in [
            (
                FakeHostEnvironment::on_tailnet(),
                SiteNodeHost::parse(TAILNET_NAME).expect("name"),
                HostSource::Default(Some("tailnet name".to_owned())),
            ),
            (
                FakeHostEnvironment::without_tailnet_or_lan(),
                SiteNodeHost::LOOPBACK,
                HostSource::Default(None),
            ),
        ] {
            let (selected, candidates) = select_host(None, None, None, &environment).await;

            assert_eq!(selected.host(), &expected_host);
            assert_eq!(selected.source(), &expected_source);
            assert!(!selected.is_override());
            assert!(!candidates.is_empty());
            assert_eq!(environment.tailnet_reads(), 1);
        }
    }

    #[test]
    fn only_the_flag_a_choice_and_the_listener_are_overrides() {
        let recommended = SelectedHost::new(
            host("100.64.0.7"),
            HostSource::Default(Some("tailnet ip".to_owned())),
        );

        assert_eq!(recommended.clone().picked(host("100.64.0.7")), recommended);
        let chosen = recommended.clone().picked(host("192.0.2.10"));
        assert_eq!(chosen.source(), &HostSource::Chosen);
        for (selected, is_override) in [
            (SelectedHost::explicit(SiteNodeHost::LOOPBACK), true),
            (chosen, true),
            (
                SelectedHost::new(SiteNodeHost::LOOPBACK, HostSource::Listener(String::new())),
                true,
            ),
            (
                SelectedHost::new(host("192.0.2.40"), HostSource::Recorded),
                false,
            ),
            (recommended, false),
            (SelectedHost::loopback(), false),
        ] {
            assert_eq!(selected.is_override(), is_override, "{selected:?}");
        }
    }

    #[test]
    fn each_source_reads_as_a_short_phrase() {
        assert_eq!(
            [
                HostSource::Flag,
                HostSource::Chosen,
                HostSource::Listener("DOTCORE_INFERENCE_ADDR".to_owned()),
                HostSource::Recorded,
                HostSource::Default(Some("address on enp5s0".to_owned())),
                HostSource::Default(None),
            ]
            .map(|source| source.to_string()),
            [
                "from --host",
                "chosen",
                "from DOTCORE_INFERENCE_ADDR",
                "already recorded",
                "recommended address on enp5s0",
                "default, no tailnet or LAN address found",
            ]
        );
    }

    fn host(value: &str) -> SiteNodeHost {
        SiteNodeHost::from_ipv4(value.parse::<Ipv4Addr>().expect("IPv4 address"))
            .expect("specific address")
    }
}
