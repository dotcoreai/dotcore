//! Owner-neutral typed parameters: definition validation, value checks against a definition,
//! and the canonical text form a value takes when projected into configuration.
//!
//! Application graphs use these definitions today; the model lane keeps its own engine and
//! option parameter schemas until it adopts this one. Owners layer their own rules on top,
//! such as whether a default is required.

use std::collections::BTreeSet;

use anyhow::anyhow;
use dotcore_domain::valid_id;
use dotcore_proto::dotcore::catalog::v1::{
    ParameterDefinition, ParameterValue, parameter_definition::ParameterType,
    parameter_value::Value,
};

/// Validates one definition's identity, label, and type constraints, including any default.
pub(crate) fn validate_parameter_definition(
    definition: &ParameterDefinition,
) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    let id = &definition.parameter_id;
    if !valid_id(id) {
        errors.push(anyhow!(
            "parameter id {id:?} must match [a-z][a-z0-9-]{{0,62}}"
        ));
    }
    if definition.display_name.trim().is_empty() {
        errors.push(anyhow!("parameter {id:?} requires a display name"));
    }
    match definition.parameter_type.as_ref() {
        Some(ParameterType::Boolean(_)) => {}
        Some(ParameterType::UnsignedInteger(range)) if range.minimum > range.maximum => {
            errors.push(anyhow!(
                "parameter {id:?} minimum must not exceed its maximum"
            ));
        }
        Some(ParameterType::UnsignedInteger(_)) => {}
        Some(ParameterType::StringEnum(schema)) => {
            let unique = schema.values.iter().collect::<BTreeSet<_>>();
            if schema.values.is_empty() {
                errors.push(anyhow!("parameter {id:?} requires at least one enum value"));
            } else if unique.len() != schema.values.len() {
                errors.push(anyhow!("parameter {id:?} requires unique enum values"));
            }
            if schema
                .values
                .iter()
                .any(|value| value.is_empty() || value.chars().any(char::is_control))
            {
                errors.push(anyhow!(
                    "parameter {id:?} enum values must be non-empty without control characters"
                ));
            }
        }
        None => errors.push(anyhow!("parameter {id:?} requires a type")),
    }
    if errors.is_empty()
        && let Some(default) = default_value(definition)
        && let Err(error) = check_value(definition, &default)
    {
        errors.push(anyhow!("parameter {id:?} default is invalid: {error}"));
    }
    errors
}

/// Returns the definition's declared default, if any.
pub fn default_value(definition: &ParameterDefinition) -> Option<Value> {
    match definition.parameter_type.as_ref()? {
        ParameterType::Boolean(schema) => schema.default_value.map(Value::Boolean),
        ParameterType::UnsignedInteger(schema) => schema.default_value.map(Value::UnsignedInteger),
        ParameterType::StringEnum(schema) => schema.default_value.clone().map(Value::String),
    }
}

/// Checks that a value has the definition's type and lies within its range or choices.
pub fn check_value(definition: &ParameterDefinition, value: &Value) -> anyhow::Result<()> {
    match (definition.parameter_type.as_ref(), value) {
        (Some(ParameterType::Boolean(_)), Value::Boolean(_)) => Ok(()),
        (Some(ParameterType::UnsignedInteger(range)), Value::UnsignedInteger(value)) => {
            if (range.minimum..=range.maximum).contains(value) {
                Ok(())
            } else {
                Err(anyhow!(
                    "{value} is outside {}..={}",
                    range.minimum,
                    range.maximum
                ))
            }
        }
        (Some(ParameterType::StringEnum(schema)), Value::String(value)) => {
            if schema.values.contains(value) {
                Ok(())
            } else {
                Err(anyhow!("{value:?} is not one of {:?}", schema.values))
            }
        }
        _ => Err(anyhow!("value type does not match the parameter type")),
    }
}

/// Renders a value in its canonical text form: `true`/`false`, a decimal integer, or the enum
/// value verbatim.
pub fn canonical_text(value: &Value) -> String {
    match value {
        Value::Boolean(value) => value.to_string(),
        Value::UnsignedInteger(value) => value.to_string(),
        Value::String(value) => value.clone(),
    }
}

/// Returns the canonical text of the value for `parameter_id` among `values`, if present.
pub fn canonical_value_text(values: &[ParameterValue], parameter_id: &str) -> Option<String> {
    values
        .iter()
        .find(|value| value.parameter_id == parameter_id)
        .and_then(|value| value.value.as_ref())
        .map(canonical_text)
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::catalog::v1::{
        BooleanParameterSchema, StringEnumParameterSchema, UnsignedIntegerParameterSchema,
    };

    use super::*;

    #[test]
    fn canonical_text_renders_each_type() {
        assert_eq!(canonical_text(&Value::Boolean(true)), "true");
        assert_eq!(canonical_text(&Value::Boolean(false)), "false");
        assert_eq!(canonical_text(&Value::UnsignedInteger(42)), "42");
        assert_eq!(
            canonical_text(&Value::String("highvram".into())),
            "highvram"
        );
    }

    #[test]
    fn check_value_accepts_matching_values_and_rejects_others() {
        let integer = definition(ParameterType::UnsignedInteger(
            UnsignedIntegerParameterSchema {
                minimum: 1,
                maximum: 8,
                unit: "workers".into(),
                default_value: Some(2),
            },
        ));
        assert!(check_value(&integer, &Value::UnsignedInteger(8)).is_ok());
        assert_eq!(
            check_value(&integer, &Value::UnsignedInteger(9))
                .unwrap_err()
                .to_string(),
            "9 is outside 1..=8"
        );
        assert_eq!(
            check_value(&integer, &Value::Boolean(true))
                .unwrap_err()
                .to_string(),
            "value type does not match the parameter type"
        );

        let choice = enum_definition(&["auto", "highvram"], Some("auto"));
        assert!(check_value(&choice, &Value::String("highvram".into())).is_ok());
        assert_eq!(
            check_value(&choice, &Value::String("lowvram".into()))
                .unwrap_err()
                .to_string(),
            "\"lowvram\" is not one of [\"auto\", \"highvram\"]"
        );
    }

    #[test]
    fn definition_validation_rejects_invalid_shapes() {
        let mut bad_id = definition(ParameterType::Boolean(BooleanParameterSchema {
            default_value: Some(false),
        }));
        bad_id.parameter_id = "Bad".into();
        assert_errors(
            &bad_id,
            &["parameter id \"Bad\" must match [a-z][a-z0-9-]{0,62}"],
        );

        assert_errors(
            &definition(ParameterType::UnsignedInteger(
                UnsignedIntegerParameterSchema {
                    minimum: 5,
                    maximum: 1,
                    unit: String::new(),
                    default_value: None,
                },
            )),
            &["parameter \"mode\" minimum must not exceed its maximum"],
        );
        assert_errors(
            &enum_definition(&["auto", "auto"], None),
            &["parameter \"mode\" requires unique enum values"],
        );
        assert_errors(
            &enum_definition(&[], None),
            &["parameter \"mode\" requires at least one enum value"],
        );
        assert_errors(
            &enum_definition(&["auto"], Some("other")),
            &["parameter \"mode\" default is invalid: \"other\" is not one of [\"auto\"]"],
        );
        let mut untyped = enum_definition(&["auto"], None);
        untyped.parameter_type = None;
        assert_errors(&untyped, &["parameter \"mode\" requires a type"]);
        assert!(
            validate_parameter_definition(&enum_definition(&["auto"], Some("auto"))).is_empty()
        );
    }

    fn assert_errors(definition: &ParameterDefinition, expected: &[&str]) {
        let errors = validate_parameter_definition(definition)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert_eq!(errors, expected);
    }

    fn enum_definition(values: &[&str], default: Option<&str>) -> ParameterDefinition {
        definition(ParameterType::StringEnum(StringEnumParameterSchema {
            values: values.iter().map(|value| (*value).to_owned()).collect(),
            default_value: default.map(str::to_owned),
        }))
    }

    fn definition(parameter_type: ParameterType) -> ParameterDefinition {
        ParameterDefinition {
            parameter_id: "mode".into(),
            display_name: "Mode".into(),
            description: String::new(),
            parameter_type: Some(parameter_type),
        }
    }
}
