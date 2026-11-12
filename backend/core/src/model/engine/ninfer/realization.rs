//! Materializes the resolved ninfer engine selection from a configured model option set.
//!
//! The retained state is the configuration's fixed settings and option realizations. Image input
//! is dropped from the public capabilities when the vision option is disabled, and an MTP option
//! must carry a resolved speculative-token count, where 0 is off. ninfer loads a single weights
//! file, so no auxiliary artifacts are ever selected.

use std::collections::BTreeMap;

use dotcore_catalog::options::{
    SPECULATIVE_TOKEN_COUNT_PARAMETER_ID, boolean_value, unsigned_value,
};
use dotcore_proto::dotcore::{
    catalog::v1::{
        Model, ModelArtifactBinding, ModelEngineConfiguration, ModelInputModality,
        ModelOptionValues,
    },
    engine::{
        ninfer::v1::{NinferResolvedModelState, ninfer_option_realization::Realization},
        v1::{
            EngineKind, ResolvedEngineModelState, engine_model_configuration::Configuration,
            resolved_engine_model_state::State,
        },
    },
};

use crate::model::engine::realization::ResolvedEngineSelection;

pub(crate) const SEMANTICS_VERSION: u32 = 3;

pub(crate) fn realize_model(
    _model: &Model,
    configuration: &ModelEngineConfiguration,
    binding: &ModelArtifactBinding,
    option_values: &[ModelOptionValues],
) -> anyhow::Result<ResolvedEngineSelection> {
    let payload = configuration
        .engine_configuration
        .as_ref()
        .and_then(|payload| payload.configuration.as_ref())
        .and_then(|payload| match payload {
            Configuration::Ninfer(payload) => Some(payload),
            Configuration::LlamaCpp(_) | Configuration::Vllm(_) | Configuration::Strata(_) => None,
        })
        .ok_or_else(|| anyhow::anyhow!("configuration has no ninfer payload"))?;
    let fixed_settings = payload
        .fixed_settings
        .clone()
        .ok_or_else(|| anyhow::anyhow!("configuration has no fixed ninfer settings"))?;
    if !binding.auxiliary_artifact_ids.is_empty() {
        anyhow::bail!("ninfer binding selects auxiliary artifacts");
    }
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
            Some(Realization::Mtp(_)) => {
                if unsigned_value(values, SPECULATIVE_TOKEN_COUNT_PARAMETER_ID).is_none() {
                    anyhow::bail!("MTP option has no resolved speculative token count");
                }
            }
            None => anyhow::bail!(
                "option {:?} has no ninfer realization",
                realization.option_id
            ),
        }
    }

    Ok(ResolvedEngineSelection {
        state: ResolvedEngineModelState {
            engine_kind: EngineKind::Ninfer as i32,
            semantics_version: SEMANTICS_VERSION,
            state: Some(State::Ninfer(NinferResolvedModelState {
                fixed_settings: Some(fixed_settings),
                option_realizations: payload.option_realizations.clone(),
            })),
        },
        auxiliary_artifact_ids: Vec::new(),
        input_capabilities,
    })
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::catalog::v1::{
        ModelOptionParameterValue, model_option_parameter_value::Value,
    };

    use super::*;
    use crate::test_support::ninfer::{configuration, model};

    #[test]
    fn vision_toggle_controls_image_capability_and_state_keeps_realizations() {
        let enabled = realize(true, Some(3)).expect("vision enabled");
        assert_eq!(
            enabled.input_capabilities,
            configuration().input_capabilities.expect("capabilities")
        );
        let Some(State::Ninfer(state)) = enabled.state.state.as_ref() else {
            panic!("ninfer state");
        };
        let Some(Configuration::Ninfer(payload)) = configuration()
            .engine_configuration
            .and_then(|wrapper| wrapper.configuration)
        else {
            panic!("ninfer payload");
        };
        assert_eq!(state.fixed_settings, payload.fixed_settings);
        assert_eq!(state.option_realizations, payload.option_realizations);
        assert_eq!(enabled.state.semantics_version, SEMANTICS_VERSION);
        assert!(enabled.auxiliary_artifact_ids.is_empty());

        let disabled = realize(false, Some(3)).expect("vision disabled");
        assert_eq!(
            disabled.input_capabilities.supported_modalities,
            [ModelInputModality::Text as i32]
        );
        assert_eq!(disabled.input_capabilities.image_input_capacity, None);
    }

    #[test]
    fn mtp_requires_a_resolved_token_count() {
        for count in [0, 3] {
            realize(true, Some(count)).expect("resolved token count");
        }
        assert_eq!(
            realize(true, None)
                .err()
                .expect("missing token count")
                .to_string(),
            "MTP option has no resolved speculative token count"
        );
    }

    fn realize(vision: bool, token_count: Option<u64>) -> anyhow::Result<ResolvedEngineSelection> {
        let configuration = configuration();
        let values = |option_id: &str, values: Vec<(&str, Value)>| ModelOptionValues {
            option_id: option_id.to_owned(),
            values: values
                .into_iter()
                .map(|(parameter_id, value)| ModelOptionParameterValue {
                    parameter_id: parameter_id.to_owned(),
                    value: Some(value),
                })
                .collect(),
        };
        realize_model(
            &model(),
            &configuration,
            &configuration.artifact_bindings[0],
            &[
                values("vision", vec![("enabled", Value::Boolean(vision))]),
                values(
                    "mtp",
                    token_count
                        .map(|count| {
                            (
                                SPECULATIVE_TOKEN_COUNT_PARAMETER_ID,
                                Value::UnsignedInteger(count),
                            )
                        })
                        .into_iter()
                        .collect(),
                ),
            ],
        )
    }
}
