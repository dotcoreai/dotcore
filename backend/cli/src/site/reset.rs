//! Reset of a site to a fresh one.
//!
//! The site is locked against a running server for the whole reset. The operator first approves
//! removing the runtime resources the site owns, then the new inputs are collected the same way
//! initialization collects them, and only then are the site's resources, state and trust removed
//! and the new inputs written in their place. Declining or missing approval stops the reset before
//! the new inputs are asked for, and changes nothing.

use std::path::Path;

use anyhow::Context;
use dotcore_runtime::SiteRuntimeCleanup;
use dotcore_site::{
    initial::{
        input_store::InitialSiteInputWriter, inputs::InitialSiteInputs, request::InitialSiteRequest,
    },
    reset::{
        persistence::SitePersistenceReset,
        transaction::{
            SiteResetLockError, SiteResetSession, SiteResetTransaction, SiteResetTransactionError,
        },
    },
};

use super::{
    HostAwareIdentityGenerator, SiteLocation, SiteResetArgs,
    collection::{CollectionAdapters, CollectionMode, collect_initial_site},
    output::{report_host, write_success},
    system_initial_site_prober, system_listener_host, validator,
};
use crate::{
    RootPathArgs,
    hosts::{ListenerHost, SystemHostEnvironment},
    runtime_confirmation::{
        RuntimeRemovalApproval, approve_runtime_removal, site_runtime_cleanup,
        site_runtime_cleanups,
    },
    terminal::{Message, NextStep, Terminal, notify_stderr, open_terminal},
};

/// Resets the site the root path options name to a fresh one.
pub(super) async fn reset(arguments: SiteResetArgs, paths: &RootPathArgs) -> anyhow::Result<()> {
    let location = SiteLocation::resolve(paths)?;
    let cache_root = paths.configured_cache_root()?;
    let listener = system_listener_host(&arguments.configuration).await?;
    let prober = system_initial_site_prober();
    let mut terminal = open_terminal();
    reset_site(
        arguments,
        &location,
        cache_root.as_deref(),
        listener.as_ref(),
        terminal.as_mut(),
        CollectionAdapters {
            prober: &prober,
            environment: &SystemHostEnvironment,
            identities: &mut HostAwareIdentityGenerator::new(),
        },
        site_runtime_cleanup,
    )
    .await
}

/// Locks the site and gets approval to remove its runtime resources, then collects its new inputs
/// and removes and republishes it.
async fn reset_site(
    arguments: SiteResetArgs,
    location: &SiteLocation,
    cache_root: Option<&Path>,
    listener: Option<&ListenerHost>,
    terminal: &mut dyn Terminal,
    adapters: CollectionAdapters<'_>,
    select_runtime: impl FnOnce(&SiteResetSession) -> anyhow::Result<Box<dyn SiteRuntimeCleanup>>,
) -> anyhow::Result<()> {
    let site_dir = location.site_dir.as_path();
    let skip_runtime_cleanup = arguments.removal.skip_runtime_cleanup;
    let cleanups = site_runtime_cleanups(cache_root);
    let session = if skip_runtime_cleanup {
        SiteResetSession::begin_unidentified_site(site_dir, cleanups)
    } else {
        SiteResetSession::begin(site_dir, cleanups)
    }
    .map_err(|error| reset_transaction_failure(error, site_dir))?;
    let approval = RuntimeRemovalApproval {
        subject: format!("Site at {}", site_dir.display()),
        verb: "reset",
        question: "Remove them and reset the site?",
        yes: arguments.removal.yes,
        non_interactive: arguments.configuration.non_interactive,
        skip_runtime_cleanup,
    };
    let transaction = approve_removal(
        &session,
        select_runtime,
        terminal,
        &approval,
        location.catalog_dir.as_deref(),
    )
    .await?;

    let mode = CollectionMode::select(&arguments.configuration, || &mut *terminal)?;
    let collected = collect_initial_site(
        &arguments.configuration,
        location,
        listener,
        None,
        mode,
        adapters,
    )
    .await?;
    remove_and_republish(
        session,
        transaction,
        &collected.prepared.inputs,
        &collected.prepared.request,
    )
    .await?;
    if let Some(host) = &collected.unreported_host {
        report_host(host);
    }
    Ok(())
}

/// Lists the site's runtime resources and obtains the operator's approval to remove them. Nothing
/// changes, whether the operator declines or approval is missing.
async fn approve_removal(
    session: &SiteResetSession,
    select_runtime: impl FnOnce(&SiteResetSession) -> anyhow::Result<Box<dyn SiteRuntimeCleanup>>,
    terminal: &mut dyn Terminal,
    approval: &RuntimeRemovalApproval<'_>,
    catalog_dir: Option<&Path>,
) -> anyhow::Result<SiteResetTransaction> {
    let site_dir = session.site_root();
    let persistence = SitePersistenceReset::new(session.cleanups());
    let transaction = SiteResetTransaction::new(
        select_runtime(session)?,
        Box::new(persistence),
        InitialSiteInputWriter::new(validator(catalog_dir)),
    );

    let inventory = transaction
        .inventory(session)
        .await
        .map_err(|error| reset_transaction_failure(error, site_dir))?;
    approve_runtime_removal(terminal, site_dir, &inventory, approval)?;
    Ok(transaction)
}

/// Cleans, wipes and republishes the site with its new inputs, then warns when joined nodes lost
/// their trust.
async fn remove_and_republish(
    session: SiteResetSession,
    transaction: SiteResetTransaction,
    inputs: &InitialSiteInputs,
    request: &InitialSiteRequest,
) -> anyhow::Result<()> {
    let site_dir = session.site_root().to_owned();
    let provisioned_workers = session.provisioned_workers().with_context(|| {
        format!(
            "read provisioned workers of the site at {}",
            site_dir.display()
        )
    })?;

    transaction
        .reset_locked(session, inputs)
        .await
        .map_err(|error| reset_transaction_failure(error, &site_dir))?;
    if provisioned_workers > 0 {
        notify_stderr(
            &[
                Message::Warning(format!(
                    "Site trust was rotated: {provisioned_workers} joined node(s) can no longer connect"
                )),
                Message::Next(vec![
                    NextStep::new("Join each node again, with this on the manager:")
                        .with_command("dotc node add"),
                    NextStep::new("and this on the node:").with_command("dotc node join"),
                ]),
            ],
            &format!(
                "Notice: site trust was rotated; {provisioned_workers} previously joined node(s) can no longer connect; join each node again with dotc node add and dotc node join"
            ),
        );
    }
    write_success("Site reset", &site_dir, inputs, request)
}

fn reset_transaction_failure(error: SiteResetTransactionError, site_dir: &Path) -> anyhow::Error {
    let context = match &error {
        SiteResetTransactionError::Lock(SiteResetLockError::SiteRunning { .. }) => format!(
            "cannot reset site at {}; stop the backend and retry",
            site_dir.display()
        ),
        SiteResetTransactionError::Lock(SiteResetLockError::NodeRunning { .. }) => format!(
            "cannot reset the node folder at {}; stop the node server and retry",
            site_dir.display()
        ),
        SiteResetTransactionError::NativeCleanup(_) => {
            crate::runtime_confirmation::native_cleanup_failure("reset", site_dir)
        }
        _ => format!("offline site reset failed at {}", site_dir.display()),
    };
    anyhow::Error::new(error).context(context)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        fs,
        future::Future,
        path::{Path, PathBuf},
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use dotcore_node_hardware::hardware::{
        AcceleratorUnavailable, AcceleratorVendorAdapter, UnavailableReason,
    };
    use dotcore_runtime::{DockerCapabilityProbe, DockerCapabilityReport};
    use dotcore_site::{
        initial::{input_store::InitialSiteWriteError, probe::InitialSiteProber},
        reset::transaction::{
            SiteResetIdentityError, SiteResetLockError, SiteResetSession, SiteResetTransactionError,
        },
    };

    use super::{reset_site, reset_transaction_failure};
    use crate::{
        hosts::{ListenerHost, test_support::listener_host},
        site::{
            ApplicationsChoice, InferenceChoice, SiteResetArgs,
            test_support::{
                FakeDocker, SiteRoot, UnavailableNvidia, adapters, location, named_identities,
                reset_arguments,
            },
        },
        terminal::{
            Message, PromptError, ScriptedAnswer, ScriptedPrompt, ScriptedTerminal,
            TerminalCondition,
        },
        test_support::FakeSiteRuntime,
    };

    #[test]
    fn reset_execution_has_no_backend_server_or_management_composition_path() {
        let source = [include_str!("reset.rs"), include_str!("collection.rs")]
            .map(|file| {
                file.split("#[cfg(test)]")
                    .next()
                    .expect("production site CLI source")
            })
            .concat();

        for forbidden in [
            "BackendComposition",
            "server::start",
            "ManagementClient",
            "Process Compose",
            "process::Command",
            "owner API",
            "observer",
        ] {
            assert!(
                !source.contains(forbidden),
                "reset CLI contains {forbidden}"
            );
        }
        assert!(source.contains("SiteResetSession::begin"));
        assert!(source.contains("site_runtime_cleanup"));
        assert!(source.contains("SitePersistenceReset::new"));
        assert!(source.contains("SiteResetTransaction::new"));
        assert!(source.contains("reset_locked"));
    }

    #[test]
    fn busy_reset_with_docker_and_nvidia_choices_stops_before_every_probe_or_mutation() {
        let fixture = SiteRoot::new();
        let site_dir = fixture.root.join("busy-reset");
        fs::create_dir(&site_dir).expect("Site root");
        fs::write(site_dir.join("site.yaml"), "siteId: retained-site\n").expect("retained site");
        fs::write(site_dir.join("marker"), "unchanged").expect("retained marker");
        let before_site = fs::read(site_dir.join("site.yaml")).expect("Site bytes");
        let before_marker = fs::read(site_dir.join("marker")).expect("marker bytes");
        let held = SiteResetSession::begin(
            &site_dir,
            crate::runtime_confirmation::site_runtime_cleanups(None),
        )
        .expect("held reset lock");
        let docker_calls = Arc::new(AtomicUsize::new(0));
        let nvidia_calls = Arc::new(AtomicUsize::new(0));
        let prober = InitialSiteProber::new(
            Box::new(CountingDocker(Arc::clone(&docker_calls))),
            Box::new(CountingNvidia(Arc::clone(&nvidia_calls))),
        );
        let mut arguments = reset_arguments(InferenceChoice::Nvidia, ApplicationsChoice::Docker);
        arguments.configuration.non_interactive = false;
        arguments.configuration.interactive = true;

        let runtime = FakeSiteRuntime::with_resources(&["model-one"]);
        let error = crate::build_runtime()
            .expect("runtime")
            .block_on(reset_site(
                arguments,
                &location(&site_dir),
                None,
                None,
                &mut ScriptedTerminal::interactive([]),
                adapters(&prober, &mut named_identities(None)),
                |_| Ok(Box::new(runtime.clone())),
            ))
            .expect_err("busy reset");

        assert!(error.to_string().contains("stop the backend and retry"));
        assert_eq!(docker_calls.load(Ordering::Relaxed), 0);
        assert_eq!(nvidia_calls.load(Ordering::Relaxed), 0);
        assert_eq!(
            fs::read(site_dir.join("site.yaml")).expect("Site after"),
            before_site
        );
        assert_eq!(
            fs::read(site_dir.join("marker")).expect("marker after"),
            before_marker
        );
        assert!(runtime.calls().is_empty());
        drop(held);
    }

    #[test]
    fn confirmed_reset_lists_resources_then_cleans_them_and_republishes() {
        let fixture = ResetFixture::new(&["model-one", "app-one"]);
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(true)]);

        fixture
            .run(&mut terminal, false, false, false)
            .expect("confirmed reset");

        assert_eq!(
            fixture.runtime.calls(),
            ["inventory:retained-site", "cleanup:retained-site"]
        );
        assert!(fixture.runtime.resources().is_empty());
        assert!(
            fs::read_to_string(fixture.site_dir.join("site.yaml"))
                .expect("fresh site")
                .contains("siteId: site-test\n")
        );
        assert!(!fixture.site_dir.join("state.pb").exists());
        assert_eq!(
            terminal.messages(),
            [
                Message::Heading(format!(
                    "Site at {} owns these resources",
                    fixture.site_dir.display()
                )),
                Message::KeyValues(vec![
                    ("Test Runtime".to_owned(), "model-one".to_owned()),
                    ("Test Runtime".to_owned(), "app-one".to_owned()),
                ]),
            ]
        );
        assert!(matches!(
            terminal.prompts(),
            [ScriptedPrompt::Confirm(prompt)]
                if prompt.title() == "Remove them and reset the site?"
        ));
    }

    #[test]
    fn declined_reset_is_a_cancel_and_changes_nothing() {
        let fixture = ResetFixture::new(&["model-one"]);
        let before = fixture.site_tree();
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(false)]);

        let error = fixture
            .run(&mut terminal, false, false, false)
            .expect_err("declined reset");

        assert!(matches!(
            error.downcast_ref::<PromptError>(),
            Some(PromptError::Cancelled)
        ));
        assert_eq!(fixture.site_tree(), before);
        assert_eq!(fixture.runtime.calls(), ["inventory:retained-site"]);
        assert_eq!(fixture.runtime.resources(), ["model-one"]);
    }

    #[test]
    fn unattended_reset_requires_yes_when_resources_exist() {
        let fixture = ResetFixture::new(&["model-one"]);
        let before = fixture.site_tree();
        let expected = format!(
            "Site at {} owns 1 runtime resources; pass --yes to reset without confirmation",
            fixture.site_dir.display()
        );

        let mut interactive = ScriptedTerminal::interactive([]);
        let flag_error = fixture
            .run(&mut interactive, false, true, false)
            .expect_err("--non-interactive needs --yes");
        let mut no_terminal = ScriptedTerminal::new(TerminalCondition::NonInteractive, []);
        let terminal_error = fixture
            .run(&mut no_terminal, false, false, false)
            .expect_err("no terminal needs --yes");

        assert_eq!(flag_error.to_string(), expected);
        assert_eq!(terminal_error.to_string(), expected);
        assert!(interactive.prompts().is_empty());
        assert_eq!(fixture.site_tree(), before);
        assert_eq!(fixture.runtime.resources(), ["model-one"]);

        fixture
            .run(&mut no_terminal, true, true, false)
            .expect("--yes resets");

        assert!(fixture.runtime.resources().is_empty());
        assert!(!fixture.site_dir.join("state.pb").exists());
        assert_eq!(
            fixture.runtime.calls(),
            [
                "inventory:retained-site",
                "inventory:retained-site",
                "inventory:retained-site",
                "cleanup:retained-site"
            ]
        );
    }

    #[test]
    fn skip_runtime_cleanup_resets_an_unidentifiable_site_without_runtime_calls() {
        let fixture = ResetFixture::new(&["model-one"]);
        fs::write(fixture.site_dir.join("site.yaml"), "siteId: [\n").expect("broken site");
        let mut terminal = ScriptedTerminal::new(TerminalCondition::NonInteractive, []);

        let error = fixture
            .run(&mut terminal, false, true, false)
            .expect_err("unidentifiable without --skip-runtime-cleanup");
        assert!(format!("{error:#}").contains("offline site reset failed"));

        fixture
            .run(&mut terminal, false, true, true)
            .expect("reset without runtime cleanup");

        assert!(fixture.runtime.calls().is_empty());
        assert_eq!(fixture.runtime.resources(), ["model-one"]);
        assert!(
            fs::read_to_string(fixture.site_dir.join("site.yaml"))
                .expect("fresh site")
                .contains("siteId: site-test\n")
        );
        assert!(!fixture.site_dir.join("state.pb").exists());
        assert!(terminal.prompts().is_empty());
    }

    #[test]
    fn reset_without_resources_needs_no_prompt_or_yes() {
        let fixture = ResetFixture::new(&[]);
        let mut terminal = ScriptedTerminal::new(TerminalCondition::NonInteractive, []);

        fixture
            .run(&mut terminal, false, true, false)
            .expect("reset without resources");

        assert!(terminal.prompts().is_empty());
        assert_eq!(
            fixture.runtime.calls(),
            ["inventory:retained-site", "cleanup:retained-site"]
        );
        assert!(!fixture.site_dir.join("state.pb").exists());
    }

    #[test]
    fn guided_reset_shows_the_listener_host_as_fixed_and_records_it() {
        let fixture = ResetFixture::new(&[]);
        let mut arguments =
            reset_arguments(InferenceChoice::Simulated, ApplicationsChoice::Simulated);
        arguments.configuration.host = None;
        arguments.configuration.non_interactive = false;
        arguments.configuration.interactive = true;
        let mut terminal = ScriptedTerminal::interactive([
            ScriptedAnswer::Input(String::new()),
            ScriptedAnswer::Confirm(true),
        ]);

        fixture
            .run_with(
                arguments,
                Some(&listener_host("100.64.0.1:6402")),
                &mut terminal,
            )
            .expect("guided reset");

        assert_eq!(
            terminal.messages()[0],
            Message::Info("Node host: 100.64.0.1, from DOTCORE_INFERENCE_ADDR".to_owned())
        );
        assert!(!terminal.prompts().iter().any(|prompt| matches!(
            prompt,
            ScriptedPrompt::Select(select) if select.title() == "Node host"
        )));
        let site_yaml = fs::read_to_string(fixture.site_dir.join("site.yaml")).expect("Site");
        assert!(site_yaml.contains("host: 100.64.0.1\n"), "{site_yaml}");
    }

    #[test]
    fn guided_reset_asks_for_removal_approval_before_any_new_site_prompt() {
        let fixture = ResetFixture::new(&["model-one"]);
        let mut arguments =
            reset_arguments(InferenceChoice::Simulated, ApplicationsChoice::Simulated);
        arguments.configuration.host = None;
        arguments.configuration.non_interactive = false;
        arguments.configuration.interactive = true;
        let mut terminal = ScriptedTerminal::interactive([
            ScriptedAnswer::Confirm(true),
            ScriptedAnswer::Input(String::new()),
            ScriptedAnswer::Confirm(true),
        ]);

        fixture
            .run_with(
                arguments,
                Some(&listener_host("100.64.0.1:6402")),
                &mut terminal,
            )
            .expect("guided reset");

        assert!(matches!(
            terminal.prompts().first(),
            Some(ScriptedPrompt::Confirm(prompt))
                if prompt.title() == "Remove them and reset the site?"
        ));
        assert_eq!(terminal.prompts().len(), 3);
        assert!(fixture.runtime.resources().is_empty());
    }

    #[test]
    fn declined_guided_reset_probes_and_asks_nothing_for_the_new_site() {
        let fixture = ResetFixture::new(&["model-one"]);
        let before = fixture.site_tree();
        let docker_calls = Arc::new(AtomicUsize::new(0));
        let nvidia_calls = Arc::new(AtomicUsize::new(0));
        let prober = InitialSiteProber::new(
            Box::new(CountingDocker(Arc::clone(&docker_calls))),
            Box::new(CountingNvidia(Arc::clone(&nvidia_calls))),
        );
        let mut arguments = reset_arguments(InferenceChoice::Nvidia, ApplicationsChoice::Docker);
        arguments.configuration.non_interactive = false;
        arguments.configuration.interactive = true;
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(false)]);

        let error = fixture
            .run_probed(arguments, None, &mut terminal, &prober)
            .expect_err("declined reset");

        assert!(matches!(
            error.downcast_ref::<PromptError>(),
            Some(PromptError::Cancelled)
        ));
        assert!(matches!(
            terminal.prompts(),
            [ScriptedPrompt::Confirm(prompt)]
                if prompt.title() == "Remove them and reset the site?"
        ));
        assert_eq!(docker_calls.load(Ordering::Relaxed), 0);
        assert_eq!(nvidia_calls.load(Ordering::Relaxed), 0);
        assert_eq!(fixture.site_tree(), before);
        assert_eq!(fixture.runtime.calls(), ["inventory:retained-site"]);
        assert_eq!(fixture.runtime.resources(), ["model-one"]);
    }

    #[test]
    fn reset_refused_for_missing_yes_probes_nothing() {
        let fixture = ResetFixture::new(&["model-one"]);
        let before = fixture.site_tree();
        let docker_calls = Arc::new(AtomicUsize::new(0));
        let nvidia_calls = Arc::new(AtomicUsize::new(0));
        let prober = InitialSiteProber::new(
            Box::new(CountingDocker(Arc::clone(&docker_calls))),
            Box::new(CountingNvidia(Arc::clone(&nvidia_calls))),
        );
        let arguments = reset_arguments(InferenceChoice::Nvidia, ApplicationsChoice::Docker);
        let mut terminal = ScriptedTerminal::new(TerminalCondition::NonInteractive, []);

        let error = fixture
            .run_probed(arguments, None, &mut terminal, &prober)
            .expect_err("refused without --yes");

        assert!(error.to_string().contains("pass --yes to reset"));
        assert_eq!(docker_calls.load(Ordering::Relaxed), 0);
        assert_eq!(nvidia_calls.load(Ordering::Relaxed), 0);
        assert!(terminal.prompts().is_empty());
        assert_eq!(fixture.site_tree(), before);
    }

    #[test]
    fn busy_reset_error_retains_typed_lock_context() {
        let site_dir = Path::new("/tmp/typed-busy-site");
        let error = reset_transaction_failure(
            SiteResetTransactionError::Lock(SiteResetLockError::SiteRunning {
                site_root: site_dir.to_owned(),
            }),
            site_dir,
        );

        assert!(error.to_string().contains("stop the backend and retry"));
        assert!(error.chain().any(|source| {
            matches!(
                source.downcast_ref::<SiteResetTransactionError>(),
                Some(SiteResetTransactionError::Lock(
                    SiteResetLockError::SiteRunning { site_root }
                )) if site_root == site_dir
            )
        }));
    }

    #[test]
    fn reset_administrator_failures_retain_every_transaction_category() {
        let site_dir = Path::new("/tmp/typed-reset-site");
        let errors = [
            SiteResetTransactionError::ExistingSiteIdentity(SiteResetIdentityError::InvalidSiteId),
            SiteResetTransactionError::RuntimeCleanup(anyhow::anyhow!("injected runtime source")),
            SiteResetTransactionError::PersistenceCleanup(anyhow::anyhow!(
                "injected persistence source"
            )),
            SiteResetTransactionError::FreshInitialization(InitialSiteWriteError::InvalidInputs(
                anyhow::anyhow!("injected publication source"),
            )),
        ];

        for transaction_error in errors {
            let error = reset_transaction_failure(transaction_error, site_dir);

            assert!(
                error
                    .to_string()
                    .contains(&site_dir.to_string_lossy().into_owned())
            );
            assert!(
                error
                    .chain()
                    .any(|source| { source.downcast_ref::<SiteResetTransactionError>().is_some() })
            );
        }
    }

    /// Existing site with retained state whose fake runtime owns the given resources.
    struct ResetFixture {
        _root: SiteRoot,
        site_dir: PathBuf,
        runtime: FakeSiteRuntime,
    }

    impl ResetFixture {
        fn new(resources: &[&str]) -> Self {
            let root = SiteRoot::new();
            let site_dir = root.root.join("site");
            fs::create_dir(&site_dir).expect("Site root");
            fs::write(
                site_dir.join("site.yaml"),
                "siteId: retained-site\nmodelDeployments: []\n",
            )
            .expect("retained site");
            fs::write(site_dir.join("state.pb"), b"retained state").expect("retained state");
            Self {
                _root: root,
                site_dir,
                runtime: FakeSiteRuntime::with_resources(resources),
            }
        }

        fn run(
            &self,
            terminal: &mut ScriptedTerminal,
            yes: bool,
            non_interactive: bool,
            skip_runtime_cleanup: bool,
        ) -> anyhow::Result<()> {
            let mut arguments =
                reset_arguments(InferenceChoice::Simulated, ApplicationsChoice::Simulated);
            arguments.configuration.non_interactive = non_interactive;
            arguments.removal.yes = yes;
            arguments.removal.skip_runtime_cleanup = skip_runtime_cleanup;
            self.run_with(arguments, None, terminal)
        }

        fn run_with(
            &self,
            arguments: SiteResetArgs,
            listener: Option<&ListenerHost>,
            terminal: &mut ScriptedTerminal,
        ) -> anyhow::Result<()> {
            let prober = InitialSiteProber::new(Box::new(FakeDocker), Box::new(UnavailableNvidia));
            self.run_probed(arguments, listener, terminal, &prober)
        }

        fn run_probed(
            &self,
            arguments: SiteResetArgs,
            listener: Option<&ListenerHost>,
            terminal: &mut ScriptedTerminal,
            prober: &InitialSiteProber,
        ) -> anyhow::Result<()> {
            let runtime = self.runtime.clone();
            crate::build_runtime()
                .expect("runtime")
                .block_on(reset_site(
                    arguments,
                    &location(&self.site_dir),
                    None,
                    listener,
                    terminal,
                    adapters(prober, &mut named_identities(None)),
                    move |_| Ok(Box::new(runtime)),
                ))
        }

        /// Every file under the site root with its bytes, in path order.
        fn site_tree(&self) -> Vec<(PathBuf, Vec<u8>)> {
            let mut tree = fs::read_dir(&self.site_dir)
                .expect("Site entries")
                .map(|entry| {
                    let path = entry.expect("Site entry").path();
                    let bytes = fs::read(&path).expect("Site file");
                    (path, bytes)
                })
                .collect::<Vec<_>>();
            tree.sort();
            tree
        }
    }

    struct CountingDocker(Arc<AtomicUsize>);

    impl DockerCapabilityProbe for CountingDocker {
        fn probe(
            &self,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<DockerCapabilityReport>> + Send + '_>>
        {
            self.0.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { anyhow::bail!("unexpected Docker probe") })
        }

        fn nvidia_cdi_selectors(
            &self,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<BTreeSet<String>>> + Send + '_>> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { anyhow::bail!("unexpected Docker selector probe") })
        }
    }

    struct CountingNvidia(Arc<AtomicUsize>);

    impl AcceleratorVendorAdapter for CountingNvidia {
        fn vendor(&self) -> dotcore_proto::dotcore::observation::v1::AcceleratorVendor {
            dotcore_proto::dotcore::observation::v1::AcceleratorVendor::Nvidia
        }

        fn collect_accelerator_facts(
            &self,
        ) -> Result<
            Vec<dotcore_proto::dotcore::observation::v1::AcceleratorFacts>,
            AcceleratorUnavailable,
        > {
            self.0.fetch_add(1, Ordering::Relaxed);
            Err(AcceleratorUnavailable::stable(UnavailableReason::NoDevice))
        }
    }
}
