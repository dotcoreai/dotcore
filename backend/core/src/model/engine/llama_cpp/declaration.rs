//! llama.cpp-owned catalog declaration validation.

use std::collections::BTreeSet;

use dotcore_catalog::{
    options::KV_CACHE_PRECISION_PARAMETER_ID,
    shared_parameters::{
        CONTEXT_PER_REQUEST_PARAMETER_ID, PARALLEL_REQUESTS_PARAMETER_ID,
        TOTAL_CONTEXT_PARAMETER_ID, maximum_context_tokens,
    },
};
use dotcore_domain::Accelerator;
use dotcore_proto::dotcore::{
    catalog::v1::{
        EngineParameterDefinition, EngineParameterValue, EngineParameterValues, EngineRelease,
        HardwareClass, Model, ModelConfigurationOption, ModelEngineConfiguration,
        ModelInputModality, SpeculativeDecodingMethod, engine_parameter_definition::ParameterType,
        engine_parameter_value::Value as EngineParameterValueKind,
        hugging_face_artifact_source::Content as HuggingFaceContent,
        model_option::Kind as ModelOptionKind,
        model_option_parameter_definition::ParameterType as ModelOptionParameterType,
    },
    engine::{
        llama_cpp::v1::{
            LlamaCppImageDetail, LlamaCppImageInputCapacity, LlamaCppImageMediaType,
            LlamaCppModelConfiguration, LlamaCppOptionRealization,
            llama_cpp_option_realization::Realization,
        },
        v1::engine_model_configuration::Configuration,
    },
};

use super::plan::{
    DEFAULT_KV_CACHE_PRECISION, KV_CACHE_PRECISIONS, PROMPT_CACHE_MEMORY_PARAMETER_ID,
    speculative_type,
};
use crate::model::engine::{
    declaration::classes_outside_vendors,
    speculative_draft::{self, SpeculativeDraft},
    split_gguf::split_gguf_entry,
    unsigned_parameter_range, unsigned_parameter_value,
};

/// Largest total context and context per request a llama.cpp release may declare, in tokens. A
/// release may declare lower maxima.
const MAXIMUM_CONTEXT_TOKENS: u64 = 16_777_216;
/// Largest parallel request count a llama.cpp release may declare, within llama.cpp's own sequence
/// limit. Sizing counts each request's own state, so estimates follow the count.
const MAXIMUM_PARALLEL_REQUESTS: u64 = 64;

/// Accepts a release whose schema declares exactly the shared total context, context per request
/// and parallel requests, then the prompt cache memory bound, in that order, within llama.cpp's
/// limits. The catalog checks the shared parameters' fixed shape.
pub(crate) fn validate_engine_release(engine: &EngineRelease) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    let valid =
        engine
            .parameter_schema
            .as_ref()
            .is_some_and(|schema| match schema.parameters.as_slice() {
                [total, per_request, parallel, cache] => {
                    is_bounded(total, TOTAL_CONTEXT_PARAMETER_ID, MAXIMUM_CONTEXT_TOKENS)
                        && is_bounded(
                            per_request,
                            CONTEXT_PER_REQUEST_PARAMETER_ID,
                            MAXIMUM_CONTEXT_TOKENS,
                        )
                        && is_bounded(
                            parallel,
                            PARALLEL_REQUESTS_PARAMETER_ID,
                            MAXIMUM_PARALLEL_REQUESTS,
                        )
                        && is_prompt_cache_memory(cache)
                }
                _ => false,
            });
    if !valid {
        errors.push(anyhow::anyhow!(
            "llama.cpp engine release requires the approved parameter schema: \
             {TOTAL_CONTEXT_PARAMETER_ID} and {CONTEXT_PER_REQUEST_PARAMETER_ID} up to \
             {MAXIMUM_CONTEXT_TOKENS} tokens, {PARALLEL_REQUESTS_PARAMETER_ID} up to \
             {MAXIMUM_PARALLEL_REQUESTS}, and {PROMPT_CACHE_MEMORY_PARAMETER_ID}, in that order"
        ));
    }
    errors
}

pub(crate) fn validate_model_configuration(
    model: &Model,
    configuration: &ModelEngineConfiguration,
) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    validate_bound_artifacts(model, configuration, &mut errors);

    let Some(payload) = configuration.engine_configuration.as_ref() else {
        return errors;
    };
    let Some(Configuration::LlamaCpp(payload)) = payload.configuration.as_ref() else {
        errors.push(anyhow::anyhow!(
            "configuration {:?} has a payload that does not match llama.cpp",
            configuration.configuration_id
        ));
        return errors;
    };
    validate_payload(model, configuration, payload, &mut errors);
    errors
}

/// Accepts supported hardware classes that select NVIDIA or Apple accelerators, the vendors the
/// llama.cpp artifacts run on. Selectors match single-device targets only, so llama.cpp runs on
/// exactly one accelerator.
pub(crate) fn validate_hardware_support(
    configuration: &ModelEngineConfiguration,
    hardware_classes: &[&HardwareClass],
) -> Vec<anyhow::Error> {
    classes_outside_vendors(hardware_classes, &[Accelerator::Nvidia, Accelerator::Apple])
        .into_iter()
        .map(|hardware_class_id| {
            anyhow::anyhow!(
                "configuration {:?} hardware class {hardware_class_id:?} must select nvidia or apple accelerators for llama.cpp",
                configuration.configuration_id
            )
        })
        .collect()
}

/// Returns the engine parameter defaults a configuration's fixed settings imply. A single
/// micro-batch configuration defaults the total context to its maximum context, since the
/// micro-batch spans the total context and the release default would not follow the model.
pub(crate) fn parameter_defaults(
    model: &Model,
    configuration: &ModelEngineConfiguration,
) -> EngineParameterValues {
    if !uses_single_micro_batch(configuration) {
        return EngineParameterValues::default();
    }
    EngineParameterValues {
        values: vec![EngineParameterValue {
            parameter_id: TOTAL_CONTEXT_PARAMETER_ID.to_owned(),
            value: Some(EngineParameterValueKind::UnsignedInteger(u64::from(
                maximum_context_tokens(model, configuration),
            ))),
        }],
    }
}

/// Rejects a single micro-batch configuration whose maximum context, the total context it
/// defaults to, lies outside the total context range its engine release accepts, since every
/// default launch would then fail.
pub(crate) fn validate_total_context_default(
    model: &Model,
    configuration: &ModelEngineConfiguration,
    engine: &EngineRelease,
) -> Option<anyhow::Error> {
    if !uses_single_micro_batch(configuration) {
        return None;
    }
    let range = engine
        .parameter_schema
        .as_ref()
        .and_then(|schema| unsigned_parameter_range(schema, TOTAL_CONTEXT_PARAMETER_ID))?;
    let maximum = maximum_context_tokens(model, configuration);
    (!range.contains(&u64::from(maximum))).then(|| {
        anyhow::anyhow!(
            "configuration {:?} processes prompts in a single llama.cpp micro-batch, so its total context defaults to its maximum context of {maximum} tokens, outside the {} to {} tokens its engine release accepts; declare a maximum context within that range",
            configuration.configuration_id,
            range.start(),
            range.end()
        )
    })
}

/// Rejects a total context above the maximum context of a single micro-batch configuration, whose
/// micro-batch, and so compute memory, grows with the total context while no request could use
/// the excess.
pub(crate) fn validate_parameter_values(
    model: &Model,
    configuration: &ModelEngineConfiguration,
    values: &EngineParameterValues,
) -> anyhow::Result<()> {
    if !uses_single_micro_batch(configuration) {
        return Ok(());
    }
    let maximum = maximum_context_tokens(model, configuration);
    if let Some(total_context) = unsigned_parameter_value(values, TOTAL_CONTEXT_PARAMETER_ID)
        && total_context > u64::from(maximum)
    {
        anyhow::bail!(
            "total context of {total_context} tokens is above the maximum context of {maximum} tokens of configuration {:?}, which processes prompts in a single llama.cpp micro-batch; lower the total context",
            configuration.configuration_id
        );
    }
    Ok(())
}

/// Checks that explicitly pinned sampling values have finite, usable engine ranges.
/// Missing values leave the corresponding llama.cpp default unchanged.
pub(super) fn validate_sampling_settings(
    sampling: &dotcore_proto::dotcore::engine::llama_cpp::v1::LlamaCppSamplingSettings,
) -> Result<(), LlamaCppSamplingError> {
    // std::stof can reject overflow and subnormal underflow even for finite doubles.
    let engine_range = f64::from(f32::MIN_POSITIVE)..=f64::from(f32::MAX);

    for (parameter, value, valid, range) in [
        (
            "min-p",
            sampling.min_p,
            sampling
                .min_p
                .is_none_or(|value| (0.0..=1.0).contains(&value)),
            "between 0 and 1",
        ),
        (
            "repeat-penalty",
            sampling.repeat_penalty,
            sampling.repeat_penalty.is_none_or(|value| value > 0.0),
            "greater than 0",
        ),
        (
            "temperature",
            sampling.temperature,
            sampling.temperature.is_none_or(|value| value >= 0.0),
            "at least 0",
        ),
        (
            "top-p",
            sampling.top_p,
            sampling
                .top_p
                .is_none_or(|value| (0.0..=1.0).contains(&value)),
            "between 0 and 1",
        ),
    ] {
        if let Some(value) = value
            && (!value.is_finite()
                || !valid
                || (value != 0.0 && !engine_range.contains(&value.abs())))
        {
            return Err(LlamaCppSamplingError::InvalidValue {
                parameter,
                value,
                range,
            });
        }
    }

    Ok(())
}

/// Whether the configuration's fixed llama.cpp settings process each prompt in a single
/// micro-batch.
fn uses_single_micro_batch(configuration: &ModelEngineConfiguration) -> bool {
    matches!(
        configuration
            .engine_configuration
            .as_ref()
            .and_then(|payload| payload.configuration.as_ref()),
        Some(Configuration::LlamaCpp(payload))
            if payload
                .fixed_settings
                .as_ref()
                .is_some_and(|fixed| fixed.single_micro_batch)
    )
}

/// Checks that every bound model artifact is a GGUF file or a split GGUF set, and every bound
/// auxiliary artifact is a single GGUF file, since auxiliaries mount at file destinations.
fn validate_bound_artifacts(
    model: &Model,
    configuration: &ModelEngineConfiguration,
    errors: &mut Vec<anyhow::Error>,
) {
    let model_artifact_ids = configuration
        .artifact_bindings
        .iter()
        .map(|binding| binding.model_artifact_id.as_str())
        .collect::<BTreeSet<_>>();
    for artifact in model
        .artifacts
        .iter()
        .filter(|artifact| model_artifact_ids.contains(artifact.artifact_id.as_str()))
    {
        match artifact
            .source
            .as_ref()
            .and_then(|source| source.content.as_ref())
        {
            Some(HuggingFaceContent::File(file)) if file.path.ends_with(".gguf") => {}
            Some(HuggingFaceContent::Snapshot(snapshot)) => {
                if let Err(error) =
                    split_gguf_entry(snapshot.files.iter().map(|file| file.path.as_str()))
                {
                    errors.push(anyhow::Error::new(error).context(format!(
                        "configuration {:?} binds model artifact {:?}, which is not a valid \
                         split GGUF set",
                        configuration.configuration_id, artifact.artifact_id
                    )));
                }
            }
            _ => errors.push(anyhow::anyhow!(
                "configuration {:?} requires model artifact {:?} to be a GGUF file or a split \
                 GGUF set for llama.cpp",
                configuration.configuration_id,
                artifact.artifact_id
            )),
        }
    }

    let auxiliary_artifact_ids = configuration
        .artifact_bindings
        .iter()
        .flat_map(|binding| binding.auxiliary_artifact_ids.iter().map(String::as_str))
        .collect::<BTreeSet<_>>();
    for artifact in model
        .auxiliary_artifacts
        .iter()
        .filter(|artifact| auxiliary_artifact_ids.contains(artifact.auxiliary_artifact_id.as_str()))
    {
        if !matches!(
            artifact
                .source
                .as_ref()
                .and_then(|source| source.content.as_ref()),
            Some(HuggingFaceContent::File(file)) if file.path.ends_with(".gguf")
        ) {
            errors.push(anyhow::anyhow!(
                "configuration {:?} requires auxiliary artifact {:?} to be a single GGUF file \
                 for llama.cpp",
                configuration.configuration_id,
                artifact.auxiliary_artifact_id
            ));
        }
    }
}

fn validate_payload(
    model: &Model,
    configuration: &ModelEngineConfiguration,
    payload: &LlamaCppModelConfiguration,
    errors: &mut Vec<anyhow::Error>,
) {
    let auxiliary = model
        .auxiliary_artifacts
        .iter()
        .map(|artifact| artifact.auxiliary_artifact_id.as_str())
        .collect::<BTreeSet<_>>();

    let Some(fixed) = payload.fixed_settings.as_ref() else {
        errors.push(anyhow::anyhow!(
            "configuration {:?} requires fixed llama.cpp settings",
            configuration.configuration_id
        ));
        return;
    };
    // The micro-batch spans the total context, which defaults to the configuration's maximum, so
    // the model's own maximum would size it far beyond what the configuration's sizing covers.
    if fixed.single_micro_batch && configuration.maximum_context_tokens.is_none() {
        errors.push(anyhow::anyhow!(
            "configuration {:?} processes prompts in a single llama.cpp micro-batch and must declare its own maximum context",
            configuration.configuration_id
        ));
    }
    if let Some(sampling) = fixed.sampling.as_ref()
        && let Err(error) = validate_sampling_settings(sampling)
    {
        errors.push(anyhow::Error::new(error).context(format!(
            "configuration {:?} has invalid llama.cpp sampling settings",
            configuration.configuration_id
        )));
    }

    let mut fixed_auxiliary = BTreeSet::new();
    if let Some(projector) = fixed.vision_projector.as_ref() {
        fixed_auxiliary.insert(projector.auxiliary_artifact_id.as_str());
    }
    if let Some(speculative) = fixed.speculative.as_ref() {
        if speculative_type(speculative.method).is_none() {
            errors.push(anyhow::anyhow!(
                "configuration {:?} has an unsupported fixed llama.cpp speculative method",
                configuration.configuration_id
            ));
        }
        match speculative_draft::llama_cpp(speculative.draft.as_ref()) {
            Some(SpeculativeDraft::Embedded) => {}
            Some(SpeculativeDraft::Artifact(artifact_id)) => {
                fixed_auxiliary.insert(artifact_id);
            }
            None => errors.push(anyhow::anyhow!(
                "configuration {:?} has no fixed llama.cpp speculative draft source",
                configuration.configuration_id
            )),
        }
        if speculative.draft_maximum == 0 {
            errors.push(anyhow::anyhow!(
                "configuration {:?} has an invalid fixed llama.cpp speculative draft maximum",
                configuration.configuration_id
            ));
        }
    }
    if fixed_auxiliary.iter().any(|id| !auxiliary.contains(id))
        || configuration.artifact_bindings.iter().any(|binding| {
            !fixed_auxiliary.iter().all(|id| {
                binding
                    .auxiliary_artifact_ids
                    .iter()
                    .any(|candidate| candidate == id)
            })
        })
    {
        errors.push(anyhow::anyhow!(
            "configuration {:?} has invalid fixed llama.cpp auxiliary settings",
            configuration.configuration_id
        ));
    }

    let image_capable = configuration
        .input_capabilities
        .as_ref()
        .is_some_and(|capabilities| {
            capabilities
                .supported_modalities
                .contains(&(ModelInputModality::Image as i32))
        });
    if image_capable != fixed.vision_projector.is_some() {
        errors.push(anyhow::anyhow!(
            "configuration {:?} has a llama.cpp capability and projector mismatch",
            configuration.configuration_id
        ));
    }

    validate_realizations(model, configuration, payload, &auxiliary, fixed, errors);
    validate_kv_cache_precisions(configuration, payload, fixed, errors);
}

fn validate_realizations(
    model: &Model,
    configuration: &ModelEngineConfiguration,
    payload: &LlamaCppModelConfiguration,
    auxiliary: &BTreeSet<&str>,
    fixed: &dotcore_proto::dotcore::engine::llama_cpp::v1::LlamaCppFixedModelSettings,
    errors: &mut Vec<anyhow::Error>,
) {
    let mut option_ids = BTreeSet::new();
    let mut selectable_auxiliary = BTreeSet::new();
    let mut vision_count = 0;
    let mut mtp_count = 0;

    for realization in &payload.option_realizations {
        let option = configuration
            .options
            .iter()
            .find(|option| option.option_id == realization.option_id);
        let model_kind = model
            .options
            .iter()
            .find(|option| option.option_id == realization.option_id)
            .and_then(|option| option.kind.as_ref());
        let valid = option_ids.insert(realization.option_id.clone())
            && option.is_some_and(|option| {
                valid_realization(
                    realization,
                    option,
                    model_kind,
                    auxiliary,
                    &mut selectable_auxiliary,
                    &mut vision_count,
                    &mut mtp_count,
                )
            });
        if !valid {
            errors.push(anyhow::anyhow!(
                "configuration {:?} has invalid llama.cpp realization for option {:?}",
                configuration.configuration_id,
                realization.option_id
            ));
        }
    }

    if option_ids.len() != configuration.options.len()
        || configuration
            .options
            .iter()
            .any(|option| !option_ids.contains(&option.option_id))
    {
        errors.push(anyhow::anyhow!(
            "configuration {:?} requires one llama.cpp realization per model option",
            configuration.configuration_id
        ));
    }
    if vision_count > 1
        || mtp_count > 1
        || (vision_count > 0 && fixed.vision_projector.is_some())
        || (mtp_count > 0 && fixed.speculative.is_some())
        || configuration.artifact_bindings.iter().any(|binding| {
            binding
                .auxiliary_artifact_ids
                .iter()
                .any(|id| selectable_auxiliary.contains(id.as_str()))
        })
    {
        errors.push(anyhow::anyhow!(
            "configuration {:?} has ambiguous llama.cpp auxiliary realization",
            configuration.configuration_id
        ));
    }
}

#[allow(clippy::too_many_arguments)]
fn valid_realization(
    realization: &LlamaCppOptionRealization,
    option: &ModelConfigurationOption,
    model_kind: Option<&ModelOptionKind>,
    auxiliary: &BTreeSet<&str>,
    selectable_auxiliary: &mut BTreeSet<String>,
    vision_count: &mut usize,
    mtp_count: &mut usize,
) -> bool {
    match (model_kind, realization.realization.as_ref()) {
        (Some(ModelOptionKind::Vision(_)), Some(Realization::Vision(vision))) => {
            *vision_count += 1;
            let unique_auxiliary =
                selectable_auxiliary.insert(vision.auxiliary_artifact_id.clone());
            parameter_has_type(option, &vision.enabled_parameter_id, |kind| {
                matches!(kind, ModelOptionParameterType::Boolean(_))
            }) && auxiliary.contains(vision.auxiliary_artifact_id.as_str())
                && unique_auxiliary
                && valid_image_capacity(vision.image_input_capacity.as_ref())
        }
        (Some(ModelOptionKind::KvCachePrecision(_)), Some(Realization::KvCachePrecision(_))) => {
            true
        }
        (Some(ModelOptionKind::SpeculativeDecoding(kind)), Some(Realization::Mtp(mtp))) => {
            *mtp_count += 1;
            let valid_draft = match speculative_draft::llama_cpp(mtp.draft.as_ref()) {
                Some(SpeculativeDraft::Embedded) => true,
                Some(SpeculativeDraft::Artifact(artifact_id)) => {
                    auxiliary.contains(artifact_id)
                        && selectable_auxiliary.insert(artifact_id.to_owned())
                }
                None => false,
            };
            kind.method == SpeculativeDecodingMethod::Mtp as i32 && valid_draft
        }
        _ => false,
    }
}

/// Checks the precisions each KV cache precision realization offers: at most one realization sets
/// the cache types, every offered precision is one llama.cpp launches, and a quantized precision
/// needs flash attention, without which llama.cpp refuses a quantized V cache.
fn validate_kv_cache_precisions(
    configuration: &ModelEngineConfiguration,
    payload: &LlamaCppModelConfiguration,
    fixed: &dotcore_proto::dotcore::engine::llama_cpp::v1::LlamaCppFixedModelSettings,
    errors: &mut Vec<anyhow::Error>,
) {
    let option_ids = payload
        .option_realizations
        .iter()
        .filter(|realization| {
            matches!(
                realization.realization,
                Some(Realization::KvCachePrecision(_))
            )
        })
        .map(|realization| realization.option_id.as_str())
        .collect::<Vec<_>>();
    if option_ids.len() > 1 {
        errors.push(anyhow::anyhow!(
            "configuration {:?} declares KV cache precision options {option_ids:?}, but llama.cpp \
             supports one",
            configuration.configuration_id
        ));
    }

    for option_id in option_ids {
        let precisions = configuration
            .options
            .iter()
            .find(|option| option.option_id == option_id)
            .and_then(|option| {
                option
                    .parameters
                    .iter()
                    .find(|parameter| parameter.parameter_id == KV_CACHE_PRECISION_PARAMETER_ID)
            })
            .and_then(|parameter| match parameter.parameter_type.as_ref() {
                Some(ModelOptionParameterType::StringEnum(schema)) => Some(&schema.values),
                _ => None,
            });
        // The catalog reports a missing option or a precision parameter of the wrong shape.
        let Some(precisions) = precisions else {
            continue;
        };
        for precision in precisions {
            if !KV_CACHE_PRECISIONS.contains(&precision.as_str()) {
                errors.push(anyhow::anyhow!(
                    "configuration {:?} KV cache precision option {option_id:?} offers precision \
                     {precision:?}, which llama.cpp does not support; supported precisions are \
                     {KV_CACHE_PRECISIONS:?}",
                    configuration.configuration_id
                ));
            } else if precision != DEFAULT_KV_CACHE_PRECISION && !fixed.flash_attention {
                errors.push(anyhow::anyhow!(
                    "configuration {:?} KV cache precision option {option_id:?} offers quantized \
                     precision {precision:?}, which llama.cpp supports only with flash \
                     attention; enable flash attention in the fixed settings or remove the \
                     precision",
                    configuration.configuration_id
                ));
            }
        }
    }
}

fn parameter_has_type(
    option: &ModelConfigurationOption,
    parameter_id: &str,
    predicate: impl FnOnce(&ModelOptionParameterType) -> bool,
) -> bool {
    option
        .parameters
        .iter()
        .find(|parameter| parameter.parameter_id == parameter_id)
        .and_then(|parameter| parameter.parameter_type.as_ref())
        .is_some_and(predicate)
}

fn valid_image_capacity(capacity: Option<&LlamaCppImageInputCapacity>) -> bool {
    capacity.is_some_and(|capacity| {
        capacity.maximum_image_count > 0
            && capacity.maximum_encoded_bytes > 0
            && capacity.maximum_decoded_bytes > 0
            && capacity.maximum_width > 0
            && capacity.maximum_height > 0
            && capacity.maximum_pixels > 0
            && capacity.fixed_visual_tokens > 0
            && capacity.fixed_detail == LlamaCppImageDetail::Auto as i32
            && !capacity.supported_media_types.is_empty()
            && capacity.supported_media_types.iter().all(|value| {
                matches!(
                    LlamaCppImageMediaType::try_from(*value),
                    Ok(LlamaCppImageMediaType::Jpeg | LlamaCppImageMediaType::Png)
                )
            })
    })
}

/// Reports a sampling value outside the finite range accepted by a model declaration.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LlamaCppSamplingError {
    #[error(
        "llama.cpp sampling parameter {parameter} must be {range} and either zero or within the finite normal 32-bit float range; received {value}"
    )]
    InvalidValue {
        parameter: &'static str,
        value: f64,
        range: &'static str,
    },
}

/// Accepts a shared parameter bounded by llama.cpp's own limit. The bound on the total context is
/// high enough that a large pool exceeds memory before it exceeds the bound.
fn is_bounded(definition: &EngineParameterDefinition, parameter_id: &str, maximum: u64) -> bool {
    definition.parameter_id == parameter_id
        && matches!(
            definition.parameter_type.as_ref(),
            Some(ParameterType::UnsignedInteger(range)) if range.maximum <= maximum
        )
}

fn is_prompt_cache_memory(definition: &EngineParameterDefinition) -> bool {
    definition.parameter_id == PROMPT_CACHE_MEMORY_PARAMETER_ID
        && definition.display_name == "Prompt cache memory"
        && matches!(
            definition.parameter_type.as_ref(),
            Some(ParameterType::UnsignedInteger(range))
                if range.minimum == 0 && range.maximum == 65_536 && range.unit == "MiB"
        )
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, path::Path};

    use dotcore_catalog::{Catalog, OciArtifactPolicy, test_support::vendor_hardware_class};
    use dotcore_proto::dotcore::{
        catalog::v1::{
            EngineParameterDefinition, HuggingFaceArtifactSource, HuggingFaceFileArtifact,
            HuggingFaceSnapshotArtifact, HuggingFaceSnapshotFile, Model, ModelArtifact,
            ModelArtifactBinding, ModelAuxiliaryArtifact, ModelConfigurationOption,
            ModelEngineConfiguration, SpeculativeDecodingMethod, SpeculativeDecodingModelOption,
            VisionModelOption, engine_parameter_definition::ParameterType,
            hugging_face_artifact_source::Content as HuggingFaceContent, model_option,
        },
        engine::{
            llama_cpp::v1::{
                LlamaCppEmbeddedDraft, LlamaCppMtpOptionRealization, LlamaCppOptionRealization,
                LlamaCppSpeculativeDraftSource, llama_cpp_option_realization::Realization,
                llama_cpp_speculative_draft_source::Source,
            },
            v1::{EngineKind, engine_model_configuration::Configuration},
        },
    };

    use super::{
        LlamaCppSamplingError, valid_realization, validate_engine_release,
        validate_hardware_support, validate_model_configuration, validate_sampling_settings,
        validate_total_context_default,
    };
    use crate::model::engine::declaration_validator;

    #[test]
    fn sampling_validation_accepts_absent_values_and_valid_boundaries() {
        use dotcore_proto::dotcore::engine::llama_cpp::v1::LlamaCppSamplingSettings;

        assert!(validate_sampling_settings(&LlamaCppSamplingSettings::default()).is_ok());
        assert!(
            validate_sampling_settings(&LlamaCppSamplingSettings {
                min_p: Some(0.0),
                repeat_penalty: Some(1.0),
                temperature: Some(0.0),
                top_p: Some(1.0),
            })
            .is_ok()
        );
        assert!(
            validate_sampling_settings(&LlamaCppSamplingSettings {
                min_p: Some(f64::from(f32::MIN_POSITIVE)),
                repeat_penalty: Some(f64::from(f32::MAX)),
                temperature: Some(f64::from(f32::MAX)),
                top_p: Some(1.0),
            })
            .is_ok()
        );
    }

    #[test]
    fn sampling_validation_rejects_invalid_values_with_parameter_context() {
        use dotcore_proto::dotcore::engine::llama_cpp::v1::LlamaCppSamplingSettings;

        for (sampling, expected_parameter, expected_value) in [
            (
                LlamaCppSamplingSettings {
                    min_p: Some(-0.01),
                    ..Default::default()
                },
                "min-p",
                -0.01,
            ),
            (
                LlamaCppSamplingSettings {
                    min_p: Some(1.01),
                    ..Default::default()
                },
                "min-p",
                1.01,
            ),
            (
                LlamaCppSamplingSettings {
                    top_p: Some(1.01),
                    ..Default::default()
                },
                "top-p",
                1.01,
            ),
            (
                LlamaCppSamplingSettings {
                    repeat_penalty: Some(0.0),
                    ..Default::default()
                },
                "repeat-penalty",
                0.0,
            ),
            (
                LlamaCppSamplingSettings {
                    repeat_penalty: Some(f64::INFINITY),
                    ..Default::default()
                },
                "repeat-penalty",
                f64::INFINITY,
            ),
            (
                LlamaCppSamplingSettings {
                    temperature: Some(-1.0),
                    ..Default::default()
                },
                "temperature",
                -1.0,
            ),
            (
                LlamaCppSamplingSettings {
                    temperature: Some(f64::NAN),
                    ..Default::default()
                },
                "temperature",
                f64::NAN,
            ),
            (
                LlamaCppSamplingSettings {
                    repeat_penalty: Some(1e300),
                    ..Default::default()
                },
                "repeat-penalty",
                1e300,
            ),
            (
                LlamaCppSamplingSettings {
                    temperature: Some(1e-300),
                    ..Default::default()
                },
                "temperature",
                1e-300,
            ),
            (
                LlamaCppSamplingSettings {
                    top_p: Some(f64::from(f32::MIN_POSITIVE) / 2.0),
                    ..Default::default()
                },
                "top-p",
                f64::from(f32::MIN_POSITIVE) / 2.0,
            ),
        ] {
            let LlamaCppSamplingError::InvalidValue {
                parameter, value, ..
            } = validate_sampling_settings(&sampling).expect_err("invalid sampling value");
            assert_eq!(parameter, expected_parameter);
            assert_eq!(value.to_bits(), expected_value.to_bits());
        }
    }

    #[test]
    fn accepts_only_the_shared_parameters_then_the_prompt_cache_within_llama_cpp_limits() {
        let release = development_catalog()
            .list_engine_releases()
            .into_iter()
            .find(|release| release.engine_kind == EngineKind::LlamaCpp as i32)
            .expect("llama.cpp release");
        assert!(validate_engine_release(&release).is_empty());
        let rejection = |edit: &dyn Fn(&mut Vec<EngineParameterDefinition>)| {
            let mut release = release.clone();
            edit(
                &mut release
                    .parameter_schema
                    .as_mut()
                    .expect("parameter schema")
                    .parameters,
            );
            validate_engine_release(&release)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        };
        let maximum = |parameters: &mut Vec<EngineParameterDefinition>, index: usize, value| {
            if let Some(ParameterType::UnsignedInteger(range)) =
                parameters[index].parameter_type.as_mut()
            {
                range.maximum = value;
            }
        };
        let expected = [
            "llama.cpp engine release requires the approved parameter schema: total-context and \
             context-per-request up to 16777216 tokens, parallel-requests up to 64, and \
             prompt-cache-memory-mib, in that order"
                .to_owned(),
        ];

        assert_eq!(rejection(&|parameters| parameters.truncate(3)), expected);
        assert_eq!(rejection(&|parameters| parameters.swap(0, 1)), expected);
        assert_eq!(
            rejection(&|parameters| parameters[0].parameter_id = "other".to_owned()),
            expected
        );
        assert_eq!(
            rejection(&|parameters| maximum(parameters, 0, 131_072)),
            Vec::<String>::new()
        );
        assert_eq!(
            rejection(&|parameters| maximum(parameters, 0, 16_777_217)),
            expected
        );
        assert_eq!(
            rejection(&|parameters| maximum(parameters, 1, 16_777_217)),
            expected
        );
        assert_eq!(
            rejection(&|parameters| maximum(parameters, 2, 65)),
            expected
        );
        assert_eq!(
            rejection(&|parameters| parameters[3].display_name = "Cache RAM".to_owned()),
            expected
        );
        assert_eq!(
            rejection(&|parameters| {
                let mut extra = parameters[3].clone();
                extra.parameter_id = "extra".to_owned();
                parameters.push(extra);
            }),
            expected
        );
    }

    #[test]
    fn rejects_invalid_vision_and_ambiguous_auxiliary_realizations() {
        let catalog = development_catalog();
        let mut model = catalog
            .list_model_releases()
            .into_iter()
            .find(|model| model.model_id == "fixture-vision-model")
            .expect("fixture vision model");
        let mut configuration = model.configurations.remove(0);
        let payload = configuration
            .engine_configuration
            .as_mut()
            .and_then(|payload| payload.configuration.as_mut())
            .and_then(|payload| match payload {
                Configuration::LlamaCpp(payload) => Some(payload),
                Configuration::Vllm(_) | Configuration::Ninfer(_) | Configuration::Strata(_) => {
                    None
                }
            })
            .expect("llama.cpp payload");
        let vision = payload.option_realizations[0]
            .realization
            .as_mut()
            .map(|realization| match realization {
                Realization::Vision(vision) => vision,
                Realization::Mtp(_) | Realization::KvCachePrecision(_) => {
                    panic!("vision realization")
                }
            })
            .expect("vision realization");
        vision.enabled_parameter_id = "missing".to_owned();
        configuration.artifact_bindings[0]
            .auxiliary_artifact_ids
            .push(vision.auxiliary_artifact_id.clone());

        let errors = validate_model_configuration(&model, &configuration)
            .into_iter()
            .map(|error| error.to_string())
            .collect::<Vec<_>>();

        assert!(
            errors
                .iter()
                .any(|error| error.contains("invalid llama.cpp realization")),
            "missing realization error: {errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|error| error.contains("ambiguous llama.cpp auxiliary realization")),
            "missing ambiguity error: {errors:?}"
        );
    }

    #[test]
    fn rejects_invalid_mtp_realization() {
        let catalog = development_catalog();
        let mut model = catalog
            .list_model_releases()
            .into_iter()
            .find(|model| model.model_id == "fixture-vision-model")
            .expect("fixture vision model");
        let mut configuration = model.configurations.remove(0);
        let payload = configuration
            .engine_configuration
            .as_mut()
            .and_then(|payload| payload.configuration.as_mut())
            .and_then(|payload| match payload {
                Configuration::LlamaCpp(payload) => Some(payload),
                Configuration::Vllm(_) | Configuration::Ninfer(_) | Configuration::Strata(_) => {
                    None
                }
            })
            .expect("llama.cpp payload");
        let mtp = payload.option_realizations[1]
            .realization
            .as_mut()
            .map(|realization| match realization {
                Realization::Mtp(mtp) => mtp,
                Realization::Vision(_) | Realization::KvCachePrecision(_) => {
                    panic!("MTP realization")
                }
            })
            .expect("MTP realization");
        mtp.draft = artifact_draft("missing");

        let errors = validate_model_configuration(&model, &configuration);

        assert!(
            errors
                .iter()
                .any(|error| error.to_string().contains("invalid llama.cpp realization")),
            "missing MTP realization error: {errors:?}"
        );
    }

    #[test]
    fn mtp_realization_requires_an_mtp_speculative_option() {
        let realization = LlamaCppOptionRealization {
            option_id: "mtp".to_owned(),
            realization: Some(Realization::Mtp(LlamaCppMtpOptionRealization {
                draft: artifact_draft("draft"),
            })),
        };
        let valid = |method: SpeculativeDecodingMethod| {
            valid_realization(
                &realization,
                &ModelConfigurationOption::default(),
                Some(&model_option::Kind::SpeculativeDecoding(
                    SpeculativeDecodingModelOption {
                        method: method as i32,
                    },
                )),
                &BTreeSet::from(["draft"]),
                &mut BTreeSet::new(),
                &mut 0,
                &mut 0,
            )
        };

        assert!(valid(SpeculativeDecodingMethod::Mtp));
        assert!(!valid(SpeculativeDecodingMethod::Unspecified));
    }

    #[test]
    fn mtp_draft_source_is_explicit_embedded_or_a_declared_artifact() {
        use dotcore_proto::dotcore::{
            catalog::v1::ModelOption,
            engine::{
                llama_cpp::v1::{
                    LlamaCppFixedModelSettings, LlamaCppModelConfiguration,
                    LlamaCppSpeculativeDraftSource, LlamaCppSpeculativeMethod,
                    LlamaCppSpeculativeSettings,
                },
                v1::EngineModelConfiguration,
            },
        };

        let mtp_option = |draft: Option<LlamaCppSpeculativeDraftSource>| {
            let model = Model {
                options: vec![ModelOption {
                    option_id: "mtp".to_owned(),
                    kind: Some(model_option::Kind::SpeculativeDecoding(
                        SpeculativeDecodingModelOption {
                            method: SpeculativeDecodingMethod::Mtp as i32,
                        },
                    )),
                    ..Default::default()
                }],
                ..Default::default()
            };
            let configuration = ModelEngineConfiguration {
                configuration_id: "local".to_owned(),
                options: vec![ModelConfigurationOption {
                    option_id: "mtp".to_owned(),
                    ..Default::default()
                }],
                engine_configuration: Some(EngineModelConfiguration {
                    configuration: Some(Configuration::LlamaCpp(LlamaCppModelConfiguration {
                        fixed_settings: Some(Default::default()),
                        option_realizations: vec![LlamaCppOptionRealization {
                            option_id: "mtp".to_owned(),
                            realization: Some(Realization::Mtp(LlamaCppMtpOptionRealization {
                                draft,
                            })),
                        }],
                    })),
                    ..Default::default()
                }),
                ..Default::default()
            };
            (model, configuration)
        };
        let errors = |model: &Model, configuration: &ModelEngineConfiguration| {
            validate_model_configuration(model, configuration)
                .iter()
                .map(|error| error.to_string())
                .filter(|error| error.contains("realization") || error.contains("auxiliary"))
                .collect::<Vec<_>>()
        };

        let (model, configuration) = mtp_option(embedded_draft());
        assert_eq!(errors(&model, &configuration), Vec::<String>::new());

        // Fixed speculative settings of an embedded head carry no auxiliary requirement either, and
        // need a supported method and a draft source.
        let fixed_speculative =
            |method: LlamaCppSpeculativeMethod, draft: Option<LlamaCppSpeculativeDraftSource>| {
                let (model, mut configuration) = mtp_option(None);
                let Some(Configuration::LlamaCpp(payload)) = configuration
                    .engine_configuration
                    .as_mut()
                    .and_then(|payload| payload.configuration.as_mut())
                else {
                    panic!("llama.cpp payload");
                };
                payload.option_realizations.clear();
                configuration.options.clear();
                payload.fixed_settings = Some(LlamaCppFixedModelSettings {
                    speculative: Some(LlamaCppSpeculativeSettings {
                        method: method as i32,
                        draft,
                        draft_maximum: 2,
                    }),
                    ..Default::default()
                });
                validate_model_configuration(&model, &configuration)
                    .iter()
                    .map(|error| error.to_string())
                    .filter(|error| error.contains("speculative") || error.contains("auxiliary"))
                    .collect::<Vec<_>>()
            };
        assert_eq!(
            fixed_speculative(LlamaCppSpeculativeMethod::Mtp, embedded_draft()),
            Vec::<String>::new()
        );
        assert_eq!(
            fixed_speculative(LlamaCppSpeculativeMethod::Mtp, None),
            ["configuration \"local\" has no fixed llama.cpp speculative draft source"]
        );
        assert_eq!(
            fixed_speculative(LlamaCppSpeculativeMethod::Unspecified, embedded_draft()),
            ["configuration \"local\" has an unsupported fixed llama.cpp speculative method"]
        );

        // A draft artifact the model does not declare, a missing source and an empty artifact id
        // are rejected.
        let invalid =
            ["configuration \"local\" has invalid llama.cpp realization for option \"mtp\""];
        for draft in [
            artifact_draft("missing"),
            None,
            Some(LlamaCppSpeculativeDraftSource::default()),
            artifact_draft(""),
        ] {
            let (model, configuration) = mtp_option(draft.clone());
            assert_eq!(errors(&model, &configuration), invalid, "draft {draft:?}");
        }
    }

    #[test]
    fn kv_cache_precision_realization_accepts_f16_and_q8_0_with_flash_attention() {
        assert_eq!(
            kv_cache_errors(&kv_cache_declaration(&["f16", "q8_0"], true)),
            Vec::<String>::new()
        );
        assert_eq!(
            kv_cache_errors(&kv_cache_declaration(&["f16"], false)),
            Vec::<String>::new()
        );
    }

    #[test]
    fn kv_cache_precision_realization_rejects_unsupported_precisions() {
        assert_eq!(
            kv_cache_errors(&kv_cache_declaration(&["f16", "q4_0", "bf16"], true)),
            [
                "configuration \"local\" KV cache precision option \"kv-cache\" offers precision \
                 \"q4_0\", which llama.cpp does not support; supported precisions are \
                 [\"f16\", \"q8_0\"]",
                "configuration \"local\" KV cache precision option \"kv-cache\" offers precision \
                 \"bf16\", which llama.cpp does not support; supported precisions are \
                 [\"f16\", \"q8_0\"]",
            ]
        );
    }

    #[test]
    fn kv_cache_precision_realization_rejects_q8_0_without_flash_attention() {
        assert_eq!(
            kv_cache_errors(&kv_cache_declaration(&["f16", "q8_0"], false)),
            [
                "configuration \"local\" KV cache precision option \"kv-cache\" offers quantized \
                 precision \"q8_0\", which llama.cpp supports only with flash attention; enable \
                 flash attention in the fixed settings or remove the precision"
            ]
        );
    }

    #[test]
    fn kv_cache_precision_realization_requires_its_option_kind_and_one_realization() {
        let (mut model, configuration) = kv_cache_declaration(&["f16", "q8_0"], true);
        model.options[0].kind = Some(model_option::Kind::Vision(VisionModelOption {}));
        assert_eq!(
            kv_cache_errors(&(model, configuration)),
            ["configuration \"local\" has invalid llama.cpp realization for option \"kv-cache\""]
        );

        let (mut model, mut configuration) = kv_cache_declaration(&["f16", "q8_0"], true);
        let mut second_option = model.options[0].clone();
        second_option.option_id = "kv-cache-2".to_owned();
        model.options.push(second_option);
        let mut second = configuration.options[0].clone();
        second.option_id = "kv-cache-2".to_owned();
        configuration.options.push(second);
        let realizations = &mut llama_cpp_payload(&mut configuration).option_realizations;
        let mut second = realizations[0].clone();
        second.option_id = "kv-cache-2".to_owned();
        realizations.push(second);
        assert_eq!(
            kv_cache_errors(&(model, configuration)),
            [
                "configuration \"local\" declares KV cache precision options [\"kv-cache\", \
                 \"kv-cache-2\"], but llama.cpp supports one"
            ]
        );
    }

    #[test]
    fn a_single_micro_batch_configuration_must_declare_its_own_maximum_context() {
        use dotcore_proto::dotcore::engine::{
            llama_cpp::v1::{LlamaCppFixedModelSettings, LlamaCppModelConfiguration},
            v1::EngineModelConfiguration,
        };

        let problems = |single_micro_batch: bool, maximum_context_tokens: Option<u32>| {
            let configuration = ModelEngineConfiguration {
                configuration_id: "local".to_owned(),
                maximum_context_tokens,
                engine_configuration: Some(EngineModelConfiguration {
                    configuration: Some(Configuration::LlamaCpp(LlamaCppModelConfiguration {
                        fixed_settings: Some(LlamaCppFixedModelSettings {
                            single_micro_batch,
                            ..Default::default()
                        }),
                        ..Default::default()
                    })),
                    ..Default::default()
                }),
                ..Default::default()
            };
            validate_model_configuration(&Model::default(), &configuration)
                .iter()
                .map(|error| error.to_string())
                .filter(|error| error.contains("micro-batch"))
                .collect::<Vec<_>>()
        };

        assert_eq!(
            problems(true, None),
            [
                "configuration \"local\" processes prompts in a single llama.cpp micro-batch and must declare its own maximum context"
            ]
        );
        assert_eq!(problems(true, Some(16_384)), Vec::<String>::new());
        assert_eq!(problems(false, None), Vec::<String>::new());
    }

    #[test]
    fn a_single_micro_batch_maximum_context_must_be_a_valid_total_context() {
        use dotcore_proto::dotcore::{
            catalog::v1::{
                EngineParameterSchema, EngineRelease, UnsignedIntegerEngineParameterSchema,
            },
            engine::{
                llama_cpp::v1::{LlamaCppFixedModelSettings, LlamaCppModelConfiguration},
                v1::EngineModelConfiguration,
            },
        };

        let model = Model {
            maximum_context_tokens: 262_144,
            ..Default::default()
        };
        let engine = EngineRelease {
            parameter_schema: Some(EngineParameterSchema {
                parameters: vec![EngineParameterDefinition {
                    parameter_id: "total-context".to_owned(),
                    parameter_type: Some(ParameterType::UnsignedInteger(
                        UnsignedIntegerEngineParameterSchema {
                            minimum: 1024,
                            maximum: 8192,
                            ..Default::default()
                        },
                    )),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let problem = |single_micro_batch: bool, maximum_context_tokens: u32| {
            let configuration = ModelEngineConfiguration {
                configuration_id: "local".to_owned(),
                maximum_context_tokens: Some(maximum_context_tokens),
                engine_configuration: Some(EngineModelConfiguration {
                    configuration: Some(Configuration::LlamaCpp(LlamaCppModelConfiguration {
                        fixed_settings: Some(LlamaCppFixedModelSettings {
                            single_micro_batch,
                            ..Default::default()
                        }),
                        ..Default::default()
                    })),
                    ..Default::default()
                }),
                ..Default::default()
            };
            validate_total_context_default(&model, &configuration, &engine)
                .map(|error| error.to_string())
        };
        let outside = |maximum: u32| {
            Some(format!(
                "configuration \"local\" processes prompts in a single llama.cpp micro-batch, so its total context defaults to its maximum context of {maximum} tokens, outside the 1024 to 8192 tokens its engine release accepts; declare a maximum context within that range"
            ))
        };

        assert_eq!(problem(true, 0), outside(0));
        assert_eq!(problem(true, 512), outside(512));
        assert_eq!(problem(true, 8193), outside(8193));
        assert_eq!(problem(true, 1024), None);
        assert_eq!(problem(true, 8192), None);
        assert_eq!(problem(false, 16_384), None);
    }

    #[test]
    fn accepts_split_model_snapshots_and_rejects_other_artifact_shapes() {
        let snapshot = |paths: &[&str]| {
            HuggingFaceContent::Snapshot(HuggingFaceSnapshotArtifact {
                files: paths
                    .iter()
                    .map(|path| HuggingFaceSnapshotFile {
                        path: (*path).to_owned(),
                        provenance_digest: "2".repeat(64),
                    })
                    .collect(),
            })
        };
        let file = |path: &str| {
            HuggingFaceContent::File(HuggingFaceFileArtifact {
                path: path.to_owned(),
                provenance_digest: "2".repeat(64),
            })
        };
        let errors = |model_content: HuggingFaceContent, auxiliary_content: HuggingFaceContent| {
            let mut model = Model {
                artifacts: vec![ModelArtifact {
                    artifact_id: "weights".to_owned(),
                    source: Some(HuggingFaceArtifactSource {
                        content: Some(model_content),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                auxiliary_artifacts: vec![ModelAuxiliaryArtifact {
                    auxiliary_artifact_id: "draft".to_owned(),
                    source: Some(HuggingFaceArtifactSource {
                        content: Some(auxiliary_content),
                        ..Default::default()
                    }),
                }],
                ..Default::default()
            };
            model.configurations = vec![ModelEngineConfiguration {
                configuration_id: "local".to_owned(),
                artifact_bindings: vec![ModelArtifactBinding {
                    binding_id: "default".to_owned(),
                    model_artifact_id: "weights".to_owned(),
                    auxiliary_artifact_ids: vec!["draft".to_owned()],
                }],
                ..Default::default()
            }];
            validate_model_configuration(&model, &model.configurations[0])
                .iter()
                .map(|error| format!("{error:#}"))
                .filter(|error| error.contains("artifact"))
                .collect::<Vec<_>>()
        };
        let split = ["Q4/m-00001-of-00002.gguf", "Q4/m-00002-of-00002.gguf"];

        assert_eq!(
            errors(snapshot(&split), file("MTP/mtp.gguf")),
            Vec::<String>::new()
        );
        assert_eq!(
            errors(file("m.gguf"), file("mtp.gguf")),
            Vec::<String>::new()
        );
        assert_eq!(
            errors(
                snapshot(&["config.json", "model.safetensors"]),
                file("mtp.gguf")
            ),
            [
                "configuration \"local\" binds model artifact \"weights\", which is not a valid \
                 split GGUF set: split GGUF file \"config.json\" is not named \
                 <prefix>-NNNNN-of-NNNNN.gguf with five-digit shard numbers"
            ]
        );
        assert_eq!(
            errors(file("m.safetensors"), snapshot(&split)),
            [
                "configuration \"local\" requires model artifact \"weights\" to be a GGUF file \
                 or a split GGUF set for llama.cpp",
                "configuration \"local\" requires auxiliary artifact \"draft\" to be a single \
                 GGUF file for llama.cpp",
            ]
        );
    }

    #[test]
    fn hardware_classes_must_select_nvidia_or_apple() {
        let configuration = ModelEngineConfiguration {
            configuration_id: "synthetic-config".to_owned(),
            ..Default::default()
        };
        let nvidia = vendor_hardware_class("nvidia");
        let apple = vendor_hardware_class("apple");
        let mut mixed = vendor_hardware_class("mixed");
        mixed.selector.as_mut().expect("selector").accelerators =
            vec!["nvidia".to_owned(), "amd".to_owned()];
        let mut any = vendor_hardware_class("any");
        any.selector
            .as_mut()
            .expect("selector")
            .accelerators
            .clear();

        assert!(validate_hardware_support(&configuration, &[&nvidia, &apple]).is_empty());
        assert_eq!(
            validate_hardware_support(&configuration, &[&nvidia, &mixed, &any])
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            [
                "configuration \"synthetic-config\" hardware class \"mixed\" must select nvidia or apple accelerators for llama.cpp",
                "configuration \"synthetic-config\" hardware class \"any\" must select nvidia or apple accelerators for llama.cpp",
            ]
        );
    }

    /// Model and llama.cpp configuration offering the given KV cache precisions, f16 by default.
    fn kv_cache_declaration(
        precisions: &[&str],
        flash_attention: bool,
    ) -> (Model, ModelEngineConfiguration) {
        use dotcore_proto::dotcore::{
            catalog::v1::{
                KvCachePrecisionModelOption, ModelOption, ModelOptionParameterDefinition,
                ModelOptionParameterValue, ModelOptionValues, StringEnumModelOptionParameterSchema,
                model_option_parameter_definition::ParameterType as OptionParameterType,
                model_option_parameter_value::Value,
            },
            engine::{
                llama_cpp::v1::{
                    LlamaCppFixedModelSettings, LlamaCppKvCachePrecisionOptionRealization,
                    LlamaCppModelConfiguration,
                },
                v1::EngineModelConfiguration,
            },
        };

        let model = Model {
            options: vec![ModelOption {
                option_id: "kv-cache".to_owned(),
                display_name: "KV cache precision".to_owned(),
                kind: Some(model_option::Kind::KvCachePrecision(
                    KvCachePrecisionModelOption {},
                )),
            }],
            ..Default::default()
        };
        let configuration = ModelEngineConfiguration {
            configuration_id: "local".to_owned(),
            options: vec![ModelConfigurationOption {
                option_id: "kv-cache".to_owned(),
                parameters: vec![ModelOptionParameterDefinition {
                    parameter_id: "precision".to_owned(),
                    display_name: "Precision".to_owned(),
                    parameter_type: Some(OptionParameterType::StringEnum(
                        StringEnumModelOptionParameterSchema {
                            values: precisions.iter().map(|&value| value.to_owned()).collect(),
                        },
                    )),
                }],
                default_values: Some(ModelOptionValues {
                    option_id: "kv-cache".to_owned(),
                    values: vec![ModelOptionParameterValue {
                        parameter_id: "precision".to_owned(),
                        value: Some(Value::String("f16".to_owned())),
                    }],
                }),
                ..Default::default()
            }],
            engine_configuration: Some(EngineModelConfiguration {
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
                ..Default::default()
            }),
            ..Default::default()
        };
        (model, configuration)
    }

    fn kv_cache_errors((model, configuration): &(Model, ModelEngineConfiguration)) -> Vec<String> {
        validate_model_configuration(model, configuration)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    fn llama_cpp_payload(
        configuration: &mut ModelEngineConfiguration,
    ) -> &mut dotcore_proto::dotcore::engine::llama_cpp::v1::LlamaCppModelConfiguration {
        match configuration
            .engine_configuration
            .as_mut()
            .and_then(|payload| payload.configuration.as_mut())
        {
            Some(Configuration::LlamaCpp(payload)) => payload,
            _ => panic!("llama.cpp payload"),
        }
    }

    fn development_catalog() -> Catalog {
        Catalog::load_with_policy(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/development-catalog"),
            OciArtifactPolicy::Development,
            declaration_validator(),
        )
        .expect("development fixture catalog")
    }

    fn embedded_draft() -> Option<LlamaCppSpeculativeDraftSource> {
        Some(LlamaCppSpeculativeDraftSource {
            source: Some(Source::Embedded(LlamaCppEmbeddedDraft {})),
        })
    }

    fn artifact_draft(artifact_id: &str) -> Option<LlamaCppSpeculativeDraftSource> {
        Some(LlamaCppSpeculativeDraftSource {
            source: Some(Source::AuxiliaryArtifactId(artifact_id.to_owned())),
        })
    }
}
