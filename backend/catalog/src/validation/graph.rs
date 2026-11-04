//! Application graph identity, reference, and cycle validation.

use std::collections::{BTreeMap, BTreeSet};

use dotcore_domain::valid_id;
use dotcore_proto::dotcore::catalog::v1::{
    ApplicationDependencyCondition, ApplicationEndpointProtocol, ApplicationGraph,
    ApplicationService, application_configuration_binding, parameter_definition::ParameterType,
};

use super::application::valid_configuration_value;
use super::service::validate_application_service;
use crate::OciArtifactPolicy;
use crate::typed_parameters::{default_value, validate_parameter_definition};

pub(super) fn validate_application_graph(
    graph: &ApplicationGraph,
    oci_artifact_policy: OciArtifactPolicy,
) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    if graph.services.is_empty() {
        errors.push(anyhow::anyhow!(
            "application graph must contain at least one service"
        ));
    }
    let mut service_names = BTreeSet::new();
    for service in &graph.services {
        validate_service_identity(service, &mut service_names, &mut errors);
        errors.extend(validate_application_service(service, oci_artifact_policy));
    }
    let has_duplicate_services = service_names.len() != graph.services.len();
    for service in &graph.services {
        errors.extend(validate_service_references(
            service,
            &service_names,
            !has_duplicate_services,
        ));
    }
    validate_primary_endpoint(graph, has_duplicate_services, &mut errors);
    validate_parameters(graph, &mut errors);
    if !has_duplicate_services && application_dependencies_are_cyclic(graph, &service_names) {
        errors.push(anyhow::anyhow!(
            "application service dependencies must be acyclic"
        ));
    }
    errors
}

fn validate_service_identity<'a>(
    service: &'a ApplicationService,
    service_names: &mut BTreeSet<&'a str>,
    errors: &mut Vec<anyhow::Error>,
) {
    if !valid_id(&service.service_name) {
        errors.push(anyhow::anyhow!(
            "application service name {:?} must match [a-z][a-z0-9-]{{0,62}}",
            service.service_name
        ));
    }
    if !service_names.insert(service.service_name.as_str()) {
        errors.push(anyhow::anyhow!(
            "duplicate application service name {:?}",
            service.service_name
        ));
    }
}

fn validate_service_references(
    service: &ApplicationService,
    service_names: &BTreeSet<&str>,
    validate_cross_service_references: bool,
) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    let mut dependency_names = BTreeSet::new();
    for dependency in &service.dependencies {
        if !dependency_names.insert(dependency.service_name.as_str()) {
            errors.push(anyhow::anyhow!(
                "duplicate dependency service {:?} on service {:?}",
                dependency.service_name,
                service.service_name
            ));
        }
        if validate_cross_service_references
            && !service_names.contains(dependency.service_name.as_str())
        {
            errors.push(anyhow::anyhow!(
                "application service {:?} references missing dependency service {:?}",
                service.service_name,
                dependency.service_name
            ));
        }
        if !matches!(
            ApplicationDependencyCondition::try_from(dependency.condition),
            Ok(ApplicationDependencyCondition::Started | ApplicationDependencyCondition::Ready)
        ) {
            errors.push(anyhow::anyhow!(
                "application service {:?} dependency {:?} requires STARTED or READY",
                service.service_name,
                dependency.service_name
            ));
        }
    }
    if let Some(readiness) = service.readiness.as_ref() {
        match readiness.check.as_ref() {
            Some(
                dotcore_proto::dotcore::catalog::v1::application_service_readiness::Check::HttpGet(
                    http_get,
                ),
            ) => {
                if !service
                    .ports
                    .iter()
                    .any(|port| port.port_name == http_get.port_name)
                {
                    errors.push(anyhow::anyhow!(
                        "application service {:?} readiness references missing port {:?}",
                        service.service_name,
                        http_get.port_name
                    ));
                }
                if !http_get.path.starts_with('/') {
                    errors.push(anyhow::anyhow!(
                        "application service {:?} readiness must use an absolute HTTP path",
                        service.service_name
                    ));
                }
            }
            None => errors.push(anyhow::anyhow!(
                "application service {:?} readiness requires an HTTP check",
                service.service_name
            )),
        }
    }
    errors
}

fn validate_primary_endpoint(
    graph: &ApplicationGraph,
    has_duplicate_services: bool,
    errors: &mut Vec<anyhow::Error>,
) {
    let Some(endpoint) = graph.primary_endpoint.as_ref() else {
        errors.push(anyhow::anyhow!(
            "application graph requires exactly one primary endpoint"
        ));
        return;
    };
    if has_duplicate_services {
        return;
    }
    if ApplicationEndpointProtocol::try_from(endpoint.protocol).ok()
        != Some(ApplicationEndpointProtocol::Http)
    {
        errors.push(anyhow::anyhow!(
            "application primary endpoint must use HTTP"
        ));
    }
    if let Some(service) = graph
        .services
        .iter()
        .find(|service| service.service_name == endpoint.service_name)
    {
        if !service
            .ports
            .iter()
            .any(|port| port.port_name == endpoint.port_name)
        {
            errors.push(anyhow::anyhow!(
                "application primary endpoint references missing port {:?} on service {:?}",
                endpoint.port_name,
                endpoint.service_name
            ));
        }
    } else {
        errors.push(anyhow::anyhow!(
            "application primary endpoint references missing service {:?}",
            endpoint.service_name
        ));
    }
}

/// Checks parameter definitions and that bindings and parameters reference each other exactly:
/// every binding names a declared parameter and every parameter supplies at least one binding.
fn validate_parameters(graph: &ApplicationGraph, errors: &mut Vec<anyhow::Error>) {
    let mut parameter_ids = BTreeSet::new();
    for parameter in &graph.parameters {
        let Some(definition) = parameter.definition.as_ref() else {
            errors.push(anyhow::anyhow!(
                "application parameter requires a definition"
            ));
            continue;
        };
        let id = &definition.parameter_id;
        if !parameter_ids.insert(id.as_str()) {
            errors.push(anyhow::anyhow!("duplicate application parameter {id:?}"));
        }
        let definition_errors = validate_parameter_definition(definition);
        let valid_definition = definition_errors.is_empty();
        errors.extend(definition_errors);
        if valid_definition && default_value(definition).is_none() {
            errors.push(anyhow::anyhow!(
                "application parameter {id:?} requires a default"
            ));
        }
        // Enum values are projected verbatim into environment variables.
        if let Some(ParameterType::StringEnum(schema)) = definition.parameter_type.as_ref()
            && !schema
                .values
                .iter()
                .all(|value| valid_configuration_value(value))
        {
            errors.push(anyhow::anyhow!(
                "application parameter {id:?} has an unsafe enum value"
            ));
        }
    }

    let mut referenced_ids = BTreeSet::new();
    for service in &graph.services {
        for binding in &service.configuration_bindings {
            let Some(application_configuration_binding::Source::ParameterId(id)) =
                binding.source.as_ref()
            else {
                continue;
            };
            referenced_ids.insert(id.as_str());
            if !parameter_ids.contains(id.as_str()) {
                errors.push(anyhow::anyhow!(
                    "application configuration binding {:?} on service {:?} references missing parameter {id:?}",
                    binding.binding_name,
                    service.service_name
                ));
            }
        }
    }
    for id in parameter_ids.difference(&referenced_ids) {
        errors.push(anyhow::anyhow!(
            "application parameter {id:?} is not used by any configuration binding"
        ));
    }
}

fn application_dependencies_are_cyclic(
    graph: &ApplicationGraph,
    service_names: &BTreeSet<&str>,
) -> bool {
    let dependency_graph = graph
        .services
        .iter()
        .map(|service| {
            (
                service.service_name.as_str(),
                service
                    .dependencies
                    .iter()
                    .map(|dependency| dependency.service_name.as_str())
                    .filter(|dependency| service_names.contains(dependency))
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    dependency_graph
        .keys()
        .any(|service| dependency_cycle(service, &dependency_graph, &mut visiting, &mut visited))
}

fn dependency_cycle<'a>(
    service: &'a str,
    dependencies: &BTreeMap<&'a str, Vec<&'a str>>,
    visiting: &mut BTreeSet<&'a str>,
    visited: &mut BTreeSet<&'a str>,
) -> bool {
    if visited.contains(service) {
        return false;
    }
    if !visiting.insert(service) {
        return true;
    }
    let cyclic = dependencies
        .get(service)
        .is_some_and(|service_dependencies| {
            service_dependencies
                .iter()
                .any(|dependency| dependency_cycle(dependency, dependencies, visiting, visited))
        });
    visiting.remove(service);
    visited.insert(service);
    cyclic
}
