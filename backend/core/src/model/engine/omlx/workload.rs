//! Materializes a resolved oMLX model launch into the native workload the runtime executes.
//!
//! It verifies the engine constraints, a native Nix package for darwin/arm64 with one Apple
//! accelerator, then binds the stored plan to the package launcher under the retained store path,
//! keeping the snapshot and service port tokens for the native runtime to resolve. The launcher
//! sets oMLX's environment itself, so the workload passes none.

use std::collections::BTreeMap;

use dotcore_artifact::PreparedModelArtifacts;
use dotcore_domain::{Accelerator, ExecutionPlatform, WorkloadArtifactKind};
use dotcore_proto::dotcore::{
    engine::v1::{EngineKind, resolved_engine_model_state::State},
    model::v1::{
        PortableModelLaunchPlan, portable_model_launch_argument_token::Value as ArgumentValue,
    },
};
use dotcore_runtime::{
    ExecutionTarget, WorkloadArtifactBinding, WorkloadExecutable, WorkloadExecutableArtifact,
    WorkloadIpcMode, WorkloadPort, WorkloadSpec, WorkloadToken, WorkloadValue,
};

use super::plan::{MODEL_BINDING_ID, PLAN_FORMAT_VERSION, SERVICE_NAME};
use crate::model::resolved::{ResolvedEngineSource, ResolvedModelLaunch};

/// Executable artifact kinds an oMLX engine artifact can provide: a Nix package for native
/// runtimes.
pub(super) const ARTIFACT_KINDS: &[WorkloadArtifactKind] = &[WorkloadArtifactKind::Nix];

/// Port the workload declares for its service. The native runtime assigns a loopback port in its
/// place, which the plan's port token resolves to.
const SERVICE_PORT: u16 = 8000;

/// Builds the workload spec by binding the stored symbolic plan to the package launcher.
pub(crate) fn workload(
    model: &ResolvedModelLaunch,
    target: &ExecutionTarget,
    artifacts: &PreparedModelArtifacts,
) -> Result<WorkloadSpec, OmlxWorkloadError> {
    validate_runtime_constraints(model, target)?;
    let ResolvedEngineSource::Nix { executable, .. } = &model.engine.source else {
        return Err(OmlxWorkloadError::UnsupportedEngine);
    };
    let materialized = materialize_plan(&model.plan, artifacts)?;
    Ok(WorkloadSpec {
        executable: WorkloadExecutable {
            artifact: WorkloadExecutableArtifact::NixStorePath(
                model.engine.launch_identity.clone(),
            ),
            path: format!("bin/{executable}"),
        },
        required_platform: ExecutionPlatform::darwin_arm64(),
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

fn validate_runtime_constraints(
    model: &ResolvedModelLaunch,
    target: &ExecutionTarget,
) -> Result<(), OmlxWorkloadError> {
    let state_matches = model
        .snapshot
        .resolved_engine_state
        .as_ref()
        .is_some_and(|state| {
            state.engine_kind == EngineKind::Omlx as i32
                && state.semantics_version == super::realization::SEMANTICS_VERSION
                && matches!(state.state, Some(State::Omlx(_)))
        });
    let platform = &model.engine.platform;
    if model.engine.kind != EngineKind::Omlx as i32
        || !state_matches
        || (
            platform.operating_system.as_str(),
            platform.architecture.as_str(),
            platform.accelerator.as_str(),
        ) != ("darwin", "arm64", Accelerator::Apple.catalog_name())
    {
        return Err(OmlxWorkloadError::UnsupportedEngine);
    }
    let [requirement] = model.hardware.accelerator_requirements.as_slice() else {
        return Err(OmlxWorkloadError::InvalidAcceleratorRequirement);
    };
    if requirement.vendor != Accelerator::Apple.catalog_name() || requirement.minimum_count != 1 {
        return Err(OmlxWorkloadError::InvalidAcceleratorRequirement);
    }
    if target.accelerator_device_ids.len() > 1
        || target
            .accelerator_device_ids
            .iter()
            .any(|device| !valid_device_id(device))
    {
        return Err(OmlxWorkloadError::UnsafeAcceleratorDevice);
    }
    Ok(())
}

/// Binds the plan to the prepared snapshot: the executable token must lead, and every other token
/// is a literal, the snapshot binding, or the service port.
fn materialize_plan(
    plan: &PortableModelLaunchPlan,
    artifacts: &PreparedModelArtifacts,
) -> Result<MaterializedPlan, OmlxWorkloadError> {
    if plan.format_version != PLAN_FORMAT_VERSION {
        return Err(OmlxWorkloadError::UnsupportedPlan);
    }
    let [binding] = plan.artifact_bindings.as_slice() else {
        return Err(OmlxWorkloadError::InvalidPlan);
    };
    if binding.binding_id != MODEL_BINDING_ID || !binding.read_only || binding.artifact.is_none() {
        return Err(OmlxWorkloadError::InvalidPlan);
    }
    let source = artifacts
        .artifacts
        .get(MODEL_BINDING_ID)
        .ok_or(OmlxWorkloadError::MissingPreparedArtifact)?;
    if artifacts.artifacts.len() != 1 {
        return Err(OmlxWorkloadError::UnexpectedPreparedArtifact);
    }
    let Some((executable, arguments)) = plan.arguments.split_first() else {
        return Err(OmlxWorkloadError::InvalidPlan);
    };
    if !matches!(executable.value, Some(ArgumentValue::EngineExecutable(_))) {
        return Err(OmlxWorkloadError::InvalidPlan);
    }
    let arguments = arguments
        .iter()
        .map(|argument| match argument.value.as_ref() {
            Some(ArgumentValue::Literal(value)) => Ok(WorkloadValue::literal(value)),
            Some(ArgumentValue::ArtifactBindingId(id)) if id == MODEL_BINDING_ID => {
                Ok(WorkloadValue::token(WorkloadToken::Artifact(id.clone())))
            }
            Some(ArgumentValue::ServicePort(name)) if name == SERVICE_NAME => Ok(
                WorkloadValue::token(WorkloadToken::ServicePort(name.clone())),
            ),
            _ => Err(OmlxWorkloadError::InvalidPlan),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(MaterializedPlan {
        arguments,
        artifacts: vec![WorkloadArtifactBinding {
            name: MODEL_BINDING_ID.to_owned(),
            source: source.clone(),
            // Native runtimes pass the host path through the artifact token and mount nothing.
            destination: "/opt/dotcore/models/snapshot".to_owned(),
            read_only: true,
        }],
    })
}

#[derive(Debug)]
struct MaterializedPlan {
    arguments: Vec<WorkloadValue>,
    artifacts: Vec<WorkloadArtifactBinding>,
}

/// Indicates the stored plan cannot safely materialize into an oMLX workload.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum OmlxWorkloadError {
    #[error(
        "oMLX model profile requires a native Nix engine artifact for darwin/arm64 with Apple accelerators"
    )]
    UnsupportedEngine,

    #[error("oMLX model profile requires one Apple accelerator")]
    InvalidAcceleratorRequirement,

    #[error("oMLX model profile has an unsafe accelerator device identity")]
    UnsafeAcceleratorDevice,

    #[error("oMLX workload has an unsupported desired-plan format")]
    UnsupportedPlan,

    #[error("oMLX workload has corrupt desired-plan bindings or arguments")]
    InvalidPlan,

    #[error("oMLX workload has no prepared artifact for a plan binding")]
    MissingPreparedArtifact,

    #[error("oMLX workload has an unexpected prepared artifact binding")]
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

    use super::*;
    use crate::test_support::omlx::{STORE_PATH, local_model_snapshot};

    #[test]
    fn the_package_launcher_runs_from_the_retained_store_path_on_the_apple_gpu() {
        let model = resolved();

        let spec = workload(
            &model,
            &apple_gpu(),
            &prepared([("model", "/artifacts/snapshot")]),
        )
        .expect("oMLX workload");

        assert_eq!(
            spec.executable,
            WorkloadExecutable {
                artifact: WorkloadExecutableArtifact::NixStorePath(STORE_PATH.to_owned()),
                path: "bin/omlx-launcher".to_owned(),
            }
        );
        assert_eq!(spec.required_platform, ExecutionPlatform::darwin_arm64());
        assert_eq!(
            spec.arguments
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            [
                "--snapshot",
                "{artifact:model}",
                "--served-id",
                "qwen3-5-4b",
                "--port",
                "{service:http.port}",
                "--context-per-request",
                "32768",
                "--parallel-requests",
                "2",
                "--memory-ceiling-gb",
                "10.573",
                "--prompt-cache-gb",
                "2",
            ]
        );
        assert_eq!(spec.environment, BTreeMap::new());
        assert_eq!(
            spec.artifacts,
            [WorkloadArtifactBinding {
                name: "model".to_owned(),
                source: PathBuf::from("/artifacts/snapshot"),
                destination: "/opt/dotcore/models/snapshot".to_owned(),
                read_only: true,
            }]
        );
        assert_eq!(
            spec.ports,
            [WorkloadPort {
                name: "http".to_owned(),
                container_port: 8000,
            }]
        );
        assert_eq!(spec.accelerator_device_ids, ["apple-gpu-0"]);
        assert_eq!(spec.required_accelerator_device_count, 1);
    }

    #[test]
    fn workload_rejects_engines_other_than_a_native_apple_package() {
        let mut image = resolved();
        image.engine.source = ResolvedEngineSource::OciImage {
            reference: "registry.dotcore.test/omlx:dev".to_owned(),
        };
        let mut linux = resolved();
        linux.engine.platform.operating_system = "linux".to_owned();
        let mut nvidia = resolved();
        nvidia.engine.platform.accelerator = "nvidia".to_owned();
        let mut other_kind = resolved();
        other_kind.engine.kind = EngineKind::LlamaCpp as i32;

        for (case, model) in [
            ("OCI image", image),
            ("Linux", linux),
            ("NVIDIA", nvidia),
            ("other kind", other_kind),
        ] {
            assert_eq!(
                workload(
                    &model,
                    &apple_gpu(),
                    &prepared([("model", "/artifacts/snapshot")])
                ),
                Err(OmlxWorkloadError::UnsupportedEngine),
                "{case}"
            );
        }
        assert_eq!(
            OmlxWorkloadError::UnsupportedEngine.to_string(),
            "oMLX model profile requires a native Nix engine artifact for darwin/arm64 with Apple \
             accelerators"
        );
    }

    #[test]
    fn workload_rejects_another_accelerator_requirement_or_several_devices() {
        let mut nvidia = resolved();
        nvidia.hardware.accelerator_requirements[0].vendor = "nvidia".to_owned();
        assert_eq!(
            workload(
                &nvidia,
                &apple_gpu(),
                &prepared([("model", "/artifacts/snapshot")])
            ),
            Err(OmlxWorkloadError::InvalidAcceleratorRequirement)
        );

        let two_devices = ExecutionTarget {
            accelerator_device_ids: vec!["apple-gpu-0".to_owned(), "apple-gpu-1".to_owned()],
            ..apple_gpu()
        };
        assert_eq!(
            workload(
                &resolved(),
                &two_devices,
                &prepared([("model", "/artifacts/snapshot")])
            ),
            Err(OmlxWorkloadError::UnsafeAcceleratorDevice)
        );
    }

    #[test]
    fn materialization_fails_closed_on_corrupt_plans_and_prepared_artifacts() {
        let plan = resolved().plan;
        assert_eq!(
            materialize_plan(&plan, &prepared([])).expect_err("missing prepared artifact"),
            OmlxWorkloadError::MissingPreparedArtifact
        );
        assert_eq!(
            materialize_plan(
                &plan,
                &prepared([
                    ("model", "/artifacts/snapshot"),
                    ("extra", "/artifacts/extra")
                ])
            )
            .expect_err("unexpected prepared artifact"),
            OmlxWorkloadError::UnexpectedPreparedArtifact
        );

        let mut unsupported = plan.clone();
        unsupported.format_version = 2;
        assert_eq!(
            materialize_plan(&unsupported, &prepared([("model", "/artifacts/snapshot")]))
                .expect_err("unsupported plan"),
            OmlxWorkloadError::UnsupportedPlan
        );

        let mut missing_executable = plan.clone();
        missing_executable.arguments.remove(0);
        let mut service_host = plan.clone();
        service_host.arguments[6].value = Some(ArgumentValue::ServiceHost("http".to_owned()));
        let mut unknown_binding = plan.clone();
        unknown_binding.artifact_bindings[0].binding_id = "weights".to_owned();
        let mut writable = plan;
        writable.artifact_bindings[0].read_only = false;
        for (case, corrupt) in [
            ("missing executable", missing_executable),
            ("service host", service_host),
            ("unknown binding", unknown_binding),
            ("writable", writable),
        ] {
            assert_eq!(
                materialize_plan(&corrupt, &prepared([("model", "/artifacts/snapshot")]))
                    .expect_err(case),
                OmlxWorkloadError::InvalidPlan,
                "{case}"
            );
        }
    }

    fn resolved() -> ResolvedModelLaunch {
        ResolvedModelLaunch::from_snapshot(&local_model_snapshot("qwen3-5-4b"))
            .expect("resolved launch")
    }

    fn apple_gpu() -> ExecutionTarget {
        ExecutionTarget {
            managed_node_id: dotcore_domain::SiteNodeId::for_test("node"),
            accelerator_device_ids: vec![dotcore_domain::APPLE_GPU_DEVICE_ID.to_owned()],
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
