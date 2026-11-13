//! Whether an application configuration or secret binding reaches its service.
//!
//! A binding may be enabled by a boolean parameter of its graph: the service receives it only
//! when that parameter's resolved value is true, and otherwise the variable is not set at all.
//! Creation, launch and the retained declaration checks all decide through this one rule.

use dotcore_proto::dotcore::catalog::v1::{ParameterValue, parameter_value::Value};

/// Returns whether a binding enabled by `enabled_by_parameter_id` reaches its service, given a
/// deployment's resolved parameter values. A binding without an enabling parameter always does.
/// Returns `None` when the enabling parameter has no boolean value.
pub(super) fn binding_enabled(
    enabled_by_parameter_id: Option<&str>,
    parameter_values: &[ParameterValue],
) -> Option<bool> {
    let Some(parameter_id) = enabled_by_parameter_id else {
        return Some(true);
    };
    match parameter_values
        .iter()
        .find(|value| value.parameter_id == parameter_id)?
        .value
    {
        Some(Value::Boolean(enabled)) => Some(enabled),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_enabled_follows_its_boolean_parameter() {
        let values = [
            boolean("login", true),
            boolean("signup", false),
            ParameterValue {
                parameter_id: "mode".to_owned(),
                value: Some(Value::String("auto".to_owned())),
            },
        ];

        assert_eq!(binding_enabled(None, &values), Some(true));
        assert_eq!(binding_enabled(Some("login"), &values), Some(true));
        assert_eq!(binding_enabled(Some("signup"), &values), Some(false));
        assert_eq!(binding_enabled(Some("mode"), &values), None);
        assert_eq!(binding_enabled(Some("missing"), &values), None);
    }

    fn boolean(parameter_id: &str, value: bool) -> ParameterValue {
        ParameterValue {
            parameter_id: parameter_id.to_owned(),
            value: Some(Value::Boolean(value)),
        }
    }
}
