//! Owner-scoped persistence operations for inference route declarations.

use std::{collections::BTreeSet, sync::Arc};

use dotcore_domain::inference_route::{RouteShapeError, is_valid_route_name, validate_route_shape};
use dotcore_domain::valid_id;
use dotcore_proto::dotcore::{
    inference::v1::{FallbackGroup, FallbackGroupSelection, InferenceRoute},
    model::v1::{ModelDeploymentDeclaration, ModelDeploymentDesiredState},
    site::v1::Site,
};
use dotcore_site::repository::{
    RouteSiteTransaction, ScopedSiteMutationError, SiteRepository, SiteRepositoryError,
};

use super::CompiledRouteSnapshot;

/// Route-manager-scoped access to the shared site store.
#[derive(Clone)]
pub(crate) struct RouteDeclarationRepository {
    site: SiteRepository,
}

impl RouteDeclarationRepository {
    /// Creates the scoped repository over the shared authoritative site.
    pub(crate) fn new(site: SiteRepository) -> Self {
        Self { site }
    }

    /// Compiles the complete current declaration set into an immutable route snapshot.
    pub(crate) fn compiled_snapshot(&self) -> Result<CompiledRouteSnapshot, RouteRepositoryError> {
        self.site
            .snapshot()
            .map(|site| CompiledRouteSnapshot::new(site.inference_routes, &site.model_deployments))
            .map_err(site_error)
    }

    /// Registers for accepted site changes so snapshots remain current across owners.
    pub(crate) fn register_change_listener(&self) -> std::sync::Arc<tokio::sync::Notify> {
        self.site.register_change_listener()
    }

    #[cfg(test)]
    pub(crate) fn site_state_pointer(&self) -> usize {
        self.site.state_pointer()
    }

    /// Registers an owner projection synchronized with accepted site changes.
    pub(crate) fn register_change_observer(&self, observer: Arc<dyn Fn(&Site) + Send + Sync>) {
        self.site.register_change_observer(observer);
    }

    /// Builds the single-member route for a deployment's Direct exposure without accepting either
    /// owner delta.
    pub(crate) fn prepare_exposure_route(
        &self,
        name: String,
        deployment: &ModelDeploymentDeclaration,
    ) -> Result<PreparedExposureRoute, RouteRepositoryError> {
        let mut candidate = self.site.snapshot().map_err(site_error)?;
        candidate.model_deployments.push(deployment.clone());
        let route = InferenceRoute {
            inference_route_id: next_route_id(&candidate.inference_routes),
            name,
            enabled: true,
            fallback_groups: vec![FallbackGroup {
                selection: FallbackGroupSelection::Single as i32,
                model_deployment_ids: vec![deployment.model_deployment_id.clone()],
            }],
            generation: 1,
        };
        let transaction = RouteSiteTransaction {
            site_id: &candidate.site_id,
            deployments: &candidate.model_deployments,
            applications: &candidate.application_deployments,
            routes: &mut candidate.inference_routes,
        };
        validate_new_route(&transaction, &route)?;
        Ok(PreparedExposureRoute::new(route))
    }

    /// Persists one server-created complete route declaration.
    pub(crate) fn create(
        &self,
        name: String,
        enabled: bool,
        fallback_groups: Vec<FallbackGroup>,
    ) -> Result<InferenceRoute, RouteRepositoryError> {
        self.site
            .mutate_inference_routes(|transaction| {
                let route = InferenceRoute {
                    inference_route_id: next_route_id(transaction.routes),
                    name,
                    enabled,
                    fallback_groups,
                    generation: 1,
                };
                validate_new_route(transaction, &route)?;
                transaction.routes.push(route.clone());
                Ok(route)
            })
            .map_err(scoped_error)
    }

    /// Replaces a complete mutable route declaration at its exact generation.
    pub(crate) fn replace(
        &self,
        inference_route_id: &str,
        expected_generation: u64,
        name: String,
        enabled: bool,
        fallback_groups: Vec<FallbackGroup>,
    ) -> Result<InferenceRoute, RouteRepositoryError> {
        if expected_generation == 0 {
            return Err(RouteRepositoryError::InvalidArgument(
                "expected generation must be positive".to_owned(),
            ));
        }
        self.site
            .mutate_inference_routes(|transaction| {
                let index = transaction
                    .routes
                    .iter()
                    .position(|route| route.inference_route_id == inference_route_id)
                    .ok_or_else(|| RouteRepositoryError::NotFound(inference_route_id.to_owned()))?;
                let existing = &transaction.routes[index];
                if existing.generation != expected_generation {
                    return Err(RouteRepositoryError::FailedPrecondition(
                        "route generation does not match the current declaration".to_owned(),
                    ));
                }
                let route = InferenceRoute {
                    inference_route_id: existing.inference_route_id.clone(),
                    name,
                    enabled,
                    fallback_groups,
                    generation: existing.generation.checked_add(1).ok_or_else(|| {
                        RouteRepositoryError::FailedPrecondition(
                            "route generation cannot be incremented".to_owned(),
                        )
                    })?,
                };
                validate_replacement_route(transaction, index, &route)?;
                transaction.routes[index] = route.clone();
                Ok(route)
            })
            .map_err(scoped_error)
    }

    /// Deletes one route declaration at its exact generation.
    pub(crate) fn delete(
        &self,
        inference_route_id: &str,
        expected_generation: u64,
    ) -> Result<(), RouteRepositoryError> {
        if expected_generation == 0 {
            return Err(RouteRepositoryError::InvalidArgument(
                "expected generation must be positive".to_owned(),
            ));
        }
        self.site
            .mutate_inference_routes(|transaction| {
                let index = transaction
                    .routes
                    .iter()
                    .position(|route| route.inference_route_id == inference_route_id)
                    .ok_or_else(|| RouteRepositoryError::NotFound(inference_route_id.to_owned()))?;
                if transaction.routes[index].generation != expected_generation {
                    return Err(RouteRepositoryError::FailedPrecondition(
                        "route generation does not match the current declaration".to_owned(),
                    ));
                }
                let mut references = transaction
                    .applications
                    .iter()
                    .filter(|application| {
                        application.inference_route_id.as_deref() == Some(inference_route_id)
                    })
                    .map(|application| application.application_deployment_id.clone())
                    .collect::<Vec<_>>();
                references.sort();
                if !references.is_empty() {
                    return Err(RouteRepositoryError::ApplicationReferences(references));
                }
                transaction.routes.remove(index);
                Ok(())
            })
            .map_err(scoped_error)
    }
}

/// Single-member route for a deployment's Direct exposure, prepared for focused acceptance.
pub(crate) struct PreparedExposureRoute {
    declaration: InferenceRoute,
}

impl PreparedExposureRoute {
    fn new(declaration: InferenceRoute) -> Self {
        Self { declaration }
    }

    pub(crate) fn into_declaration(self) -> InferenceRoute {
        self.declaration
    }
}

/// Route declaration persistence outcomes exposed to the route manager.
#[derive(Debug, thiserror::Error)]
pub enum RouteRepositoryError {
    #[error("Inference route name already exists: {0}")]
    NameAlreadyExists(String),

    #[error("Inference route ID collided with an existing route: {0}")]
    IdCollision(String),

    #[error("Inference route was not found: {0}")]
    NotFound(String),

    #[error("Invalid inference route: {0}")]
    InvalidArgument(String),

    #[error("Invalid inference route shape")]
    InvalidShape(#[from] RouteShapeError),

    #[error("Application deployments reference the inference route: {0:?}")]
    ApplicationReferences(Vec<String>),

    #[error("Inference route precondition failed: {0}")]
    FailedPrecondition(String),

    #[error("Inference route was accepted but site persistence failed: {0}")]
    Persistence(#[source] SiteRepositoryError),

    #[error("Inference route site store is unavailable")]
    Unavailable,

    #[error("Inference route site repository failed")]
    Repository(#[source] SiteRepositoryError),
}

fn validate_new_route(
    transaction: &RouteSiteTransaction<'_>,
    route: &InferenceRoute,
) -> Result<(), RouteRepositoryError> {
    if transaction
        .routes
        .iter()
        .any(|existing| existing.inference_route_id == route.inference_route_id)
    {
        return Err(RouteRepositoryError::IdCollision(
            route.inference_route_id.clone(),
        ));
    }
    validate_route(transaction, route, None)
}

fn validate_replacement_route(
    transaction: &RouteSiteTransaction<'_>,
    index: usize,
    route: &InferenceRoute,
) -> Result<(), RouteRepositoryError> {
    validate_route(transaction, route, Some(index))
}

fn validate_route(
    transaction: &RouteSiteTransaction<'_>,
    route: &InferenceRoute,
    replacing_index: Option<usize>,
) -> Result<(), RouteRepositoryError> {
    if !valid_id(&route.inference_route_id) {
        return invalid("route ID must match [a-z][a-z0-9-]{0,62}");
    }
    if !is_valid_route_name(&route.name) {
        return invalid("route name must match [a-z][a-z0-9.-]{0,62}");
    }
    if route.generation == 0 {
        return invalid("route generation must be positive");
    }
    if transaction
        .routes
        .iter()
        .enumerate()
        .any(|(index, existing)| Some(index) != replacing_index && existing.name == route.name)
    {
        return Err(RouteRepositoryError::NameAlreadyExists(route.name.clone()));
    }
    validate_route_shape(route)?;
    for group in &route.fallback_groups {
        for deployment_id in &group.model_deployment_ids {
            let deployment = transaction
                .deployments
                .iter()
                .find(|deployment| deployment.model_deployment_id == *deployment_id)
                .ok_or_else(|| {
                    RouteRepositoryError::InvalidArgument(format!(
                        "Model deployment {deployment_id:?} does not exist"
                    ))
                })?;
            if deployment.desired_state == ModelDeploymentDesiredState::Teardown as i32 {
                return invalid("a tearing-down model deployment cannot be routed");
            }
        }
    }
    Ok(())
}

fn next_route_id(routes: &[InferenceRoute]) -> String {
    let route_ids = routes
        .iter()
        .map(|route| route.inference_route_id.as_str())
        .collect::<BTreeSet<_>>();
    (1_u64..)
        .map(|number| format!("route-{number}"))
        .find(|candidate| !route_ids.contains(candidate.as_str()))
        .expect("unbounded route identity space")
}

fn invalid<T>(message: impl Into<String>) -> Result<T, RouteRepositoryError> {
    Err(RouteRepositoryError::InvalidArgument(message.into()))
}

fn scoped_error(error: ScopedSiteMutationError<RouteRepositoryError>) -> RouteRepositoryError {
    match error {
        ScopedSiteMutationError::Owner(error) => error,
        ScopedSiteMutationError::Repository(error) => site_error(error),
    }
}

fn site_error(error: SiteRepositoryError) -> RouteRepositoryError {
    match error {
        SiteRepositoryError::Storage(error) => {
            RouteRepositoryError::Persistence(SiteRepositoryError::Storage(error))
        }
        SiteRepositoryError::Unavailable => RouteRepositoryError::Unavailable,
        error => RouteRepositoryError::Repository(error),
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use anyhow::anyhow;
    use dotcore_proto::dotcore::{
        application::v1::{
            ApplicationDeploymentDeclaration, ApplicationDeploymentDesiredState,
            ResolvedApplicationDeploymentSnapshot,
        },
        catalog::v1::{ModelParallelShape, OciPlatform},
        deployment::v1::ExactPlacement,
        inference::v1::{FallbackGroup, FallbackGroupSelection, InferenceRoute},
        model::v1::{
            ModelDeploymentDeclaration, ModelDeploymentDesiredState,
            ModelDeploymentLifecycleTransition,
        },
        site::v1::{
            ApplicationRuntimeBinding, ExecutionTarget, ModelRuntimeBinding,
            SimulatedApplicationRuntimeAdapter, SimulatedRuntimeAdapter, SimulatedRuntimeScenario,
            Site, SiteManagerRole, SiteNode, application_runtime_binding, execution_target,
            model_runtime_binding,
        },
    };
    use dotcore_site::{
        persistence::SiteStorageError,
        repository::{RouteSiteTransaction, SiteRepository, SiteRepositoryError},
    };

    use super::{RouteRepositoryError, site_error, validate_route};

    #[test]
    fn site_repository_categories_preserve_expected_route_error_boundaries() {
        let cases = [
            SiteRepositoryError::InvalidSite(anyhow!("internal site validation diagnostic")),
            SiteRepositoryError::Yaml(dotcore_proto::yaml::RestrictedYamlError::DocumentCount {
                count: 2,
                location: None,
            }),
            SiteRepositoryError::SerializeYaml(
                dotcore_proto::yaml::RestrictedYamlError::DocumentCount {
                    count: 2,
                    location: None,
                },
            ),
        ];

        for site in cases {
            let error = site_error(site);
            assert_eq!(error.to_string(), "Inference route site repository failed");
            let source = error.source().expect("retained site repository source");
            assert!(source.downcast_ref::<SiteRepositoryError>().is_some());
            assert!(!error.to_string().contains("internal"));
        }

        let storage = site_error(SiteRepositoryError::Storage(SiteStorageError::Storage(
            anyhow!("internal site persistence diagnostic"),
        )));
        assert!(matches!(storage, RouteRepositoryError::Persistence(_)));
        assert_eq!(
            storage
                .source()
                .expect("retained site persistence source")
                .to_string(),
            "Desired site storage failed: site storage failed: internal site persistence diagnostic"
        );

        let unavailable = site_error(SiteRepositoryError::Unavailable);
        assert!(matches!(unavailable, RouteRepositoryError::Unavailable));
        assert!(unavailable.source().is_none());
    }

    #[test]
    fn route_names_allow_periods_while_route_ids_do_not() {
        assert!(validate_route_fixture(route("route-1", "claude-3.5-sonnet")).is_ok());

        let error = validate_route_fixture(route("route-1", ".claude"))
            .expect_err("a period-leading route name must be rejected");
        assert_eq!(
            error.to_string(),
            "Invalid inference route: route name must match [a-z][a-z0-9.-]{0,62}"
        );

        let error = validate_route_fixture(route("route.1", "claude"))
            .expect_err("a period in the route ID must be rejected");
        assert_eq!(
            error.to_string(),
            "Invalid inference route: route ID must match [a-z][a-z0-9-]{0,62}"
        );
    }

    #[test]
    fn application_binding_persistence_winner_blocks_route_deletion_without_state_delta() {
        let directory = crate::test_support::canonical_tempdir().expect("Site directory");
        let path = directory.path().join("site.yaml");
        let original = routed_site();
        std::fs::write(
            &path,
            dotcore_proto::yaml::to_string(&original).expect("desired site YAML"),
        )
        .expect("desired site fixture");
        let site = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
            .expect("desired site");
        let routes = super::RouteDeclarationRepository::new(site.clone());

        site.mutate_application_deployments(|deployments| {
            deployments.push(route_bound_application("application-two"));
            deployments.push(route_bound_application("application-one"));
            Ok::<(), std::convert::Infallible>(())
        })
        .expect("Application binding persistence wins");
        let before_route_deletion = site.snapshot().expect("Site before route deletion");
        let persisted_before_route_deletion =
            std::fs::read(&path).expect("Site before route deletion");
        let route_deletion = routes.delete("route-one", 1);
        let after_route_deletion = site.snapshot().expect("Site after rejected route deletion");

        assert!(matches!(
            route_deletion,
            Err(RouteRepositoryError::ApplicationReferences(references))
                if references == ["application-one", "application-two"]
        ));
        assert_eq!(after_route_deletion, before_route_deletion);
        assert_eq!(
            std::fs::read(&path).expect("Site after rejected route deletion"),
            persisted_before_route_deletion
        );
    }

    /// A site with one model deployment on a simulated target, the sole-member route serving it,
    /// and an application target for route-bound applications.
    fn routed_site() -> Site {
        let simulated_platform = || OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "amd64".to_owned(),
            variant: None,
        };
        Site {
            site_id: "site-one".to_owned(),
            nodes: vec![SiteNode {
                node_id: "node-one".to_owned(),
                host: "127.0.0.1".to_owned(),
                display_name: "Node One".to_owned(),
                site_manager: Some(SiteManagerRole {}),
                manager_host: String::new(),
            }],
            model_deployment_identity_allocator: Some(crate::test_support::identity_allocator()),
            execution_targets: vec![
                ExecutionTarget {
                    execution_target_id: "target-one".to_owned(),
                    display_name: "target-one".to_owned(),
                    site_node_id: "node-one".to_owned(),
                    runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                        ModelRuntimeBinding {
                            adapter: Some(model_runtime_binding::Adapter::SimulatedRuntime(
                                SimulatedRuntimeAdapter {
                                    default_scenario: Some(SimulatedRuntimeScenario::default()),
                                    deployment_scenarios: Vec::new(),
                                },
                            )),
                        },
                    )),
                    platform: Some(simulated_platform()),
                    accelerator_device_ids: vec![
                        "GPU-00000000-0000-0000-0000-000000005090".to_owned(),
                    ],
                },
                ExecutionTarget {
                    execution_target_id: "application-target".to_owned(),
                    display_name: "application-target".to_owned(),
                    site_node_id: "node-one".to_owned(),
                    runtime_binding: Some(execution_target::RuntimeBinding::ApplicationRuntime(
                        ApplicationRuntimeBinding {
                            adapter: Some(application_runtime_binding::Adapter::SimulatedRuntime(
                                SimulatedApplicationRuntimeAdapter {},
                            )),
                        },
                    )),
                    platform: Some(simulated_platform()),
                    ..Default::default()
                },
            ],
            model_deployments: vec![ModelDeploymentDeclaration {
                model_deployment_id: "deployment-one".to_owned(),
                model_deployment_name: "Model".to_owned(),
                placement: Some(ExactPlacement {
                    site_node_id: "node-one".to_owned(),
                    execution_target_id: "target-one".to_owned(),
                }),
                requested_parallel_shape: ModelParallelShape::One as i32,
                snapshot: Some(crate::test_support::local_model_snapshot("model-one")),
                desired_state: ModelDeploymentDesiredState::Present as i32,
                lifecycle_generation: 1,
                lifecycle_transition: ModelDeploymentLifecycleTransition::Create as i32,
                ..Default::default()
            }],
            inference_routes: vec![InferenceRoute {
                inference_route_id: "route-one".to_owned(),
                name: "route-one".to_owned(),
                enabled: true,
                fallback_groups: vec![FallbackGroup {
                    selection: FallbackGroupSelection::Single as i32,
                    model_deployment_ids: vec!["deployment-one".to_owned()],
                }],
                generation: 1,
            }],
            ..Default::default()
        }
    }

    fn route_bound_application(
        application_deployment_id: &str,
    ) -> ApplicationDeploymentDeclaration {
        let placement = ExactPlacement {
            site_node_id: "node-one".to_owned(),
            execution_target_id: "application-target".to_owned(),
        };
        ApplicationDeploymentDeclaration {
            lifecycle_generation: 1,
            application_deployment_id: application_deployment_id.to_owned(),
            application_deployment_name: format!("Application {application_deployment_id}"),
            placement: Some(placement.clone()),
            snapshot: Some(ResolvedApplicationDeploymentSnapshot {
                placement: Some(placement),
                platform: Some(OciPlatform {
                    operating_system: "linux".to_owned(),
                    architecture: "amd64".to_owned(),
                    variant: None,
                }),
                ..Default::default()
            }),
            desired_state: ApplicationDeploymentDesiredState::Present as i32,
            inference_route_id: Some("route-one".to_owned()),
            ..Default::default()
        }
    }

    fn route(route_id: &str, name: &str) -> InferenceRoute {
        InferenceRoute {
            inference_route_id: route_id.to_owned(),
            name: name.to_owned(),
            enabled: true,
            fallback_groups: vec![FallbackGroup {
                selection: FallbackGroupSelection::Single as i32,
                model_deployment_ids: vec!["dep-1".to_owned()],
            }],
            generation: 1,
        }
    }

    fn validate_route_fixture(route: InferenceRoute) -> Result<(), RouteRepositoryError> {
        let deployments = [ModelDeploymentDeclaration {
            model_deployment_id: "dep-1".to_owned(),
            desired_state: ModelDeploymentDesiredState::Present as i32,
            ..Default::default()
        }];
        let transaction = RouteSiteTransaction {
            site_id: "site",
            deployments: &deployments,
            applications: &[],
            routes: &mut Vec::new(),
        };
        validate_route(&transaction, &route, None)
    }
}
