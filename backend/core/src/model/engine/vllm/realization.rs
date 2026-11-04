//! Materializes the resolved vLLM engine selection from a configured model option set.
//!
//! Given a vLLM engine configuration, its artifact binding, and the resolved option values, it produces the
//! engine state to retain—fixed settings and option realizations—plus the selected auxiliary artifacts and
//! the input capabilities. Image input is dropped from the modalities when vision is disabled, and an
//! MTP option must carry a resolved speculative-token count, where 0 is off; a separate MTP draft
//! checkpoint is selected only while the count is above 0.

use std::collections::{BTreeMap, BTreeSet};

use dotcore_catalog::options::{
    SPECULATIVE_TOKEN_COUNT_PARAMETER_ID, boolean_value, unsigned_value,
};
use dotcore_proto::dotcore::{
    catalog::v1::{
        Model, ModelArtifactBinding, ModelEngineConfiguration, ModelInputModality,
        ModelOptionValues,
    },
    engine::{
        v1::{
            EngineKind, ResolvedEngineModelState, engine_model_configuration::Configuration,
            resolved_engine_model_state::State,
        },
        vllm::v1::{
            VllmModelConfiguration, VllmResolvedModelState, vllm_option_realization::Realization,
        },
    },
};

use crate::model::engine::{realization::ResolvedEngineSelection, speculative_draft};

pub(crate) const SEMANTICS_VERSION: u32 = 2;

pub(crate) fn realize_model(
    _model: &Model,
    configuration: &ModelEngineConfiguration,
    binding: &ModelArtifactBinding,
    option_values: &[ModelOptionValues],
) -> anyhow::Result<ResolvedEngineSelection> {
    let payload = vllm_payload(configuration)
        .ok_or_else(|| anyhow::anyhow!("configuration has no vLLM payload"))?;
    let fixed_settings = payload
        .fixed_settings
        .clone()
        .ok_or_else(|| anyhow::anyhow!("configuration has no fixed vLLM settings"))?;
    let mut auxiliary_artifact_ids = binding.auxiliary_artifact_ids.clone();
    let mut selected_auxiliary = auxiliary_artifact_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    add_auxiliary(
        &fixed_settings.chat_template_auxiliary_artifact_id,
        &mut selected_auxiliary,
        &mut auxiliary_artifact_ids,
    );
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
                let enabled =
                    boolean_value(values, &vision.enabled_parameter_id).ok_or_else(|| {
                        anyhow::anyhow!("vision option has no resolved boolean enable parameter")
                    })?;
                if !enabled {
                    input_capabilities
                        .supported_modalities
                        .retain(|value| *value != ModelInputModality::Image as i32);
                    input_capabilities.image_input_capacity = None;
                }
            }
            Some(Realization::Mtp(mtp)) => {
                let count = unsigned_value(values, SPECULATIVE_TOKEN_COUNT_PARAMETER_ID)
                    .ok_or_else(|| {
                        anyhow::anyhow!("MTP option has no resolved speculative token count")
                    })?;
                let draft = speculative_draft::vllm(mtp.draft.as_ref())
                    .ok_or_else(|| anyhow::anyhow!("MTP option has no draft source"))?;
                if let Some(artifact_id) = draft.artifact_id().filter(|_| count > 0) {
                    add_auxiliary(
                        artifact_id,
                        &mut selected_auxiliary,
                        &mut auxiliary_artifact_ids,
                    );
                }
            }
            None => anyhow::bail!("option {:?} has no vLLM realization", realization.option_id),
        }
    }

    Ok(ResolvedEngineSelection {
        state: ResolvedEngineModelState {
            engine_kind: EngineKind::Vllm as i32,
            semantics_version: SEMANTICS_VERSION,
            state: Some(State::Vllm(VllmResolvedModelState {
                fixed_settings: Some(fixed_settings),
                option_realizations: payload.option_realizations.clone(),
            })),
        },
        auxiliary_artifact_ids,
        input_capabilities,
    })
}

/// Sets of auxiliary artifacts the configuration's options can add: none, plus the MTP draft
/// checkpoint when one is declared.
pub(crate) fn selectable_auxiliary_artifact_sets(
    configuration: &ModelEngineConfiguration,
) -> Vec<BTreeSet<String>> {
    let draft = vllm_payload(configuration).and_then(|payload| {
        payload.option_realizations.iter().find_map(|realization| {
            match realization.realization.as_ref()? {
                Realization::Mtp(mtp) => speculative_draft::vllm(mtp.draft.as_ref())?.artifact_id(),
                Realization::Vision(_) => None,
            }
        })
    });
    let mut sets = vec![BTreeSet::new()];
    if let Some(draft) = draft {
        sets.push(BTreeSet::from([draft.to_owned()]));
    }
    sets
}

/// Reads the vLLM payload of a configuration, or none for another engine kind.
fn vllm_payload(configuration: &ModelEngineConfiguration) -> Option<&VllmModelConfiguration> {
    configuration
        .engine_configuration
        .as_ref()
        .and_then(|payload| payload.configuration.as_ref())
        .and_then(|payload| match payload {
            Configuration::Vllm(payload) => Some(payload),
            Configuration::LlamaCpp(_) | Configuration::Ninfer(_) => None,
        })
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

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::{
        catalog::v1::{
            ModelInputCapabilities, ModelOptionParameterValue, ModelOptionValues,
            model_option_parameter_value::Value,
        },
        engine::{
            v1::{EngineModelConfiguration, engine_model_configuration::Configuration},
            vllm::v1::{
                VllmEmbeddedMtpHead, VllmFixedModelSettings, VllmModelConfiguration,
                VllmMtpDraftSource, VllmMtpOptionRealization, VllmOptionRealization,
                VllmVisionOptionRealization, vllm_mtp_draft_source::Source as DraftSource,
                vllm_option_realization::Realization,
            },
        },
    };

    use super::*;

    fn realize(vision: bool, mtp: bool, draft: Option<&str>) -> ResolvedEngineSelection {
        let configuration = ModelEngineConfiguration {
            engine_configuration: Some(EngineModelConfiguration {
                configuration: Some(Configuration::Vllm(VllmModelConfiguration {
                    fixed_settings: Some(VllmFixedModelSettings {
                        chat_template_auxiliary_artifact_id: "template-x".to_owned(),
                        ..Default::default()
                    }),
                    option_realizations: vec![
                        VllmOptionRealization {
                            option_id: "renamed-vision".to_owned(),
                            realization: Some(Realization::Vision(VllmVisionOptionRealization {
                                enabled_parameter_id: "vision-enabled".to_owned(),
                            })),
                        },
                        VllmOptionRealization {
                            option_id: "renamed-mtp".to_owned(),
                            realization: Some(Realization::Mtp(VllmMtpOptionRealization {
                                draft: Some(VllmMtpDraftSource {
                                    source: Some(draft.map_or(
                                        DraftSource::Embedded(VllmEmbeddedMtpHead {}),
                                        |id| DraftSource::AuxiliaryArtifactId(id.to_owned()),
                                    )),
                                }),
                            })),
                        },
                    ],
                })),
                ..Default::default()
            }),
            input_capabilities: Some(ModelInputCapabilities {
                supported_modalities: vec![
                    ModelInputModality::Text as i32,
                    ModelInputModality::Image as i32,
                ],
                image_input_capacity: Some(Default::default()),
            }),
            ..Default::default()
        };
        let option_values = vec![
            ModelOptionValues {
                option_id: "renamed-vision".to_owned(),
                values: vec![ModelOptionParameterValue {
                    parameter_id: "vision-enabled".to_owned(),
                    value: Some(Value::Boolean(vision)),
                }],
            },
            ModelOptionValues {
                option_id: "renamed-mtp".to_owned(),
                values: vec![ModelOptionParameterValue {
                    parameter_id: SPECULATIVE_TOKEN_COUNT_PARAMETER_ID.to_owned(),
                    value: Some(Value::UnsignedInteger(if mtp { 4 } else { 0 })),
                }],
            },
        ];
        realize_model(
            &Model::default(),
            &configuration,
            &ModelArtifactBinding::default(),
            &option_values,
        )
        .expect("realization")
    }

    #[test]
    fn vision_and_mtp_realizations_are_independently_toggled() {
        let text = realize(false, false, None);
        assert!(
            text.input_capabilities
                .supported_modalities
                .iter()
                .all(|mode| *mode != ModelInputModality::Image as i32)
        );
        assert!(text.input_capabilities.image_input_capacity.is_none());
        let multimodal_mtp = realize(true, true, None);
        assert!(
            multimodal_mtp
                .input_capabilities
                .image_input_capacity
                .is_some()
        );
        assert_eq!(multimodal_mtp.auxiliary_artifact_ids, ["template-x"]);
    }

    #[test]
    fn mtp_draft_checkpoint_is_selected_only_while_the_count_is_above_zero() {
        assert_eq!(
            realize(false, false, Some("draft-x")).auxiliary_artifact_ids,
            ["template-x"]
        );
        assert_eq!(
            realize(false, true, Some("draft-x")).auxiliary_artifact_ids,
            ["template-x", "draft-x"]
        );
    }
}
