//! Materializes a resolved Strata model launch into the OCI workload the runtime runs.
//!
//! The workload runs the retained engine image identity with the command materialized from the
//! portable launch plan, the model's files and any draft layer mounted read-only, one NVIDIA
//! accelerator, isolated IPC, and the HTTP service port 8080. The deployment's data folder comes from the engine release's storage,
//! which the engine registry adds. Strata runs only on Linux with NVIDIA, so other engine platforms
//! are rejected.

use std::collections::BTreeMap;

use dotcore_artifact::{ModelArtifactSource, PreparedModelArtifacts};
use dotcore_domain::{ExecutionPlatform, WorkloadArtifactKind};
use dotcore_proto::dotcore::{
    engine::v1::{EngineKind, resolved_engine_model_state::State},
    model::v1::{
        PortableModelLaunchPlan, portable_model_launch_argument_token::Value as ArgumentValue,
    },
};
use dotcore_runtime::{
    ExecutionTarget, WorkloadArtifactBinding, WorkloadExecutable, WorkloadExecutableArtifact,
    WorkloadIpcMode, WorkloadPort, WorkloadSpec, WorkloadValue,
};

use super::{
    SERVICE_PORT,
    plan::{DRAFT_LAYER_BINDING_ID, MODEL_BINDING_ID, PLAN_FORMAT_VERSION},
};
use crate::model::resolved::ResolvedModelLaunch;

/// Executable artifact kinds a Strata engine artifact can provide.
pub(super) const ARTIFACT_KINDS: &[WorkloadArtifactKind] = &[WorkloadArtifactKind::Oci];

/// Container path at which the Strata start wrapper reads a single-file model binding.
pub(super) const CONTAINER_MODEL_PATH: &str = "/opt/dotcore/models/model.gguf";
/// Container path at which the Strata start wrapper reads a model binding holding several files.
pub(super) const CONTAINER_MODEL_FOLDER: &str = "/opt/dotcore/models/model";
/// Container path at which the Strata start wrapper reads the draft layer binding.
pub(super) const CONTAINER_DRAFT_LAYER_FOLDER: &str = "/opt/dotcore/models/draft-layer";
const SERVICE_PORT_NAME: &str = "http";

/// Builds the literal container workload spec by materializing the stored portable plan.
pub(crate) fn workload(
    model: &ResolvedModelLaunch,
    target: &ExecutionTarget,
    artifacts: &PreparedModelArtifacts,
) -> Result<WorkloadSpec, StrataWorkloadError> {
    validate_runtime_constraints(model, target)?;
    let model_folder = matches!(
        model.artifacts.get(MODEL_BINDING_ID),
        Some(ModelArtifactSource::HuggingFaceSnapshot { .. })
    );
    let (command, mounts) = materialize_plan(&model.plan, model_folder, artifacts)?;
    let required_platform = ExecutionPlatform::normalize(
        &model.engine.platform.operating_system,
        &model.engine.platform.architecture,
    )
    .map_err(|_| StrataWorkloadError::UnsupportedEngine)?;
    let (executable, arguments) = command
        .split_first()
        .ok_or(StrataWorkloadError::InvalidPlan)?;
    Ok(WorkloadSpec {
        executable: WorkloadExecutable {
            artifact: WorkloadExecutableArtifact::OciImage(
                model
                    .engine
                    .container_image()
                    .ok_or(StrataWorkloadError::UnsupportedEngine)?,
            ),
            path: executable.clone(),
        },
        required_platform,
        arguments: arguments.iter().map(WorkloadValue::literal).collect(),
        environment: BTreeMap::new(),
        artifacts: mounts,
        storage: Vec::new(),
        ports: vec![WorkloadPort {
            name: SERVICE_PORT_NAME.to_owned(),
            container_port: SERVICE_PORT,
        }],
        accelerator_device_ids: target.accelerator_device_ids.clone(),
        required_accelerator_device_count: model.hardware.accelerator_device_count(),
        ipc_mode: WorkloadIpcMode::Isolated,
        expected_memory: Default::default(),
    })
}

fn validate_runtime_constraints(
    model: &ResolvedModelLaunch,
    target: &ExecutionTarget,
) -> Result<(), StrataWorkloadError> {
    let strata_state = model
        .snapshot
        .resolved_engine_state
        .as_ref()
        .is_some_and(|state| {
            model.engine.kind == EngineKind::Strata as i32
                && state.engine_kind == EngineKind::Strata as i32
                && state.semantics_version == super::realization::SEMANTICS_VERSION
                && matches!(state.state.as_ref(), Some(State::Strata(_)))
        });
    if !strata_state
        || model.engine.platform.operating_system != "linux"
        || model.engine.platform.accelerator != "nvidia"
    {
        return Err(StrataWorkloadError::UnsupportedEngine);
    }
    let [requirement] = model.hardware.accelerator_requirements.as_slice() else {
        return Err(StrataWorkloadError::InvalidAcceleratorRequirement);
    };
    if requirement.vendor != "nvidia"
        || requirement.minimum_count != 1
        || target.accelerator_device_ids.len() > requirement.minimum_count as usize
        || target
            .accelerator_device_ids
            .iter()
            .any(|device| !valid_device_id(device))
    {
        return Err(StrataWorkloadError::UnsafeAcceleratorDevice);
    }
    Ok(())
}

/// Resolves binding tokens to container paths and returns the command and read-only mounts. The
/// model binding is a folder when `model_folder` is set, and a single GGUF file otherwise.
fn materialize_plan(
    plan: &PortableModelLaunchPlan,
    model_folder: bool,
    artifacts: &PreparedModelArtifacts,
) -> Result<(Vec<String>, Vec<WorkloadArtifactBinding>), StrataWorkloadError> {
    if plan.format_version != PLAN_FORMAT_VERSION {
        return Err(StrataWorkloadError::UnsupportedPlan);
    }
    let mut mounts = BTreeMap::new();
    for binding in &plan.artifact_bindings {
        let destination = match binding.binding_id.as_str() {
            MODEL_BINDING_ID if model_folder => CONTAINER_MODEL_FOLDER,
            MODEL_BINDING_ID => CONTAINER_MODEL_PATH,
            DRAFT_LAYER_BINDING_ID => CONTAINER_DRAFT_LAYER_FOLDER,
            _ => return Err(StrataWorkloadError::InvalidPlan),
        };
        if !binding.read_only || binding.artifact.is_none() {
            return Err(StrataWorkloadError::InvalidPlan);
        }
        let source = artifacts
            .artifacts
            .get(&binding.binding_id)
            .ok_or(StrataWorkloadError::MissingPreparedArtifact)?;
        let mount = WorkloadArtifactBinding {
            name: binding.binding_id.clone(),
            source: source.clone(),
            destination: destination.to_owned(),
            read_only: true,
        };
        if mounts.insert(binding.binding_id.clone(), mount).is_some() {
            return Err(StrataWorkloadError::InvalidPlan);
        }
    }
    if !mounts.contains_key(MODEL_BINDING_ID) {
        return Err(StrataWorkloadError::InvalidPlan);
    }
    if artifacts
        .artifacts
        .keys()
        .any(|binding_id| !mounts.contains_key(binding_id))
    {
        return Err(StrataWorkloadError::UnexpectedPreparedArtifact);
    }
    let command = plan
        .arguments
        .iter()
        .map(|argument| match argument.value.as_ref() {
            Some(ArgumentValue::Literal(value)) => Ok(value.clone()),
            Some(ArgumentValue::ArtifactBindingId(id)) => mounts
                .get(id)
                .map(|mount| mount.destination.clone())
                .ok_or(StrataWorkloadError::InvalidPlan),
            _ => Err(StrataWorkloadError::InvalidPlan),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if command.first().is_none_or(String::is_empty) {
        return Err(StrataWorkloadError::InvalidPlan);
    }
    Ok((command, mounts.into_values().collect()))
}

fn valid_device_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.chars().all(|character| {
            !character.is_control() && !character.is_whitespace() && character != ','
        })
}

/// Indicates the stored plan cannot safely materialize into a Strata workload.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum StrataWorkloadError {
    #[error("Strata model profile requires a Linux NVIDIA engine artifact")]
    UnsupportedEngine,

    #[error("Strata model profile requires one NVIDIA accelerator")]
    InvalidAcceleratorRequirement,

    #[error("Strata model profile has an unsafe accelerator device identity")]
    UnsafeAcceleratorDevice,

    #[error("Strata workload has an unsupported desired-plan format")]
    UnsupportedPlan,

    #[error("Strata workload has corrupt desired-plan bindings or arguments")]
    InvalidPlan,

    #[error("Strata workload has no prepared artifact for a plan binding")]
    MissingPreparedArtifact,

    #[error("Strata workload has an unexpected prepared artifact binding")]
    UnexpectedPreparedArtifact,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use dotcore_domain::SiteNodeId;

    use super::*;

    const IMAGE_ID: &str =
        "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    #[test]
    fn workload_runs_the_wrapper_under_init_on_one_device_with_a_read_only_model_mount() {
        let mut model = resolved();
        model.engine.source = crate::model::resolved::ResolvedEngineSource::OciImage {
            reference: "dotcore/strata:0.2.13".to_owned(),
        };
        model.engine.launch_identity = IMAGE_ID.to_owned();

        let workload = workload(&model, &target(&["GPU-0"]), &prepared()).expect("workload");

        assert_eq!(
            workload,
            WorkloadSpec {
                executable: WorkloadExecutable {
                    artifact: WorkloadExecutableArtifact::OciImage(IMAGE_ID.to_owned()),
                    path: "/usr/bin/tini".to_owned(),
                },
                required_platform: ExecutionPlatform::linux_amd64(),
                arguments: [
                    "--",
                    "/usr/local/bin/dotcore-strata-start",
                    "--model",
                    "/opt/dotcore/models/model.gguf",
                    "--model-name",
                    "qwen3-6-35b-a3b",
                    "--arch",
                    "qwen35moe",
                    "--mtp",
                    "2",
                    "--context",
                    "32768",
                    "--parallel",
                    "1",
                    "--expert-vram-mib",
                    "8192",
                    "--port",
                    "8080",
                ]
                .map(WorkloadValue::literal)
                .to_vec(),
                environment: BTreeMap::new(),
                artifacts: vec![WorkloadArtifactBinding {
                    name: "model".to_owned(),
                    source: PathBuf::from("/cache/model-q4.gguf"),
                    destination: "/opt/dotcore/models/model.gguf".to_owned(),
                    read_only: true,
                }],
                storage: Vec::new(),
                ports: vec![WorkloadPort {
                    name: "http".to_owned(),
                    container_port: 8080,
                }],
                accelerator_device_ids: vec!["GPU-0".to_owned()],
                required_accelerator_device_count: 1,
                ipc_mode: WorkloadIpcMode::Isolated,
                expected_memory: Default::default(),
            }
        );
    }

    #[test]
    fn pack_path_mounts_the_model_folder_and_the_draft_layer() {
        let model = ResolvedModelLaunch::from_snapshot(
            &crate::test_support::strata::local_model_snapshot_with(
                "qwen3-8-flash-next",
                &crate::test_support::strata::split_gguf_configuration(),
            ),
        )
        .expect("resolved Strata launch");
        let prepared = PreparedModelArtifacts {
            artifacts: BTreeMap::from([
                ("model".to_owned(), PathBuf::from("/cache/iq2-xs")),
                ("draft-layer".to_owned(), PathBuf::from("/cache/mtp")),
            ]),
        };

        let spec = workload(&model, &target(&["GPU-0"]), &prepared).expect("workload");

        assert_eq!(
            spec.arguments[1..6],
            [
                "/usr/local/bin/dotcore-strata-start",
                "--model",
                "/opt/dotcore/models/model",
                "--draft-layer",
                "/opt/dotcore/models/draft-layer",
            ]
            .map(WorkloadValue::literal)
        );
        assert_eq!(
            spec.artifacts,
            [
                WorkloadArtifactBinding {
                    name: "draft-layer".to_owned(),
                    source: PathBuf::from("/cache/mtp"),
                    destination: "/opt/dotcore/models/draft-layer".to_owned(),
                    read_only: true,
                },
                WorkloadArtifactBinding {
                    name: "model".to_owned(),
                    source: PathBuf::from("/cache/iq2-xs"),
                    destination: "/opt/dotcore/models/model".to_owned(),
                    read_only: true,
                },
            ]
        );
        assert_eq!(
            workload(&model, &target(&["GPU-0"]), &prepared_model_only()).expect_err("no draft"),
            StrataWorkloadError::MissingPreparedArtifact
        );
    }

    #[test]
    fn workload_rejects_other_engines_and_multiple_devices() {
        let mut apple = resolved();
        apple.engine.platform.accelerator = "apple".to_owned();
        assert_eq!(
            workload(&apple, &target(&["GPU-0"]), &prepared()).expect_err("apple engine"),
            StrataWorkloadError::UnsupportedEngine
        );

        assert_eq!(
            workload(&resolved(), &target(&["GPU-0", "GPU-1"]), &prepared())
                .expect_err("two devices"),
            StrataWorkloadError::UnsafeAcceleratorDevice
        );
    }

    #[test]
    fn workload_fails_closed_on_missing_or_unexpected_prepared_artifacts() {
        assert_eq!(
            workload(
                &resolved(),
                &target(&["GPU-0"]),
                &PreparedModelArtifacts {
                    artifacts: BTreeMap::new(),
                },
            )
            .expect_err("missing artifact"),
            StrataWorkloadError::MissingPreparedArtifact
        );

        let mut extra = prepared();
        extra
            .artifacts
            .insert("extra".to_owned(), PathBuf::from("/cache/extra"));
        assert_eq!(
            workload(&resolved(), &target(&["GPU-0"]), &extra).expect_err("extra artifact"),
            StrataWorkloadError::UnexpectedPreparedArtifact
        );
    }

    fn resolved() -> ResolvedModelLaunch {
        ResolvedModelLaunch::from_snapshot(&crate::test_support::strata::local_model_snapshot(
            "qwen3-6-35b-a3b",
        ))
        .expect("resolved Strata launch")
    }

    fn target(devices: &[&str]) -> ExecutionTarget {
        ExecutionTarget {
            managed_node_id: SiteNodeId::for_test("node"),
            accelerator_device_ids: devices.iter().map(|device| (*device).to_owned()).collect(),
        }
    }

    fn prepared_model_only() -> PreparedModelArtifacts {
        PreparedModelArtifacts {
            artifacts: BTreeMap::from([("model".to_owned(), PathBuf::from("/cache/iq2-xs"))]),
        }
    }

    fn prepared() -> PreparedModelArtifacts {
        PreparedModelArtifacts {
            artifacts: BTreeMap::from([(
                "model".to_owned(),
                PathBuf::from("/cache/model-q4.gguf"),
            )]),
        }
    }
}
