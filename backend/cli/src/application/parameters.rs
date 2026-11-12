//! The parameters of one application deployment, as rows an operator can read.
//!
//! Each row carries the parameter's display name and its value, tagged when the value is the
//! catalog default or was generated. Secret values are masked unless the caller asks to reveal
//! them.

use dotcore_proto::dotcore::{
    application::v1::ApplicationDeployment,
    catalog::v1::{ParameterDefinition, parameter_definition, parameter_value},
};

use crate::management::single_line;

const SECRET_MASK: &str = "********";
const EMPTY_TEXT: &str = "Empty";

/// One shown parameter.
pub(super) struct ParameterRow {
    /// Stable parameter identity.
    pub(super) id: String,
    /// Human-readable parameter label.
    pub(super) name: String,
    /// The value with its `(default)` and `(generated)` tags.
    pub(super) value: String,
}

/// The deployment's parameters in catalog order. A parameter without a retained value shows its
/// default, and one with neither is left out.
pub(super) fn rows(deployment: &ApplicationDeployment, reveal_secrets: bool) -> Vec<ParameterRow> {
    let Some(snapshot) = deployment.snapshot.as_ref() else {
        return Vec::new();
    };
    let Some(graph) = snapshot.graph.as_ref() else {
        return Vec::new();
    };

    graph
        .parameters
        .iter()
        .filter_map(|definition| {
            let retained = snapshot
                .parameter_values
                .iter()
                .find(|value| value.parameter_id == definition.parameter_id)
                .and_then(|value| value.value.as_ref());
            let default = default_of(definition);
            let value = retained.or(default.as_ref())?;
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
            if default.as_ref() == Some(value) {
                text.push_str(" (default)");
            }
            if snapshot
                .generated_parameter_ids
                .contains(&definition.parameter_id)
            {
                text.push_str(" (generated)");
            }

            Some(ParameterRow {
                id: definition.parameter_id.clone(),
                name: single_line(&definition.display_name),
                value: text,
            })
        })
        .collect()
}

/// The record key suffix of a parameter: its id upper-cased, with every non-alphanumeric
/// character replaced by an underscore.
pub(super) fn record_key(id: &str) -> String {
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

fn default_of(definition: &ParameterDefinition) -> Option<parameter_value::Value> {
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
