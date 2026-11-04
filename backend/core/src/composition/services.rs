//! Application composition pieces: the route-manager projection of linked routes and the
//! image preparer a node prepares application images with.

use std::sync::Arc;

use dotcore_proto::dotcore::catalog::v1::InferenceRequestKind;

use super::CompositionError;
use crate::{
    application::{InferenceRouteResolutionError, InferenceRouteResolver, ResolvedInferenceRoute},
    route_manager::RouteManager,
};

/// Connects the image preparer application realization uses when one of `targets` runs
/// application workloads on Docker; other nodes prepare no application image.
pub(super) async fn application_oci_preparer_for_targets<'a, C>(
    targets: impl IntoIterator<Item = &'a dotcore_proto::dotcore::site::v1::ExecutionTarget>,
    connect: impl FnOnce() -> C,
) -> Result<Option<Arc<dyn dotcore_artifact::OciImagePreparer>>, CompositionError>
where
    C: Future<
        Output = Result<
            Arc<dyn dotcore_artifact::OciImagePreparer>,
            dotcore_artifact::OciClientError,
        >,
    >,
{
    if !targets.into_iter().any(|target| {
        matches!(
                target.runtime_binding,
                Some(dotcore_proto::dotcore::site::v1::execution_target::RuntimeBinding::ApplicationRuntime(
                    dotcore_proto::dotcore::site::v1::ApplicationRuntimeBinding {
                        adapter: Some(dotcore_proto::dotcore::site::v1::application_runtime_binding::Adapter::DockerRuntime(_)),
                    }
                ))
            )
    }) {
        return Ok(None);
    }
    connect().await.map(Some).map_err(|source| {
        CompositionError::composition_with_source(
            "Application local Docker OCI client initialization failed",
            source,
        )
    })
}

pub(super) fn inference_route_resolver(manager: RouteManager) -> Arc<dyn InferenceRouteResolver> {
    Arc::new(RouteManagerInferenceRouteResolver { manager })
}

/// Projects linked routes from route-manager declarations without availability or enabled checks.
struct RouteManagerInferenceRouteResolver {
    manager: RouteManager,
}

impl InferenceRouteResolver for RouteManagerInferenceRouteResolver {
    #[cfg(test)]
    fn composition_route_manager_pointer(&self) -> Option<usize> {
        Some(self.manager.shared_pointer())
    }

    fn resolve_for_create(
        &self,
        inference_route_id: &str,
        expected_generation: u64,
    ) -> Result<ResolvedInferenceRoute, InferenceRouteResolutionError> {
        // The route and the kinds its members declare come from one snapshot.
        let snapshot = self.manager.snapshot();
        let route = snapshot
            .by_id(inference_route_id)
            .ok_or(InferenceRouteResolutionError::Unknown)?;
        if route.generation != expected_generation {
            return Err(InferenceRouteResolutionError::Stale);
        }
        if !snapshot
            .request_kinds(inference_route_id)
            .contains(&InferenceRequestKind::Chat)
        {
            return Err(InferenceRouteResolutionError::NotChat);
        }
        Ok(project(route.clone()))
    }

    fn project_current(
        &self,
        inference_route_id: &str,
    ) -> Result<ResolvedInferenceRoute, InferenceRouteResolutionError> {
        self.manager
            .application_binding_projection(inference_route_id)
            .map(project)
    }
}

fn project(route: dotcore_proto::dotcore::inference::v1::InferenceRoute) -> ResolvedInferenceRoute {
    ResolvedInferenceRoute {
        inference_route_id: route.inference_route_id,
        route_name: route.name,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use dotcore_proto::dotcore::site::v1::{
        ApplicationRuntimeBinding, DockerApplicationRuntimeAdapter, ExecutionTarget, Site,
        SiteNode, application_runtime_binding, execution_target,
    };
    use dotcore_site::{
        repository::SiteRepository, store::FileCurrentState, view::fencing::ActionAssignment,
    };
    use tokio_util::sync::CancellationToken;

    use super::*;

    #[tokio::test]
    async fn apps_only_docker_node_receives_an_artifact_preparer_without_model_targets() {
        let connections = Arc::new(AtomicUsize::new(0));
        let node = SiteNode {
            node_id: "node-one".to_owned(),
            ..Default::default()
        };
        let site = Site {
            execution_targets: vec![ExecutionTarget {
                site_node_id: node.node_id.clone(),
                runtime_binding: Some(execution_target::RuntimeBinding::ApplicationRuntime(
                    ApplicationRuntimeBinding {
                        adapter: Some(application_runtime_binding::Adapter::DockerRuntime(
                            DockerApplicationRuntimeAdapter {},
                        )),
                    },
                )),
                ..Default::default()
            }],
            ..Default::default()
        };

        application_oci_preparer_for_targets(&site.execution_targets, {
            let connections = Arc::clone(&connections);
            move || {
                connections.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(
                    Arc::new(ReadyPreparer) as Arc<dyn dotcore_artifact::OciImagePreparer>
                ))
            }
        })
        .await
        .expect("Apps-only Docker composition")
        .expect("Application image preparer");

        assert_eq!(connections.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn routes_project_declared_request_kinds_and_application_links_require_chat() {
        use dotcore_proto::dotcore::{
            inference::v1::{
                CreateRouteRequest, FallbackGroup, FallbackGroupSelection, InferenceRouteView,
            },
            model::v1::model_deployment_snapshot::Source,
        };

        let chat = InferenceRequestKind::Chat as i32;
        let decision = InferenceRequestKind::Decision as i32;
        let mut declaration = crate::test_support::routed_site(&["chat-model", "decision-model"]);
        let Some(Source::Local(local)) = declaration.model_deployments[1]
            .snapshot
            .as_mut()
            .and_then(|snapshot| snapshot.source.as_mut())
        else {
            panic!("local fixture snapshot");
        };
        local.request_kinds = vec![decision];

        let directory = crate::test_support::canonical_tempdir().expect("Site directory");
        let path = directory.path().join("site.yaml");
        std::fs::write(
            &path,
            dotcore_proto::yaml::to_string(&declaration).expect("site YAML"),
        )
        .expect("Site fixture");
        let site = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
            .expect("validated site");
        let (_, serving_targets) =
            crate::model::controller::serving_targets::ServingTargetsPublisher::new();
        let routes = RouteManager::new(
            crate::route_manager::RouteDeclarationRepository::new(site),
            crate::route_manager::RouteAvailabilityTargetsSource::new(serving_targets),
        )
        .expect("Route manager");
        let resolver = inference_route_resolver(routes.clone());
        let create = |name: &str, members: &[&str]| -> InferenceRouteView {
            routes
                .create(CreateRouteRequest {
                    name: name.to_owned(),
                    enabled: true,
                    fallback_groups: members
                        .iter()
                        .map(|member| FallbackGroup {
                            selection: FallbackGroupSelection::Single as i32,
                            model_deployment_ids: vec![(*member).to_owned()],
                        })
                        .collect(),
                })
                .expect("created route")
        };
        let kinds = |view: &InferenceRouteView| view.request_kinds.clone();

        let chat_only = create("chat-only", &["chat-model"]);
        let decision_only = create("decision-only", &["decision-model"]);
        let mixed = create("mixed", &["chat-model", "decision-model"]);

        // No member is ready, so the kinds come from the declarations alone.
        assert_eq!(kinds(&chat_only), [chat]);
        assert_eq!(kinds(&decision_only), [decision]);
        assert_eq!(kinds(&mixed), [chat, decision]);

        let link = |view: &InferenceRouteView| {
            let route = view.route.as_ref().expect("route declaration");
            resolver
                .resolve_for_create(&route.inference_route_id, route.generation)
                .map(|resolved| resolved.route_name)
        };
        assert_eq!(link(&chat_only), Ok("chat-only".to_owned()));
        assert_eq!(link(&mixed), Ok("mixed".to_owned()));
        assert_eq!(
            link(&decision_only),
            Err(InferenceRouteResolutionError::NotChat)
        );
    }

    #[test]
    fn route_manager_projection_changes_application_assignments_only_on_rename() {
        use dotcore_proto::dotcore::{
            application::private::v1::{
                ApplicationLinkedRouteProjection, application_linked_route_projection,
            },
            inference::v1::{
                CreateRouteRequest, DeleteRouteRequest, FallbackGroup, FallbackGroupSelection,
                ReplaceRouteRequest,
            },
        };
        use dotcore_site::view::desired::ApplicationNodeViewOwner as _;

        let directory = crate::test_support::canonical_tempdir().expect("Site directory");
        let path = directory.path().join("site.yaml");
        std::fs::write(
            &path,
            dotcore_proto::yaml::to_string(&crate::test_support::routed_site(&[
                "model-one",
                "model-two",
            ]))
            .expect("site YAML"),
        )
        .expect("Site fixture");
        let site = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
            .expect("validated site");
        let current = FileCurrentState::open(&path, "site-one").expect("current state");
        let (_, serving_targets) =
            crate::model::controller::serving_targets::ServingTargetsPublisher::new();
        let routes = RouteManager::new(
            crate::route_manager::RouteDeclarationRepository::new(site.clone()),
            crate::route_manager::RouteAvailabilityTargetsSource::new(serving_targets),
        )
        .expect("Route manager");
        let section = crate::application::ApplicationNodeViewSection::new(
            inference_route_resolver(routes.clone()),
            site,
            current,
        );
        let group = |model_deployment_id: &str| FallbackGroup {
            selection: FallbackGroupSelection::Single as i32,
            model_deployment_ids: vec![model_deployment_id.to_owned()],
        };
        let route = routes
            .create(CreateRouteRequest {
                name: "chat".to_owned(),
                enabled: true,
                fallback_groups: vec![group("model-one")],
            })
            .expect("created route")
            .route
            .expect("route declaration");
        let declaration =
            dotcore_proto::dotcore::application::v1::ApplicationDeploymentDeclaration {
                application_deployment_id: "application-one".to_owned(),
                inference_route_id: Some(route.inference_route_id.clone()),
                lifecycle_generation: 1,
                ..Default::default()
            };
        let linked_route = |name: Option<&str>| {
            Some(ApplicationLinkedRouteProjection {
                resolution: Some(match name {
                    Some(name) => {
                        application_linked_route_projection::Resolution::RouteName(name.to_owned())
                    }
                    None => application_linked_route_projection::Resolution::Unknown(
                        dotcore_proto::google::protobuf::Empty {},
                    ),
                }),
            })
        };
        let created = section.assignments(std::slice::from_ref(&declaration));
        assert_eq!(created[0].linked_route, linked_route(Some("chat")));

        let replace = |generation: u64, name: &str, enabled: bool, member: &str| {
            routes
                .replace(ReplaceRouteRequest {
                    inference_route_id: route.inference_route_id.clone(),
                    expected_generation: generation,
                    name: name.to_owned(),
                    enabled,
                    fallback_groups: vec![group(member)],
                })
                .expect("replaced route");
        };
        replace(1, "chat", false, "model-one");
        assert_eq!(
            section.assignments(std::slice::from_ref(&declaration)),
            created,
            "disable"
        );
        replace(2, "chat", true, "model-one");
        assert_eq!(
            section.assignments(std::slice::from_ref(&declaration)),
            created,
            "enable"
        );
        replace(3, "chat", true, "model-two");
        assert_eq!(
            section.assignments(std::slice::from_ref(&declaration)),
            created,
            "members"
        );

        replace(4, "chat-renamed", true, "model-two");
        let renamed = section.assignments(std::slice::from_ref(&declaration));
        assert_eq!(renamed[0].linked_route, linked_route(Some("chat-renamed")));
        assert_ne!(
            ActionAssignment::from(&renamed[0]),
            ActionAssignment::from(&created[0]),
            "a rename changes the application assignment digest"
        );

        routes
            .delete(DeleteRouteRequest {
                inference_route_id: route.inference_route_id.clone(),
                expected_generation: 5,
            })
            .expect("deleted route");
        assert_eq!(
            section.assignments(std::slice::from_ref(&declaration))[0].linked_route,
            linked_route(None)
        );
    }

    struct ReadyPreparer;

    impl dotcore_artifact::OciImagePreparer for ReadyPreparer {
        fn prepare(
            &self,
            _: dotcore_artifact::OciPreparationRequest,
            _: CancellationToken,
            _: dotcore_artifact::StepOutput,
        ) -> dotcore_artifact::OciPreparationFuture {
            Box::pin(async { Ok(()) })
        }
    }
}
