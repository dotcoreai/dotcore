//! Whether an application configuration or secret binding reaches its service.
//!
//! A binding fed by a parameter reaches its service only while that parameter is enabled: a
//! disabled parameter keeps no value, so the variable is not set at all. Other bindings always
//! reach their service. Creation, launch and the retained declaration checks all decide through
//! this one rule.

use std::collections::BTreeSet;

use dotcore_catalog::parameter_groups::retained_enabled_parameters;
use dotcore_proto::dotcore::catalog::v1::{
    ApplicationConfigurationBinding, ApplicationGraph, ApplicationSecretBinding, ParameterValue,
    application_configuration_binding,
};

/// Returns the ids of a deployment's enabled parameters, given its resolved parameter values.
/// Returns `None` when the values don't hold exactly one value per enabled parameter.
pub(super) fn enabled_parameters<'a>(
    graph: &'a ApplicationGraph,
    parameter_values: &[ParameterValue],
) -> Option<BTreeSet<&'a str>> {
    retained_enabled_parameters(&graph.parameters, &graph.parameter_groups, parameter_values)
}

/// Returns whether a configuration binding reaches its service: always, unless a disabled
/// parameter feeds it.
pub(super) fn configuration_binding_enabled(
    binding: &ApplicationConfigurationBinding,
    enabled_parameters: &BTreeSet<&str>,
) -> bool {
    match binding.source.as_ref() {
        Some(application_configuration_binding::Source::ParameterId(id)) => {
            enabled_parameters.contains(id.as_str())
        }
        _ => true,
    }
}

/// Returns whether a secret binding reaches its service: only while its parameter is enabled.
pub(super) fn secret_binding_enabled(
    binding: &ApplicationSecretBinding,
    enabled_parameters: &BTreeSet<&str>,
) -> bool {
    enabled_parameters.contains(binding.parameter_id.as_str())
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::catalog::v1::{
        BooleanParameterSchema, ParameterDefinition, ParameterGroup, ParameterVisibility,
        SecretParameterSchema, parameter_definition::ParameterType, parameter_value::Value,
    };

    use super::*;

    #[test]
    fn bindings_follow_their_parameter_and_others_always_reach_the_service() {
        let graph = ApplicationGraph {
            parameters: vec![
                login_member("login", boolean()),
                login_member("password", secret()),
            ],
            parameter_groups: vec![ParameterGroup {
                group_id: "login".to_owned(),
                switch_parameter_id: Some("login".to_owned()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let off = [ParameterValue {
            parameter_id: "login".to_owned(),
            value: Some(Value::Boolean(false)),
        }];
        let enabled = enabled_parameters(&graph, &off).expect("consistent values");
        let parameter_binding = ApplicationConfigurationBinding {
            source: Some(application_configuration_binding::Source::ParameterId(
                "login".to_owned(),
            )),
            ..Default::default()
        };
        let literal_binding = ApplicationConfigurationBinding {
            source: Some(application_configuration_binding::Source::LiteralValue(
                "auto".to_owned(),
            )),
            ..Default::default()
        };
        let secret_binding = ApplicationSecretBinding {
            parameter_id: "password".to_owned(),
            ..Default::default()
        };

        assert_eq!(enabled, BTreeSet::from(["login"]));
        assert!(configuration_binding_enabled(&parameter_binding, &enabled));
        assert!(configuration_binding_enabled(&literal_binding, &enabled));
        assert!(!secret_binding_enabled(&secret_binding, &enabled));
        assert!(secret_binding_enabled(
            &secret_binding,
            &BTreeSet::from(["login", "password"])
        ));
        assert_eq!(enabled_parameters(&graph, &[]), None);
    }

    fn login_member(id: &str, parameter_type: ParameterType) -> ParameterDefinition {
        ParameterDefinition {
            parameter_id: id.to_owned(),
            display_name: id.to_owned(),
            description: String::new(),
            visibility: ParameterVisibility::Basic as i32,
            group_id: Some("login".to_owned()),
            enabled_by_parameter_id: None,
            parameter_type: Some(parameter_type),
        }
    }

    fn boolean() -> ParameterType {
        ParameterType::Boolean(BooleanParameterSchema {
            default_value: Some(false),
        })
    }

    fn secret() -> ParameterType {
        ParameterType::Secret(SecretParameterSchema {
            generated_length: 32,
            minimum_length: 12,
            maximum_length: None,
        })
    }
}
