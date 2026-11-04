//! llama.cpp option-effect materialization.

use std::collections::{BTreeMap, BTreeSet};

use dotcore_catalog::options::{
    SPECULATIVE_TOKEN_COUNT_PARAMETER_ID, boolean_value, unsigned_value,
};
use dotcore_proto::dotcore::{
    catalog::v1::{
        Model, ModelArtifactBinding, ModelEngineConfiguration, ModelImageDetail,
        ModelImageInputCapacity, ModelImageMediaType, ModelInputModality, ModelOptionValues,
    },
    engine::{
        llama_cpp::v1::{
            LlamaCppImageDetail, LlamaCppImageMediaType, LlamaCppModelConfiguration,
            LlamaCppOptionRealization, LlamaCppResolvedModelState, LlamaCppSpeculativeMethod,
            LlamaCppSpeculativeSettings, LlamaCppVisionProjectorSettings,
            llama_cpp_option_realization::Realization,
        },
        v1::{
            EngineKind, ResolvedEngineModelState, engine_model_configuration::Configuration,
            resolved_engine_model_state::State,
        },
    },
};

use crate::model::engine::{realization::ResolvedEngineSelection, speculative_draft};

pub(super) const SEMANTICS_VERSION: u32 = 2;

pub(crate) fn realize_model(
    _model: &Model,
    configuration: &ModelEngineConfiguration,
    binding: &ModelArtifactBinding,
    option_values: &[ModelOptionValues],
) -> anyhow::Result<ResolvedEngineSelection> {
    let payload = llama_cpp_payload(configuration)
        .ok_or_else(|| anyhow::anyhow!("configuration has no llama.cpp payload"))?;
    let mut fixed_settings = payload
        .fixed_settings
        .clone()
        .ok_or_else(|| anyhow::anyhow!("configuration has no fixed llama.cpp settings"))?;
    let mut auxiliary_artifact_ids = binding.auxiliary_artifact_ids.clone();
    let mut selected_auxiliary = auxiliary_artifact_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut input_capabilities = configuration
        .input_capabilities
        .clone()
        .ok_or_else(|| anyhow::anyhow!("configuration has no input capabilities"))?;
    let values = option_values
        .iter()
        .map(|values| (values.option_id.as_str(), values))
        .collect::<BTreeMap<_, _>>();

    for realization in &payload.option_realizations {
        let values = values.get(realization.option_id.as_str()).ok_or_else(|| {
            anyhow::anyhow!("resolved values omit option {:?}", realization.option_id)
        })?;
        match realization.realization.as_ref() {
            Some(Realization::Vision(vision)) => {
                if !boolean_value(values, &vision.enabled_parameter_id).ok_or_else(|| {
                    anyhow::anyhow!("vision option has no resolved boolean enable parameter")
                })? {
                    continue;
                }
                add_auxiliary(
                    &vision.auxiliary_artifact_id,
                    &mut selected_auxiliary,
                    &mut auxiliary_artifact_ids,
                );
                fixed_settings.vision_projector = Some(LlamaCppVisionProjectorSettings {
                    auxiliary_artifact_id: vision.auxiliary_artifact_id.clone(),
                });
                if !input_capabilities
                    .supported_modalities
                    .contains(&(ModelInputModality::Image as i32))
                {
                    input_capabilities
                        .supported_modalities
                        .push(ModelInputModality::Image as i32);
                }
                input_capabilities.image_input_capacity = vision
                    .image_input_capacity
                    .as_ref()
                    .map(public_image_capacity)
                    .transpose()?;
            }
            Some(Realization::Mtp(mtp)) => {
                let draft_maximum = unsigned_value(values, SPECULATIVE_TOKEN_COUNT_PARAMETER_ID)
                    .and_then(|value| u32::try_from(value).ok())
                    .ok_or_else(|| anyhow::anyhow!("MTP option has no valid token count"))?;
                let draft = speculative_draft::llama_cpp(mtp.draft.as_ref())
                    .ok_or_else(|| anyhow::anyhow!("MTP option has no draft source"))?;
                if draft_maximum == 0 {
                    continue;
                }
                if let Some(artifact_id) = draft.artifact_id() {
                    add_auxiliary(
                        artifact_id,
                        &mut selected_auxiliary,
                        &mut auxiliary_artifact_ids,
                    );
                }
                fixed_settings.speculative = Some(LlamaCppSpeculativeSettings {
                    method: LlamaCppSpeculativeMethod::Mtp as i32,
                    draft: mtp.draft.clone(),
                    draft_maximum,
                });
            }
            // The plan launches the resolved precision; it adds no artifact or fixed setting.
            Some(Realization::KvCachePrecision(_)) => {}
            None => anyhow::bail!(
                "option {:?} has no llama.cpp realization",
                realization.option_id
            ),
        }
    }

    Ok(ResolvedEngineSelection {
        state: ResolvedEngineModelState {
            engine_kind: EngineKind::LlamaCpp as i32,
            semantics_version: SEMANTICS_VERSION,
            state: Some(State::LlamaCpp(LlamaCppResolvedModelState {
                fixed_settings: Some(fixed_settings),
                option_realizations: payload.option_realizations.clone(),
            })),
        },
        auxiliary_artifact_ids,
        input_capabilities,
    })
}

fn public_image_capacity(
    capacity: &dotcore_proto::dotcore::engine::llama_cpp::v1::LlamaCppImageInputCapacity,
) -> anyhow::Result<ModelImageInputCapacity> {
    let supported_media_types = capacity
        .supported_media_types
        .iter()
        .map(|value| match LlamaCppImageMediaType::try_from(*value) {
            Ok(LlamaCppImageMediaType::Jpeg) => Ok(ModelImageMediaType::Jpeg as i32),
            Ok(LlamaCppImageMediaType::Png) => Ok(ModelImageMediaType::Png as i32),
            Ok(LlamaCppImageMediaType::Unspecified) | Err(_) => {
                anyhow::bail!("llama.cpp image realization has an unsupported media type")
            }
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let fixed_detail = match LlamaCppImageDetail::try_from(capacity.fixed_detail) {
        Ok(LlamaCppImageDetail::Auto) => ModelImageDetail::Auto as i32,
        Ok(LlamaCppImageDetail::Unspecified) | Err(_) => {
            anyhow::bail!("llama.cpp image realization has an unsupported detail")
        }
    };
    Ok(ModelImageInputCapacity {
        maximum_image_count: capacity.maximum_image_count,
        supported_media_types,
        maximum_encoded_bytes: capacity.maximum_encoded_bytes,
        maximum_decoded_bytes: capacity.maximum_decoded_bytes,
        maximum_width: capacity.maximum_width,
        maximum_height: capacity.maximum_height,
        maximum_pixels: capacity.maximum_pixels,
        fixed_visual_tokens: capacity.fixed_visual_tokens,
        fixed_detail,
    })
}

/// Sets of auxiliary artifacts the configuration's options can add, one set per combination.
///
/// Validation requires one realization per option, each switched on or off by its resolved values,
/// so the combinations are every subset of the realization artifacts, at most four of them.
pub(crate) fn selectable_auxiliary_artifact_sets(
    configuration: &ModelEngineConfiguration,
) -> Vec<BTreeSet<String>> {
    let Some(payload) = llama_cpp_payload(configuration) else {
        return vec![BTreeSet::new()];
    };
    let selectable = payload
        .option_realizations
        .iter()
        .filter_map(realization_auxiliary_artifact)
        .collect::<Vec<_>>();
    let mut sets = vec![BTreeSet::new()];
    for artifact in selectable {
        let mut combinations = Vec::new();
        for set in &sets {
            let mut combination = set.clone();
            combination.insert(artifact.clone());
            combinations.push(combination);
        }
        sets.extend(combinations);
    }
    sets
}

/// Reads the llama.cpp payload of a configuration, or none for another engine kind.
fn llama_cpp_payload(
    configuration: &ModelEngineConfiguration,
) -> Option<&LlamaCppModelConfiguration> {
    configuration
        .engine_configuration
        .as_ref()
        .and_then(|payload| payload.configuration.as_ref())
        .and_then(|payload| match payload {
            Configuration::LlamaCpp(payload) => Some(payload),
            Configuration::Vllm(_) | Configuration::Ninfer(_) => None,
        })
}

/// Reads the auxiliary artifact one option realization adds when it is enabled, if any.
fn realization_auxiliary_artifact(realization: &LlamaCppOptionRealization) -> Option<String> {
    match realization.realization.as_ref()? {
        Realization::Vision(vision) => Some(vision.auxiliary_artifact_id.clone()),
        Realization::Mtp(mtp) => speculative_draft::llama_cpp(mtp.draft.as_ref())?
            .artifact_id()
            .map(str::to_owned),
        Realization::KvCachePrecision(_) => None,
    }
}

fn add_auxiliary(
    artifact_id: &str,
    selected_auxiliary: &mut BTreeSet<String>,
    auxiliary_artifact_ids: &mut Vec<String>,
) {
    if selected_auxiliary.insert(artifact_id.to_owned()) {
        auxiliary_artifact_ids.push(artifact_id.to_owned());
    }
}
