//! Owner-neutral typed parameters: definition validation, defaults, value checks against a
//! definition, and the canonical text form a value takes when projected into configuration.
//!
//! Application graphs and engine releases declare their parameters with these definitions. Owners
//! layer their own rules on top, such as which types and visibilities they accept and whether a
//! default is required.

use std::collections::BTreeSet;

use anyhow::anyhow;
use dotcore_domain::valid_id;
use dotcore_proto::dotcore::catalog::v1::{
    ParameterDefinition, ParameterValue, ParameterVisibility, parameter_definition::ParameterType,
    parameter_value::Value,
};

/// Validates one definition's identity, label, visibility and type constraints, including any
/// default.
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
    if !matches!(
        ParameterVisibility::try_from(definition.visibility),
        Ok(ParameterVisibility::Basic
            | ParameterVisibility::Advanced
            | ParameterVisibility::Hidden)
    ) {
        errors.push(anyhow!(
            "parameter {id:?} requires a basic, advanced or hidden visibility"
        ));
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
        Some(ParameterType::Text(schema)) => {
            if schema.maximum_length == 0 {
                errors.push(anyhow!(
                    "parameter {id:?} maximum length must be at least 1"
                ));
            }
            if schema.minimum_length > schema.maximum_length {
                errors.push(anyhow!(
                    "parameter {id:?} minimum length must not exceed its maximum length"
                ));
            }
            if schema.maximum_length > MAXIMUM_FREE_TEXT_LENGTH {
                errors.push(anyhow!(
                    "parameter {id:?} maximum length must not exceed {MAXIMUM_FREE_TEXT_LENGTH}"
                ));
            }
        }
        Some(ParameterType::Secret(schema)) => {
            if schema.minimum_length == 0 {
                errors.push(anyhow!(
                    "parameter {id:?} minimum length must be at least 1"
                ));
            }
            if schema.minimum_length > schema.generated_length {
                errors.push(anyhow!(
                    "parameter {id:?} minimum length must not exceed its generated length"
                ));
            }
            if schema.generated_length > MAXIMUM_FREE_TEXT_LENGTH {
                errors.push(anyhow!(
                    "parameter {id:?} generated length must not exceed {MAXIMUM_FREE_TEXT_LENGTH}"
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

/// Returns the definition's declared default, if any. A secret never has one.
pub fn default_value(definition: &ParameterDefinition) -> Option<Value> {
    match definition.parameter_type.as_ref()? {
        ParameterType::Boolean(schema) => schema.default_value.map(Value::Boolean),
        ParameterType::UnsignedInteger(schema) => schema.default_value.map(Value::UnsignedInteger),
        ParameterType::StringEnum(schema) => schema.default_value.clone().map(Value::String),
        ParameterType::Text(schema) => schema.default_value.clone().map(Value::String),
        ParameterType::Secret(_) => None,
    }
}

/// Returns whether the definition declares a secret.
pub fn is_secret(definition: &ParameterDefinition) -> bool {
    matches!(definition.parameter_type, Some(ParameterType::Secret(_)))
}

/// Checks that a value has the definition's type and lies within its range, choices or length
/// bounds. Errors never quote a text or secret value.
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
        (Some(ParameterType::Text(schema)), Value::String(value)) => {
            check_free_text(value, schema.minimum_length, schema.maximum_length)
        }
        (Some(ParameterType::Secret(schema)), Value::String(value)) => {
            check_free_text(value, schema.minimum_length, MAXIMUM_FREE_TEXT_LENGTH)
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

/// Longest text or secret value in characters, which keeps every value a reasonable environment
/// value; a secret declares no maximum of its own.
const MAXIMUM_FREE_TEXT_LENGTH: u32 = 1024;

/// Checks a text or secret value's length in characters and that it is safe as an environment
/// value: no `=` and no control characters.
fn check_free_text(value: &str, minimum: u32, maximum: u32) -> anyhow::Result<()> {
    if value.contains('=') || value.chars().any(char::is_control) {
        return Err(anyhow!("it contains `=` or a control character"));
    }
    let length = value.chars().count();
    if length < minimum as usize {
        return Err(anyhow!(
            "it is shorter than the minimum length of {minimum}"
        ));
    }
    if length > maximum as usize {
        return Err(anyhow!("it is longer than the maximum length of {maximum}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::catalog::v1::{
        BooleanParameterSchema, SecretParameterSchema, StringEnumParameterSchema,
        TextParameterSchema, UnsignedIntegerParameterSchema,
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
    fn check_value_bounds_text_and_secrets_without_quoting_them() {
        let text = definition(ParameterType::Text(TextParameterSchema {
            minimum_length: 2,
            maximum_length: 4,
            default_value: None,
        }));
        let secret = definition(ParameterType::Secret(SecretParameterSchema {
            generated_length: 32,
            minimum_length: 3,
        }));
        let check = |definition: &ParameterDefinition, value: &str| {
            check_value(definition, &Value::String(value.to_owned()))
                .map_err(|error| error.to_string())
        };

        assert_eq!(check(&text, "ab"), Ok(()));
        assert_eq!(check(&text, "éèêë"), Ok(()));
        assert_eq!(
            check(&text, "a"),
            Err("it is shorter than the minimum length of 2".to_owned())
        );
        assert_eq!(
            check(&text, "abcde"),
            Err("it is longer than the maximum length of 4".to_owned())
        );
        assert_eq!(check(&secret, &"s".repeat(1024)), Ok(()));
        assert_eq!(
            check(&secret, &"s".repeat(1025)),
            Err("it is longer than the maximum length of 1024".to_owned())
        );
        assert_eq!(
            check(&secret, ""),
            Err("it is shorter than the minimum length of 3".to_owned())
        );
        for unsafe_value in ["a=bc", "ab\tc", "ab\nc"] {
            assert_eq!(
                check(&secret, unsafe_value),
                Err("it contains `=` or a control character".to_owned())
            );
        }
        assert_eq!(
            check_value(&secret, &Value::Boolean(true))
                .unwrap_err()
                .to_string(),
            "value type does not match the parameter type"
        );
        assert_eq!(default_value(&secret), None);
        assert!(is_secret(&secret));
        assert!(!is_secret(&text));
    }

    #[test]
    fn definition_validation_caps_text_and_generated_secret_lengths() {
        assert_errors(
            &definition(ParameterType::Text(TextParameterSchema {
                minimum_length: 0,
                maximum_length: 1025,
                default_value: None,
            })),
            &["parameter \"mode\" maximum length must not exceed 1024"],
        );
        assert_errors(
            &definition(ParameterType::Secret(SecretParameterSchema {
                generated_length: 1025,
                minimum_length: 12,
            })),
            &["parameter \"mode\" generated length must not exceed 1024"],
        );
        assert!(
            validate_parameter_definition(&definition(ParameterType::Secret(
                SecretParameterSchema {
                    generated_length: 1024,
                    minimum_length: 12,
                }
            )))
            .is_empty()
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
        let mut unplaced = enum_definition(&["auto"], Some("auto"));
        unplaced.visibility = ParameterVisibility::Unspecified as i32;
        assert_errors(
            &unplaced,
            &["parameter \"mode\" requires a basic, advanced or hidden visibility"],
        );
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
            visibility: ParameterVisibility::Basic as i32,
            parameter_type: Some(parameter_type),
        }
    }
}
