//! Materializes a resolved model launch into the llama.cpp workload spec the
//! runtime runs.
//!
//! The retained plan names the executable, artifacts, and HTTP service
//! symbolically; this module binds them to the selected engine artifact, for
//! example `/app/llama-server` and container paths under `/opt/dotcore/models`
//! for an OCI image, or `bin/llama-server` inside the retained store path for a
//! Nix package, and leaves the tokens for the runtime to resolve. A split GGUF model
//! mounts as its whole published directory, and `--model` names the first shard
//! inside it, for example `/opt/dotcore/models/model/Q4/model-00001-of-00003.gguf`.
//! It is used both
//! by the inference controller and by the development workload path. Engine
//! artifacts outside the supported platform and accelerator pairs and plans with
//! unknown bindings or services are rejected.

use std::collections::BTreeMap;

use dotcore_artifact::{ModelArtifactSource, PreparedModelArtifacts};
use dotcore_domain::{ExecutionPlatform, WorkloadArtifactKind};
use dotcore_proto::dotcore::model::v1::{
    PortableModelLaunchPlan, portable_model_launch_argument_token::Value as ArgumentValue,
};
use dotcore_runtime::{
    ExecutionTarget, WorkloadArtifactBinding, WorkloadExecutable, WorkloadExecutableArtifact,
    WorkloadIpcMode, WorkloadPort, WorkloadSpec, WorkloadToken, WorkloadValue,
};

use super::{
    declaration::split_gguf_entry,
    plan::{
        MODEL_BINDING_ID, PLAN_FORMAT_VERSION, PROJECTOR_BINDING_ID, SERVICE_NAME,
        SPECULATIVE_DRAFT_BINDING_ID,
    },
};
use crate::model::resolved::{ResolvedEngineSource, ResolvedModelLaunch};

/// Executable artifact kinds a llama.cpp engine artifact can provide: a Linux CUDA image or a
/// Nix package for native runtimes.
pub(super) const ARTIFACT_KINDS: &[WorkloadArtifactKind] =
    &[WorkloadArtifactKind::Oci, WorkloadArtifactKind::Nix];

/// Operating system, architecture, and accelerator triples llama.cpp engine artifacts may declare.
const SUPPORTED_PLATFORMS: [(&str, &str, &str); 3] = [
    ("linux", "amd64", "nvidia"),
    ("linux", "arm64", "nvidia"),
    ("darwin", "arm64", "apple"),
];
const OCI_EXECUTABLE_PATH: &str = "/app/llama-server";

/// Port llama.cpp listens on inside a container; native runtimes substitute an assigned loopback
/// port through the service port token instead.
const SERVICE_PORT: u16 = 8080;

/// Builds the workload spec by binding the stored symbolic plan to the selected engine artifact.
pub(crate) fn workload(
    model: &ResolvedModelLaunch,
    target: &ExecutionTarget,
    artifacts: &PreparedModelArtifacts,
) -> Result<WorkloadSpec, LlamaCppWorkloadError> {
    validate_runtime_constraints(model, target)?;
    let model_entry = split_model_entry(model)?;
    let materialized = materialize_plan(&model.plan, model_entry.as_deref(), artifacts)?;
    let required_platform = ExecutionPlatform::normalize(
        &model.engine.platform.operating_system,
        &model.engine.platform.architecture,
    )
    .map_err(|_| LlamaCppWorkloadError::UnsupportedEngine)?;
    Ok(WorkloadSpec {
        executable: executable(model)?,
        required_platform,
        arguments: materialized.arguments,
        environment: BTreeMap::new(),
        artifacts: materialized.artifacts,
        storage: Vec::new(),
        ports: vec![WorkloadPort {
            name: SERVICE_NAME.to_owned(),
            container_port: SERVICE_PORT,
        }],
        accelerator_device_ids: target.accelerator_device_ids.clone(),
        required_accelerator_device_count: model.hardware.accelerator_device_count(),
        ipc_mode: WorkloadIpcMode::Isolated,
        expected_memory: Default::default(),
    })
}

/// Binds the engine executable to the launch identity: the image's server for an OCI image or the
/// image a Docker-delivered Nix artifact realizes, or the package's declared program under `bin/`
/// of the retained store path for a native Nix package.
fn executable(model: &ResolvedModelLaunch) -> Result<WorkloadExecutable, LlamaCppWorkloadError> {
    Ok(match &model.engine.source {
        ResolvedEngineSource::OciImage { .. } | ResolvedEngineSource::NixDockerImage { .. } => {
            WorkloadExecutable {
                artifact: WorkloadExecutableArtifact::OciImage(
                    model
                        .engine
                        .container_image()
                        .ok_or(LlamaCppWorkloadError::UnsupportedEngine)?,
                ),
                path: OCI_EXECUTABLE_PATH.to_owned(),
            }
        }
        ResolvedEngineSource::Nix { executable, .. } => WorkloadExecutable {
            artifact: WorkloadExecutableArtifact::NixStorePath(
                model.engine.launch_identity.clone(),
            ),
            path: format!("bin/{executable}"),
        },
    })
}

fn validate_runtime_constraints(
    model: &ResolvedModelLaunch,
    target: &ExecutionTarget,
) -> Result<(), LlamaCppWorkloadError> {
    if model.engine.kind
        != dotcore_proto::dotcore::engine::v1::EngineKind::LlamaCpp as i32
        || model
            .snapshot
            .resolved_engine_state
            .as_ref()
            .is_none_or(|state| {
                state.engine_kind
                    != dotcore_proto::dotcore::engine::v1::EngineKind::LlamaCpp as i32
                    || state.semantics_version != super::realization::SEMANTICS_VERSION
                    || !matches!(
                        state.state.as_ref(),
                        Some(
                            dotcore_proto::dotcore::engine::v1::resolved_engine_model_state::State::LlamaCpp(_)
                        )
                    )
            })
    {
        return Err(LlamaCppWorkloadError::UnsupportedEngine);
    }
    let platform = &model.engine.platform;
    if !SUPPORTED_PLATFORMS.contains(&(
        platform.operating_system.as_str(),
        platform.architecture.as_str(),
        platform.accelerator.as_str(),
    )) {
        return Err(LlamaCppWorkloadError::UnsupportedEngine);
    }
    let [requirement] = model.hardware.accelerator_requirements.as_slice() else {
        return Err(LlamaCppWorkloadError::InvalidAcceleratorRequirement);
    };
    if requirement.vendor != platform.accelerator
        || requirement.minimum_count != 1
        || target.accelerator_device_ids.len() > requirement.minimum_count as usize
        || target
            .accelerator_device_ids
            .iter()
            .any(|device| !valid_device_id(device))
    {
        return Err(LlamaCppWorkloadError::UnsafeAcceleratorDevice);
    }
    Ok(())
}

/// Returns the first shard's path inside the model publication when the model artifact is a split
/// GGUF set, or `None` for a single GGUF file.
fn split_model_entry(model: &ResolvedModelLaunch) -> Result<Option<String>, LlamaCppWorkloadError> {
    match model.artifacts.get(MODEL_BINDING_ID) {
        Some(ModelArtifactSource::HuggingFaceSnapshot { files, .. }) => {
            split_gguf_entry(files.keys().map(String::as_str))
                .map(Some)
                .map_err(|_| LlamaCppWorkloadError::InvalidSplitModel)
        }
        _ => Ok(None),
    }
}

/// Binds the plan to prepared artifacts. `model_entry` is the first shard of a split model, which
/// mounts as a directory and is named inside it.
fn materialize_plan(
    plan: &PortableModelLaunchPlan,
    model_entry: Option<&str>,
    artifacts: &PreparedModelArtifacts,
) -> Result<MaterializedPlan, LlamaCppWorkloadError> {
    if plan.format_version != PLAN_FORMAT_VERSION {
        return Err(LlamaCppWorkloadError::UnsupportedPlan);
    }
    let bindings = prepared_bindings(plan, model_entry.is_some(), artifacts)?;
    let Some((executable, arguments)) = plan.arguments.split_first() else {
        return Err(LlamaCppWorkloadError::InvalidPlan);
    };
    if !matches!(executable.value, Some(ArgumentValue::EngineExecutable(_))) {
        return Err(LlamaCppWorkloadError::InvalidPlan);
    }
    let arguments = arguments
        .iter()
        .map(|argument| match argument.value.as_ref() {
            Some(ArgumentValue::Literal(value)) => Ok(WorkloadValue::literal(value)),
            Some(ArgumentValue::ArtifactBindingId(id)) if bindings.contains_key(id) => {
                artifact_value(id, model_entry)
            }
            Some(ArgumentValue::ServiceHost(name)) if name == SERVICE_NAME => Ok(
                WorkloadValue::token(WorkloadToken::ServiceHost(name.clone())),
            ),
            Some(ArgumentValue::ServicePort(name)) if name == SERVICE_NAME => Ok(
                WorkloadValue::token(WorkloadToken::ServicePort(name.clone())),
            ),
            _ => Err(LlamaCppWorkloadError::InvalidPlan),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(MaterializedPlan {
        arguments,
        artifacts: bindings.into_values().collect(),
    })
}

/// Refers to one plan binding; a split model's value joins its directory with the first shard.
fn artifact_value(
    binding_id: &str,
    model_entry: Option<&str>,
) -> Result<WorkloadValue, LlamaCppWorkloadError> {
    let token = WorkloadValue::token(WorkloadToken::Artifact(binding_id.to_owned()));
    match model_entry {
        Some(entry) if binding_id == MODEL_BINDING_ID => {
            // The canonical text form escapes the literal path, so parsing it back is exact.
            let entry = WorkloadValue::literal(format!("/{entry}"));
            WorkloadValue::parse(&format!("{token}{entry}"))
                .map_err(|_| LlamaCppWorkloadError::InvalidSplitModel)
        }
        _ => Ok(token),
    }
}

fn prepared_bindings(
    plan: &PortableModelLaunchPlan,
    split_model: bool,
    artifacts: &PreparedModelArtifacts,
) -> Result<BTreeMap<String, WorkloadArtifactBinding>, LlamaCppWorkloadError> {
    let mut bindings = BTreeMap::new();
    for binding in &plan.artifact_bindings {
        let destination = oci_destination(&binding.binding_id, split_model)
            .ok_or(LlamaCppWorkloadError::InvalidPlan)?;
        if !binding.read_only || binding.artifact.is_none() {
            return Err(LlamaCppWorkloadError::InvalidPlan);
        }
        let source = artifacts
            .artifacts
            .get(&binding.binding_id)
            .ok_or(LlamaCppWorkloadError::MissingPreparedArtifact)?;
        let prepared = WorkloadArtifactBinding {
            name: binding.binding_id.clone(),
            source: source.clone(),
            destination: destination.to_owned(),
            read_only: true,
        };
        if bindings
            .insert(binding.binding_id.clone(), prepared)
            .is_some()
        {
            return Err(LlamaCppWorkloadError::InvalidPlan);
        }
    }
    if artifacts
        .artifacts
        .keys()
        .any(|binding_id| !bindings.contains_key(binding_id))
    {
        return Err(LlamaCppWorkloadError::UnexpectedPreparedArtifact);
    }
    Ok(bindings)
}

/// Returns the container path at which an OCI llama.cpp image reads one plan binding: a file, or
/// the directory of a split model.
fn oci_destination(binding_id: &str, split_model: bool) -> Option<&'static str> {
    match binding_id {
        MODEL_BINDING_ID if split_model => Some("/opt/dotcore/models/model"),
        MODEL_BINDING_ID => Some("/opt/dotcore/models/model.gguf"),
        PROJECTOR_BINDING_ID => Some("/opt/dotcore/models/projector.gguf"),
        SPECULATIVE_DRAFT_BINDING_ID => Some("/opt/dotcore/models/speculative-draft.gguf"),
        _ => None,
    }
}

#[derive(Debug)]
struct MaterializedPlan {
    arguments: Vec<WorkloadValue>,
    artifacts: Vec<WorkloadArtifactBinding>,
}

/// Indicates the stored plan cannot safely materialize into a llama.cpp workload.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum LlamaCppWorkloadError {
    #[error(
        "llama.cpp model profile requires an engine artifact for linux/amd64 or linux/arm64 with \
         NVIDIA, or darwin/arm64 with Apple accelerators"
    )]
    UnsupportedEngine,

    #[error("llama.cpp model profile must declare exactly one accelerator requirement")]
    InvalidAcceleratorRequirement,

    #[error("llama.cpp model profile has an unsafe accelerator device identity")]
    UnsafeAcceleratorDevice,

    #[error("llama.cpp workload has an unsupported desired-plan format")]
    UnsupportedPlan,

    #[error("llama.cpp workload has corrupt desired-plan bindings or arguments")]
    InvalidPlan,

    #[error("llama.cpp workload has no prepared artifact for a plan binding")]
    MissingPreparedArtifact,

    #[error("llama.cpp workload has an unexpected prepared artifact binding")]
    UnexpectedPreparedArtifact,

    #[error("llama.cpp workload has a model artifact that is not a complete split GGUF set")]
    InvalidSplitModel,
}

fn valid_device_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.chars().all(|character| {
            !character.is_control() && !character.is_whitespace() && character != ','
        })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn materialization_keeps_plan_tokens_and_binds_oci_artifact_destinations() {
        let plan = translated_plan();
        let artifacts = prepared([("model", "/cache/model.gguf")]);

        let materialized = materialize_plan(&plan, None, &artifacts).expect("materialized plan");

        assert_eq!(
            materialized
                .arguments
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            [
                "--model",
                "{artifact:model}",
                "--alias",
                "fixture-model",
                "--host",
                "{service:http.host}",
                "--port",
                "{service:http.port}",
                "--ctx-size",
                "32768",
                "--n-gpu-layers",
                "0",
                "--cache-ram",
                "0",
                "-np",
                "4",
                "--kv-unified",
            ]
        );
        assert_eq!(
            materialized.artifacts,
            [WorkloadArtifactBinding {
                name: "model".to_owned(),
                source: PathBuf::from("/cache/model.gguf"),
                destination: "/opt/dotcore/models/model.gguf".to_owned(),
                read_only: true,
            }]
        );
    }

    #[test]
    fn materialization_preserves_exact_custom_tokens_as_exec_form_argv() {
        let mut snapshot = crate::test_support::local_model_snapshot("fixture-model");
        let local = match snapshot.source.as_mut().expect("snapshot source") {
            dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(local) => {
                local
            }
            _ => panic!("local snapshot"),
        };
        local.custom_argument_tokens = vec![
            "--threads=4".to_owned(),
            "--threads".to_owned(),
            "2".to_owned(),
            String::new(),
            "  shell $(echo inert)  ".to_owned(),
        ];
        let plan = super::super::plan::translate(local).expect("translated plan");

        let materialized =
            materialize_plan(&plan, None, &prepared([("model", "/cache/model.gguf")]))
                .expect("materialized plan");

        assert_eq!(
            materialized.arguments[17..]
                .iter()
                .map(|argument| argument.as_literal().expect("literal custom argument"))
                .collect::<Vec<_>>(),
            [
                "--threads=4",
                "--threads",
                "2",
                "",
                "  shell $(echo inert)  ",
            ]
        );
    }

    #[test]
    fn materialization_fails_closed_on_missing_duplicate_unexpected_and_unsupported_bindings() {
        let plan = translated_plan();
        assert_eq!(
            materialize_plan(&plan, None, &prepared([])).expect_err("missing prepared binding"),
            LlamaCppWorkloadError::MissingPreparedArtifact
        );
        assert_eq!(
            materialize_plan(
                &plan,
                None,
                &prepared([
                    ("model", "/cache/model.gguf"),
                    ("extra", "/cache/extra.gguf")
                ]),
            )
            .expect_err("unexpected prepared binding"),
            LlamaCppWorkloadError::UnexpectedPreparedArtifact
        );

        let mut duplicate = plan.clone();
        duplicate
            .artifact_bindings
            .push(duplicate.artifact_bindings[0].clone());
        assert_eq!(
            materialize_plan(
                &duplicate,
                None,
                &prepared([
                    ("model", "/cache/model.gguf"),
                    ("other", "/cache/other.gguf")
                ]),
            )
            .expect_err("duplicate plan binding"),
            LlamaCppWorkloadError::InvalidPlan
        );

        let mut missing_executable = plan.clone();
        missing_executable.arguments.remove(0);
        let mut unknown_service = plan.clone();
        unknown_service.arguments[6].value = Some(ArgumentValue::ServiceHost("metrics".to_owned()));
        let mut unknown_binding = plan.clone();
        unknown_binding.artifact_bindings[0].binding_id = "weights".to_owned();
        for (case, corrupt) in [
            ("missing executable", missing_executable),
            ("unknown service", unknown_service),
            ("unknown binding", unknown_binding),
        ] {
            assert_eq!(
                materialize_plan(&corrupt, None, &prepared([("model", "/cache/model.gguf")]))
                    .expect_err(case),
                LlamaCppWorkloadError::InvalidPlan,
                "{case}"
            );
        }

        let mut unsupported = plan;
        unsupported.format_version = 1;
        assert_eq!(
            materialize_plan(
                &unsupported,
                None,
                &prepared([("model", "/cache/model.gguf")])
            )
            .expect_err("unsupported plan"),
            LlamaCppWorkloadError::UnsupportedPlan
        );
    }

    #[test]
    fn workload_rejects_engine_artifacts_outside_supported_platform_pairs() {
        for (operating_system, architecture, accelerator) in [
            ("darwin", "arm64", "nvidia"),
            ("darwin", "amd64", "apple"),
            ("linux", "amd64", "apple"),
            ("linux", "arm64", "apple"),
            ("linux", "riscv64", "nvidia"),
        ] {
            let mut model = resolved(crate::test_support::local_model_snapshot("fixture-model"));
            model.engine.platform.operating_system = operating_system.to_owned();
            model.engine.platform.architecture = architecture.to_owned();
            model.engine.platform.accelerator = accelerator.to_owned();

            assert_eq!(
                workload(
                    &model,
                    &target(),
                    &prepared([("model", "/cache/model.gguf")])
                ),
                Err(LlamaCppWorkloadError::UnsupportedEngine),
                "{operating_system}/{architecture}/{accelerator}"
            );
        }
        assert_eq!(
            LlamaCppWorkloadError::UnsupportedEngine.to_string(),
            "llama.cpp model profile requires an engine artifact for linux/amd64 or linux/arm64 \
             with NVIDIA, or darwin/arm64 with Apple accelerators"
        );
    }

    #[test]
    fn oci_engine_launches_the_image_server_on_linux_nvidia() {
        let model = resolved(crate::test_support::local_model_snapshot("fixture-model"));

        let spec = workload(
            &model,
            &target(),
            &prepared([("model", "/cache/model.gguf")]),
        )
        .expect("OCI workload");

        assert_eq!(
            spec.executable,
            WorkloadExecutable {
                artifact: WorkloadExecutableArtifact::OciImage(
                    model.engine.launch_identity.clone()
                ),
                path: "/app/llama-server".to_owned(),
            }
        );
        assert_eq!(spec.required_platform, ExecutionPlatform::linux_amd64());
        assert_eq!(spec.accelerator_device_ids, ["0"]);
    }

    #[test]
    fn docker_delivered_engine_launches_the_image_its_loader_store_path_names() {
        let (_, store_path) = crate::test_support::nix::DOCKER_IMAGE_STORE_PATHS[0];
        for flake in [
            dotcore_domain::test_support::nix::locked_flake(),
            "self".to_owned(),
        ] {
            let mut snapshot = crate::test_support::local_model_snapshot("fixture-model");
            crate::test_support::nix::retain_docker_delivered_engine(
                &mut snapshot,
                &flake,
                store_path,
            );
            let model = resolved(snapshot);

            let spec = workload(
                &model,
                &target(),
                &prepared([("model", "/cache/model.gguf")]),
            )
            .expect("Docker-delivered workload");

            assert_eq!(
                spec.executable,
                WorkloadExecutable {
                    artifact: WorkloadExecutableArtifact::OciImage(
                        "dotcore-nix/docker-image-llama-cpp-cuda:0123456789abcdfghijklmnpqrsvwxyz"
                            .to_owned()
                    ),
                    path: "/app/llama-server".to_owned(),
                },
                "{flake}"
            );
            assert_eq!(model.engine.launch_identity, store_path);
            assert_eq!(
                model.engine.source.kind(),
                dotcore_domain::WorkloadArtifactKind::Oci
            );
        }
    }

    #[test]
    fn nix_engine_launches_the_declared_program_of_its_store_path_on_apple() {
        let model = resolved(crate::test_support::nix::apple_metal_snapshot(
            "fixture-model",
        ));
        let apple_gpu = ExecutionTarget {
            managed_node_id: dotcore_domain::SiteNodeId::for_test("node"),
            accelerator_device_ids: vec!["apple-gpu-0".to_owned()],
        };

        let spec = workload(
            &model,
            &apple_gpu,
            &prepared([("model", "/artifacts/model.gguf")]),
        )
        .expect("Nix workload");

        assert_eq!(
            spec.executable,
            WorkloadExecutable {
                artifact: WorkloadExecutableArtifact::NixStorePath(
                    dotcore_domain::test_support::nix::STORE_PATH.to_owned()
                ),
                path: "bin/llama-server".to_owned(),
            }
        );
        assert_eq!(spec.required_platform, ExecutionPlatform::darwin_arm64());
        assert_eq!(spec.accelerator_device_ids, ["apple-gpu-0"]);
        assert_eq!(spec.required_accelerator_device_count, 1);
        assert_eq!(
            spec.arguments
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            [
                "--model",
                "{artifact:model}",
                "--alias",
                "fixture-model",
                "--host",
                "{service:http.host}",
                "--port",
                "{service:http.port}",
                "--ctx-size",
                "32768",
                "--n-gpu-layers",
                "99",
                "--load-mode",
                "none",
                "--cache-ram",
                "2048",
                "-np",
                "4",
                "--kv-unified",
            ]
        );
    }

    #[test]
    fn apple_engine_rejects_a_requirement_of_another_vendor() {
        let mut model = resolved(crate::test_support::nix::apple_metal_snapshot(
            "fixture-model",
        ));
        model.hardware.accelerator_requirements[0].vendor = "nvidia".to_owned();

        assert_eq!(
            workload(
                &model,
                &target(),
                &prepared([("model", "/artifacts/model.gguf")])
            ),
            Err(LlamaCppWorkloadError::UnsafeAcceleratorDevice)
        );
    }

    #[test]
    fn docker_split_model_mounts_its_directory_and_names_the_first_shard() {
        let mut model = resolved(crate::test_support::local_model_snapshot("fixture-model"));
        model
            .artifacts
            .insert(MODEL_BINDING_ID.to_owned(), split_model_artifact());

        let spec = workload(
            &model,
            &target(),
            &prepared([("model", "/cache/.dotcore-snapshots/split")]),
        )
        .expect("split workload");

        assert_eq!(
            spec.resolve(&dotcore_runtime_backends::docker::ContainerTokens)
                .expect("container command")
                .arguments,
            [
                "--model",
                "/opt/dotcore/models/model/UD-Q2_K_XL/model-UD-Q2_K_XL-00001-of-00002.gguf",
                "--alias",
                "fixture-model",
                "--host",
                "0.0.0.0",
                "--port",
                "8080",
                "--ctx-size",
                "32768",
                "--n-gpu-layers",
                "0",
                "--cache-ram",
                "0",
                "-np",
                "4",
                "--kv-unified",
            ]
        );
        assert_eq!(
            spec.artifacts,
            [WorkloadArtifactBinding {
                name: "model".to_owned(),
                source: PathBuf::from("/cache/.dotcore-snapshots/split"),
                destination: "/opt/dotcore/models/model".to_owned(),
                read_only: true,
            }]
        );
    }

    #[test]
    fn native_split_model_names_the_first_shard_under_its_host_publication() {
        let mut model = resolved(crate::test_support::nix::apple_metal_snapshot(
            "fixture-model",
        ));
        model
            .artifacts
            .insert(MODEL_BINDING_ID.to_owned(), split_model_artifact());
        let apple_gpu = ExecutionTarget {
            managed_node_id: dotcore_domain::SiteNodeId::for_test("node"),
            accelerator_device_ids: vec!["apple-gpu-0".to_owned()],
        };

        let spec = workload(
            &model,
            &apple_gpu,
            &prepared([("model", "/artifacts/.dotcore-snapshots/split")]),
        )
        .expect("split workload");

        assert_eq!(
            spec.arguments[..2]
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            [
                "--model",
                "{artifact:model}/UD-Q2_K_XL/model-UD-Q2_K_XL-00001-of-00002.gguf",
            ]
        );
        assert_eq!(
            spec.artifacts
                .iter()
                .map(|binding| binding.source.clone())
                .collect::<Vec<_>>(),
            [PathBuf::from("/artifacts/.dotcore-snapshots/split")]
        );
    }

    #[test]
    fn single_file_model_keeps_its_file_mount() {
        let model = resolved(crate::test_support::local_model_snapshot("fixture-model"));

        let spec = workload(
            &model,
            &target(),
            &prepared([("model", "/cache/model.gguf")]),
        )
        .expect("single-file workload");

        assert_eq!(
            spec.resolve(&dotcore_runtime_backends::docker::ContainerTokens)
                .expect("container command")
                .arguments[..2],
            ["--model", "/opt/dotcore/models/model.gguf"]
        );
        assert_eq!(
            spec.artifacts[0].destination,
            "/opt/dotcore/models/model.gguf"
        );
    }

    #[test]
    fn split_model_rejects_an_incomplete_retained_set() {
        let mut model = resolved(crate::test_support::local_model_snapshot("fixture-model"));
        let mut artifact = split_model_artifact();
        let ModelArtifactSource::HuggingFaceSnapshot { files, .. } = &mut artifact else {
            panic!("snapshot artifact");
        };
        files.pop_first();
        model
            .artifacts
            .insert(MODEL_BINDING_ID.to_owned(), artifact);

        assert_eq!(
            workload(
                &model,
                &target(),
                &prepared([("model", "/cache/.dotcore-snapshots/split")])
            ),
            Err(LlamaCppWorkloadError::InvalidSplitModel)
        );
    }

    fn split_model_artifact() -> ModelArtifactSource {
        ModelArtifactSource::HuggingFaceSnapshot {
            repository: "owner/repository".to_owned(),
            revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            files: [
                "UD-Q2_K_XL/model-UD-Q2_K_XL-00002-of-00002.gguf",
                "UD-Q2_K_XL/model-UD-Q2_K_XL-00001-of-00002.gguf",
            ]
            .map(|path| (path.to_owned(), "2".repeat(64)))
            .into(),
        }
    }

    fn resolved(
        snapshot: dotcore_proto::dotcore::model::v1::ModelDeploymentSnapshot,
    ) -> ResolvedModelLaunch {
        ResolvedModelLaunch::from_snapshot(&snapshot).expect("resolved launch")
    }

    fn translated_plan() -> PortableModelLaunchPlan {
        let snapshot = crate::test_support::local_model_snapshot("fixture-model");
        let local = match snapshot.source.expect("snapshot source") {
            dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(local) => {
                local
            }
            _ => panic!("local snapshot"),
        };
        super::super::plan::translate(&local).expect("translated plan")
    }

    fn target() -> ExecutionTarget {
        ExecutionTarget {
            managed_node_id: dotcore_domain::SiteNodeId::for_test("node"),
            accelerator_device_ids: vec!["0".to_owned()],
        }
    }

    fn prepared<const N: usize>(values: [(&str, &str); N]) -> PreparedModelArtifacts {
        PreparedModelArtifacts {
            artifacts: values
                .into_iter()
                .map(|(binding_id, path)| (binding_id.to_owned(), PathBuf::from(path)))
                .collect(),
        }
    }
}
