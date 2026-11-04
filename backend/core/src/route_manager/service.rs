//! In-process route manager and RouteService management adapter.

use std::sync::{Arc, Mutex, RwLock};

use dotcore_proto::dotcore::inference::v1::{
    CreateRouteRequest, CreateRouteResponse, DeleteRouteRequest, DeleteRouteResponse,
    GetRouteRequest, GetRouteResponse, InferenceRoute, InferenceRouteView, ListRoutesRequest,
    ListRoutesResponse, ReplaceRouteRequest, ReplaceRouteResponse,
    route_service_server::RouteService as RouteServiceContract,
};
use tonic::{Request, Response, Status};
use tracing::{debug, error, info, warn};

use super::{
    CompiledRouteSnapshot, RouteAvailabilityTargetsSource, RouteDeclarationRepository,
    repository::RouteRepositoryError,
};
use crate::application::InferenceRouteResolutionError;

/// In-process owner of desired inference routes and their immutable snapshots.
#[derive(Clone)]
pub(crate) struct RouteManager {
    shared: Arc<RouteManagerShared>,
}

impl RouteManager {
    /// Builds the manager from the shared site store and controller target evidence.
    pub(crate) fn new(
        declarations: RouteDeclarationRepository,
        targets: RouteAvailabilityTargetsSource,
    ) -> Result<Self, RouteRepositoryError> {
        let snapshot = declarations.compiled_snapshot()?;
        let listener = declarations.register_change_listener();
        let shared = Arc::new(RouteManagerShared {
            declarations,
            targets,
            snapshot: RwLock::new(Arc::new(snapshot)),
            availability: Mutex::new(std::collections::BTreeMap::new()),
            mutation: Mutex::new(()),
            listener,
        });
        let observer_shared = Arc::downgrade(&shared);
        shared
            .declarations
            .register_change_observer(Arc::new(move |site| {
                if let Some(shared) = observer_shared.upgrade() {
                    publish_snapshot(
                        &shared,
                        CompiledRouteSnapshot::new(
                            site.inference_routes.clone(),
                            &site.model_deployments,
                        ),
                    );
                }
            }));
        Ok(Self { shared })
    }

    /// Starts the accepted-site-change observer after management transport is ready.
    pub(crate) fn start_observer(&self) {
        let manager = self.clone();
        tokio::spawn(async move {
            loop {
                manager.shared.listener.notified().await;
                manager.refresh();
            }
        });
    }

    /// Compares route availability after controller-owned serving-target evidence changes.
    pub(crate) fn observe_serving_target_change(&self) {
        let snapshot = self.snapshot();
        for route in snapshot.routes() {
            self.view(&snapshot, route);
        }
    }

    #[cfg(test)]
    pub(crate) fn shared_pointer(&self) -> usize {
        Arc::as_ptr(&self.shared) as usize
    }

    #[cfg(test)]
    pub(crate) fn declaration_site_state_pointer(&self) -> usize {
        self.shared.declarations.site_state_pointer()
    }

    /// Returns one immutable desired snapshot for private serving-path consumers.
    pub(crate) fn snapshot(&self) -> Arc<CompiledRouteSnapshot> {
        Arc::clone(
            &self
                .shared
                .snapshot
                .read()
                .unwrap_or_else(|value| value.into_inner()),
        )
    }

    /// Lists the current desired routes with independently derived transient availability.
    pub(crate) fn list(&self) -> Vec<InferenceRouteView> {
        let snapshot = self.snapshot();
        snapshot
            .routes()
            .iter()
            .map(|route| self.view(&snapshot, route))
            .collect()
    }

    /// Gets one current desired route with independently derived transient availability.
    pub(crate) fn get(&self, inference_route_id: &str) -> Option<InferenceRouteView> {
        let snapshot = self.snapshot();
        snapshot
            .by_id(inference_route_id)
            .map(|route| self.view(&snapshot, route))
    }

    /// Returns one current declaration for application route links, regardless of availability.
    pub(crate) fn application_binding_projection(
        &self,
        inference_route_id: &str,
    ) -> Result<InferenceRoute, InferenceRouteResolutionError> {
        self.snapshot()
            .by_id(inference_route_id)
            .cloned()
            .ok_or(InferenceRouteResolutionError::Unknown)
    }

    /// Creates and publishes one complete route declaration.
    pub(crate) fn create(
        &self,
        request: CreateRouteRequest,
    ) -> Result<InferenceRouteView, RouteRepositoryError> {
        self.mutate("create", || {
            self.shared
                .declarations
                .create(request.name, request.enabled, request.fallback_groups)
        })
    }

    /// Replaces and publishes one complete route declaration.
    pub(crate) fn replace(
        &self,
        request: ReplaceRouteRequest,
    ) -> Result<InferenceRouteView, RouteRepositoryError> {
        self.mutate("replace", || {
            self.shared.declarations.replace(
                &request.inference_route_id,
                request.expected_generation,
                request.name,
                request.enabled,
                request.fallback_groups,
            )
        })
    }

    /// Deletes one route declaration and publishes the complete resulting snapshot.
    pub(crate) fn delete(&self, request: DeleteRouteRequest) -> Result<(), RouteRepositoryError> {
        let _mutation = self
            .shared
            .mutation
            .lock()
            .unwrap_or_else(|value| value.into_inner());
        let result = self
            .shared
            .declarations
            .delete(&request.inference_route_id, request.expected_generation);
        if result.is_ok() || matches!(&result, Err(RouteRepositoryError::Persistence(_))) {
            self.refresh();
        }
        log_mutation("delete", &request.inference_route_id, &result);
        result
    }

    fn mutate(
        &self,
        operation_name: &'static str,
        operation: impl FnOnce() -> Result<InferenceRoute, RouteRepositoryError>,
    ) -> Result<InferenceRouteView, RouteRepositoryError> {
        let _mutation = self
            .shared
            .mutation
            .lock()
            .unwrap_or_else(|value| value.into_inner());
        let result = operation();
        if result.is_ok() || matches!(&result, Err(RouteRepositoryError::Persistence(_))) {
            self.refresh();
        }
        if let Err(error) = &result {
            log_mutation_error(operation_name, error);
        }
        let route = result?;
        info!(
            event_kind = "route_mutation_accepted",
            operation = operation_name,
            route_id = %route.inference_route_id,
            route_name = %route.name,
            generation = route.generation,
            "inference route mutation accepted",
        );
        Ok(self.view(&self.snapshot(), &route))
    }

    /// Projects one route with its availability and the request kinds its members declare in
    /// `snapshot`.
    fn view(&self, snapshot: &CompiledRouteSnapshot, route: &InferenceRoute) -> InferenceRouteView {
        let availability = self.shared.targets.availability(route);
        let previous = self
            .shared
            .availability
            .lock()
            .unwrap_or_else(|value| value.into_inner())
            .insert(route.inference_route_id.clone(), availability.clone());
        if availability_changed(previous.as_ref(), &availability) {
            info!(
                event_kind = "route_availability_changed",
                route_id = %route.inference_route_id,
                route_name = %route.name,
                available = availability.available,
                previous_available = previous.as_ref().map(|value| value.available),
                "inference route availability changed",
            );
        }
        InferenceRouteView {
            route: Some(route.clone()),
            availability: Some(availability),
            request_kinds: snapshot
                .request_kinds(&route.inference_route_id)
                .into_iter()
                .map(|kind| kind as i32)
                .collect(),
        }
    }

    fn refresh(&self) {
        let Ok(snapshot) = self.shared.declarations.compiled_snapshot() else {
            return;
        };
        publish_snapshot(&self.shared, snapshot);
    }
}

fn publish_snapshot(shared: &RouteManagerShared, snapshot: CompiledRouteSnapshot) {
    let mut current = shared
        .snapshot
        .write()
        .unwrap_or_else(|value| value.into_inner());
    if **current != snapshot {
        let route_count = snapshot.routes().len();
        *current = Arc::new(snapshot);
        info!(
            event_kind = "route_snapshot_changed",
            route_count, "inference route snapshot changed",
        );
    } else {
        debug!(
            event_kind = "route_snapshot_unchanged",
            "inference route snapshot unchanged"
        );
    }
}

fn availability_changed(
    previous: Option<&dotcore_proto::dotcore::inference::v1::InferenceRouteAvailability>,
    current: &dotcore_proto::dotcore::inference::v1::InferenceRouteAvailability,
) -> bool {
    previous.is_some_and(|previous| previous != current)
}

fn log_mutation(operation: &str, route_id: &str, result: &Result<(), RouteRepositoryError>) {
    match result {
        Ok(()) => info!(
            event_kind = "route_mutation_accepted",
            operation, route_id, "inference route mutation accepted",
        ),
        Err(error) => log_mutation_error(operation, error),
    }
}

fn log_mutation_error(operation: &str, error: &RouteRepositoryError) {
    match error {
        RouteRepositoryError::Persistence(_)
        | RouteRepositoryError::Unavailable
        | RouteRepositoryError::Repository(_) => error!(
            event_kind = "route_mutation_failed",
            operation,
            error_kind = %route_error_kind(error),
            "inference route mutation failed",
        ),
        _ => warn!(
            event_kind = "route_mutation_rejected",
            operation,
            error_kind = %route_error_kind(error),
            "inference route mutation rejected",
        ),
    }
}

fn route_error_kind(error: &RouteRepositoryError) -> &'static str {
    match error {
        RouteRepositoryError::NameAlreadyExists(_) => "already_exists",
        RouteRepositoryError::IdCollision(_) => "id_collision",
        RouteRepositoryError::NotFound(_) => "not_found",
        RouteRepositoryError::InvalidArgument(_) => "invalid_argument",
        RouteRepositoryError::InvalidShape(_) => "invalid_shape",
        RouteRepositoryError::ApplicationReferences(_) => "application_references",
        RouteRepositoryError::FailedPrecondition(_) => "failed_precondition",
        RouteRepositoryError::Persistence(_) => "persistence",
        RouteRepositoryError::Unavailable => "unavailable",
        RouteRepositoryError::Repository(_) => "repository",
    }
}

struct RouteManagerShared {
    declarations: RouteDeclarationRepository,
    targets: RouteAvailabilityTargetsSource,
    snapshot: RwLock<Arc<CompiledRouteSnapshot>>,
    availability: Mutex<
        std::collections::BTreeMap<
            String,
            dotcore_proto::dotcore::inference::v1::InferenceRouteAvailability,
        >,
    >,
    mutation: Mutex<()>,
    listener: Arc<tokio::sync::Notify>,
}

/// Management API adapter for route-manager-owned declarations.
#[derive(Clone)]
pub(crate) struct RouteService {
    manager: RouteManager,
}

impl RouteService {
    /// Creates the management API adapter for one in-process route manager.
    pub(crate) fn new(manager: RouteManager) -> Self {
        Self { manager }
    }

    /// Starts the route manager's site change observer.
    pub(crate) fn start_observer(&self) {
        self.manager.start_observer();
    }

    #[cfg(test)]
    pub(crate) fn manager_pointer(&self) -> usize {
        self.manager.shared_pointer()
    }

    #[cfg(test)]
    pub(crate) fn declaration_site_state_pointer(&self) -> usize {
        self.manager.declaration_site_state_pointer()
    }
}

#[tonic::async_trait]
impl RouteServiceContract for RouteService {
    async fn list_routes(
        &self,
        _request: Request<ListRoutesRequest>,
    ) -> Result<Response<ListRoutesResponse>, Status> {
        Ok(Response::new(ListRoutesResponse {
            routes: self.manager.list(),
        }))
    }

    async fn get_route(
        &self,
        request: Request<GetRouteRequest>,
    ) -> Result<Response<GetRouteResponse>, Status> {
        let inference_route_id = request.into_inner().inference_route_id;
        self.manager
            .get(&inference_route_id)
            .map(|route| Response::new(GetRouteResponse { route: Some(route) }))
            .ok_or_else(|| Status::not_found("Inference route was not found"))
    }

    async fn create_route(
        &self,
        request: Request<CreateRouteRequest>,
    ) -> Result<Response<CreateRouteResponse>, Status> {
        self.manager
            .create(request.into_inner())
            .map(|route| Response::new(CreateRouteResponse { route: Some(route) }))
            .map_err(translate_error)
    }

    async fn replace_route(
        &self,
        request: Request<ReplaceRouteRequest>,
    ) -> Result<Response<ReplaceRouteResponse>, Status> {
        self.manager
            .replace(request.into_inner())
            .map(|route| Response::new(ReplaceRouteResponse { route: Some(route) }))
            .map_err(translate_error)
    }

    async fn delete_route(
        &self,
        request: Request<DeleteRouteRequest>,
    ) -> Result<Response<DeleteRouteResponse>, Status> {
        self.manager
            .delete(request.into_inner())
            .map(|()| Response::new(DeleteRouteResponse {}))
            .map_err(translate_error)
    }
}

fn translate_error(error: RouteRepositoryError) -> Status {
    match error {
        RouteRepositoryError::NameAlreadyExists(_) => {
            Status::already_exists("Inference route name already exists")
        }
        RouteRepositoryError::IdCollision(_) => Status::internal("Inference route ID collided"),
        RouteRepositoryError::NotFound(_) => Status::not_found("Inference route was not found"),
        RouteRepositoryError::InvalidArgument(_) | RouteRepositoryError::InvalidShape(_) => {
            Status::invalid_argument("Inference route declaration is invalid")
        }
        RouteRepositoryError::ApplicationReferences(references) => {
            Status::failed_precondition(format!(
                "Inference route is referenced by application deployments: {}",
                references.join(", ")
            ))
        }
        RouteRepositoryError::FailedPrecondition(_) => Status::failed_precondition(
            "Inference route operation is not valid for the current declaration",
        ),
        RouteRepositoryError::Persistence(_) => Status::unavailable(
            "Inference route change was accepted but site persistence failed; inspect current state before retrying.",
        ),
        RouteRepositoryError::Unavailable => {
            Status::unavailable("Inference route site store is unavailable")
        }
        RouteRepositoryError::Repository(_) => {
            Status::internal("Inference route site repository failed")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use dotcore_domain::inference_route::RouteShapeError;
    use dotcore_site::{persistence::SiteStorageError, repository::SiteRepositoryError};
    use tonic::Code;

    use super::{availability_changed, translate_error};
    use crate::route_manager::RouteRepositoryError;

    #[test]
    fn initial_availability_projection_does_not_emit_for_unrelated_target_evidence() {
        let unavailable = dotcore_proto::dotcore::inference::v1::InferenceRouteAvailability {
            available: false,
            ..Default::default()
        };
        let available = dotcore_proto::dotcore::inference::v1::InferenceRouteAvailability {
            available: true,
            ..Default::default()
        };

        let mut cache = BTreeMap::new();
        let initial_publication = [
            ("unrelated-route", unavailable.clone()),
            ("affected-route", unavailable.clone()),
        ];

        let false_transitions = initial_publication
            .into_iter()
            .filter(|(route_id, availability)| {
                availability_changed(
                    cache.insert(*route_id, availability.clone()).as_ref(),
                    availability,
                )
            })
            .count();
        assert_eq!(false_transitions, 0);

        let first_transition = availability_changed(
            cache.insert("affected-route", available.clone()).as_ref(),
            &available,
        );
        let repeated_transition = availability_changed(
            cache.insert("affected-route", available.clone()).as_ref(),
            &available,
        );
        assert!(first_transition);
        assert!(!repeated_transition);
    }

    #[test]
    fn route_repository_errors_have_exact_safe_transport_statuses() {
        let cases = [
            (
                RouteRepositoryError::NameAlreadyExists("internal route name".to_owned()),
                Code::AlreadyExists,
                "Inference route name already exists",
            ),
            (
                RouteRepositoryError::IdCollision("internal route ID".to_owned()),
                Code::Internal,
                "Inference route ID collided",
            ),
            (
                RouteRepositoryError::NotFound("internal route ID".to_owned()),
                Code::NotFound,
                "Inference route was not found",
            ),
            (
                RouteRepositoryError::InvalidArgument("internal diagnostic".to_owned()),
                Code::InvalidArgument,
                "Inference route declaration is invalid",
            ),
            (
                RouteRepositoryError::InvalidShape(RouteShapeError::GroupCount(0)),
                Code::InvalidArgument,
                "Inference route declaration is invalid",
            ),
            (
                RouteRepositoryError::ApplicationReferences(vec![
                    "application-two".to_owned(),
                    "application-one".to_owned(),
                ]),
                Code::FailedPrecondition,
                "Inference route is referenced by application deployments: application-two, application-one",
            ),
            (
                RouteRepositoryError::FailedPrecondition("internal diagnostic".to_owned()),
                Code::FailedPrecondition,
                "Inference route operation is not valid for the current declaration",
            ),
            (
                RouteRepositoryError::Persistence(SiteRepositoryError::Storage(
                    SiteStorageError::Storage(anyhow::anyhow!("internal persistence diagnostic")),
                )),
                Code::Unavailable,
                "Inference route change was accepted but site persistence failed; inspect current state before retrying.",
            ),
            (
                RouteRepositoryError::Unavailable,
                Code::Unavailable,
                "Inference route site store is unavailable",
            ),
            (
                RouteRepositoryError::Repository(SiteRepositoryError::InvalidSite(
                    anyhow::anyhow!("internal site diagnostic"),
                )),
                Code::Internal,
                "Inference route site repository failed",
            ),
            (
                RouteRepositoryError::Repository(SiteRepositoryError::Yaml(
                    dotcore_proto::yaml::RestrictedYamlError::DocumentCount {
                        count: 2,
                        location: None,
                    },
                )),
                Code::Internal,
                "Inference route site repository failed",
            ),
            (
                RouteRepositoryError::Repository(SiteRepositoryError::SerializeYaml(
                    dotcore_proto::yaml::RestrictedYamlError::DocumentCount {
                        count: 2,
                        location: None,
                    },
                )),
                Code::Internal,
                "Inference route site repository failed",
            ),
        ];

        for (error, code, message) in cases {
            let status = translate_error(error);
            assert_eq!(status.code(), code);
            assert_eq!(status.message(), message);
            assert!(!status.message().contains("internal"));
            assert!(!status.message().contains('/'));
        }
    }
}
