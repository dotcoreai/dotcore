//! Shared support for backend unit tests.

use dotcore_proto::dotcore::{
    deployment::v1::ArtifactPreparationPolicy,
    model::v1::{ModelDeploymentSnapshot, model_deployment_snapshot},
};

pub(crate) mod http;
pub(crate) mod ninfer;
pub(crate) mod nix;
pub(crate) mod strata;

pub(crate) use dotcore_domain::test_support::{
    canonical_tempdir, child,
    model::{local_model_configuration, local_model_snapshot},
};
pub(crate) use dotcore_site::test_support::{identity_allocator, private_tls};
use dotcore_site::{
    node::target_inventory::{
        NodeAcceleratorObservation, RuntimePlatformObservation, TargetCapabilityInventory,
        TargetCapabilityReconciler,
    },
    validation::SiteDeclarationRules,
};

pub(crate) const DIAGNOSTIC_DEADLOCK_WATCHDOG: std::time::Duration =
    std::time::Duration::from_secs(10);

/// Serializes unit tests whose child processes have tight startup deadlines with tests that run
/// the host `nix`, whose evaluation load would otherwise delay those children.
pub(crate) fn host_process_load_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Unsigned engine parameter values for the given parameter and value pairs, in order.
pub(crate) fn unsigned_parameter_values<'a>(
    values: impl IntoIterator<Item = (&'a str, u64)>,
) -> dotcore_proto::dotcore::catalog::v1::ParameterValues {
    use dotcore_proto::dotcore::catalog::v1::{
        ParameterValue, ParameterValues, parameter_value::Value,
    };

    ParameterValues {
        values: values
            .into_iter()
            .map(|(parameter_id, value)| ParameterValue {
                parameter_id: parameter_id.to_owned(),
                value: Some(Value::UnsignedInteger(value)),
            })
            .collect(),
    }
}

/// Model runtime binding of a Docker-bound target, which realizes OCI artifacts.
pub(crate) fn docker_model_runtime_binding()
-> dotcore_proto::dotcore::site::v1::execution_target::RuntimeBinding {
    use dotcore_proto::dotcore::site::v1::{
        ModelRuntimeBinding, execution_target::RuntimeBinding, model_runtime_binding::Adapter,
    };

    RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
        adapter: Some(Adapter::DockerRuntime(Default::default())),
    })
}

pub(crate) fn execution_target_capabilities(
    platform: dotcore_domain::ExecutionPlatform,
) -> (
    dotcore_proto::dotcore::site::v1::ExecutionTarget,
    TargetCapabilityInventory,
) {
    execution_target_capabilities_with_memory(
        platform,
        dotcore_domain::MemoryArchitecture::Dedicated,
    )
}

pub(crate) fn execution_target_capabilities_with_memory(
    platform: dotcore_domain::ExecutionPlatform,
    memory_architecture: dotcore_domain::MemoryArchitecture,
) -> (
    dotcore_proto::dotcore::site::v1::ExecutionTarget,
    TargetCapabilityInventory,
) {
    let target_id = "fixture-target";
    let node_id = "fixture-node";
    let target = dotcore_proto::dotcore::site::v1::ExecutionTarget {
        execution_target_id: target_id.to_owned(),
        display_name: target_id.to_owned(),
        site_node_id: node_id.to_owned(),
        accelerator_device_ids: vec!["0".to_owned()],
        platform: Some(platform.to_oci_platform()),
        runtime_binding: Some(docker_model_runtime_binding()),
    };
    let accelerator = NodeAcceleratorObservation::nvidia(node_id, "580.173.02")
        .with_memory_architecture(Some(memory_architecture));
    let accelerator = match memory_architecture {
        dotcore_domain::MemoryArchitecture::Dedicated => accelerator
            .with_device_memory("GPU-0", Some(34_190_917_632), None)
            .with_host_memory(68_719_476_736, 68_719_476_736),
        dotcore_domain::MemoryArchitecture::Unified => {
            accelerator.with_host_memory(34_190_917_632, 34_190_917_632)
        }
    };
    let inventory = TargetCapabilityReconciler::reconcile_now(
        [&target],
        [RuntimePlatformObservation::available(target_id, platform)],
        [accelerator],
    );
    (target, inventory)
}

/// Rendered reference of the engine image in [`tag_only_engine_catalog`].
pub(crate) const TAG_ONLY_ENGINE_REFERENCE: &str = "registry.dotcore.test/fixture:dev";

/// Loads the simulated-site fixture catalog with its engine image declared by tag only, the
/// shape of a locally built development image.
pub(crate) fn tag_only_engine_catalog() -> dotcore_catalog::Catalog {
    let digest = r#"digest: { algorithm: "sha256", encoded: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" }"#;
    fixture_engine_catalog(|engine| {
        assert!(engine.contains(digest), "fixture engine is digest-pinned");
        engine.replace(digest, r#"tag: "dev""#)
    })
}

/// The simulated-site fixture catalog with its engine artifact rewritten by `engine`, which
/// receives the engine declaration YAML.
pub(crate) fn fixture_engine_catalog(
    engine: impl FnOnce(String) -> String,
) -> dotcore_catalog::Catalog {
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/simulated-site/catalog");
    let root = tempfile::tempdir().expect("Catalog root");
    for directory in ["engines", "models", "hardware-classes"] {
        std::fs::create_dir_all(root.path().join(directory)).expect("Catalog directory");
    }
    std::fs::copy(
        fixture.join("hardware-classes/fixture-nvidia.yaml"),
        root.path().join("hardware-classes/fixture-nvidia.yaml"),
    )
    .expect("fixture hardware class");
    std::fs::copy(
        fixture.join("models/fixture-model.yaml"),
        root.path().join("models/fixture-model.yaml"),
    )
    .expect("fixture model");
    let declaration =
        std::fs::read_to_string(fixture.join("engines/fixture.yaml")).expect("fixture engine");
    std::fs::write(
        root.path().join("engines/fixture.yaml"),
        engine(declaration),
    )
    .expect("fixture engine");
    dotcore_catalog::Catalog::load_with_policy(
        root.path(),
        dotcore_catalog::OciArtifactPolicy::Development,
        crate::model::engine::declaration_validator(),
    )
    .expect("fixture engine catalog")
}

/// Synthetic platform catalog holding only the OpenAI-compatible upstream template, for tests of
/// upstream model deployments that never touch local models.
pub(crate) fn upstream_template_catalog() -> dotcore_catalog::Catalog {
    dotcore_catalog::CatalogBuilder::new()
        .upstream_model_template(
            "openai-compatible",
            dotcore_catalog::test_support::openai_compatible_template(),
        )
        .build()
        .expect("upstream template catalog")
}

/// Returns the fixture local snapshot with its engine image declared by tag only, as a
/// local-development image, retaining `launch_identity` (empty when its hosting node resolves it).
pub(crate) fn tag_only_engine_snapshot(launch_identity: &str) -> ModelDeploymentSnapshot {
    let mut snapshot = local_model_snapshot("fixture-model");
    retain_tag_only_engine(&mut snapshot, launch_identity);
    snapshot
}

/// Declares the snapshot's engine image by the tag [`TAG_ONLY_ENGINE_REFERENCE`], retaining
/// `launch_identity` under the local-development policy.
pub(crate) fn retain_tag_only_engine(
    snapshot: &mut ModelDeploymentSnapshot,
    launch_identity: &str,
) {
    let Some(model_deployment_snapshot::Source::Local(local)) = snapshot.source.as_mut() else {
        panic!("tag-only engine requires a local snapshot");
    };
    let engine = local.engine_artifact.as_mut().expect("snapshot engine");
    let reference = engine
        .artifact
        .as_mut()
        .and_then(dotcore_domain::test_support::oci::reference_mut)
        .expect("snapshot engine reference");
    reference.tag = Some("dev".to_owned());
    reference.digest = None;
    engine.preparation_policy = ArtifactPreparationPolicy::LocalDevelopment as i32;
    engine.launch_identity = launch_identity.to_owned();
}

/// A single-node site with one local model deployment per id in `deployment_ids`, which routes
/// may name as members.
pub(crate) fn routed_site(deployment_ids: &[&str]) -> dotcore_proto::dotcore::site::v1::Site {
    use dotcore_proto::dotcore::{
        catalog::v1::{ModelParallelShape, OciPlatform},
        deployment::v1::ExactPlacement,
        model::v1::{
            ModelDeploymentDeclaration, ModelDeploymentDesiredState,
            ModelDeploymentLifecycleTransition,
        },
        site::v1::{
            ExecutionTarget, ModelDeploymentIdentityAllocator, ModelRuntimeBinding,
            SimulatedRuntimeAdapter, SimulatedRuntimeScenario, Site, SiteManagerRole, SiteNode,
            execution_target, model_runtime_binding,
        },
    };

    let model = |model_deployment_id: &str| ModelDeploymentDeclaration {
        model_deployment_id: model_deployment_id.to_owned(),
        model_deployment_name: model_deployment_id.to_owned(),
        placement: Some(ExactPlacement {
            site_node_id: "node-one".to_owned(),
            execution_target_id: "model-target".to_owned(),
        }),
        requested_parallel_shape: ModelParallelShape::One as i32,
        snapshot: Some(local_model_snapshot(model_deployment_id)),
        desired_state: ModelDeploymentDesiredState::Present as i32,
        lifecycle_generation: 1,
        lifecycle_transition: ModelDeploymentLifecycleTransition::Create as i32,
        ..Default::default()
    };
    Site {
        site_id: "site-one".to_owned(),
        nodes: vec![SiteNode {
            node_id: "node-one".to_owned(),
            host: "127.0.0.1".to_owned(),
            display_name: "Node one".to_owned(),
            site_manager: Some(SiteManagerRole {}),
            manager_host: String::new(),
        }],
        model_deployment_identity_allocator: Some(ModelDeploymentIdentityAllocator {
            namespace: "0123456789abcdef".to_owned(),
            next_sequence: 3,
        }),
        execution_targets: vec![ExecutionTarget {
            execution_target_id: "model-target".to_owned(),
            display_name: "model-target".to_owned(),
            site_node_id: "node-one".to_owned(),
            platform: Some(OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                variant: None,
            }),
            runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                ModelRuntimeBinding {
                    adapter: Some(model_runtime_binding::Adapter::SimulatedRuntime(
                        SimulatedRuntimeAdapter {
                            default_scenario: Some(SimulatedRuntimeScenario::default()),
                            deployment_scenarios: Vec::new(),
                        },
                    )),
                },
            )),
            accelerator_device_ids: vec!["GPU-00000000-0000-0000-0000-000000005090".to_owned()],
        }],
        model_deployments: deployment_ids.iter().map(|id| model(id)).collect(),
        ..Default::default()
    }
}

/// The owners' site declaration rules, for unit tests that load or validate a site.
pub(crate) fn site_declaration_rules() -> SiteDeclarationRules {
    crate::model::site_declaration_rules()
}

/// A secret generator whose values tests can predict: the `n`th call returns `secret<n>` padded
/// with `x` to the requested length.
pub(crate) fn test_secret_generator() -> std::sync::Arc<dyn crate::SecretGenerator> {
    std::sync::Arc::new(SequentialSecretGenerator::default())
}

/// The `n`th value [`test_secret_generator`] returns at `length`, counting from 1.
pub(crate) fn sequential_secret(n: usize, length: usize) -> String {
    let mut value = format!("secret{n}");
    value.extend(std::iter::repeat_n('x', length.saturating_sub(value.len())));
    value.truncate(length);
    value
}

#[derive(Default)]
struct SequentialSecretGenerator {
    calls: std::sync::atomic::AtomicUsize,
}

impl crate::SecretGenerator for SequentialSecretGenerator {
    fn generate(&self, length: usize) -> anyhow::Result<String> {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        Ok(sequential_secret(n, length))
    }
}
