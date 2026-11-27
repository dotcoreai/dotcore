//! Materializes the resolved Strata engine selection from a model configuration.
//!
//! The retained state is the configuration's fixed settings and option realizations. A binding
//! selects exactly the draft layer or the tokenizer the fixed settings name, when the engine path
//! needs one. On Strata's Inkling engine, a vision option selects its vision file while it is on
//! and drops image input while it is off, a DFlash option selects its drafter file while its switch
//! is on, and a KV cache precision option must carry a resolved precision. Option files always
//! follow the binding's files in one order, vision first, then the drafter.

use std::collections::{BTreeMap, BTreeSet};

use dotcore_catalog::options::{
    KV_CACHE_PRECISION_PARAMETER_ID, SPECULATIVE_ENABLED_PARAMETER_ID, boolean_value, string_value,
};
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

pub(crate) const SEMANTICS_VERSION: u32 = 4;

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
    let mut auxiliary_artifact_ids = super::bound_auxiliary_artifact_id(&fixed_settings)
        .map(|id| vec![id.to_owned()])
        .unwrap_or_default();
    if binding.auxiliary_artifact_ids != auxiliary_artifact_ids {
        anyhow::bail!(
            "Strata binding must select exactly the draft layer or tokenizer its fixed settings \
             name, if any"
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
    let mut vision_file = None;
    let mut drafter_file = None;
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
                    vision_file = Some(vision.auxiliary_artifact_id.clone());
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
            Some(Realization::Dflash(dflash)) => {
                let enabled = boolean_value(values, SPECULATIVE_ENABLED_PARAMETER_ID)
                    .ok_or_else(|| anyhow::anyhow!("DFlash option has no resolved switch"))?;
                if enabled {
                    drafter_file = Some(dflash.auxiliary_artifact_id.clone());
                }
            }
            None => anyhow::bail!(
                "option {:?} has no Strata realization",
                realization.option_id
            ),
        }
    }
    auxiliary_artifact_ids.extend(vision_file.into_iter().chain(drafter_file));

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

/// Sets of auxiliary artifacts the configuration's options can add: every combination of the
/// declared vision and drafter files, from none to both.
pub(crate) fn selectable_auxiliary_artifact_sets(
    configuration: &ModelEngineConfiguration,
) -> Vec<BTreeSet<String>> {
    let files = strata_payload(configuration)
        .map(|payload| {
            payload
                .option_realizations
                .iter()
                .filter_map(|realization| match realization.realization.as_ref()? {
                    Realization::Vision(vision) => Some(vision.auxiliary_artifact_id.clone()),
                    Realization::Dflash(dflash) => Some(dflash.auxiliary_artifact_id.clone()),
                    Realization::KvCachePrecision(_) => None,
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut sets = vec![BTreeSet::new()];
    for file in files {
        let with_file = sets
            .iter()
            .map(|set| {
                let mut set = set.clone();
                set.insert(file.clone());
                set
            })
            .collect::<Vec<_>>();
        sets.extend(with_file);
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
        DFLASH_ARTIFACT_ID, DRAFT_LAYER_ARTIFACT_ID, INKLING_TOKENIZER_ARTIFACT_ID,
        VISION_ARTIFACT_ID, configuration, inkling_configuration, inkling_options_declaration,
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
            "Strata binding must select exactly the draft layer or tokenizer its fixed settings name, if any"
        );

        let split = split_gguf_configuration();
        let mut no_draft = split.artifact_bindings[0].clone();
        no_draft.auxiliary_artifact_ids.clear();
        assert_eq!(
            realize_model(&model(), &split, &no_draft, &[])
                .err()
                .expect("missing draft layer")
                .to_string(),
            "Strata binding must select exactly the draft layer or tokenizer its fixed settings name, if any"
        );
    }

    #[test]
    fn inkling_options_select_their_files_only_while_on_after_the_tokenizer() {
        let (model, configuration) = inkling_options_declaration();
        let realize = |vision: bool, dflash: bool| {
            let values = configuration
                .options
                .iter()
                .map(|option| {
                    let mut values = option.default_values.clone().expect("defaults");
                    match option.option_id.as_str() {
                        "vision" => values.values[0].value = Some(Value::Boolean(vision)),
                        "dflash" => values.values[0].value = Some(Value::Boolean(dflash)),
                        _ => {}
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

        for (vision, dflash, expected) in [
            (false, false, vec![INKLING_TOKENIZER_ARTIFACT_ID]),
            (
                true,
                false,
                vec![INKLING_TOKENIZER_ARTIFACT_ID, VISION_ARTIFACT_ID],
            ),
            (
                false,
                true,
                vec![INKLING_TOKENIZER_ARTIFACT_ID, DFLASH_ARTIFACT_ID],
            ),
            (
                true,
                true,
                vec![
                    INKLING_TOKENIZER_ARTIFACT_ID,
                    VISION_ARTIFACT_ID,
                    DFLASH_ARTIFACT_ID,
                ],
            ),
        ] {
            assert_eq!(
                realize(vision, dflash).auxiliary_artifact_ids,
                expected,
                "vision {vision}, DFlash {dflash}"
            );
        }

        let on = realize(true, true);
        assert_eq!(
            Some(on.input_capabilities),
            configuration.input_capabilities
        );
        let off = realize(false, true);
        assert_eq!(
            off.input_capabilities.supported_modalities,
            [ModelInputModality::Text as i32]
        );
        assert_eq!(off.input_capabilities.image_input_capacity, None);
    }

    #[test]
    fn inkling_options_can_add_every_combination_of_their_files() {
        let (_, configuration) = inkling_options_declaration();

        assert_eq!(
            selectable_auxiliary_artifact_sets(&configuration),
            [
                BTreeSet::new(),
                BTreeSet::from([VISION_ARTIFACT_ID.to_owned()]),
                BTreeSet::from([DFLASH_ARTIFACT_ID.to_owned()]),
                BTreeSet::from([VISION_ARTIFACT_ID.to_owned(), DFLASH_ARTIFACT_ID.to_owned()]),
            ]
        );
        assert_eq!(
            selectable_auxiliary_artifact_sets(&inkling_configuration()),
            [BTreeSet::new()]
        );
    }

    #[test]
    fn every_inkling_binding_selects_the_tokenizer() {
        let configuration = inkling_configuration();
        for binding in &configuration.artifact_bindings {
            let selection =
                realize_model(&model(), &configuration, binding, &[]).expect("Inkling selection");

            assert_eq!(
                selection.auxiliary_artifact_ids,
                [INKLING_TOKENIZER_ARTIFACT_ID],
                "{}",
                binding.binding_id
            );
        }

        let mut no_tokenizer = configuration.artifact_bindings[1].clone();
        no_tokenizer.auxiliary_artifact_ids.clear();
        assert_eq!(
            realize_model(&model(), &configuration, &no_tokenizer, &[])
                .err()
                .expect("missing tokenizer")
                .to_string(),
            "Strata binding must select exactly the draft layer or tokenizer its fixed settings name, if any"
        );
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
