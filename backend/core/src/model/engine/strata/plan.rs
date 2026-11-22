//! Translation and validation of the portable Strata launch plan for a resolved deployment.
//!
//! From a resolved deployment snapshot it builds the canonical command line of Dotcore's Strata
//! start wrapper, run under the image's init: the model's files mounted read-only, the deployment's
//! model identity as the served name, the context per request and the parallel requests. For the
//! main engine it adds the draft layer and a ready-made pack's file roles when the model has them,
//! the configuration's architecture and MTP draft depth, the expert memory budgets, and whether the
//! pack path runs on unified memory; for the Inkling engine it names the model artifact's single
//! pack file instead. The
//! wrapper takes no other arguments, so custom arguments are refused. It also checks that a
//! retained plan exactly matches the plan its resolved state would produce.

use dotcore_catalog::shared_parameters::{
    CONTEXT_PER_REQUEST_PARAMETER_ID, PARALLEL_REQUESTS_PARAMETER_ID,
};
use dotcore_proto::dotcore::{
    catalog::v1::parameter_value::Value as EngineParameterValueKind,
    engine::{
        strata::v1::{
            StrataMainEngine, StrataResolvedModelState, strata_fixed_model_settings::EnginePath,
            strata_main_engine::ModelFiles,
        },
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

use super::{SERVICE_PORT, draft_layer_artifact_id, inkling_pack_path, valid_fixed_settings};

pub(super) const PLAN_FORMAT_VERSION: u32 = 3;
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
    match fixed.engine_path.as_ref() {
        Some(EnginePath::MainEngine(main)) => {
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
            arguments.extend(main_engine_arguments(
                snapshot,
                main,
                draft_layer.is_some(),
            )?);
        }
        Some(EnginePath::InklingPack(_)) => {
            let pack_path = inkling_pack_path(
                model
                    .source
                    .as_ref()
                    .and_then(|source| source.content.as_ref()),
            )
            .ok_or(StrataPlanError::NoInklingPack)?;
            arguments.extend(inkling_pack_arguments(snapshot, pack_path)?);
        }
        None => return Err(StrataPlanError::InvalidResolvedState),
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

/// The main engine's arguments after its model files: a ready-made pack's file roles, the served
/// name, the architecture and MTP draft depth, the context per request and parallel requests, the
/// expert memory budgets, and the unified memory setting, which only the pack path has.
fn main_engine_arguments(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
    main: &StrataMainEngine,
    pack_path: bool,
) -> Result<Vec<PortableModelLaunchArgumentToken>, StrataPlanError> {
    let expert_vram_mib = positive_parameter(snapshot, &main.expert_vram_mib_parameter_id)?;
    let (context_per_request, parallel_requests) = context_and_parallel_requests(snapshot)?;
    let mut arguments = Vec::new();
    if let Some(ModelFiles::ReadyPack(pack)) = main.model_files.as_ref() {
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
        &main.architecture,
        "--mtp",
        &main.mtp_draft_tokens.to_string(),
        "--context",
        &context_per_request.to_string(),
        "--parallel",
        &parallel_requests.to_string(),
        "--expert-vram-mib",
        &expert_vram_mib.to_string(),
    ]));
    if !main.expert_host_memory_mib_parameter_id.is_empty() {
        let expert_host_memory_mib =
            unsigned_parameter(snapshot, &main.expert_host_memory_mib_parameter_id)?;
        arguments.extend(literals([
            "--expert-host-memory-mib",
            &expert_host_memory_mib.to_string(),
        ]));
    }
    if pack_path && uses_unified_memory(snapshot)? {
        arguments.push(literal("--unified-memory"));
    }
    Ok(arguments)
}

/// The Inkling engine's arguments after its model files: the pack file within the model artifact,
/// the served name, the context per request and the parallel requests.
fn inkling_pack_arguments(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
    pack_path: &str,
) -> Result<Vec<PortableModelLaunchArgumentToken>, StrataPlanError> {
    let (context_per_request, parallel_requests) = context_and_parallel_requests(snapshot)?;
    Ok(literals([
        "--inkling-pack",
        pack_path,
        "--model-name",
        &snapshot.model_id,
        "--context",
        &context_per_request.to_string(),
        "--parallel",
        &parallel_requests.to_string(),
    ]))
}

fn context_and_parallel_requests(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<(u64, u64), StrataPlanError> {
    Ok((
        positive_parameter(snapshot, CONTEXT_PER_REQUEST_PARAMETER_ID)?,
        positive_parameter(snapshot, PARALLEL_REQUESTS_PARAMETER_ID)?,
    ))
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
    unsigned_parameter(snapshot, parameter_id).and_then(|value| {
        (value > 0)
            .then_some(value)
            .ok_or_else(|| StrataPlanError::ParameterNotConfigured(parameter_id.to_owned()))
    })
}

fn unsigned_parameter(
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
            Some(EngineParameterValueKind::UnsignedInteger(value)) => Some(*value),
            _ => None,
        })
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

    #[error("Strata launch plan's model artifact holds no single Inkling pack file")]
    NoInklingPack,

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
        catalog::v1::{
            HuggingFaceSnapshotFile, ModelEngineConfiguration,
            hugging_face_artifact_source::Content as HuggingFaceContent,
        },
        engine::{
            strata::v1::{StrataFixedModelSettings, StrataReadyPack, StrataSplitGguf},
            v1::engine_model_configuration::Configuration,
        },
        model::v1::model_deployment_snapshot::Source,
    };

    use super::*;
    use crate::test_support::strata::{
        DRAFT_LAYER_ARTIFACT_ID, EXPERT_HOST_MEMORY_MIB_PARAMETER_ID, EXPERT_VRAM_MIB_PARAMETER_ID,
        configuration, inkling_pack_configuration, local_model_snapshot_with, parameter_values,
        ready_pack_configuration, refresh_launch_plan, split_gguf_configuration,
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
        assert_eq!(plan.format_version, 3);
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
    fn a_zero_expert_host_memory_reaches_the_wrapper() {
        let mut local = local_with("qwen3-8-flash-next", &split_gguf_configuration());
        local.engine_parameter_values = Some(parameter_values([
            (TOTAL_CONTEXT_PARAMETER_ID, 32_768),
            (CONTEXT_PER_REQUEST_PARAMETER_ID, 32_768),
            (PARALLEL_REQUESTS_PARAMETER_ID, 1),
            (EXPERT_VRAM_MIB_PARAMETER_ID, 8_192),
            (EXPERT_HOST_MEMORY_MIB_PARAMETER_ID, 0),
        ]));
        refresh_launch_plan(&mut local);

        let argv = argv(local.desired_launch_plan.as_ref().expect("plan"));

        assert_eq!(
            argv[argv.len() - 6..],
            [
                "--expert-vram-mib",
                "8192",
                "--expert-host-memory-mib",
                "0",
                "--port",
                "8080"
            ]
        );
        validate_snapshot(&local).expect("canonical plan with no expert host memory");
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
                "--expert-host-memory-mib",
                "4096",
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
        // Unified memory configurations exclude the expert host memory.
        let mut configuration = ready_pack_configuration();
        configuration.excluded_engine_parameter_ids =
            vec![EXPERT_HOST_MEMORY_MIB_PARAMETER_ID.to_owned()];
        if let Some(Configuration::Strata(payload)) = configuration
            .engine_configuration
            .as_mut()
            .and_then(|wrapper| wrapper.configuration.as_mut())
            && let Some(EnginePath::MainEngine(main)) = payload
                .fixed_settings
                .as_mut()
                .and_then(|fixed| fixed.engine_path.as_mut())
        {
            main.expert_host_memory_mib_parameter_id.clear();
        }
        let mut local = local_with("qwen3-8-flash-next", &configuration);
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
    fn inkling_pack_launches_the_inkling_path_with_its_pack_file() {
        let mut local = local_with("inkling-2-mini", &inkling_pack_configuration());
        local.engine_parameter_values = Some(parameter_values([
            (TOTAL_CONTEXT_PARAMETER_ID, 65_536),
            (CONTEXT_PER_REQUEST_PARAMETER_ID, 32_768),
            (PARALLEL_REQUESTS_PARAMETER_ID, 2),
        ]));
        // The Inkling engine finds its own memory settings, whatever the accelerator shares.
        local
            .selected_target
            .as_mut()
            .expect("selected target")
            .accelerator_memory_architecture = AcceleratorMemoryArchitecture::Unified as i32;
        refresh_launch_plan(&mut local);

        let plan = local.desired_launch_plan.as_ref().expect("plan");

        assert_eq!(
            argv(plan),
            [
                "/usr/bin/tini",
                "--",
                "/usr/local/bin/dotcore-strata-start",
                "--model",
                "<model>",
                "--inkling-pack",
                "SQ-10.strata",
                "--model-name",
                "inkling-2-mini",
                "--context",
                "32768",
                "--parallel",
                "2",
                "--port",
                "8080",
            ]
        );
        assert_eq!(
            plan.artifact_bindings,
            [PortableModelArtifactBinding {
                binding_id: "model".to_owned(),
                artifact: Some(BoundArtifact::ModelArtifactId(
                    "fixture-inkling-pack".to_owned()
                )),
                read_only: true,
            }]
        );
        validate_snapshot(&local).expect("canonical Inkling pack plan");
    }

    #[test]
    fn an_inkling_artifact_without_a_single_plain_pack_file_cannot_launch() {
        for files in [
            vec!["tokenizer.json", "chat_template.jinja"],
            vec![
                "SQ-10.strata",
                "SQ-15.strata",
                "tokenizer.json",
                "chat_template.jinja",
            ],
            vec![".strata", "tokenizer.json", "chat_template.jinja"],
            vec!["--port=1.strata", "tokenizer.json", "chat_template.jinja"],
        ] {
            let mut local = local_with("inkling-2-mini", &inkling_pack_configuration());
            let Some(HuggingFaceContent::Snapshot(snapshot)) = local
                .model_artifact
                .as_mut()
                .and_then(|artifact| artifact.source.as_mut())
                .and_then(|source| source.content.as_mut())
            else {
                panic!("snapshot artifact");
            };
            snapshot.files = files
                .iter()
                .map(|path| HuggingFaceSnapshotFile {
                    path: (*path).to_owned(),
                    provenance_digest: "0".repeat(64),
                })
                .collect();

            assert_eq!(
                translate(&local).expect_err("no single pack").to_string(),
                "Strata launch plan's model artifact holds no single Inkling pack file",
                "{files:?}"
            );
        }
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
            let main = main_engine(&mut local);
            main.architecture = architecture.to_owned();
            main.mtp_draft_tokens = mtp_draft_tokens;

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
            main_engine(&mut local).model_files = model_files.clone();

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
                .value = Some(EngineParameterValueKind::UnsignedInteger(0));
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
                "Strata deployment was created with launch plan format 2, which this Dotcore \
                 version no longer supports; delete and recreate the deployment",
            ),
            (
                PLAN_FORMAT_VERSION + 1,
                "Strata deployment was created with launch plan format 4 by a newer Dotcore \
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

    fn main_engine(local: &mut ResolvedLocalModelDeploymentSnapshot) -> &mut StrataMainEngine {
        match fixed_settings(local).engine_path.as_mut() {
            Some(EnginePath::MainEngine(main)) => main,
            _ => panic!("main engine settings"),
        }
    }

    /// Renders the plan with the single-file model binding at its container path and other
    /// bindings as `<binding>`. Only a single GGUF binds one file: a plan with a draft layer or an
    /// Inkling pack binds a folder.
    fn argv(plan: &PortableModelLaunchPlan) -> Vec<String> {
        let folder =
            plan.artifact_bindings.len() > 1 || plan.arguments.contains(&literal("--inkling-pack"));
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
