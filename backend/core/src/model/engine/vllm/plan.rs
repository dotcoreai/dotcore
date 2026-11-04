//! Translation and validation of the portable vLLM launch plan for a resolved deployment.
//!
//! From a resolved deployment snapshot it builds the canonical `vllm serve` command line from the fixed
//! settings and option realizations, resolves each engine parameter value, binds the model, chat-template
//! and MTP draft artifacts, and rejects custom arguments that overlap the flags the adapter owns. It also checks that a
//! retained plan exactly matches the plan its resolved state would produce.

use std::collections::{BTreeMap, BTreeSet};

use dotcore_catalog::{
    options::{SPECULATIVE_TOKEN_COUNT_PARAMETER_ID, boolean_value, unsigned_value},
    shared_parameters::{CONTEXT_PER_REQUEST_PARAMETER_ID, PARALLEL_REQUESTS_PARAMETER_ID},
};
use dotcore_proto::dotcore::{
    catalog::v1::engine_parameter_value::Value as EngineParameterValue,
    engine::{
        v1::{EngineKind, resolved_engine_model_state::State},
        vllm::v1::{
            VllmKvCacheDataType, VllmKvOffloadingBackend, VllmResolvedModelState,
            vllm_option_realization::Realization,
        },
    },
    model::v1::{
        PortableModelArtifactBinding, PortableModelLaunchArgumentToken, PortableModelLaunchPlan,
        ResolvedLocalModelDeploymentSnapshot,
        portable_model_artifact_binding::Artifact as BoundArtifact,
        portable_model_launch_argument_token::Value as ArgumentValue,
    },
};

use crate::model::engine::speculative_draft::{self, SpeculativeDraft};

pub(super) const PLAN_FORMAT_VERSION: u32 = 2;
pub(super) const MODEL_BINDING_ID: &str = "model-snapshot";
pub(super) const TEMPLATE_BINDING_ID: &str = "chat-template";
pub(super) const SPECULATIVE_DRAFT_BINDING_ID: &str = "speculative-draft";
/// Memory utilization is rendered in ten-thousandths, the precision vLLM's startup check needs.
const UTILIZATION_SCALE: u128 = 10_000;
const ADAPTER_OWNED_FLAGS: [&str; 26] = [
    "--host",
    "--port",
    "--served-model-name",
    "--model",
    "--chat-template",
    "--reasoning-parser",
    "--tool-call-parser",
    "--tool-strict-level",
    "--max-model-len",
    "--max-num-seqs",
    "--max-num-batched-tokens",
    "--max-cudagraph-capture-size",
    "--kv-cache-dtype",
    "--kv-cache-memory-bytes",
    "--gpu-memory-utilization",
    "--kv-offloading-size",
    "--kv-offloading-backend",
    "--speculative-config",
    "--language-model-only",
    "--trust-remote-code",
    "--enable-log-requests",
    "--enable-auto-tool-choice",
    "--enable-chunked-prefill",
    "--async-scheduling",
    "--enable-prefix-caching",
    "--enable-prompt-tokens-details",
];
const ADAPTER_OWNED_NEGATABLE_FLAGS: [&str; 8] = [
    "--language-model-only",
    "--trust-remote-code",
    "--enable-log-requests",
    "--enable-auto-tool-choice",
    "--enable-chunked-prefill",
    "--async-scheduling",
    "--enable-prefix-caching",
    "--enable-prompt-tokens-details",
];
const ADAPTER_OWNED_OVERRIDE_FLAGS: [&str; 6] = [
    "--config",
    "--compilation-config",
    "--cudagraph-capture-sizes",
    "--spec-method",
    "--spec-tokens",
    "--spec-model",
];
const ADAPTER_OWNED_SHORT_ALIASES: [&str; 2] = ["-sc", "-cc"];

/// Produces the single portable desired plan for an accepted vLLM selection.
pub(crate) fn translate(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<PortableModelLaunchPlan, VllmPlanError> {
    let state = resolved_state(snapshot)?;
    let fixed = state
        .fixed_settings
        .as_ref()
        .ok_or(VllmPlanError::NoFixedSettings)?;
    let model = snapshot
        .model_artifact
        .as_ref()
        .ok_or(VllmPlanError::NoModelArtifact)?;
    ensure_auxiliary(snapshot, &fixed.chat_template_auxiliary_artifact_id)?;

    let mut arguments = literals(["vllm", "serve"]);
    arguments.push(binding(MODEL_BINDING_ID));
    arguments.extend(literals([
        "--host",
        "0.0.0.0",
        "--port",
        &fixed.container_port.to_string(),
    ]));

    if fixed.enable_auto_tool_choice {
        arguments.push(literal("--enable-auto-tool-choice"));
    }
    arguments.push(boolean_argument(
        "--enable-chunked-prefill",
        fixed.enable_chunked_prefill,
    ));
    if fixed.enable_async_scheduling {
        arguments.push(literal("--async-scheduling"));
    }
    arguments.push(boolean_argument(
        "--enable-prefix-caching",
        fixed.enable_prefix_caching,
    ));
    if fixed.enable_prompt_token_details {
        arguments.push(literal("--enable-prompt-tokens-details"));
    }

    let mut bindings = vec![
        model_binding(&model.artifact_id),
        auxiliary_binding(
            TEMPLATE_BINDING_ID,
            &fixed.chat_template_auxiliary_artifact_id,
        ),
    ];
    let options = resolved_options(snapshot)?;
    let mut vision_enabled = false;
    let mut speculative = None;
    for realization in &state.option_realizations {
        let values = options
            .get(realization.option_id.as_str())
            .ok_or(VllmPlanError::InvalidResolvedState)?;
        match realization.realization.as_ref() {
            Some(Realization::Vision(vision)) => {
                vision_enabled = boolean_value(values, &vision.enabled_parameter_id)
                    .ok_or(VllmPlanError::InvalidResolvedState)?;
            }
            Some(Realization::Mtp(mtp)) => {
                let count = unsigned_value(values, SPECULATIVE_TOKEN_COUNT_PARAMETER_ID)
                    .ok_or(VllmPlanError::InvalidResolvedState)?;
                let draft = speculative_draft::vllm(mtp.draft.as_ref())
                    .ok_or(VllmPlanError::InvalidResolvedState)?;
                speculative = (count > 0).then_some((count, draft));
            }
            None => return Err(VllmPlanError::InvalidResolvedState),
        }
    }
    if !vision_enabled {
        arguments.push(literal("--language-model-only"));
    }
    arguments.extend(literals(["--served-model-name", &snapshot.model_id]));
    match speculative {
        Some((tokens, SpeculativeDraft::Embedded)) => {
            arguments.extend(literals(["--speculative-config"]));
            arguments.push(literal(&format!(
                r#"{{"method":"mtp","num_speculative_tokens":{tokens}}}"#
            )));
        }
        Some((tokens, SpeculativeDraft::Artifact(artifact_id))) => {
            // The draft path is only known once the workload mounts it, and a binding token is a
            // whole argument, so the draft case uses dotted keys instead of one JSON literal.
            ensure_auxiliary(snapshot, artifact_id)?;
            bindings.push(auxiliary_binding(SPECULATIVE_DRAFT_BINDING_ID, artifact_id));
            arguments.extend(literals([
                "--speculative-config.method",
                "mtp",
                "--speculative-config.model",
            ]));
            arguments.push(binding(SPECULATIVE_DRAFT_BINDING_ID));
            arguments.extend(literals([
                "--speculative-config.num_speculative_tokens",
                &tokens.to_string(),
            ]));
        }
        None => {}
    }
    if !fixed.tool_call_parser.is_empty() {
        arguments.extend(literals(["--tool-call-parser", &fixed.tool_call_parser]));
        // Without it, vLLM constrains only the call envelope and the model may invent arguments.
        arguments.extend(literals(["--tool-strict-level", "parameter"]));
    }
    if !fixed.reasoning_parser.is_empty() {
        arguments.extend(literals(["--reasoning-parser", &fixed.reasoning_parser]));
    }
    arguments.extend(literals(["--chat-template"]));
    arguments.push(binding(TEMPLATE_BINDING_ID));

    // The total context reaches vLLM through the KV cache memory sizing derives from it.
    arguments.extend(parameter_arguments(
        snapshot,
        "--max-model-len",
        CONTEXT_PER_REQUEST_PARAMETER_ID,
    )?);
    arguments.extend(parameter_arguments(
        snapshot,
        "--max-num-seqs",
        PARALLEL_REQUESTS_PARAMETER_ID,
    )?);
    arguments.extend(parameter_arguments(
        snapshot,
        "--max-num-batched-tokens",
        &fixed.maximum_batched_tokens_parameter_id,
    )?);
    arguments.extend(parameter_arguments(
        snapshot,
        "--max-cudagraph-capture-size",
        &fixed.maximum_cuda_graph_capture_size_parameter_id,
    )?);
    if fixed.trust_remote_code {
        arguments.push(literal("--trust-remote-code"));
    }
    if fixed.enable_log_requests {
        arguments.push(literal("--enable-log-requests"));
    }
    match VllmKvCacheDataType::try_from(fixed.kv_cache_data_type) {
        Ok(VllmKvCacheDataType::Fp8) => {
            arguments.extend(literals(["--kv-cache-dtype", "fp8"]));
        }
        Ok(VllmKvCacheDataType::Unspecified) | Err(_) => {
            return Err(VllmPlanError::InvalidResolvedState);
        }
    }
    arguments.extend(parameter_arguments(
        snapshot,
        "--kv-cache-memory-bytes",
        &fixed.kv_cache_memory_bytes_parameter_id,
    )?);
    arguments.extend(literals([
        "--gpu-memory-utilization",
        &memory_utilization(snapshot)?,
    ]));
    let kv_offloading_size =
        unsigned_integer_parameter(snapshot, &fixed.kv_offloading_size_gib_parameter_id)?;
    match VllmKvOffloadingBackend::try_from(fixed.kv_offloading_backend) {
        Ok(VllmKvOffloadingBackend::Native) => {
            if kv_offloading_size > 0 {
                arguments.extend(literals([
                    "--kv-offloading-size",
                    &kv_offloading_size.to_string(),
                    "--kv-offloading-backend",
                    "native",
                ]));
            }
        }
        Ok(VllmKvOffloadingBackend::Unspecified) | Err(_) => {
            return Err(VllmPlanError::InvalidResolvedState);
        }
    }

    validate_custom_argument_tokens(&snapshot.custom_argument_tokens)?;
    arguments.extend(literals_from(&snapshot.custom_argument_tokens));
    Ok(PortableModelLaunchPlan {
        arguments,
        format_version: PLAN_FORMAT_VERSION,
        artifact_bindings: bindings,
    })
}

/// Reports whether the resolved snapshot offloads KV cache blocks to host memory.
/// A zero offloading size disables offload.
pub(crate) fn enables_host_kv_offload(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<bool, VllmPlanError> {
    let fixed = resolved_state(snapshot)?
        .fixed_settings
        .as_ref()
        .ok_or(VllmPlanError::NoFixedSettings)?;
    Ok(unsigned_integer_parameter(snapshot, &fixed.kv_offloading_size_gib_parameter_id)? > 0)
}

/// Validates that a retained plan is canonical for its versioned resolved state.
pub(crate) fn validate_snapshot(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<(), VllmPlanError> {
    validate_resolved_consistency(snapshot)?;
    let expected = translate(snapshot)?;
    if snapshot.desired_launch_plan.as_ref() != Some(&expected) {
        return Err(VllmPlanError::NonCanonicalPlan);
    }
    Ok(())
}

/// Renders the retained footprint over the retained memory-pool total, rounded up to four
/// decimals and capped at 1. vLLM's startup check then asks for room for this footprint on both
/// dedicated and unified memory, where free memory never approaches the default fraction.
fn memory_utilization(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<String, VllmPlanError> {
    let footprint = snapshot
        .sizing
        .as_ref()
        .and_then(|sizing| sizing.accelerator_memory_bytes)
        .ok_or(VllmPlanError::SizingUnavailable)?;
    let total = snapshot
        .selected_target
        .as_ref()
        .and_then(|target| target.accelerator_memory_pool_total_bytes)
        .filter(|total| *total > 0)
        .ok_or(VllmPlanError::MemoryPoolTotalUnavailable)?;
    let scaled = (u128::from(footprint) * UTILIZATION_SCALE)
        .div_ceil(u128::from(total))
        .clamp(1, UTILIZATION_SCALE);
    Ok(format!(
        "{}.{:04}",
        scaled / UTILIZATION_SCALE,
        scaled % UTILIZATION_SCALE
    ))
}

fn resolved_state(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<&VllmResolvedModelState, VllmPlanError> {
    snapshot
        .resolved_engine_state
        .as_ref()
        .filter(|state| {
            state.engine_kind == EngineKind::Vllm as i32
                && state.semantics_version == super::realization::SEMANTICS_VERSION
        })
        .and_then(|state| state.state.as_ref())
        .and_then(|state| match state {
            State::Vllm(state) => Some(state),
            State::LlamaCpp(_) | State::Ninfer(_) => None,
        })
        .ok_or(VllmPlanError::InvalidResolvedState)
}

fn validate_resolved_consistency(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<(), VllmPlanError> {
    let state = resolved_state(snapshot)?;
    let fixed = state
        .fixed_settings
        .as_ref()
        .ok_or(VllmPlanError::NoFixedSettings)?;
    let options = resolved_options(snapshot)?;
    if options.len() != snapshot.resolved_options.len()
        || options.len() != state.option_realizations.len()
    {
        return Err(VllmPlanError::InvalidResolvedState);
    }

    let mut vision_enabled = false;
    let mut expected_auxiliary =
        BTreeSet::from([fixed.chat_template_auxiliary_artifact_id.as_str()]);
    for realization in &state.option_realizations {
        let values = options
            .get(realization.option_id.as_str())
            .ok_or(VllmPlanError::InvalidResolvedState)?;
        match realization.realization.as_ref() {
            Some(Realization::Vision(vision)) => {
                vision_enabled = boolean_value(values, &vision.enabled_parameter_id)
                    .ok_or(VllmPlanError::InvalidResolvedState)?;
            }
            Some(Realization::Mtp(mtp)) => {
                let count = unsigned_value(values, SPECULATIVE_TOKEN_COUNT_PARAMETER_ID)
                    .ok_or(VllmPlanError::InvalidResolvedState)?;
                let draft = speculative_draft::vllm(mtp.draft.as_ref())
                    .ok_or(VllmPlanError::InvalidResolvedState)?;
                if let Some(artifact_id) = draft.artifact_id().filter(|_| count > 0) {
                    expected_auxiliary.insert(artifact_id);
                }
            }
            None => return Err(VllmPlanError::InvalidResolvedState),
        }
    }
    let image_capable = snapshot
        .input_capabilities
        .as_ref()
        .and_then(|capabilities| capabilities.image_input_capacity.as_ref())
        .is_some();
    if vision_enabled != image_capable {
        return Err(VllmPlanError::InvalidResolvedState);
    }

    let selected = snapshot
        .auxiliary_artifacts
        .iter()
        .map(|artifact| artifact.auxiliary_artifact_id.as_str())
        .collect::<BTreeSet<_>>();
    if selected != expected_auxiliary {
        return Err(VllmPlanError::InvalidResolvedState);
    }
    Ok(())
}

fn resolved_options(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<BTreeMap<&str, &dotcore_proto::dotcore::catalog::v1::ModelOptionValues>, VllmPlanError>
{
    let values = snapshot
        .resolved_options
        .iter()
        .filter_map(|option| option.values.as_ref())
        .map(|values| (values.option_id.as_str(), values))
        .collect::<BTreeMap<_, _>>();
    if values.len() != snapshot.resolved_options.len() {
        return Err(VllmPlanError::InvalidResolvedState);
    }
    Ok(values)
}

fn parameter_arguments(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
    flag: &str,
    parameter_id: &str,
) -> Result<Vec<PortableModelLaunchArgumentToken>, VllmPlanError> {
    let value = parameter(snapshot, parameter_id)?.to_string();
    Ok(literals([flag, &value]))
}

fn parameter(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
    parameter_id: &str,
) -> Result<u64, VllmPlanError> {
    let value = unsigned_integer_parameter(snapshot, parameter_id)?;
    (value > 0)
        .then_some(value)
        .ok_or_else(|| VllmPlanError::ParameterNotConfigured(parameter_id.to_owned()))
}

fn unsigned_integer_parameter(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
    parameter_id: &str,
) -> Result<u64, VllmPlanError> {
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
        .ok_or_else(|| VllmPlanError::ParameterNotConfigured(parameter_id.to_owned()))
}

fn validate_custom_argument_tokens(tokens: &[String]) -> Result<(), VllmPlanError> {
    for token in tokens {
        if conflicts_with_resolved_settings(token) {
            return Err(VllmPlanError::ConflictingCustomArgument(token.clone()));
        }
    }
    Ok(())
}

fn conflicts_with_resolved_settings(token: &str) -> bool {
    let Some(root) = normalized_option_root(token) else {
        return false;
    };
    if root.starts_with("--") {
        return ADAPTER_OWNED_FLAGS
            .iter()
            .chain(ADAPTER_OWNED_OVERRIDE_FLAGS.iter())
            .any(|owned| owned.starts_with(&root))
            || ADAPTER_OWNED_NEGATABLE_FLAGS
                .iter()
                .any(|owned| negative_form_starts_with(&root, owned))
            || root.starts_with("--kv-cache-")
            || root.starts_with("--kv-offloading-");
    }

    ADAPTER_OWNED_SHORT_ALIASES
        .iter()
        .any(|owned| owned.starts_with(&root) || root.starts_with(owned))
}

fn normalized_option_root(token: &str) -> Option<String> {
    let option = token.split_once('=').map_or(token, |(option, _)| option);
    if let Some(long) = option.strip_prefix("--") {
        let root = long.split_once('.').map_or(long, |(root, _)| root);
        return Some(format!("--{}", root.replace('_', "-")));
    }

    option.starts_with('-').then(|| option.to_owned())
}

fn negative_form_starts_with(root: &str, owned: &str) -> bool {
    root.strip_prefix("--no-").is_some_and(|candidate| {
        owned
            .strip_prefix("--")
            .is_some_and(|owned| owned.starts_with(candidate))
    })
}

fn ensure_auxiliary(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
    id: &str,
) -> Result<(), VllmPlanError> {
    snapshot
        .auxiliary_artifacts
        .iter()
        .any(|artifact| artifact.auxiliary_artifact_id == id)
        .then_some(())
        .ok_or(VllmPlanError::AuxiliaryArtifactNotResolved)
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

fn boolean_argument(flag: &str, enabled: bool) -> PortableModelLaunchArgumentToken {
    if enabled {
        literal(flag)
    } else {
        literal(&format!(
            "--no-{}",
            flag.strip_prefix("--").expect("vLLM long option")
        ))
    }
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

/// Indicates that resolved values cannot produce a complete portable vLLM plan.
#[derive(Debug, thiserror::Error)]
pub(crate) enum VllmPlanError {
    #[error("vLLM launch plan has invalid resolved engine state")]
    InvalidResolvedState,

    #[error("vLLM launch plan is not canonical for its resolved state")]
    NonCanonicalPlan,

    #[error("vLLM launch plan has no fixed settings")]
    NoFixedSettings,

    #[error("vLLM launch plan has no model artifact")]
    NoModelArtifact,

    #[error("vLLM launch plan has a missing auxiliary artifact")]
    AuxiliaryArtifactNotResolved,

    #[error("vLLM launch plan has no valid value for engine parameter {0:?}")]
    ParameterNotConfigured(String),

    #[error("vLLM custom argument {0:?} conflicts with resolved settings")]
    ConflictingCustomArgument(String),

    #[error(
        "vLLM launch plan has no accelerator memory footprint; the catalog binding must declare an acceleratorMemory estimate"
    )]
    SizingUnavailable,

    #[error(
        "vLLM launch plan has no target memory-pool total; every target in the pool must report one, so recreate the deployment once they do"
    )]
    MemoryPoolTotalUnavailable,
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::{
        catalog::v1::{
            EngineParameterValue, EngineParameterValues, ModelArtifact, ModelAuxiliaryArtifact,
            ModelOptionParameterValue, ModelOptionValues, engine_parameter_value::Value,
        },
        engine::v1::{ResolvedEngineModelState, resolved_engine_model_state::State},
        engine::vllm::v1::{
            VllmEmbeddedMtpHead, VllmFixedModelSettings, VllmKvCacheDataType,
            VllmKvOffloadingBackend, VllmMtpDraftSource, VllmMtpOptionRealization,
            VllmOptionRealization, VllmReadinessMode, VllmReadinessPolicy, VllmResolvedModelState,
            vllm_mtp_draft_source::Source as DraftSource,
        },
        model::v1::{
            ResolvedLocalModelDeploymentSnapshot, ResolvedModelOption, ResolvedSelectedTarget,
            ResolvedSizing,
        },
    };

    use super::*;

    const FOOTPRINT_BYTES: u64 = 32_006_097_397;
    const DEDICATED_TOTAL_BYTES: u64 = 34_190_917_632;
    const UNIFIED_TOTAL_BYTES: u64 = 130_596_225_024;

    fn snapshot() -> ResolvedLocalModelDeploymentSnapshot {
        let fixed = VllmFixedModelSettings {
            reasoning_parser: "reasoning-fixture".to_owned(),
            tool_call_parser: "tools-fixture".to_owned(),
            chat_template_auxiliary_artifact_id: "tmpl-17".to_owned(),
            enable_auto_tool_choice: true,
            enable_chunked_prefill: true,
            enable_async_scheduling: true,
            enable_prefix_caching: true,
            enable_prompt_token_details: true,
            trust_remote_code: true,
            enable_log_requests: true,
            kv_cache_data_type: VllmKvCacheDataType::Fp8 as i32,
            kv_cache_memory_bytes_parameter_id: "bytes".to_owned(),
            maximum_batched_tokens_parameter_id: "batch".to_owned(),
            maximum_cuda_graph_capture_size_parameter_id: "graphs".to_owned(),
            kv_offloading_size_gib_parameter_id: "offload".to_owned(),
            kv_offloading_backend: VllmKvOffloadingBackend::Native as i32,
            container_port: 15123,
            readiness_policy: Some(VllmReadinessPolicy {
                mode: VllmReadinessMode::Models as i32,
            }),
            ..Default::default()
        };
        let state = VllmResolvedModelState {
            fixed_settings: Some(fixed),
            option_realizations: Vec::new(),
        };
        ResolvedLocalModelDeploymentSnapshot {
            model_id: "unrelated-model".to_owned(),
            model_artifact: Some(ModelArtifact::default()),
            auxiliary_artifacts: vec![ModelAuxiliaryArtifact {
                auxiliary_artifact_id: "tmpl-17".to_owned(),
                ..Default::default()
            }],
            resolved_engine_state: Some(ResolvedEngineModelState {
                engine_kind: EngineKind::Vllm as i32,
                semantics_version: super::super::realization::SEMANTICS_VERSION,
                state: Some(State::Vllm(state)),
            }),
            engine_parameter_values: Some(EngineParameterValues {
                values: [
                    ("bytes", 7000_u64),
                    (PARALLEL_REQUESTS_PARAMETER_ID, 8),
                    ("batch", 4096),
                    ("graphs", 2048),
                    ("offload", 16),
                    (CONTEXT_PER_REQUEST_PARAMETER_ID, 32768),
                ]
                .into_iter()
                .map(|(parameter_id, value)| EngineParameterValue {
                    parameter_id: parameter_id.to_owned(),
                    value: Some(Value::UnsignedInteger(value)),
                })
                .collect(),
            }),
            selected_target: Some(ResolvedSelectedTarget {
                accelerator_memory_pool_total_bytes: Some(DEDICATED_TOTAL_BYTES),
                ..Default::default()
            }),
            sizing: Some(ResolvedSizing {
                accelerator_memory_bytes: Some(FOOTPRINT_BYTES),
                derived_engine_parameter_ids: vec!["bytes".to_owned()],
                host_memory_bytes: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn host_kv_offload_follows_the_offloading_size_parameter() {
        let mut local = snapshot();
        assert!(enables_host_kv_offload(&local).expect("positive offload size"));

        let values = &mut local
            .engine_parameter_values
            .as_mut()
            .expect("parameter values")
            .values;
        values
            .iter_mut()
            .find(|value| value.parameter_id == "offload")
            .expect("offload parameter")
            .value = Some(Value::UnsignedInteger(0));
        assert!(!enables_host_kv_offload(&local).expect("zero offload size"));

        local
            .engine_parameter_values
            .as_mut()
            .expect("parameter values")
            .values
            .retain(|value| value.parameter_id != "offload");
        assert_eq!(
            enables_host_kv_offload(&local)
                .expect_err("missing offload size")
                .to_string(),
            translate(&local)
                .expect_err("missing offload size cannot launch")
                .to_string()
        );
    }

    #[test]
    fn canonical_tokens_use_the_shared_parameters_and_retained_identity() {
        let plan = translate(&snapshot()).expect("plan");
        let values = plan
            .arguments
            .iter()
            .filter_map(|token| token.value.as_ref())
            .collect::<Vec<_>>();
        let literals = values
            .iter()
            .filter_map(|value| match value {
                ArgumentValue::Literal(value) => Some(value.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            literals,
            [
                "vllm",
                "serve",
                "--host",
                "0.0.0.0",
                "--port",
                "15123",
                "--enable-auto-tool-choice",
                "--enable-chunked-prefill",
                "--async-scheduling",
                "--enable-prefix-caching",
                "--enable-prompt-tokens-details",
                "--language-model-only",
                "--served-model-name",
                "unrelated-model",
                "--tool-call-parser",
                "tools-fixture",
                "--tool-strict-level",
                "parameter",
                "--reasoning-parser",
                "reasoning-fixture",
                "--chat-template",
                "--max-model-len",
                "32768",
                "--max-num-seqs",
                "8",
                "--max-num-batched-tokens",
                "4096",
                "--max-cudagraph-capture-size",
                "2048",
                "--trust-remote-code",
                "--enable-log-requests",
                "--kv-cache-dtype",
                "fp8",
                "--kv-cache-memory-bytes",
                "7000",
                "--gpu-memory-utilization",
                "0.9361",
                "--kv-offloading-size",
                "16",
                "--kv-offloading-backend",
                "native",
            ]
        );
        assert!(!literals.contains(&"qwen3-8-27b"));
    }

    #[test]
    fn memory_utilization_divides_the_footprint_by_the_retained_pool_total() {
        for (total, footprint, expected) in [
            (DEDICATED_TOTAL_BYTES, FOOTPRINT_BYTES, "0.9361"),
            (UNIFIED_TOTAL_BYTES, FOOTPRINT_BYTES, "0.2451"),
            (DEDICATED_TOTAL_BYTES, DEDICATED_TOTAL_BYTES + 1, "1.0000"),
        ] {
            let mut fixture = snapshot();
            fixture
                .selected_target
                .as_mut()
                .expect("selected target")
                .accelerator_memory_pool_total_bytes = Some(total);
            fixture
                .sizing
                .as_mut()
                .expect("sizing")
                .accelerator_memory_bytes = Some(footprint);

            let plan = translate(&fixture).expect("plan");
            let literals = plan
                .arguments
                .iter()
                .filter_map(|token| match token.value.as_ref() {
                    Some(ArgumentValue::Literal(value)) => Some(value.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert!(
                literals
                    .windows(2)
                    .any(|pair| pair == ["--gpu-memory-utilization", expected]),
                "{total} {footprint}: {literals:?}"
            );
        }
    }

    #[test]
    fn missing_footprint_or_pool_total_cannot_launch() {
        let mut no_sizing = snapshot();
        no_sizing.sizing = None;
        assert!(matches!(
            translate(&no_sizing),
            Err(VllmPlanError::SizingUnavailable)
        ));
        assert!(matches!(
            validate_snapshot(&no_sizing),
            Err(VllmPlanError::SizingUnavailable)
        ));

        let mut no_total = snapshot();
        no_total
            .selected_target
            .as_mut()
            .expect("selected target")
            .accelerator_memory_pool_total_bytes = None;
        assert_eq!(
            translate(&no_total).expect_err("no pool total").to_string(),
            "vLLM launch plan has no target memory-pool total; every target in the pool must report one, so recreate the deployment once they do"
        );
    }

    #[test]
    fn canonical_tokens_omit_kv_offloading_when_size_is_zero() {
        let mut fixture = snapshot();
        let offload = fixture
            .engine_parameter_values
            .as_mut()
            .expect("parameter values")
            .values
            .iter_mut()
            .find(|parameter| parameter.parameter_id == "offload")
            .expect("offload parameter");
        offload.value = Some(Value::UnsignedInteger(0));

        let plan = translate(&fixture).expect("plan");
        let literals = plan
            .arguments
            .iter()
            .filter_map(|token| match token.value.as_ref() {
                Some(ArgumentValue::Literal(value)) => Some(value.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(!literals.contains(&"--kv-offloading-size"));
        assert!(!literals.contains(&"--kv-offloading-backend"));
    }

    #[test]
    fn canonical_state_rejects_wrong_semantics_version() {
        let mut fixture = snapshot();
        fixture
            .resolved_engine_state
            .as_mut()
            .expect("state")
            .semantics_version += 1;
        assert!(matches!(
            validate_snapshot(&fixture),
            Err(VllmPlanError::InvalidResolvedState)
        ));
    }

    #[test]
    fn custom_arguments_reject_adapter_owned_flags() {
        for token in [
            "--served-model-name=other",
            "--kv-cache-memory-bytes",
            "--gpu-memory-utilization=0.5",
            "--language-model-only",
            "--speculative-config",
        ] {
            assert!(matches!(
                validate_custom_argument_tokens(&[token.to_owned()]),
                Err(VllmPlanError::ConflictingCustomArgument(value)) if value == token
            ));
        }
    }

    #[test]
    fn custom_arguments_reject_abbreviated_adapter_owned_flags() {
        for token in [
            "--po=9999",
            "--served-model-n=other",
            "--kv-cache-memory-b=1",
            "--gpu-mem=0.5",
            "--language-model-o",
            "--speculative-c",
        ] {
            assert!(matches!(
                validate_custom_argument_tokens(&[token.to_owned()]),
                Err(VllmPlanError::ConflictingCustomArgument(value)) if value == token
            ));
        }
    }

    #[test]
    fn custom_arguments_reject_normalized_and_aliased_adapter_owned_flags() {
        for token in [
            "--served_model_name=other",
            "--gpu_memory_utilization=0.5",
            "--enable_chunked_prefill",
            "--no-language-model-only",
            "--no-trust-remote-code",
            "--no-enable-log-requests",
            "--no-enable-auto-tool-choice",
            "--no-enable-chunked-prefill",
            "--no-async-scheduling",
            "--no-enable-prefix-caching",
            "--no-enable-prompt-tokens-details",
            "--no-enable-prefix",
            "--no_enable_prefix_caching",
            "--speculative-config.method=mtp",
            "--speculative_config.num_speculative_tokens=3",
            "--served-model-name.foo=1",
            "-sc",
            "-sc={\"method\":\"mtp\"}",
            "-sc{\"method\":\"mtp\"}",
            "-s",
            "--kv_cache_metrics",
        ] {
            assert!(matches!(
                validate_custom_argument_tokens(&[token.to_owned()]),
                Err(VllmPlanError::ConflictingCustomArgument(value)) if value == token
            ));
        }
    }

    #[test]
    fn custom_arguments_reject_indirect_adapter_owned_settings() {
        for token in [
            "--config",
            "--compilation-config.max_cudagraph_capture_size=128",
            "-cc={\"max_cudagraph_capture_size\":128}",
            "--cudagraph-capture-sizes=128",
            "--spec-method=mtp",
            "--spec-tokens=4",
            "--spec-model=model",
        ] {
            assert!(matches!(
                validate_custom_argument_tokens(&[token.to_owned()]),
                Err(VllmPlanError::ConflictingCustomArgument(value)) if value == token
            ));
        }
    }

    #[test]
    fn custom_arguments_preserve_unowned_vllm_flags() {
        assert!(
            validate_custom_argument_tokens(&[
                "--disable-log-stats".to_owned(),
                "--uvicorn-log-level=warning".to_owned(),
                "--default-chat-template-kwargs".to_owned(),
                "{\"enable_thinking\":true}".to_owned(),
                "--max-num-scheduled-tokens=4096".to_owned(),
                "--enable-flashinfer-autotune".to_owned(),
                "--no-enable-flashinfer-autotune".to_owned(),
                "--seed".to_owned(),
                "7".to_owned(),
            ])
            .is_ok()
        );
    }

    #[test]
    fn model_dependent_booleans_emit_explicit_true_and_false_forms() {
        let mut fixture = snapshot();
        let State::Vllm(state) = fixture
            .resolved_engine_state
            .as_mut()
            .and_then(|state| state.state.as_mut())
            .expect("vLLM state")
        else {
            panic!("vLLM state");
        };
        let fixed = state.fixed_settings.as_mut().expect("fixed settings");
        fixed.enable_chunked_prefill = false;
        fixed.enable_prefix_caching = false;

        let plan = translate(&fixture).expect("disabled plan");
        let literals = plan
            .arguments
            .iter()
            .filter_map(|token| match token.value.as_ref() {
                Some(ArgumentValue::Literal(value)) => Some(value.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(literals.contains(&"--no-enable-chunked-prefill"));
        assert!(literals.contains(&"--no-enable-prefix-caching"));
        assert!(!literals.contains(&"--enable-chunked-prefill"));
        assert!(!literals.contains(&"--enable-prefix-caching"));

        fixture.desired_launch_plan = Some(plan);
        validate_snapshot(&fixture).expect("canonical disabled plan");
    }

    #[test]
    fn mtp_token_count_controls_the_speculative_config_argument() {
        let mut off = snapshot();
        configure_mtp(&mut off, 0, None);
        let off_plan = translate(&off).expect("MTP off plan");
        let mut on = snapshot();
        configure_mtp(&mut on, 4, None);
        let on_plan = translate(&on).expect("MTP on plan");

        let off_arguments = vec![
            ArgumentValue::Literal("vllm".to_owned()),
            ArgumentValue::Literal("serve".to_owned()),
            ArgumentValue::ArtifactBindingId("model-snapshot".to_owned()),
            ArgumentValue::Literal("--host".to_owned()),
            ArgumentValue::Literal("0.0.0.0".to_owned()),
            ArgumentValue::Literal("--port".to_owned()),
            ArgumentValue::Literal("15123".to_owned()),
            ArgumentValue::Literal("--enable-auto-tool-choice".to_owned()),
            ArgumentValue::Literal("--enable-chunked-prefill".to_owned()),
            ArgumentValue::Literal("--async-scheduling".to_owned()),
            ArgumentValue::Literal("--enable-prefix-caching".to_owned()),
            ArgumentValue::Literal("--enable-prompt-tokens-details".to_owned()),
            ArgumentValue::Literal("--language-model-only".to_owned()),
            ArgumentValue::Literal("--served-model-name".to_owned()),
            ArgumentValue::Literal("unrelated-model".to_owned()),
            ArgumentValue::Literal("--tool-call-parser".to_owned()),
            ArgumentValue::Literal("tools-fixture".to_owned()),
            ArgumentValue::Literal("--tool-strict-level".to_owned()),
            ArgumentValue::Literal("parameter".to_owned()),
            ArgumentValue::Literal("--reasoning-parser".to_owned()),
            ArgumentValue::Literal("reasoning-fixture".to_owned()),
            ArgumentValue::Literal("--chat-template".to_owned()),
            ArgumentValue::ArtifactBindingId("chat-template".to_owned()),
            ArgumentValue::Literal("--max-model-len".to_owned()),
            ArgumentValue::Literal("32768".to_owned()),
            ArgumentValue::Literal("--max-num-seqs".to_owned()),
            ArgumentValue::Literal("8".to_owned()),
            ArgumentValue::Literal("--max-num-batched-tokens".to_owned()),
            ArgumentValue::Literal("4096".to_owned()),
            ArgumentValue::Literal("--max-cudagraph-capture-size".to_owned()),
            ArgumentValue::Literal("2048".to_owned()),
            ArgumentValue::Literal("--trust-remote-code".to_owned()),
            ArgumentValue::Literal("--enable-log-requests".to_owned()),
            ArgumentValue::Literal("--kv-cache-dtype".to_owned()),
            ArgumentValue::Literal("fp8".to_owned()),
            ArgumentValue::Literal("--kv-cache-memory-bytes".to_owned()),
            ArgumentValue::Literal("7000".to_owned()),
            ArgumentValue::Literal("--gpu-memory-utilization".to_owned()),
            ArgumentValue::Literal("0.9361".to_owned()),
            ArgumentValue::Literal("--kv-offloading-size".to_owned()),
            ArgumentValue::Literal("16".to_owned()),
            ArgumentValue::Literal("--kv-offloading-backend".to_owned()),
            ArgumentValue::Literal("native".to_owned()),
        ];
        let mut on_arguments = off_arguments.clone();
        on_arguments.splice(
            15..15,
            [
                ArgumentValue::Literal("--speculative-config".to_owned()),
                ArgumentValue::Literal(r#"{"method":"mtp","num_speculative_tokens":4}"#.to_owned()),
            ],
        );

        assert_eq!(argument_values(&off_plan), off_arguments);
        assert_eq!(argument_values(&on_plan), on_arguments);
        off.desired_launch_plan = Some(off_plan);
        validate_snapshot(&off).expect("MTP off snapshot");
        on.desired_launch_plan = Some(on_plan);
        validate_snapshot(&on).expect("MTP on snapshot");
    }

    #[test]
    fn mtp_draft_checkpoint_is_bound_through_dotted_speculative_config_keys() {
        let mut embedded = snapshot();
        configure_mtp(&mut embedded, 0, None);
        let base_plan = translate(&embedded).expect("MTP off plan");
        let mut off = snapshot();
        configure_mtp(&mut off, 0, Some("assistant"));
        let off_plan = translate(&off).expect("draft MTP off plan");
        let mut on = snapshot();
        configure_mtp(&mut on, 3, Some("assistant"));
        let on_plan = translate(&on).expect("draft MTP on plan");

        assert_eq!(off_plan, base_plan);
        let mut on_arguments = argument_values(&base_plan);
        on_arguments.splice(
            15..15,
            [
                ArgumentValue::Literal("--speculative-config.method".to_owned()),
                ArgumentValue::Literal("mtp".to_owned()),
                ArgumentValue::Literal("--speculative-config.model".to_owned()),
                ArgumentValue::ArtifactBindingId("speculative-draft".to_owned()),
                ArgumentValue::Literal("--speculative-config.num_speculative_tokens".to_owned()),
                ArgumentValue::Literal("3".to_owned()),
            ],
        );
        assert_eq!(argument_values(&on_plan), on_arguments);
        let mut on_bindings = base_plan.artifact_bindings.clone();
        on_bindings.push(PortableModelArtifactBinding {
            binding_id: "speculative-draft".to_owned(),
            artifact: Some(BoundArtifact::AuxiliaryArtifactId("assistant".to_owned())),
            read_only: true,
        });
        assert_eq!(on_plan.artifact_bindings, on_bindings);

        off.desired_launch_plan = Some(off_plan);
        validate_snapshot(&off).expect("draft MTP off snapshot");
        on.desired_launch_plan = Some(on_plan.clone());
        validate_snapshot(&on).expect("draft MTP on snapshot");

        // The draft is selected exactly while the count is above zero.
        on.auxiliary_artifacts.pop();
        assert!(matches!(
            translate(&on),
            Err(VllmPlanError::AuxiliaryArtifactNotResolved)
        ));
        off.auxiliary_artifacts.push(ModelAuxiliaryArtifact {
            auxiliary_artifact_id: "assistant".to_owned(),
            ..Default::default()
        });
        assert!(matches!(
            validate_snapshot(&off),
            Err(VllmPlanError::InvalidResolvedState)
        ));
    }

    /// Realizes an MTP option at the given count, with an embedded head or, given its id, a draft
    /// checkpoint selected while the count is above zero.
    fn configure_mtp(
        snapshot: &mut ResolvedLocalModelDeploymentSnapshot,
        count: u64,
        draft: Option<&str>,
    ) {
        let State::Vllm(state) = snapshot
            .resolved_engine_state
            .as_mut()
            .and_then(|state| state.state.as_mut())
            .expect("vLLM state")
        else {
            panic!("vLLM state");
        };
        state.option_realizations = vec![VllmOptionRealization {
            option_id: "mtp".to_owned(),
            realization: Some(Realization::Mtp(VllmMtpOptionRealization {
                draft: Some(VllmMtpDraftSource {
                    source: Some(
                        draft.map_or(DraftSource::Embedded(VllmEmbeddedMtpHead {}), |id| {
                            DraftSource::AuxiliaryArtifactId(id.to_owned())
                        }),
                    ),
                }),
            })),
        }];
        if let Some(draft) = draft.filter(|_| count > 0) {
            snapshot.auxiliary_artifacts.push(ModelAuxiliaryArtifact {
                auxiliary_artifact_id: draft.to_owned(),
                ..Default::default()
            });
        }
        snapshot.resolved_options = vec![ResolvedModelOption {
            values: Some(ModelOptionValues {
                option_id: "mtp".to_owned(),
                values: vec![ModelOptionParameterValue {
                    parameter_id: SPECULATIVE_TOKEN_COUNT_PARAMETER_ID.to_owned(),
                    value: Some(
                        dotcore_proto::dotcore::catalog::v1::model_option_parameter_value::Value::UnsignedInteger(
                            count,
                        ),
                    ),
                }],
            }),
        }];
    }

    #[test]
    fn translated_plan_is_accepted_by_the_workload_materialization() {
        let plan = translate(&snapshot()).expect("plan");
        let artifacts = dotcore_artifact::PreparedModelArtifacts {
            artifacts: [
                (MODEL_BINDING_ID, "/cache/model-snapshot"),
                (TEMPLATE_BINDING_ID, "/cache/template.jinja"),
            ]
            .into_iter()
            .map(|(binding_id, path)| (binding_id.to_owned(), std::path::PathBuf::from(path)))
            .collect(),
        };

        let materialized = super::super::workload::materialize_plan(&plan, &artifacts)
            .expect("translated plan materializes");

        assert_eq!(plan.format_version, PLAN_FORMAT_VERSION);
        assert_eq!(materialized.command.len(), plan.arguments.len());
    }

    fn argument_values(plan: &PortableModelLaunchPlan) -> Vec<ArgumentValue> {
        plan.arguments
            .iter()
            .map(|argument| argument.value.clone().expect("argument value"))
            .collect()
    }
}
