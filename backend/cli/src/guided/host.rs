//! Node host step of guided site initialization and reset.
//!
//! Offers this machine's ranked host candidates with the recommended one
//! preselected, and accepts any other specific address or DNS name the operator
//! types. Accepting the default records no operator intent. When the listener
//! configuration or the existing site already fixes the host, the step shows it
//! without offering a choice.

use dotcore_domain::SiteNodeHost;

use crate::{
    hosts::{HostCandidate, HostSource, SelectedHost},
    terminal::{InputPrompt, Message, SelectItem, SelectPrompt, Terminal},
};

/// Returns the selected host when the operator named it, the listener variables require it or
/// the existing site records it, else asks the operator to choose among `candidates` with the
/// selected default offered first.
pub(super) fn collect_host(
    selected: SelectedHost,
    candidates: &[HostCandidate],
    terminal: &mut dyn Terminal,
) -> anyhow::Result<SelectedHost> {
    let reason = match selected.source() {
        HostSource::Flag | HostSource::Chosen => return Ok(selected),
        HostSource::Listener(_) | HostSource::Recorded => {
            terminal.message(&Message::Info(format!(
                "Node host: {}, {}",
                selected.host(),
                selected.source()
            )))?;
            return Ok(selected);
        }
        HostSource::Default(Some(label)) => format!("Recommended: {label}."),
        HostSource::Default(None) => "No tailnet or LAN address found.".to_owned(),
    };

    let (prompt, choices) = host_selector(candidates, &selected);
    terminal.message(&Message::Hint(
        "Other nodes reach this node at this host.".to_owned(),
    ))?;
    terminal.message(&Message::Hint(reason))?;
    let host = match &choices[terminal.select(&prompt)?] {
        HostChoice::Listed(host) => host.clone(),
        HostChoice::Other => collect_other_host(terminal)?,
    };
    Ok(selected.picked(host))
}

/// One entry of the host selector.
#[derive(Clone, Debug, Eq, PartialEq)]
enum HostChoice {
    /// A listed host.
    Listed(SiteNodeHost),

    /// Typing a host that is not listed.
    Other,
}

/// Builds the selector and its choices in display order: the offered default first, then the
/// other candidates in rank order, then the entry for another host.
fn host_selector(
    candidates: &[HostCandidate],
    default: &SelectedHost,
) -> (SelectPrompt, Vec<HostChoice>) {
    let default_note = match default.source() {
        HostSource::Default(Some(_)) => "recommended",
        _ => "default",
    };
    let default_label = candidates
        .iter()
        .find(|candidate| candidate.host() == default.host())
        .map_or("loopback", HostCandidate::label);

    let mut items = vec![
        SelectItem::new(default.host().to_string())
            .with_description(format!("{default_label}, {default_note}")),
    ];
    let mut choices = vec![HostChoice::Listed(default.host().clone())];
    for candidate in candidates
        .iter()
        .filter(|candidate| candidate.host() != default.host())
    {
        items.push(
            SelectItem::new(candidate.host().to_string()).with_description(candidate.label()),
        );
        choices.push(HostChoice::Listed(candidate.host().clone()));
    }
    items.push(SelectItem::new("Enter another address or host name…"));
    choices.push(HostChoice::Other);

    (SelectPrompt::new("Node host", items), choices)
}

/// Asks for a host until it is a specific IPv4 address or a DNS host name, recorded lowercase
/// without a trailing dot.
fn collect_other_host(terminal: &mut dyn Terminal) -> anyhow::Result<SiteNodeHost> {
    loop {
        let value = terminal.input(&InputPrompt::new("Node host address or name"))?;
        let value = value.trim();
        match SiteNodeHost::normalize(value) {
            Ok(host) => return Ok(host),
            Err(error) => {
                terminal.message(&Message::Error(format!("Node host {value:?} {error}")))?
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use dotcore_domain::SiteNodeHost;

    use super::collect_host;
    use crate::{
        hosts::{
            HostCandidate, HostSource, SelectedHost, select_host,
            test_support::{FakeHostEnvironment, listener_host_for},
        },
        terminal::{
            InputPrompt, Message, ScriptedAnswer, ScriptedPrompt, ScriptedTerminal, SelectItem,
            SelectPrompt,
        },
    };

    const NAME: &str = "gpu1.example.ts.net";

    #[test]
    fn selector_lists_the_recommended_candidate_first_then_the_others_then_another_host() {
        let candidates = tailnet_candidates();
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::SelectDefault]);

        let selected = choose(&candidates, &mut terminal);

        assert_eq!(selected, SelectedHost::recommended(&candidates));
        assert_eq!(selected.host().to_string(), NAME);
        assert_eq!(
            terminal.prompts(),
            [ScriptedPrompt::Select(SelectPrompt::new(
                "Node host",
                [
                    SelectItem::new(NAME).with_description("tailnet name, recommended"),
                    SelectItem::new("100.101.102.103").with_description("tailnet ip"),
                    SelectItem::new("192.168.1.20").with_description("address on enp5s0"),
                    SelectItem::new("172.17.0.1").with_description("virtual"),
                    SelectItem::new("127.0.0.1").with_description("loopback"),
                    SelectItem::new("Enter another address or host name…"),
                ],
            ))]
        );
        assert_eq!(
            terminal.messages(),
            [
                Message::Hint("Other nodes reach this node at this host.".to_owned()),
                Message::Hint("Recommended: tailnet name.".to_owned()),
            ]
        );
    }

    #[test]
    fn without_a_tailnet_or_lan_candidate_loopback_is_preselected_with_its_label() {
        for (candidates, rows) in [
            (
                vec![
                    candidate("172.17.0.1", "virtual", false),
                    candidate("127.0.0.1", "loopback", false),
                    candidate("169.254.3.4", "link-local", false),
                ],
                vec![
                    "127.0.0.1 loopback, default",
                    "172.17.0.1 virtual",
                    "169.254.3.4 link-local",
                    "Enter another address or host name…",
                ],
            ),
            (
                Vec::new(),
                vec![
                    "127.0.0.1 loopback, default",
                    "Enter another address or host name…",
                ],
            ),
        ] {
            let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::SelectDefault]);

            let selected = choose(&candidates, &mut terminal);

            assert_eq!(selected, SelectedHost::loopback());
            assert_eq!(select_rows(&terminal), rows);
            assert_eq!(
                terminal.messages()[1],
                Message::Hint("No tailnet or LAN address found.".to_owned())
            );
        }
    }

    #[test]
    fn only_a_pick_other_than_the_offered_default_is_a_choice() {
        let candidates = tailnet_candidates();

        for (answer, expected) in [
            (
                ScriptedAnswer::SelectDefault,
                SelectedHost::recommended(&candidates),
            ),
            (
                ScriptedAnswer::Select(2),
                SelectedHost::recommended(&candidates).picked(ipv4_host("192.168.1.20")),
            ),
        ] {
            let mut terminal = ScriptedTerminal::interactive([answer]);

            let selected = choose(&candidates, &mut terminal);

            assert_eq!(selected, expected);
            assert_eq!(terminal.prompts().len(), 1);
            assert_eq!(terminal.remaining_answers(), 0);
        }
        let picked = SelectedHost::recommended(&candidates).picked(ipv4_host("192.168.1.20"));
        assert_eq!(picked.source(), &HostSource::Chosen);
        assert!(picked.is_override());

        let mut terminal = ScriptedTerminal::interactive([
            ScriptedAnswer::Select(5),
            ScriptedAnswer::Input(format!(" {} ", NAME.to_uppercase())),
        ]);
        let typed_default = choose(&candidates, &mut terminal);
        assert_eq!(typed_default, SelectedHost::recommended(&candidates));
        assert!(!typed_default.is_override());
    }

    #[test]
    fn another_host_rejects_malformed_and_wildcard_values_until_specific() {
        let candidates = [candidate("192.0.2.10", "address on en0", true)];
        let mut terminal = ScriptedTerminal::interactive([
            ScriptedAnswer::Select(1),
            ScriptedAnswer::Input("not_a_host".to_owned()),
            ScriptedAnswer::Input("0.0.0.0".to_owned()),
            ScriptedAnswer::Input("198.51.100.7".to_owned()),
        ]);

        let selected = choose(&candidates, &mut terminal);

        assert_eq!(selected.host(), &ipv4_host("198.51.100.7"));
        assert_eq!(selected.source(), &HostSource::Chosen);
        assert_eq!(
            terminal.prompts()[1..],
            vec![ScriptedPrompt::Input(InputPrompt::new("Node host address or name")); 3]
        );
        assert_eq!(
            terminal.messages()[2..],
            [
                Message::Error(
                    "Node host \"not_a_host\" must be an IPv4 address or a DNS host name"
                        .to_owned()
                ),
                Message::Error(
                    "Node host \"0.0.0.0\" must be a specific non-wildcard IPv4 address or \
                     a DNS host name"
                        .to_owned()
                ),
            ]
        );
    }

    #[test]
    fn listener_host_is_shown_as_fixed_without_a_selector() {
        let fixed = listener_host_for(&[
            ("DOTCORE_MANAGEMENT_ADDR", "127.0.0.1:6400"),
            ("DOTCORE_INFERENCE_ADDR", "127.0.0.1:6402"),
        ]);
        let mut terminal = ScriptedTerminal::interactive([]);
        let (locked, _) = crate::build_runtime()
            .expect("runtime")
            .block_on(select_host(
                None,
                Some(&fixed),
                None,
                &FakeHostEnvironment::default(),
            ));

        let selected = collect_host(locked, &[], &mut terminal).expect("fixed host");

        assert_eq!(selected.host(), &SiteNodeHost::LOOPBACK);
        assert!(terminal.prompts().is_empty());
        assert_eq!(
            terminal.messages(),
            [Message::Info(
                "Node host: 127.0.0.1, from DOTCORE_MANAGEMENT_ADDR, DOTCORE_INFERENCE_ADDR"
                    .to_owned()
            )]
        );
    }

    /// Runs the host step with the recommended host among `candidates` offered.
    fn choose(candidates: &[HostCandidate], terminal: &mut ScriptedTerminal) -> SelectedHost {
        collect_host(SelectedHost::recommended(candidates), candidates, terminal).expect("host")
    }

    /// A tailnet machine's candidates in rank order, as discovery lists them.
    fn tailnet_candidates() -> Vec<HostCandidate> {
        vec![
            HostCandidate::new(
                SiteNodeHost::parse(NAME).expect("name"),
                "tailnet name",
                true,
            ),
            candidate("100.101.102.103", "tailnet ip", true),
            candidate("192.168.1.20", "address on enp5s0", true),
            candidate("172.17.0.1", "virtual", false),
            candidate("127.0.0.1", "loopback", false),
        ]
    }

    fn candidate(address: &str, label: &str, recommendable: bool) -> HostCandidate {
        HostCandidate::new(ipv4_host(address), label, recommendable)
    }

    fn ipv4_host(address: &str) -> SiteNodeHost {
        SiteNodeHost::from_ipv4(address.parse::<Ipv4Addr>().expect("IPv4 address"))
            .expect("specific address")
    }

    /// Renders each row of the only select prompt as `<label> <description>`.
    fn select_rows(terminal: &ScriptedTerminal) -> Vec<String> {
        let [ScriptedPrompt::Select(prompt)] = terminal.prompts() else {
            panic!("expected one select prompt: {:?}", terminal.prompts())
        };
        assert_eq!(prompt.default(), 0);
        prompt
            .items()
            .iter()
            .map(|item| match item.description() {
                Some(description) => format!("{} {description}", item.label()),
                None => item.label().to_owned(),
            })
            .collect()
    }
}
