//! Initialization of a site's input bundle from the arguments or guided answers.
//!
//! An existing site is kept when it already matches what was asked, replaced in place only on
//! request, and a request to start a fresh site is handed to the reset flow. Nothing runs
//! before the arguments are complete or the operator confirms the guided plan.

use std::path::Path;

use dotcore_site::initial::input_store::{
    InitialSiteInputWriter, InitialSitePresence, InitialSiteWriteError,
};

use super::{
    HostAwareIdentityGenerator, SiteInitArgs, SiteLocation, SiteResetArgs,
    collection::{CollectionAdapters, CollectionMode, collect_initial_site},
    output::write_transaction_success,
    recorded_host, reset, system_initial_site_prober, system_listener_host, validator,
};
use crate::{
    RootPathArgs,
    hosts::{ListenerHost, SystemHostEnvironment},
    terminal::open_terminal,
};

/// Initializes the site the root path options name, from the arguments or guided answers.
pub(super) async fn initialize(
    arguments: SiteInitArgs,
    paths: &RootPathArgs,
) -> anyhow::Result<()> {
    if arguments.replace && arguments.force {
        return reset::reset(
            SiteResetArgs {
                configuration: arguments.configuration,
                removal: arguments.removal,
            },
            paths,
        )
        .await;
    }
    let location = SiteLocation::resolve(paths)?;
    let site_dir = location.site_dir.as_path();
    if arguments.if_missing {
        let writer = InitialSiteInputWriter::new(validator(location.catalog_dir.as_deref()));
        match writer
            .inspect_if_missing(site_dir)
            .map_err(|error| initialization_transaction_failure(error, site_dir))?
        {
            InitialSitePresence::Missing => {}
            InitialSitePresence::Initialized => return Ok(()),
        }
    }
    let listener = system_listener_host(&arguments.configuration).await?;
    let prober = system_initial_site_prober();
    let mut identities = HostAwareIdentityGenerator::new();
    let mut terminal = None;
    let mode = CollectionMode::select(&arguments.configuration, || {
        terminal.insert(open_terminal()).as_mut()
    })?;
    initialize_site(
        arguments,
        &location,
        listener.as_ref(),
        mode,
        CollectionAdapters {
            prober: &prober,
            environment: &SystemHostEnvironment,
            identities: &mut identities,
        },
    )
    .await
}

/// Collects the site in `mode`, then writes it unless an existing site already matches.
async fn initialize_site(
    arguments: SiteInitArgs,
    location: &SiteLocation,
    listener: Option<&ListenerHost>,
    mode: CollectionMode<'_>,
    adapters: CollectionAdapters<'_>,
) -> anyhow::Result<()> {
    let site_dir = location.site_dir.as_path();
    let recorded = if arguments.replace {
        None
    } else {
        recorded_host(site_dir)
    };
    let collected = collect_initial_site(
        &arguments.configuration,
        location,
        listener,
        recorded,
        mode,
        adapters,
    )
    .await?;
    let prepared = &collected.prepared;
    let writer = InitialSiteInputWriter::new(validator(location.catalog_dir.as_deref()));
    let outcome = writer
        .initialize(
            site_dir,
            &prepared.inputs,
            &prepared.request,
            arguments.replace,
        )
        .map_err(|error| initialization_transaction_failure(error, site_dir))?;
    write_transaction_success(
        outcome,
        site_dir,
        &prepared.inputs,
        &prepared.request,
        collected.unreported_host.as_ref(),
    )
}

fn initialization_transaction_failure(
    error: InitialSiteWriteError,
    site_dir: &Path,
) -> anyhow::Error {
    let context = match &error {
        InitialSiteWriteError::ExistingInputsInvalid(_)
        | InitialSiteWriteError::ExistingInputsUnreadable(_)
        | InitialSiteWriteError::DeploymentInspection(_) => format!(
            "existing site inputs are invalid; run dotc site validate --site-dir {}",
            site_dir.display()
        ),
        InitialSiteWriteError::IntentConflict => format!(
            "run dotc site validate --site-dir {} to check the existing site",
            site_dir.display()
        ),
        InitialSiteWriteError::RetainedStateResetRequired => {
            return anyhow::anyhow!(
                "Site at {} has deployments or state from a backend run that --replace keeps; rerun with --replace --force, or run dotc site reset, to start a fresh site",
                site_dir.display()
            );
        }
        InitialSiteWriteError::Symlink { path } => {
            format!(
                "initial site target {} must not be a symbolic link",
                path.display()
            )
        }
        _ => return anyhow::Error::new(error),
    };
    anyhow::Error::new(error).context(context)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use dotcore_node_hardware::hardware::SystemNvidiaAcceleratorAdapter;
    use dotcore_site::initial::{
        input_store::{InitialSiteInputWriter, InitialSiteWriteOutcome},
        inputs::InitialSiteInputs,
        probe::InitialSiteProber,
    };

    use super::initialize_site;
    use crate::{
        hosts::test_support::{FakeHostEnvironment, TAILNET_NAME, listener_host},
        site::{
            ApplicationsChoice, InferenceChoice, NativeWorkloadLifetimeChoice,
            collection::{CollectionAdapters, CollectionMode},
            output::success_records,
            prepare_initial_site,
            test_support::{
                FakeApple, FakeDocker, SiteRoot, UnavailableNvidia, adapters, flagged_request,
                init_arguments, lan_machine, location, named_identities,
            },
            validator,
        },
        terminal::{ScriptedAnswer, ScriptedTerminal},
    };

    #[test]
    fn apple_init_declares_a_native_target_with_the_observed_device_and_lifetime() {
        use dotcore_proto::dotcore::site::v1::{
            NativeRuntimeAdapter, NativeWorkloadLifetime, execution_target, model_runtime_binding,
        };

        for (choice, lifetime) in [
            (None, NativeWorkloadLifetime::Persistent),
            (
                Some(NativeWorkloadLifetimeChoice::Ephemeral),
                NativeWorkloadLifetime::Ephemeral,
            ),
        ] {
            let fixture = SiteRoot::new();
            let site_dir = fixture.root.join("site");
            let mut arguments = init_arguments(ApplicationsChoice::Simulated);
            arguments.configuration.inference = vec![InferenceChoice::Apple.into()];
            arguments.configuration.native_workload_lifetime = choice;
            let prober = InitialSiteProber::new(Box::new(FakeDocker), Box::new(UnavailableNvidia))
                .with_apple_accelerator(Box::new(FakeApple));

            crate::build_runtime()
                .expect("runtime")
                .block_on(initialize_site(
                    arguments,
                    &location(&site_dir),
                    None,
                    CollectionMode::Arguments,
                    adapters(&prober, &mut named_identities(None)),
                ))
                .expect("initialize Apple site");

            let inputs = InitialSiteInputs::load(
                &site_dir,
                &dotcore_backend::model::site_declaration_rules(),
            )
            .expect("Site inputs");
            let model = inputs
                .site()
                .execution_targets
                .iter()
                .find(|target| {
                    matches!(
                        target.runtime_binding,
                        Some(execution_target::RuntimeBinding::ModelRuntime(_))
                    )
                })
                .expect("model target");
            assert_eq!(model.accelerator_device_ids, ["apple-gpu-0"]);
            assert_eq!(
                model.platform,
                Some(dotcore_domain::ExecutionPlatform::darwin_arm64().to_oci_platform())
            );
            let Some(execution_target::RuntimeBinding::ModelRuntime(binding)) =
                &model.runtime_binding
            else {
                panic!("model binding")
            };
            assert_eq!(
                binding.adapter,
                Some(model_runtime_binding::Adapter::NativeRuntime(
                    NativeRuntimeAdapter {
                        workload_lifetime: lifetime as i32
                    }
                ))
            );
        }
    }

    #[test]
    fn apple_init_without_an_apple_gpu_fails_before_writing() {
        let fixture = SiteRoot::new();
        let site_dir = fixture.root.join("site");
        let mut arguments = init_arguments(ApplicationsChoice::Simulated);
        arguments.configuration.inference = vec![InferenceChoice::Apple.into()];
        let prober = InitialSiteProber::new(Box::new(FakeDocker), Box::new(UnavailableNvidia));

        let error = crate::build_runtime()
            .expect("runtime")
            .block_on(initialize_site(
                arguments,
                &location(&site_dir),
                None,
                CollectionMode::Arguments,
                adapters(&prober, &mut named_identities(None)),
            ))
            .expect_err("Apple without a GPU");

        assert!(format!("{error:#}").contains("Apple inference is unavailable: Unsupported"));
        assert!(!site_dir.exists());
    }

    #[test]
    fn noninteractive_init_without_a_host_records_the_listener_host() {
        let fixture = SiteRoot::new();
        let site_dir = fixture.root.join("site");
        let mut arguments = init_arguments(ApplicationsChoice::Simulated);
        arguments.configuration.host = None;
        let prober = InitialSiteProber::new(Box::new(FakeDocker), Box::new(UnavailableNvidia));

        crate::build_runtime()
            .expect("runtime")
            .block_on(initialize_site(
                arguments,
                &location(&site_dir),
                Some(&listener_host("100.64.0.1:6402")),
                CollectionMode::Arguments,
                adapters(&prober, &mut named_identities(None)),
            ))
            .expect("initial site");

        let site_yaml = fs::read_to_string(site_dir.join("site.yaml")).expect("Site input");
        assert!(site_yaml.contains("host: 100.64.0.1\n"), "{site_yaml}");
    }

    #[test]
    fn a_recommended_tailnet_name_is_written_and_a_later_default_still_matches() {
        let fixture = SiteRoot::new();
        let site_dir = fixture.root.join("site");
        let prober = InitialSiteProber::new(Box::new(FakeDocker), Box::new(UnavailableNvidia));
        let runtime = crate::build_runtime().expect("runtime");
        let unhosted = || {
            let mut arguments = init_arguments(ApplicationsChoice::Simulated);
            arguments.configuration.host = None;
            arguments
        };

        runtime
            .block_on(initialize_site(
                unhosted(),
                &location(&site_dir),
                None,
                CollectionMode::Arguments,
                CollectionAdapters {
                    prober: &prober,
                    environment: &FakeHostEnvironment::on_tailnet(),
                    identities: &mut named_identities(None),
                },
            ))
            .expect("initial site");
        let site_yaml = fs::read_to_string(site_dir.join("site.yaml")).expect("site input");
        assert!(
            site_yaml.contains(&format!("host: {TAILNET_NAME}\n")),
            "{site_yaml}"
        );

        runtime
            .block_on(initialize_site(
                unhosted(),
                &location(&site_dir),
                None,
                CollectionMode::Arguments,
                CollectionAdapters {
                    prober: &prober,
                    environment: &FakeHostEnvironment::without_tailnet_or_lan(),
                    identities: &mut named_identities(None),
                },
            ))
            .expect("another default still matches");
        assert_eq!(
            fs::read_to_string(site_dir.join("site.yaml")).expect("site input"),
            site_yaml
        );
    }

    #[test]
    fn fake_driven_execution_creates_and_preserves_matching_site_inputs() {
        let fixture = SiteRoot::new();
        let site_dir = fixture.root.join("site");
        let arguments = init_arguments(ApplicationsChoice::Docker);
        let prober = InitialSiteProber::new(
            Box::new(FakeDocker),
            Box::new(SystemNvidiaAcceleratorAdapter::for_current_host()),
        );
        let runtime = crate::build_runtime().expect("runtime");

        runtime
            .block_on(initialize_site(
                arguments,
                &location(&site_dir),
                None,
                CollectionMode::Arguments,
                adapters(&prober, &mut named_identities(None)),
            ))
            .expect("initialize with fake Docker");
        let site_yaml = fs::read(site_dir.join("site.yaml")).expect("Site input");
        let node_yaml = fs::read(site_dir.join("node.yaml")).expect("node input");

        runtime
            .block_on(initialize_site(
                init_arguments(ApplicationsChoice::Docker),
                &location(&site_dir),
                None,
                CollectionMode::Arguments,
                adapters(&prober, &mut named_identities(None)),
            ))
            .expect("matching site is preserved");
        assert_eq!(
            fs::read(site_dir.join("site.yaml")).expect("Site input"),
            site_yaml
        );
        assert_eq!(
            fs::read(site_dir.join("node.yaml")).expect("node input"),
            node_yaml
        );
        let matching_arguments = init_arguments(ApplicationsChoice::Docker);
        let existing =
            InitialSiteInputs::load(&site_dir, &dotcore_backend::model::site_declaration_rules())
                .expect("existing site inputs");
        assert_eq!(
            success_records(
                "Site already initialized",
                &site_dir,
                &existing,
                &flagged_request(&matching_arguments.configuration),
            )
            .expect("success records"),
            format!(
                "Site already initialized\nDOTCORE_SITE_DIR={}\nDOTCORE_SITE_ID=site-test\nDOTCORE_NODE_ID=node-test\nDOTCORE_NODE_HOST=192.0.2.10\nDOTCORE_INFERENCE=simulated\nDOTCORE_APPLICATIONS=docker\n",
                site_dir.display()
            )
        );

        let error = runtime
            .block_on(initialize_site(
                init_arguments(ApplicationsChoice::Simulated),
                &location(&site_dir),
                None,
                CollectionMode::Arguments,
                adapters(&prober, &mut named_identities(None)),
            ))
            .expect_err("conflicting workload choice");
        assert!(crate::format_failure(&error).contains("--replace"));
        assert_eq!(
            fs::read(site_dir.join("site.yaml")).expect("Site input"),
            site_yaml
        );
    }

    #[test]
    fn node_name_default_is_written_but_never_counts_as_operator_intent() {
        let fixture = SiteRoot::new();
        let site_dir = fixture.root.join("site");
        let prober = InitialSiteProber::new(
            Box::new(FakeDocker),
            Box::new(SystemNvidiaAcceleratorAdapter::for_current_host()),
        );
        let runtime = crate::build_runtime().expect("runtime");
        runtime
            .block_on(initialize_site(
                init_arguments(ApplicationsChoice::Simulated),
                &location(&site_dir),
                None,
                CollectionMode::Arguments,
                adapters(&prober, &mut named_identities(Some("studio"))),
            ))
            .expect("initial site");
        let site_yaml = fs::read(site_dir.join("site.yaml")).expect("Site input");
        let node_yaml = fs::read(site_dir.join("node.yaml")).expect("node input");
        assert!(
            String::from_utf8_lossy(&site_yaml).contains("displayName: studio\n"),
            "{}",
            String::from_utf8_lossy(&site_yaml)
        );

        runtime
            .block_on(initialize_site(
                init_arguments(ApplicationsChoice::Simulated),
                &location(&site_dir),
                None,
                CollectionMode::Arguments,
                adapters(&prober, &mut named_identities(Some("other-host"))),
            ))
            .expect("different default still matches");
        let arguments = init_arguments(ApplicationsChoice::Simulated);
        let request = flagged_request(&arguments.configuration);
        let prepared = runtime
            .block_on(prepare_initial_site(
                request,
                &prober,
                lan_machine(),
                &mut named_identities(Some("other-host")),
            ))
            .expect("prepared");
        let outcome = InitialSiteInputWriter::new(validator(None))
            .initialize(&site_dir, &prepared.inputs, &prepared.request, false)
            .expect("initialize");
        assert!(matches!(
            outcome,
            InitialSiteWriteOutcome::AlreadyInitialized(_)
        ));
        assert_eq!(
            fs::read(site_dir.join("site.yaml")).expect("Site input"),
            site_yaml
        );
        assert_eq!(
            fs::read(site_dir.join("node.yaml")).expect("node input"),
            node_yaml
        );

        let mut renamed = init_arguments(ApplicationsChoice::Simulated);
        renamed.configuration.display_name = Some("Test node".to_owned());
        let error = runtime
            .block_on(initialize_site(
                renamed,
                &location(&site_dir),
                None,
                CollectionMode::Arguments,
                adapters(&prober, &mut named_identities(Some("studio"))),
            ))
            .expect_err("different explicit name conflicts");
        assert!(
            crate::format_failure(&error).contains("--replace"),
            "{error}"
        );
        assert_eq!(
            fs::read(site_dir.join("site.yaml")).expect("Site input"),
            site_yaml
        );
    }

    #[test]
    fn explicit_display_name_flag_is_written_over_the_default() {
        let fixture = SiteRoot::new();
        let site_dir = fixture.root.join("site");
        let prober = InitialSiteProber::new(
            Box::new(FakeDocker),
            Box::new(SystemNvidiaAcceleratorAdapter::for_current_host()),
        );
        let mut arguments = init_arguments(ApplicationsChoice::Simulated);
        arguments.configuration.display_name = Some("Test node".to_owned());

        crate::build_runtime()
            .expect("runtime")
            .block_on(initialize_site(
                arguments,
                &location(&site_dir),
                None,
                CollectionMode::Arguments,
                adapters(&prober, &mut named_identities(Some("studio"))),
            ))
            .expect("initial site");

        let site_yaml = fs::read_to_string(site_dir.join("site.yaml")).expect("Site input");
        assert!(
            site_yaml.contains("displayName: Test node\n"),
            "{site_yaml}"
        );
    }

    #[test]
    fn confirmed_guided_plan_uses_the_existing_writer_path() {
        let fixture = SiteRoot::new();
        let site_dir = fixture.root.join("guided-site");
        let mut arguments = init_arguments(ApplicationsChoice::Simulated);
        arguments.configuration.non_interactive = false;
        arguments.configuration.interactive = true;
        let mut terminal = ScriptedTerminal::interactive([
            ScriptedAnswer::Input(String::new()),
            ScriptedAnswer::Confirm(true),
        ]);
        let prober = InitialSiteProber::new(Box::new(FakeDocker), Box::new(UnavailableNvidia));

        crate::build_runtime()
            .expect("runtime")
            .block_on(initialize_site(
                arguments,
                &location(&site_dir),
                None,
                CollectionMode::Guided {
                    terminal: &mut terminal,
                },
                adapters(&prober, &mut named_identities(None)),
            ))
            .expect("confirmed guided initialization");

        assert!(site_dir.join("site.yaml").is_file());
        assert!(site_dir.join("node.yaml").is_file());
    }

    #[test]
    fn guided_matching_site_preserves_existing_inputs_without_replacement() {
        let fixture = SiteRoot::new();
        let site_dir = fixture.root.join("guided-existing-site");
        let prober = InitialSiteProber::new(
            Box::new(FakeDocker),
            Box::new(SystemNvidiaAcceleratorAdapter::for_current_host()),
        );
        let runtime = crate::build_runtime().expect("runtime");
        runtime
            .block_on(initialize_site(
                init_arguments(ApplicationsChoice::Simulated),
                &location(&site_dir),
                None,
                CollectionMode::Arguments,
                adapters(&prober, &mut named_identities(None)),
            ))
            .expect("initial site");
        let before = fs::read(site_dir.join("site.yaml")).expect("existing site input");

        let mut arguments = init_arguments(ApplicationsChoice::Simulated);
        arguments.configuration.non_interactive = false;
        arguments.configuration.interactive = true;
        let mut terminal = ScriptedTerminal::interactive([
            ScriptedAnswer::Input(String::new()),
            ScriptedAnswer::Confirm(true),
        ]);
        let guided_prober =
            InitialSiteProber::new(Box::new(FakeDocker), Box::new(UnavailableNvidia));
        runtime
            .block_on(initialize_site(
                arguments,
                &location(&site_dir),
                None,
                CollectionMode::Guided {
                    terminal: &mut terminal,
                },
                adapters(&guided_prober, &mut named_identities(Some("studio"))),
            ))
            .expect("matching guided site");

        assert_eq!(
            fs::read(site_dir.join("site.yaml")).expect("existing site input"),
            before
        );
    }

    #[test]
    fn invalid_existing_site_is_unchanged_and_directs_to_validation() {
        let fixture = SiteRoot::new();
        let site_dir = fixture.root.join("site");
        fs::create_dir(&site_dir).expect("existing site root");
        let site_yaml = b"siteId: [\n";
        fs::write(site_dir.join("site.yaml"), site_yaml).expect("invalid site input");
        fs::write(site_dir.join("node.yaml"), "nodeId: node-test\n").expect("node input");
        let prober = InitialSiteProber::new(
            Box::new(FakeDocker),
            Box::new(SystemNvidiaAcceleratorAdapter::for_current_host()),
        );

        let error = crate::build_runtime()
            .expect("runtime")
            .block_on(initialize_site(
                init_arguments(ApplicationsChoice::Docker),
                &location(&site_dir),
                None,
                CollectionMode::Arguments,
                adapters(&prober, &mut named_identities(None)),
            ))
            .expect_err("invalid existing site");

        assert!(error.to_string().contains("site validate"));
        assert_eq!(
            fs::read(site_dir.join("site.yaml")).expect("Site input"),
            site_yaml
        );
    }

    #[test]
    fn nvidia_without_hardware_fails_before_writing_with_fake_probe_seams() {
        let fixture = SiteRoot::new();
        let site_dir = fixture.root.join("site");
        let mut arguments = init_arguments(ApplicationsChoice::Simulated);
        arguments.configuration.inference = vec![InferenceChoice::Nvidia.into()];
        let prober = InitialSiteProber::new(Box::new(FakeDocker), Box::new(UnavailableNvidia));

        let error = crate::build_runtime()
            .expect("runtime")
            .block_on(initialize_site(
                arguments,
                &location(&site_dir),
                None,
                CollectionMode::Arguments,
                adapters(&prober, &mut named_identities(None)),
            ))
            .expect_err("NVIDIA without hardware");

        let rendered = format!("{error:#}");
        assert!(rendered.contains("NVIDIA inference is unavailable: MissingLibrary"));
        assert!(rendered.contains("libnvidia-ml.so.1"));
        assert!(rendered.contains("/usr/lib/aarch64-linux-gnu/libnvidia-ml.so.1"));
        assert!(rendered.contains("cannot open shared object file"));
        assert!(!site_dir.exists());
    }
}
