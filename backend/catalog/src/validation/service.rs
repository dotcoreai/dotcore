//! Application service artifact, port, and binding validation.

use std::collections::BTreeSet;

use dotcore_domain::valid_id;
use dotcore_proto::dotcore::catalog::v1::{
    ApplicationInferenceRouteProjection, ApplicationSecretForm, ApplicationService,
    application_configuration_binding, application_deployment_input,
    application_service_artifact::Kind as ServiceArtifactKind,
};

use super::{
    application::{valid_configuration_value, valid_environment_variable},
    common::{validate_application_artifact_platforms, validate_oci_artifact},
    nix::{AcceptedProvides, nix_systems, validate_nix_artifact},
    storage::{StorageOwner, validate_storage_bindings},
};
use crate::OciArtifactPolicy;

pub(super) fn validate_application_service(
    service: &ApplicationService,
    oci_artifact_policy: OciArtifactPolicy,
) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    match service.artifact.as_ref() {
        Some(artifact) => {
            match artifact.kind.as_ref() {
                Some(ServiceArtifactKind::Oci(reference)) => {
                    if let Err(error) = validate_oci_artifact(
                        reference,
                        &artifact.supported_platforms,
                        oci_artifact_policy,
                    ) {
                        errors.push(anyhow::anyhow!(
                            "application service {:?} has an invalid OCI artifact: {error}",
                            service.service_name
                        ));
                    }
                }
                Some(ServiceArtifactKind::Nix(nix)) => {
                    let platforms = artifact.supported_platforms.iter().map(|platform| {
                        (
                            platform.operating_system.as_str(),
                            platform.architecture.as_str(),
                        )
                    });
                    let systems = nix_systems(nix, platforms);
                    if let Err(error) =
                        validate_nix_artifact(nix, &systems, AcceptedProvides::DockerImage)
                    {
                        errors.push(anyhow::anyhow!(
                            "application service {:?} has an invalid Nix artifact: {error}",
                            service.service_name
                        ));
                    }
                }
                None => errors.push(anyhow::anyhow!(
                    "application service {:?} requires an OCI or Nix artifact",
                    service.service_name
                )),
            }
            if let Err(error) = validate_application_artifact_platforms(artifact) {
                errors.push(anyhow::anyhow!(
                    "application service {:?} has an invalid artifact: {error}",
                    service.service_name
                ));
            }
        }
        None => errors.push(anyhow::anyhow!(
            "application service {:?} requires an artifact",
            service.service_name
        )),
    }
    if service
        .command
        .iter()
        .chain(&service.arguments)
        .any(String::is_empty)
    {
        errors.push(anyhow::anyhow!(
            "application service {:?} command and arguments must not contain empty values",
            service.service_name
        ));
    }
    if service
        .command
        .iter()
        .chain(&service.arguments)
        .any(|value| value.chars().any(char::is_control))
    {
        errors.push(anyhow::anyhow!(
            "application service {:?} command and arguments must not contain control characters",
            service.service_name
        ));
    }
    validate_ports(service, &mut errors);
    validate_bindings(service, &mut errors);
    errors
}

fn validate_ports(service: &ApplicationService, errors: &mut Vec<anyhow::Error>) {
    let mut port_names = BTreeSet::new();
    for port in &service.ports {
        if !valid_id(&port.port_name) || port.port_number == 0 || port.port_number > u16::MAX as u32
        {
            errors.push(anyhow::anyhow!(
                "application service {:?} port requires a name and an internal TCP port from 1 to 65535",
                service.service_name
            ));
        }
        if !port_names.insert(port.port_name.as_str()) {
            errors.push(anyhow::anyhow!(
                "duplicate application service port name {:?} on service {:?}",
                port.port_name,
                service.service_name
            ));
        }
    }
}

fn validate_bindings(service: &ApplicationService, errors: &mut Vec<anyhow::Error>) {
    let mut binding_names = BTreeSet::new();
    let mut environment_variables = BTreeSet::new();
    for binding in &service.configuration_bindings {
        validate_configuration_binding(
            binding,
            &mut binding_names,
            &mut environment_variables,
            errors,
        );
    }
    validate_storage_bindings(
        &service.storage_bindings,
        StorageOwner::ApplicationService,
        &format!("application service {:?}", service.service_name),
        &mut binding_names,
        &mut environment_variables,
        errors,
    );
    for binding in &service.secret_bindings {
        validate_binding(
            &binding.binding_name,
            &binding.environment_variable,
            &mut binding_names,
            &mut environment_variables,
            "secret",
            errors,
        );
        if !matches!(
            ApplicationSecretForm::try_from(binding.form),
            Ok(ApplicationSecretForm::Plain)
        ) {
            errors.push(anyhow::anyhow!(
                "application secret binding {:?} requires a known form",
                binding.binding_name
            ));
        }
    }
}

fn validate_configuration_binding(
    binding: &dotcore_proto::dotcore::catalog::v1::ApplicationConfigurationBinding,
    binding_names: &mut BTreeSet<String>,
    environment_variables: &mut BTreeSet<String>,
    errors: &mut Vec<anyhow::Error>,
) {
    validate_binding(
        &binding.binding_name,
        &binding.environment_variable,
        binding_names,
        environment_variables,
        "configuration",
        errors,
    );
    match binding.source.as_ref() {
        Some(application_configuration_binding::Source::LiteralValue(value))
            if valid_configuration_value(value) => {}
        Some(application_configuration_binding::Source::LiteralValue(_)) => {
            errors.push(anyhow::anyhow!(
                "application configuration binding {:?} has an unsafe literal value",
                binding.binding_name
            ));
        }
        Some(application_configuration_binding::Source::ResolvedValueName(name))
            if valid_id(name) => {}
        Some(application_configuration_binding::Source::ResolvedValueName(_)) => {
            errors.push(anyhow::anyhow!(
                "application configuration binding {:?} requires a valid resolved value name",
                binding.binding_name
            ));
        }
        Some(application_configuration_binding::Source::ParameterId(id)) if valid_id(id) => {}
        Some(application_configuration_binding::Source::ParameterId(_)) => {
            errors.push(anyhow::anyhow!(
                "application configuration binding {:?} requires a valid parameter id",
                binding.binding_name
            ));
        }
        None => errors.push(anyhow::anyhow!(
            "application configuration binding {:?} requires exactly one source",
            binding.binding_name
        )),
    }
    validate_deployment_input(binding, errors);
}

fn validate_deployment_input(
    binding: &dotcore_proto::dotcore::catalog::v1::ApplicationConfigurationBinding,
    errors: &mut Vec<anyhow::Error>,
) {
    match binding.input.as_ref().and_then(|input| input.kind.as_ref()) {
        None if binding.input.is_some() => errors.push(anyhow::anyhow!(
            "application deployment input {:?} requires a known kind",
            binding.binding_name
        )),
        Some(application_deployment_input::Kind::InferenceRoute(input))
            if !matches!(
                ApplicationInferenceRouteProjection::try_from(input.projection),
                Ok(ApplicationInferenceRouteProjection::RouteName)
            ) =>
        {
            errors.push(anyhow::anyhow!(
                "application inference route input {:?} requires a known projection",
                binding.binding_name
            ));
        }
        // A deployment without a linked route launches with the literal fallback value.
        Some(application_deployment_input::Kind::InferenceRoute(_))
            if !matches!(
                binding.source,
                Some(application_configuration_binding::Source::LiteralValue(_))
            ) =>
        {
            errors.push(anyhow::anyhow!(
                "application inference route input {:?} requires a literal fallback value",
                binding.binding_name
            ));
        }
        _ => {}
    }
}

fn validate_binding(
    binding_name: &str,
    environment_variable: &str,
    binding_names: &mut BTreeSet<String>,
    environment_variables: &mut BTreeSet<String>,
    kind: &str,
    errors: &mut Vec<anyhow::Error>,
) {
    if !binding_names.insert(binding_name.to_owned()) {
        errors.push(anyhow::anyhow!(
            "duplicate application binding name {binding_name:?}"
        ));
    } else if !valid_id(binding_name) || !valid_environment_variable(environment_variable) {
        errors.push(anyhow::anyhow!(
            "application {kind} binding requires a name and environment destination"
        ));
    }
    if !environment_variables.insert(environment_variable.to_owned()) {
        errors.push(anyhow::anyhow!(
            "duplicate application environment destination {environment_variable:?}"
        ));
    }
}
