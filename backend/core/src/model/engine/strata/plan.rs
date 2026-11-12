//! Translation and validation of the portable Strata launch plan for a resolved deployment.
//!
//! From a resolved deployment snapshot it builds the canonical command line of Dotcore's Strata
//! start wrapper, run under the image's init: the model's files mounted read-only, with the draft
//! layer and a ready-made pack's file roles when the model has them, the deployment's model
//! identity as the served name, the configuration's architecture and MTP draft depth, the context
//! per request, the parallel requests, the expert accelerator memory cap, and whether the pack path
//! runs on unified memory. The wrapper takes no other arguments, so custom arguments are refused.
//! It also checks that a retained plan exactly matches the plan its resolved state would produce.

use dotcore_catalog::shared_parameters::{
    CONTEXT_PER_REQUEST_PARAMETER_ID, PARALLEL_REQUESTS_PARAMETER_ID,
};
use dotcore_proto::dotcore::{
    catalog::v1::engine_parameter_value::Value as EngineParameterValue,
    engine::{
        strata::v1::{StrataResolvedModelState, strata_fixed_model_settings::ModelFiles},
        v1::{EngineKind, resolved_engine_model_state::State},
    },
    model::v1::{
        PortableModelArtifactBinding, PortableModelLaunchArgumentToken, PortableModelLaunchPlan,
        ResolvedLocalModelDeploymentSnapshot,
        portable_model_artifact_binding::Artifact as BoundArtifact,
        portable_model_launch_argument_token::Value as ArgumentValue,
    },
    observation::v1::AcceleratorMemoryArchitecture,
};

use super::{SERVICE_PORT, draft_layer_artifact_id, valid_fixed_settings};

pub(super) const PLAN_FORMAT_VERSION: u32 = 1;
pub(super) const MODEL_BINDING_ID: &str = "model";
pub(super) const DRAFT_LAYER_BINDING_ID: &str = "draft-layer";
/// The image's init, run explicitly because the runtime replaces the image's entrypoint with the
/// workload's executable; it reaps the engine process and forwards stop signals to the server.
const INIT_BINARY: &str = "/usr/bin/tini";
const START_WRAPPER: &str = "/usr/local/bin/dotcore-strata-start";

/// Produces the single portable desired plan for an accepted Strata selection.
pub(crate) fn translate(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<PortableModelLaunchPlan, StrataPlanError> {
    let state = resolved_state(snapshot)?;
    let fixed = state
        .fixed_settings
        .as_ref()
        .ok_or(StrataPlanError::NoFixedSettings)?;
    if !valid_fixed_settings(fixed) {
        return Err(StrataPlanError::InvalidResolvedState);
    }
    let model = snapshot
        .model_artifact
        .as_ref()
        .ok_or(StrataPlanError::NoModelArtifact)?;
    let expert_vram_mib = positive_parameter(snapshot, &fixed.expert_vram_mib_parameter_id)?;
    // The wrapper parses with argparse, where any token could name or abbreviate an option the
    // plan sets, and it takes no other arguments.
    if let Some(token) = snapshot.custom_argument_tokens.first() {
        return Err(StrataPlanError::CustomArgument(token.clone()));
    }

    let mut arguments = vec![
        literal(INIT_BINARY),
        literal("--"),
        literal(START_WRAPPER),
        literal("--model"),
        binding(MODEL_BINDING_ID),
    ];
    let mut artifact_bindings = vec![PortableModelArtifactBinding {
        binding_id: MODEL_BINDING_ID.to_owned(),
        artifact: Some(BoundArtifact::ModelArtifactId(model.artifact_id.clone())),
        read_only: true,
    }];
    let draft_layer = draft_layer_artifact_id(fixed);
    if let Some(artifact_id) = draft_layer {
        if !snapshot
            .auxiliary_artifacts
            .iter()
            .any(|artifact| artifact.auxiliary_artifact_id == artifact_id)
        {
            return Err(StrataPlanError::DraftLayerNotResolved);
        }
        arguments.extend([literal("--draft-layer"), binding(DRAFT_LAYER_BINDING_ID)]);
        artifact_bindings.push(PortableModelArtifactBinding {
            binding_id: DRAFT_LAYER_BINDING_ID.to_owned(),
            artifact: Some(BoundArtifact::AuxiliaryArtifactId(artifact_id.to_owned())),
            read_only: true,
        });
    }
    if let Some(ModelFiles::ReadyPack(pack)) = fixed.model_files.as_ref() {
        arguments.extend(literals([
            "--pack",
            &pack.pack_path,
            "--dense-gguf",
            &pack.dense_gguf_path,
            "--lookup-table-gguf",
            &pack.lookup_table_gguf_path,
            "--token-embedding-gguf",
            &pack.token_embedding_gguf_path,
        ]));
    }
    arguments.extend(literals([
        "--model-name",
        &snapshot.model_id,
        "--arch",
        &fixed.architecture,
        "--mtp",
        &fixed.mtp_draft_tokens.to_string(),
        "--context",
        &positive_parameter(snapshot, CONTEXT_PER_REQUEST_PARAMETER_ID)?.to_string(),
        "--parallel",
        &positive_parameter(snapshot, PARALLEL_REQUESTS_PARAMETER_ID)?.to_string(),
        "--expert-vram-mib",
        &expert_vram_mib.to_string(),
    ]));
    // Only the pack path has settings for an accelerator sharing the host's memory.
    if draft_layer.is_some() && uses_unified_memory(snapshot)? {
        arguments.push(literal("--unified-memory"));
    }
    arguments.extend(literals(["--port", &SERVICE_PORT.to_string()]));
    Ok(PortableModelLaunchPlan {
        arguments,
        format_version: PLAN_FORMAT_VERSION,
        artifact_bindings,
    })
}

/// Validates that a retained plan is canonical for its versioned resolved state.
pub(crate) fn validate_snapshot(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<(), StrataPlanError> {
    if let Some(plan) = snapshot.desired_launch_plan.as_ref() {
        let found = plan.format_version;
        if found < PLAN_FORMAT_VERSION {
            return Err(StrataPlanError::RetainedPlanFormat { found });
        }
        if found > PLAN_FORMAT_VERSION {
            return Err(StrataPlanError::NewerRetainedPlanFormat { found });
        }
    }
    if !snapshot.resolved_options.is_empty() {
        return Err(StrataPlanError::InvalidResolvedState);
    }
    let expected = translate(snapshot)?;
    // The selected auxiliary artifacts are exactly the ones the plan binds.
    let selected = snapshot
        .auxiliary_artifacts
        .iter()
        .map(|artifact| artifact.auxiliary_artifact_id.as_str())
        .collect::<Vec<_>>();
    let bound = expected
        .artifact_bindings
        .iter()
        .filter_map(|binding| match binding.artifact.as_ref() {
            Some(BoundArtifact::AuxiliaryArtifactId(id)) => Some(id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if selected != bound {
        return Err(StrataPlanError::InvalidResolvedState);
    }
    if snapshot.desired_launch_plan.as_ref() != Some(&expected) {
        return Err(StrataPlanError::NonCanonicalPlan);
    }
    Ok(())
}

fn resolved_state(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<&StrataResolvedModelState, StrataPlanError> {
    snapshot
        .resolved_engine_state
        .as_ref()
        .filter(|state| {
            state.engine_kind == EngineKind::Strata as i32
                && state.semantics_version == super::realization::SEMANTICS_VERSION
        })
        .and_then(|state| state.state.as_ref())
        .and_then(|state| match state {
            State::Strata(state) => Some(state),
            State::LlamaCpp(_) | State::Vllm(_) | State::Ninfer(_) => None,
        })
        .ok_or(StrataPlanError::InvalidResolvedState)
}

fn uses_unified_memory(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<bool, StrataPlanError> {
    match snapshot
        .selected_target
        .as_ref()
        .map(|target| target.accelerator_memory_architecture())
    {
        Some(AcceleratorMemoryArchitecture::Dedicated) => Ok(false),
        Some(AcceleratorMemoryArchitecture::Unified) => Ok(true),
        Some(AcceleratorMemoryArchitecture::Unspecified) | None => {
            Err(StrataPlanError::NoMemoryArchitecture)
        }
    }
}

fn positive_parameter(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
    parameter_id: &str,
) -> Result<u64, StrataPlanError> {
    snapshot
        .engine_parameter_values
        .as_ref()
        .and_then(|values| {
            values
                .values
                .iter()
                .find(|value| value.parameter_id == parameter_id)
        })
        .and_then(|value| match value.value.as_ref() {
            Some(EngineParameterValue::UnsignedInteger(value)) => Some(*value),
            _ => None,
        })
        .filter(|value| *value > 0)
        .ok_or_else(|| StrataPlanError::ParameterNotConfigured(parameter_id.to_owned()))
}

fn literals<const N: usize>(values: [&str; N]) -> Vec<PortableModelLaunchArgumentToken> {
    values.into_iter().map(literal).collect()
}

fn literal(value: &str) -> PortableModelLaunchArgumentToken {
    PortableModelLaunchArgumentToken {
        value: Some(ArgumentValue::Literal(value.to_owned())),
    }
}

fn binding(binding_id: &str) -> PortableModelLaunchArgumentToken {
    PortableModelLaunchArgumentToken {
        value: Some(ArgumentValue::ArtifactBindingId(binding_id.to_owned())),
    }
}

/// Indicates that resolved values cannot produce a complete portable Strata plan.
#[derive(Debug, thiserror::Error)]
pub(crate) enum StrataPlanError {
    #[error("Strata launch plan has invalid resolved engine state")]
    InvalidResolvedState,

    #[error("Strata launch plan is not canonical for its resolved state")]
    NonCanonicalPlan,

    #[error(
        "Strata deployment was created with launch plan format {found}, which this Dotcore \
         version no longer supports; delete and recreate the deployment"
    )]
    RetainedPlanFormat { found: u32 },

    #[error(
        "Strata deployment was created with launch plan format {found} by a newer Dotcore \
         version; upgrade Dotcore, or delete and recreate the deployment with this version"
    )]
    NewerRetainedPlanFormat { found: u32 },

    #[error("Strata launch plan has no fixed settings")]
    NoFixedSettings,

    #[error("Strata launch plan has no model artifact")]
    NoModelArtifact,

    #[error("Strata launch plan has no resolved draft layer artifact")]
    DraftLayerNotResolved,

    #[error("Strata launch plan has no selected target memory architecture")]
    NoMemoryArchitecture,

    #[error("Strata launch plan has no valid value for engine parameter {0:?}")]
    ParameterNotConfigured(String),

    #[error(
        "Strata takes no custom arguments, but got {0:?}; its start wrapper accepts only the \
         options Dotcore sets"
    )]
    CustomArgument(String),
}

#[cfg(test)]
mod tests {
    use dotcore_catalog::shared_parameters::TOTAL_CONTEXT_PARAMETER_ID;
    use dotcore_proto::dotcore::{
        catalog::v1::ModelEngineConfiguration,
        engine::strata::v1::{StrataFixedModelSettings, StrataReadyPack, StrataSplitGguf},
        model::v1::model_deployment_snapshot::Source,
    };

    use super::*;
    use crate::test_support::strata::{
        DRAFT_LAYER_ARTIFACT_ID, EXPERT_VRAM_MIB_PARAMETER_ID, configuration,
        local_model_snapshot_with, parameter_values, ready_pack_configuration, refresh_launch_plan,
        split_gguf_configuration,
    };

    #[test]
    fn profile_values_produce_the_exact_wrapper_command() {
        let local = local("qwen3-6-35b-a3b");

        let plan = translate(&local).expect("plan");

        assert_eq!(
            argv(&plan),
            [
                "/usr/bin/tini",
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
        );
        assert_eq!(plan.format_version, 1);
        assert_eq!(
            plan.artifact_bindings,
            [PortableModelArtifactBinding {
                binding_id: "model".to_owned(),
                artifact: Some(BoundArtifact::ModelArtifactId(
                    "fixture-strata-artifact".to_owned()
                )),
                read_only: true,
            }]
        );
        validate_snapshot(&local).expect("canonical profile plan");
    }

    #[test]
    fn parameter_overrides_reach_the_wrapper_with_the_exact_expert_cap() {
        let mut local = local("served-alias");
        local.engine_parameter_values = Some(parameter_values([
            (TOTAL_CONTEXT_PARAMETER_ID, 262_144),
            (CONTEXT_PER_REQUEST_PARAMETER_ID, 131_072),
            (PARALLEL_REQUESTS_PARAMETER_ID, 2),
            (EXPERT_VRAM_MIB_PARAMETER_ID, 11_999),
        ]));
        refresh_launch_plan(&mut local);

        let argv = argv(local.desired_launch_plan.as_ref().expect("plan"));

        assert_eq!(
            argv[5..],
            [
                "--model-name",
                "served-alias",
                "--arch",
                "qwen35moe",
                "--mtp",
                "2",
                "--context",
                "131072",
                "--parallel",
                "2",
                "--expert-vram-mib",
                "11999",
                "--port",
                "8080",
            ]
        );
        validate_snapshot(&local).expect("canonical overridden plan");
    }

    #[test]
    fn split_gguf_launches_the_pack_path_with_its_draft_layer() {
        let local = local_with("qwen3-8-flash-next", &split_gguf_configuration());

        let plan = translate(&local).expect("plan");

        assert_eq!(
            argv(&plan),
            [
                "/usr/bin/tini",
                "--",
                "/usr/local/bin/dotcore-strata-start",
                "--model",
                "<model>",
                "--draft-layer",
                "<draft-layer>",
                "--model-name",
                "qwen3-8-flash-next",
                "--arch",
                "qwen4exp",
                "--mtp",
                "4",
                "--context",
                "32768",
                "--parallel",
                "1",
                "--expert-vram-mib",
                "8192",
                "--port",
                "8080",
            ]
        );
        assert_eq!(
            plan.artifact_bindings,
            [
                PortableModelArtifactBinding {
                    binding_id: "model".to_owned(),
                    artifact: Some(BoundArtifact::ModelArtifactId(
                        "fixture-split-gguf".to_owned()
                    )),
                    read_only: true,
                },
                PortableModelArtifactBinding {
                    binding_id: "draft-layer".to_owned(),
                    artifact: Some(BoundArtifact::AuxiliaryArtifactId(
                        DRAFT_LAYER_ARTIFACT_ID.to_owned()
                    )),
                    read_only: true,
                },
            ]
        );
        validate_snapshot(&local).expect("canonical split GGUF plan");
    }

    #[test]
    fn ready_pack_on_unified_memory_names_its_files_and_the_unified_settings() {
        let mut local = local_with("qwen3-8-flash-next", &ready_pack_configuration());
        local
            .selected_target
            .as_mut()
            .expect("selected target")
            .accelerator_memory_architecture = AcceleratorMemoryArchitecture::Unified as i32;
        refresh_launch_plan(&mut local);

        let argv = argv(local.desired_launch_plan.as_ref().expect("plan"));

        assert_eq!(
            argv[3..],
            [
                "--model",
                "<model>",
                "--draft-layer",
                "<draft-layer>",
                "--pack",
                "pack",
                "--dense-gguf",
                "dense.gguf",
                "--lookup-table-gguf",
                "lookup-table.gguf",
                "--token-embedding-gguf",
                "token-embedding.gguf",
                "--model-name",
                "qwen3-8-flash-next",
                "--arch",
                "qwen4exp",
                "--mtp",
                "4",
                "--context",
                "32768",
                "--parallel",
                "1",
                "--expert-vram-mib",
                "8192",
                "--unified-memory",
                "--port",
                "8080",
            ]
        );
        validate_snapshot(&local).expect("canonical ready pack plan");
    }

    #[test]
    fn single_gguf_ignores_unified_memory_and_the_pack_path_requires_its_inputs() {
        let mut unified = local("qwen3-6-35b-a3b");
        unified.selected_target = None;
        assert_eq!(
            argv(&translate(&unified).expect("single GGUF plan")),
            argv(&translate(&local("qwen3-6-35b-a3b")).expect("dedicated plan"))
        );

        let mut no_target = local_with("qwen3-8-flash-next", &split_gguf_configuration());
        no_target.selected_target = None;
        assert!(matches!(
            translate(&no_target),
            Err(StrataPlanError::NoMemoryArchitecture)
        ));

        let mut no_draft = local_with("qwen3-8-flash-next", &split_gguf_configuration());
        no_draft.auxiliary_artifacts.clear();
        assert!(matches!(
            translate(&no_draft),
            Err(StrataPlanError::DraftLayerNotResolved)
        ));
    }

    #[test]
    fn snapshot_validation_requires_exactly_the_bound_auxiliary_artifacts() {
        let mut extra = local("qwen3-6-35b-a3b");
        extra.auxiliary_artifacts =
            local_with("qwen3-6-35b-a3b", &split_gguf_configuration()).auxiliary_artifacts;
        assert!(matches!(
            validate_snapshot(&extra),
            Err(StrataPlanError::InvalidResolvedState)
        ));

        let mut doubled = local_with("qwen3-8-flash-next", &split_gguf_configuration());
        doubled
            .auxiliary_artifacts
            .push(doubled.auxiliary_artifacts[0].clone());
        assert!(matches!(
            validate_snapshot(&doubled),
            Err(StrataPlanError::InvalidResolvedState)
        ));
    }

    #[test]
    fn fixed_settings_outside_the_wrapper_contract_cannot_launch() {
        for (architecture, mtp_draft_tokens) in
            [("", 2), ("Qwen 3", 2), ("qwen35moe", 0), ("qwen35moe", 9)]
        {
            let mut local = local("qwen3-6-35b-a3b");
            let fixed = fixed_settings(&mut local);
            fixed.architecture = architecture.to_owned();
            fixed.mtp_draft_tokens = mtp_draft_tokens;

            assert!(
                matches!(
                    translate(&local),
                    Err(StrataPlanError::InvalidResolvedState)
                ),
                "{architecture:?} {mtp_draft_tokens}"
            );
        }
    }

    #[test]
    fn model_files_outside_the_wrapper_contract_cannot_launch() {
        let ready = |edit: fn(&mut StrataReadyPack)| {
            let mut pack = StrataReadyPack {
                draft_layer_auxiliary_artifact_id: DRAFT_LAYER_ARTIFACT_ID.to_owned(),
                pack_path: "pack".to_owned(),
                dense_gguf_path: "dense.gguf".to_owned(),
                lookup_table_gguf_path: "lookup-table.gguf".to_owned(),
                token_embedding_gguf_path: "token-embedding.gguf".to_owned(),
            };
            edit(&mut pack);
            Some(ModelFiles::ReadyPack(pack))
        };
        for model_files in [
            None,
            Some(ModelFiles::SplitGguf(StrataSplitGguf::default())),
            ready(|pack| pack.draft_layer_auxiliary_artifact_id.clear()),
            ready(|pack| pack.pack_path.clear()),
            ready(|pack| pack.pack_path = "/pack".to_owned()),
            ready(|pack| pack.dense_gguf_path = "../dense.gguf".to_owned()),
            ready(|pack| pack.lookup_table_gguf_path = "./lookup-table.gguf".to_owned()),
            ready(|pack| pack.token_embedding_gguf_path = "--port=1".to_owned()),
            ready(|pack| pack.pack_path = "pack//tokenizer".to_owned()),
        ] {
            let mut local = local_with("qwen3-8-flash-next", &ready_pack_configuration());
            fixed_settings(&mut local).model_files = model_files.clone();

            assert!(
                matches!(
                    translate(&local),
                    Err(StrataPlanError::InvalidResolvedState)
                ),
                "{model_files:?}"
            );
        }
    }

    #[test]
    fn missing_or_zero_parameters_cannot_launch() {
        for parameter_id in [
            CONTEXT_PER_REQUEST_PARAMETER_ID,
            PARALLEL_REQUESTS_PARAMETER_ID,
            EXPERT_VRAM_MIB_PARAMETER_ID,
        ] {
            let mut zero = local("qwen3-6-35b-a3b");
            zero.engine_parameter_values
                .as_mut()
                .expect("values")
                .values
                .iter_mut()
                .find(|value| value.parameter_id == parameter_id)
                .expect("parameter")
                .value = Some(EngineParameterValue::UnsignedInteger(0));
            let mut missing = local("qwen3-6-35b-a3b");
            missing
                .engine_parameter_values
                .as_mut()
                .expect("values")
                .values
                .retain(|value| value.parameter_id != parameter_id);

            for snapshot in [zero, missing] {
                assert_eq!(
                    translate(&snapshot).expect_err("no value").to_string(),
                    format!(
                        "Strata launch plan has no valid value for engine parameter {parameter_id:?}"
                    )
                );
            }
        }
    }

    #[test]
    fn every_custom_argument_is_refused() {
        for token in [
            "--help",
            "-h",
            "--model=/other.gguf",
            "--po=9090",
            "--future-option",
            "value",
        ] {
            let mut local = local("qwen3-6-35b-a3b");
            local.custom_argument_tokens = vec![token.to_owned()];

            assert_eq!(
                translate(&local).expect_err("custom argument").to_string(),
                format!(
                    "Strata takes no custom arguments, but got {token:?}; its start wrapper accepts only the options Dotcore sets"
                )
            );
        }
    }

    #[test]
    fn snapshot_validation_rejects_non_canonical_plans_and_inconsistent_state() {
        let mut stale = local("qwen3-6-35b-a3b");
        stale.model_id = "renamed".to_owned();
        assert!(matches!(
            validate_snapshot(&stale),
            Err(StrataPlanError::NonCanonicalPlan)
        ));

        let mut wrong_version = local("qwen3-6-35b-a3b");
        wrong_version
            .resolved_engine_state
            .as_mut()
            .expect("state")
            .semantics_version += 1;
        assert!(matches!(
            validate_snapshot(&wrong_version),
            Err(StrataPlanError::InvalidResolvedState)
        ));

        let mut options = local("qwen3-6-35b-a3b");
        options.resolved_options.push(Default::default());
        assert!(matches!(
            validate_snapshot(&options),
            Err(StrataPlanError::InvalidResolvedState)
        ));
    }

    #[test]
    fn snapshot_validation_rejects_retained_plans_of_other_formats() {
        for (format_version, expected) in [
            (
                PLAN_FORMAT_VERSION - 1,
                "Strata deployment was created with launch plan format 0, which this Dotcore \
                 version no longer supports; delete and recreate the deployment",
            ),
            (
                PLAN_FORMAT_VERSION + 1,
                "Strata deployment was created with launch plan format 2 by a newer Dotcore \
                 version; upgrade Dotcore, or delete and recreate the deployment with this version",
            ),
        ] {
            let mut retained = local("qwen3-6-35b-a3b");
            retained
                .desired_launch_plan
                .as_mut()
                .expect("retained plan")
                .format_version = format_version;

            assert_eq!(
                validate_snapshot(&retained)
                    .expect_err("retained plan format")
                    .to_string(),
                expected
            );
        }
    }

    fn local(model_id: &str) -> ResolvedLocalModelDeploymentSnapshot {
        local_with(model_id, &configuration())
    }

    fn local_with(
        model_id: &str,
        configuration: &ModelEngineConfiguration,
    ) -> ResolvedLocalModelDeploymentSnapshot {
        match local_model_snapshot_with(model_id, configuration).source {
            Some(Source::Local(local)) if local.model_id == model_id => local,
            _ => panic!("local Strata snapshot"),
        }
    }

    fn fixed_settings(
        local: &mut ResolvedLocalModelDeploymentSnapshot,
    ) -> &mut StrataFixedModelSettings {
        let Some(State::Strata(state)) = local
            .resolved_engine_state
            .as_mut()
            .and_then(|state| state.state.as_mut())
        else {
            panic!("Strata state");
        };
        state.fixed_settings.as_mut().expect("fixed settings")
    }

    /// Renders the plan with the single-file model binding at its container path and other
    /// bindings as `<binding>`.
    fn argv(plan: &PortableModelLaunchPlan) -> Vec<String> {
        let folder = plan.artifact_bindings.len() > 1;
        plan.arguments
            .iter()
            .map(|argument| match argument.value.as_ref() {
                Some(ArgumentValue::Literal(value)) => value.clone(),
                Some(ArgumentValue::ArtifactBindingId(id)) if id == "model" && !folder => {
                    super::super::workload::CONTAINER_MODEL_PATH.to_owned()
                }
                Some(ArgumentValue::ArtifactBindingId(id)) => format!("<{id}>"),
                other => panic!("unexpected argument {other:?}"),
            })
            .collect()
    }
}
