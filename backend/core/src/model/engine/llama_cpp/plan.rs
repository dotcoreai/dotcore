//! Trusted llama.cpp translation and validation of portable launch plans.

use std::collections::{BTreeMap, BTreeSet};

use dotcore_catalog::{
    options::{
        KV_CACHE_PRECISION_PARAMETER_ID, SPECULATIVE_TOKEN_COUNT_PARAMETER_ID, boolean_value,
        string_value, unsigned_value,
    },
    shared_parameters::{
        CONTEXT_PER_REQUEST_PARAMETER_ID, PARALLEL_REQUESTS_PARAMETER_ID,
        TOTAL_CONTEXT_PARAMETER_ID,
    },
};
use dotcore_proto::{
    dotcore::{
        catalog::v1::{ModelOptionValues, parameter_value::Value as EngineParameterValueKind},
        engine::{
            llama_cpp::v1::{
                LlamaCppFixedModelSettings, LlamaCppResolvedModelState, LlamaCppSpeculativeMethod,
                llama_cpp_option_realization::Realization,
            },
            v1::{EngineKind, resolved_engine_model_state::State},
        },
        model::v1::{
            PortableModelArtifactBinding, PortableModelLaunchArgumentToken,
            PortableModelLaunchPlan, ResolvedLocalModelDeploymentSnapshot,
            portable_model_artifact_binding::Artifact as BoundArtifact,
            portable_model_launch_argument_token::Value as ArgumentValue,
        },
        observation::v1::AcceleratorMemoryArchitecture,
    },
    google::protobuf::Empty,
};

use crate::model::engine::speculative_draft::{self, SpeculativeDraft};

/// Version of the symbolic plan; plans retained in an older format must be recreated.
pub(super) const PLAN_FORMAT_VERSION: u32 = 4;
pub(super) const MODEL_BINDING_ID: &str = "model";
pub(super) const PROJECTOR_BINDING_ID: &str = "vision-projector";
pub(super) const SPECULATIVE_DRAFT_BINDING_ID: &str = "speculative-draft";
/// Name of the HTTP service the plan binds through service tokens.
pub(super) const SERVICE_NAME: &str = "http";
/// Engine parameter bounding the host prompt cache (`--cache-ram`), in MiB; releases without it
/// keep llama.cpp's default bound, which custom arguments may then override.
pub(super) const PROMPT_CACHE_MEMORY_PARAMETER_ID: &str = "prompt-cache-memory-mib";
/// KV cache precisions the adapter launches. Upstream builds compile flash-attention kernels only
/// for matching K and V types and silently fall back to CPU attention otherwise, so one precision
/// sets both caches.
pub(super) const KV_CACHE_PRECISIONS: [&str; 2] = [DEFAULT_KV_CACHE_PRECISION, "q8_0"];
/// llama.cpp's own KV cache precision, launched without cache-type arguments so that plans
/// translated before the option existed stay canonical.
pub(super) const DEFAULT_KV_CACHE_PRECISION: &str = "f16";

/// Produces the single portable desired plan for an accepted llama.cpp selection.
pub(crate) fn translate(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<PortableModelLaunchPlan, LlamaCppPlanError> {
    let state = resolved_state(snapshot)?;
    let fixed = state
        .fixed_settings
        .as_ref()
        .ok_or(LlamaCppPlanError::NoFixedSettings)?;
    if let Some(sampling) = fixed.sampling.as_ref() {
        super::declaration::validate_sampling_settings(sampling)?;
    }
    let total_context = total_context(snapshot)?;
    let parallel_requests = parallel_requests(snapshot)?;
    let kv_cache = kv_cache_precision(snapshot, state, fixed)?;
    let model = snapshot
        .model_artifact
        .as_ref()
        .ok_or(LlamaCppPlanError::NoModelArtifact)?;
    let mut bindings = vec![model_binding(&model.artifact_id)];
    let mut arguments = vec![token(ArgumentValue::EngineExecutable(Empty {}))];
    arguments.push(literal("--model"));
    arguments.push(binding(MODEL_BINDING_ID));
    arguments.extend(literals(["--alias", &snapshot.model_id, "--host"]));
    arguments.push(token(ArgumentValue::ServiceHost(SERVICE_NAME.to_owned())));
    arguments.push(literal("--port"));
    arguments.push(token(ArgumentValue::ServicePort(SERVICE_NAME.to_owned())));
    arguments.extend(literals([
        "--ctx-size",
        &total_context.to_string(),
        "--n-gpu-layers",
        &fixed.gpu_layers.to_string(),
    ]));
    // llama.cpp clamps the batch to the micro-batch for decision models, so the micro-batch is the
    // effective bound on the prompt; both are set to keep the launch explicit.
    if fixed.single_micro_batch {
        let total_context = total_context.to_string();
        arguments.extend(literals([
            "--batch-size",
            &total_context,
            "--ubatch-size",
            &total_context,
        ]));
    }
    if fixed.flash_attention {
        arguments.extend(literals(["--flash-attn", "on"]));
    }
    if fixed.jinja {
        arguments.push(literal("--jinja"));
    }
    if let Some(sampling) = fixed.sampling.as_ref() {
        for (flag, value) in [
            ("--min-p", sampling.min_p),
            ("--repeat-penalty", sampling.repeat_penalty),
            ("--temp", sampling.temperature),
            ("--top-p", sampling.top_p),
        ] {
            if let Some(value) = value {
                arguments.extend(literals([flag, &value.to_string()]));
            }
        }
    }
    // On unified memory, mmap keeps a page-cache copy of the weights in the same pool as the
    // accelerator copy and slows loading. Every supported release reads the load mode, while
    // some no longer accept `--no-mmap`.
    if uses_unified_memory(snapshot)? {
        arguments.extend(literals(["--load-mode", "none"]));
    }
    if let Some(cache) = unsigned_parameter(snapshot, PROMPT_CACHE_MEMORY_PARAMETER_ID) {
        arguments.extend(literals(["--cache-ram", &cache.to_string()]));
    }
    // An explicit slot count alone disables the unified KV cache and splits the total context
    // across slots, so the unified cache llama.cpp's automatic mode uses is requested explicitly.
    arguments.extend(literals([
        "-np",
        &parallel_requests.to_string(),
        "--kv-unified",
    ]));
    // The per-slot cap reserves nothing, and at the total context it caps nothing, so the default
    // launch keeps llama.cpp's own behavior.
    if let Some(cap) = unsigned_parameter(snapshot, CONTEXT_PER_REQUEST_PARAMETER_ID)
        .filter(|cap| *cap < total_context)
    {
        arguments.extend(literals(["--kv-unified-per-slot", &cap.to_string()]));
    }
    if let Some(precision) = kv_cache.filter(|precision| *precision != DEFAULT_KV_CACHE_PRECISION) {
        arguments.extend(literals([
            "--cache-type-k",
            precision,
            "--cache-type-v",
            precision,
        ]));
    }
    if let Some(projector) = fixed.vision_projector.as_ref() {
        ensure_auxiliary(snapshot, &projector.auxiliary_artifact_id)?;
        bindings.push(auxiliary_binding(
            PROJECTOR_BINDING_ID,
            &projector.auxiliary_artifact_id,
        ));
        arguments.extend(literals(["--mmproj"]));
        arguments.push(binding(PROJECTOR_BINDING_ID));
        let tokens = snapshot
            .input_capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.image_input_capacity.as_ref())
            .ok_or(LlamaCppPlanError::ImageInputUnsupported)?
            .fixed_visual_tokens
            .to_string();
        arguments.extend(literals([
            "--image-min-tokens",
            &tokens,
            "--image-max-tokens",
            &tokens,
        ]));
    }
    if let Some(speculative) = fixed.speculative.as_ref() {
        let speculative_type =
            speculative_type(speculative.method).ok_or(LlamaCppPlanError::InvalidResolvedState)?;
        arguments.extend(literals(["--spec-type", speculative_type]));
        match speculative_draft::llama_cpp(speculative.draft.as_ref()) {
            // The engine loads a head embedded in the main model file without a draft argument.
            Some(SpeculativeDraft::Embedded) => {}
            Some(SpeculativeDraft::Artifact(artifact_id)) => {
                ensure_auxiliary(snapshot, artifact_id)?;
                bindings.push(auxiliary_binding(SPECULATIVE_DRAFT_BINDING_ID, artifact_id));
                arguments.extend(literals(["--model-draft"]));
                arguments.push(binding(SPECULATIVE_DRAFT_BINDING_ID));
            }
            None => return Err(LlamaCppPlanError::InvalidResolvedState),
        }
        let maximum = speculative.draft_maximum.to_string();
        arguments.extend(literals(["--spec-draft-n-max", &maximum]));
    }
    validate_custom_argument_tokens(snapshot, kv_cache.is_some(), fixed)?;
    arguments.extend(literals_from(&snapshot.custom_argument_tokens));
    Ok(PortableModelLaunchPlan {
        arguments,
        format_version: PLAN_FORMAT_VERSION,
        artifact_bindings: bindings,
    })
}

/// Reports whether custom arguments keep the KV cache in host memory instead of on the accelerator.
pub(crate) fn enables_host_kv_offload(snapshot: &ResolvedLocalModelDeploymentSnapshot) -> bool {
    snapshot
        .custom_argument_tokens
        .iter()
        .any(|token| matches!(token.as_str(), "-nkvo" | "--no-kv-offload"))
}

/// Validates that a retained plan is the canonical plan for its versioned resolved state.
pub(crate) fn validate_snapshot(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<(), LlamaCppPlanError> {
    if let Some(plan) = snapshot.desired_launch_plan.as_ref() {
        let found = plan.format_version;
        if found < PLAN_FORMAT_VERSION {
            return Err(LlamaCppPlanError::RetainedPlanFormat { found });
        }
        if found > PLAN_FORMAT_VERSION {
            return Err(LlamaCppPlanError::NewerRetainedPlanFormat { found });
        }
    }
    validate_resolved_consistency(snapshot)?;
    let expected = translate(snapshot)?;
    if snapshot.desired_launch_plan.as_ref() != Some(&expected) {
        return Err(LlamaCppPlanError::NonCanonicalPlan);
    }
    Ok(())
}

fn resolved_state(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<&LlamaCppResolvedModelState, LlamaCppPlanError> {
    snapshot
        .resolved_engine_state
        .as_ref()
        .filter(|state| {
            state.engine_kind == EngineKind::LlamaCpp as i32
                && state.semantics_version == super::realization::SEMANTICS_VERSION
        })
        .and_then(|state| state.state.as_ref())
        .and_then(|state| match state {
            State::LlamaCpp(state) => Some(state),
            State::Vllm(_) | State::Ninfer(_) | State::Strata(_) => None,
        })
        .ok_or(LlamaCppPlanError::InvalidResolvedState)
}

fn validate_resolved_consistency(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<(), LlamaCppPlanError> {
    let state = resolved_state(snapshot)?;
    let fixed = state
        .fixed_settings
        .as_ref()
        .ok_or(LlamaCppPlanError::NoFixedSettings)?;
    let values = snapshot
        .resolved_options
        .iter()
        .filter_map(|option| option.values.as_ref())
        .map(|values| (values.option_id.as_str(), values))
        .collect::<BTreeMap<_, _>>();
    if values.len() != snapshot.resolved_options.len()
        || values.len() != state.option_realizations.len()
    {
        return Err(LlamaCppPlanError::InvalidResolvedState);
    }
    for realization in &state.option_realizations {
        let option = values
            .get(realization.option_id.as_str())
            .ok_or(LlamaCppPlanError::InvalidResolvedState)?;
        let valid = match realization.realization.as_ref() {
            Some(Realization::Vision(vision)) => {
                let enabled = boolean_value(option, &vision.enabled_parameter_id)
                    .ok_or(LlamaCppPlanError::InvalidResolvedState)?;
                enabled
                    == fixed.vision_projector.as_ref().is_some_and(|projector| {
                        projector.auxiliary_artifact_id == vision.auxiliary_artifact_id
                    })
            }
            Some(Realization::KvCachePrecision(_)) => {
                supported_kv_cache_precision(option, fixed)?;
                true
            }
            Some(Realization::Mtp(mtp)) => {
                let count = unsigned_value(option, SPECULATIVE_TOKEN_COUNT_PARAMETER_ID)
                    .and_then(|value| u32::try_from(value).ok())
                    .ok_or(LlamaCppPlanError::InvalidResolvedState)?;
                (count > 0)
                    == fixed.speculative.as_ref().is_some_and(|settings| {
                        settings.method == LlamaCppSpeculativeMethod::Mtp as i32
                            && settings.draft == mtp.draft
                            && settings.draft_maximum == count
                    })
            }
            None => false,
        };
        if !valid {
            return Err(LlamaCppPlanError::InvalidResolvedState);
        }
    }
    validate_selected_auxiliaries(snapshot, fixed)?;
    let image_capable = snapshot
        .input_capabilities
        .as_ref()
        .and_then(|capabilities| capabilities.image_input_capacity.as_ref())
        .is_some();
    if image_capable != fixed.vision_projector.is_some() {
        return Err(LlamaCppPlanError::InvalidResolvedState);
    }
    Ok(())
}

fn validate_selected_auxiliaries(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
    fixed: &LlamaCppFixedModelSettings,
) -> Result<(), LlamaCppPlanError> {
    let selected = snapshot
        .auxiliary_artifacts
        .iter()
        .map(|artifact| artifact.auxiliary_artifact_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut expected = BTreeSet::new();
    if let Some(projector) = fixed.vision_projector.as_ref() {
        expected.insert(projector.auxiliary_artifact_id.as_str());
    }
    if let Some(artifact_id) = fixed
        .speculative
        .as_ref()
        .and_then(|speculative| speculative_draft::llama_cpp(speculative.draft.as_ref()))
        .and_then(SpeculativeDraft::artifact_id)
    {
        expected.insert(artifact_id);
    }
    if selected != expected {
        return Err(LlamaCppPlanError::InvalidResolvedState);
    }
    Ok(())
}

/// Reads the KV cache precision a KV cache precision option resolved to, or none when the
/// configuration declares no such option and the cache keeps llama.cpp's default.
fn kv_cache_precision<'a>(
    snapshot: &'a ResolvedLocalModelDeploymentSnapshot,
    state: &LlamaCppResolvedModelState,
    fixed: &LlamaCppFixedModelSettings,
) -> Result<Option<&'a str>, LlamaCppPlanError> {
    let Some(realization) = state.option_realizations.iter().find(|realization| {
        matches!(
            realization.realization,
            Some(Realization::KvCachePrecision(_))
        )
    }) else {
        return Ok(None);
    };
    let values = snapshot
        .resolved_options
        .iter()
        .filter_map(|option| option.values.as_ref())
        .find(|values| values.option_id == realization.option_id)
        .ok_or(LlamaCppPlanError::InvalidResolvedState)?;
    supported_kv_cache_precision(values, fixed).map(Some)
}

/// Reads a resolved KV cache precision the adapter launches with the fixed settings: one of the
/// supported precisions, quantized only with flash attention.
fn supported_kv_cache_precision<'a>(
    values: &'a ModelOptionValues,
    fixed: &LlamaCppFixedModelSettings,
) -> Result<&'a str, LlamaCppPlanError> {
    string_value(values, KV_CACHE_PRECISION_PARAMETER_ID)
        .filter(|precision| {
            KV_CACHE_PRECISIONS.contains(precision)
                && (*precision == DEFAULT_KV_CACHE_PRECISION || fixed.flash_attention)
        })
        .ok_or(LlamaCppPlanError::InvalidResolvedState)
}

/// Maps a speculative method to its llama.cpp speculative type, or none when it is unsupported.
pub(super) fn speculative_type(method: i32) -> Option<&'static str> {
    match LlamaCppSpeculativeMethod::try_from(method).ok()? {
        LlamaCppSpeculativeMethod::Mtp => Some("draft-mtp"),
        LlamaCppSpeculativeMethod::Unspecified => None,
    }
}

/// Rejects custom tokens that override what the plan owns. The prompt cache bound and the KV cache
/// types are owned only when the release declares the bound and the configuration declares a KV
/// cache precision option, so other deployments may still set them through custom arguments.
fn validate_custom_argument_tokens(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
    owns_kv_cache_precision: bool,
    fixed: &LlamaCppFixedModelSettings,
) -> Result<(), LlamaCppPlanError> {
    let owns_prompt_cache = declares_parameter(snapshot, PROMPT_CACHE_MEMORY_PARAMETER_ID);
    for token in &snapshot.custom_argument_tokens {
        if conflicts_with_resolved_settings(token)
            || (owns_prompt_cache && conflicts_with_prompt_cache_memory(token))
            || (owns_kv_cache_precision && conflicts_with_kv_cache_precision(token))
            || (fixed.single_micro_batch && conflicts_with_batch_sizes(token))
        {
            return Err(LlamaCppPlanError::ConflictingCustomArgument(token.clone()));
        }
    }
    Ok(())
}

fn total_context(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<u64, LlamaCppPlanError> {
    unsigned_parameter(snapshot, TOTAL_CONTEXT_PARAMETER_ID)
        .filter(|value| *value > 0)
        .ok_or(LlamaCppPlanError::TotalContextNotConfigured)
}

fn parallel_requests(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<u64, LlamaCppPlanError> {
    unsigned_parameter(snapshot, PARALLEL_REQUESTS_PARAMETER_ID)
        .filter(|value| *value > 0)
        .ok_or(LlamaCppPlanError::ParallelRequestsNotConfigured)
}

fn unsigned_parameter(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
    parameter_id: &str,
) -> Option<u64> {
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
}

fn uses_unified_memory(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<bool, LlamaCppPlanError> {
    match snapshot
        .selected_target
        .as_ref()
        .map(|target| target.accelerator_memory_architecture())
    {
        Some(AcceleratorMemoryArchitecture::Dedicated) => Ok(false),
        Some(AcceleratorMemoryArchitecture::Unified) => Ok(true),
        Some(AcceleratorMemoryArchitecture::Unspecified) | None => {
            Err(LlamaCppPlanError::NoMemoryArchitecture)
        }
    }
}

fn conflicts_with_resolved_settings(token: &str) -> bool {
    let flag = token.split_once('=').map_or(token, |(flag, _)| flag);
    matches!(
        flag,
        "-c" | "--ctx-size"
            | "--ctx_size"
            | "-ngl"
            | "--gpu-layers"
            | "--n-gpu-layers"
            | "-fa"
            | "--flash-attn"
            | "--mmproj"
            | "--mmproj-url"
            | "--image-min-tokens"
            | "--image-max-tokens"
            | "-md"
            | "--model-draft"
            | "--mmap"
            | "--no-mmap"
            | "-lm"
            | "--load-mode"
            | "-np"
            | "--parallel"
            | "-kvu"
            | "--kv-unified"
            | "-no-kvu"
            | "--no-kv-unified"
            | "--kv-unified-per-slot"
    ) || flag.starts_with("--spec-")
        || flag.starts_with("--draft-")
        // A glued context size is digits only, so other `-c`-prefixed flags such as `-cram` pass.
        || token
            .strip_prefix("-c")
            .is_some_and(|value| value.starts_with(|c: char| c.is_ascii_digit()))
        || ["-ngl", "-fa"].iter().any(|prefix| {
            token
                .strip_prefix(prefix)
                .is_some_and(|value| !value.is_empty() && !value.trim_start_matches('=').is_empty())
        })
}

/// Reports whether the release's parameter schema declares, and so owns, the given parameter.
fn declares_parameter(snapshot: &ResolvedLocalModelDeploymentSnapshot, parameter_id: &str) -> bool {
    snapshot
        .engine_parameter_schema
        .as_ref()
        .is_some_and(|schema| {
            schema
                .parameters
                .iter()
                .any(|parameter| parameter.parameter_id == parameter_id)
        })
}

fn conflicts_with_prompt_cache_memory(token: &str) -> bool {
    let flag = token.split_once('=').map_or(token, |(flag, _)| flag);
    matches!(flag, "-cram" | "--cache-ram")
}

fn conflicts_with_kv_cache_precision(token: &str) -> bool {
    let flag = token.split_once('=').map_or(token, |(flag, _)| flag);
    matches!(flag, "-ctk" | "--cache-type-k" | "-ctv" | "--cache-type-v")
}

/// Reports whether a custom argument sets the batch or micro-batch size, which a single micro-batch
/// launch owns. Glued short forms such as `-b512` are matched like the context size.
fn conflicts_with_batch_sizes(token: &str) -> bool {
    let flag = token.split_once('=').map_or(token, |(flag, _)| flag);
    matches!(flag, "-b" | "--batch-size" | "-ub" | "--ubatch-size")
        || ["-b", "-ub"].iter().any(|prefix| {
            token
                .strip_prefix(prefix)
                .is_some_and(|value| value.starts_with(|c: char| c.is_ascii_digit()))
        })
}

fn ensure_auxiliary(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
    id: &str,
) -> Result<(), LlamaCppPlanError> {
    snapshot
        .auxiliary_artifacts
        .iter()
        .any(|artifact| artifact.auxiliary_artifact_id == id)
        .then_some(())
        .ok_or(LlamaCppPlanError::AuxiliaryArtifactNotResolved)
}

fn model_binding(artifact_id: &str) -> PortableModelArtifactBinding {
    PortableModelArtifactBinding {
        binding_id: MODEL_BINDING_ID.to_owned(),
        artifact: Some(BoundArtifact::ModelArtifactId(artifact_id.to_owned())),
        read_only: true,
    }
}

fn auxiliary_binding(binding_id: &str, artifact_id: &str) -> PortableModelArtifactBinding {
    PortableModelArtifactBinding {
        binding_id: binding_id.to_owned(),
        artifact: Some(BoundArtifact::AuxiliaryArtifactId(artifact_id.to_owned())),
        read_only: true,
    }
}

fn literals<const N: usize>(values: [&str; N]) -> Vec<PortableModelLaunchArgumentToken> {
    values.into_iter().map(literal).collect()
}

fn literals_from(values: &[String]) -> Vec<PortableModelLaunchArgumentToken> {
    values.iter().map(|value| literal(value)).collect()
}

fn literal(value: &str) -> PortableModelLaunchArgumentToken {
    token(ArgumentValue::Literal(value.to_owned()))
}

fn binding(binding_id: &str) -> PortableModelLaunchArgumentToken {
    token(ArgumentValue::ArtifactBindingId(binding_id.to_owned()))
}

fn token(value: ArgumentValue) -> PortableModelLaunchArgumentToken {
    PortableModelLaunchArgumentToken { value: Some(value) }
}

/// Indicates that resolved values cannot produce a complete portable llama.cpp plan.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LlamaCppPlanError {
    #[error("llama.cpp launch plan has invalid resolved engine state")]
    InvalidResolvedState,

    #[error("llama.cpp launch plan is not canonical for its resolved state")]
    NonCanonicalPlan,

    #[error(
        "llama.cpp deployment was created with launch plan format {found}, which this Dotcore \
         version no longer supports; delete and recreate the deployment"
    )]
    RetainedPlanFormat { found: u32 },

    #[error(
        "llama.cpp deployment was created with launch plan format {found} by a newer Dotcore \
         version; upgrade Dotcore, or delete and recreate the deployment with this version"
    )]
    NewerRetainedPlanFormat { found: u32 },

    #[error("llama.cpp launch plan has no fixed settings")]
    NoFixedSettings,

    #[error("llama.cpp launch plan has invalid sampling settings")]
    InvalidSampling(#[from] super::declaration::LlamaCppSamplingError),

    #[error("llama.cpp launch plan has no valid total context")]
    TotalContextNotConfigured,

    #[error("llama.cpp launch plan has no valid parallel requests")]
    ParallelRequestsNotConfigured,

    #[error("llama.cpp launch plan has no selected target memory architecture")]
    NoMemoryArchitecture,

    #[error("llama.cpp launch plan has no model artifact")]
    NoModelArtifact,

    #[error("llama.cpp launch plan has a missing auxiliary artifact")]
    AuxiliaryArtifactNotResolved,

    #[error("llama.cpp launch plan has vision settings without image capability")]
    ImageInputUnsupported,

    #[error("llama.cpp custom argument {0:?} conflicts with resolved settings")]
    ConflictingCustomArgument(String),
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::catalog::v1::{
        ParameterDefinition, ParameterValue as EngineParameterValueMessage, ParameterVisibility,
    };

    use super::*;

    #[test]
    fn fixed_template_and_sampling_settings_produce_exact_arguments() {
        let mut snapshot = crate::test_support::local_model_snapshot("fixture-model");
        let local = match snapshot.source.as_mut().expect("snapshot source") {
            dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(local) => {
                local
            }
            _ => panic!("local snapshot"),
        };
        let state = match local
            .resolved_engine_state
            .as_mut()
            .expect("engine state")
            .state
            .as_mut()
            .expect("llama.cpp state")
        {
            State::LlamaCpp(state) => state,
            _ => panic!("llama.cpp state"),
        };
        let fixed = state.fixed_settings.as_mut().expect("fixed settings");
        fixed.jinja = true;
        fixed.sampling = Some(
            dotcore_proto::dotcore::engine::llama_cpp::v1::LlamaCppSamplingSettings {
                min_p: Some(0.01),
                repeat_penalty: Some(1.0),
                temperature: Some(0.7),
                top_p: Some(0.95),
            },
        );

        let plan = translate(local).expect("portable plan");

        assert_eq!(
            plan.arguments
                .iter()
                .map(|argument| argument.value.clone().expect("argument value"))
                .collect::<Vec<_>>(),
            [
                ArgumentValue::EngineExecutable(Empty {}),
                ArgumentValue::Literal("--model".to_owned()),
                ArgumentValue::ArtifactBindingId("model".to_owned()),
                ArgumentValue::Literal("--alias".to_owned()),
                ArgumentValue::Literal("fixture-model".to_owned()),
                ArgumentValue::Literal("--host".to_owned()),
                ArgumentValue::ServiceHost("http".to_owned()),
                ArgumentValue::Literal("--port".to_owned()),
                ArgumentValue::ServicePort("http".to_owned()),
                ArgumentValue::Literal("--ctx-size".to_owned()),
                ArgumentValue::Literal("32768".to_owned()),
                ArgumentValue::Literal("--n-gpu-layers".to_owned()),
                ArgumentValue::Literal("0".to_owned()),
                ArgumentValue::Literal("--jinja".to_owned()),
                ArgumentValue::Literal("--min-p".to_owned()),
                ArgumentValue::Literal("0.01".to_owned()),
                ArgumentValue::Literal("--repeat-penalty".to_owned()),
                ArgumentValue::Literal("1".to_owned()),
                ArgumentValue::Literal("--temp".to_owned()),
                ArgumentValue::Literal("0.7".to_owned()),
                ArgumentValue::Literal("--top-p".to_owned()),
                ArgumentValue::Literal("0.95".to_owned()),
                ArgumentValue::Literal("--cache-ram".to_owned()),
                ArgumentValue::Literal("0".to_owned()),
                ArgumentValue::Literal("-np".to_owned()),
                ArgumentValue::Literal("4".to_owned()),
                ArgumentValue::Literal("--kv-unified".to_owned()),
            ]
        );
    }

    #[test]
    fn empty_sampling_settings_emit_no_arguments() {
        let mut snapshot = crate::test_support::local_model_snapshot("fixture-model");
        let local = match snapshot.source.as_mut().expect("snapshot source") {
            dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(local) => {
                local
            }
            _ => panic!("local snapshot"),
        };
        let before = translate(local).expect("portable plan without sampling");
        let state = match local
            .resolved_engine_state
            .as_mut()
            .expect("engine state")
            .state
            .as_mut()
            .expect("llama.cpp state")
        {
            State::LlamaCpp(state) => state,
            _ => panic!("llama.cpp state"),
        };
        state
            .fixed_settings
            .as_mut()
            .expect("fixed settings")
            .sampling = Some(Default::default());

        assert_eq!(
            translate(local).expect("portable plan with empty sampling"),
            before
        );
    }

    #[test]
    fn custom_tokens_preserve_exact_argv_semantics() {
        let mut snapshot = crate::test_support::local_model_snapshot("fixture-model");
        let local = match snapshot.source.as_mut().expect("snapshot source") {
            dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(local) => {
                local
            }
            _ => panic!("local snapshot"),
        };
        local.custom_argument_tokens = vec![
            "--port".to_owned(),
            "9000".to_owned(),
            "--port".to_owned(),
            "8081".to_owned(),
            "  spaced value  ".to_owned(),
            "$(touch /tmp/not-executed); --shell-shaped".to_owned(),
        ];

        let plan = translate(local).expect("portable plan");

        assert_eq!(
            plan.arguments[18..]
                .iter()
                .map(|argument| argument.value.clone().expect("argument value"))
                .collect::<Vec<_>>(),
            [
                ArgumentValue::Literal("--port".to_owned()),
                ArgumentValue::Literal("9000".to_owned()),
                ArgumentValue::Literal("--port".to_owned()),
                ArgumentValue::Literal("8081".to_owned()),
                ArgumentValue::Literal("  spaced value  ".to_owned()),
                ArgumentValue::Literal("$(touch /tmp/not-executed); --shell-shaped".to_owned()),
            ]
        );
    }

    #[test]
    fn translator_rejects_custom_tokens_that_override_snapshot_owned_settings() {
        for token in [
            "-c",
            "-c=4096",
            "-c4096",
            "-ngl",
            "-ngl=0",
            "-ngl0",
            "--gpu-layers",
            "--gpu-layers=0",
            "--n-gpu-layers",
            "--n-gpu-layers=0",
            "-fa",
            "-fa=off",
            "-faoff",
            "--flash-attn",
            "--flash-attn=off",
            "--ctx-size",
            "--ctx-size=4096",
            "--ctx_size=4096",
            "--mmproj",
            "--mmproj=other.gguf",
            "--image-min-tokens",
            "--image-max-tokens=4096",
            "-md",
            "--model-draft",
            "--spec-type=draft-simple",
            "--spec-draft-n-max",
            "--mmap",
            "--no-mmap",
            "-lm",
            "--load-mode=mmap",
        ] {
            let mut snapshot = crate::test_support::local_model_snapshot("fixture-model");
            let local = match snapshot.source.as_mut().expect("snapshot source") {
                dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(
                    local,
                ) => local,
                _ => panic!("local snapshot"),
            };
            local.custom_argument_tokens = vec![token.to_owned()];

            assert_eq!(
                translate(local)
                    .expect_err("snapshot-owned custom argument")
                    .to_string(),
                format!("llama.cpp custom argument {token:?} conflicts with resolved settings")
            );
        }
    }

    #[test]
    fn unified_memory_translation_disables_mmap_once() {
        let mut snapshot = crate::test_support::local_model_snapshot("fixture-model");
        let local = match snapshot.source.as_mut().expect("snapshot source") {
            dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(local) => {
                local
            }
            _ => panic!("local snapshot"),
        };
        local
            .selected_target
            .as_mut()
            .expect("selected target")
            .accelerator_memory_architecture = AcceleratorMemoryArchitecture::Unified as i32;

        let plan = translate(local).expect("portable plan");

        assert_eq!(
            plan.arguments
                .iter()
                .map(|argument| argument.value.clone().expect("argument value"))
                .collect::<Vec<_>>(),
            [
                ArgumentValue::EngineExecutable(Empty {}),
                ArgumentValue::Literal("--model".to_owned()),
                ArgumentValue::ArtifactBindingId("model".to_owned()),
                ArgumentValue::Literal("--alias".to_owned()),
                ArgumentValue::Literal("fixture-model".to_owned()),
                ArgumentValue::Literal("--host".to_owned()),
                ArgumentValue::ServiceHost("http".to_owned()),
                ArgumentValue::Literal("--port".to_owned()),
                ArgumentValue::ServicePort("http".to_owned()),
                ArgumentValue::Literal("--ctx-size".to_owned()),
                ArgumentValue::Literal("32768".to_owned()),
                ArgumentValue::Literal("--n-gpu-layers".to_owned()),
                ArgumentValue::Literal("0".to_owned()),
                ArgumentValue::Literal("--load-mode".to_owned()),
                ArgumentValue::Literal("none".to_owned()),
                ArgumentValue::Literal("--cache-ram".to_owned()),
                ArgumentValue::Literal("0".to_owned()),
                ArgumentValue::Literal("-np".to_owned()),
                ArgumentValue::Literal("4".to_owned()),
                ArgumentValue::Literal("--kv-unified".to_owned()),
            ]
        );

        local.selected_target = None;
        assert_eq!(
            translate(local)
                .expect_err("missing memory architecture")
                .to_string(),
            "llama.cpp launch plan has no selected target memory architecture"
        );
    }

    #[test]
    fn prompt_cache_memory_bounds_the_host_prompt_cache() {
        let mut snapshot = crate::test_support::local_model_snapshot("fixture-model");
        let local = match snapshot.source.as_mut().expect("snapshot source") {
            dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(local) => {
                local
            }
            _ => panic!("local snapshot"),
        };
        without_prompt_cache(local);
        let cache_arguments = |local: &ResolvedLocalModelDeploymentSnapshot| {
            let plan = translate(local).expect("portable plan");
            plan.arguments
                .iter()
                .map(|argument| argument.value.clone().expect("argument value"))
                .skip_while(|value| *value != ArgumentValue::Literal("--cache-ram".to_owned()))
                .take(2)
                .collect::<Vec<_>>()
        };
        assert_eq!(cache_arguments(local), []);

        for mib in [2048, 0] {
            let values = local
                .engine_parameter_values
                .as_mut()
                .expect("engine parameter values");
            values
                .values
                .retain(|value| value.parameter_id != PROMPT_CACHE_MEMORY_PARAMETER_ID);
            values.values.push(EngineParameterValueMessage {
                parameter_id: PROMPT_CACHE_MEMORY_PARAMETER_ID.to_owned(),
                value: Some(EngineParameterValueKind::UnsignedInteger(mib)),
            });

            assert_eq!(
                cache_arguments(local),
                [
                    ArgumentValue::Literal("--cache-ram".to_owned()),
                    ArgumentValue::Literal(mib.to_string()),
                ]
            );
        }
    }

    #[test]
    fn custom_prompt_cache_arguments_are_reserved_only_when_the_schema_owns_them() {
        let mut snapshot = crate::test_support::local_model_snapshot("fixture-model");
        let local = match snapshot.source.as_mut().expect("snapshot source") {
            dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(local) => {
                local
            }
            _ => panic!("local snapshot"),
        };
        without_prompt_cache(local);
        let tokens = ["-cram", "--cache-ram", "--cache-ram=-1"];

        for token in tokens {
            local.custom_argument_tokens = vec![token.to_owned()];
            let plan = translate(local).expect("a release without the bound accepts it");
            assert_eq!(
                plan.arguments
                    .last()
                    .and_then(|argument| argument.value.clone()),
                Some(ArgumentValue::Literal(token.to_owned()))
            );
        }

        local
            .engine_parameter_schema
            .as_mut()
            .expect("engine parameter schema")
            .parameters
            .push(ParameterDefinition {
                parameter_id: PROMPT_CACHE_MEMORY_PARAMETER_ID.to_owned(),
                display_name: "Prompt cache memory".to_owned(),
                parameter_type: None,
                visibility: ParameterVisibility::Advanced as i32,
                description: String::new(),
            });
        for token in tokens {
            local.custom_argument_tokens = vec![token.to_owned()];
            assert_eq!(
                translate(local)
                    .expect_err("schema-owned prompt cache bound")
                    .to_string(),
                format!("llama.cpp custom argument {token:?} conflicts with resolved settings")
            );
        }
    }

    #[test]
    fn parallel_requests_set_the_slot_count_with_a_unified_kv_cache() {
        let mut snapshot = crate::test_support::local_model_snapshot("fixture-model");
        let local = match snapshot.source.as_mut().expect("snapshot source") {
            dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(local) => {
                local
            }
            _ => panic!("local snapshot"),
        };
        without_prompt_cache(local);

        for requests in [4, 1, 8, 64] {
            set_unsigned(local, PARALLEL_REQUESTS_PARAMETER_ID, requests);

            assert_eq!(
                argument_values(&translate(local).expect("portable plan")),
                leading_arguments()
                    .into_iter()
                    .chain([
                        ArgumentValue::Literal("-np".to_owned()),
                        ArgumentValue::Literal(requests.to_string()),
                        ArgumentValue::Literal("--kv-unified".to_owned()),
                    ])
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn translation_requires_parallel_requests_like_the_total_context() {
        let mut snapshot = crate::test_support::local_model_snapshot("fixture-model");
        let local = match snapshot.source.as_mut().expect("snapshot source") {
            dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(local) => {
                local
            }
            _ => panic!("local snapshot"),
        };

        set_unsigned(local, PARALLEL_REQUESTS_PARAMETER_ID, 0);
        assert_eq!(
            translate(local)
                .expect_err("zero parallel requests")
                .to_string(),
            "llama.cpp launch plan has no valid parallel requests"
        );

        local
            .engine_parameter_values
            .as_mut()
            .expect("engine parameter values")
            .values
            .retain(|value| value.parameter_id != PARALLEL_REQUESTS_PARAMETER_ID);
        assert_eq!(
            translate(local)
                .expect_err("missing parallel requests")
                .to_string(),
            "llama.cpp launch plan has no valid parallel requests"
        );

        set_unsigned(local, PARALLEL_REQUESTS_PARAMETER_ID, 4);
        set_unsigned(local, TOTAL_CONTEXT_PARAMETER_ID, 0);
        assert_eq!(
            translate(local)
                .expect_err("zero total context")
                .to_string(),
            "llama.cpp launch plan has no valid total context"
        );
    }

    #[test]
    fn engine_defaults_launch_one_shared_total_context_without_a_per_request_cap() {
        let snapshot = crate::test_support::local_model_snapshot("fixture-model");
        let Some(dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(
            local,
        )) = snapshot.source
        else {
            panic!("local snapshot");
        };
        assert_eq!(
            unsigned_parameter(&local, CONTEXT_PER_REQUEST_PARAMETER_ID),
            Some(32_768)
        );

        assert_eq!(
            argument_values(&translate(&local).expect("portable plan")),
            [
                ArgumentValue::EngineExecutable(Empty {}),
                ArgumentValue::Literal("--model".to_owned()),
                ArgumentValue::ArtifactBindingId("model".to_owned()),
                ArgumentValue::Literal("--alias".to_owned()),
                ArgumentValue::Literal("fixture-model".to_owned()),
                ArgumentValue::Literal("--host".to_owned()),
                ArgumentValue::ServiceHost("http".to_owned()),
                ArgumentValue::Literal("--port".to_owned()),
                ArgumentValue::ServicePort("http".to_owned()),
                ArgumentValue::Literal("--ctx-size".to_owned()),
                ArgumentValue::Literal("32768".to_owned()),
                ArgumentValue::Literal("--n-gpu-layers".to_owned()),
                ArgumentValue::Literal("0".to_owned()),
                ArgumentValue::Literal("--cache-ram".to_owned()),
                ArgumentValue::Literal("0".to_owned()),
                ArgumentValue::Literal("-np".to_owned()),
                ArgumentValue::Literal("4".to_owned()),
                ArgumentValue::Literal("--kv-unified".to_owned()),
            ]
        );
    }

    #[test]
    fn single_micro_batch_sets_batch_and_micro_batch_to_the_total_context() {
        let mut local = single_micro_batch_snapshot();
        set_unsigned(&mut local, TOTAL_CONTEXT_PARAMETER_ID, 16_384);
        set_unsigned(&mut local, CONTEXT_PER_REQUEST_PARAMETER_ID, 16_384);

        let mut expected = leading_arguments();
        // The value following `--ctx-size`.
        expected[10] = ArgumentValue::Literal("16384".to_owned());
        expected.extend([
            ArgumentValue::Literal("--batch-size".to_owned()),
            ArgumentValue::Literal("16384".to_owned()),
            ArgumentValue::Literal("--ubatch-size".to_owned()),
            ArgumentValue::Literal("16384".to_owned()),
            ArgumentValue::Literal("--cache-ram".to_owned()),
            ArgumentValue::Literal("0".to_owned()),
            ArgumentValue::Literal("-np".to_owned()),
            ArgumentValue::Literal("4".to_owned()),
            ArgumentValue::Literal("--kv-unified".to_owned()),
        ]);
        assert_eq!(
            argument_values(&translate(&local).expect("portable plan")),
            expected
        );
    }

    #[test]
    fn custom_batch_size_arguments_are_reserved_only_with_a_single_micro_batch() {
        for token in [
            "-b",
            "-b=512",
            "-b512",
            "--batch-size=512",
            "-ub",
            "-ub512",
            "--ubatch-size",
        ] {
            let mut local = single_micro_batch_snapshot();
            local.custom_argument_tokens = vec![token.to_owned()];
            assert_eq!(
                translate(&local)
                    .expect_err("single micro-batch custom argument")
                    .to_string(),
                format!("llama.cpp custom argument {token:?} conflicts with resolved settings")
            );

            let mut snapshot = crate::test_support::local_model_snapshot("fixture-model");
            let local = match snapshot.source.as_mut().expect("snapshot source") {
                dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(
                    local,
                ) => local,
                _ => panic!("local snapshot"),
            };
            local.custom_argument_tokens = vec![token.to_owned()];
            assert_eq!(
                argument_values(&translate(local).expect("portable plan")).last(),
                Some(&ArgumentValue::Literal(token.to_owned()))
            );
        }
    }

    #[test]
    fn a_context_per_request_below_the_total_context_caps_each_slot() {
        let mut snapshot = crate::test_support::local_model_snapshot("fixture-model");
        let local = match snapshot.source.as_mut().expect("snapshot source") {
            dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(local) => {
                local
            }
            _ => panic!("local snapshot"),
        };

        for (total_context, cap) in [(32_768, 8192), (1_048_576, 262_144)] {
            set_unsigned(local, TOTAL_CONTEXT_PARAMETER_ID, total_context);
            set_unsigned(local, CONTEXT_PER_REQUEST_PARAMETER_ID, cap);
            let arguments = argument_values(&translate(local).expect("portable plan"));

            assert_eq!(
                arguments[9..11],
                [
                    ArgumentValue::Literal("--ctx-size".to_owned()),
                    ArgumentValue::Literal(total_context.to_string()),
                ]
            );
            assert_eq!(
                arguments[15..],
                [
                    ArgumentValue::Literal("-np".to_owned()),
                    ArgumentValue::Literal("4".to_owned()),
                    ArgumentValue::Literal("--kv-unified".to_owned()),
                    ArgumentValue::Literal("--kv-unified-per-slot".to_owned()),
                    ArgumentValue::Literal(cap.to_string()),
                ]
            );
        }
    }

    #[test]
    fn custom_slot_and_unified_kv_arguments_are_always_reserved() {
        let mut snapshot = crate::test_support::local_model_snapshot("fixture-model");
        let local = match snapshot.source.as_mut().expect("snapshot source") {
            dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(local) => {
                local
            }
            _ => panic!("local snapshot"),
        };

        for token in [
            "-np",
            "-np=2",
            "--parallel",
            "--parallel=8",
            "-kvu",
            "--kv-unified",
            "-no-kvu",
            "--no-kv-unified",
            "--kv-unified-per-slot",
            "--kv-unified-per-slot=8192",
        ] {
            local.custom_argument_tokens = vec![token.to_owned()];
            assert_eq!(
                translate(local)
                    .expect_err("shared-parameter flag")
                    .to_string(),
                format!("llama.cpp custom argument {token:?} conflicts with resolved settings")
            );
        }
    }

    #[test]
    fn translation_produces_the_exact_symbolic_portable_plan() {
        let snapshot = crate::test_support::local_model_snapshot("fixture-model");
        let mut local = match snapshot.source.expect("snapshot source") {
            dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(local) => {
                local
            }
            _ => panic!("local snapshot"),
        };
        without_prompt_cache(&mut local);

        let plan = translate(&local).expect("portable plan");

        assert_eq!(plan.format_version, 4);
        assert_eq!(
            argument_values(&plan),
            leading_arguments()
                .into_iter()
                .chain([
                    ArgumentValue::Literal("-np".to_owned()),
                    ArgumentValue::Literal("4".to_owned()),
                    ArgumentValue::Literal("--kv-unified".to_owned()),
                ])
                .collect::<Vec<_>>()
        );
        assert_eq!(
            plan.artifact_bindings,
            [PortableModelArtifactBinding {
                binding_id: "model".to_owned(),
                artifact: Some(BoundArtifact::ModelArtifactId(
                    "fixture-artifact".to_owned(),
                )),
                read_only: true,
            }]
        );
        assert!(plan.arguments.iter().all(|argument| {
            !matches!(
                argument.value.as_ref(),
                Some(ArgumentValue::Literal(value)) if value.starts_with('/') || value == "8080"
            )
        }));
    }

    #[test]
    fn mtp_token_count_selects_the_draft_artifact_and_its_arguments() {
        let off = mtp_snapshot(Some("mtp-draft-file"), 0);
        assert!(off.auxiliary_artifacts.is_empty());
        let off_plan = translate(&off).expect("MTP off plan");
        assert_eq!(argument_values(&off_plan), base_arguments());
        assert_eq!(off_plan.artifact_bindings, [model_binding()]);

        let on = mtp_snapshot(Some("mtp-draft-file"), 3);
        assert_eq!(
            on.auxiliary_artifacts
                .iter()
                .map(|artifact| artifact.auxiliary_artifact_id.as_str())
                .collect::<Vec<_>>(),
            ["mtp-draft-file"]
        );
        let on_plan = translate(&on).expect("MTP on plan");
        assert_eq!(
            argument_values(&on_plan),
            base_arguments()
                .into_iter()
                .chain([
                    ArgumentValue::Literal("--spec-type".to_owned()),
                    ArgumentValue::Literal("draft-mtp".to_owned()),
                    ArgumentValue::Literal("--model-draft".to_owned()),
                    ArgumentValue::ArtifactBindingId("speculative-draft".to_owned()),
                    ArgumentValue::Literal("--spec-draft-n-max".to_owned()),
                    ArgumentValue::Literal("3".to_owned()),
                ])
                .collect::<Vec<_>>()
        );
        assert_eq!(
            on_plan.artifact_bindings,
            [
                model_binding(),
                PortableModelArtifactBinding {
                    binding_id: "speculative-draft".to_owned(),
                    artifact: Some(BoundArtifact::AuxiliaryArtifactId(
                        "mtp-draft-file".to_owned(),
                    )),
                    read_only: true,
                },
            ]
        );

        for (mut snapshot, plan) in [(off, off_plan), (on, on_plan)] {
            snapshot.desired_launch_plan = Some(plan);
            validate_snapshot(&snapshot).expect("canonical MTP plan");
        }
    }

    #[test]
    fn embedded_mtp_head_emits_no_draft_model_argument() {
        let off = mtp_snapshot(None, 0);
        assert!(off.auxiliary_artifacts.is_empty());
        let off_plan = translate(&off).expect("embedded MTP off plan");
        assert_eq!(argument_values(&off_plan), base_arguments());
        assert_eq!(off_plan.artifact_bindings, [model_binding()]);

        let on = mtp_snapshot(None, 2);
        assert!(on.auxiliary_artifacts.is_empty());
        let on_plan = translate(&on).expect("embedded MTP on plan");
        assert_eq!(
            argument_values(&on_plan),
            base_arguments()
                .into_iter()
                .chain([
                    ArgumentValue::Literal("--spec-type".to_owned()),
                    ArgumentValue::Literal("draft-mtp".to_owned()),
                    ArgumentValue::Literal("--spec-draft-n-max".to_owned()),
                    ArgumentValue::Literal("2".to_owned()),
                ])
                .collect::<Vec<_>>()
        );
        assert_eq!(on_plan.artifact_bindings, [model_binding()]);

        for (mut snapshot, plan) in [(off, off_plan), (on, on_plan)] {
            snapshot.desired_launch_plan = Some(plan);
            validate_snapshot(&snapshot).expect("canonical embedded MTP plan");
        }
    }

    #[test]
    fn mtp_draft_artifact_at_engine_defaults_produces_the_exact_arguments() {
        let snapshot = mtp_snapshot(Some("mtp-draft"), 5);

        assert_eq!(
            argument_values(&translate(&snapshot).expect("MTP plan")),
            [
                ArgumentValue::EngineExecutable(Empty {}),
                ArgumentValue::Literal("--model".to_owned()),
                ArgumentValue::ArtifactBindingId("model".to_owned()),
                ArgumentValue::Literal("--alias".to_owned()),
                ArgumentValue::Literal("fixture-model".to_owned()),
                ArgumentValue::Literal("--host".to_owned()),
                ArgumentValue::ServiceHost("http".to_owned()),
                ArgumentValue::Literal("--port".to_owned()),
                ArgumentValue::ServicePort("http".to_owned()),
                ArgumentValue::Literal("--ctx-size".to_owned()),
                ArgumentValue::Literal("32768".to_owned()),
                ArgumentValue::Literal("--n-gpu-layers".to_owned()),
                ArgumentValue::Literal("0".to_owned()),
                ArgumentValue::Literal("--cache-ram".to_owned()),
                ArgumentValue::Literal("0".to_owned()),
                ArgumentValue::Literal("-np".to_owned()),
                ArgumentValue::Literal("4".to_owned()),
                ArgumentValue::Literal("--kv-unified".to_owned()),
                ArgumentValue::Literal("--spec-type".to_owned()),
                ArgumentValue::Literal("draft-mtp".to_owned()),
                ArgumentValue::Literal("--model-draft".to_owned()),
                ArgumentValue::ArtifactBindingId("speculative-draft".to_owned()),
                ArgumentValue::Literal("--spec-draft-n-max".to_owned()),
                ArgumentValue::Literal("5".to_owned()),
            ]
        );
    }

    #[test]
    fn kv_cache_precision_sets_both_cache_types_except_at_f16() {
        let flash_attention_arguments = || {
            leading_arguments()
                .into_iter()
                .chain([
                    ArgumentValue::Literal("--flash-attn".to_owned()),
                    ArgumentValue::Literal("on".to_owned()),
                    ArgumentValue::Literal("--cache-ram".to_owned()),
                    ArgumentValue::Literal("0".to_owned()),
                    ArgumentValue::Literal("-np".to_owned()),
                    ArgumentValue::Literal("4".to_owned()),
                    ArgumentValue::Literal("--kv-unified".to_owned()),
                ])
                .collect::<Vec<_>>()
        };
        let f16 = kv_cache_snapshot("f16", true);
        let f16_plan = translate(&f16).expect("f16 plan");
        assert_eq!(argument_values(&f16_plan), flash_attention_arguments());

        let q8 = kv_cache_snapshot("q8_0", true);
        let q8_plan = translate(&q8).expect("q8_0 plan");
        assert_eq!(
            argument_values(&q8_plan),
            flash_attention_arguments()
                .into_iter()
                .chain([
                    ArgumentValue::Literal("--cache-type-k".to_owned()),
                    ArgumentValue::Literal("q8_0".to_owned()),
                    ArgumentValue::Literal("--cache-type-v".to_owned()),
                    ArgumentValue::Literal("q8_0".to_owned()),
                ])
                .collect::<Vec<_>>()
        );
        assert_eq!(q8_plan.format_version, 4);

        for (mut snapshot, plan) in [(f16, f16_plan), (q8, q8_plan)] {
            snapshot.desired_launch_plan = Some(plan);
            validate_snapshot(&snapshot).expect("canonical KV cache precision plan");
        }
    }

    #[test]
    fn kv_cache_precision_must_be_supported_and_quantized_only_with_flash_attention() {
        for (precision, flash_attention) in [("q8_0", false), ("q4_0", true), ("", true)] {
            let snapshot = kv_cache_snapshot(precision, flash_attention);
            assert_eq!(
                translate(&snapshot)
                    .expect_err("unsupported KV cache precision")
                    .to_string(),
                "llama.cpp launch plan has invalid resolved engine state",
                "{precision:?} with flash attention {flash_attention}"
            );
        }
    }

    #[test]
    fn custom_cache_type_arguments_are_reserved_only_when_the_option_is_declared() {
        let tokens = [
            "-ctk",
            "-ctk=q4_0",
            "--cache-type-k",
            "--cache-type-k=q4_0",
            "-ctv",
            "-ctv=q4_0",
            "--cache-type-v",
            "--cache-type-v=q4_0",
        ];

        // A deployment created before the option existed keeps its cache-type arguments.
        let mut snapshot = crate::test_support::local_model_snapshot("fixture-model");
        let local = match snapshot.source.as_mut().expect("snapshot source") {
            dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(local) => {
                local
            }
            _ => panic!("local snapshot"),
        };
        for token in tokens {
            local.custom_argument_tokens = vec![token.to_owned(), "q8_0".to_owned()];
            local.desired_launch_plan = Some(translate(local).expect("undeclared option"));
            validate_snapshot(local).expect("retained plan with custom cache-type arguments");
            assert_eq!(
                argument_values(local.desired_launch_plan.as_ref().expect("plan"))
                    .split_last_chunk::<2>()
                    .map(|(_, last)| last.clone()),
                Some([
                    ArgumentValue::Literal(token.to_owned()),
                    ArgumentValue::Literal("q8_0".to_owned()),
                ])
            );
        }

        for precision in ["f16", "q8_0"] {
            let mut snapshot = kv_cache_snapshot(precision, true);
            for token in tokens {
                snapshot.custom_argument_tokens = vec![token.to_owned()];
                assert_eq!(
                    translate(&snapshot)
                        .expect_err("option-owned cache type")
                        .to_string(),
                    format!("llama.cpp custom argument {token:?} conflicts with resolved settings")
                );
            }
        }
    }

    #[test]
    fn retained_plans_in_an_older_format_require_recreating_the_deployment() {
        let mut snapshot = crate::test_support::local_model_snapshot("fixture-model");
        let local = match snapshot.source.as_mut().expect("snapshot source") {
            dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(local) => {
                local
            }
            _ => panic!("local snapshot"),
        };
        assert!(validate_snapshot(local).is_ok());

        local
            .desired_launch_plan
            .as_mut()
            .expect("retained plan")
            .format_version = PLAN_FORMAT_VERSION - 1;

        assert_eq!(
            validate_snapshot(local)
                .expect_err("old retained plan")
                .to_string(),
            "llama.cpp deployment was created with launch plan format 3, which this Dotcore \
             version no longer supports; delete and recreate the deployment"
        );

        local
            .desired_launch_plan
            .as_mut()
            .expect("retained plan")
            .format_version = PLAN_FORMAT_VERSION + 1;

        assert_eq!(
            validate_snapshot(local)
                .expect_err("newer retained plan")
                .to_string(),
            "llama.cpp deployment was created with launch plan format 5 by a newer Dotcore \
             version; upgrade Dotcore, or delete and recreate the deployment with this version"
        );
    }

    /// Builds the fixture snapshot with an MTP option realized at the given token count, using the
    /// draft auxiliary artifact or, without one, an embedded head.
    fn mtp_snapshot(
        draft_artifact_id: Option<&str>,
        count: u64,
    ) -> ResolvedLocalModelDeploymentSnapshot {
        use dotcore_proto::dotcore::{
            catalog::v1::{
                Model, ModelArtifactBinding, ModelAuxiliaryArtifact, ModelEngineConfiguration,
                ModelInputCapabilities, ModelInputModality, ModelOptionParameterValue,
                ModelOptionValues, model_option_parameter_value::Value,
            },
            engine::{
                llama_cpp::v1::{
                    LlamaCppEmbeddedDraft, LlamaCppModelConfiguration,
                    LlamaCppMtpOptionRealization, LlamaCppOptionRealization,
                    LlamaCppSpeculativeDraftSource,
                    llama_cpp_speculative_draft_source::Source as DraftSource,
                },
                v1::{EngineModelConfiguration, engine_model_configuration::Configuration},
            },
            model::v1::{ResolvedModelOption, model_deployment_snapshot::Source},
        };

        let configuration = ModelEngineConfiguration {
            engine_configuration: Some(EngineModelConfiguration {
                engine_kind: EngineKind::LlamaCpp as i32,
                configuration: Some(Configuration::LlamaCpp(LlamaCppModelConfiguration {
                    fixed_settings: Some(LlamaCppFixedModelSettings::default()),
                    option_realizations: vec![LlamaCppOptionRealization {
                        option_id: "mtp".to_owned(),
                        realization: Some(Realization::Mtp(LlamaCppMtpOptionRealization {
                            draft: Some(LlamaCppSpeculativeDraftSource {
                                source: Some(draft_artifact_id.map_or(
                                    DraftSource::Embedded(LlamaCppEmbeddedDraft {}),
                                    |id| DraftSource::AuxiliaryArtifactId(id.to_owned()),
                                )),
                            }),
                        })),
                    }],
                })),
            }),
            input_capabilities: Some(ModelInputCapabilities {
                supported_modalities: vec![ModelInputModality::Text as i32],
                image_input_capacity: None,
            }),
            ..Default::default()
        };
        let values = ModelOptionValues {
            option_id: "mtp".to_owned(),
            values: vec![ModelOptionParameterValue {
                parameter_id: SPECULATIVE_TOKEN_COUNT_PARAMETER_ID.to_owned(),
                value: Some(Value::UnsignedInteger(count)),
            }],
        };
        let selection = super::super::realization::realize_model(
            &Model::default(),
            &configuration,
            &ModelArtifactBinding::default(),
            std::slice::from_ref(&values),
        )
        .expect("MTP realization");

        let snapshot = crate::test_support::local_model_snapshot("fixture-model");
        let Some(Source::Local(mut local)) = snapshot.source else {
            panic!("local snapshot");
        };
        local.resolved_engine_state = Some(selection.state);
        local.auxiliary_artifacts = selection
            .auxiliary_artifact_ids
            .into_iter()
            .map(|auxiliary_artifact_id| ModelAuxiliaryArtifact {
                auxiliary_artifact_id,
                ..Default::default()
            })
            .collect();
        local.input_capabilities = Some(selection.input_capabilities);
        local.resolved_options = vec![ResolvedModelOption {
            values: Some(values),
        }];
        local
    }

    /// Builds the fixture snapshot with a KV cache precision option realized at the given
    /// precision, with or without flash attention.
    fn kv_cache_snapshot(
        precision: &str,
        flash_attention: bool,
    ) -> ResolvedLocalModelDeploymentSnapshot {
        use dotcore_proto::dotcore::{
            catalog::v1::{
                Model, ModelArtifactBinding, ModelEngineConfiguration, ModelInputCapabilities,
                ModelInputModality, ModelOptionParameterValue, model_option_parameter_value::Value,
            },
            engine::{
                llama_cpp::v1::{
                    LlamaCppKvCachePrecisionOptionRealization, LlamaCppModelConfiguration,
                    LlamaCppOptionRealization,
                },
                v1::{EngineModelConfiguration, engine_model_configuration::Configuration},
            },
            model::v1::{ResolvedModelOption, model_deployment_snapshot::Source},
        };

        let configuration = ModelEngineConfiguration {
            engine_configuration: Some(EngineModelConfiguration {
                engine_kind: EngineKind::LlamaCpp as i32,
                configuration: Some(Configuration::LlamaCpp(LlamaCppModelConfiguration {
                    fixed_settings: Some(LlamaCppFixedModelSettings {
                        flash_attention,
                        ..Default::default()
                    }),
                    option_realizations: vec![LlamaCppOptionRealization {
                        option_id: "kv-cache".to_owned(),
                        realization: Some(Realization::KvCachePrecision(
                            LlamaCppKvCachePrecisionOptionRealization {},
                        )),
                    }],
                })),
            }),
            input_capabilities: Some(ModelInputCapabilities {
                supported_modalities: vec![ModelInputModality::Text as i32],
                image_input_capacity: None,
            }),
            ..Default::default()
        };
        let values = ModelOptionValues {
            option_id: "kv-cache".to_owned(),
            values: vec![ModelOptionParameterValue {
                parameter_id: KV_CACHE_PRECISION_PARAMETER_ID.to_owned(),
                value: Some(Value::String(precision.to_owned())),
            }],
        };
        let selection = super::super::realization::realize_model(
            &Model::default(),
            &configuration,
            &ModelArtifactBinding::default(),
            std::slice::from_ref(&values),
        )
        .expect("KV cache precision realization");

        let snapshot = crate::test_support::local_model_snapshot("fixture-model");
        let Some(Source::Local(mut local)) = snapshot.source else {
            panic!("local snapshot");
        };
        local.resolved_engine_state = Some(selection.state);
        local.input_capabilities = Some(selection.input_capabilities);
        local.resolved_options = vec![ResolvedModelOption {
            values: Some(values),
        }];
        local
    }

    /// Builds the fixture snapshot with the single micro-batch fixed setting on.
    fn single_micro_batch_snapshot() -> ResolvedLocalModelDeploymentSnapshot {
        let snapshot = crate::test_support::local_model_snapshot("fixture-model");
        let Some(dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(
            mut local,
        )) = snapshot.source
        else {
            panic!("local snapshot");
        };
        let Some(State::LlamaCpp(state)) = local
            .resolved_engine_state
            .as_mut()
            .and_then(|state| state.state.as_mut())
        else {
            panic!("llama.cpp state");
        };
        state
            .fixed_settings
            .as_mut()
            .expect("fixed settings")
            .single_micro_batch = true;
        local
    }

    /// Arguments of the fixture snapshot, whose release owns the prompt cache and parallel
    /// requests, at the engine defaults.
    fn base_arguments() -> Vec<ArgumentValue> {
        leading_arguments()
            .into_iter()
            .chain([
                ArgumentValue::Literal("--cache-ram".to_owned()),
                ArgumentValue::Literal("0".to_owned()),
                ArgumentValue::Literal("-np".to_owned()),
                ArgumentValue::Literal("4".to_owned()),
                ArgumentValue::Literal("--kv-unified".to_owned()),
            ])
            .collect()
    }

    /// Arguments of the fixture snapshot that precede the prompt cache and parallel requests.
    fn leading_arguments() -> Vec<ArgumentValue> {
        vec![
            ArgumentValue::EngineExecutable(Empty {}),
            ArgumentValue::Literal("--model".to_owned()),
            ArgumentValue::ArtifactBindingId("model".to_owned()),
            ArgumentValue::Literal("--alias".to_owned()),
            ArgumentValue::Literal("fixture-model".to_owned()),
            ArgumentValue::Literal("--host".to_owned()),
            ArgumentValue::ServiceHost("http".to_owned()),
            ArgumentValue::Literal("--port".to_owned()),
            ArgumentValue::ServicePort("http".to_owned()),
            ArgumentValue::Literal("--ctx-size".to_owned()),
            ArgumentValue::Literal("32768".to_owned()),
            ArgumentValue::Literal("--n-gpu-layers".to_owned()),
            ArgumentValue::Literal("0".to_owned()),
        ]
    }

    /// Turns the fixture snapshot into one of a release without a prompt cache bound.
    fn without_prompt_cache(local: &mut ResolvedLocalModelDeploymentSnapshot) {
        local
            .engine_parameter_schema
            .as_mut()
            .expect("engine parameter schema")
            .parameters
            .retain(|parameter| parameter.parameter_id != PROMPT_CACHE_MEMORY_PARAMETER_ID);
        local
            .engine_parameter_values
            .as_mut()
            .expect("engine parameter values")
            .values
            .retain(|value| value.parameter_id != PROMPT_CACHE_MEMORY_PARAMETER_ID);
    }

    /// Replaces one unsigned engine parameter value of the snapshot.
    fn set_unsigned(
        local: &mut ResolvedLocalModelDeploymentSnapshot,
        parameter_id: &str,
        value: u64,
    ) {
        let values = local
            .engine_parameter_values
            .as_mut()
            .expect("engine parameter values");
        values
            .values
            .retain(|value| value.parameter_id != parameter_id);
        values.values.push(EngineParameterValueMessage {
            parameter_id: parameter_id.to_owned(),
            value: Some(EngineParameterValueKind::UnsignedInteger(value)),
        });
    }

    fn model_binding() -> PortableModelArtifactBinding {
        PortableModelArtifactBinding {
            binding_id: "model".to_owned(),
            artifact: Some(BoundArtifact::ModelArtifactId(
                "fixture-artifact".to_owned(),
            )),
            read_only: true,
        }
    }

    fn argument_values(plan: &PortableModelLaunchPlan) -> Vec<ArgumentValue> {
        plan.arguments
            .iter()
            .map(|argument| argument.value.clone().expect("argument value"))
            .collect()
    }
}
