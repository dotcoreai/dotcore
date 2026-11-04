//! Materializes a resolved ninfer model launch into the OCI workload the runtime runs.
//!
//! The workload runs the retained engine image identity with the command materialized from the
//! portable launch plan, the `.ninfer` weights mounted read-only, one NVIDIA accelerator, isolated
//! IPC, and the HTTP service port 8080. It is used both by the inference controller and by the
//! development workload path. ninfer builds only for linux/amd64, so other engine
//! platforms are rejected.

use std::collections::BTreeMap;

use dotcore_artifact::PreparedModelArtifacts;
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

use super::plan::{MODEL_BINDING_ID, PLAN_FORMAT_VERSION};
use crate::model::resolved::ResolvedModelLaunch;

/// Executable artifact kinds a ninfer engine artifact can provide.
pub(super) const ARTIFACT_KINDS: &[WorkloadArtifactKind] = &[WorkloadArtifactKind::Oci];

/// Container path at which a ninfer image reads the model binding.
pub(super) const CONTAINER_MODEL_PATH: &str = "/opt/dotcore/models/model.ninfer";
const SERVICE_PORT_NAME: &str = "http";
const SERVICE_PORT: u16 = 8080;

/// Builds the literal container workload spec by materializing the stored portable plan.
pub(crate) fn workload(
    model: &ResolvedModelLaunch,
    target: &ExecutionTarget,
    artifacts: &PreparedModelArtifacts,
) -> Result<WorkloadSpec, NinferWorkloadError> {
    validate_runtime_constraints(model, target)?;
    let (command, mounts) = materialize_plan(&model.plan, artifacts)?;
    let required_platform = ExecutionPlatform::normalize(
        &model.engine.platform.operating_system,
        &model.engine.platform.architecture,
    )
    .map_err(|_| NinferWorkloadError::UnsupportedEngine)?;
    let (executable, arguments) = command
        .split_first()
        .ok_or(NinferWorkloadError::InvalidPlan)?;
    Ok(WorkloadSpec {
        executable: WorkloadExecutable {
            artifact: WorkloadExecutableArtifact::OciImage(
                model
                    .engine
                    .container_image()
                    .ok_or(NinferWorkloadError::UnsupportedEngine)?,
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
) -> Result<(), NinferWorkloadError> {
    let ninfer_state = model
        .snapshot
        .resolved_engine_state
        .as_ref()
        .is_some_and(|state| {
            model.engine.kind == EngineKind::Ninfer as i32
                && state.engine_kind == EngineKind::Ninfer as i32
                && state.semantics_version == super::realization::SEMANTICS_VERSION
                && matches!(state.state.as_ref(), Some(State::Ninfer(_)))
        });
    if !ninfer_state
        || model.engine.platform.operating_system != "linux"
        || model.engine.platform.architecture != "amd64"
        || model.engine.platform.accelerator != "nvidia"
    {
        return Err(NinferWorkloadError::UnsupportedEngine);
    }
    let [requirement] = model.hardware.accelerator_requirements.as_slice() else {
        return Err(NinferWorkloadError::InvalidAcceleratorRequirement);
    };
    if requirement.vendor != "nvidia"
        || requirement.minimum_count != 1
        || target.accelerator_device_ids.len() > requirement.minimum_count as usize
        || target
            .accelerator_device_ids
            .iter()
            .any(|device| !valid_device_id(device))
    {
        return Err(NinferWorkloadError::UnsafeAcceleratorDevice);
    }
    Ok(())
}

/// Resolves binding tokens to container paths and returns the command and read-only mounts.
fn materialize_plan(
    plan: &PortableModelLaunchPlan,
    artifacts: &PreparedModelArtifacts,
) -> Result<(Vec<String>, Vec<WorkloadArtifactBinding>), NinferWorkloadError> {
    if plan.format_version != PLAN_FORMAT_VERSION {
        return Err(NinferWorkloadError::UnsupportedPlan);
    }
    let mut mounts = BTreeMap::new();
    for binding in &plan.artifact_bindings {
        if binding.binding_id != MODEL_BINDING_ID
            || !binding.read_only
            || binding.artifact.is_none()
        {
            return Err(NinferWorkloadError::InvalidPlan);
        }
        let source = artifacts
            .artifacts
            .get(&binding.binding_id)
            .ok_or(NinferWorkloadError::MissingPreparedArtifact)?;
        let mount = WorkloadArtifactBinding {
            name: binding.binding_id.clone(),
            source: source.clone(),
            destination: CONTAINER_MODEL_PATH.to_owned(),
            read_only: true,
        };
        if mounts.insert(binding.binding_id.clone(), mount).is_some() {
            return Err(NinferWorkloadError::InvalidPlan);
        }
    }
    if artifacts
        .artifacts
        .keys()
        .any(|binding_id| !mounts.contains_key(binding_id))
    {
        return Err(NinferWorkloadError::UnexpectedPreparedArtifact);
    }
    let command = plan
        .arguments
        .iter()
        .map(|argument| match argument.value.as_ref() {
            Some(ArgumentValue::Literal(value)) => Ok(value.clone()),
            Some(ArgumentValue::ArtifactBindingId(id)) => mounts
                .get(id)
                .map(|mount| mount.destination.clone())
                .ok_or(NinferWorkloadError::InvalidPlan),
            _ => Err(NinferWorkloadError::InvalidPlan),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if command.first().is_none_or(String::is_empty) {
        return Err(NinferWorkloadError::InvalidPlan);
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

/// Indicates the stored plan cannot safely materialize into a ninfer workload.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum NinferWorkloadError {
    #[error("ninfer model profile requires a linux/amd64 NVIDIA engine artifact")]
    UnsupportedEngine,

    #[error("ninfer model profile requires one NVIDIA accelerator")]
    InvalidAcceleratorRequirement,

    #[error("ninfer model profile has an unsafe accelerator device identity")]
    UnsafeAcceleratorDevice,

    #[error("ninfer workload has an unsupported desired-plan format")]
    UnsupportedPlan,

    #[error("ninfer workload has corrupt desired-plan bindings or arguments")]
    InvalidPlan,

    #[error("ninfer workload has no prepared artifact for a plan binding")]
    MissingPreparedArtifact,

    #[error("ninfer workload has an unexpected prepared artifact binding")]
    UnexpectedPreparedArtifact,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use dotcore_domain::{ExecutionPlatform, SiteNodeId};

    use super::*;

    const IMAGE_ID: &str =
        "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    #[test]
    fn workload_runs_the_retained_image_on_one_device_with_a_read_only_model_mount() {
        let mut model = resolved();
        model.engine.source = crate::model::resolved::ResolvedEngineSource::OciImage {
            reference: "dotcore/ninfer:fixture".to_owned(),
        };
        model.engine.launch_identity = IMAGE_ID.to_owned();

        let workload = workload(&model, &target(&["GPU-0"]), &prepared()).expect("workload");

        assert_eq!(
            workload,
            WorkloadSpec {
                executable: WorkloadExecutable {
                    artifact: WorkloadExecutableArtifact::OciImage(IMAGE_ID.to_owned()),
                    path: "/usr/local/bin/ninfer-serve".to_owned(),
                },
                required_platform: ExecutionPlatform::linux_amd64(),
                arguments: [
                    "/opt/dotcore/models/model.ninfer",
                    "--model-id",
                    "qwen3-8-27b",
                    "--host",
                    "0.0.0.0",
                    "--port",
                    "8080",
                    "--device",
                    "0",
                    "--vision",
                    "--max-context",
                    "32768",
                    "--context-capacity",
                    "2771177472",
                    "--spec",
                    "mtp",
                    "--draft-tokens",
                    "3",
                    "--max-concurrency",
                    "4",
                    "--pending-timeout",
                    "120000ms",
                    "--kv-dtype",
                    "fp8",
                    "--host-cache-capacity",
                    "20480MiB",
                    "--memory-headroom",
                    "0B",
                    "--proposal-head",
                    "optimized",
                    "--preserve-thinking",
                ]
                .map(WorkloadValue::literal)
                .to_vec(),
                environment: BTreeMap::new(),
                artifacts: vec![WorkloadArtifactBinding {
                    name: "model".to_owned(),
                    source: PathBuf::from("/cache/fixture_nvfp4.ninfer"),
                    destination: "/opt/dotcore/models/model.ninfer".to_owned(),
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
    fn workload_rejects_non_amd64_engines_and_multiple_devices() {
        let mut arm64 = resolved();
        arm64.engine.platform.architecture = "arm64".to_owned();
        assert_eq!(
            workload(&arm64, &target(&["GPU-0"]), &prepared()).expect_err("arm64 engine"),
            NinferWorkloadError::UnsupportedEngine
        );

        assert_eq!(
            workload(&resolved(), &target(&["GPU-0", "GPU-1"]), &prepared())
                .expect_err("two devices"),
            NinferWorkloadError::UnsafeAcceleratorDevice
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
            NinferWorkloadError::MissingPreparedArtifact
        );

        let mut extra = prepared();
        extra
            .artifacts
            .insert("extra".to_owned(), PathBuf::from("/cache/extra"));
        assert_eq!(
            workload(&resolved(), &target(&["GPU-0"]), &extra).expect_err("extra artifact"),
            NinferWorkloadError::UnexpectedPreparedArtifact
        );
    }

    #[test]
    fn retained_plans_with_legacy_container_destinations_keep_their_workload_and_identity() {
        use dotcore_proto::dotcore::model::v1::{
            ModelDeploymentSnapshot, PortableModelLaunchArgumentToken,
            model_deployment_snapshot::Source,
        };
        use prost::Message;

        /// Wire shape of a plan binding retained before bindings dropped their container path.
        #[derive(Clone, PartialEq, Message)]
        struct LegacyBinding {
            #[prost(string, tag = "1")]
            binding_id: String,
            #[prost(string, tag = "2")]
            model_artifact_id: String,
            #[prost(string, tag = "4")]
            destination: String,
            #[prost(bool, tag = "5")]
            read_only: bool,
        }

        #[derive(Clone, PartialEq, Message)]
        struct LegacyPlan {
            #[prost(message, repeated, tag = "1")]
            arguments: Vec<PortableModelLaunchArgumentToken>,
            #[prost(uint32, tag = "2")]
            format_version: u32,
            #[prost(message, repeated, tag = "3")]
            artifact_bindings: Vec<LegacyBinding>,
        }

        let canonical = resolved();
        let legacy_bytes = LegacyPlan {
            arguments: canonical.plan.arguments.clone(),
            format_version: canonical.plan.format_version,
            artifact_bindings: vec![LegacyBinding {
                binding_id: "model".to_owned(),
                model_artifact_id: "fixture-ninfer-artifact".to_owned(),
                destination: "/opt/dotcore/models/model.ninfer".to_owned(),
                read_only: true,
            }],
        }
        .encode_to_vec();
        assert!(legacy_bytes.len() > canonical.plan.encode_to_vec().len());
        let mut snapshot = crate::test_support::ninfer::local_model_snapshot("qwen3-8-27b");
        let Some(Source::Local(local)) = snapshot.source.as_mut() else {
            panic!("local snapshot");
        };
        local.desired_launch_plan = Some(
            PortableModelLaunchPlan::decode(legacy_bytes.as_slice()).expect("legacy plan decodes"),
        );

        let legacy = ResolvedModelLaunch::from_snapshot(&ModelDeploymentSnapshot {
            source: snapshot.source,
        })
        .expect("legacy retained plan validates");
        let target = target(&["GPU-0"]);
        let expected = workload(&canonical, &target, &prepared()).expect("canonical workload");
        let retained = workload(&legacy, &target, &prepared()).expect("legacy workload");

        assert_eq!(retained, expected);
        assert_eq!(
            retained.artifacts[0].destination,
            "/opt/dotcore/models/model.ninfer"
        );
        assert_eq!(
            retained.reconcile_generation(&target),
            expected.reconcile_generation(&target)
        );
    }

    fn resolved() -> ResolvedModelLaunch {
        ResolvedModelLaunch::from_snapshot(&crate::test_support::ninfer::local_model_snapshot(
            "qwen3-8-27b",
        ))
        .expect("resolved ninfer launch")
    }

    fn target(devices: &[&str]) -> ExecutionTarget {
        ExecutionTarget {
            managed_node_id: SiteNodeId::for_test("node"),
            accelerator_device_ids: devices.iter().map(|device| (*device).to_owned()).collect(),
        }
    }

    fn prepared() -> PreparedModelArtifacts {
        PreparedModelArtifacts {
            artifacts: BTreeMap::from([(
                "model".to_owned(),
                PathBuf::from("/cache/fixture_nvfp4.ninfer"),
            )]),
        }
    }
}
