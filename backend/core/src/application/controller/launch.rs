//! Apps-controller compilation of immutable application declarations into runtime launches.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::Ipv4Addr,
    num::NonZeroU16,
    time::Duration,
};

use dotcore_catalog::typed_parameters::canonical_value_text;
use dotcore_catalog::valid_configuration_value;
use dotcore_domain::ExecutionPlatform;
use dotcore_domain::deployment::launch_pin::RetainedArtifact;
use dotcore_domain::valid_id;
use dotcore_domain::{SiteNodeHost, SiteNodeId};
use dotcore_proto::dotcore::{
    application::v1::{
        ApplicationConfigurationAttachment, ApplicationDeploymentDeclaration,
        ApplicationSecretAttachment, ApplicationStorageAttachment, ApplicationStorageChoice,
    },
    catalog::v1::{
        Application, ApplicationDependencyCondition, ApplicationGraph,
        ApplicationInferenceRouteProjection, ApplicationSecretForm, ParameterValue,
        StoragePlacement, application_configuration_binding, application_deployment_input,
        application_service_readiness,
    },
};
use dotcore_runtime::{
    ExecutionTarget, ReadyLaunchGate, WorkloadDependencyCondition, WorkloadGroupId,
    WorkloadGroupLaunch, WorkloadPublicationRequest, WorkloadStorageAttachment, WorkloadStorageKey,
    WorkloadStorageKind, WorkloadUnitDependency, WorkloadUnitLaunch, WorkloadUnitName,
};
use tracing::debug;

use super::{
    ApplicationConfigurationSources, ApplicationLifecycleError, InferenceRouteResolver,
    ResolvedInferenceRoute, StorageChoiceRejection, configuration, configuration_with_source,
};
use crate::application::secret_form::bcrypt_hash;
use crate::storage::{deployment_storage, workload_storage};

pub(super) fn workload_group_launch(
    desired: &ApplicationDeploymentDeclaration,
    publication_host: Ipv4Addr,
    inference_route_resolver: &dyn InferenceRouteResolver,
) -> Result<WorkloadGroupLaunch, ApplicationLifecycleError> {
    let plan = LaunchPlan::compile(desired, publication_host)?;
    let route = plan.resolve_route(inference_route_resolver)?;
    let launch = plan.materialize(route.as_ref());
    if let Some(route) = route {
        debug!(
            event_kind = "application_launch_projected",
            deployment_id = %launch.group_id.0,
            inference_route_id = %route.inference_route_id,
            route_name = %route.route_name,
            reconcile_generation = launch.correlation.reconcile_generation,
            "Application launch projected its linked inference route"
        );
    }
    Ok(launch)
}

/// Returns the validated SiteNode identity declared for one application deployment.
pub(super) fn deployment_site_node_id(
    desired: &ApplicationDeploymentDeclaration,
) -> Result<SiteNodeId, ApplicationLifecycleError> {
    let placement = desired
        .placement
        .as_ref()
        .ok_or_else(|| configuration("Application deployment exact placement is missing"))?;
    SiteNodeId::parse(&placement.site_node_id).map_err(|source| {
        configuration_with_source("Application deployment node identity is invalid", source)
    })
}

/// Returns the data storage placed with one application deployment alone: its launch attaches
/// deployment-placed bindings under this key and its teardown removes it.
pub(super) fn deployment_data(desired: &ApplicationDeploymentDeclaration) -> WorkloadStorageKey {
    deployment_storage(WorkloadStorageKind::Data, &deployment_storage_key(desired))
}

/// Returns the retained host port a deployment's primary endpoint is published on.
fn published_port(
    desired: &ApplicationDeploymentDeclaration,
) -> Result<NonZeroU16, ApplicationLifecycleError> {
    u16::try_from(desired.published_port)
        .ok()
        .and_then(NonZeroU16::new)
        .ok_or_else(|| configuration("Application deployment published host port is invalid"))
}

/// Returns the key of storage placed with one application deployment.
fn deployment_storage_key(desired: &ApplicationDeploymentDeclaration) -> String {
    desired.application_deployment_id.clone()
}

struct LaunchPlan {
    deployment_id: String,
    site_node_id: SiteNodeId,
    execution_target_id: String,
    accelerator_device_ids: Vec<String>,
    required_platform: ExecutionPlatform,
    route_id: Option<String>,
    primary: LaunchPrimaryEndpoint,
    units: BTreeMap<WorkloadUnitName, WorkloadUnitPlan>,
    publication_host: Ipv4Addr,
    published_port: NonZeroU16,
}

impl LaunchPlan {
    fn compile(
        desired: &ApplicationDeploymentDeclaration,
        publication_host: Ipv4Addr,
    ) -> Result<Self, ApplicationLifecycleError> {
        let declaration = ValidatedLaunchDeclaration::extract(desired)?;
        let attachments = AttachmentIndexes::build(desired)?;
        let storage_key = |placement| match placement {
            StoragePlacement::Deployment => Some(deployment_storage_key(desired)),
            StoragePlacement::Application => Some(declaration.application_id.to_owned()),
            StoragePlacement::EngineArtifact | StoragePlacement::Unspecified => None,
        };
        let ready_dependency_names = declaration
            .services
            .iter()
            .flat_map(|service| &service.dependencies)
            .filter(|dependency| {
                dependency.condition == ApplicationDependencyCondition::Ready as i32
            })
            .map(|dependency| dependency.service_name.as_str())
            .collect::<BTreeSet<_>>();
        let units = declaration
            .services
            .iter()
            .map(|service| {
                let image = declaration
                    .service_images
                    .get(service.service_name.as_str())
                    .ok_or_else(|| configuration("Application service selection is missing"))?;
                WorkloadUnitPlan::compile(
                    service,
                    image,
                    &attachments,
                    &storage_key,
                    declaration.route_id.is_some(),
                    ready_dependency_names.contains(service.service_name.as_str()),
                )
                .map(|unit| (WorkloadUnitName(service.service_name.clone()), unit))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            deployment_id: desired.application_deployment_id.clone(),
            site_node_id: deployment_site_node_id(desired)?,
            execution_target_id: declaration.execution_target_id,
            accelerator_device_ids: declaration.accelerator_device_ids,
            required_platform: declaration.required_platform,
            route_id: declaration.route_id,
            primary: declaration.primary,
            units,
            publication_host,
            published_port: published_port(desired)?,
        })
    }

    fn resolve_route(
        &self,
        resolver: &dyn InferenceRouteResolver,
    ) -> Result<Option<ResolvedInferenceRoute>, ApplicationLifecycleError> {
        self.route_id
            .as_deref()
            .filter(|_| {
                self.units
                    .values()
                    .any(WorkloadUnitPlan::uses_route_projection)
            })
            .map(|route_id| {
                resolver
                    .project_current(route_id)
                    .map_err(ApplicationLifecycleError::InferenceRoute)
            })
            .transpose()
    }

    fn materialize(self, route: Option<&ResolvedInferenceRoute>) -> WorkloadGroupLaunch {
        let host_aliases = self
            .units
            .values()
            .flat_map(|unit| unit.host_aliases.iter().cloned())
            .collect();
        let units = self
            .units
            .into_iter()
            .map(|(name, unit)| (name, unit.materialize(route)))
            .collect();
        let execution_target = ExecutionTarget {
            managed_node_id: self.site_node_id.clone(),
            accelerator_device_ids: self.accelerator_device_ids,
        };
        let publication = WorkloadPublicationRequest {
            unit_name: WorkloadUnitName(self.primary.service_name),
            port_name: self.primary.port_name,
            configured_host: self.publication_host,
            host_port: self.published_port,
        };
        let reconcile_generation = canonical_launch_generation(
            &execution_target,
            &self.execution_target_id,
            &self.required_platform,
            &units,
            &publication,
        );
        WorkloadGroupLaunch {
            group_id: WorkloadGroupId(self.deployment_id.clone()),
            correlation: dotcore_runtime::WorkloadCorrelation {
                owner_id: self.deployment_id,
                reconcile_generation,
            },
            execution_target,
            execution_target_id: self.execution_target_id,
            required_platform: self.required_platform,
            units,
            publication,
            host_aliases,
        }
    }
}

#[derive(Debug)]
struct ValidatedLaunchDeclaration<'a> {
    application_id: &'a str,
    services: &'a [dotcore_proto::dotcore::catalog::v1::ApplicationService],
    service_images: BTreeMap<&'a str, String>,
    execution_target_id: String,
    accelerator_device_ids: Vec<String>,
    required_platform: ExecutionPlatform,
    route_id: Option<String>,
    primary: LaunchPrimaryEndpoint,
}

impl<'a> ValidatedLaunchDeclaration<'a> {
    fn extract(
        desired: &'a ApplicationDeploymentDeclaration,
    ) -> Result<Self, ApplicationLifecycleError> {
        let snapshot = desired
            .snapshot
            .as_ref()
            .ok_or_else(|| configuration("Application snapshot is missing"))?;
        let graph = snapshot
            .graph
            .as_ref()
            .ok_or_else(|| configuration("Application graph is missing"))?;
        let primary = graph
            .primary_endpoint
            .as_ref()
            .ok_or_else(|| configuration("Application primary endpoint is missing"))?;
        // Each service launches the image its pinned identity names: an OCI identity itself, or
        // the reference derived from a Docker-delivered Nix artifact's loader store path. The
        // graph's artifact must still be a valid reference; the repository validates the pair.
        let service_images = graph
            .services
            .iter()
            .filter_map(|service| {
                let selection = snapshot
                    .service_selections
                    .iter()
                    .find(|selection| selection.service_name == service.service_name)
                    .filter(|selection| !selection.launch_identity.is_empty())?;
                let artifact = RetainedArtifact::of_service(service.artifact.as_ref()?)?;
                match artifact {
                    RetainedArtifact::Oci(reference) => {
                        dotcore_domain::oci::render_reference(reference).ok()?;
                    }
                    // A Nix pin must be a loader of the retained artifact: a locked artifact's pin
                    // one of its catalog store paths.
                    RetainedArtifact::Nix(_) => {
                        dotcore_domain::deployment::artifact::validate_retained_launch_identity(
                            artifact,
                            selection.preparation_policy,
                            &selection.launch_identity,
                        )
                        .ok()?;
                    }
                }
                let image = artifact.container_image(&selection.launch_identity)?;
                Some((service.service_name.as_str(), image))
            })
            .collect::<BTreeMap<_, _>>();
        if service_images.len() != graph.services.len()
            || snapshot.service_selections.len() != graph.services.len()
        {
            return Err(configuration(
                "Application service selections do not match the retained graph",
            ));
        }
        let required_platform = snapshot
            .platform
            .as_ref()
            .ok_or_else(|| configuration("Application retained platform is missing"))
            .and_then(|platform| {
                ExecutionPlatform::try_from(platform).map_err(|source| {
                    configuration_with_source("Application retained platform is invalid", source)
                })
            })?;
        let placement = snapshot
            .placement
            .as_ref()
            .ok_or_else(|| configuration("Application retained exact placement is missing"))?;
        Ok(Self {
            application_id: &snapshot.application_id,
            services: &graph.services,
            service_images,
            execution_target_id: placement.execution_target_id.clone(),
            accelerator_device_ids: snapshot.accelerator_device_ids.clone(),
            required_platform,
            route_id: desired.inference_route_id.clone(),
            primary: LaunchPrimaryEndpoint {
                service_name: primary.service_name.clone(),
                port_name: primary.port_name.clone(),
            },
        })
    }
}

#[derive(Debug)]
struct LaunchPrimaryEndpoint {
    service_name: String,
    port_name: String,
}

/// Retained attachment values keyed by service and binding name. It holds secret values, so it
/// has no `Debug` form.
struct AttachmentIndexes {
    storage: BTreeMap<(String, String), (String, StoragePlacement)>,
    configuration: BTreeMap<(String, String), String>,
    secret: BTreeMap<(String, String), String>,
}

impl AttachmentIndexes {
    fn build(
        desired: &ApplicationDeploymentDeclaration,
    ) -> Result<Self, ApplicationLifecycleError> {
        let storage = desired
            .storage_attachments
            .iter()
            .map(|attachment| {
                let placement = StoragePlacement::try_from(attachment.placement)
                    .map_err(|_| configuration("Application storage placement is invalid"))?;
                Ok((
                    (
                        attachment.service_name.clone(),
                        attachment.binding_name.clone(),
                    ),
                    (attachment.storage_name.clone(), placement),
                ))
            })
            .collect::<Result<BTreeMap<_, _>, ApplicationLifecycleError>>()?;
        let configuration_values = desired
            .configuration_attachments
            .iter()
            .map(|attachment| {
                Ok((
                    (
                        attachment.service_name.clone(),
                        attachment.binding_name.clone(),
                    ),
                    attachment.value.clone(),
                ))
            })
            .collect::<Result<BTreeMap<_, _>, ApplicationLifecycleError>>()?;
        let secret = desired
            .secret_attachments
            .iter()
            .map(|attachment| {
                (
                    (
                        attachment.service_name.clone(),
                        attachment.binding_name.clone(),
                    ),
                    attachment.value.clone(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        if storage.len() != desired.storage_attachments.len()
            || configuration_values.len() != desired.configuration_attachments.len()
            || secret.len() != desired.secret_attachments.len()
        {
            return Err(configuration("Application attachment is duplicated"));
        }
        Ok(Self {
            storage,
            configuration: configuration_values,
            secret,
        })
    }
}

struct WorkloadUnitPlan {
    image: String,
    command: Vec<String>,
    arguments: Vec<String>,
    environment: Vec<(String, LaunchEnvironmentValue)>,
    storage: Vec<WorkloadStorageAttachment>,
    ports: Vec<dotcore_runtime::WorkloadPort>,
    dependencies: Vec<WorkloadUnitDependency>,
    ready_launch_gate: Option<ReadyLaunchGate>,
    host_aliases: BTreeSet<SiteNodeHost>,
}

impl WorkloadUnitPlan {
    fn compile(
        service: &dotcore_proto::dotcore::catalog::v1::ApplicationService,
        image: &str,
        attachments: &AttachmentIndexes,
        storage_key: &dyn Fn(StoragePlacement) -> Option<String>,
        route_linked: bool,
        has_ready_dependents: bool,
    ) -> Result<Self, ApplicationLifecycleError> {
        let mut environment: Vec<(String, LaunchEnvironmentValue)> = service
            .configuration_bindings
            .iter()
            .map(|binding| {
                let value = if route_linked && is_route_name_binding(binding)? {
                    LaunchEnvironmentValue::RouteName
                } else {
                    LaunchEnvironmentValue::Fixed(
                        attachments
                            .configuration
                            .get(&(service.service_name.clone(), binding.binding_name.clone()))
                            .cloned()
                            .ok_or_else(|| {
                                configuration("Application configuration attachment is missing")
                            })?,
                    )
                };
                Ok((binding.environment_variable.clone(), value))
            })
            .collect::<Result<_, ApplicationLifecycleError>>()?;
        for binding in &service.secret_bindings {
            let value = attachments
                .secret
                .get(&(service.service_name.clone(), binding.binding_name.clone()))
                .cloned()
                .ok_or_else(|| configuration("Application secret attachment is missing"))?;
            environment.push((
                binding.environment_variable.clone(),
                LaunchEnvironmentValue::Fixed(value),
            ));
        }
        let host_aliases = service
            .configuration_bindings
            .iter()
            .filter_map(|binding| {
                let value = attachments
                    .configuration
                    .get(&(service.service_name.clone(), binding.binding_name.clone()))?;
                site_host_alias(binding, value)
            })
            .collect();
        let named_bindings = service
            .storage_bindings
            .iter()
            .map(|binding| {
                attachments
                    .storage
                    .get(&(service.service_name.clone(), binding.binding_name.clone()))
                    .cloned()
                    .map(|(name, placement)| (name, binding, placement))
                    .ok_or_else(|| configuration("Application storage attachment is missing"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let storage = workload_storage(named_bindings, storage_key, |name| {
            environment.iter().any(|(set, _)| set == name)
        })
        .map_err(|source| {
            configuration_with_source("Application storage cannot be attached", source)
        })?;
        environment.extend(
            storage
                .environment
                .into_iter()
                .map(|(name, value)| (name, LaunchEnvironmentValue::Fixed(value))),
        );
        let dependencies = service
            .dependencies
            .iter()
            .map(|dependency| {
                let condition = match ApplicationDependencyCondition::try_from(dependency.condition)
                {
                    Ok(ApplicationDependencyCondition::Started) => {
                        WorkloadDependencyCondition::Started
                    }
                    Ok(ApplicationDependencyCondition::Ready) => WorkloadDependencyCondition::Ready,
                    _ => return Err(configuration("Application dependency condition is invalid")),
                };
                Ok(WorkloadUnitDependency {
                    unit_name: WorkloadUnitName(dependency.service_name.clone()),
                    condition,
                })
            })
            .collect::<Result<_, ApplicationLifecycleError>>()?;

        // The gate only orders READY dependents during group launch; product readiness is the
        // controller probe. Readiness is still validated for services nobody waits on.
        let ready_launch_gate = service
            .readiness
            .as_ref()
            .and_then(|readiness| readiness.check.as_ref())
            .map(|check| match check {
                application_service_readiness::Check::HttpGet(http) => ReadyLaunchGate::http_get(
                    &http.port_name,
                    &http.path,
                    Duration::from_secs(30),
                    Duration::from_secs(1),
                )
                .map_err(|source| {
                    configuration_with_source(
                        "Application readiness configuration is invalid",
                        source,
                    )
                }),
            })
            .transpose()?
            .filter(|_| has_ready_dependents);

        Ok(Self {
            image: image.to_owned(),
            command: service.command.clone(),
            arguments: service.arguments.clone(),
            environment,
            storage: storage.attachments,
            ports: service
                .ports
                .iter()
                .map(|port| dotcore_runtime::WorkloadPort {
                    name: port.port_name.clone(),
                    container_port: port.port_number as u16,
                })
                .collect(),
            dependencies,
            ready_launch_gate,
            host_aliases,
        })
    }

    fn uses_route_projection(&self) -> bool {
        self.environment
            .iter()
            .any(|(_, value)| matches!(value, LaunchEnvironmentValue::RouteName))
    }

    fn materialize(self, route: Option<&ResolvedInferenceRoute>) -> WorkloadUnitLaunch {
        let reconfigurable_environment = self
            .environment
            .iter()
            .filter(|(_, value)| matches!(value, LaunchEnvironmentValue::RouteName))
            .map(|(name, _)| name.clone())
            .collect();
        let environment = self
            .environment
            .into_iter()
            .map(|(name, value)| (name, value.materialize(route)))
            .collect();
        WorkloadUnitLaunch {
            image: self.image,
            command: self.command,
            arguments: self.arguments,
            environment,
            reconfigurable_environment,
            storage: self.storage,
            ports: self.ports,
            dependencies: self.dependencies,
            ready_launch_gate: self.ready_launch_gate,
        }
    }
}

enum LaunchEnvironmentValue {
    Fixed(String),

    RouteName,
}

impl LaunchEnvironmentValue {
    fn materialize(self, route: Option<&ResolvedInferenceRoute>) -> String {
        match self {
            Self::Fixed(value) => value,
            Self::RouteName => route
                .expect("route projection plans resolve exactly one route")
                .route_name
                .clone(),
        }
    }
}

fn canonical_launch_generation(
    execution_target: &ExecutionTarget,
    execution_target_id: &str,
    required_platform: &ExecutionPlatform,
    units: &BTreeMap<WorkloadUnitName, WorkloadUnitLaunch>,
    publication: &WorkloadPublicationRequest,
) -> u64 {
    let mut value = 0xcbf29ce484222325_u64;
    // A unit's `Debug` form leaves out environment values, so they are hashed explicitly.
    let environment = units
        .values()
        .flat_map(|unit| &unit.environment)
        .map(|(name, value)| format!("{name}={value}\n"))
        .collect::<String>();
    for byte in format!(
        "{execution_target:?}:{execution_target_id}:{required_platform:?}:{units:?}:{publication:?}:{environment}"
    )
    .bytes()
    {
        value ^= u64::from(byte);
        value = value.wrapping_mul(0x100000001b3);
    }
    value.max(1)
}

/// Returns the host name a binding's value names when its source carries a site host, for example
/// `manager.example.ts.net` for `http://manager.example.ts.net:6421/v1`. Catalog literals and
/// IPv4 hosts need no alias.
fn site_host_alias(
    binding: &dotcore_proto::dotcore::catalog::v1::ApplicationConfigurationBinding,
    value: &str,
) -> Option<SiteNodeHost> {
    let Some(application_configuration_binding::Source::ResolvedValueName(name)) =
        binding.source.as_ref()
    else {
        return None;
    };
    if !ApplicationConfigurationSources::carries_site_host(name) {
        return None;
    }
    let url = url::Url::parse(value).ok()?;
    let host = SiteNodeHost::parse(url.host_str()?).ok()?;
    host.is_name().then_some(host)
}

/// Resolves the retained configuration values of a new deployment, naming values through
/// `resolve` for the SiteNode that hosts it.
///
/// Route-name bindings get no attachment when a route is linked, since launch projects the current
/// route name; without a link they retain their catalog literal fallback. Parameter bindings
/// retain the canonical text of their resolved `parameter_values` entry.
pub(super) fn configuration_attachments(
    application: &Application,
    resolve: impl Fn(&str) -> Option<String>,
    parameter_values: &[ParameterValue],
    route_linked: bool,
) -> Result<Vec<ApplicationConfigurationAttachment>, ApplicationLifecycleError> {
    let graph = application
        .graph
        .as_ref()
        .ok_or_else(|| configuration("Application graph is missing"))?;
    let resolve = &resolve;
    graph
        .services
        .iter()
        .flat_map(|service| {
            service
                .configuration_bindings
                .iter()
                .filter_map(move |binding| match is_route_name_binding(binding) {
                    Ok(true) if route_linked => None,
                    Ok(_) => {
                        let value = match configuration_attachment_value(
                            binding,
                            resolve,
                            parameter_values,
                        ) {
                            Ok(value) if valid_configuration_value(&value) => value,
                            Ok(_) => {
                                return Some(Err(configuration(
                                    "Application configuration value cannot be represented safely",
                                )));
                            }
                            Err(error) => return Some(Err(error)),
                        };
                        Some(Ok(ApplicationConfigurationAttachment {
                            service_name: service.service_name.clone(),
                            binding_name: binding.binding_name.clone(),
                            environment_variable: binding.environment_variable.clone(),
                            value,
                        }))
                    }
                    Err(error) => Some(Err(error)),
                })
        })
        .collect()
}

/// Returns whether a binding projects the linked route name, rejecting unknown projections.
fn is_route_name_binding(
    binding: &dotcore_proto::dotcore::catalog::v1::ApplicationConfigurationBinding,
) -> Result<bool, ApplicationLifecycleError> {
    let Some(application_deployment_input::Kind::InferenceRoute(input)) =
        binding.input.as_ref().and_then(|input| input.kind.as_ref())
    else {
        return Ok(false);
    };
    match ApplicationInferenceRouteProjection::try_from(input.projection) {
        Ok(ApplicationInferenceRouteProjection::RouteName) => Ok(true),
        Ok(ApplicationInferenceRouteProjection::Unspecified) | Err(_) => Err(configuration(
            "Application inference route projection is invalid",
        )),
    }
}

fn configuration_attachment_value(
    binding: &dotcore_proto::dotcore::catalog::v1::ApplicationConfigurationBinding,
    resolve: &impl Fn(&str) -> Option<String>,
    parameter_values: &[ParameterValue],
) -> Result<String, ApplicationLifecycleError> {
    match binding.source.as_ref() {
        Some(application_configuration_binding::Source::LiteralValue(value)) => Ok(value.clone()),
        Some(application_configuration_binding::Source::ResolvedValueName(name)) => resolve(name)
            .ok_or_else(|| configuration("Application configuration source is unavailable")),
        Some(application_configuration_binding::Source::ParameterId(id)) => {
            canonical_value_text(parameter_values, id)
                .ok_or_else(|| configuration("Application parameter value is missing"))
        }
        None => Err(configuration("Application configuration source is invalid")),
    }
}

/// Resolves the retained secret attachments of a new deployment: each secret binding receives its
/// parameter's resolved value in the binding's form. A bcrypt form hashes, so callers run it off
/// the async executor, and only once per deployment.
pub(super) fn secret_attachments(
    graph: &ApplicationGraph,
    parameter_values: &[ParameterValue],
) -> Result<Vec<ApplicationSecretAttachment>, ApplicationLifecycleError> {
    graph
        .services
        .iter()
        .flat_map(|service| {
            service.secret_bindings.iter().map(move |binding| {
                let value = canonical_value_text(parameter_values, &binding.parameter_id)
                    .ok_or_else(|| {
                        configuration("Application secret parameter value is missing")
                    })?;
                let value = match ApplicationSecretForm::try_from(binding.form) {
                    Ok(ApplicationSecretForm::Plain) => value,
                    Ok(ApplicationSecretForm::Bcrypt) => bcrypt_hash(&value).map_err(|error| {
                        configuration_with_source(
                            "Application secret form cannot be computed",
                            error,
                        )
                    })?,
                    Ok(ApplicationSecretForm::Unspecified) | Err(_) => {
                        return Err(configuration("Application secret binding form is invalid"));
                    }
                };
                if !valid_configuration_value(&value) {
                    return Err(configuration(
                        "Application secret value cannot be represented safely",
                    ));
                }
                Ok(ApplicationSecretAttachment {
                    service_name: service.service_name.clone(),
                    binding_name: binding.binding_name.clone(),
                    environment_variable: binding.environment_variable.clone(),
                    value,
                })
            })
        })
        .collect()
}

/// Resolves one storage attachment per storage binding of a new deployment, each placed where the
/// operator's `choices` say or at the binding's first allowed placement.
pub(super) fn storage_attachments(
    application: &Application,
    choices: &[ApplicationStorageChoice],
) -> Result<Vec<ApplicationStorageAttachment>, ApplicationLifecycleError> {
    let graph = application
        .graph
        .as_ref()
        .ok_or_else(|| configuration("Application graph is missing"))?;
    let mut chosen = BTreeMap::new();
    for choice in choices {
        let reject = |reason| ApplicationLifecycleError::InvalidStorageChoice {
            service_name: choice.service_name.clone(),
            binding_name: choice.binding_name.clone(),
            reason,
        };
        let binding = graph
            .services
            .iter()
            .filter(|service| service.service_name == choice.service_name)
            .flat_map(|service| &service.storage_bindings)
            .find(|binding| binding.binding_name == choice.binding_name)
            .ok_or_else(|| reject(StorageChoiceRejection::Unknown))?;
        if !binding.placements.contains(&choice.placement) {
            let placement = StoragePlacement::try_from(choice.placement).map_or_else(
                |_| choice.placement.to_string(),
                |placement| placement.as_str_name().to_owned(),
            );
            return Err(reject(StorageChoiceRejection::NotAllowed(placement)));
        }
        let key = (choice.service_name.as_str(), choice.binding_name.as_str());
        if chosen.insert(key, choice.placement).is_some() {
            return Err(reject(StorageChoiceRejection::Duplicate));
        }
    }

    graph
        .services
        .iter()
        .flat_map(|service| {
            let chosen = &chosen;
            service.storage_bindings.iter().map(move |binding| {
                let storage_name = format!("{}-{}", service.service_name, binding.binding_name);
                if !valid_id(&storage_name) {
                    return Err(configuration(
                        "Application storage attachment name cannot be represented safely",
                    ));
                }
                let placement = chosen
                    .get(&(service.service_name.as_str(), binding.binding_name.as_str()))
                    .or(binding.placements.first())
                    .copied()
                    .ok_or_else(|| configuration("Application storage binding has no placement"))?;
                Ok(ApplicationStorageAttachment {
                    service_name: service.service_name.clone(),
                    binding_name: binding.binding_name.clone(),
                    storage_name,
                    placement,
                })
            })
        })
        .collect()
}
