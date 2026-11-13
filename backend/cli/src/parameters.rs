//! Parameters of a deployment as rows an operator can read.
//!
//! Each row carries the parameter's display name and its value, tagged when the value is the
//! default, was sized by the platform or was generated. Secret values are masked unless the caller
//! asks to reveal them.

use dotcore_proto::dotcore::catalog::v1::{
    ParameterDefinition, ParameterValue, parameter_definition, parameter_value,
};

use crate::management::single_line;

const SECRET_MASK: &str = "********";
const EMPTY_TEXT: &str = "Empty";

/// One shown parameter.
pub(crate) struct ParameterRow {
    /// Stable parameter identity.
    pub(crate) id: String,
    /// Human-readable parameter label.
    pub(crate) name: String,
    /// The value with its tags.
    pub(crate) value: String,
}

/// What a deployment retains about one parameter, from which its row is built.
pub(crate) struct ParameterInput<'a> {
    pub(crate) definition: &'a ParameterDefinition,
    /// The retained value, if any.
    pub(crate) value: Option<&'a parameter_value::Value>,
    /// The value the deployment's owner defaults to, tagged when the shown value equals it.
    pub(crate) default: Option<parameter_value::Value>,
    /// Whether the platform sized the value rather than the operator choosing it.
    pub(crate) sized: bool,
    /// Whether the value was generated at creation.
    pub(crate) generated: bool,
}

impl ParameterInput<'_> {
    /// The row of this parameter. A parameter without a retained value shows its default, and one
    /// with neither is left out.
    pub(crate) fn row(&self, reveal_secrets: bool) -> Option<ParameterRow> {
        let definition = self.definition;
        let value = self.value.or(self.default.as_ref())?;
        let is_secret = matches!(
            definition.parameter_type,
            Some(parameter_definition::ParameterType::Secret(_))
        );

        let mut text = if is_secret && !reveal_secrets {
            SECRET_MASK.to_owned()
        } else {
            match single_line(&display(value, definition)) {
                empty if empty.is_empty() => EMPTY_TEXT.to_owned(),
                text => text,
            }
        };
        if self.default.as_ref() == Some(value) {
            text.push_str(" (default)");
        }
        if self.sized {
            text.push_str(" (sized)");
        }
        if self.generated {
            text.push_str(" (generated)");
        }

        Some(ParameterRow {
            id: definition.parameter_id.clone(),
            name: single_line(&definition.display_name),
            value: text,
        })
    }
}

/// The row of a secret that has no parameter definition, masked unless revealed.
pub(crate) fn secret_row(id: &str, name: &str, secret: &str, reveal_secrets: bool) -> ParameterRow {
    ParameterRow {
        id: id.to_owned(),
        name: name.to_owned(),
        value: if reveal_secrets {
            single_line(secret)
        } else {
            SECRET_MASK.to_owned()
        },
    }
}

/// The retained value of a parameter, if it has one.
pub(crate) fn retained<'a>(
    values: &'a [ParameterValue],
    parameter_id: &str,
) -> Option<&'a parameter_value::Value> {
    values
        .iter()
        .find(|value| value.parameter_id == parameter_id)
        .and_then(|value| value.value.as_ref())
}

/// The default the parameter's own definition declares; secrets have none.
pub(crate) fn definition_default(
    definition: &ParameterDefinition,
) -> Option<parameter_value::Value> {
    use parameter_definition::ParameterType;

    match definition.parameter_type.as_ref()? {
        ParameterType::Boolean(schema) => schema.default_value.map(parameter_value::Value::Boolean),
        ParameterType::UnsignedInteger(schema) => schema
            .default_value
            .map(parameter_value::Value::UnsignedInteger),
        ParameterType::StringEnum(schema) => schema
            .default_value
            .clone()
            .map(parameter_value::Value::String),
        ParameterType::Text(schema) => schema
            .default_value
            .clone()
            .map(parameter_value::Value::String),
        ParameterType::Secret(_) => None,
    }
}

/// The record key suffix of a parameter: its id upper-cased, with every non-alphanumeric
/// character replaced by an underscore.
pub(crate) fn record_key(id: &str) -> String {
    id.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// One record line per row under the given key prefix.
pub(crate) fn record_lines(prefix: &str, rows: &[ParameterRow]) -> String {
    rows.iter()
        .map(|row| format!("{prefix}_{}={}\n", record_key(&row.id), row.value))
        .collect()
}

fn display(value: &parameter_value::Value, definition: &ParameterDefinition) -> String {
    match value {
        parameter_value::Value::Boolean(value) => value.to_string(),
        parameter_value::Value::UnsignedInteger(value) => {
            match definition.parameter_type.as_ref() {
                Some(parameter_definition::ParameterType::UnsignedInteger(schema))
                    if !schema.unit.is_empty() =>
                {
                    format!("{value} {}", schema.unit)
                }
                _ => value.to_string(),
            }
        }
        parameter_value::Value::String(value) => value.clone(),
    }
}
