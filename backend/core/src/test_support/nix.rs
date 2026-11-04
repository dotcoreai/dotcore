//! Nix engine artifact catalog and snapshot fixtures for backend unit tests.

use dotcore_domain::{
    ExecutionPlatform,
    test_support::nix::{STORE_PATH, engine_artifact, locked_flake},
};
use dotcore_proto::dotcore::{
    catalog::v1::{
        EngineArtifact, NixArtifact, engine_artifact::Kind as EngineArtifactKind,
        engine_parameter_value,
    },
    deployment::v1::ArtifactPreparationPolicy,
    engine::v1::resolved_engine_model_state,
    model::v1::{ModelDeploymentSnapshot, model_deployment_snapshot},
    observation::v1::AcceleratorMemoryArchitecture,
};

/// Returns the fixture llama.cpp snapshot selected for an Apple native target: the darwin/arm64
/// Metal Nix artifact from a locked flake retaining [`STORE_PATH`], one Apple GPU requirement, full
/// GPU offload, unified accelerator memory, and a 2048 MiB prompt cache, with its launch plan
/// translated from those inputs.
pub(crate) fn apple_metal_snapshot(model_id: &str) -> ModelDeploymentSnapshot {
    let mut snapshot = super::local_model_snapshot(model_id);
    let Some(model_deployment_snapshot::Source::Local(local)) = snapshot.source.as_mut() else {
        panic!("Apple Metal fixture requires a local snapshot");
    };
    let engine = local.engine_artifact.as_mut().expect("snapshot engine");
    engine.engine_release_id = "llama-cpp-2026-10-19".to_owned();
    let mut artifact = EngineArtifact {
        selectors: Vec::new(),
        ..engine_artifact("metal", &locked_flake(), "darwin", "arm64", "apple")
    };
    if let Some(EngineArtifactKind::Nix(nix)) = artifact.kind.as_mut() {
        nix.store_paths = vec![dotcore_proto::dotcore::catalog::v1::NixSystemStorePath {
            system: "aarch64-darwin".to_owned(),
            store_path: STORE_PATH.to_owned(),
        }];
    }
    engine.artifact = Some(artifact);
    engine.platform = Some(ExecutionPlatform::darwin_arm64().to_oci_platform());
    engine.accelerator = "apple".to_owned();
    engine.minimum_nvidia_driver_major_version = 0;
    engine.preparation_policy = ArtifactPreparationPolicy::ImmutableRemote as i32;
    engine.launch_identity = STORE_PATH.to_owned();

    let hardware_support = local
        .hardware_support
        .as_mut()
        .expect("snapshot hardware support");
    hardware_support.hardware_class_id = "apple".to_owned();
    hardware_support
        .selector
        .as_mut()
        .expect("hardware class selector")
        .accelerators = vec!["apple".to_owned()];
    let Some(resolved_engine_model_state::State::LlamaCpp(state)) = local
        .resolved_engine_state
        .as_mut()
        .and_then(|state| state.state.as_mut())
    else {
        panic!("Apple Metal fixture requires a llama.cpp snapshot");
    };
    state
        .fixed_settings
        .as_mut()
        .expect("llama.cpp fixed settings")
        .gpu_layers = 99;
    local
        .selected_target
        .as_mut()
        .expect("selected target")
        .accelerator_memory_architecture = AcceleratorMemoryArchitecture::Unified as i32;
    local
        .engine_parameter_values
        .as_mut()
        .expect("engine parameter values")
        .values
        .iter_mut()
        .find(|value| value.parameter_id == "prompt-cache-memory-mib")
        .expect("prompt cache value")
        .value = Some(engine_parameter_value::Value::UnsignedInteger(2048));

    local.desired_launch_plan = Some(
        crate::model::engine::EngineIntegrationRegistry::production()
            .translate(local)
            .expect("Apple Metal fixture plan"),
    );
    snapshot
}

/// Loader store paths of the Docker-delivered fixture engine for each Nix system loading its
/// platforms.
pub(crate) const DOCKER_IMAGE_STORE_PATHS: [(&str, &str); 3] = [
    (
        "x86_64-linux",
        "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-docker-image-llama-cpp-cuda",
    ),
    (
        "aarch64-linux",
        "/nix/store/1123456789abcdfghijklmnpqrsvwxyz-docker-image-llama-cpp-cuda",
    ),
    (
        "aarch64-darwin",
        "/nix/store/2123456789abcdfghijklmnpqrsvwxyz-docker-image-llama-cpp-cuda",
    ),
];

/// Returns the simulated-site fixture catalog whose engine artifact is the Docker image
/// derivation package `engines.llama-cpp-cuda.docker` of `flake`, with the store paths of
/// [`DOCKER_IMAGE_STORE_PATHS`] for a locked flake.
pub(crate) fn docker_delivered_engine_catalog(flake: &str) -> dotcore_catalog::Catalog {
    let oci = r#"    oci:
      repository:
        registry: { host: "registry.dotcore.test" }
        name: "fixture"
      digest: { algorithm: "sha256", encoded: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" }
"#;
    let store_paths = if flake == "self" {
        String::new()
    } else {
        DOCKER_IMAGE_STORE_PATHS
            .iter()
            .map(|(system, store_path)| {
                format!("        - {{ system: \"{system}\", storePath: \"{store_path}\" }}\n")
            })
            .fold("      storePaths:\n".to_owned(), |paths, entry| {
                paths + &entry
            })
    };
    let nix = format!(
        "    nix:\n      flake: \"{flake}\"\n      package: \"engines.llama-cpp-cuda.docker\"\n      dockerImage: {{}}\n{store_paths}"
    );
    super::fixture_engine_catalog(|engine| {
        assert!(engine.contains(oci), "fixture engine is an OCI image");
        engine.replace(oci, &nix)
    })
}

/// Declares the snapshot's engine as the Docker image derivation package
/// `engines.llama-cpp-cuda.docker` of `flake`, retaining the catalog store paths of
/// [`DOCKER_IMAGE_STORE_PATHS`] for a locked flake, and `launch_identity` under the policy the
/// flake implies; an empty identity leaves the store path for the hosting SiteNode to pin.
pub(crate) fn retain_docker_delivered_engine(
    snapshot: &mut ModelDeploymentSnapshot,
    flake: &str,
    launch_identity: &str,
) {
    let Some(model_deployment_snapshot::Source::Local(local)) = snapshot.source.as_mut() else {
        panic!("Docker-delivered engine requires a local snapshot");
    };
    let locked = flake != "self";
    let engine = local.engine_artifact.as_mut().expect("snapshot engine");
    engine
        .artifact
        .as_mut()
        .expect("snapshot engine artifact")
        .kind = Some(EngineArtifactKind::Nix(NixArtifact {
        flake: flake.to_owned(),
        package: Some("engines.llama-cpp-cuda.docker".to_owned()),
        provides: Some(
            dotcore_proto::dotcore::catalog::v1::nix_artifact::Provides::DockerImage(
                dotcore_proto::dotcore::catalog::v1::NixDockerImage {},
            ),
        ),
        store_paths: if locked {
            DOCKER_IMAGE_STORE_PATHS
                .map(|(system, store_path)| {
                    dotcore_proto::dotcore::catalog::v1::NixSystemStorePath {
                        system: system.to_owned(),
                        store_path: store_path.to_owned(),
                    }
                })
                .to_vec()
        } else {
            Vec::new()
        },
    }));
    engine.preparation_policy = if locked {
        ArtifactPreparationPolicy::ImmutableRemote
    } else {
        ArtifactPreparationPolicy::DevelopmentSource
    } as i32;
    engine.launch_identity = launch_identity.to_owned();
}

/// Declares the snapshot's engine as package `engines.llama-cpp.native` of the development source
/// (`self`), running `llama-server`, and retains `launch_identity` under the development-source
/// policy; an empty identity leaves the store path for the hosting SiteNode to pin.
pub(crate) fn retain_development_source_engine(
    snapshot: &mut ModelDeploymentSnapshot,
    launch_identity: &str,
) {
    let Some(model_deployment_snapshot::Source::Local(local)) = snapshot.source.as_mut() else {
        panic!("development-source engine requires a local snapshot");
    };
    let engine = local.engine_artifact.as_mut().expect("snapshot engine");
    engine
        .artifact
        .as_mut()
        .expect("snapshot engine artifact")
        .kind = Some(EngineArtifactKind::Nix(NixArtifact {
        flake: "self".to_owned(),
        package: Some("engines.llama-cpp.native".to_owned()),
        provides: Some(
            dotcore_proto::dotcore::catalog::v1::nix_artifact::Provides::Executable(
                "llama-server".to_owned(),
            ),
        ),
        store_paths: Vec::new(),
    }));
    engine.preparation_policy = ArtifactPreparationPolicy::DevelopmentSource as i32;
    engine.launch_identity = launch_identity.to_owned();
}
