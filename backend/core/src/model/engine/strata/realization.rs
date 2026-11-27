//! Materializes the resolved Strata engine selection from a model configuration.
//!
//! The retained state is the configuration's fixed settings and option realizations. A binding
//! selects exactly the draft layer the fixed settings name, when the model's files need one. On
//! Strata's Inkling engine, a vision option selects its vision file while it is on and drops
//! image input while it is off, and a KV cache precision option must carry a resolved precision.

use std::collections::{BTreeMap, BTreeSet};

use dotcore_catalog::options::{KV_CACHE_PRECISION_PARAMETER_ID, boolean_value, string_value};
use dotcore_proto::dotcore::{
    catalog::v1::{
        Model, ModelArtifactBinding, ModelEngineConfiguration, ModelInputModality,
        ModelOptionValues,
    },
    engine::{
        strata::v1::{
            StrataModelConfiguration, StrataResolvedModelState,
            strata_option_realization::Realization,
        },
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
    let payload = strata_payload(configuration)
        .ok_or_else(|| anyhow::anyhow!("configuration has no Strata payload"))?;
    let fixed_settings = payload
        .fixed_settings
        .clone()
        .ok_or_else(|| anyhow::anyhow!("configuration has no fixed Strata settings"))?;
    let mut auxiliary_artifact_ids = super::draft_layer_artifact_id(&fixed_settings)
        .map(|id| vec![id.to_owned()])
        .unwrap_or_default();
    if binding.auxiliary_artifact_ids != auxiliary_artifact_ids {
        anyhow::bail!(
            "Strata binding must select exactly the draft layer its fixed settings name, if any"
        );
    }
    let mut input_capabilities = configuration
        .input_capabilities
        .clone()
        .ok_or_else(|| anyhow::anyhow!("configuration has no input capabilities"))?;

    let values = option_values
        .iter()
        .map(|values| (values.option_id.as_str(), values))
        .collect::<BTreeMap<_, _>>();
    if values.len() != payload.option_realizations.len() {
        anyhow::bail!("Strata option values must match the configuration's option realizations");
    }
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
                if enabled {
                    auxiliary_artifact_ids.push(vision.auxiliary_artifact_id.clone());
                } else {
                    input_capabilities
                        .supported_modalities
                        .retain(|value| *value != ModelInputModality::Image as i32);
                    input_capabilities.image_input_capacity = None;
                }
            }
            Some(Realization::KvCachePrecision(_)) => {
                string_value(values, KV_CACHE_PRECISION_PARAMETER_ID).ok_or_else(|| {
                    anyhow::anyhow!("KV cache precision option has no resolved precision")
                })?;
            }
            None => anyhow::bail!(
                "option {:?} has no Strata realization",
                realization.option_id
            ),
        }
    }

    Ok(ResolvedEngineSelection {
        state: ResolvedEngineModelState {
            engine_kind: EngineKind::Strata as i32,
            semantics_version: SEMANTICS_VERSION,
            state: Some(State::Strata(StrataResolvedModelState {
                fixed_settings: Some(fixed_settings),
                option_realizations: payload.option_realizations.clone(),
            })),
        },
        auxiliary_artifact_ids,
        input_capabilities,
    })
}

/// Sets of auxiliary artifacts the configuration's options can add: none, plus the vision file
/// when a vision option is declared.
pub(crate) fn selectable_auxiliary_artifact_sets(
    configuration: &ModelEngineConfiguration,
) -> Vec<BTreeSet<String>> {
    let vision = strata_payload(configuration).and_then(|payload| {
        payload.option_realizations.iter().find_map(|realization| {
            match realization.realization.as_ref()? {
                Realization::Vision(vision) => Some(vision.auxiliary_artifact_id.clone()),
                Realization::KvCachePrecision(_) => None,
            }
        })
    });
    let mut sets = vec![BTreeSet::new()];
    if let Some(vision) = vision {
        sets.push(BTreeSet::from([vision]));
    }
    sets
}

/// Reads the Strata payload of a configuration, or none for another engine kind.
fn strata_payload(configuration: &ModelEngineConfiguration) -> Option<&StrataModelConfiguration> {
    configuration
        .engine_configuration
        .as_ref()
        .and_then(|payload| payload.configuration.as_ref())
        .and_then(|payload| match payload {
            Configuration::Strata(payload) => Some(payload),
            Configuration::LlamaCpp(_) | Configuration::Vllm(_) | Configuration::Ninfer(_) => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dotcore_proto::dotcore::catalog::v1::model_option_parameter_value::Value;

    use crate::test_support::strata::{
        DRAFT_LAYER_ARTIFACT_ID, VISION_ARTIFACT_ID, configuration, inkling_options_declaration,
        model, ready_pack_configuration, split_gguf_configuration,
    };

    #[test]
    fn state_keeps_the_fixed_settings_and_the_configuration_input() {
        let configuration = configuration();

        let selection = realize_model(
            &model(),
            &configuration,
            &configuration.artifact_bindings[0],
            &[],
        )
        .expect("Strata selection");

        let Some(Configuration::Strata(payload)) = configuration
            .engine_configuration
            .clone()
            .and_then(|wrapper| wrapper.configuration)
        else {
            panic!("Strata payload");
        };
        assert_eq!(
            selection.state,
            ResolvedEngineModelState {
                engine_kind: EngineKind::Strata as i32,
                semantics_version: SEMANTICS_VERSION,
                state: Some(State::Strata(StrataResolvedModelState {
                    fixed_settings: payload.fixed_settings,
                    option_realizations: Vec::new(),
                })),
            }
        );
        assert_eq!(
            Some(selection.input_capabilities),
            configuration.input_capabilities
        );
        assert!(selection.auxiliary_artifact_ids.is_empty());
    }

    #[test]
    fn rejects_option_values_and_auxiliary_artifacts() {
        let configuration = configuration();
        let options = realize_model(
            &model(),
            &configuration,
            &configuration.artifact_bindings[0],
            &[ModelOptionValues {
                option_id: "mtp".to_owned(),
                values: Vec::new(),
            }],
        );
        assert_eq!(
            options.err().expect("option values").to_string(),
            "Strata option values must match the configuration's option realizations"
        );

        let mut binding = configuration.artifact_bindings[0].clone();
        binding.auxiliary_artifact_ids.push("template".to_owned());
        assert_eq!(
            realize_model(&model(), &configuration, &binding, &[])
                .err()
                .expect("auxiliary artifact")
                .to_string(),
            "Strata binding must select exactly the draft layer its fixed settings name, if any"
        );

        let split = split_gguf_configuration();
        let mut no_draft = split.artifact_bindings[0].clone();
        no_draft.auxiliary_artifact_ids.clear();
        assert_eq!(
            realize_model(&model(), &split, &no_draft, &[])
                .err()
                .expect("missing draft layer")
                .to_string(),
            "Strata binding must select exactly the draft layer its fixed settings name, if any"
        );
    }

    #[test]
    fn inkling_vision_selects_its_file_only_while_on() {
        let (model, configuration) = inkling_options_declaration();
        let realize = |vision: bool| {
            let values = configuration
                .options
                .iter()
                .map(|option| {
                    let mut values = option.default_values.clone().expect("defaults");
                    if option.option_id == "vision" {
                        values.values[0].value = Some(Value::Boolean(vision));
                    }
                    values
                })
                .collect::<Vec<_>>();
            realize_model(
                &model,
                &configuration,
                &configuration.artifact_bindings[0],
                &values,
            )
            .expect("Inkling selection")
        };

        let on = realize(true);
        assert_eq!(on.auxiliary_artifact_ids, [VISION_ARTIFACT_ID]);
        assert_eq!(
            Some(on.input_capabilities),
            configuration.input_capabilities
        );

        let off = realize(false);
        assert!(off.auxiliary_artifact_ids.is_empty());
        assert_eq!(
            off.input_capabilities.supported_modalities,
            [ModelInputModality::Text as i32]
        );
        assert_eq!(off.input_capabilities.image_input_capacity, None);
    }

    #[test]
    fn pack_paths_select_their_draft_layer() {
        for configuration in [split_gguf_configuration(), ready_pack_configuration()] {
            let selection = realize_model(
                &model(),
                &configuration,
                &configuration.artifact_bindings[0],
                &[],
            )
            .expect("Strata selection");

            assert_eq!(
                selection.auxiliary_artifact_ids,
                [DRAFT_LAYER_ARTIFACT_ID],
                "{}",
                configuration.configuration_id
            );
        }
    }
}
