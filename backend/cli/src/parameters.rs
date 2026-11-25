//! Parameters of a deployment as rows an operator can read.
//!
//! Each row carries the parameter's display name and its value. Secret values are masked unless
//! the caller asks to reveal them.

use dotcore_catalog::typed_parameters::is_secret;
use dotcore_proto::dotcore::catalog::v1::{
    ParameterDefinition, ParameterValue, parameter_definition, parameter_value,
};

use crate::management::single_line;
use crate::terminal::Message;

pub(crate) const SECRET_MASK: &str = "********";
const EMPTY_TEXT: &str = "Empty";

/// One shown parameter.
pub(crate) struct ParameterRow {
    /// Human-readable parameter label.
    pub(crate) name: String,
    /// The value as shown.
    pub(crate) value: String,
}

/// What a deployment retains about one parameter, from which its row is built.
pub(crate) struct ParameterInput<'a> {
    /// The parameter as the deployment snapshot declares it.
    pub(crate) definition: &'a ParameterDefinition,
    /// The retained value, if any.
    pub(crate) value: Option<&'a parameter_value::Value>,
    /// The value the deployment's owner defaults to, shown when nothing is retained.
    pub(crate) default: Option<parameter_value::Value>,
}

impl ParameterInput<'_> {
    /// The row of this parameter. A parameter without a retained value shows its default, and one
    /// with neither is left out.
    pub(crate) fn row(&self, reveal_secrets: bool) -> Option<ParameterRow> {
        let definition = self.definition;
        let value = self.value.or(self.default.as_ref())?;
        let text = if is_secret(definition) && !reveal_secrets {
            SECRET_MASK.to_owned()
        } else {
            match single_line(&display(value, definition)) {
                empty if empty.is_empty() => EMPTY_TEXT.to_owned(),
                text => text,
            }
        };
        Some(ParameterRow {
            name: single_line(&definition.display_name),
            value: text,
        })
    }
}

/// The row of a secret that has no parameter definition, masked unless revealed.
pub(crate) fn secret_row(name: &str, secret: &str, reveal_secrets: bool) -> ParameterRow {
    ParameterRow {
        name: name.to_owned(),
        value: if reveal_secrets {
            single_line(secret)
        } else {
            SECRET_MASK.to_owned()
        },
    }
}

/// Replaces the values of the secret parameters among `definitions` with the mask, leaving every
/// other value as it is.
pub(crate) fn mask_secret_values(
    definitions: &[ParameterDefinition],
    values: &mut [ParameterValue],
) {
    for value in values {
        let secret = definitions.iter().any(|definition| {
            definition.parameter_id == value.parameter_id && is_secret(definition)
        });
        if secret && value.value.is_some() {
            value.value = Some(parameter_value::Value::String(SECRET_MASK.to_owned()));
        }
    }
}

/// Returns the mask shown in place of a secret value.
pub(crate) fn masked(secret: &str) -> String {
    if secret.is_empty() {
        String::new()
    } else {
        SECRET_MASK.to_owned()
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

/// The messages that show the parameter rows under a Parameters heading, or none when there are
/// no rows.
pub(crate) fn parameter_messages(rows: &[ParameterRow]) -> Vec<Message> {
    if rows.is_empty() {
        return Vec::new();
    }
    vec![
        Message::Heading("Parameters".to_owned()),
        Message::KeyValues(
            rows.iter()
                .map(|row| (row.name.clone(), row.value.clone()))
                .collect(),
        ),
    ]
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
