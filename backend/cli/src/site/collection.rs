//! Collection of the values a new site needs, shared by initializing and resetting a site.
//!
//! The values come from the arguments alone, or from guided questions on the terminal for what
//! the arguments leave out. Both ways select the node host by the same rule, looking for this
//! machine's reachable addresses only when nothing else decides it, and end with the same
//! validated in-memory site, ready to persist; nothing is written here.

use std::collections::BTreeSet;

use anyhow::{Context, bail};
use dotcore_domain::SiteNodeHost;
use dotcore_proto::dotcore::site::v1::NativeWorkloadLifetime;
use dotcore_site::initial::{
    probe::InitialSiteProber,
    request::{
        InitialApplicationRuntime, InitialInferenceRuntime, InitialSiteIdentityGenerator,
        InitialSiteRequest,
    },
};

use super::{
    ApplicationsChoice, InferenceArgument, InferenceChoice, InitialSiteConfigurationArgs,
    PreparedInitialSite, SiteLocation, prepare_initial_site,
};
use crate::{
    guided::{
        GuidedInitialSiteCollection, GuidedInitialSiteDefaults, collect_guided_initial_site,
        in_dev_env,
    },
    hosts::{HostCandidate, HostEnvironment, ListenerHost, SelectedHost, select_host},
    terminal::{Message, PromptError, Terminal, TerminalCondition, notify_stderr},
};

/// How init and reset obtain the values the arguments leave out.
pub(super) enum CollectionMode<'a> {
    /// Asks the operator on an interactive terminal.
    Guided { terminal: &'a mut dyn Terminal },

    /// Uses the arguments alone, which name both workload choices.
    Arguments,
}

impl<'a> CollectionMode<'a> {
    /// Selects guided collection when the arguments ask for it or leave a workload choice out,
    /// failing when `terminal` cannot interact; `terminal` is opened only when needed.
    pub(super) fn select(
        arguments: &InitialSiteConfigurationArgs,
        terminal: impl FnOnce() -> &'a mut dyn Terminal,
    ) -> anyhow::Result<Self> {
        if !needs_guided_collection(arguments) {
            return Ok(Self::Arguments);
        }

        let terminal = terminal();
        if terminal.condition() == TerminalCondition::NonInteractive {
            if arguments.interactive {
                bail!("guided interaction requires a terminal; omit --interactive or attach a TTY")
            }
            // The argument contract already rejects missing workload choices without a
            // controlling terminal, so only a terminal lost since then reaches this.
            return Err(PromptError::NotInteractive.into());
        }
        Ok(Self::Guided { terminal })
    }
}

/// This machine's facts that collection reads: its capabilities, its hosts, and the identities
/// and node name it suggests.
pub(super) struct CollectionAdapters<'a> {
    pub(super) prober: &'a InitialSiteProber,
    pub(super) environment: &'a dyn HostEnvironment,
    pub(super) identities: &'a mut dyn InitialSiteIdentityGenerator,
}

/// A collected site ready to persist.
pub(super) struct CollectedSite {
    pub(super) prepared: PreparedInitialSite,

    /// The selected host when no guided summary showed it, so the outcome reports it.
    pub(super) unreported_host: Option<SelectedHost>,
}

/// Collects the values in `mode`, then probes this machine and composes the validated
/// in-memory site. `recorded` is the host an existing site keeps, which replaces discovery. A
/// declined guided confirmation is a cancel.
pub(super) async fn collect_initial_site(
    arguments: &InitialSiteConfigurationArgs,
    location: &SiteLocation,
    listener: Option<&ListenerHost>,
    recorded: Option<SiteNodeHost>,
    mode: CollectionMode<'_>,
    adapters: CollectionAdapters<'_>,
) -> anyhow::Result<CollectedSite> {
    let (host, candidates) = select_host(
        arguments.host.as_ref(),
        listener,
        recorded,
        adapters.environment,
    )
    .await;
    match mode {
        CollectionMode::Guided { terminal } => {
            let defaults = guided_defaults(arguments, location, host, candidates)?;
            let collection = collect_guided_initial_site(
                defaults,
                terminal,
                adapters.environment,
                adapters.prober,
                adapters.identities,
            )
            .await
            .context("guided site configuration")?;
            match collection {
                GuidedInitialSiteCollection::Confirmed(plan) => Ok(CollectedSite {
                    prepared: plan.into_prepared(),
                    unreported_host: None,
                }),
                GuidedInitialSiteCollection::Declined => Err(PromptError::Cancelled.into()),
            }
        }
        CollectionMode::Arguments => {
            let request = request_from_arguments(arguments, host.clone())?;
            let prepared = prepare_initial_site(
                request,
                adapters.prober,
                adapters.environment,
                adapters.identities,
            )
            .await?;
            if let Some(warning) = &prepared.host_warning {
                notify_stderr(
                    &[Message::Warning(warning.clone())],
                    &format!("Warning: {warning}"),
                );
            }
            Ok(CollectedSite {
                prepared,
                unreported_host: Some(host),
            })
        }
    }
}

/// Builds the request from complete arguments with the selected node `host`.
pub(super) fn request_from_arguments(
    arguments: &InitialSiteConfigurationArgs,
    host: SelectedHost,
) -> anyhow::Result<InitialSiteRequest> {
    let inference =
        InferenceChoice::capabilities(&InferenceArgument::choices(&arguments.inference))?
            .context("missing inference choice")?;
    let applications = ApplicationsChoice::capabilities(&arguments.applications)?
        .context("missing applications choice")?;
    InitialSiteChoices {
        inference,
        applications,
        host,
        site_id: arguments.site_id.clone(),
        node_id: arguments.node_id.clone(),
        node_name: arguments.display_name.clone(),
        nvidia_cdi_selectors: (!arguments.nvidia_device.is_empty())
            .then(|| arguments.nvidia_device.clone()),
        native_workload_lifetime: arguments
            .native_workload_lifetime
            .map(|lifetime| lifetime.to_lifetime()),
        simulated_hardware: InferenceArgument::simulated_hardware(&arguments.inference),
    }
    .request()
}

/// Final initial site values, from the arguments or guided collection, that make the request.
/// Unset optional values leave the request's own defaults.
pub(crate) struct InitialSiteChoices {
    pub(crate) inference: BTreeSet<InitialInferenceRuntime>,
    pub(crate) applications: BTreeSet<InitialApplicationRuntime>,
    pub(crate) host: SelectedHost,
    pub(crate) site_id: Option<String>,
    pub(crate) node_id: Option<String>,
    pub(crate) node_name: Option<String>,
    pub(crate) nvidia_cdi_selectors: Option<Vec<String>>,
    pub(crate) native_workload_lifetime: Option<NativeWorkloadLifetime>,
    pub(crate) simulated_hardware: Option<Vec<String>>,
}

impl InitialSiteChoices {
    /// Builds the request; only an operator-named or required host becomes an override.
    pub(crate) fn request(&self) -> anyhow::Result<InitialSiteRequest> {
        let mut request =
            InitialSiteRequest::new(self.inference.clone(), self.applications.clone());
        if let Some(site_id) = &self.site_id {
            request = request.with_site_id_override(site_id);
        }
        if let Some(node_id) = &self.node_id {
            request = request.with_node_id_override(node_id);
        }
        if let Some(name) = &self.node_name {
            request = request.with_site_node_display_name_override(name);
        }
        request = if self.host.is_override() {
            request.with_site_node_host_override(self.host.host().to_string())?
        } else {
            request.with_site_node_host_default(self.host.host().clone())
        };
        if let Some(selectors) = &self.nvidia_cdi_selectors {
            request = request.with_nvidia_cdi_selectors(selectors);
        }
        if let Some(lifetime) = self.native_workload_lifetime {
            request = request.with_native_workload_lifetime(lifetime);
        }
        if let Some(hardware) = &self.simulated_hardware {
            request = request.with_simulated_hardware(hardware.clone());
        }
        Ok(request)
    }
}

/// Returns whether the arguments ask for guided collection or leave a workload choice out.
fn needs_guided_collection(arguments: &InitialSiteConfigurationArgs) -> bool {
    arguments.interactive || arguments.inference.is_empty() || arguments.applications.is_empty()
}

/// Carries every value the arguments supply into guided collection, so it is not asked for,
/// with the selected host and the candidates to offer next to it.
fn guided_defaults(
    arguments: &InitialSiteConfigurationArgs,
    location: &SiteLocation,
    host: SelectedHost,
    candidates: Vec<HostCandidate>,
) -> anyhow::Result<GuidedInitialSiteDefaults> {
    let defaults = GuidedInitialSiteDefaults::new(location.site_dir.clone())
        .with_development(in_dev_env())
        .with_host(host)
        .with_host_candidates(candidates);
    let defaults = match &arguments.display_name {
        Some(name) => defaults.with_node_name(name.clone()),
        None => defaults,
    };
    let defaults =
        match InferenceChoice::capabilities(&InferenceArgument::choices(&arguments.inference))? {
            Some(inference) => defaults.with_inference(inference),
            None => defaults,
        };
    let defaults = match ApplicationsChoice::capabilities(&arguments.applications)? {
        Some(applications) => defaults.with_applications(applications),
        None => defaults,
    };
    let defaults = match &arguments.site_id {
        Some(site_id) => defaults.with_site_id(site_id.clone()),
        None => defaults,
    };
    let defaults = match &arguments.node_id {
        Some(node_id) => defaults.with_node_id(node_id.clone()),
        None => defaults,
    };
    let defaults = if arguments.nvidia_device.is_empty() {
        defaults
    } else {
        defaults.with_nvidia_cdi_selectors(arguments.nvidia_device.clone())
    };
    let defaults = match arguments.native_workload_lifetime {
        Some(lifetime) => defaults.with_native_workload_lifetime(lifetime.to_lifetime()),
        None => defaults,
    };
    Ok(
        match InferenceArgument::simulated_hardware(&arguments.inference) {
            Some(hardware) => defaults.with_simulated_hardware(hardware),
            None => defaults,
        },
    )
}

#[cfg(test)]
mod tests {
    use std::{net::Ipv4Addr, path::Path};

    use dotcore_domain::SiteNodeHost;
    use dotcore_site::initial::probe::InitialSiteProber;

    use super::{CollectedSite, CollectionAdapters, CollectionMode, collect_initial_site};
    use crate::{
        hosts::{
            HostSource, ListenerHost,
            test_support::{FakeHostEnvironment, TAILNET_NAME, listener_host},
        },
        site::{
            ApplicationsChoice, InitialSiteConfigurationArgs,
            test_support::{
                FakeDocker, UnavailableNvidia, init_arguments, location, named_identities,
            },
        },
        terminal::{Message, ScriptedAnswer, ScriptedPrompt, ScriptedTerminal},
    };

    #[test]
    fn the_recommended_tailnet_name_is_the_default_in_both_modes_and_no_override() {
        let environment = FakeHostEnvironment::on_tailnet();
        let mut terminal = guided_terminal(vec![ScriptedAnswer::SelectDefault]);

        let guided = collect(
            &environment,
            &unhosted_arguments(),
            None,
            None,
            Some(&mut terminal),
        )
        .expect("guided site");
        let direct =
            collect(&environment, &unhosted_arguments(), None, None, None).expect("direct site");

        for prepared in [&guided.prepared, &direct.prepared] {
            assert_eq!(prepared.request.site_node_host().to_string(), TAILNET_NAME);
            assert_eq!(prepared.request.site_node_host_override(), None);
        }
        assert!(guided.unreported_host.is_none());
        assert_eq!(
            direct.unreported_host.map(|host| host.source().clone()),
            Some(HostSource::Default(Some("tailnet name".to_owned())))
        );
        let ScriptedPrompt::Select(selector) = &terminal.prompts()[0] else {
            panic!("host selector first: {:?}", terminal.prompts())
        };
        assert_eq!(
            selector
                .items()
                .iter()
                .map(|item| item.label())
                .collect::<Vec<_>>(),
            [
                TAILNET_NAME,
                "100.101.102.103",
                "192.168.1.20",
                "127.0.0.1",
                "Enter another address or host name…",
            ]
        );
    }

    #[test]
    fn picking_the_lan_entry_or_typing_a_host_records_an_override() {
        for (answers, expected) in [
            (vec![ScriptedAnswer::Select(2)], "192.168.1.20"),
            (
                vec![
                    ScriptedAnswer::Select(4),
                    ScriptedAnswer::Input("manager.example.net".to_owned()),
                ],
                "manager.example.net",
            ),
        ] {
            let environment = FakeHostEnvironment::on_tailnet();
            let mut terminal = guided_terminal(answers);

            let collected = collect(
                &environment,
                &unhosted_arguments(),
                None,
                None,
                Some(&mut terminal),
            )
            .expect("guided site");

            let expected = SiteNodeHost::parse(expected).expect("host");
            assert_eq!(collected.prepared.request.site_node_host(), expected);
            assert_eq!(
                collected.prepared.request.site_node_host_override(),
                Some(&expected)
            );
            assert_eq!(terminal.remaining_answers(), 0);
        }
    }

    #[test]
    fn a_flag_listener_or_recorded_host_skips_discovery_and_the_selector_in_both_modes() {
        let flagged = init_arguments(ApplicationsChoice::Simulated).configuration;
        let listener = listener_host("127.0.0.1:6402");
        let recorded = SiteNodeHost::from_ipv4(Ipv4Addr::new(192, 0, 2, 40)).expect("host");

        for (arguments, listener, recorded, expected_host, expected_override, fixed_line) in [
            (&flagged, None, None, "192.0.2.10", true, None),
            (
                &unhosted_arguments(),
                Some(&listener),
                None,
                "127.0.0.1",
                true,
                Some("Node host: 127.0.0.1, from DOTCORE_INFERENCE_ADDR"),
            ),
            (
                &unhosted_arguments(),
                None,
                Some(recorded.clone()),
                "192.0.2.40",
                false,
                Some("Node host: 192.0.2.40, already recorded"),
            ),
        ] {
            let environment = FakeHostEnvironment::on_tailnet();
            let mut terminal = guided_terminal(Vec::new());

            let guided = collect(
                &environment,
                arguments,
                listener,
                recorded.clone(),
                Some(&mut terminal),
            )
            .expect("guided site");
            let direct =
                collect(&environment, arguments, listener, recorded, None).expect("direct site");

            let expected = SiteNodeHost::parse(expected_host).expect("host");
            for prepared in [guided.prepared, direct.prepared] {
                assert_eq!(prepared.request.site_node_host(), expected);
                assert_eq!(
                    prepared.request.site_node_host_override().is_some(),
                    expected_override
                );
            }
            let host_lines: Vec<_> = terminal
                .messages()
                .iter()
                .filter(|message| matches!(message, Message::Info(line) if line.starts_with("Node host")))
                .cloned()
                .collect();
            assert_eq!(
                host_lines,
                fixed_line
                    .map(|line| Message::Info(line.to_owned()))
                    .into_iter()
                    .collect::<Vec<_>>(),
                "{expected_host}"
            );
            assert_eq!(terminal.remaining_answers(), 0);
            assert_eq!(environment.tailnet_reads(), 0, "{expected_host}");
        }
    }

    /// Collects a site on `environment`, guided on `terminal` when given, else from the
    /// arguments, for an existing site that records `recorded` when given.
    fn collect(
        environment: &FakeHostEnvironment,
        arguments: &InitialSiteConfigurationArgs,
        listener: Option<&ListenerHost>,
        recorded: Option<SiteNodeHost>,
        terminal: Option<&mut ScriptedTerminal>,
    ) -> anyhow::Result<CollectedSite> {
        let prober = InitialSiteProber::new(Box::new(FakeDocker), Box::new(UnavailableNvidia));
        let mut identities = named_identities(None);
        let mode = match terminal {
            Some(terminal) => CollectionMode::Guided { terminal },
            None => CollectionMode::Arguments,
        };
        crate::build_runtime()
            .expect("runtime")
            .block_on(collect_initial_site(
                arguments,
                &location(Path::new("/tmp/collected-site")),
                listener,
                recorded,
                mode,
                CollectionAdapters {
                    prober: &prober,
                    environment,
                    identities: &mut identities,
                },
            ))
    }

    /// Complete workload arguments without a host flag.
    fn unhosted_arguments() -> InitialSiteConfigurationArgs {
        let mut arguments = init_arguments(ApplicationsChoice::Simulated).configuration;
        arguments.host = None;
        arguments
    }

    /// A terminal answering `host_answers`, then accepting the node name and the plan.
    fn guided_terminal(host_answers: Vec<ScriptedAnswer>) -> ScriptedTerminal {
        let mut answers = host_answers;
        answers.extend([
            ScriptedAnswer::Input(String::new()),
            ScriptedAnswer::Confirm(true),
        ]);
        ScriptedTerminal::interactive(answers)
    }
}
