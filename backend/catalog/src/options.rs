//! Resolves generic model option values from configuration defaults, profile presets,
//! and deployment overrides.
//!
//! This module is a dependency leaf. It validates and overlays typed values without
//! interpreting engine-owned realization payloads or producing engine effects. A resolved
//! value set activates at most one speculative-decoding option.

use std::collections::{BTreeMap, BTreeSet};

use dotcore_proto::dotcore::catalog::v1::{
    ModelConfigurationOption, ModelEngineConfiguration, ModelOption,
    ModelOptionParameterDefinition, ModelOptionParameterValue, ModelOptionValues, ModelProfile,
    model_option::Kind as ModelOptionKind, model_option_parameter_definition::ParameterType,
    model_option_parameter_value::Value,
};

/// Fixed parameter id of the speculative-token count every speculative-decoding option
/// declares. A count of 0 turns speculative decoding off.
pub const SPECULATIVE_TOKEN_COUNT_PARAMETER_ID: &str = "token-count";

/// Fixed parameter id of the precision every KV cache precision option declares. Engine adapters
/// own which precisions they accept.
pub const KV_CACHE_PRECISION_PARAMETER_ID: &str = "precision";

/// Resolves configuration, profile, and deployment option values in declaration order.
/// The model-level options identify the speculative-decoding options, at most one of which may
/// resolve to a nonzero speculative-token count.
pub fn resolve_model_option_values(
    model_options: &[ModelOption],
    configuration: &ModelEngineConfiguration,
    profile: &ModelProfile,
    overrides: &[ModelOptionValues],
) -> anyhow::Result<Vec<ModelOptionValues>> {
    let values = overlay_model_option_values(model_options, configuration, profile, overrides)?;
    if !crate::sizing::compatible(configuration, &values).map_err(anyhow::Error::new)? {
        anyhow::bail!("resolved model options are incompatible");
    }
    Ok(values)
}

/// Overlays configuration, profile, and deployment option values like
/// [`resolve_model_option_values`], without checking the configuration's compatibility rule.
pub(crate) fn overlay_model_option_values(
    model_options: &[ModelOption],
    configuration: &ModelEngineConfiguration,
    profile: &ModelProfile,
    overrides: &[ModelOptionValues],
) -> anyhow::Result<Vec<ModelOptionValues>> {
    let mut profile_defaults = values_by_option(&profile.option_defaults, "profile defaults")?;
    let mut overrides = values_by_option(overrides, "deployment overrides")?;
    reject_unknown_options(configuration, profile_defaults.keys(), "profile defaults")?;
    reject_unknown_options(configuration, overrides.keys(), "deployment overrides")?;

    let mut values = Vec::with_capacity(configuration.options.len());
    for option in &configuration.options {
        values.push(resolve_option_values(
            option,
            profile_defaults.remove(&option.option_id),
            overrides.remove(&option.option_id),
        )?);
    }
    reject_multiple_active_speculative_options(model_options, &values)?;
    Ok(values)
}

/// Checks one complete or sparse value set against a configuration-local schema.
pub fn valid_option_values(
    values: &ModelOptionValues,
    option: &ModelConfigurationOption,
    complete: bool,
) -> bool {
    if values.option_id != option.option_id {
        return false;
    }
    let mut ids = BTreeSet::new();
    if complete && values.values.len() != option.parameters.len() {
        return false;
    }
    values.values.iter().all(|value| {
        ids.insert(value.parameter_id.clone())
            && option
                .parameters
                .iter()
                .find(|definition| definition.parameter_id == value.parameter_id)
                .is_some_and(|definition| value_matches_definition(value, definition))
    })
}

pub fn boolean_value(values: &ModelOptionValues, parameter_id: &str) -> Option<bool> {
    values
        .values
        .iter()
        .find(|value| value.parameter_id == parameter_id)
        .and_then(|value| match value.value.as_ref() {
            Some(Value::Boolean(value)) => Some(*value),
            _ => None,
        })
}

pub fn unsigned_value(values: &ModelOptionValues, parameter_id: &str) -> Option<u64> {
    values
        .values
        .iter()
        .find(|value| value.parameter_id == parameter_id)
        .and_then(|value| match value.value.as_ref() {
            Some(Value::UnsignedInteger(value)) => Some(*value),
            _ => None,
        })
}

pub fn string_value<'a>(values: &'a ModelOptionValues, parameter_id: &str) -> Option<&'a str> {
    values
        .values
        .iter()
        .find(|value| value.parameter_id == parameter_id)
        .and_then(|value| match value.value.as_ref() {
            Some(Value::String(value)) => Some(value.as_str()),
            _ => None,
        })
}

/// Rejects a value set in which more than one speculative-decoding option has a nonzero
/// speculative-token count, naming every active option.
pub(crate) fn reject_multiple_active_speculative_options(
    model_options: &[ModelOption],
    values: &[ModelOptionValues],
) -> anyhow::Result<()> {
    let active = values
        .iter()
        .filter(|values| {
            model_options.iter().any(|option| {
                option.option_id == values.option_id
                    && matches!(option.kind, Some(ModelOptionKind::SpeculativeDecoding(_)))
            }) && unsigned_value(values, SPECULATIVE_TOKEN_COUNT_PARAMETER_ID)
                .is_some_and(|count| count > 0)
        })
        .map(|values| format!("{:?}", values.option_id))
        .collect::<Vec<_>>();
    if active.len() > 1 {
        anyhow::bail!(
            "at most one speculative decoding option may have a token count above 0, but {} are active",
            active.join(", ")
        );
    }
    Ok(())
}

fn resolve_option_values(
    option: &ModelConfigurationOption,
    profile_defaults: Option<&ModelOptionValues>,
    overrides: Option<&ModelOptionValues>,
) -> anyhow::Result<ModelOptionValues> {
    let defaults = option
        .default_values
        .as_ref()
        .filter(|values| valid_option_values(values, option, true))
        .ok_or_else(|| anyhow::anyhow!("option {:?} has invalid defaults", option.option_id))?;
    if profile_defaults.is_some_and(|values| !valid_option_values(values, option, false)) {
        anyhow::bail!(
            "profile has invalid defaults for option {:?}",
            option.option_id
        );
    }
    if overrides.is_some_and(|values| !valid_option_values(values, option, false)) {
        anyhow::bail!(
            "deployment has invalid overrides for option {:?}",
            option.option_id
        );
    }

    let mut selected = defaults
        .values
        .iter()
        .map(|value| (value.parameter_id.clone(), value.clone()))
        .collect::<BTreeMap<_, _>>();
    for layer in [profile_defaults, overrides].into_iter().flatten() {
        for value in &layer.values {
            selected.insert(value.parameter_id.clone(), value.clone());
        }
    }
    let values = option
        .parameters
        .iter()
        .map(|definition| {
            selected
                .remove(&definition.parameter_id)
                .expect("validated complete configuration defaults")
        })
        .collect();
    Ok(ModelOptionValues {
        option_id: option.option_id.clone(),
        values,
    })
}

fn values_by_option<'a>(
    values: &'a [ModelOptionValues],
    source: &str,
) -> anyhow::Result<BTreeMap<String, &'a ModelOptionValues>> {
    let mut indexed = BTreeMap::new();
    for value in values {
        if indexed.insert(value.option_id.clone(), value).is_some() {
            anyhow::bail!("{source} contain duplicate option {:?}", value.option_id);
        }
    }
    Ok(indexed)
}

fn reject_unknown_options<'a>(
    configuration: &ModelEngineConfiguration,
    option_ids: impl Iterator<Item = &'a String>,
    source: &str,
) -> anyhow::Result<()> {
    for option_id in option_ids {
        if !configuration
            .options
            .iter()
            .any(|option| option.option_id == *option_id)
        {
            anyhow::bail!("{source} reference unknown option {option_id:?}");
        }
    }
    Ok(())
}

fn value_matches_definition(
    value: &ModelOptionParameterValue,
    definition: &ModelOptionParameterDefinition,
) -> bool {
    matches!(
        (definition.parameter_type.as_ref(), value.value.as_ref()),
        (Some(ParameterType::Boolean(_)), Some(Value::Boolean(_)))
    ) || matches!(
        (definition.parameter_type.as_ref(), value.value.as_ref()),
        (Some(ParameterType::UnsignedInteger(range)), Some(Value::UnsignedInteger(value)))
            if *value >= range.minimum && *value <= range.maximum
    ) || matches!(
        (definition.parameter_type.as_ref(), value.value.as_ref()),
        (Some(ParameterType::StringEnum(schema)), Some(Value::String(value)))
            if schema.values.contains(value)
    )
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::catalog::v1::{
        BooleanModelOptionParameterSchema, ModelConfigurationOption, ModelEngineConfiguration,
        ModelOption, ModelOptionParameterDefinition, ModelOptionParameterValue, ModelOptionValues,
        ModelProfile, SpeculativeDecodingMethod, SpeculativeDecodingModelOption,
        UnsignedIntegerModelOptionParameterSchema, model_option,
        model_option_parameter_definition::ParameterType, model_option_parameter_value::Value,
    };

    use super::{SPECULATIVE_TOKEN_COUNT_PARAMETER_ID, resolve_model_option_values};

    #[test]
    fn resolution_preserves_explicit_false_over_profile_default() {
        let configuration = ModelEngineConfiguration {
            options: vec![ModelConfigurationOption {
                option_id: "vision".to_owned(),
                parameters: vec![ModelOptionParameterDefinition {
                    parameter_id: "enabled".to_owned(),
                    display_name: "Enabled".to_owned(),
                    parameter_type: Some(ParameterType::Boolean(
                        BooleanModelOptionParameterSchema {},
                    )),
                }],
                default_values: Some(values(false)),
                ..Default::default()
            }],
            ..Default::default()
        };
        let profile = ModelProfile {
            option_defaults: vec![values(true)],
            ..Default::default()
        };

        assert_eq!(
            resolve_model_option_values(&[], &configuration, &profile, &[values(false)])
                .expect("valid override"),
            vec![values(false)]
        );
    }

    #[test]
    fn resolution_preserves_explicit_zero_over_profile_default() {
        let profile = ModelProfile {
            option_defaults: vec![token_count("mtp", 5), token_count("dflash", 3)],
            ..Default::default()
        };

        assert_eq!(
            resolve_model_option_values(
                &speculative_model_options(),
                &speculative_configuration(),
                &profile,
                &[token_count("mtp", 0)],
            )
            .expect("valid override"),
            vec![token_count("mtp", 0), token_count("dflash", 3)]
        );
    }

    #[test]
    fn resolution_accepts_one_active_speculative_option() {
        let profile = ModelProfile {
            option_defaults: vec![token_count("mtp", 4)],
            ..Default::default()
        };

        assert_eq!(
            resolve_model_option_values(
                &speculative_model_options(),
                &speculative_configuration(),
                &profile,
                &[],
            )
            .expect("one active speculative option"),
            vec![token_count("mtp", 4), token_count("dflash", 0)]
        );
    }

    #[test]
    fn resolution_rejects_two_active_speculative_options() {
        let profile = ModelProfile {
            option_defaults: vec![token_count("mtp", 4)],
            ..Default::default()
        };

        let error = resolve_model_option_values(
            &speculative_model_options(),
            &speculative_configuration(),
            &profile,
            &[token_count("dflash", 2)],
        )
        .expect_err("two active speculative options");

        assert_eq!(
            error.to_string(),
            "at most one speculative decoding option may have a token count above 0, but \"mtp\", \"dflash\" are active"
        );
    }

    fn values(enabled: bool) -> ModelOptionValues {
        ModelOptionValues {
            option_id: "vision".to_owned(),
            values: vec![ModelOptionParameterValue {
                parameter_id: "enabled".to_owned(),
                value: Some(Value::Boolean(enabled)),
            }],
        }
    }

    /// Model options for two speculative-decoding methods.
    fn speculative_model_options() -> Vec<ModelOption> {
        ["mtp", "dflash"]
            .into_iter()
            .map(|option_id| ModelOption {
                option_id: option_id.to_owned(),
                display_name: option_id.to_uppercase(),
                kind: Some(model_option::Kind::SpeculativeDecoding(
                    SpeculativeDecodingModelOption {
                        method: SpeculativeDecodingMethod::Mtp as i32,
                    },
                )),
            })
            .collect()
    }

    /// Configuration implementing both speculative options, each off by default.
    fn speculative_configuration() -> ModelEngineConfiguration {
        ModelEngineConfiguration {
            options: ["mtp", "dflash"]
                .into_iter()
                .map(|option_id| ModelConfigurationOption {
                    option_id: option_id.to_owned(),
                    parameters: vec![ModelOptionParameterDefinition {
                        parameter_id: SPECULATIVE_TOKEN_COUNT_PARAMETER_ID.to_owned(),
                        display_name: "Token count".to_owned(),
                        parameter_type: Some(ParameterType::UnsignedInteger(
                            UnsignedIntegerModelOptionParameterSchema {
                                minimum: 0,
                                maximum: 8,
                                unit: "tokens".to_owned(),
                            },
                        )),
                    }],
                    default_values: Some(token_count(option_id, 0)),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    fn token_count(option_id: &str, count: u64) -> ModelOptionValues {
        ModelOptionValues {
            option_id: option_id.to_owned(),
            values: vec![ModelOptionParameterValue {
                parameter_id: SPECULATIVE_TOKEN_COUNT_PARAMETER_ID.to_owned(),
                value: Some(Value::UnsignedInteger(count)),
            }],
        }
    }
}
