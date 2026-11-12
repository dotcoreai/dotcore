//! Keeps secrets out of the master catalog's application declarations.
//!
//! A value the catalog itself sets on a service's environment ships to every site, so a variable
//! whose name marks it as a secret must get its value from a secret parameter instead. An empty
//! value stays allowed: it only tells the service that no secret is set.

use std::collections::BTreeMap;

use anyhow::anyhow;
use dotcore_proto::dotcore::catalog::v1::{
    Application, ApplicationConfigurationBinding, ParameterDefinition,
    application_configuration_binding::Source,
};

use crate::{
    is_sensitive_name,
    typed_parameters::{canonical_text, default_value},
};

/// Returns one error per configuration binding that sets a non-empty catalog value, either a
/// literal or a parameter default, on an environment variable with a sensitive name. Errors name
/// the application, service, binding and variable, never the value.
pub(crate) fn validate_sensitive_literals(application: &Application) -> Vec<anyhow::Error> {
    let Some(graph) = application.graph.as_ref() else {
        return Vec::new();
    };
    let parameters = graph
        .parameters
        .iter()
        .map(|definition| (definition.parameter_id.as_str(), definition))
        .collect::<BTreeMap<_, _>>();

    let mut errors = Vec::new();
    for service in &graph.services {
        for binding in &service.configuration_bindings {
            if !is_sensitive_name(&binding.environment_variable)
                || !binds_catalog_value(binding, &parameters)
            {
                continue;
            }
            errors.push(anyhow!(
                "application {:?} service {:?} configuration binding {:?} sets a catalog value on sensitive variable {:?}; bind a secret parameter instead",
                application.application_id,
                service.service_name,
                binding.binding_name,
                binding.environment_variable
            ));
        }
    }
    errors
}

/// Whether the binding's environment value comes from the catalog and is not empty.
fn binds_catalog_value(
    binding: &ApplicationConfigurationBinding,
    parameters: &BTreeMap<&str, &ParameterDefinition>,
) -> bool {
    match binding.source.as_ref() {
        Some(Source::LiteralValue(value)) => !value.is_empty(),
        Some(Source::ParameterId(id)) => parameters
            .get(id.as_str())
            .and_then(|definition| default_value(definition))
            .is_some_and(|value| !canonical_text(&value).is_empty()),
        Some(Source::ResolvedValueName(_)) | None => false,
    }
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::catalog::v1::{
        ApplicationConfigurationBinding, ParameterDefinition, ParameterVisibility,
        TextParameterSchema, application_configuration_binding::Source,
        parameter_definition::ParameterType,
    };

    use super::validate_sensitive_literals;
    use crate::test_support::application;

    #[test]
    fn non_empty_literal_on_a_sensitive_variable_is_rejected_without_its_value() {
        let mut application = application();
        add_binding(
            &mut application,
            "api-key",
            "SERVICE_API_KEY",
            Source::LiteralValue("hunter2".to_owned()),
        );

        let errors = messages(&application);

        assert_eq!(
            errors,
            [
                "application \"fixture-application\" service \"web\" configuration binding \"api-key\" sets a catalog value on sensitive variable \"SERVICE_API_KEY\"; bind a secret parameter instead"
            ]
        );
        assert!(!errors[0].contains("hunter2"));
    }

    #[test]
    fn empty_literals_and_non_sensitive_variables_are_allowed() {
        let mut application = application();
        add_binding(
            &mut application,
            "openai-api-key",
            "OPENAI_API_KEY",
            Source::LiteralValue(String::new()),
        );
        add_binding(
            &mut application,
            "tokenizer",
            "TOKENIZER_PATH",
            Source::LiteralValue("/models/tokenizer".to_owned()),
        );
        add_binding(
            &mut application,
            "endpoint-token",
            "ENDPOINT_TOKEN",
            Source::ResolvedValueName("published-endpoint-url".to_owned()),
        );

        assert_eq!(messages(&application), Vec::<String>::new());
    }

    #[test]
    fn parameter_default_on_a_sensitive_variable_is_rejected_unless_empty() {
        let mut application = application();
        add_text_parameter(&mut application, "admin-password", "changeme");
        add_text_parameter(&mut application, "upstream-token", "");
        add_binding(
            &mut application,
            "admin-password",
            "ADMIN_PASSWORD",
            Source::ParameterId("admin-password".to_owned()),
        );
        add_binding(
            &mut application,
            "upstream-token",
            "UPSTREAM_TOKEN",
            Source::ParameterId("upstream-token".to_owned()),
        );

        assert_eq!(
            messages(&application),
            [
                "application \"fixture-application\" service \"web\" configuration binding \"admin-password\" sets a catalog value on sensitive variable \"ADMIN_PASSWORD\"; bind a secret parameter instead"
            ]
        );
    }

    fn messages(application: &dotcore_proto::dotcore::catalog::v1::Application) -> Vec<String> {
        validate_sensitive_literals(application)
            .into_iter()
            .map(|error| error.to_string())
            .collect()
    }

    fn add_binding(
        application: &mut dotcore_proto::dotcore::catalog::v1::Application,
        binding_name: &str,
        environment_variable: &str,
        source: Source,
    ) {
        application.graph.as_mut().expect("graph").services[0]
            .configuration_bindings
            .push(ApplicationConfigurationBinding {
                binding_name: binding_name.to_owned(),
                environment_variable: environment_variable.to_owned(),
                source: Some(source),
                input: None,
            });
    }

    fn add_text_parameter(
        application: &mut dotcore_proto::dotcore::catalog::v1::Application,
        parameter_id: &str,
        default_value: &str,
    ) {
        application
            .graph
            .as_mut()
            .expect("graph")
            .parameters
            .push(ParameterDefinition {
                parameter_id: parameter_id.to_owned(),
                visibility: ParameterVisibility::Basic as i32,
                parameter_type: Some(ParameterType::Text(TextParameterSchema {
                    default_value: Some(default_value.to_owned()),
                    ..Default::default()
                })),
                ..Default::default()
            });
    }
}
