//! Public behavior contracts for loading and resolving catalog declarations.
//!
//! Repository coverage applies generic invariants to every declaration. Focused
//! validation and service contracts use the smallest available fixture instead
//! of duplicating the contents of committed catalog YAML.

use std::{collections::BTreeSet, path::Path, sync::Arc};

use dotcore::model::{
    resolution::{
        EngineArtifactSelection, EngineLaunchIdentity, ModelLaunchResolutionError,
        resolve_local_launch, resolve_upstream_snapshot, select_engine_artifact,
    },
    resolved::{ResolvedEngineSource, ResolvedModelLaunch},
};
use dotcore_catalog::{
    Catalog, CatalogBuilder, CatalogModelId,
    service::CatalogService,
    test_support::{
        ACCEPT_ENGINE_DECLARATIONS, SIZING_ENGINE_CACHE_DEFAULT, create_empty_catalog_source,
        temporary_catalog_root, vendor_hardware_class, write_sizing_engine,
        write_sizing_model_with,
    },
};
use dotcore_domain::{ExecutionPlatform, MemoryArchitecture};
use dotcore_proto::dotcore::catalog::v1::catalog_service_server::CatalogService as CatalogServiceContract;
use dotcore_proto::dotcore::{
    catalog::v1::{
        EngineArtifactReference, EngineRelease, HardwareSupport, HuggingFaceSnapshotArtifact,
        HuggingFaceSnapshotFile, IntelligenceScore, License, ListEngineReleasesRequest,
        ListModelsRequest, ListUpstreamModelTemplatesRequest, Model, NixSystemStorePath,
        OciPlatform, Selector, UpstreamModelTemplate, UpstreamWireProtocol,
        engine_artifact::Kind as EngineArtifactKind, engine_parameter_value,
        hugging_face_artifact_source::Content as HuggingFaceContent,
    },
    deployment::v1::ArtifactPreparationPolicy,
    engine::v1::EngineKind,
    model::v1::{
        LocalModelDeploymentConfiguration, ModelDeploymentSnapshot,
        ResolvedLocalModelDeploymentSnapshot,
        model_deployment_snapshot::Source as ModelDeploymentSnapshotSource,
        portable_model_launch_argument_token::Value as PortableArgument,
    },
    site::v1::ExecutionTarget,
};
use dotcore_site::node::target_inventory::{
    NodeAcceleratorFacts, NodeAcceleratorObservation, RuntimePlatformObservation,
    TargetCapabilityInventory, TargetCapabilityReconciler,
};
use tonic::Request;

/// Engines whose releases build for one Linux platform only. Every other repository model
/// configuration that declares a Linux platform must resolve on both linux/amd64 and linux/arm64.
const SINGLE_PLATFORM_ENGINES: [SinglePlatformEngine; 1] = [SinglePlatformEngine {
    engine_kind: EngineKind::Ninfer,
    platform: ExecutionPlatform::linux_amd64(),
    reason: "ninfer compiles only for sm_120a (RTX 5090), and no arm64 RTX 5090 host exists",
}];

/// Host memory pool of a 32 GiB unified-memory node, used by the GB10 and Docker Desktop targets.
const HOST_POOL_BYTES: u64 = 34_190_917_632;

/// Placeholder local image ID standing in for the node-local identity of a tag-only engine image.
const PLACEHOLDER_IMAGE_ID: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000001";

/// Placeholder store path standing in for the precomputed store path of a locked Nix engine
/// artifact.
const PLACEHOLDER_STORE_PATH: &str = "/nix/store/00000000000000000000000000000001-placeholder";

/// Locked GitHub flake reference used by synthetic Nix artifacts.
const LOCKED_FLAKE: &str = "github:dotcoreai/dotcore/0123456789abcdef0123456789abcdef01234567";

#[test]
fn repository_catalog_profiles_resolve_their_declared_defaults() {
    let catalog = repository_catalog();
    let models = catalog.list_model_releases();
    assert!(
        !models.is_empty(),
        "repository catalog must declare at least one model"
    );
    let releases = catalog.list_engine_releases();
    for entry in &SINGLE_PLATFORM_ENGINES {
        assert!(
            releases
                .iter()
                .any(|release| release.engine_kind == entry.engine_kind as i32),
            "single-platform allowlist entry {:?} names no repository engine",
            entry.engine_kind
        );
    }
    let mut resolved_profile_count = 0;

    for platform in [
        ExecutionPlatform::linux_amd64(),
        ExecutionPlatform::linux_arm64(),
        ExecutionPlatform::darwin_arm64(),
    ] {
        let (target, inventory) = target_capabilities(platform.clone());
        for model in &models {
            let model_id = CatalogModelId(model.model_id.clone());
            let mut resolved_configuration_ids = BTreeSet::new();

            for profile in catalog.profiles_for_model(&model_id) {
                let (binding_id, defaults) = catalog
                    .local_defaults_for(&model_id, &profile.profile_id)
                    .expect("validated profile defaults");
                assert_eq!(binding_id, profile.default_model_artifact_binding_id);
                let deployment_configuration = LocalModelDeploymentConfiguration {
                    model_artifact_binding_id: binding_id.clone(),
                    engine_parameter_values: Some(defaults.clone()),
                    ..Default::default()
                };

                // A profile must not resolve on a platform its approved artifacts do not declare.
                if !configuration_platforms(&releases, model, &profile.configuration_id)
                    .contains(&platform)
                {
                    assert!(
                        select_engine_artifact(
                            &catalog,
                            &model_id,
                            &profile.profile_id,
                            &deployment_configuration,
                            &target,
                            &inventory,
                        )
                        .is_err(),
                        "profile {:?} of {:?} resolves on {platform:?}, which its approved artifacts do not declare",
                        profile.profile_id,
                        model.model_id,
                    );
                    continue;
                }

                resolved_profile_count += 1;
                let local = resolve_local_launch_with_placeholder_identity(
                    &catalog,
                    &model_id,
                    &profile.profile_id,
                    &deployment_configuration,
                    &target,
                    &inventory,
                )
                .expect("resolved default snapshot");

                resolved_configuration_ids.insert(local.configuration_id.clone());
                assert_eq!(local.model_id, model.model_id);
                assert_eq!(local.configuration_id, profile.configuration_id);
                assert_eq!(local.profile_id, profile.profile_id);
                assert_eq!(
                    local
                        .model_artifact
                        .as_ref()
                        .map(|artifact| artifact.artifact_id.as_str()),
                    Some(binding_id.as_str())
                );
                // Resolved values are the profile defaults plus the values sizing derived.
                let derived_ids = local
                    .sizing
                    .as_ref()
                    .map(|sizing| sizing.derived_engine_parameter_ids.clone())
                    .unwrap_or_default();
                let resolved_values = local
                    .engine_parameter_values
                    .clone()
                    .expect("resolved engine parameter values")
                    .values;
                assert!(
                    derived_ids.iter().all(|id| {
                        !defaults
                            .values
                            .iter()
                            .any(|value| &value.parameter_id == id)
                            && resolved_values
                                .iter()
                                .any(|value| &value.parameter_id == id)
                    }),
                    "derived parameters are exactly the resolved values the profile omits"
                );
                assert_eq!(
                    resolved_values
                        .into_iter()
                        .filter(|value| !derived_ids.contains(&value.parameter_id))
                        .collect::<Vec<_>>(),
                    defaults.values
                );
            }

            assert_eq!(
                resolved_configuration_ids,
                model
                    .configurations
                    .iter()
                    .filter(|configuration| {
                        configuration_platforms(&releases, model, &configuration.configuration_id)
                            .contains(&platform)
                    })
                    .map(|configuration| configuration.configuration_id.clone())
                    .collect(),
                "every committed local configuration must resolve on each platform it declares, here {platform:?}",
            );
        }
    }

    for model in &models {
        for configuration in &model.configurations {
            let platforms =
                configuration_platforms(&releases, model, &configuration.configuration_id);
            let linux = platforms
                .iter()
                .filter(|platform| platform.is_linux())
                .cloned()
                .collect::<BTreeSet<_>>();
            if linux.is_empty() {
                continue;
            }
            match single_platform_engine(&releases, model, &configuration.configuration_id) {
                Some(entry) => assert_eq!(
                    linux,
                    BTreeSet::from([entry.platform.clone()]),
                    "configuration {:?} of {:?} uses allowlisted {:?}, which must support {:?} only ({})",
                    configuration.configuration_id,
                    model.model_id,
                    entry.engine_kind,
                    entry.platform,
                    entry.reason,
                ),
                None => assert_eq!(
                    linux,
                    BTreeSet::from([
                        ExecutionPlatform::linux_amd64(),
                        ExecutionPlatform::linux_arm64(),
                    ]),
                    "configuration {:?} of {:?} must support both Linux platforms",
                    configuration.configuration_id,
                    model.model_id,
                ),
            }
        }
    }

    assert!(
        resolved_profile_count > 0,
        "repository catalog must expose at least one model profile"
    );
}

#[test]
fn repository_catalog_sizing_is_linear_in_the_total_context() {
    assert_eq!(
        dotcore_catalog::test_support::sizing_not_linear_in_total_context(&repository_catalog()),
        Vec::<String>::new()
    );
}

/// Every repository profile's largest fit on each GPU target of each simulated hardware choice
/// (each named GPU alone on the simulated host, and each named machine), idle, fills
/// the target's free memory less the headroom: both verdicts read "fits" there, and the
/// free-memory verdict reads "exceeds" 1024 tokens later, unless the engine's range or a derived
/// value stops the total first. A profile without a largest fit exceeds at its lowest total
/// context, on the hardware's total or on its free memory. Machines run as their rendered
/// readings through the site's reconciliation, so no hardware is needed.
#[test]
fn repository_largest_fits_fill_free_memory_on_every_simulated_choice() {
    use dotcore::{model::sizing_scenario::SizingScenario, testing::profile_compatibility};
    use dotcore_node_hardware::MachineDescription;
    use dotcore_proto::dotcore::{
        deployment::v1::MemoryFit,
        model::v1::{ModelCompatibilityStatus, SizingScenarioUnsupportedReason},
    };

    const STEP_TOKENS: u32 = 1024;
    let catalog = repository_catalog();
    let mut checked = 0;
    let mut clamped = Vec::new();
    let mut nothing_fits = Vec::new();

    for (machine, gpu) in MachineDescription::simulated_choices()
        .into_iter()
        .flat_map(|choice| {
            let gpus = simulated_alone(choice.name).gpu_ids().count();
            (0..gpus).map(move |gpu| (choice.name, gpu))
        })
    {
        let (target, inventory) = simulated_target(machine, gpu);
        let targets = [target];
        for model in catalog.list_model_releases() {
            let models = [model];
            let read = |scenario: Option<&SizingScenario>| {
                profile_compatibility(&catalog, &models, &targets, scenario, &inventory)
            };
            for entry in read(None).entries {
                let label = format!(
                    "{} {} on {machine} GPU {gpu}",
                    models[0].model_id, entry.profile_id
                );
                if entry.status() != ModelCompatibilityStatus::Runs {
                    continue;
                }
                let (lowest, context_per_request) =
                    lowest_total_context(&catalog, &models[0], &entry.profile_id);
                let verdict = |total_context: u32| {
                    let scenario = SizingScenario::new(total_context, context_per_request)
                        .unwrap_or_else(|error| panic!("{label}: scenario: {error}"));
                    let response = read(Some(&scenario));
                    let sizing = response
                        .profiles
                        .iter()
                        .find(|sizing| sizing.profile_id == entry.profile_id)
                        .map(|sizing| sizing.unsupported_reason())
                        .unwrap_or_else(|| panic!("{label}: no sizing"));
                    let fit = response
                        .entries
                        .iter()
                        .find(|other| other.profile_id == entry.profile_id)
                        .unwrap_or_else(|| panic!("{label}: no entry"));
                    let reason = match sizing {
                        SizingScenarioUnsupportedReason::Unspecified => fit.unsupported_reason(),
                        reason => reason,
                    };
                    (fit.free_memory_fit(), fit.memory_fit(), reason)
                };
                let Some(largest) = entry.largest_total_context_tokens else {
                    let why = match verdict(lowest) {
                        (
                            MemoryFit::Exceeds,
                            MemoryFit::Exceeds,
                            SizingScenarioUnsupportedReason::Unspecified,
                        ) => "exceeds the total",
                        (
                            MemoryFit::Exceeds,
                            MemoryFit::Fits,
                            SizingScenarioUnsupportedReason::Unspecified,
                        ) => "exceeds free memory",
                        other => panic!(
                            "{label}: no largest fit, yet {other:?} at the lowest total of {lowest} tokens"
                        ),
                    };
                    nothing_fits.push(format!("{label}: {why} at {lowest} tokens"));
                    continue;
                };

                assert_eq!(
                    verdict(largest),
                    (
                        MemoryFit::Fits,
                        MemoryFit::Fits,
                        SizingScenarioUnsupportedReason::Unspecified
                    ),
                    "{label}: largest fit of {largest} tokens"
                );
                match verdict(largest + STEP_TOKENS) {
                    (on_free, _, SizingScenarioUnsupportedReason::Unspecified) => assert_eq!(
                        on_free,
                        MemoryFit::Exceeds,
                        "{label}: {largest} tokens leave room for {STEP_TOKENS} more"
                    ),
                    (_, _, reason) => {
                        clamped.push(format!("{label}: {largest} tokens, next {reason:?}"))
                    }
                }
                checked += 1;
            }
        }
    }

    println!("checked {checked} profile and machine pairs");
    println!("clamped by the engine range: {clamped:#?}");
    println!("nothing fits at idle: {nothing_fits:#?}");
    assert!(checked > 0, "no repository profile has a largest fit");
}

/// Every repository configuration approving several engine releases resolves each of its profiles
/// to the Linux image on NVIDIA targets (by memory architecture) and the Nix package on macOS
/// native, and has no artifact for macOS Docker Desktop's Linux VM.
#[test]
fn repository_multi_release_profiles_resolve_per_platform_and_hardware_class() {
    use dotcore_proto::dotcore::site::v1::model_runtime_binding::Adapter;

    let catalog = repository_catalog();
    let (gb10, gb10_inventory) = accelerator_target(
        Adapter::DockerRuntime(Default::default()),
        ExecutionPlatform::linux_arm64(),
        NodeAcceleratorFacts::nvidia("580").with_host_memory(HOST_POOL_BYTES, HOST_POOL_BYTES),
        Some(MemoryArchitecture::Unified),
    );
    let (rtx, rtx_inventory) = accelerator_target(
        Adapter::DockerRuntime(Default::default()),
        ExecutionPlatform::linux_amd64(),
        NodeAcceleratorFacts::nvidia("580"),
        None,
    );
    let (native, native_inventory) = macos_native_target_capabilities();
    let (desktop, desktop_inventory) = accelerator_target(
        Adapter::DockerRuntime(Default::default()),
        ExecutionPlatform::linux_arm64(),
        NodeAcceleratorFacts::apple().with_host_memory(HOST_POOL_BYTES, HOST_POOL_BYTES),
        None,
    );
    let mut configuration_count = 0;
    let mut profile_count = 0;

    for model in catalog.list_model_releases() {
        let model_id = CatalogModelId(model.model_id.clone());
        for configuration in &model.configurations {
            let release_ids = configuration
                .engine_artifacts
                .iter()
                .map(|reference| reference.engine_release_id.as_str())
                .collect::<BTreeSet<_>>();
            if release_ids.len() < 2 {
                continue;
            }
            configuration_count += 1;

            for profile in catalog
                .profiles_for_model(&model_id)
                .into_iter()
                .filter(|profile| profile.configuration_id == configuration.configuration_id)
            {
                profile_count += 1;
                let (binding_id, defaults) = catalog
                    .local_defaults_for(&model_id, &profile.profile_id)
                    .expect("validated profile defaults");
                let deployment = LocalModelDeploymentConfiguration {
                    model_artifact_binding_id: binding_id,
                    engine_parameter_values: Some(defaults),
                    ..Default::default()
                };
                let label = format!("profile {:?} of {:?}", profile.profile_id, model.model_id);
                let resolve = |target, inventory| {
                    resolve_local_launch_with_placeholder_identity(
                        &catalog,
                        &model_id,
                        &profile.profile_id,
                        &deployment,
                        target,
                        inventory,
                    )
                };
                // (artifact runs as a container image, platform, accelerator, hardware class)
                let resolved = |local: ResolvedLocalModelDeploymentSnapshot| {
                    let engine = local.engine_artifact.expect("engine artifact");
                    (
                        match engine.artifact.expect("artifact").kind {
                            Some(EngineArtifactKind::Oci(_)) => true,
                            Some(EngineArtifactKind::Nix(nix)) => {
                                dotcore_domain::nix::provides_docker_image(&nix)
                            }
                            None => false,
                        },
                        engine.platform.map(|platform| {
                            format!("{}/{}", platform.operating_system, platform.architecture)
                        }),
                        engine.accelerator,
                        local
                            .hardware_support
                            .expect("hardware support")
                            .hardware_class_id,
                    )
                };
                let expected = |is_container, platform: &str, accelerator: &str, class: &str| {
                    (
                        is_container,
                        Some(platform.to_owned()),
                        accelerator.to_owned(),
                        class.to_owned(),
                    )
                };

                assert_eq!(
                    resolved(resolve(&gb10, &gb10_inventory).expect(&label)),
                    expected(true, "linux/arm64", "nvidia", "nvidia-unified"),
                    "{label} on GB10"
                );
                assert_eq!(
                    resolved(resolve(&rtx, &rtx_inventory).expect(&label)),
                    expected(true, "linux/amd64", "nvidia", "nvidia-dedicated"),
                    "{label} on RTX"
                );
                assert_eq!(
                    resolved(resolve(&native, &native_inventory).expect(&label)),
                    expected(false, "darwin/arm64", "apple", "apple"),
                    "{label} on macOS native"
                );
                let desktop_error = resolve(&desktop, &desktop_inventory)
                    .expect_err(&format!("{label} must not run on macOS Docker Desktop"));
                assert!(
                    matches!(
                        &desktop_error,
                        ModelLaunchResolutionError::EngineArtifactSelection(_)
                    ) && desktop_error.to_string().contains(
                        "no approved engine artifact supports target platform linux/arm64 with apple accelerators"
                    ),
                    "{label} on macOS Docker Desktop: {desktop_error}"
                );
            }
        }
    }

    assert!(
        configuration_count > 0 && profile_count >= configuration_count,
        "repository catalog must keep a configuration approving several engine releases ({configuration_count} configurations, {profile_count} profiles)"
    );
}

/// Declared sizing equations resolve into the snapshot's estimates: accelerator memory grows
/// linearly with context and counts the prompt cache, and host memory follows its own equation.
#[test]
fn catalog_sizing_resolves_estimates_into_the_deployment_snapshot() {
    let catalog = synthetic_sizing_catalog();
    let model_id = CatalogModelId("fixture-model".to_owned());
    let (target, inventory) = target_capabilities(ExecutionPlatform::linux_amd64());
    let resolve = |parameters: &[(&str, u64)]| {
        let (binding_id, mut defaults) = catalog
            .local_defaults_for(&model_id, "balanced")
            .expect("profile defaults");
        for (parameter_id, value) in parameters {
            defaults
                .values
                .iter_mut()
                .find(|default| default.parameter_id == *parameter_id)
                .unwrap_or_else(|| panic!("{parameter_id} default"))
                .value = Some(engine_parameter_value::Value::UnsignedInteger(*value));
        }
        resolve_local_launch_with_placeholder_identity(
            &catalog,
            &model_id,
            "balanced",
            &LocalModelDeploymentConfiguration {
                model_artifact_binding_id: binding_id,
                engine_parameter_values: Some(defaults),
                ..Default::default()
            },
            &target,
            &inventory,
        )
        .expect("resolved profile")
        .sizing
        .expect("resolved sizing")
    };

    // base 1000 + 10 per total context token + prompt cache MiB; the fixture default cache is
    // 0 MiB.
    for (total_context, expected) in [(40_000, 401_000), (80_000, 801_000), (120_000, 1_201_000)] {
        assert_eq!(
            resolve(&[("total-context", total_context)]).accelerator_memory_bytes,
            Some(expected),
            "total context {total_context}"
        );
    }
    assert_eq!(
        resolve(&[("total-context", 40_000), ("prompt-cache-memory-mib", 2048)])
            .accelerator_memory_bytes,
        Some(401_000 + 2048 * 1_048_576)
    );
    // One MiB of host memory per parallel request.
    for requests in [1, 4] {
        assert_eq!(
            resolve(&[("parallel-requests", requests)]).host_memory_bytes,
            Some(requests * 1_048_576),
            "{requests} parallel requests"
        );
    }
}

/// A llama.cpp sizing equation counts each parallel request's own state once per request, so the
/// estimate moves by exactly that state per request added or removed.
#[test]
fn llama_cpp_request_state_scales_the_estimate_with_the_parallel_requests() {
    let root = temporary_catalog_root();
    create_empty_catalog_source(&root);
    write_sizing_engine(&root);
    write_sizing_model_with(
        &root,
        r#"    sizing:
      bindingParameters: ["baseAccelBytes"]
      parameters:
        slotStateBytes: 1000
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "${baseAccelBytes} + 10 * ${total-context} + ${parallel-requests} * ${slotStateBytes}"
"#,
        r#"        bindingSizing:
          - bindingId: "sized"
            parameters: { baseAccelBytes: 5000 }
          - bindingId: "second"
            parameters: { baseAccelBytes: 5000 }
"#,
        &format!("      values:\n{SIZING_ENGINE_CACHE_DEFAULT}"),
    );
    let catalog = Catalog::load_with_policy(
        &root,
        dotcore_catalog::OciArtifactPolicy::Development,
        dotcore::model::engine::declaration_validator(),
    )
    .expect("parallel request sizing catalog");
    std::fs::remove_dir_all(root).expect("remove fixture catalog");
    let model_id = CatalogModelId("fixture-model".to_owned());
    let (target, inventory) = target_capabilities(ExecutionPlatform::linux_amd64());
    let estimate = |requests: u64| {
        resolve_local_launch(
            &catalog,
            &model_id,
            "balanced",
            &LocalModelDeploymentConfiguration {
                model_artifact_binding_id: "sized".to_owned(),
                engine_parameter_values: Some(unsigned_values(&[
                    ("total-context", 30_000),
                    ("prompt-cache-memory-mib", 0),
                    ("parallel-requests", requests),
                ])),
                ..Default::default()
            },
            &target,
            &inventory,
            None,
        )
        .expect("resolved parallel requests")
        .sizing
        .expect("resolved sizing")
        .accelerator_memory_bytes
        .expect("accelerator estimate")
    };

    // The default 4 requests keep the 4-request measurement: 5000 + 4 * 1000 + 10 * 30000.
    assert_eq!(estimate(4), 309_000);
    assert_eq!(estimate(8), 309_000 + 4 * 1000);
    assert_eq!(estimate(2), 309_000 - 2 * 1000);
}

#[tokio::test]
async fn catalog_service_lists_engine_releases_in_identity_order_without_changing_them() {
    let release = synthetic_engine_release();
    let mut later = release.clone();
    later.engine_release_id = "z-release".to_owned();
    let mut earlier = release;
    earlier.engine_release_id = "a-release".to_owned();
    let service = CatalogService::new(Arc::new(
        catalog_builder()
            .engine_validator(dotcore::model::engine::declaration_validator())
            .engine_release("later.yaml", later.clone())
            .engine_release("earlier.yaml", earlier.clone())
            .build()
            .expect("engine release catalog"),
    ));

    let response = CatalogServiceContract::list_engine_releases(
        &service,
        Request::new(ListEngineReleasesRequest {}),
    )
    .await
    .expect("engine release catalog response")
    .into_inner();

    assert_eq!(response.engine_releases, [earlier, later]);
}

#[tokio::test]
async fn catalog_service_lists_loaded_upstream_model_templates_without_changing_them() {
    let template = upstream_model_template();
    let service = CatalogService::new(Arc::new(
        catalog_builder()
            .upstream_model_template("template.yaml", template.clone())
            .build()
            .expect("upstream model template catalog"),
    ));

    let response = CatalogServiceContract::list_upstream_model_templates(
        &service,
        Request::new(ListUpstreamModelTemplatesRequest {}),
    )
    .await
    .expect("upstream model template catalog response")
    .into_inner();

    assert_eq!(response.upstream_model_templates, [template]);
}

#[test]
fn upstream_model_template_snapshots_remain_complete_after_catalog_removal() {
    let template = upstream_model_template();
    let catalog = catalog_builder()
        .upstream_model_template("template.yaml", template.clone())
        .build()
        .expect("upstream model template catalog");
    let first = resolve_upstream_snapshot(&catalog, &template.upstream_model_template_id)
        .expect("first independent upstream model template snapshot");
    let second = resolve_upstream_snapshot(&catalog, &template.upstream_model_template_id)
        .expect("second independent upstream model template snapshot");
    let removed_catalog = catalog_builder()
        .build()
        .expect("Catalog without the selected template");

    assert_eq!(first, second);
    assert!(
        resolve_upstream_snapshot(&removed_catalog, &template.upstream_model_template_id).is_none()
    );
    assert_eq!(first_upstream_model_template(&first), template);
    assert_eq!(first_upstream_model_template(&second), template);

    let retained_template = first_upstream_model_template(&first);
    let catalog_from_retained_template = catalog_builder()
        .upstream_model_template("retained-template.yaml", retained_template)
        .build()
        .expect("retained embedded template remains a valid catalog declaration");
    assert_eq!(
        resolve_upstream_snapshot(
            &catalog_from_retained_template,
            &template.upstream_model_template_id,
        )
        .expect("retained template remains usable independently"),
        first
    );
}

#[tokio::test]
async fn catalog_lists_a_valid_intelligence_score_and_loads_models_without_one() {
    let score = IntelligenceScore { overall: 34.0 };
    let mut scored = synthetic_model();
    scored.intelligence = Some(score);
    let mut unscored = synthetic_model();
    unscored.model_id = "unscored-model".to_owned();
    let service = CatalogService::new(Arc::new(
        catalog_builder()
            .engine_validator(dotcore::model::engine::declaration_validator())
            .engine_release("engine.yaml", synthetic_engine_release())
            .model("scored.yaml", scored)
            .model("unscored.yaml", unscored)
            .build()
            .expect("Catalog with and without a score"),
    ));

    let models = CatalogServiceContract::list_models(&service, Request::new(ListModelsRequest {}))
        .await
        .expect("model catalog response")
        .into_inner()
        .models;

    let by_id = |id: &str| {
        models
            .iter()
            .find(|model| model.model_id == id)
            .unwrap_or_else(|| panic!("{id} must be listed"))
    };
    assert_eq!(by_id("fixture-model").intelligence, Some(score));
    assert_eq!(by_id("unscored-model").intelligence, None);
}

#[test]
fn catalog_rejects_invalid_intelligence_scores() {
    let cases = [
        (
            f64::NAN,
            "intelligence score NaN must be finite and within 0 to 100",
        ),
        (
            -0.5,
            "intelligence score -0.5 must be finite and within 0 to 100",
        ),
        (
            100.5,
            "intelligence score 100.5 must be finite and within 0 to 100",
        ),
    ];

    for (overall, expected) in cases {
        let mut model = synthetic_model();
        model.intelligence = Some(IntelligenceScore { overall });
        let error = catalog_builder()
            .engine_validator(dotcore::model::engine::declaration_validator())
            .engine_release("engine.yaml", synthetic_engine_release())
            .model("model.yaml", model)
            .build()
            .expect_err(expected)
            .to_string();
        assert!(error.contains(expected), "{overall}: {error}");
    }
}

#[test]
fn catalog_accepts_intelligence_score_bounds() {
    for overall in [0.0, 100.0] {
        let mut model = synthetic_model();
        model.intelligence = Some(IntelligenceScore { overall });
        catalog_builder()
            .engine_validator(dotcore::model::engine::declaration_validator())
            .engine_release("engine.yaml", synthetic_engine_release())
            .model("model.yaml", model)
            .build()
            .unwrap_or_else(|error| panic!("score {overall} must load: {error}"));
    }
}

#[test]
fn catalog_rejects_a_model_without_a_maximum_context() {
    let mut model = synthetic_model();
    model.maximum_context_tokens = 0;

    let error = catalog_builder()
        .engine_validator(dotcore::model::engine::declaration_validator())
        .engine_release("engine.yaml", synthetic_engine_release())
        .model("model.yaml", model)
        .build()
        .expect_err("model without a maximum context")
        .to_string();

    assert!(
        error.contains("model \"fixture-model\" requires a positive maximum context"),
        "unexpected validation error: {error}"
    );
}

#[test]
fn catalog_rejects_a_configuration_maximum_context_above_the_model_maximum() {
    let load = |maximum_context_tokens| {
        let mut model = synthetic_model();
        model.configurations[0].maximum_context_tokens = Some(maximum_context_tokens);
        catalog_builder()
            .engine_validator(dotcore::model::engine::declaration_validator())
            .engine_release("engine.yaml", synthetic_engine_release())
            .model("model.yaml", model)
            .build()
    };

    let error = load(262_145)
        .expect_err("configuration maximum above the model maximum")
        .to_string();
    assert!(
        error.contains(
            "configuration \"local\" maximum context of 262145 tokens is above the model's maximum context of 262144 tokens; a configuration may only lower it"
        ),
        "unexpected validation error: {error}"
    );
    let error = load(0).expect_err("zero configuration maximum").to_string();
    assert!(
        error.contains(
            "configuration \"local\" maximum context of 0 tokens is below the minimum context of 1024 tokens"
        ),
        "unexpected validation error: {error}"
    );
    load(262_144).expect("configuration maximum at the model maximum");
    load(131_072).expect("configuration maximum below the model maximum");
}

#[tokio::test]
async fn catalog_lists_a_valid_license_and_loads_models_without_one() {
    let license = License {
        id: "Apache-2.0".to_owned(),
        terms_url: "https://www.apache.org/licenses/LICENSE-2.0".to_owned(),
    };
    let mut licensed = synthetic_model();
    licensed.license = Some(license.clone());
    let mut unlicensed = synthetic_model();
    unlicensed.model_id = "unlicensed-model".to_owned();
    let service = CatalogService::new(Arc::new(
        catalog_builder()
            .engine_validator(dotcore::model::engine::declaration_validator())
            .engine_release("engine.yaml", synthetic_engine_release())
            .model("licensed.yaml", licensed)
            .model("unlicensed.yaml", unlicensed)
            .build()
            .expect("Catalog with and without a licence"),
    ));

    let models = CatalogServiceContract::list_models(&service, Request::new(ListModelsRequest {}))
        .await
        .expect("model catalog response")
        .into_inner()
        .models;

    let by_id = |id: &str| {
        models
            .iter()
            .find(|model| model.model_id == id)
            .unwrap_or_else(|| panic!("{id} must be listed"))
    };
    assert_eq!(by_id("fixture-model").license, Some(license));
    assert_eq!(by_id("unlicensed-model").license, None);
}

#[test]
fn catalog_rejects_invalid_licenses() {
    let identifier = "an SPDX-style identifier (letters and digits separated by single '.', '-' or '+') or \"custom\"";
    let cases = [
        ("", "", format!("model \"fixture-model\" license id \"\" must be {identifier}")),
        (
            "Apache 2.0",
            "",
            format!("model \"fixture-model\" license id \"Apache 2.0\" must be {identifier}"),
        ),
        (
            "MIT OR Apache-2.0",
            "",
            format!("model \"fixture-model\" license id \"MIT OR Apache-2.0\" must be {identifier}"),
        ),
        (
            "Apache--2.0",
            "",
            format!("model \"fixture-model\" license id \"Apache--2.0\" must be {identifier}"),
        ),
        (
            "MIT-",
            "",
            format!("model \"fixture-model\" license id \"MIT-\" must be {identifier}"),
        ),
        (
            "Custom",
            "https://example.test/terms",
            "model \"fixture-model\" license id \"Custom\" must be spelled \"custom\" in lowercase"
                .to_owned(),
        ),
        (
            "CUSTOM",
            "https://example.test/terms",
            "model \"fixture-model\" license id \"CUSTOM\" must be spelled \"custom\" in lowercase"
                .to_owned(),
        ),
        (
            "custom",
            "",
            "model \"fixture-model\" license id \"custom\" requires a terms URL".to_owned(),
        ),
        (
            "LicenseRef-OpenMDW-1.1",
            "",
            "model \"fixture-model\" license id \"LicenseRef-OpenMDW-1.1\" requires a terms URL"
                .to_owned(),
        ),
        (
            "MIT",
            "https://",
            "model \"fixture-model\" license terms URL \"https://\" must be an http(s) URL with a host"
                .to_owned(),
        ),
        (
            "MIT",
            "ftp://example.test/mit",
            "model \"fixture-model\" license terms URL \"ftp://example.test/mit\" must be an http(s) URL with a host"
                .to_owned(),
        ),
        (
            "MIT",
            "example.test/mit",
            "model \"fixture-model\" license terms URL \"example.test/mit\" must be an http(s) URL with a host"
                .to_owned(),
        ),
    ];

    for (id, terms_url, expected) in cases {
        let mut model = synthetic_model();
        model.license = Some(License {
            id: id.to_owned(),
            terms_url: terms_url.to_owned(),
        });
        let error = catalog_builder()
            .engine_validator(dotcore::model::engine::declaration_validator())
            .engine_release("engine.yaml", synthetic_engine_release())
            .model("model.yaml", model)
            .build()
            .expect_err(&expected)
            .to_string();
        assert!(error.contains(&expected), "{id} {terms_url}: {error}");
    }
}

#[test]
fn catalog_accepts_valid_licenses() {
    let cases = [
        ("Apache-2.0", ""),
        ("MIT", "http://example.test/mit"),
        ("BSD-3-Clause", ""),
        ("LicenseRef-OpenMDW-1.1", "https://openmdw.ai/license"),
        ("custom", "https://example.test/terms"),
    ];

    for (id, terms_url) in cases {
        let mut model = synthetic_model();
        model.license = Some(License {
            id: id.to_owned(),
            terms_url: terms_url.to_owned(),
        });
        catalog_builder()
            .engine_validator(dotcore::model::engine::declaration_validator())
            .engine_release("engine.yaml", synthetic_engine_release())
            .model("model.yaml", model)
            .build()
            .unwrap_or_else(|error| panic!("licence {id} {terms_url} must load: {error}"));
    }
}

#[test]
fn catalog_preserves_profile_presentation() {
    let mut model = synthetic_model();
    model.profiles[0].display_name = "Balanced".to_owned();
    model.profiles[0].description = "Balances quality and resource use.".to_owned();
    let catalog = catalog_builder()
        .engine_validator(dotcore::model::engine::declaration_validator())
        .engine_release("engine.yaml", synthetic_engine_release())
        .model("model.yaml", model)
        .build()
        .expect("Catalog with profile presentation");

    let models = catalog.list_model_releases();
    let profile = &models[0].profiles[0];
    assert_eq!(profile.display_name, "Balanced");
    assert_eq!(profile.description, "Balances quality and resource use.");
}

#[test]
fn catalog_accepts_omitted_profile_presentation() {
    let mut model = synthetic_model();
    model.profiles[0].display_name.clear();
    model.profiles[0].description.clear();

    catalog_builder()
        .engine_validator(dotcore::model::engine::declaration_validator())
        .engine_release("engine.yaml", synthetic_engine_release())
        .model("model.yaml", model)
        .build()
        .expect("Catalog accepts omitted profile presentation");
}

#[test]
fn catalog_accepts_multi_platform_artifacts_and_non_overlapping_approved_alternatives() {
    let mut release = synthetic_engine_release();
    let mut amd64 = release.artifacts[0].clone();
    amd64.artifact_id = "linux-amd64".to_owned();
    amd64.selectors = vec![nvidia_selector("linux", "amd64", None)];
    let mut unapproved = amd64.clone();
    unapproved.artifact_id = "unapproved-amd64".to_owned();
    let mut arm64 = release.artifacts[0].clone();
    arm64.artifact_id = "linux-arm64".to_owned();
    arm64.selectors = vec![nvidia_selector("linux", "arm64", None)];
    release.artifacts = vec![amd64, arm64, unapproved];

    let mut model = synthetic_model();
    model.configurations[0].engine_artifacts = vec![
        EngineArtifactReference {
            engine_release_id: "fixture-engine".to_owned(),
            engine_artifact_id: "linux-amd64".to_owned(),
        },
        EngineArtifactReference {
            engine_release_id: "fixture-engine".to_owned(),
            engine_artifact_id: "linux-arm64".to_owned(),
        },
    ];

    catalog_builder()
        .engine_validator(dotcore::model::engine::declaration_validator())
        .engine_release("engine.yaml", release)
        .model("model.yaml", model)
        .build()
        .expect("non-overlapping approved artifact alternatives");

    catalog_builder()
        .engine_validator(dotcore::model::engine::declaration_validator())
        .engine_release("engine.yaml", synthetic_engine_release())
        .model("model.yaml", synthetic_model())
        .build()
        .expect("one approved multi-platform artifact");
}

#[test]
fn catalog_rejects_invalid_platform_driver_and_approved_artifact_sets() {
    let mut cases = Vec::new();

    for (label, selectors, expected) in [
        (
            "missing selectors",
            Vec::new(),
            "requires at least one selector",
        ),
        (
            "duplicate platforms",
            vec![
                nvidia_selector("linux", "amd64", None),
                nvidia_selector("linux", "amd64", None),
            ],
            "each platform may appear in only one selector",
        ),
        (
            "unsupported architecture",
            vec![nvidia_selector("linux", "any", None)],
            "support only linux/amd64 or linux/arm64",
        ),
        (
            "unsupported variant",
            vec![nvidia_selector("linux", "arm64", Some("v8"))],
            "support only linux/amd64 or linux/arm64",
        ),
        (
            "darwin OCI platform",
            vec![apple_selector()],
            "support only linux/amd64 or linux/arm64",
        ),
        (
            "apple on linux",
            vec![Selector {
                accelerators: vec!["apple".to_owned()],
                minimum_nvidia_driver_major_version: 0,
                ..nvidia_selector("linux", "arm64", None)
            }],
            "apple requires the darwin/arm64 platform",
        ),
        (
            "unknown accelerator",
            vec![Selector {
                accelerators: vec!["amd".to_owned()],
                ..nvidia_selector("linux", "amd64", None)
            }],
            "accelerators must be nvidia or apple",
        ),
        (
            "invalid driver minimum",
            vec![Selector {
                minimum_nvidia_driver_major_version: 0,
                ..nvidia_selector("linux", "amd64", None)
            }],
            "nvidia requires a minimum NVIDIA driver major version",
        ),
    ] {
        let mut release = synthetic_engine_release();
        release.artifacts[0].selectors = selectors;
        cases.push((label, release, synthetic_model(), expected));
    }

    let mut empty = synthetic_model();
    empty.configurations[0].engine_artifacts.clear();
    cases.push((
        "empty approved set",
        synthetic_engine_release(),
        empty,
        "requires at least one approved",
    ));

    let mut duplicate = synthetic_model();
    let duplicate_reference = duplicate.configurations[0].engine_artifacts[0].clone();
    duplicate.configurations[0]
        .engine_artifacts
        .push(duplicate_reference);
    cases.push((
        "duplicate artifact id",
        synthetic_engine_release(),
        duplicate,
        "invalid or duplicate approved",
    ));

    let mut invalid = synthetic_model();
    invalid.configurations[0].engine_artifacts[0]
        .engine_artifact_id
        .clear();
    cases.push((
        "empty artifact id",
        synthetic_engine_release(),
        invalid,
        "invalid or duplicate approved",
    ));

    let mut unknown = synthetic_model();
    unknown.configurations[0].engine_artifacts[0].engine_artifact_id = "missing".to_owned();
    cases.push((
        "unknown artifact id",
        synthetic_engine_release(),
        unknown,
        "references a missing engine artifact",
    ));

    let mut missing_release = synthetic_model();
    missing_release.configurations[0]
        .engine_artifacts
        .push(EngineArtifactReference {
            engine_release_id: "another-release".to_owned(),
            engine_artifact_id: "linux-amd64".to_owned(),
        });
    cases.push((
        "missing engine release",
        synthetic_engine_release(),
        missing_release,
        "configuration \"local\" references a missing engine release \"another-release\"",
    ));

    let mut overlapping_release = synthetic_engine_release();
    let mut overlapping_artifact = overlapping_release.artifacts[0].clone();
    overlapping_artifact.artifact_id = "overlap".to_owned();
    overlapping_release.artifacts.push(overlapping_artifact);
    let mut overlapping_model = synthetic_model();
    overlapping_model.configurations[0]
        .engine_artifacts
        .push(EngineArtifactReference {
            engine_release_id: "fixture-engine".to_owned(),
            engine_artifact_id: "overlap".to_owned(),
        });
    cases.push((
        "overlapping artifact selectors",
        overlapping_release,
        overlapping_model,
        "configuration \"local\" approves engine artifacts \"linux-amd64\" and \"overlap\" whose selectors can match the same target",
    ));

    for (label, release, model, expected) in cases {
        let error = catalog_builder()
            .engine_validator(dotcore::model::engine::declaration_validator())
            .engine_release("engine.yaml", release)
            .model("model.yaml", model)
            .build()
            .expect_err(label)
            .to_string();
        assert!(error.contains(expected), "{label}: {error}");
    }
}

#[test]
fn catalog_validates_nix_artifacts_and_rejects_invalid_flake_references() {
    let catalog = macos_catalog(synthetic_macos_engine_release());
    let release = catalog
        .list_engine_releases()
        .into_iter()
        .find(|release| release.engine_release_id == "fixture-engine-macos")
        .expect("Nix engine release");
    assert!(matches!(
        release.artifacts[0].kind.as_ref(),
        Some(EngineArtifactKind::Nix(nix)) if nix.flake == "self"
    ));

    for (label, flake, expected) in [
        (
            "unlocked github",
            "github:dotcoreai/dotcore",
            "Nix GitHub flake reference must pin a revision",
        ),
        (
            "branch revision",
            "github:dotcoreai/dotcore/main",
            "revision must be 40 lowercase hexadecimal characters",
        ),
        (
            "other repository",
            "github:example/engines/0123456789abcdef0123456789abcdef01234567",
            "must name the dotcoreai/dotcore repository",
        ),
        (
            "path reference",
            "path:/src/dotcore",
            "must be a locked github:dotcoreai/dotcore/<revision> or self reference",
        ),
        (
            "absolute path",
            "/src/dotcore",
            "must be a locked github:dotcoreai/dotcore/<revision> or self reference",
        ),
        (
            "subdirectory flake",
            "self?dir=engines/fixture",
            "must not carry query parameters or a fragment",
        ),
    ] {
        let mut release = synthetic_macos_engine_release();
        nix_artifact_mut(&mut release).flake = flake.to_owned();
        let error = try_macos_catalog(release).expect_err(label).to_string();
        assert!(
            error.contains("engine artifact \"metal\" has an invalid Nix artifact")
                && error.contains(expected),
            "{label}: {error}"
        );
    }

    let mut precomputed_self = synthetic_macos_engine_release();
    nix_artifact_mut(&mut precomputed_self).store_paths = vec![darwin_store_path()];
    let mut linux_nvidia = synthetic_macos_engine_release();
    linux_nvidia.artifacts[0].selectors = vec![nvidia_selector("linux", "arm64", Some("v8"))];
    let mut without_executable = synthetic_macos_engine_release();
    nix_artifact_mut(&mut without_executable).provides = None;
    let mut locked_without_store_paths = synthetic_macos_engine_release();
    nix_artifact_mut(&mut locked_without_store_paths).flake = LOCKED_FLAKE.to_owned();
    let mut locked_missing_a_system = synthetic_macos_engine_release();
    let nix = nix_artifact_mut(&mut locked_missing_a_system);
    nix.flake = LOCKED_FLAKE.to_owned();
    nix.store_paths = vec![darwin_store_path()];
    locked_missing_a_system.artifacts[0]
        .selectors
        .push(nvidia_selector("linux", "arm64", None));
    for (label, release, expected) in [
        (
            "precomputed self",
            precomputed_self,
            "store paths require a locked flake reference",
        ),
        (
            "missing executable",
            without_executable,
            "must name what its package provides",
        ),
        (
            "locked without store paths",
            locked_without_store_paths,
            "aarch64-darwin has none",
        ),
        (
            "locked missing a system",
            locked_missing_a_system,
            "aarch64-linux has none",
        ),
        (
            "platform variant",
            linux_nvidia,
            "Nix artifacts support only linux/amd64, linux/arm64, or darwin/arm64",
        ),
    ] {
        let error = try_macos_catalog(release).expect_err(label).to_string();
        assert!(error.contains(expected), "{label}: {error}");
    }
}

#[test]
fn two_releases_of_one_engine_kind_validate_together_and_each_configuration_resolves_its_own() {
    let catalog = macos_catalog(synthetic_macos_engine_release());
    let model_id = CatalogModelId("fixture-model".to_owned());
    let releases = catalog.list_engine_releases();
    assert_eq!(
        releases
            .iter()
            .map(|release| (release.engine_release_id.as_str(), release.engine_kind))
            .collect::<Vec<_>>(),
        [
            ("fixture-engine", EngineKind::LlamaCpp as i32),
            ("fixture-engine-macos", EngineKind::LlamaCpp as i32),
        ]
    );
    let (linux, linux_inventory) = target_capabilities(ExecutionPlatform::linux_arm64());
    let (darwin, darwin_inventory) = target_capabilities(ExecutionPlatform::darwin_arm64());

    let linux_local = resolve_local_launch_with_placeholder_identity(
        &catalog,
        &model_id,
        "balanced",
        &default_configuration(&catalog, "balanced"),
        &linux,
        &linux_inventory,
    )
    .expect("Linux profile on a Linux Docker target");
    let darwin_local = resolve_local_launch_with_placeholder_identity(
        &catalog,
        &model_id,
        "metal",
        &default_configuration(&catalog, "metal"),
        &darwin,
        &darwin_inventory,
    )
    .expect("macOS profile on a darwin target");

    let engine = |local: &ResolvedLocalModelDeploymentSnapshot| {
        let engine = local.engine_artifact.clone().expect("engine artifact");
        (
            engine.engine_release_id,
            engine.artifact.expect("artifact").artifact_id,
            local.engine_parameter_schema_id.clone(),
        )
    };
    assert_eq!(
        engine(&linux_local),
        (
            "fixture-engine".to_owned(),
            "linux-amd64".to_owned(),
            "fixture-shared".to_owned()
        )
    );
    assert_eq!(
        engine(&darwin_local),
        (
            "fixture-engine-macos".to_owned(),
            "metal".to_owned(),
            "fixture-shared".to_owned()
        )
    );
    for (profile_id, target, inventory) in [
        ("balanced", &darwin, &darwin_inventory),
        ("metal", &linux, &linux_inventory),
    ] {
        assert!(
            matches!(
                select_engine_artifact(
                    &catalog,
                    &model_id,
                    profile_id,
                    &default_configuration(&catalog, profile_id),
                    target,
                    inventory,
                ),
                Err(ModelLaunchResolutionError::EngineArtifactSelection(_))
            ),
            "{profile_id} must not resolve on the other release's platform"
        );
    }
}

#[test]
fn one_configuration_approving_two_releases_pins_the_selected_artifacts_own_release() {
    let mut model = synthetic_model();
    let configuration = &mut model.configurations[0];
    configuration
        .engine_artifacts
        .push(EngineArtifactReference {
            engine_release_id: "fixture-engine-macos".to_owned(),
            engine_artifact_id: "metal".to_owned(),
        });
    configuration.hardware_support.push(HardwareSupport {
        hardware_class_id: "apple".to_owned(),
        hardware_requirements: None,
    });
    let catalog = catalog_builder()
        .engine_validator(dotcore::model::engine::declaration_validator())
        .engine_release("engine.yaml", synthetic_engine_release())
        .engine_release("engine-macos.yaml", synthetic_macos_engine_release())
        .model("model.yaml", model)
        .build()
        .expect("one configuration approving both releases");
    let model_id = CatalogModelId("fixture-model".to_owned());
    let (linux, linux_inventory) = target_capabilities(ExecutionPlatform::linux_amd64());
    let (native, native_inventory) = macos_native_target_capabilities();

    let resolved =
        [(&linux, &linux_inventory), (&native, &native_inventory)].map(|(target, inventory)| {
            let local = resolve_local_launch_with_placeholder_identity(
                &catalog,
                &model_id,
                "balanced",
                &default_configuration(&catalog, "balanced"),
                target,
                inventory,
            )
            .expect("profile resolves on the target");
            let engine = local.engine_artifact.expect("engine artifact");
            (
                engine.engine_release_id,
                engine.engine_version,
                engine.artifact.expect("artifact").artifact_id,
                local.engine_parameter_schema_id,
                local
                    .hardware_support
                    .expect("hardware support")
                    .hardware_class_id,
            )
        });

    assert_eq!(
        resolved,
        [
            (
                "fixture-engine".to_owned(),
                "fixture".to_owned(),
                "linux-amd64".to_owned(),
                "fixture-shared".to_owned(),
                "nvidia".to_owned(),
            ),
            (
                "fixture-engine-macos".to_owned(),
                "fixture-macos".to_owned(),
                "metal".to_owned(),
                "fixture-shared".to_owned(),
                "apple".to_owned(),
            ),
        ]
    );
}

#[test]
fn locked_nix_deployment_retains_its_catalog_store_paths_for_the_hosting_node_across_catalog_edits()
{
    const EDITED_STORE_PATH: &str = "/nix/store/00000000000000000000000000000003-edited";
    let model_id = CatalogModelId("fixture-model".to_owned());
    let (darwin, inventory) = target_capabilities(ExecutionPlatform::darwin_arm64());
    let precomputed = |flake: &str, store_path: &str| {
        let mut release = synthetic_macos_engine_release();
        let nix = nix_artifact_mut(&mut release);
        nix.flake = flake.to_owned();
        nix.store_paths = vec![NixSystemStorePath {
            store_path: store_path.to_owned(),
            ..darwin_store_path()
        }];
        macos_catalog(release)
    };
    let resolve = |catalog: &Catalog, identity: Option<&EngineLaunchIdentity>| {
        resolve_local_launch(
            catalog,
            &model_id,
            "metal",
            &default_configuration(catalog, "metal"),
            &darwin,
            &inventory,
            identity,
        )
    };

    let catalog = precomputed(LOCKED_FLAKE, PLACEHOLDER_STORE_PATH);
    assert_eq!(
        select_engine_artifact(
            &catalog,
            &model_id,
            "metal",
            &default_configuration(&catalog, "metal"),
            &darwin,
            &inventory,
        )
        .expect("selection"),
        EngineArtifactSelection::Nix(dotcore::model::resolution::NixEngineSelection {
            flake: LOCKED_FLAKE.to_owned(),
            package: "engines.llama-cpp.native".to_owned(),
        })
    );
    let local = resolve(&catalog, None).expect("precomputed locked snapshot");
    let engine = local.engine_artifact.as_ref().expect("engine artifact");
    assert_eq!(
        (
            engine.preparation_policy(),
            engine.launch_identity.as_str(),
            retained_store_paths(engine),
        ),
        (
            ArtifactPreparationPolicy::ImmutableRemote,
            "",
            vec![NixSystemStorePath {
                store_path: PLACEHOLDER_STORE_PATH.to_owned(),
                ..darwin_store_path()
            }],
        )
    );

    // The hosting node pins its own system's catalog store path; every launch applies it.
    let mut pinned = local.clone();
    pinned
        .engine_artifact
        .as_mut()
        .expect("engine artifact")
        .launch_identity = PLACEHOLDER_STORE_PATH.to_owned();
    let snapshot = ModelDeploymentSnapshot {
        source: Some(ModelDeploymentSnapshotSource::Local(pinned)),
    };
    let retained = retained_nix_engine(&snapshot);
    assert_eq!(
        retained,
        (
            ArtifactPreparationPolicy::ImmutableRemote,
            PLACEHOLDER_STORE_PATH.to_owned(),
            ResolvedEngineSource::Nix {
                flake: LOCKED_FLAKE.to_owned(),
                package: "engines.llama-cpp.native".to_owned(),
                executable: "fixture-server".to_owned(),
            },
            ("darwin".to_owned(), "arm64".to_owned()),
        )
    );

    // A later catalog edit selects a new revision for new deployments only.
    let edited_flake = LOCKED_FLAKE.replace("0123456789", "abcdef0123");
    let edited = precomputed(&edited_flake, EDITED_STORE_PATH);
    let edited_local = resolve(&edited, None).expect("edited catalog snapshot");
    assert_eq!(
        retained_store_paths(
            edited_local
                .engine_artifact
                .as_ref()
                .expect("engine artifact")
        ),
        [NixSystemStorePath {
            store_path: EDITED_STORE_PATH.to_owned(),
            ..darwin_store_path()
        }]
    );
    assert_eq!(retained_nix_engine(&snapshot), retained);

    // A pin outside the retained store paths never launches.
    let mut foreign = snapshot.clone();
    let Some(ModelDeploymentSnapshotSource::Local(local)) = foreign.source.as_mut() else {
        unreachable!("local snapshot");
    };
    local
        .engine_artifact
        .as_mut()
        .expect("engine artifact")
        .launch_identity = EDITED_STORE_PATH.to_owned();
    assert!(ResolvedModelLaunch::from_snapshot(&foreign).is_err());

    assert!(matches!(
        resolve(&catalog, Some(&placeholder_image_identity(LOCKED_FLAKE))),
        Err(ModelLaunchResolutionError::EngineIdentityUnexpected { reference })
            if reference == LOCKED_FLAKE
    ));
}

#[test]
fn development_source_nix_deployment_leaves_its_store_path_for_the_hosting_node() {
    let catalog = macos_catalog(synthetic_macos_engine_release());
    let model_id = CatalogModelId("fixture-model".to_owned());
    let (darwin, inventory) = target_capabilities(ExecutionPlatform::darwin_arm64());
    let resolve = |identity: Option<&EngineLaunchIdentity>| {
        resolve_local_launch(
            &catalog,
            &model_id,
            "metal",
            &default_configuration(&catalog, "metal"),
            &darwin,
            &inventory,
            identity,
        )
    };

    let local = resolve(None).expect("development-source snapshot");
    let engine = local.engine_artifact.clone().expect("engine artifact");
    assert_eq!(
        (
            engine.preparation_policy(),
            engine.launch_identity.as_str(),
            engine.artifact.and_then(|artifact| artifact.kind),
        ),
        (
            ArtifactPreparationPolicy::DevelopmentSource,
            "",
            Some(EngineArtifactKind::Nix(
                dotcore_proto::dotcore::catalog::v1::NixArtifact {
                    flake: "self".to_owned(),
                    package: Some("engines.llama-cpp.native".to_owned()),
                    provides: Some(
                        dotcore_proto::dotcore::catalog::v1::nix_artifact::Provides::Executable(
                            "fixture-server".to_owned()
                        )
                    ),
                    store_paths: Vec::new(),
                }
            )),
        )
    );
    // Until the hosting SiteNode's pin is applied, the snapshot never launches.
    assert!(
        ResolvedModelLaunch::from_snapshot(&ModelDeploymentSnapshot {
            source: Some(ModelDeploymentSnapshotSource::Local(local)),
        })
        .is_err()
    );
    assert!(matches!(
        resolve(Some(&placeholder_image_identity("self"))),
        Err(ModelLaunchResolutionError::EngineIdentityUnexpected { reference })
            if reference == "self"
    ));
}

#[test]
fn catalog_resolves_hardware_support_against_the_shared_classes() {
    let build = |hardware_class_id: &str| {
        let mut model = synthetic_model();
        model.configurations[0].hardware_support = vec![HardwareSupport {
            hardware_class_id: hardware_class_id.to_owned(),
            hardware_requirements: None,
        }];
        catalog_builder()
            .engine_validator(dotcore::model::engine::declaration_validator())
            .engine_release("engine.yaml", synthetic_engine_release())
            .model("model.yaml", model)
            .build()
            .map(|_| ())
            .map_err(|error| error.to_string())
    };

    assert_eq!(build("nvidia"), Ok(()));
    assert_eq!(
        build("apple"),
        Err(
            "Catalog diagnostics: model.yaml: model \"fixture-model\" configuration \"local\" approves engine artifact \"linux-amd64\", which no supported hardware class [\"apple\"] describes\nmodel.yaml: model \"fixture-model\" configuration \"local\" supports hardware class \"apple\", which no approved engine artifact [\"linux-amd64\"] can run on"
                .to_owned()
        )
    );
    assert_eq!(
        build("tpu"),
        Err(
            "Catalog diagnostics: model.yaml: model \"fixture-model\" configuration \"local\" references unknown hardware class \"tpu\""
                .to_owned()
        )
    );
}

#[test]
fn catalog_accepts_a_model_artifact_with_an_omitted_digest() {
    let mut model = synthetic_model();
    let source = model.artifacts[0]
        .source
        .as_mut()
        .expect("Hugging Face source");
    let HuggingFaceContent::File(file) = source.content.as_mut().expect("source content") else {
        panic!("file source");
    };
    file.provenance_digest.clear();

    catalog_builder()
        .engine_validator(dotcore::model::engine::declaration_validator())
        .engine_release("engine.yaml", synthetic_engine_release())
        .model("model.yaml", model)
        .build()
        .expect("Catalog accepts an omitted artifact digest");
}

#[test]
fn catalog_rejects_unsafe_or_ambiguous_hugging_face_sources() {
    let invalid_manifests = [
        Vec::new(),
        vec![snapshot_file("../weights.safetensors", '2')],
        vec![snapshot_file("nested\\weights.safetensors", '2')],
        vec![snapshot_file("bad\npath", '2')],
        vec![
            snapshot_file("config.json", '2'),
            snapshot_file("config.json", '3'),
        ],
        vec![
            snapshot_file("tokenizer", '2'),
            snapshot_file("tokenizer/config.json", '3'),
        ],
    ];

    for files in invalid_manifests {
        let mut model = synthetic_model();
        model.artifacts[0]
            .source
            .as_mut()
            .expect("Hugging Face source")
            .content = Some(HuggingFaceContent::Snapshot(HuggingFaceSnapshotArtifact {
            files,
        }));

        let error = catalog_builder()
            .engine_validator(dotcore::model::engine::declaration_validator())
            .engine_release("engine.yaml", synthetic_engine_release())
            .model("model.yaml", model)
            .build()
            .expect_err("invalid Hugging Face snapshot source")
            .to_string();

        assert!(
            error.contains("model artifact \"weights\" is invalid"),
            "unexpected validation error: {error}"
        );
    }

    let mut missing_content = synthetic_model();
    missing_content.artifacts[0]
        .source
        .as_mut()
        .expect("Hugging Face source")
        .content = None;
    let error = catalog_builder()
        .engine_validator(dotcore::model::engine::declaration_validator())
        .engine_release("engine.yaml", synthetic_engine_release())
        .model("model.yaml", missing_content)
        .build()
        .expect_err("missing Hugging Face source content")
        .to_string();
    assert!(
        error.contains("model artifact \"weights\" is invalid"),
        "unexpected validation error: {error}"
    );
}

#[test]
fn catalog_resolves_file_artifacts_in_repository_subdirectories() {
    let mut model = synthetic_model();
    set_file_path(
        model.artifacts[0].source.as_mut(),
        "UD-Q4_K_XL/weights.gguf",
    );
    set_file_path(
        model.auxiliary_artifacts[0].source.as_mut(),
        "MTP/projector.gguf",
    );
    let catalog = catalog_builder()
        .engine_validator(dotcore::model::engine::declaration_validator())
        .engine_release("engine.yaml", synthetic_engine_release())
        .model("model.yaml", model)
        .build()
        .expect("catalog accepts nested file paths");
    let (target, inventory) = target_capabilities(ExecutionPlatform::linux_amd64());

    let resolved = resolve_local_launch(
        &catalog,
        &CatalogModelId("fixture-model".to_owned()),
        "balanced",
        &default_configuration(&catalog, "balanced"),
        &target,
        &inventory,
        None,
    )
    .expect("resolved nested file artifacts");

    let file_path =
        |source: Option<&dotcore_proto::dotcore::catalog::v1::HuggingFaceArtifactSource>| {
            match source.and_then(|source| source.content.as_ref()) {
                Some(HuggingFaceContent::File(file)) => file.path.clone(),
                _ => panic!("file source"),
            }
        };
    assert_eq!(
        file_path(
            resolved
                .model_artifact
                .as_ref()
                .and_then(|artifact| artifact.source.as_ref())
        ),
        "UD-Q4_K_XL/weights.gguf"
    );
    assert_eq!(
        resolved
            .auxiliary_artifacts
            .iter()
            .map(|artifact| file_path(artifact.source.as_ref()))
            .collect::<Vec<_>>(),
        ["MTP/projector.gguf"]
    );
}

#[test]
fn catalog_rejects_unsafe_file_artifact_paths() {
    for path in [
        "../weights.gguf",
        "./weights.gguf",
        "/weights.gguf",
        "nested//weights.gguf",
        "nested/",
        "nested\\weights.gguf",
    ] {
        let mut model_path = synthetic_model();
        set_file_path(model_path.artifacts[0].source.as_mut(), path);
        let mut auxiliary_path = synthetic_model();
        set_file_path(auxiliary_path.auxiliary_artifacts[0].source.as_mut(), path);

        for (model, expected) in [
            (model_path, "model artifact \"weights\" is invalid"),
            (
                auxiliary_path,
                "auxiliary artifact \"projector\" is invalid",
            ),
        ] {
            let error = catalog_builder()
                .engine_validator(dotcore::model::engine::declaration_validator())
                .engine_release("engine.yaml", synthetic_engine_release())
                .model("model.yaml", model)
                .build()
                .expect_err("unsafe file path")
                .to_string();

            assert!(error.contains(expected), "{path:?}: {error}");
        }
    }
}

#[test]
fn catalog_loads_a_split_gguf_snapshot_model_artifact_for_llama_cpp() {
    let mut model = synthetic_model();
    model.artifacts[0]
        .source
        .as_mut()
        .expect("Hugging Face source")
        .content = Some(HuggingFaceContent::Snapshot(HuggingFaceSnapshotArtifact {
        files: vec![
            snapshot_file("UD-Q2_K_XL/model-UD-Q2_K_XL-00002-of-00002.gguf", '2'),
            snapshot_file("UD-Q2_K_XL/model-UD-Q2_K_XL-00001-of-00002.gguf", '3'),
        ],
    }));

    let catalog = catalog_builder()
        .engine_validator(dotcore::model::engine::declaration_validator())
        .engine_release("engine.yaml", synthetic_engine_release())
        .model("model.yaml", model.clone())
        .build()
        .expect("catalog accepts a split GGUF snapshot");

    assert_eq!(catalog.list_model_releases(), [model]);
}

#[test]
fn catalog_rejects_a_llama_cpp_snapshot_that_is_not_a_split_gguf_set() {
    let mut model = synthetic_model();
    model.artifacts[0]
        .source
        .as_mut()
        .expect("Hugging Face source")
        .content = Some(HuggingFaceContent::Snapshot(HuggingFaceSnapshotArtifact {
        files: vec![
            snapshot_file("config.json", '2'),
            snapshot_file("model.safetensors", '3'),
        ],
    }));

    let error = catalog_builder()
        .engine_validator(dotcore::model::engine::declaration_validator())
        .engine_release("engine.yaml", synthetic_engine_release())
        .model("model.yaml", model)
        .build()
        .expect_err("non-split llama.cpp snapshot")
        .to_string();

    assert!(
        error.contains(
            "configuration \"local\" binds model artifact \"weights\", which is not a valid split \
             GGUF set"
        ),
        "unexpected validation error: {error}"
    );
}

#[test]
fn catalog_rejects_bindings_missing_or_duplicating_fixed_auxiliaries() {
    let model = synthetic_model();

    for (auxiliary_artifact_ids, expected_error) in [
        (Vec::new(), "has invalid fixed llama.cpp auxiliary settings"),
        (
            vec!["projector".to_owned(), "projector".to_owned()],
            "has an invalid artifact binding",
        ),
    ] {
        let mut invalid = model.clone();
        invalid.configurations[0].artifact_bindings[0].auxiliary_artifact_ids =
            auxiliary_artifact_ids;
        let error = catalog_builder()
            .engine_validator(dotcore::model::engine::declaration_validator())
            .engine_release("engine.yaml", synthetic_engine_release())
            .model("model.yaml", invalid)
            .build()
            .expect_err("invalid fixed auxiliary binding")
            .to_string();

        assert!(
            error.contains(expected_error),
            "unexpected validation error: {error}"
        );
    }
}

#[test]
fn catalog_rejects_invalid_parameters_and_unsupported_bindings() {
    let catalog = synthetic_local_catalog();
    let model_id = CatalogModelId("fixture-model".to_owned());
    let (target, inventory) = target_capabilities(ExecutionPlatform::linux_amd64());
    let invalid = dotcore_proto::dotcore::catalog::v1::EngineParameterValues {
        values: vec![dotcore_proto::dotcore::catalog::v1::EngineParameterValue {
            parameter_id: "total-context".to_owned(),
            value: Some(
                dotcore_proto::dotcore::catalog::v1::engine_parameter_value::Value::UnsignedInteger(
                    16_777_217,
                ),
            ),
        }],
    };

    let invalid_configuration = LocalModelDeploymentConfiguration {
        model_artifact_binding_id: "default".to_owned(),
        engine_parameter_values: Some(invalid.clone()),
        ..Default::default()
    };
    assert_eq!(
        dotcore::model::resolution::resolve_local_launch(
            &catalog,
            &model_id,
            "balanced",
            &invalid_configuration,
            &target,
            &inventory,
            None,
        )
        .expect_err("out-of-range parameter")
        .to_string(),
        "invalid engine parameter values: profile \"balanced\" engine parameter \"total-context\" value 16777217 is outside its range 1024..=16777216"
    );
    assert_eq!(
        dotcore::model::resolution::resolve_local_launch(
            &catalog,
            &model_id,
            "balanced",
            &LocalModelDeploymentConfiguration {
                model_artifact_binding_id: "missing".to_owned(),
                engine_parameter_values: Some(invalid),
                ..Default::default()
            },
            &target,
            &inventory,
            None,
        )
        .expect_err("unsupported binding")
        .to_string(),
        "could not resolve catalog model: Invalid catalog: selected model artifact binding is not supported by the profile configuration"
    );
}

/// A llama.cpp launch caps each request at its context per request, filled from the model's
/// 262144-token maximum when omitted, so a 1048576-token total context resolves 262144 tokens per
/// request and caps each slot there. A supplied context per request above the maximum or above
/// the total context is rejected.
#[test]
fn llama_cpp_launch_rejects_a_context_per_request_above_the_model_maximum() {
    let catalog = synthetic_local_catalog();
    let model_id = CatalogModelId("fixture-model".to_owned());
    let (target, inventory) = target_capabilities(ExecutionPlatform::linux_amd64());
    let launch = |values: &[(&str, u64)]| {
        resolve_local_launch(
            &catalog,
            &model_id,
            "balanced",
            &LocalModelDeploymentConfiguration {
                model_artifact_binding_id: "default".to_owned(),
                engine_parameter_values: Some(unsigned_values(values)),
                ..Default::default()
            },
            &target,
            &inventory,
            None,
        )
    };

    let filled = launch(&[
        ("total-context", 1_048_576),
        ("prompt-cache-memory-mib", 0),
        ("parallel-requests", 4),
    ])
    .expect("an omitted context per request fills to the maximum");
    assert_eq!(
        filled
            .engine_parameter_values
            .as_ref()
            .and_then(|values| {
                values
                    .values
                    .iter()
                    .find(|value| value.parameter_id == "context-per-request")
            })
            .and_then(|value| value.value),
        Some(engine_parameter_value::Value::UnsignedInteger(262_144))
    );
    let arguments = filled
        .desired_launch_plan
        .as_ref()
        .expect("desired launch plan")
        .arguments
        .iter()
        .filter_map(|argument| match argument.value.as_ref() {
            Some(PortableArgument::Literal(literal)) => Some(literal.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        arguments
            .windows(2)
            .any(|pair| pair == ["--kv-unified-per-slot", "262144"]),
        "{arguments:?}"
    );

    let error = launch(&[
        ("total-context", 1_048_576),
        ("context-per-request", 262_145),
        ("prompt-cache-memory-mib", 0),
        ("parallel-requests", 4),
    ])
    .expect_err("context per request above the maximum");
    assert!(
        matches!(
            &error,
            ModelLaunchResolutionError::ContextAboveMaximum {
                context_tokens: 262_145,
                maximum_context_tokens: 262_144,
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "engine parameter values give each request 262145 tokens of context, above the maximum context of 262144 tokens of configuration \"local\" of catalog model \"fixture-model\"; reduce the context per request"
    );
    assert_eq!(
        launch(&[
            ("total-context", 32_768),
            ("context-per-request", 65_536),
            ("prompt-cache-memory-mib", 0),
            ("parallel-requests", 4),
        ])
        .expect_err("context per request above the total context")
        .to_string(),
        "invalid engine parameter values: context per request of 65536 tokens is above the total context of 32768 tokens; lower the context per request or raise the total context"
    );
}

/// A configuration maximum of 65536 tokens lowers the fixture's 262144-token model maximum, so an
/// omitted context per request fills to 65536 tokens and a larger one is rejected.
#[test]
fn llama_cpp_launch_rejects_a_context_per_request_above_the_configuration_maximum() {
    let mut model = synthetic_model();
    model.configurations[0].maximum_context_tokens = Some(65_536);
    let catalog = catalog_builder()
        .engine_validator(dotcore::model::engine::declaration_validator())
        .engine_release("engine.yaml", synthetic_engine_release())
        .model("model.yaml", model)
        .build()
        .expect("catalog with a configuration maximum");
    let model_id = CatalogModelId("fixture-model".to_owned());
    let (target, inventory) = target_capabilities(ExecutionPlatform::linux_amd64());
    let launch = |context_per_request: Option<u64>| {
        let mut values = vec![
            ("total-context", 262_144),
            ("prompt-cache-memory-mib", 0),
            ("parallel-requests", 4),
        ];
        values.extend(context_per_request.map(|value| ("context-per-request", value)));
        resolve_local_launch(
            &catalog,
            &model_id,
            "balanced",
            &LocalModelDeploymentConfiguration {
                model_artifact_binding_id: "default".to_owned(),
                engine_parameter_values: Some(unsigned_values(&values)),
                ..Default::default()
            },
            &target,
            &inventory,
            None,
        )
    };

    let error =
        launch(Some(65_537)).expect_err("context per request above the configuration maximum");
    assert!(
        matches!(
            &error,
            ModelLaunchResolutionError::ContextAboveMaximum {
                context_tokens: 65_537,
                maximum_context_tokens: 65_536,
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "engine parameter values give each request 65537 tokens of context, above the maximum context of 65536 tokens of configuration \"local\" of catalog model \"fixture-model\"; reduce the context per request"
    );
    launch(Some(65_536)).expect("context per request at the configuration maximum");
    launch(None).expect("an omitted context per request fills to the configuration maximum");
}

#[test]
fn vllm_launch_rejects_a_context_per_request_above_the_model_maximum() {
    // The fixture site catalog holds only the model; its engine release is in the master catalog.
    let site_root = temporary_catalog_root();
    create_empty_catalog_source(&site_root);
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../fixtures/vllm-nvidia-site/catalog/models/qwen35-08b-qualification.yaml"),
        site_root.join("models/qwen35-08b-qualification.yaml"),
    )
    .expect("copy vLLM fixture model");
    let catalog = Catalog::load_sources_with_policy(
        &repository_catalog_root(),
        &site_root,
        dotcore_catalog::OciArtifactPolicy::Development,
        dotcore::model::engine::declaration_validator(),
    )
    .expect("vLLM fixture catalog");
    std::fs::remove_dir_all(site_root).expect("remove fixture catalog");
    let model_id = CatalogModelId("qwen35-08b-qualification".to_owned());
    let (target, inventory) = target_capabilities(ExecutionPlatform::linux_amd64());
    // The total context holds one request at the context per request.
    let launch = |context_per_request: u64| {
        let (binding_id, mut values) = catalog
            .local_defaults_for(&model_id, "small-text")
            .expect("profile defaults");
        values
            .values
            .retain(|value| value.parameter_id != "total-context");
        values.values.extend(
            unsigned_values(&[
                ("total-context", context_per_request),
                ("context-per-request", context_per_request),
            ])
            .values,
        );
        resolve_local_launch(
            &catalog,
            &model_id,
            "small-text",
            &LocalModelDeploymentConfiguration {
                model_artifact_binding_id: binding_id,
                engine_parameter_values: Some(values),
                ..Default::default()
            },
            &target,
            &inventory,
            None,
        )
    };

    assert_eq!(
        launch(262_145)
            .expect_err("context per request above the maximum")
            .to_string(),
        "engine parameter values give each request 262145 tokens of context, above the maximum context of 262144 tokens of configuration \"small-vllm-nvidia\" of catalog model \"qwen35-08b-qualification\"; reduce the context per request"
    );
    launch(4096).expect("context per request within the maximum");
}

#[test]
fn catalog_retains_boolean_engine_parameters_and_their_schema() {
    let catalog = synthetic_boolean_catalog(synthetic_boolean_model());
    let model_id = CatalogModelId("fixture-model".to_owned());
    let (target, inventory) = target_capabilities(ExecutionPlatform::linux_amd64());
    let (_, defaults) = catalog
        .local_defaults_for(&model_id, "balanced")
        .expect("validated boolean defaults");

    let snapshot = dotcore::model::resolution::resolve_local_launch(
        &catalog,
        &model_id,
        "balanced",
        &LocalModelDeploymentConfiguration {
            model_artifact_binding_id: "default".to_owned(),
            engine_parameter_values: Some(defaults),
            ..Default::default()
        },
        &target,
        &inventory,
        None,
    )
    .expect("boolean parameter resolves");

    assert_eq!(
        snapshot
            .engine_parameter_values
            .as_ref()
            .expect("resolved values")
            .values
            .iter()
            .find(|value| value.parameter_id == "feature-enabled")
            .and_then(|value| value.value.as_ref()),
        Some(&dotcore_proto::dotcore::catalog::v1::engine_parameter_value::Value::Boolean(false))
    );
    assert_eq!(
        snapshot
            .engine_parameter_schema
            .as_ref()
            .expect("retained schema"),
        synthetic_boolean_engine_release()
            .parameter_schema
            .as_ref()
            .expect("fixture schema")
    );
}

#[test]
fn catalog_rejects_mismatched_boolean_values_and_invalid_profile_exposure() {
    let mut mismatched = synthetic_boolean_model();
    mismatched.profiles[0]
        .engine_parameter_defaults
        .as_mut()
        .expect("defaults")
        .values
        .iter_mut()
        .find(|value| value.parameter_id == "feature-enabled")
        .expect("feature-enabled default")
        .value = Some(
        dotcore_proto::dotcore::catalog::v1::engine_parameter_value::Value::UnsignedInteger(1),
    );
    let error = catalog_builder()
        .engine_validator(&ACCEPT_ENGINE_DECLARATIONS)
        .engine_release("engine.yaml", synthetic_boolean_engine_release())
        .model("model.yaml", mismatched)
        .build()
        .expect_err("mismatched boolean value")
        .to_string();
    assert!(
        error.contains(
            "profile \"balanced\" engine parameter \"feature-enabled\" has a value of the wrong type"
        ),
        "unexpected validation error: {error}"
    );

    for exposed_parameter_ids in [
        vec!["total-context".to_owned(), "missing".to_owned()],
        vec!["total-context".to_owned(), "total-context".to_owned()],
    ] {
        let mut invalid = synthetic_boolean_model();
        invalid.profiles[0].exposed_parameter_ids = exposed_parameter_ids;
        let error = catalog_builder()
            .engine_validator(&ACCEPT_ENGINE_DECLARATIONS)
            .engine_release("engine.yaml", synthetic_boolean_engine_release())
            .model("model.yaml", invalid)
            .build()
            .expect_err("invalid profile exposure")
            .to_string();
        assert!(
            error.contains("profile \"balanced\" has invalid exposed engine parameters"),
            "unexpected validation error: {error}"
        );
    }
}

/// Returns a fresh target of the production runtime kind for `platform`: Docker with an NVIDIA
/// driver on Linux, and a simulated stand-in for the native runtime with Apple facts on darwin.
fn target_capabilities(
    platform: ExecutionPlatform,
) -> (ExecutionTarget, TargetCapabilityInventory) {
    use dotcore_proto::dotcore::site::v1::{
        ModelRuntimeBinding, execution_target::RuntimeBinding, model_runtime_binding::Adapter,
    };

    let linux = platform.is_linux();
    let adapter = if linux {
        Adapter::DockerRuntime(Default::default())
    } else {
        Adapter::SimulatedRuntime(Default::default())
    };
    let target = ExecutionTarget {
        execution_target_id: "fixture-target".to_owned(),
        display_name: "fixture-target".to_owned(),
        site_node_id: "fixture-node".to_owned(),
        accelerator_device_ids: vec!["0".to_owned()],
        platform: Some(platform.to_oci_platform()),
        runtime_binding: Some(RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
            adapter: Some(adapter),
        })),
    };
    let accelerator = if linux {
        NodeAcceleratorObservation::nvidia("fixture-node", "580").with_device_memory(
            "GPU-0",
            Some(34_190_917_632),
            None,
        )
    } else {
        NodeAcceleratorObservation::available("fixture-node", NodeAcceleratorFacts::apple())
            .with_host_memory(34_190_917_632, 34_190_917_632)
    };
    let inventory = TargetCapabilityReconciler::reconcile(
        [&target],
        [RuntimePlatformObservation::available(
            "fixture-target",
            platform,
        )],
        [accelerator],
    );
    (target, inventory)
}

/// Returns the lowest total context a profile's largest fit considers, with the context per
/// request its defaults supply: the engine's smallest total context, raised to that context per
/// request.
fn lowest_total_context(catalog: &Catalog, model: &Model, profile_id: &str) -> (u32, Option<u32>) {
    use dotcore_catalog::shared_parameters::{
        CONTEXT_PER_REQUEST_PARAMETER_ID, TOTAL_CONTEXT_PARAMETER_ID,
    };
    use dotcore_proto::dotcore::catalog::v1::engine_parameter_definition::ParameterType;

    let profile = model
        .profiles
        .iter()
        .find(|profile| profile.profile_id == profile_id)
        .expect("the profile");
    let release_id = model
        .configurations
        .iter()
        .find(|configuration| configuration.configuration_id == profile.configuration_id)
        .and_then(|configuration| configuration.engine_artifacts.first())
        .map(|artifact| artifact.engine_release_id.clone())
        .expect("the profile's engine release");
    let smallest_total = catalog
        .list_engine_releases()
        .into_iter()
        .find(|release| release.engine_release_id == release_id)
        .and_then(|release| release.parameter_schema)
        .and_then(|schema| {
            schema
                .parameters
                .into_iter()
                .find(|parameter| parameter.parameter_id == TOTAL_CONTEXT_PARAMETER_ID)
        })
        .and_then(|parameter| match parameter.parameter_type {
            Some(ParameterType::UnsignedInteger(range)) => Some(range.minimum),
            _ => None,
        })
        .expect("the engine's total context range");
    let context_per_request = profile
        .engine_parameter_defaults
        .iter()
        .flat_map(|defaults| &defaults.values)
        .find(|value| value.parameter_id == CONTEXT_PER_REQUEST_PARAMETER_ID)
        .and_then(|value| match value.value {
            Some(engine_parameter_value::Value::UnsignedInteger(tokens)) => Some(tokens),
            _ => None,
        });
    let lowest = context_per_request.map_or(smallest_total, |tokens| tokens.max(smallest_total));
    (
        u32::try_from(lowest).expect("a total context in tokens"),
        context_per_request.map(|tokens| u32::try_from(tokens).expect("a context in tokens")),
    )
}

/// Returns the simulated hardware `choice` alone: one named GPU on the simulated host, or one named
/// machine.
fn simulated_alone(choice: &str) -> dotcore_node_hardware::MachineDescription {
    dotcore_node_hardware::MachineDescription::simulated(&[choice.to_owned()], "amd64")
        .expect("simulated hardware")
}

/// Returns the model target on GPU `gpu` of simulated hardware `machine` and the inventory its
/// idle reading reconciles to: a Docker target on Linux, a native one on macOS.
fn simulated_target(machine: &str, gpu: usize) -> (ExecutionTarget, TargetCapabilityInventory) {
    use dotcore_proto::dotcore::site::v1::{
        ModelRuntimeBinding, execution_target::RuntimeBinding, model_runtime_binding::Adapter,
    };

    let description = simulated_alone(machine);
    let platform = description.platform().clone();
    let adapter = if platform.operating_system() == "darwin" {
        Adapter::NativeRuntime(Default::default())
    } else {
        Adapter::DockerRuntime(Default::default())
    };
    let target = ExecutionTarget {
        execution_target_id: "machine-target".to_owned(),
        display_name: "machine-target".to_owned(),
        site_node_id: "machine-node".to_owned(),
        accelerator_device_ids: vec![description.gpu_ids().nth(gpu).expect("the GPU").to_owned()],
        platform: Some(platform.to_oci_platform()),
        runtime_binding: Some(RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
            adapter: Some(adapter),
        })),
    };
    let snapshot = description
        .render(&[], std::time::SystemTime::now())
        .expect("rendered snapshot");
    let inventory = TargetCapabilityReconciler::reconcile(
        [&target],
        [RuntimePlatformObservation::available(
            "machine-target",
            platform,
        )],
        [NodeAcceleratorObservation::from_hardware_snapshot(
            "machine-node",
            &snapshot,
            false,
        )],
    );
    (target, inventory)
}

/// Returns a fresh target of the given runtime adapter and platform whose node reports `facts`,
/// optionally overriding the observed memory architecture.
fn accelerator_target(
    adapter: dotcore_proto::dotcore::site::v1::model_runtime_binding::Adapter,
    platform: ExecutionPlatform,
    facts: NodeAcceleratorFacts,
    memory_architecture: Option<MemoryArchitecture>,
) -> (ExecutionTarget, TargetCapabilityInventory) {
    use dotcore_proto::dotcore::site::v1::{ModelRuntimeBinding, execution_target::RuntimeBinding};

    let target = ExecutionTarget {
        execution_target_id: "facts-target".to_owned(),
        display_name: "facts-target".to_owned(),
        site_node_id: "facts-node".to_owned(),
        accelerator_device_ids: vec!["0".to_owned()],
        platform: Some(platform.to_oci_platform()),
        runtime_binding: Some(RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
            adapter: Some(adapter),
        })),
    };
    let mut accelerator = NodeAcceleratorObservation::available("facts-node", facts);
    if memory_architecture.is_some() {
        accelerator = accelerator.with_memory_architecture(memory_architecture);
    }
    let inventory = TargetCapabilityReconciler::reconcile(
        [&target],
        [RuntimePlatformObservation::available(
            "facts-target",
            platform,
        )],
        [accelerator],
    );
    (target, inventory)
}

/// Returns a fresh darwin/arm64 native runtime target with Apple accelerator facts.
fn macos_native_target_capabilities() -> (ExecutionTarget, TargetCapabilityInventory) {
    use dotcore_proto::dotcore::site::v1::{
        ModelRuntimeBinding, execution_target::RuntimeBinding, model_runtime_binding::Adapter,
    };

    let platform = ExecutionPlatform::darwin_arm64();
    let target = ExecutionTarget {
        execution_target_id: "native-target".to_owned(),
        display_name: "native-target".to_owned(),
        site_node_id: "mac-node".to_owned(),
        accelerator_device_ids: vec!["0".to_owned()],
        platform: Some(platform.to_oci_platform()),
        runtime_binding: Some(RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
            adapter: Some(Adapter::NativeRuntime(Default::default())),
        })),
    };
    let inventory = TargetCapabilityReconciler::reconcile(
        [&target],
        [RuntimePlatformObservation::available(
            "native-target",
            platform,
        )],
        [
            NodeAcceleratorObservation::available("mac-node", NodeAcceleratorFacts::apple())
                .with_host_memory(34_190_917_632, 34_190_917_632),
        ],
    );
    (target, inventory)
}

/// Returns the platforms a model configuration's approved artifacts declare in the artifact kind
/// and accelerator the platform's production runtime launches: OCI with NVIDIA on Linux and Nix
/// with Apple on darwin.
fn configuration_platforms(
    releases: &[EngineRelease],
    model: &Model,
    configuration_id: &str,
) -> BTreeSet<ExecutionPlatform> {
    let configuration = model
        .configurations
        .iter()
        .find(|configuration| configuration.configuration_id == configuration_id)
        .expect("profile configuration");
    configuration
        .engine_artifacts
        .iter()
        .filter_map(|reference| {
            releases
                .iter()
                .find(|release| release.engine_release_id == reference.engine_release_id)?
                .artifacts
                .iter()
                .find(|artifact| artifact.artifact_id == reference.engine_artifact_id)
        })
        .flat_map(|artifact| {
            artifact.selectors.iter().filter_map(|selector| {
                let platform = selector.platform.as_ref()?;
                let platform = ExecutionPlatform::normalize(
                    &platform.operating_system,
                    &platform.architecture,
                )
                .ok()?;
                // Images run in Docker on NVIDIA Linux hosts; native packages on Apple hosts.
                let production = match artifact.kind.as_ref()? {
                    EngineArtifactKind::Oci(_) => {
                        platform.is_linux() && selector.accelerators == ["nvidia"]
                    }
                    EngineArtifactKind::Nix(nix)
                        if dotcore_domain::nix::provides_docker_image(nix) =>
                    {
                        platform.is_linux() && selector.accelerators == ["nvidia"]
                    }
                    EngineArtifactKind::Nix(_) => {
                        !platform.is_linux() && selector.accelerators == ["apple"]
                    }
                };
                production.then_some(platform)
            })
        })
        .collect()
}

fn repository_catalog() -> Catalog {
    Catalog::load_with_policy(
        &repository_catalog_root(),
        dotcore_catalog::OciArtifactPolicy::Development,
        dotcore::model::engine::declaration_validator(),
    )
    .expect("repository catalog")
}

fn repository_catalog_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("catalog")
}

/// Builds unsigned engine parameter values from parameter id and value pairs.
fn unsigned_values(
    values: &[(&str, u64)],
) -> dotcore_proto::dotcore::catalog::v1::EngineParameterValues {
    dotcore_proto::dotcore::catalog::v1::EngineParameterValues {
        values: values
            .iter()
            .map(|(parameter_id, value)| {
                dotcore_proto::dotcore::catalog::v1::EngineParameterValue {
                    parameter_id: (*parameter_id).to_owned(),
                    value: Some(engine_parameter_value::Value::UnsignedInteger(*value)),
                }
            })
            .collect(),
    }
}

/// Single-platform allowlist entry: the engine, the only platform its releases build for, and why.
struct SinglePlatformEngine {
    engine_kind: EngineKind,
    platform: ExecutionPlatform,
    reason: &'static str,
}

/// Returns the allowlist entry of the engine whose releases a model configuration approves, if any.
fn single_platform_engine(
    releases: &[EngineRelease],
    model: &Model,
    configuration_id: &str,
) -> Option<&'static SinglePlatformEngine> {
    let configuration = model
        .configurations
        .iter()
        .find(|configuration| configuration.configuration_id == configuration_id)
        .expect("profile configuration");
    SINGLE_PLATFORM_ENGINES.iter().find(|entry| {
        configuration.engine_artifacts.iter().any(|reference| {
            releases.iter().any(|release| {
                release.engine_release_id == reference.engine_release_id
                    && release.engine_kind == entry.engine_kind as i32
            })
        })
    })
}

/// Resolves a local launch the way deployment acceptance does, standing in a placeholder image ID
/// for the node-local identity a tag-only engine image needs.
fn resolve_local_launch_with_placeholder_identity(
    catalog: &Catalog,
    model_id: &CatalogModelId,
    profile_id: &str,
    configuration: &LocalModelDeploymentConfiguration,
    target: &ExecutionTarget,
    inventory: &TargetCapabilityInventory,
) -> Result<ResolvedLocalModelDeploymentSnapshot, ModelLaunchResolutionError> {
    let selection = select_engine_artifact(
        catalog,
        model_id,
        profile_id,
        configuration,
        target,
        inventory,
    )?;
    // Nix engine artifacts never take an identity: a locked one is precomputed, and the hosting
    // node pins a development-source one.
    let identity = match selection {
        EngineArtifactSelection::OciImage {
            reference,
            tag_only: true,
        } => Some(placeholder_image_identity(&reference)),
        EngineArtifactSelection::OciImage { .. } | EngineArtifactSelection::Nix(_) => None,
    };
    resolve_local_launch(
        catalog,
        model_id,
        profile_id,
        configuration,
        target,
        inventory,
        identity.as_ref(),
    )
}

/// Stands in for the local image ID a tag-only engine image `reference` resolves to on a node.
fn placeholder_image_identity(reference: &str) -> EngineLaunchIdentity {
    EngineLaunchIdentity {
        reference: reference.to_owned(),
        image_id: PLACEHOLDER_IMAGE_ID.to_owned(),
    }
}

fn synthetic_local_catalog() -> Catalog {
    catalog_builder()
        .engine_validator(dotcore::model::engine::declaration_validator())
        .engine_release("engine.yaml", synthetic_engine_release())
        .model("model.yaml", synthetic_model())
        .build()
        .expect("synthetic local catalog")
}

/// Loads a fixture catalog whose one configuration declares accelerator and host memory equations.
fn synthetic_sizing_catalog() -> Catalog {
    let root = temporary_catalog_root();
    create_empty_catalog_source(&root);
    write_sizing_engine(&root);
    write_sizing_model_with(
        &root,
        r#"    sizing:
      bindingParameters: ["base"]
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "${base} + 10 * ${total-context} + ${prompt-cache-memory-mib} * MiB"
        hostMemory:
          unit: bytes
          equation: "${parallel-requests} * MiB"
"#,
        r#"        bindingSizing:
          - bindingId: "sized"
            parameters: { base: 1000 }
          - bindingId: "second"
            parameters: { base: 1000 }
"#,
        &format!(
            r#"      values:
{SIZING_ENGINE_CACHE_DEFAULT}    exposedParameterIds: ["total-context", "prompt-cache-memory-mib", "parallel-requests"]
"#
        ),
    );
    let catalog = Catalog::load_with_policy(
        &root,
        dotcore_catalog::OciArtifactPolicy::Development,
        dotcore::model::engine::declaration_validator(),
    )
    .expect("synthetic sizing catalog");
    std::fs::remove_dir_all(root).expect("remove fixture catalog");
    catalog
}

fn synthetic_boolean_catalog(model: Model) -> Catalog {
    catalog_builder()
        .engine_validator(&ACCEPT_ENGINE_DECLARATIONS)
        .engine_release("engine.yaml", synthetic_boolean_engine_release())
        .model("model.yaml", model)
        .build()
        .expect("synthetic boolean catalog")
}

fn synthetic_boolean_engine_release() -> EngineRelease {
    let mut engine = synthetic_engine_release();
    engine
        .parameter_schema
        .as_mut()
        .expect("fixture schema")
        .parameters
        .push(dotcore_proto::dotcore::catalog::v1::EngineParameterDefinition {
            parameter_id: "feature-enabled".to_owned(),
            display_name: "Feature enabled".to_owned(),
            parameter_type: Some(
                dotcore_proto::dotcore::catalog::v1::engine_parameter_definition::ParameterType::Boolean(
                    dotcore_proto::dotcore::catalog::v1::BooleanEngineParameterSchema {},
                ),
            ),
            exposed: true,
            description: "Enables the synthetic feature.".to_owned(),
        });
    engine
}

fn synthetic_boolean_model() -> Model {
    let mut model = synthetic_model();
    let profile = &mut model.profiles[0];
    profile
        .engine_parameter_defaults
        .as_mut()
        .expect("fixture defaults")
        .values
        .push(dotcore_proto::dotcore::catalog::v1::EngineParameterValue {
            parameter_id: "feature-enabled".to_owned(),
            value: Some(
                dotcore_proto::dotcore::catalog::v1::engine_parameter_value::Value::Boolean(false),
            ),
        });
    profile.exposed_parameter_ids = vec!["total-context".to_owned(), "feature-enabled".to_owned()];
    model
}

fn synthetic_engine_release() -> EngineRelease {
    dotcore_proto::yaml::from_str(
        r#"engineReleaseId: "fixture-engine"
engineKind: "ENGINE_KIND_LLAMA_CPP"
version: "fixture"
parameterSchema:
  schemaId: "fixture-shared"
  parameters:
    - parameterId: "total-context"
      displayName: "Total context"
      exposed: true
      unsignedInteger:
        minimum: "1024"
        maximum: "16777216"
        unit: "tokens"
        defaultValue: "32768"
    - parameterId: "context-per-request"
      displayName: "Context per request"
      exposed: true
      unsignedInteger:
        minimum: "1024"
        maximum: "16777216"
        unit: "tokens"
    - parameterId: "parallel-requests"
      displayName: "Parallel requests"
      exposed: true
      unsignedInteger:
        minimum: "1"
        maximum: "64"
        unit: "requests"
        defaultValue: "4"
    - parameterId: "prompt-cache-memory-mib"
      displayName: "Prompt cache memory"
      unsignedInteger:
        minimum: "0"
        maximum: "65536"
        unit: "MiB"
artifacts:
  - artifactId: "linux-amd64"
    oci:
      repository:
        registry:
          host: "example.invalid"
        name: "fixture/engine"
      digest:
        algorithm: "sha256"
        encoded: "1111111111111111111111111111111111111111111111111111111111111111"
    selectors:
      - platform: { operatingSystem: "linux", architecture: "amd64" }
        accelerators: ["nvidia"]
        minimumNvidiaDriverMajorVersion: 580
      - platform: { operatingSystem: "linux", architecture: "arm64" }
        accelerators: ["nvidia"]
        minimumNvidiaDriverMajorVersion: 580
"#,
    )
    .expect("synthetic engine release")
}

/// Builds a catalog holding the synthetic Linux release, `macos_release`, and a model with one
/// configuration and profile per release.
fn macos_catalog(macos_release: EngineRelease) -> Catalog {
    try_macos_catalog(macos_release).expect("synthetic macOS catalog")
}

fn try_macos_catalog(
    macos_release: EngineRelease,
) -> Result<Catalog, dotcore_catalog::CatalogError> {
    catalog_builder()
        .engine_validator(dotcore::model::engine::declaration_validator())
        .engine_release("engine.yaml", synthetic_engine_release())
        .engine_release("engine-macos.yaml", macos_release)
        .model("model.yaml", synthetic_two_release_model())
        .build()
}

/// A second llama.cpp release for darwin/arm64 whose one artifact is a development-source Nix
/// package; it shares the Linux release's parameter schema.
fn synthetic_macos_engine_release() -> EngineRelease {
    let mut release = synthetic_engine_release();
    release.engine_release_id = "fixture-engine-macos".to_owned();
    release.version = "fixture-macos".to_owned();
    release.artifacts = vec![
        dotcore_proto::yaml::from_str(
            r#"artifactId: "metal"
nix:
  flake: "self"
  package: "engines.llama-cpp.native"
  executable: "fixture-server"
selectors:
  - platform: { operatingSystem: "darwin", architecture: "arm64" }
    accelerators: ["apple"]
"#,
        )
        .expect("synthetic Nix engine artifact"),
    ];
    release
}

/// The synthetic model with a second configuration and profile approving the macOS release on the
/// `apple` hardware class.
fn synthetic_two_release_model() -> Model {
    let mut model = synthetic_model();
    let mut configuration = model.configurations[0].clone();
    configuration.configuration_id = "metal".to_owned();
    configuration.engine_artifacts = vec![EngineArtifactReference {
        engine_release_id: "fixture-engine-macos".to_owned(),
        engine_artifact_id: "metal".to_owned(),
    }];
    configuration.hardware_support = vec![HardwareSupport {
        hardware_class_id: "apple".to_owned(),
        hardware_requirements: None,
    }];
    let mut profile = model.profiles[0].clone();
    profile.profile_id = "metal".to_owned();
    profile.configuration_id = "metal".to_owned();
    model.configurations.push(configuration);
    model.profiles.push(profile);
    model
}

/// Returns the catalog store paths a snapshot engine's Nix artifact retains.
fn retained_store_paths(
    engine: &dotcore_proto::dotcore::model::v1::ResolvedEngineArtifact,
) -> Vec<NixSystemStorePath> {
    match engine
        .artifact
        .as_ref()
        .and_then(|artifact| artifact.kind.as_ref())
    {
        Some(EngineArtifactKind::Nix(nix)) => nix.store_paths.clone(),
        _ => panic!("snapshot engine has no Nix artifact"),
    }
}

fn nix_artifact_mut(
    release: &mut EngineRelease,
) -> &mut dotcore_proto::dotcore::catalog::v1::NixArtifact {
    match release.artifacts[0].kind.as_mut() {
        Some(EngineArtifactKind::Nix(nix)) => nix,
        _ => panic!("synthetic macOS release has no Nix artifact"),
    }
}

fn darwin_store_path() -> NixSystemStorePath {
    NixSystemStorePath {
        system: "aarch64-darwin".to_owned(),
        store_path: PLACEHOLDER_STORE_PATH.to_owned(),
    }
}

fn default_configuration(catalog: &Catalog, profile_id: &str) -> LocalModelDeploymentConfiguration {
    let (binding_id, defaults) = catalog
        .local_defaults_for(&CatalogModelId("fixture-model".to_owned()), profile_id)
        .expect("profile defaults");
    LocalModelDeploymentConfiguration {
        model_artifact_binding_id: binding_id,
        engine_parameter_values: Some(defaults),
        ..Default::default()
    }
}

/// Reads the retained Nix engine back from a snapshot without the catalog: policy, store path,
/// source provenance, and platform.
fn retained_nix_engine(
    snapshot: &ModelDeploymentSnapshot,
) -> (
    ArtifactPreparationPolicy,
    String,
    ResolvedEngineSource,
    (String, String),
) {
    let engine = ResolvedModelLaunch::from_snapshot(snapshot)
        .expect("self-contained snapshot")
        .engine;
    (
        engine.preparation_policy,
        engine.launch_identity,
        engine.source,
        (
            engine.platform.operating_system,
            engine.platform.architecture,
        ),
    )
}

fn nvidia_selector(operating_system: &str, architecture: &str, variant: Option<&str>) -> Selector {
    Selector {
        platform: Some(OciPlatform {
            operating_system: operating_system.to_owned(),
            architecture: architecture.to_owned(),
            variant: variant.map(str::to_owned),
        }),
        accelerators: vec!["nvidia".to_owned()],
        minimum_nvidia_driver_major_version: 580,
        ..Default::default()
    }
}

fn apple_selector() -> Selector {
    Selector {
        platform: Some(OciPlatform {
            operating_system: "darwin".to_owned(),
            architecture: "arm64".to_owned(),
            variant: None,
        }),
        accelerators: vec!["apple".to_owned()],
        minimum_nvidia_driver_major_version: 0,
        ..Default::default()
    }
}

fn synthetic_model() -> Model {
    dotcore_proto::yaml::from_str(
        r#"modelId: "fixture-model"
displayName: "Fixture model"
version: "fixture"
maximumContextTokens: 262144
artifacts:
  - artifactId: "weights"
    weightQuantization: "FIXTURE"
    source:
      repository: "fixture/model"
      revision: "1111111111111111111111111111111111111111"
      file:
        path: "weights.gguf"
        provenanceDigest: "2222222222222222222222222222222222222222222222222222222222222222"
auxiliaryArtifacts:
  - auxiliaryArtifactId: "projector"
    source:
      repository: "fixture/model"
      revision: "1111111111111111111111111111111111111111"
      file:
        path: "projector.gguf"
        provenanceDigest: "3333333333333333333333333333333333333333333333333333333333333333"
configurations:
  - configurationId: "local"
    engineArtifacts:
      - engineReleaseId: "fixture-engine"
        engineArtifactId: "linux-amd64"
    artifactBindings:
      - bindingId: "default"
        modelArtifactId: "weights"
        auxiliaryArtifactIds: ["projector"]
    hardwareSupport:
      - hardwareClassId: "nvidia"
    engineConfiguration:
      engineKind: "ENGINE_KIND_LLAMA_CPP"
      llamaCpp:
        fixedSettings:
          gpuLayers: 1
          flashAttention: true
          visionProjector:
            auxiliaryArtifactId: "projector"
    executionCapabilities:
      supportedParallelShapes: ["MODEL_PARALLEL_SHAPE_ONE"]
    requestKinds: ['INFERENCE_REQUEST_KIND_CHAT']
    inputCapabilities:
      supportedModalities: ["MODEL_INPUT_MODALITY_TEXT", "MODEL_INPUT_MODALITY_IMAGE"]
      imageInputCapacity: {}
profiles:
  - profileId: "balanced"
    configurationId: "local"
    defaultModelArtifactBindingId: "default"
    engineParameterDefaults:
      values:
        - parameterId: "prompt-cache-memory-mib"
          unsignedInteger: "0"
"#,
    )
    .expect("synthetic model")
}

fn snapshot_file(path: &str, digest_character: char) -> HuggingFaceSnapshotFile {
    HuggingFaceSnapshotFile {
        path: path.to_owned(),
        provenance_digest: std::iter::repeat_n(digest_character, 64).collect(),
    }
}
fn set_file_path(
    source: Option<&mut dotcore_proto::dotcore::catalog::v1::HuggingFaceArtifactSource>,
    path: &str,
) {
    let Some(HuggingFaceContent::File(file)) =
        source.expect("Hugging Face source").content.as_mut()
    else {
        panic!("file source");
    };
    file.path = path.to_owned();
}

fn upstream_model_template() -> UpstreamModelTemplate {
    UpstreamModelTemplate {
        upstream_model_template_id: "openai-compatible".to_owned(),
        display_name: "OpenAI-compatible".to_owned(),
        wire_protocol: UpstreamWireProtocol::OpenaiCompatible as i32,
    }
}

fn first_upstream_model_template(snapshot: &ModelDeploymentSnapshot) -> UpstreamModelTemplate {
    match snapshot.source.as_ref() {
        Some(ModelDeploymentSnapshotSource::Upstream(upstream)) => upstream
            .upstream_model_template
            .clone()
            .expect("complete embedded upstream model template"),
        Some(ModelDeploymentSnapshotSource::Local(_)) | None => {
            panic!("upstream model snapshot")
        }
    }
}

/// Catalog builder with the shared `nvidia` and `apple` hardware classes synthetic models
/// reference.
fn catalog_builder() -> CatalogBuilder {
    CatalogBuilder::new()
        .hardware_class(
            "hardware-classes/nvidia.yaml",
            vendor_hardware_class("nvidia"),
        )
        .hardware_class(
            "hardware-classes/apple.yaml",
            vendor_hardware_class("apple"),
        )
}
