//! Materializes a resolved vLLM model launch into the OCI workload the runtime executes.
//!
//! It verifies the engine and accelerator constraints—a Linux NVIDIA image with exactly one accelerator and
//! a safe device identity—then materializes the stored portable plan into the container command line and
//! read-only artifact mounts, exposing the engine's fixed container port as the HTTP service port.

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

use super::plan::{
    MODEL_BINDING_ID, PLAN_FORMAT_VERSION, SPECULATIVE_DRAFT_BINDING_ID, TEMPLATE_BINDING_ID,
};
use crate::model::resolved::ResolvedModelLaunch;

/// Executable artifact kinds a vLLM engine artifact can provide.
pub(super) const ARTIFACT_KINDS: &[WorkloadArtifactKind] = &[WorkloadArtifactKind::Oci];

const SERVICE_PORT_NAME: &str = "http";

/// Builds the literal container workload spec by materializing the stored portable plan.
pub(crate) fn workload(
    model: &ResolvedModelLaunch,
    target: &ExecutionTarget,
    artifacts: &PreparedModelArtifacts,
) -> Result<WorkloadSpec, VllmWorkloadError> {
    let fixed = validate_runtime_constraints(model, target)?;
    let materialized = materialize_plan(&model.plan, artifacts)?;
    let container_port =
        u16::try_from(fixed.container_port).map_err(|_| VllmWorkloadError::UnsupportedEngine)?;
    let required_platform = ExecutionPlatform::normalize(
        &model.engine.platform.operating_system,
        &model.engine.platform.architecture,
    )
    .map_err(|_| VllmWorkloadError::UnsupportedEngine)?;
    let (executable, arguments) = materialized
        .command
        .split_first()
        .ok_or(VllmWorkloadError::InvalidPlan)?;
    Ok(WorkloadSpec {
        executable: WorkloadExecutable {
            artifact: WorkloadExecutableArtifact::OciImage(
                model
                    .engine
                    .container_image()
                    .ok_or(VllmWorkloadError::UnsupportedEngine)?,
            ),
            path: executable.clone(),
        },
        required_platform,
        arguments: arguments.iter().map(WorkloadValue::literal).collect(),
        environment: BTreeMap::new(),
        artifacts: materialized.mounts,
        storage: Vec::new(),
        ports: vec![WorkloadPort {
            name: SERVICE_PORT_NAME.to_owned(),
            container_port,
        }],
        accelerator_device_ids: target.accelerator_device_ids.clone(),
        required_accelerator_device_count: model.hardware.accelerator_device_count(),
        ipc_mode: if fixed.requires_host_ipc {
            WorkloadIpcMode::HostShared
        } else {
            WorkloadIpcMode::Isolated
        },
        expected_memory: Default::default(),
    })
}

fn validate_runtime_constraints<'a>(
    model: &'a ResolvedModelLaunch,
    target: &ExecutionTarget,
) -> Result<&'a dotcore_proto::dotcore::engine::vllm::v1::VllmFixedModelSettings, VllmWorkloadError>
{
    let fixed = model
        .snapshot
        .resolved_engine_state
        .as_ref()
        .filter(|state| {
            model.engine.kind == EngineKind::Vllm as i32
                && state.engine_kind == EngineKind::Vllm as i32
                && state.semantics_version == super::realization::SEMANTICS_VERSION
        })
        .and_then(|state| state.state.as_ref())
        .and_then(|state| match state {
            State::Vllm(state) => state.fixed_settings.as_ref(),
            State::LlamaCpp(_) | State::Ninfer(_) => None,
        })
        .ok_or(VllmWorkloadError::UnsupportedEngine)?;
    if fixed.container_port == 0
        || fixed.container_port > u16::MAX as u32
        || model.engine.platform.operating_system != "linux"
        || !matches!(
            model.engine.platform.architecture.as_str(),
            "amd64" | "arm64"
        )
        || model.engine.platform.accelerator != "nvidia"
    {
        return Err(VllmWorkloadError::UnsupportedEngine);
    }
    let [requirement] = model.hardware.accelerator_requirements.as_slice() else {
        return Err(VllmWorkloadError::InvalidAcceleratorRequirement);
    };
    if requirement.vendor != "nvidia" || requirement.minimum_count != 1 {
        return Err(VllmWorkloadError::InvalidAcceleratorRequirement);
    }
    if target.accelerator_device_ids.len() > requirement.minimum_count as usize
        || target
            .accelerator_device_ids
            .iter()
            .any(|device| !valid_device_id(device))
    {
        return Err(VllmWorkloadError::UnsafeAcceleratorDevice);
    }
    Ok(fixed)
}

pub(super) fn materialize_plan(
    plan: &PortableModelLaunchPlan,
    artifacts: &PreparedModelArtifacts,
) -> Result<MaterializedPlan, VllmWorkloadError> {
    if plan.format_version != PLAN_FORMAT_VERSION {
        return Err(VllmWorkloadError::UnsupportedPlan);
    }
    let bindings = prepared_bindings(plan, artifacts)?;
    let command = plan
        .arguments
        .iter()
        .map(|argument| match argument.value.as_ref() {
            Some(ArgumentValue::Literal(value)) => Ok(value.clone()),
            Some(ArgumentValue::ArtifactBindingId(id)) => bindings
                .get(id)
                .map(|binding| binding.destination.clone())
                .ok_or(VllmWorkloadError::InvalidPlan),
            _ => Err(VllmWorkloadError::InvalidPlan),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if command.first().is_none_or(String::is_empty) {
        return Err(VllmWorkloadError::InvalidPlan);
    }
    Ok(MaterializedPlan {
        command,
        mounts: bindings
            .into_values()
            .map(|binding| binding.mount)
            .collect(),
    })
}

fn prepared_bindings(
    plan: &PortableModelLaunchPlan,
    artifacts: &PreparedModelArtifacts,
) -> Result<BTreeMap<String, PreparedBinding>, VllmWorkloadError> {
    let mut bindings = BTreeMap::new();
    for binding in &plan.artifact_bindings {
        let destination =
            oci_destination(&binding.binding_id).ok_or(VllmWorkloadError::InvalidPlan)?;
        if !binding.read_only || binding.artifact.is_none() {
            return Err(VllmWorkloadError::InvalidPlan);
        }
        let source = artifacts
            .artifacts
            .get(&binding.binding_id)
            .ok_or(VllmWorkloadError::MissingPreparedArtifact)?;
        let prepared = PreparedBinding {
            destination: destination.to_owned(),
            mount: WorkloadArtifactBinding {
                name: binding.binding_id.clone(),
                source: source.clone(),
                destination: destination.to_owned(),
                read_only: true,
            },
        };
        if bindings
            .insert(binding.binding_id.clone(), prepared)
            .is_some()
        {
            return Err(VllmWorkloadError::InvalidPlan);
        }
    }
    if artifacts
        .artifacts
        .keys()
        .any(|binding_id| !bindings.contains_key(binding_id))
    {
        return Err(VllmWorkloadError::UnexpectedPreparedArtifact);
    }
    Ok(bindings)
}

/// Returns the container path at which a vLLM image reads one plan binding.
fn oci_destination(binding_id: &str) -> Option<&'static str> {
    match binding_id {
        MODEL_BINDING_ID => Some("/opt/dotcore/models/snapshot"),
        TEMPLATE_BINDING_ID => Some("/opt/dotcore/models/chat-template.jinja"),
        SPECULATIVE_DRAFT_BINDING_ID => Some("/opt/dotcore/models/speculative-draft"),
        _ => None,
    }
}

#[derive(Debug)]
pub(super) struct MaterializedPlan {
    pub(super) command: Vec<String>,
    mounts: Vec<WorkloadArtifactBinding>,
}

struct PreparedBinding {
    destination: String,
    mount: WorkloadArtifactBinding,
}

/// Indicates the stored plan cannot safely materialize into a vLLM workload.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum VllmWorkloadError {
    #[error("vLLM model profile requires a linux NVIDIA engine artifact")]
    UnsupportedEngine,

    #[error("vLLM model profile requires one NVIDIA accelerator")]
    InvalidAcceleratorRequirement,

    #[error("vLLM model profile has an unsafe accelerator device identity")]
    UnsafeAcceleratorDevice,

    #[error("vLLM workload has an unsupported desired-plan format")]
    UnsupportedPlan,

    #[error("vLLM workload has corrupt desired-plan bindings or arguments")]
    InvalidPlan,

    #[error("vLLM workload has no prepared artifact for a plan binding")]
    MissingPreparedArtifact,

    #[error("vLLM workload has an unexpected prepared artifact binding")]
    UnexpectedPreparedArtifact,
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

    use dotcore_runtime::{WorkloadStorageAttachment, WorkloadStorageKind};

    use dotcore_proto::dotcore::{
        engine::{
            v1::ResolvedEngineModelState,
            vllm::v1::{VllmFixedModelSettings, VllmResolvedModelState},
        },
        model::v1::{
            PortableModelArtifactBinding, PortableModelLaunchArgumentToken,
            portable_model_artifact_binding::Artifact,
        },
    };

    use super::*;

    #[test]
    fn device_identity_validation_rejects_unsafe_and_accepts_declared_values() {
        assert!(valid_device_id("GPU-7"));
        assert!(!valid_device_id("GPU 7"));
        assert!(!valid_device_id("GPU,7"));
        assert!(!valid_device_id("GPU\n7"));
    }

    #[test]
    fn workload_image_is_the_retained_launch_identity_not_the_catalog_reference() {
        const IMAGE_ID: &str =
            "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
        let mut model = ResolvedModelLaunch::from_snapshot(
            &crate::test_support::local_model_snapshot("fixture-model"),
        )
        .expect("resolved launch");
        model.engine.kind = EngineKind::Vllm as i32;
        model.engine.platform.operating_system = "linux".to_owned();
        model.engine.platform.architecture = "amd64".to_owned();
        model.engine.platform.accelerator = "nvidia".to_owned();
        model.engine.source = crate::model::resolved::ResolvedEngineSource::OciImage {
            reference: "registry.dotcore.test/fixture:dev".to_owned(),
        };
        model.engine.launch_identity = IMAGE_ID.to_owned();
        model.plan = PortableModelLaunchPlan {
            format_version: PLAN_FORMAT_VERSION,
            arguments: vec![literal_argument("vllm")],
            artifact_bindings: Vec::new(),
        };
        model.snapshot.resolved_engine_state = Some(ResolvedEngineModelState {
            engine_kind: EngineKind::Vllm as i32,
            semantics_version: super::super::realization::SEMANTICS_VERSION,
            state: Some(State::Vllm(VllmResolvedModelState {
                fixed_settings: Some(VllmFixedModelSettings {
                    container_port: 8000,
                    ..Default::default()
                }),
                option_realizations: Vec::new(),
            })),
        });

        let workload = workload(
            &model,
            &ExecutionTarget {
                managed_node_id: dotcore_domain::SiteNodeId::for_test("node"),
                accelerator_device_ids: vec!["GPU-0".to_owned()],
            },
            &PreparedModelArtifacts {
                artifacts: BTreeMap::new(),
            },
        )
        .expect("materialized workload");

        assert_eq!(workload.executable.artifact.reference(), IMAGE_ID);
    }

    #[test]
    fn master_catalog_engine_cache_attaches_to_the_engine_artifact() {
        const IMAGE_ID: &str =
            "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
        let catalog = dotcore_catalog::Catalog::load_with_policy(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../catalog"),
            dotcore_catalog::OciArtifactPolicy::Development,
            crate::model::engine::declaration_validator(),
        )
        .expect("master catalog");
        let release = catalog
            .list_engine_releases()
            .into_iter()
            .find(|release| release.engine_kind == EngineKind::Vllm as i32)
            .expect("vLLM engine release");
        let mut snapshot = crate::test_support::local_model_snapshot("fixture-model");
        let Some(dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(
            local,
        )) = snapshot.source.as_mut()
        else {
            panic!("local fixture snapshot");
        };
        local
            .engine_artifact
            .as_mut()
            .expect("engine artifact")
            .storage_bindings = release.storage_bindings;
        let mut model = ResolvedModelLaunch::from_snapshot(&snapshot).expect("resolved launch");
        model.engine.kind = EngineKind::Vllm as i32;
        model.engine.platform.operating_system = "linux".to_owned();
        model.engine.platform.architecture = "amd64".to_owned();
        model.engine.platform.accelerator = "nvidia".to_owned();
        model.engine.source = crate::model::resolved::ResolvedEngineSource::OciImage {
            reference: "registry.dotcore.test/fixture:dev".to_owned(),
        };
        model.engine.launch_identity = IMAGE_ID.to_owned();
        model.plan = PortableModelLaunchPlan {
            format_version: PLAN_FORMAT_VERSION,
            arguments: vec![literal_argument("vllm")],
            artifact_bindings: Vec::new(),
        };
        model.snapshot.resolved_engine_state = Some(ResolvedEngineModelState {
            engine_kind: EngineKind::Vllm as i32,
            semantics_version: super::super::realization::SEMANTICS_VERSION,
            state: Some(State::Vllm(VllmResolvedModelState {
                fixed_settings: Some(VllmFixedModelSettings {
                    container_port: 8000,
                    ..Default::default()
                }),
                option_realizations: Vec::new(),
            })),
        });

        let workload = crate::model::engine::EngineIntegrationRegistry::production()
            .materialize_workload(
                &model,
                &ExecutionTarget {
                    managed_node_id: dotcore_domain::SiteNodeId::for_test("node"),
                    accelerator_device_ids: vec!["GPU-0".to_owned()],
                },
                &PreparedModelArtifacts {
                    artifacts: BTreeMap::new(),
                },
                "deployment-one",
            )
            .expect("materialized workload");

        // The key is the first 16 bytes of the SHA-256 of `IMAGE_ID`.
        assert_eq!(
            workload.storage,
            [WorkloadStorageAttachment {
                name: "engine-cache".to_owned(),
                destination: "/var/cache/dotcore-engine".to_owned(),
                kind: WorkloadStorageKind::Cache,
                namespace: "engine-artifact".to_owned(),
                key: "sha256-e3dac0fe1f10c8592b3f267de66bffe4".to_owned(),
                read_only: false,
                expires_when_unused: true,
            }]
        );
        let cache_environment = [
            "VLLM_CACHE_ROOT",
            "TRITON_CACHE_DIR",
            "FLASHINFER_WORKSPACE_BASE",
            "XDG_CACHE_HOME",
        ]
        .map(|name| (name, workload.environment.get(name).cloned()));
        assert_eq!(
            cache_environment,
            [
                (
                    "VLLM_CACHE_ROOT",
                    Some(WorkloadValue::literal("/var/cache/dotcore-engine/vllm"))
                ),
                (
                    "TRITON_CACHE_DIR",
                    Some(WorkloadValue::literal("/var/cache/dotcore-engine/triton"))
                ),
                (
                    "FLASHINFER_WORKSPACE_BASE",
                    Some(WorkloadValue::literal("/var/cache/dotcore-engine"))
                ),
                ("XDG_CACHE_HOME", None),
            ]
        );
    }

    #[test]
    fn materializes_image_entrypoint_arguments_and_read_only_bindings() {
        let plan = PortableModelLaunchPlan {
            format_version: PLAN_FORMAT_VERSION,
            arguments: vec![
                literal_argument("vllm"),
                literal_argument("serve"),
                binding_argument("model-snapshot"),
                literal_argument("--served-model-name"),
                literal_argument("fixture-model"),
                literal_argument("--chat-template"),
                binding_argument("chat-template"),
            ],
            artifact_bindings: vec![
                artifact_binding("model-snapshot", "model-artifact"),
                artifact_binding("chat-template", "template-artifact"),
            ],
        };
        let artifacts = PreparedModelArtifacts {
            artifacts: BTreeMap::from([
                (
                    "chat-template".to_owned(),
                    PathBuf::from("/cache/template.jinja"),
                ),
                (
                    "model-snapshot".to_owned(),
                    PathBuf::from("/cache/model-snapshot"),
                ),
            ]),
        };

        let materialized = materialize_plan(&plan, &artifacts).expect("materialized plan");

        assert_eq!(
            materialized.command,
            [
                "vllm",
                "serve",
                "/opt/dotcore/models/snapshot",
                "--served-model-name",
                "fixture-model",
                "--chat-template",
                "/opt/dotcore/models/chat-template.jinja",
            ]
        );
        assert_eq!(
            materialized.mounts,
            [
                WorkloadArtifactBinding {
                    name: "chat-template".to_owned(),
                    source: PathBuf::from("/cache/template.jinja"),
                    destination: "/opt/dotcore/models/chat-template.jinja".to_owned(),
                    read_only: true,
                },
                WorkloadArtifactBinding {
                    name: "model-snapshot".to_owned(),
                    source: PathBuf::from("/cache/model-snapshot"),
                    destination: "/opt/dotcore/models/snapshot".to_owned(),
                    read_only: true,
                },
            ]
        );
    }

    #[test]
    fn materialization_rejects_unbound_prepared_artifacts() {
        let plan = PortableModelLaunchPlan {
            format_version: PLAN_FORMAT_VERSION,
            arguments: vec![literal_argument("model")],
            artifact_bindings: Vec::new(),
        };
        let artifacts = PreparedModelArtifacts {
            artifacts: BTreeMap::from([(
                "unexpected".to_owned(),
                PathBuf::from("/cache/unexpected"),
            )]),
        };

        assert_eq!(
            materialize_plan(&plan, &artifacts).expect_err("unexpected artifact"),
            VllmWorkloadError::UnexpectedPreparedArtifact
        );
    }

    fn artifact_binding(binding_id: &str, artifact_id: &str) -> PortableModelArtifactBinding {
        PortableModelArtifactBinding {
            binding_id: binding_id.to_owned(),
            artifact: Some(Artifact::ModelArtifactId(artifact_id.to_owned())),
            read_only: true,
        }
    }

    fn binding_argument(binding_id: &str) -> PortableModelLaunchArgumentToken {
        PortableModelLaunchArgumentToken {
            value: Some(ArgumentValue::ArtifactBindingId(binding_id.to_owned())),
        }
    }

    fn literal_argument(value: &str) -> PortableModelLaunchArgumentToken {
        PortableModelLaunchArgumentToken {
            value: Some(ArgumentValue::Literal(value.to_owned())),
        }
    }
}
