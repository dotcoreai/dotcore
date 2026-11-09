use std::{
    collections::BTreeMap,
    error::Error as _,
    fs,
    path::{Path, PathBuf},
};

use dotcore_domain::test_support::tracing::{Record, capture_records_async};
use dotcore_proto::dotcore::{
    catalog::v1::{
        UpstreamModelBoundary, UpstreamModelDataEgressClass, UpstreamModelLocality,
        UpstreamModelTemplate, UpstreamWireProtocol,
    },
    deployment::v1::ExactPlacement,
    model::v1::{
        ModelDeploymentDeclaration, ModelDeploymentDesiredState, ModelDeploymentSnapshot,
        OpenAiCompatibleEndpointConfiguration, UpstreamModelDeploymentSnapshot,
        UpstreamModelDesiredConfiguration, UpstreamModelInputCapabilities,
        model_deployment_snapshot::Source as ModelDeploymentSnapshotSource,
    },
    site::v1::{Site, SiteNode},
};
use dotcore_site::{
    NODE_FILE,
    node::{
        binding::{BoundSiteNode, NodeBindingError},
        folder::WORKER_MANIFEST_FILE,
        target_inventory::StaticTargetCapabilitySource,
    },
    reset::transaction::{SiteResetLock, SiteResetLockError},
};
use http::HeaderValue;

use super::node_binding::node_configuration_for;
use super::*;

#[tokio::test]
async fn the_metric_catalog_follows_the_naming_and_export_rules() {
    let catalog = metric_catalog(std::time::Duration::from_secs(5));
    assert_eq!(dotcore_observability::validate_catalog(&catalog), Ok(()));
    // The store adds its own metrics and checks the whole catalog again.
    assert_eq!(
        InMemoryMetricStore::start(
            InMemoryStoreConfig::default(),
            &dotcore_domain::timing::Timing::production().metrics,
            &dotcore_domain::timing::Timing::production().watch,
            catalog,
            Arc::new(dotcore_domain::clock::SystemClock),
        )
        .err()
        .map(|error| error.to_string()),
        None
    );
}

#[test]
fn per_collection_metrics_follow_the_sites_hardware_refresh_interval() {
    let site = Site {
        hardware_observation: Some(
            dotcore_proto::dotcore::site::v1::HardwareObservationConfiguration {
                refresh_interval_seconds: Some(7),
            },
        ),
        ..Default::default()
    };

    // The same interval every node's observation configuration pins.
    let catalog = metric_catalog(dotcore_site::validation::hardware_refresh_interval(&site));

    let timing = |name: &str| {
        catalog
            .iter()
            .find(|descriptor| descriptor.name == name)
            .map(|descriptor| (descriptor.sample_interval, descriptor.reporting_delay))
    };
    let per_collection = Some((
        Some(std::time::Duration::from_secs(7)),
        Some(std::time::Duration::from_secs(5)),
    ));
    for name in [
        "deployment.accelerator.memory.used",
        "application.host.cpu.utilization",
        "application.host.memory.used",
        "application.accelerator.memory.used",
    ] {
        assert_eq!(timing(name), per_collection, "{name}");
    }
    assert_eq!(
        timing("node.host.cpu.utilization"),
        Some((Some(std::time::Duration::from_secs(1)), None))
    );
}

#[tokio::test(start_paused = true)]
async fn a_backend_reads_its_test_manual_clock_or_else_system_time() {
    let manual = dotcore_domain::clock::ManualClock::new();
    let manual_choice = backend_clock(&BackendTestSupport {
        clock: Some(Arc::new(manual.clone())),
        ..BackendTestSupport::default()
    });
    let system_choice = backend_clock(&BackendTestSupport::default());

    manual.advance(std::time::Duration::from_secs(5));

    let now = tokio::time::Instant::now();
    assert_eq!(manual_choice.now(), now + std::time::Duration::from_secs(5));
    assert_eq!(system_choice.now(), now);
}

#[test]
fn backend_configuration_pins_required_and_invalid_inputs() {
    let valid = [
        Some("/site".to_owned()),
        Some("/catalog".to_owned()),
        Some("/cache".to_owned()),
        Some("127.0.0.1:50051".to_owned()),
        Some("127.0.0.1:50052".to_owned()),
        Some("http://127.0.0.1:5173".to_owned()),
    ];
    let variables = [
        dotcore_domain::SITE_DIR,
        dotcore_domain::CATALOG_DIR,
        dotcore_domain::CACHE_DIR,
        MANAGEMENT_ADDR_ENV,
        INFERENCE_ADDR_ENV,
        FRONTEND_ORIGIN_ENV,
    ];
    let missing_diagnostics = [
        "Invalid site path: DOTCORE_SITE_DIR is not set",
        "Invalid site path: DOTCORE_CATALOG_DIR is not set",
        "Invalid site path: DOTCORE_CACHE_DIR is not set",
        "Invalid backend configuration: DOTCORE_MANAGEMENT_ADDR is not set",
        "Invalid backend configuration: DOTCORE_INFERENCE_ADDR is not set",
    ];
    for (index, variable) in variables.into_iter().take(5).enumerate() {
        let mut values = valid.clone();
        values[index] = None;
        let error = BackendConfiguration::from_values(
            values[0].clone(),
            values[1].clone(),
            values[2].clone(),
            values[3].clone(),
            values[4].clone(),
            values[5].clone(),
        )
        .expect_err("missing input");
        assert_eq!(
            format!("{:#}", anyhow::Error::from(error)),
            missing_diagnostics[index],
            "{variable}"
        );
    }

    let invalid_inputs = [
        (
            "relative/site",
            "Invalid site path: DOTCORE_SITE_DIR must be an absolute path, got \"relative/site\"",
        ),
        (
            "relative/catalog",
            "Invalid site path: DOTCORE_CATALOG_DIR must be an absolute path, got \"relative/catalog\"",
        ),
        (
            "/",
            "Invalid site path: DOTCORE_CACHE_DIR cannot be the filesystem root",
        ),
        (
            "0.0.0.0:50051",
            "Invalid backend configuration: DOTCORE_MANAGEMENT_ADDR must use a specific IPv4 address or host name, got 0.0.0.0:50051",
        ),
        (
            "not-an-address",
            "Invalid backend configuration: invalid DOTCORE_INFERENCE_ADDR value \"not-an-address\": \"not-an-address\" must be an IPv4 address or a DNS host name with a port, for example 10.0.0.7:7443",
        ),
        (
            "https://127.0.0.1:5173",
            "Invalid backend configuration: DOTCORE_FRONTEND_ORIGIN must be an http origin with a lowercase IPv4 address or host name and a port, got \"https://127.0.0.1:5173\"",
        ),
    ];
    for (index, (invalid, diagnostic)) in invalid_inputs.into_iter().enumerate() {
        let mut values = valid.clone();
        values[index] = Some(invalid.to_owned());
        let error = BackendConfiguration::from_values(
            values[0].clone(),
            values[1].clone(),
            values[2].clone(),
            values[3].clone(),
            values[4].clone(),
            values[5].clone(),
        )
        .expect_err("invalid input");
        assert_eq!(format!("{:#}", anyhow::Error::from(error)), diagnostic);
    }
}

#[test]
fn backend_configuration_reads_each_process_input_once() {
    let values = BTreeMap::from([
        (dotcore_domain::SITE_DIR, "/site"),
        (dotcore_domain::CATALOG_DIR, "/catalog"),
        (dotcore_domain::CACHE_DIR, "/cache"),
        (dotcore_domain::XDG_STATE_HOME, "/ignored/state"),
        (dotcore_domain::XDG_CACHE_HOME, "/ignored/cache"),
        (dotcore_domain::HOME, "/ignored/home"),
        (MANAGEMENT_ADDR_ENV, "127.0.0.1:50051"),
        (INFERENCE_ADDR_ENV, "127.0.0.1:50052"),
        (FRONTEND_ORIGIN_ENV, "http://127.0.0.1:5173"),
    ]);
    let mut reads = Vec::new();
    let configuration = BackendConfiguration::from_environment_with(|variable| {
        reads.push(variable);
        Ok(values.get(variable).map(|value| (*value).to_owned()))
    })
    .expect("Backend configuration");

    assert_eq!(
        reads,
        [
            dotcore_domain::SITE_DIR,
            dotcore_domain::CATALOG_DIR,
            dotcore_domain::CACHE_DIR,
            dotcore_domain::XDG_STATE_HOME,
            dotcore_domain::XDG_CACHE_HOME,
            dotcore_domain::HOME,
            MANAGEMENT_ADDR_ENV,
            INFERENCE_ADDR_ENV,
            FRONTEND_ORIGIN_ENV,
            PRIVATE_ADDR_ENV,
            crate::telemetry::TELEMETRY_DIR,
            DEV_SOURCE_FLAKE_ENV,
            super::configuration::PACKAGED_SOURCE_FLAKE_ENV,
        ]
    );
    assert_eq!(configuration.site_paths.root, Path::new("/site"));
    assert_eq!(
        configuration.telemetry_directory.as_deref(),
        Some(Path::new("/site/telemetry"))
    );
    let server = manager_server(&configuration);
    assert_eq!(
        server.management_bind_address,
        "127.0.0.1:50051".parse().expect("management address")
    );
    assert_eq!(
        server.inference_bind_address,
        "127.0.0.1:50052".parse().expect("inference address")
    );
    assert_eq!(
        server.private_listener,
        ManagerPrivateListener::SiteDefault { port: 50053 }
    );
    assert_eq!(
        server.allowed_origins,
        ["http://127.0.0.1:5173"
            .parse::<HeaderValue>()
            .expect("origin")]
    );
    assert_eq!(
        reads.len(),
        13,
        "using the snapshot performs no further reads"
    );
}

#[test]
fn inference_wildcard_and_missing_cors_origin_are_valid() {
    let configuration = BackendConfiguration::from_values(
        Some("/site".to_owned()),
        Some("/catalog".to_owned()),
        Some("/cache".to_owned()),
        Some("127.0.0.1:50051".to_owned()),
        Some("0.0.0.0:50052".to_owned()),
        None,
    )
    .expect("same-origin backend configuration");

    let server = manager_server(&configuration);
    assert_eq!(
        server.inference_bind_address,
        "0.0.0.0:50052".parse().expect("wildcard inference address")
    );
    assert!(server.allowed_origins.is_empty());
}

fn manager_server(configuration: &BackendConfiguration) -> &ServerConfiguration {
    match &configuration.role {
        BackendRole::Manager(server) => server,
        role => panic!("expected a manager configuration, got {role:?}"),
    }
}

#[test]
fn backend_configuration_resolves_xdg_and_home_path_branches() {
    let server_values = [
        (MANAGEMENT_ADDR_ENV, "127.0.0.1:50051"),
        (INFERENCE_ADDR_ENV, "127.0.0.1:50052"),
        (FRONTEND_ORIGIN_ENV, "http://127.0.0.1:5173"),
        (dotcore_domain::CATALOG_DIR, "/catalog"),
    ];
    let cases = [
        (
            vec![
                (dotcore_domain::XDG_STATE_HOME, "/xdg/state"),
                (dotcore_domain::XDG_CACHE_HOME, "/xdg/cache"),
                (dotcore_domain::HOME, "/home/user"),
            ],
            PathBuf::from("/xdg/state/dotcore/site"),
            PathBuf::from("/xdg/cache/dotcore"),
        ),
        (
            vec![
                (dotcore_domain::XDG_STATE_HOME, ""),
                (dotcore_domain::XDG_CACHE_HOME, ""),
                (dotcore_domain::HOME, "/home/user"),
            ],
            PathBuf::from("/home/user/.local/state/dotcore/site"),
            PathBuf::from("/home/user/.cache/dotcore"),
        ),
    ];

    for (path_values, expected_site, expected_cache) in cases {
        let values = server_values
            .into_iter()
            .chain(path_values)
            .collect::<BTreeMap<_, _>>();
        let configuration = BackendConfiguration::from_environment_with(|variable| {
            Ok(values.get(variable).map(|value| (*value).to_owned()))
        })
        .expect("resolved backend configuration");
        assert_eq!(configuration.site_paths.root, expected_site);
        assert_eq!(configuration.site_paths.cache_root, expected_cache);
    }
}

#[tokio::test]
async fn backend_composition_loads_catalog_once_for_all_capabilities() {
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../backend/fixtures/simulated-site");
    let root = dotcore_domain::test_support::canonical_temp_dir()
        .join(format!("dotcore-composition-test-{}", std::process::id()));
    fs::create_dir_all(root.join("catalog/models")).expect("Model directory");
    fs::create_dir_all(root.join("catalog/engines")).expect("Engine directory");
    fs::create_dir_all(root.join("catalog/hardware-classes")).expect("hardware class directory");
    fs::copy(
        source.join("catalog/hardware-classes/fixture-nvidia.yaml"),
        root.join("catalog/hardware-classes/fixture-nvidia.yaml"),
    )
    .expect("hardware class fixture");
    fs::copy(source.join("site.yaml"), root.join("site.yaml")).expect("Site fixture");
    fs::copy(source.join("node.yaml"), root.join("node.yaml")).expect("node fixture");
    fs::copy(
        source.join("catalog/models/fixture-model.yaml"),
        root.join("catalog/models/fixture-model.yaml"),
    )
    .expect("Model fixture");
    fs::copy(
        source.join("catalog/engines/fixture.yaml"),
        root.join("catalog/engines/fixture.yaml"),
    )
    .expect("Engine fixture");

    // Master catalog wiring: the composition loads the real master catalog once.
    let master = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../catalog");
    let configuration = SitePaths::from_values(
        Some(root.to_str().expect("Site path")),
        Some(master.to_str().expect("Master catalog path")),
        Some(root.join("cache").to_str().expect("cache path")),
    )
    .expect("configuration");
    let loads_before = catalog_load_count(&configuration);

    let (services, records, _) = capture_records_async(|| async {
        compose_capabilities(
            &configuration,
            None,
            50052,
            BackendTestSupport::test(),
            default_collector(),
        )
        .await
    })
    .await;
    let services = services.expect("backend services");

    assert_eq!(
        catalog_load_count(&configuration) - loads_before,
        1,
        "backend service composition must load one catalog for its configuration"
    );
    for event_kind in ["catalog_loaded", "site_configuration_loaded"] {
        assert!(
            records.iter().any(|record| matches!(
                record,
                Record::Event { fields, .. }
                    if fields.get("event_kind") == Some(&format!("\"{event_kind}\""))
            )),
            "missing {event_kind} event: {records:?}"
        );
    }
    assert!(configuration.model_artifact_cache_root().is_dir());
    drop(services);
    fs::remove_dir_all(root).expect("test fixture cleanup");
}

#[tokio::test]
async fn complete_composition_loads_site_once_and_retains_its_repository() {
    let (root, configuration) = complete_backend_test_configuration();
    let loads_before = site_load_count(&configuration.site_paths);

    let services = compose_capabilities(
        &configuration.site_paths,
        None,
        50052,
        BackendTestSupport::test(),
        default_collector(),
    )
    .await
    .expect("backend services");
    let model = services.model_service.controller().composition_pointers();
    let application = services
        .application_service
        .controller()
        .composition_pointers();

    assert_eq!(site_load_count(&configuration.site_paths) - loads_before, 1);
    assert_eq!(model.1, application.1);

    drop(services);
    fs::remove_dir_all(root).expect("test fixture cleanup");
}

#[tokio::test]
async fn supplied_input_model_owner_composition_does_not_reload_site() {
    let (root, configuration) = complete_backend_test_configuration();
    let inputs = complete_inputs(&configuration.site_paths, None).expect("complete inputs");
    let loads_before = site_load_count(&configuration.site_paths);

    let composed = model_owner_for(
        &inputs,
        Arc::new(StaticTargetCapabilitySource::new(
            dotcore_site::node::target_inventory::TargetCapabilityInventory::empty(
                dotcore_domain::timing::Timing::production()
                    .node_control
                    .capability_max_age,
                std::time::SystemTime::now(),
            ),
        )),
        crate::management_logs::ManagementLogAdmission::new(),
        dotcore_domain::timing::Timing::production(),
        Arc::new(dotcore_domain::clock::SystemClock),
    )
    .await
    .expect("model owner composition");

    assert_eq!(site_load_count(&configuration.site_paths), loads_before);
    drop(composed);
    drop(inputs);
    fs::remove_dir_all(root).expect("test fixture cleanup");
}

#[tokio::test]
async fn composed_model_and_application_services_share_log_admission() {
    let (root, configuration) = complete_backend_test_configuration();
    let services = compose_capabilities(
        &configuration.site_paths,
        None,
        50052,
        BackendTestSupport::test(),
        default_collector(),
    )
    .await
    .expect("backend services");
    let model_admission = services.model_service.log_admission();
    let application_admission = services.application_service.log_admission();
    let permits = (0..8)
        .map(|_| model_admission.try_acquire().expect("Model log permit"))
        .collect::<Vec<_>>();

    assert!(application_admission.try_acquire().is_err());
    drop(permits);
    assert!(application_admission.try_acquire().is_ok());

    drop(services);
    fs::remove_dir_all(root).expect("test fixture cleanup");
}

#[tokio::test]
async fn upstream_only_model_declarations_skip_failing_runtime_discovery_at_startup() {
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../backend/fixtures/simulated-site");
    let root = test_directory();
    fs::create_dir_all(root.join("catalog/models")).expect("Model directory");
    fs::create_dir_all(root.join("catalog/engines")).expect("Engine directory");
    fs::create_dir_all(root.join("catalog/hardware-classes")).expect("hardware class directory");
    fs::copy(
        source.join("catalog/hardware-classes/fixture-nvidia.yaml"),
        root.join("catalog/hardware-classes/fixture-nvidia.yaml"),
    )
    .expect("hardware class fixture");
    fs::copy(source.join("site.yaml"), root.join("site.yaml")).expect("Site fixture");
    fs::copy(source.join("node.yaml"), root.join("node.yaml")).expect("node fixture");
    fs::copy(
        source.join("catalog/models/fixture-model.yaml"),
        root.join("catalog/models/fixture-model.yaml"),
    )
    .expect("Model fixture");
    fs::copy(
        source.join("catalog/engines/fixture.yaml"),
        root.join("catalog/engines/fixture.yaml"),
    )
    .expect("Engine fixture");
    let site_path = root.join("site.yaml");
    let mut site: Site = dotcore_proto::yaml::from_file(&site_path).expect("Site fixture");
    site.execution_targets.retain(|target| matches!(target.runtime_binding, Some(dotcore_proto::dotcore::site::v1::execution_target::RuntimeBinding::ApplicationRuntime(_))));
    site.model_deployments.push(ModelDeploymentDeclaration {
        model_deployment_id: "upstream-model".to_owned(),
        desired_state: ModelDeploymentDesiredState::Present as i32,
        lifecycle_generation: 1,
        lifecycle_transition:
            dotcore_proto::dotcore::model::v1::ModelDeploymentLifecycleTransition::Create as i32,
        snapshot: Some(ModelDeploymentSnapshot {
            source: Some(ModelDeploymentSnapshotSource::Upstream(
                UpstreamModelDeploymentSnapshot {
                    upstream_model_template: Some(UpstreamModelTemplate {
                        upstream_model_template_id: "openai-compatible".to_owned(),
                        display_name: "OpenAI-compatible".to_owned(),
                        wire_protocol: UpstreamWireProtocol::OpenaiCompatible as i32,
                    }),
                },
            )),
        }),
        upstream_desired_configuration: Some(UpstreamModelDesiredConfiguration {
            upstream_model_id: "gpt-4o".to_owned(),
            api_key: "test-key".to_owned(),
            input_capabilities: Some(UpstreamModelInputCapabilities {
                supports_text: true,
                supports_image: false,
            }),
            endpoint: Some(OpenAiCompatibleEndpointConfiguration {
                base_url: "https://example.test/v1".to_owned(),
                allow_insecure_http: false,
            }),
            boundary: Some(UpstreamModelBoundary {
                locality: UpstreamModelLocality::PublicExternal as i32,
                trust_domain: "example.test".to_owned(),
                data_egress_class: UpstreamModelDataEgressClass::PublicInternet as i32,
            }),
        }),
        ..Default::default()
    });
    fs::write(
        &site_path,
        dotcore_proto::yaml::to_string(&site).expect("upstream-only site YAML"),
    )
    .expect("upstream-only site fixture");
    std::fs::set_permissions(
        &site_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .expect("owner-only upstream site fixture");
    let master = empty_master_catalog(&root);
    let configuration = SitePaths::from_values(
        Some(root.to_str().expect("Site path")),
        Some(master.to_str().expect("Master catalog path")),
        Some(root.join("cache").to_str().expect("cache path")),
    )
    .expect("configuration");
    let runtime =
        dotcore_runtime_backends::simulated::SimulatedRuntimeTestSupport::with_discovery_failures(
            1,
        );

    let services = compose_capabilities(
        &configuration,
        None,
        50052,
        BackendTestSupport {
            simulated_runtime: Some(runtime.clone()),
            ..BackendTestSupport::test()
        },
        default_collector(),
    )
    .await
    .expect("upstream-only backend services");

    assert_eq!(runtime.discovery_call_count(), 0);
    drop(services);
    fs::remove_dir_all(root).expect("test fixture cleanup");
}

#[tokio::test]
async fn model_composition_rejects_an_unknown_node_binding_before_runtime_setup() {
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../backend/fixtures/simulated-site");
    let root = test_directory();
    fs::create_dir_all(root.join("catalog/models")).expect("Model directory");
    fs::create_dir_all(root.join("catalog/engines")).expect("Engine directory");
    fs::create_dir_all(root.join("catalog/hardware-classes")).expect("hardware class directory");
    fs::copy(
        source.join("catalog/hardware-classes/fixture-nvidia.yaml"),
        root.join("catalog/hardware-classes/fixture-nvidia.yaml"),
    )
    .expect("hardware class fixture");
    fs::copy(source.join("site.yaml"), root.join("site.yaml")).expect("Site fixture");
    fs::write(root.join(NODE_FILE), "nodeId: unknown-node\n").expect("node fixture");
    fs::copy(
        source.join("catalog/models/fixture-model.yaml"),
        root.join("catalog/models/fixture-model.yaml"),
    )
    .expect("Model fixture");
    fs::copy(
        source.join("catalog/engines/fixture.yaml"),
        root.join("catalog/engines/fixture.yaml"),
    )
    .expect("Engine fixture");
    let master = empty_master_catalog(&root);
    let configuration = SitePaths::from_values(
        Some(root.to_str().expect("Site path")),
        Some(master.to_str().expect("Master catalog path")),
        Some(root.join("cache").to_str().expect("cache path")),
    )
    .expect("configuration");

    let Err(error) = model_service_for(&configuration).await else {
        panic!("composition must reject an unknown node binding");
    };
    assert!(
        format!("{:#}", anyhow::Error::from(error)).contains("does not reference a declared node")
    );

    fs::remove_dir_all(root).expect("test fixture cleanup");
}

#[test]
fn node_configuration_requires_an_exact_declared_site_node_binding() {
    let directory = test_directory();
    let site = Site {
        nodes: vec![dotcore_proto::dotcore::site::v1::SiteNode {
            node_id: "node-one".to_owned(),
            display_name: "Node One".to_owned(),
            ..Default::default()
        }],
        ..Default::default()
    };

    let missing = node_configuration_for(&directory, &site)
        .expect_err("a missing node configuration must fail");
    assert!(missing.to_string().contains("node.yaml"));

    fs::write(directory.join("node.yaml"), "nodeId: Invalid-node\n")
        .expect("invalid node configuration");
    let invalid =
        node_configuration_for(&directory, &site).expect_err("an invalid node ID must fail");
    assert!(
        format!("{:#}", anyhow::Error::from(invalid)).contains("must match [a-z][a-z0-9-]{0,62}")
    );

    fs::write(directory.join("node.yaml"), "nodeId: node-two\n")
        .expect("unknown node configuration");
    let unknown =
        node_configuration_for(&directory, &site).expect_err("an unknown node ID must fail");
    assert_eq!(unknown.to_string(), "Backend composition failed");
    let context = unknown.source().expect("node-binding context");
    assert_eq!(
        context.to_string(),
        "node.yaml nodeId \"node-two\" does not reference a declared node"
    );
    assert!(matches!(
        context.downcast_ref::<NodeBindingError>(),
        Some(NodeBindingError::MissingSiteNode { node_id }) if node_id == "node-two"
    ));

    let mut ambiguous_site = site;
    ambiguous_site.nodes.push(ambiguous_site.nodes[0].clone());
    fs::write(directory.join("node.yaml"), "nodeId: node-one\n")
        .expect("ambiguous node configuration");
    let ambiguous = node_configuration_for(&directory, &ambiguous_site)
        .expect_err("an ambiguous node binding must fail");
    assert!(format!("{:#}", anyhow::Error::from(ambiguous)).contains("must bind exactly one node"));

    fs::remove_dir_all(directory).expect("test fixture cleanup");
}

#[test]
fn backend_startup_observes_only_the_fresh_site_after_reset_releases_exclusive_lock() {
    let (root, configuration) = complete_backend_test_configuration();
    let fresh_site = fs::read(root.join("site.yaml")).expect("fresh site fixture");
    let reset_lock = SiteResetLock::try_acquire(&root).expect("exclusive site reset lock");
    fs::write(root.join("site.yaml"), "invalid while reset is incomplete")
        .expect("incomplete reset site marker");
    let site_paths = configuration.site_paths.clone();
    let loads_before = catalog_load_count(&site_paths);
    let (attempted, wait_for_attempt) = std::sync::mpsc::channel();

    let startup = std::thread::spawn(move || {
        tokio::runtime::Runtime::new()
            .expect("Backend startup runtime")
            .block_on(Backend::prepare_with_snapshot_collector_after_lock_attempt(
                configuration,
                std::future::pending(),
                default_collector(),
                move || attempted.send(()).expect("lock-attempt signal"),
            ))
    });
    wait_for_attempt
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("Backend shared-lock attempt");

    assert!(!startup.is_finished());
    assert_eq!(
        catalog_load_count(&site_paths),
        loads_before,
        "Backend must not observe catalog, site, current state, or runtime before lock release"
    );

    fs::write(root.join("site.yaml"), fresh_site).expect("publish fresh site fixture");
    drop(reset_lock);

    let backend = startup
        .join()
        .expect("Backend startup thread")
        .expect("Backend starts from fresh site");
    assert_eq!(catalog_load_count(&site_paths), loads_before + 1);
    drop(backend);
    fs::remove_dir_all(root).expect("test fixture cleanup");
}

#[tokio::test]
async fn invalid_node_input_prevents_log_writer_activation() {
    let (root, configuration) = complete_backend_test_configuration();
    fs::write(root.join("node.yaml"), "nodeId: unknown-node\n").expect("invalid node input");

    let error = match Backend::prepare(configuration, std::future::pending()).await {
        Ok(_) => panic!("invalid node input must fail preparation"),
        Err(error) => error,
    };

    assert_eq!(error.to_string(), "Backend composition failed");
    assert!(!root.join("logs").exists());
    fs::remove_dir_all(root).expect("test fixture cleanup");
}

#[tokio::test]
async fn logging_setup_failure_precedes_transport_preparation() {
    let (root, configuration) = complete_backend_test_configuration();
    fs::write(root.join("logs"), "not a directory").expect("log obstruction");

    let error = match Backend::prepare(configuration, std::future::pending()).await {
        Ok(_) => panic!("log setup must fail"),
        Err(error) => error,
    };

    assert!(matches!(error, CompositionError::Logging(_)));
    assert_eq!(error.to_string(), "Backend composition logging failure");
    fs::remove_dir_all(root).expect("test fixture cleanup");
}

#[tokio::test]
async fn backend_preparation_failure_starts_no_production_observer() {
    let (root, configuration) = complete_backend_test_configuration();
    fs::remove_file(root.join("site.yaml")).expect("remove site fixture");
    let (result, records, _) = capture_records_async(|| async {
        Backend::prepare(configuration, std::future::pending()).await
    })
    .await;

    assert!(result.is_err());
    assert!(observer_start_order(&records).is_empty());
    fs::remove_dir_all(root).expect("test fixture cleanup");
}

#[tokio::test]
async fn backend_serving_starts_production_observers_in_order_before_dispatch() {
    let (root, configuration) = complete_backend_test_configuration();
    let (result, records, _) = capture_records_async(|| async {
        let backend = Backend::prepare(configuration, std::future::ready(()))
            .await
            .expect("prepared backend");
        info!(
            operation = "backend_prepared",
            "Backend preparation complete"
        );
        backend.serve().await
    })
    .await;

    result.expect("served backend");
    let starts = observer_start_order(&records);
    assert_eq!(
        starts,
        [
            "node_reporter",
            "model",
            "node_control",
            "application",
            "route"
        ]
    );
    let prepared = records
        .iter()
        .position(|record| {
            matches!(record, Record::Event { fields, .. }
                if fields.get("operation").is_some_and(|value| value.contains("backend_prepared")))
        })
        .expect("Backend preparation marker");
    let first_start = records
        .iter()
        .position(|record| {
            matches!(record, Record::Event { fields, .. }
                if fields.get("operation").is_some_and(|value| value.contains("observer_start")))
        })
        .expect("observer start");
    assert!(
        prepared < first_start,
        "successful preparation must start no reconciliation tasks"
    );
    let dispatch = records
            .iter()
            .position(|record| {
                matches!(record, Record::SpanCreated { name, .. } if name == "backend_dispatch")
            })
            .expect("transport dispatch span");
    let route = records
        .iter()
        .rposition(|record| {
            matches!(record, Record::Event { fields, .. }
                if fields.get("observer").is_some_and(|value| value.contains("route")))
        })
        .expect("Route observer start");
    assert!(
        route < dispatch,
        "final route observer start must precede transport dispatch"
    );
    fs::remove_dir_all(root).expect("test fixture cleanup");
}

#[tokio::test]
async fn full_site_start_on_a_non_manager_node_names_the_node_folder_workflow() {
    let (root, configuration) = complete_backend_test_configuration();
    configure_non_manager_node(&root);
    let before = site_root_files(&root);

    let error = match Backend::prepare(configuration, std::future::ready(())).await {
        Ok(_) => panic!("a non-manager node must not start from a full site"),
        Err(error) => error,
    };

    assert_eq!(
        format!("{:#}", anyhow::Error::from(error)),
        "Backend composition failed: node worker-node is not the site manager and cannot \
             start from a full site; start it from its node folder after joining it to the site"
    );
    assert_eq!(
        site_root_files(&root),
        before,
        "a refused start writes nothing"
    );
    fs::remove_dir_all(root).expect("test fixture cleanup");
}

#[tokio::test]
async fn site_start_without_a_node_store_names_site_reset_and_composes_nothing() {
    let (root, configuration) = complete_backend_test_configuration();
    fs::write(root.join("state.pb"), b"owner state without a node store")
        .expect("pre-split owner state");
    let site_before = fs::read(root.join("site.yaml")).expect("Site bytes");

    let error = match Backend::prepare(configuration, std::future::ready(())).await {
        Ok(_) => panic!("a site without a node store must not start"),
        Err(error) => error,
    };

    assert_eq!(
        format!("{:#}", anyhow::Error::from(error)),
        format!(
            "Backend composition failed: site at {} has an owner state.pb without a node \
                 store (node/), a state layout this backend does not read; stop the backend and \
                 reset the site",
            root.display()
        )
    );
    assert_eq!(
        fs::read(root.join("state.pb")).expect("owner state"),
        b"owner state without a node store"
    );
    assert_eq!(fs::read(root.join("site.yaml")).expect("Site"), site_before);
    for untouched in ["node", "state.pb.lock", "logs"] {
        assert!(!root.join(untouched).exists(), "{untouched} was created");
    }
    fs::remove_dir_all(root).expect("test fixture cleanup");
}

#[tokio::test]
async fn node_folder_start_with_root_node_state_names_rejoining_and_composes_nothing() {
    let (root, mut configuration) = complete_backend_test_configuration();
    fs::remove_file(root.join("site.yaml")).expect("node folder root has no site");
    fs::write(root.join(WORKER_MANIFEST_FILE), "unvalidated node folder")
        .expect("node folder declaration");
    fs::write(root.join("state.pb"), b"worker state").expect("pre-split worker state");
    configuration.role = BackendRole::JoinedNode {
        private_bind_address: Some("127.0.0.1:0".parse().expect("private address")),
    };

    let error = match Backend::prepare(configuration, std::future::ready(())).await {
        Ok(_) => panic!("a node folder keeping node state at its root must not start"),
        Err(error) => error,
    };

    assert_eq!(
        format!("{:#}", anyhow::Error::from(error)),
        format!(
            "Backend composition failed: node folder at {} keeps node state in state.pb \
                 instead of its node store (node/), a state layout this backend does not read; \
                 remove the node folder and join the site again",
            root.display()
        )
    );
    assert_eq!(
        fs::read(root.join("state.pb")).expect("worker state"),
        b"worker state"
    );
    for untouched in ["node", "state.pb.lock", "logs"] {
        assert!(!root.join(untouched).exists(), "{untouched} was created");
    }
    fs::remove_dir_all(root).expect("test fixture cleanup");
}

#[tokio::test]
async fn complete_composition_shares_catalog_site_current_state_and_route_manager() {
    let (root, configuration) = complete_backend_test_configuration();
    let services = compose_capabilities(
        &configuration.site_paths,
        None,
        50052,
        BackendTestSupport::test(),
        default_collector(),
    )
    .await
    .expect("backend services");
    let model = services.model_service.controller().composition_pointers();
    let application = services
        .application_service
        .controller()
        .composition_pointers();
    let manager = &services.manager;
    let catalog = manager.catalog_service.catalog_pointer();
    let route_manager = manager.route_service.manager_pointer();
    let route_declarations = manager.route_service.declaration_site_state_pointer();

    assert_eq!(model.0, catalog);
    assert_eq!(application.0, catalog);
    assert_eq!(model.1, application.1);
    assert_eq!(model.1, route_declarations);
    assert_eq!(model.2, application.2);
    assert_eq!(application.3, Some(route_manager));
    assert_eq!(
        manager.inference_router.route_manager_pointer(),
        Some(route_manager)
    );

    drop(services);
    fs::remove_dir_all(root).expect("test fixture cleanup");
}
#[tokio::test]
async fn composition_rejects_a_non_manager_node() {
    let (root, configuration) = complete_backend_test_configuration();
    configure_non_manager_node(&root);
    let before = site_root_files(&root);

    let error = match compose_capabilities(
        &configuration.site_paths,
        None,
        50052,
        BackendTestSupport::test(),
        default_collector(),
    )
    .await
    {
        Ok(_) => panic!("a non-manager node must not compose full site services"),
        Err(error) => error,
    };

    assert!(matches!(
        &error,
        CompositionError::NotSiteManager { node_id } if node_id == "worker-node"
    ));
    assert_eq!(
        site_root_files(&root),
        before,
        "a refused composition writes nothing"
    );
    fs::remove_dir_all(root).expect("test fixture cleanup");
}

#[tokio::test]
async fn composed_route_manager_changes_are_shared_by_route_application_and_router_views() {
    use dotcore_proto::dotcore::{
        inference::v1::{
            CreateRouteRequest, DeleteRouteRequest, FallbackGroup, FallbackGroupSelection,
            GetRouteRequest, ListRoutesRequest, ReplaceRouteRequest,
            route_service_server::RouteService as RouteServiceContract,
        },
        model::v1::{
            CreateLocalModelDeploymentRequest, CreateModelDeploymentRequest,
            ModelDeploymentExposure, ModelDeploymentStatus, PauseModelRequest,
            model_deployment_exposure,
            model_deployment_service_server::ModelDeploymentService as ModelServiceContract,
        },
    };
    use tonic::Request;

    let (root, configuration) = complete_backend_test_configuration();
    let runtime =
        dotcore_runtime_backends::simulated::SimulatedRuntimeTestSupport::with_stop_failures(0)
            .with_lifecycle_delays(std::time::Duration::ZERO, std::time::Duration::ZERO);
    let BackendServices {
        model_service,
        application_service,
        manager,
        observers,
        node_lifecycle: _node_lifecycle,
        node: _node,
    } = compose_capabilities(
        &configuration.site_paths,
        None,
        50052,
        BackendTestSupport {
            simulated_runtime: Some(runtime),
            ..BackendTestSupport::test()
        },
        default_collector(),
    )
    .await
    .expect("backend services");
    let manager = &manager;
    let _observers = observers.start();
    let deployment = ModelServiceContract::create_model_deployment(
        &model_service,
        Request::new(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(ExactPlacement {
                            site_node_id: "fixture-node".to_owned(),
                            execution_target_id: "fixture-target".to_owned(),
                        }),
                        configuration: Some(crate::test_support::local_model_configuration()),
                    },
                ),
            ),
            exposure: Some(ModelDeploymentExposure {
                kind: Some(model_deployment_exposure::Kind::Hidden(Default::default())),
            }),
            model_deployment_name: None,
        }),
    )
    .await
    .expect("create model")
    .into_inner()
    .deployment
    .expect("created model");
    wait_for_composed_model_status(
        &model_service,
        &deployment.model_deployment_id,
        ModelDeploymentStatus::Ready,
    )
    .await;

    let created_view = RouteServiceContract::create_route(
        &manager.route_service,
        Request::new(CreateRouteRequest {
            name: "route-one".to_owned(),
            enabled: true,
            fallback_groups: vec![FallbackGroup {
                selection: FallbackGroupSelection::Single as i32,
                model_deployment_ids: vec![deployment.model_deployment_id.clone()],
            }],
        }),
    )
    .await
    .expect("create route")
    .into_inner()
    .route
    .expect("created route view");
    let created = created_view.route.clone().expect("created route");
    assert_eq!(
        RouteServiceContract::list_routes(
            &manager.route_service,
            Request::new(ListRoutesRequest {}),
        )
        .await
        .expect("list created route")
        .into_inner()
        .routes,
        vec![created_view.clone()]
    );
    assert_eq!(
        RouteServiceContract::get_route(
            &manager.route_service,
            Request::new(GetRouteRequest {
                inference_route_id: created.inference_route_id.clone(),
            }),
        )
        .await
        .expect("get created route")
        .into_inner()
        .route,
        Some(created_view.clone())
    );
    // Router targets are projected asynchronously from node status, after the model is Ready.
    wait_for_router_models(&manager.inference_router, &["route-one"]).await;
    assert_eq!(
        application_service
            .controller()
            .project_route_for_test(&created.inference_route_id),
        Ok(crate::application::ResolvedInferenceRoute {
            inference_route_id: created.inference_route_id.clone(),
            route_name: "route-one".to_owned(),
        })
    );

    let replaced_view = RouteServiceContract::replace_route(
        &manager.route_service,
        Request::new(ReplaceRouteRequest {
            inference_route_id: created.inference_route_id.clone(),
            expected_generation: created.generation,
            name: "route-two".to_owned(),
            enabled: true,
            fallback_groups: created.fallback_groups.clone(),
        }),
    )
    .await
    .expect("replace route")
    .into_inner()
    .route
    .expect("replaced route view");
    let replaced = replaced_view.route.clone().expect("replaced route");
    assert_eq!(
        RouteServiceContract::list_routes(
            &manager.route_service,
            Request::new(ListRoutesRequest {}),
        )
        .await
        .expect("list replaced route")
        .into_inner()
        .routes,
        vec![replaced_view.clone()]
    );
    assert_eq!(
        RouteServiceContract::get_route(
            &manager.route_service,
            Request::new(GetRouteRequest {
                inference_route_id: replaced.inference_route_id.clone(),
            }),
        )
        .await
        .expect("get replaced route")
        .into_inner()
        .route,
        Some(replaced_view.clone())
    );
    assert_eq!(
        manager
            .inference_router
            .available_models()
            .models
            .into_iter()
            .map(|model| model.model_id)
            .collect::<Vec<_>>(),
        vec!["route-two".to_owned()]
    );
    assert_eq!(
        application_service
            .controller()
            .project_route_for_test(&replaced.inference_route_id),
        Ok(crate::application::ResolvedInferenceRoute {
            inference_route_id: replaced.inference_route_id.clone(),
            route_name: "route-two".to_owned(),
        })
    );

    let (_, records, _) = capture_records_async(|| async {
        ModelServiceContract::pause_model(
            &model_service,
            Request::new(PauseModelRequest {
                model_deployment_id: deployment.model_deployment_id.clone(),
            }),
        )
        .await
        .expect("pause model");
    })
    .await;
    assert!(records.iter().any(|record| matches!(
        record,
        Record::Event { level, fields }
            if level == "INFO"
                && fields.get("event_kind") == Some(&"\"route_availability_changed\"".to_owned())
                && fields.get("route_name") == Some(&"route-two".to_owned())
                && fields.get("available") == Some(&"false".to_owned())
                && fields.get("previous_available") == Some(&"true".to_owned())
    )));

    let (_, repeated_records, _) = capture_records_async(|| async {
        ModelServiceContract::pause_model(
            &model_service,
            Request::new(PauseModelRequest {
                model_deployment_id: deployment.model_deployment_id.clone(),
            }),
        )
        .await
        .expect("repeat pause model");
    })
    .await;
    assert!(!repeated_records.iter().any(|record| matches!(
        record,
        Record::Event { fields, .. }
            if fields.get("event_kind") == Some(&"\"route_availability_changed\"".to_owned())
    )));

    RouteServiceContract::delete_route(
        &manager.route_service,
        Request::new(DeleteRouteRequest {
            inference_route_id: replaced.inference_route_id.clone(),
            expected_generation: replaced.generation,
        }),
    )
    .await
    .expect("delete route");
    assert_eq!(
        RouteServiceContract::list_routes(
            &manager.route_service,
            Request::new(ListRoutesRequest {}),
        )
        .await
        .expect("list deleted route")
        .into_inner()
        .routes,
        Vec::new()
    );
    let deleted = RouteServiceContract::get_route(
        &manager.route_service,
        Request::new(GetRouteRequest {
            inference_route_id: replaced.inference_route_id.clone(),
        }),
    )
    .await
    .expect_err("deleted route must not be available");
    assert_eq!(deleted.code(), tonic::Code::NotFound);
    assert_eq!(deleted.message(), "Inference route was not found");
    assert_eq!(
        manager
            .inference_router
            .available_models()
            .models
            .into_iter()
            .map(|model| model.model_id)
            .collect::<Vec<_>>(),
        Vec::<String>::new()
    );
    assert_eq!(
        application_service
            .controller()
            .project_route_for_test(&replaced.inference_route_id),
        Err(crate::application::InferenceRouteResolutionError::Unknown)
    );

    drop((model_service, application_service, _observers, _node));
    fs::remove_dir_all(root).expect("test fixture cleanup");
}

#[test]
fn the_manager_node_installs_only_its_own_execution_targets() {
    let target = |execution_target_id: &str, site_node_id: &str| {
        dotcore_proto::dotcore::site::v1::ExecutionTarget {
            execution_target_id: execution_target_id.to_owned(),
            site_node_id: site_node_id.to_owned(),
            ..Default::default()
        }
    };
    let site = dotcore_proto::dotcore::site::v1::Site {
        execution_targets: vec![
            target("manager-models", "manager-node"),
            target("worker-applications", "worker-one"),
            target("manager-applications", "manager-node"),
        ],
        ..Default::default()
    };
    let bound_node = BoundSiteNode {
        node_id: dotcore_domain::SiteNodeId::for_test("manager-node"),
        declaration: dotcore_proto::dotcore::site::v1::SiteNode {
            node_id: "manager-node".to_owned(),
            ..Default::default()
        },
        declared_machine: None,
    };

    assert_eq!(
        super::manager_node_targets(&site, &bound_node)
            .iter()
            .map(|target| target.execution_target_id.as_str())
            .collect::<Vec<_>>(),
        ["manager-models", "manager-applications"]
    );
}

#[tokio::test]
async fn isolated_service_helpers_compose_independent_roots_with_the_site_route_manager() {
    let (model_root, model_configuration) = complete_backend_test_configuration();
    let (application_root, application_configuration) = complete_backend_test_configuration();
    let (catalog_root, catalog_configuration) = complete_backend_test_configuration();

    let model = model_service_for(&model_configuration.site_paths)
        .await
        .expect("isolated model service");
    let application = application_service_for(&application_configuration.site_paths, 50052)
        .await
        .expect("isolated application service");
    let catalog =
        catalog_service_for(&catalog_configuration.site_paths).expect("isolated catalog service");
    let model_catalog = model.controller().composition_pointers().0;
    let application_pointers = application.controller().composition_pointers();

    assert_ne!(model_catalog, application_pointers.0);
    assert_ne!(model_catalog, catalog.catalog_pointer());
    assert_ne!(application_pointers.0, catalog.catalog_pointer());
    assert!(
        application_pointers.3.is_some(),
        "isolated application composition projects linked routes from the site's route manager"
    );
    assert_eq!(
        application
            .controller()
            .project_route_for_test("isolated-route"),
        Err(crate::application::InferenceRouteResolutionError::Unknown)
    );

    drop((model, application, catalog));
    for root in [model_root, application_root, catalog_root] {
        fs::remove_dir_all(root).expect("test fixture cleanup");
    }
}

#[tokio::test]
async fn late_current_state_failure_preserves_typed_error_and_failure_telemetry() {
    let (root, configuration) = complete_backend_test_configuration();
    fs::create_dir(root.join("node")).expect("node store");
    fs::write(root.join("state.pb"), [0xff, 0xff]).expect("invalid current state");

    let (result, records, _) = capture_records_async(|| async {
        compose_capabilities(
            &configuration.site_paths,
            None,
            50052,
            BackendTestSupport::test(),
            default_collector(),
        )
        .await
    })
    .await;
    let Err(error) = result else {
        panic!("invalid current state must fail composition");
    };

    let CompositionError::CurrentState(crate::model::state::CurrentStateError::Decode(source)) =
        &error
    else {
        panic!("current-state decode error");
    };
    assert_eq!(
        source.to_string(),
        "failed to decode Protobuf message: invalid varint"
    );
    assert_eq!(
        backend_composition_close_fields(&records),
        &BTreeMap::from([
            ("error_kind".to_owned(), "\"composition\"".to_owned()),
            ("operation".to_owned(), "\"backend_composition\"".to_owned()),
            ("outcome".to_owned(), "\"failed\"".to_owned()),
            ("execution_target_count".to_owned(), "<empty>".to_owned()),
        ])
    );
    fs::remove_dir_all(root).expect("test fixture cleanup");
}

#[tokio::test]
async fn backend_composition_records_exact_success_and_early_failure_fields() {
    let (root, configuration) = complete_backend_test_configuration();
    let (result, records, _) = capture_records_async(|| async {
        compose_capabilities(
            &configuration.site_paths,
            None,
            50052,
            BackendTestSupport::test(),
            default_collector(),
        )
        .await
    })
    .await;
    result.expect("backend services");
    assert_eq!(
        backend_composition_close_fields(&records),
        &BTreeMap::from([
            ("error_kind".to_owned(), "<empty>".to_owned()),
            ("operation".to_owned(), "\"backend_composition\"".to_owned()),
            ("outcome".to_owned(), "\"success\"".to_owned()),
            ("execution_target_count".to_owned(), "2".to_owned()),
        ])
    );
    fs::remove_dir_all(&root).expect("test fixture cleanup");

    let (failure_root, failure_configuration) = complete_backend_test_configuration();
    fs::remove_file(failure_root.join(SITE_FILE)).expect("remove site fixture");
    let (result, records, _) = capture_records_async(|| async {
        compose_capabilities(
            &failure_configuration.site_paths,
            None,
            50052,
            BackendTestSupport::test(),
            default_collector(),
        )
        .await
    })
    .await;
    assert!(result.is_err());
    assert_eq!(
        backend_composition_close_fields(&records),
        &BTreeMap::from([
            ("error_kind".to_owned(), "\"composition\"".to_owned()),
            ("operation".to_owned(), "\"backend_composition\"".to_owned()),
            ("outcome".to_owned(), "\"failed\"".to_owned()),
            ("execution_target_count".to_owned(), "<empty>".to_owned()),
        ])
    );
    fs::remove_dir_all(failure_root).expect("test fixture cleanup");
}

#[tokio::test]
async fn prepared_backend_retains_and_releases_the_site_input_lock() {
    let (root, configuration) = complete_backend_test_configuration();
    let backend = Backend::prepare(configuration, std::future::pending())
        .await
        .expect("prepared backend");

    assert!(matches!(
        SiteResetLock::try_acquire(&root),
        Err(SiteResetLockError::SiteRunning { .. })
    ));
    drop(backend);
    SiteResetLock::try_acquire(&root).expect("dropped backend releases site lock");
    fs::remove_dir_all(root).expect("test fixture cleanup");
}

#[test]
fn composition_preserves_typed_runtime_failures() {
    let error = CompositionError::from(crate::dispatch::model::RuntimeTargetError::Adapter {
        execution_target_id: "target".into(),
        source: dotcore_runtime::RuntimeBackendError::InvalidLaunch(anyhow!(
            "injected runtime launch failure"
        )),
    });

    assert!(matches!(
        &error,
        CompositionError::Runtime(crate::dispatch::model::RuntimeTargetError::Adapter { source, .. })
            if matches!(source, dotcore_runtime::RuntimeBackendError::InvalidLaunch(_))
    ));
    let runtime_target = error
        .source()
        .expect("Runtime target source")
        .downcast_ref::<crate::dispatch::model::RuntimeTargetError>()
        .expect("typed runtime target source");
    let runtime_backend = runtime_target
        .source()
        .expect("Runtime backend source")
        .downcast_ref::<dotcore_runtime::RuntimeBackendError>()
        .expect("typed runtime backend source");
    assert_eq!(
        runtime_backend
            .source()
            .expect("Runtime leaf source")
            .to_string(),
        "injected runtime launch failure"
    );
}

fn test_directory() -> PathBuf {
    let path = dotcore_domain::test_support::canonical_temp_dir().join(format!(
        "dotcore-composition-test-{}",
        dotcore_domain::test_support::unique_suffix()
    ));
    fs::create_dir(&path).expect("fixture directory");
    path
}

fn complete_backend_test_configuration() -> (PathBuf, BackendConfiguration) {
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../backend/fixtures/simulated-site");
    let root = test_directory();
    fs::create_dir_all(root.join("catalog/models")).expect("Model directory");
    fs::create_dir_all(root.join("catalog/engines")).expect("Engine directory");
    fs::create_dir_all(root.join("catalog/hardware-classes")).expect("hardware class directory");
    fs::copy(
        source.join("catalog/hardware-classes/fixture-nvidia.yaml"),
        root.join("catalog/hardware-classes/fixture-nvidia.yaml"),
    )
    .expect("hardware class fixture");
    for file in [
        "site.yaml",
        "node.yaml",
        "catalog/models/fixture-model.yaml",
        "catalog/engines/fixture.yaml",
    ] {
        fs::copy(source.join(file), root.join(file)).expect("Backend fixture file");
    }
    let configuration = BackendConfiguration {
        site_paths: backend_test_site_paths(&root),
        telemetry_directory: None,
        dotcore_sources: Default::default(),
        role: BackendRole::Manager(ServerConfiguration {
            management_bind_address: "127.0.0.1:0".parse().expect("management address"),
            inference_bind_address: "127.0.0.1:0".parse().expect("inference address"),
            private_listener: ManagerPrivateListener::SiteDefault {
                port: DEFAULT_PRIVATE_PORT,
            },
            allowed_origins: vec!["http://127.0.0.1:5173".parse().expect("origin")],
        }),
    };
    (root, configuration)
}

fn configure_non_manager_node(root: &Path) {
    let site_path = root.join("site.yaml");
    let mut site: Site = dotcore_proto::yaml::from_file(&site_path).expect("Site fixture");
    site.nodes.push(SiteNode {
        node_id: "worker-node".to_owned(),
        host: "127.0.0.2".to_owned(),
        display_name: "Worker node".to_owned(),
        ..Default::default()
    });
    fs::write(
        site_path,
        dotcore_proto::yaml::to_string(&site).expect("two-node site YAML"),
    )
    .expect("node site fixture");
    fs::write(root.join(NODE_FILE), "nodeId: worker-node\n").expect("worker node fixture");
}

/// Every file and directory under a site root, with file contents.
fn site_root_files(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).expect("Site root entries") {
            let path = entry.expect("Site root entry").path();
            if path.is_dir() {
                pending.push(path.clone());
                files.insert(path, None);
            } else {
                let contents = fs::read(&path).expect("Site root file");
                files.insert(path, Some(contents));
            }
        }
    }
    files
}

fn backend_test_site_paths(root: &Path) -> SitePaths {
    let master = empty_master_catalog(root);
    SitePaths::from_values(
        Some(root.to_str().expect("Site path")),
        Some(master.to_str().expect("Catalog path")),
        Some(root.join("cache").to_str().expect("cache path")),
    )
    .expect("Site paths")
}

/// Creates an empty master catalog under the site root, for tests whose declarations all live in
/// the site catalog.
fn empty_master_catalog(root: &Path) -> PathBuf {
    let master = root.join("master-catalog");
    dotcore_catalog::test_support::create_empty_catalog_source(&master);
    master
}

fn backend_composition_close_fields(records: &[Record]) -> &BTreeMap<String, String> {
    let fields = records
        .iter()
        .filter_map(|record| match record {
            Record::SpanClosed { name, fields } if name == "backend_composition" => Some(fields),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(fields.len(), 1, "{records:#?}");
    fields[0]
}

fn observer_start_order(records: &[Record]) -> Vec<&str> {
    records
        .iter()
        .filter_map(|record| match record {
            Record::Event { fields, .. }
                if fields
                    .get("operation")
                    .is_some_and(|value| value.contains("observer_start")) =>
            {
                fields.get("observer").map(String::as_str)
            }
            _ => None,
        })
        .map(|observer| observer.trim_matches('"'))
        .collect()
}

async fn wait_for_composed_model_status(
    service: &SelectedModelService,
    deployment_id: &str,
    expected: dotcore_proto::dotcore::model::v1::ModelDeploymentStatus,
) {
    use dotcore_proto::dotcore::model::v1::{
        ListModelDeploymentsRequest,
        model_deployment_service_server::ModelDeploymentService as ModelServiceContract,
    };

    let started = tokio::time::Instant::now();
    let mut last_state = None;
    let mut observer_error = None;
    loop {
        match ModelServiceContract::list_model_deployments(
            service,
            tonic::Request::new(ListModelDeploymentsRequest {}),
        )
        .await
        {
            Ok(response) => {
                if let Some(state) = response
                    .into_inner()
                    .deployments
                    .into_iter()
                    .find(|candidate| candidate.model_deployment_id == deployment_id)
                    .and_then(|candidate| candidate.state)
                {
                    if state.status == expected as i32 {
                        return;
                    }
                    last_state = Some((state.status, state.status_message));
                }
            }
            Err(error) => observer_error = Some(error),
        }

        if started.elapsed() >= crate::test_support::DIAGNOSTIC_DEADLOCK_WATCHDOG {
            panic!(
                "model convergence deadlock watchdog: expected={expected:?}, last_status_and_message={last_state:?}, observer_error={observer_error:?}"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

async fn wait_for_router_models(router: &InferenceRouter, expected: &[&str]) {
    let started = tokio::time::Instant::now();
    loop {
        let models = router
            .available_models()
            .models
            .into_iter()
            .map(|model| model.model_id)
            .collect::<Vec<_>>();
        if models == expected {
            return;
        }

        if started.elapsed() >= crate::test_support::DIAGNOSTIC_DEADLOCK_WATCHDOG {
            panic!(
                "router targets deadlock watchdog: expected_models={expected:?}, last_models={models:?}"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

fn catalog_load_count(configuration: &SitePaths) -> usize {
    CATALOG_LOAD_COUNTS
        .lock()
        .expect("Catalog load count lock")
        .get(&configuration.root)
        .copied()
        .unwrap_or_default()
}

fn site_load_count(configuration: &SitePaths) -> usize {
    SITE_LOAD_COUNTS
        .lock()
        .expect("Site load count lock")
        .get(&configuration.root)
        .copied()
        .unwrap_or_default()
}

fn bound_node_for_host_tests() -> BoundSiteNode {
    BoundSiteNode {
        node_id: dotcore_domain::SiteNodeId::for_test("manager-node"),
        declaration: dotcore_proto::dotcore::site::v1::SiteNode {
            node_id: "manager-node".to_owned(),
            ..Default::default()
        },
        declared_machine: None,
    }
}

#[tokio::test]
async fn unresolvable_plain_node_host_names_the_network_and_the_reset_remedy() {
    let host = dotcore_domain::SiteNodeHost::parse("gb10-host.example.com").expect("host");
    let resolver = dotcore_domain::test_support::host_resolver::FakeHostResolver::default();

    let error = super::publication_host_for(&bound_node_for_host_tests(), &host, &resolver)
        .await
        .expect_err("unresolvable host");

    assert!(
        matches!(
            &error,
            CompositionError::UnresolvableNodeHost { host, .. } if host.to_string() == "gb10-host.example.com"
        ),
        "expected an unresolvable node host, got {error:?}"
    );
    assert_eq!(
        error.to_string(),
        "cannot resolve the node host gb10-host.example.com; start the network that provides it \
         and start the server again, or reset the site with another node host"
    );
}
