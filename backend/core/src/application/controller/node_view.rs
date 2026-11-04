//! Apps-controller owner of the application section of node views on the site manager.
//!
//! It turns each application declaration placed on a node into a complete assignment, adding
//! the current name of a linked inference route read through the route manager port, for example
//! `chat-default`, or an explicit unknown marker when the route manager no longer knows the route.
//! Route availability and enabled state are never projected, so only a rename changes an
//! assignment. It also accepts the application status those nodes report.

use std::sync::Arc;

use dotcore_proto::dotcore::application::{
    private::v1::{
        ApplicationAssignment, ApplicationCleanupReceipt, ApplicationControllerStatusSection,
        ApplicationLinkedRouteProjection, application_linked_route_projection,
    },
    v1::ApplicationDeploymentDeclaration,
};
use dotcore_site::{
    repository::SiteRepository,
    store::FileCurrentState,
    view::{
        desired::ApplicationNodeViewOwner,
        fencing::{ManagerSession, NodeIncarnation, NodeViewProjection},
    },
};
use tracing::warn;

use super::{
    InferenceRouteResolutionError, InferenceRouteResolver,
    remote_status::{ApplicationStatusReport, accept_application_status},
};

/// Manager apps-controller owner of node application sections and of the application status
/// those nodes report.
pub(crate) struct ApplicationNodeViewSection {
    inference_route_resolver: Arc<dyn InferenceRouteResolver>,
    site: SiteRepository,
    current: FileCurrentState,
}

impl ApplicationNodeViewSection {
    /// Builds the section owner over the manager's authoritative site and the same current state
    /// the apps-controller owner projects from.
    pub(crate) fn new(
        inference_route_resolver: Arc<dyn InferenceRouteResolver>,
        site: SiteRepository,
        current: FileCurrentState,
    ) -> Self {
        Self {
            inference_route_resolver,
            site,
            current,
        }
    }

    fn linked_route(
        &self,
        declaration: &ApplicationDeploymentDeclaration,
    ) -> Option<ApplicationLinkedRouteProjection> {
        let inference_route_id = declaration.inference_route_id.as_deref()?;
        let resolution = match self
            .inference_route_resolver
            .project_current(inference_route_id)
        {
            Ok(route) => {
                application_linked_route_projection::Resolution::RouteName(route.route_name)
            }
            Err(error) => {
                if error != InferenceRouteResolutionError::Unknown {
                    warn!(event_kind = "application_route_projection", outcome = "unresolved", deployment_id = %declaration.application_deployment_id, inference_route_id, error = %error, "Linked inference route could not be projected for a node; it is projected as unknown");
                }
                application_linked_route_projection::Resolution::Unknown(
                    dotcore_proto::google::protobuf::Empty {},
                )
            }
        };
        Some(ApplicationLinkedRouteProjection {
            resolution: Some(resolution),
        })
    }
}

impl ApplicationNodeViewOwner for ApplicationNodeViewSection {
    fn assignments(
        &self,
        declarations: &[ApplicationDeploymentDeclaration],
    ) -> Vec<ApplicationAssignment> {
        declarations
            .iter()
            .map(|declaration| ApplicationAssignment {
                declaration: Some(declaration.clone()),
                linked_route: self.linked_route(declaration),
            })
            .collect()
    }

    fn accept_remote_status(
        &self,
        node_id: &str,
        projection: &NodeViewProjection,
        application: &ApplicationControllerStatusSection,
        manager_session: &ManagerSession,
        node_incarnation: &NodeIncarnation,
        status_sequence: u64,
    ) -> Vec<ApplicationCleanupReceipt> {
        accept_application_status(
            &self.site,
            &self.current,
            &ApplicationStatusReport {
                node_id,
                projection,
                application,
                manager_session,
                node_incarnation,
                status_sequence,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::{
        application::{
            private::v1::{
                ApplicationAssignment, ApplicationLinkedRouteProjection,
                ApplicationStatusFailureCode, application_linked_route_projection,
            },
            v1::{ApplicationDeploymentDeclaration, ApplicationDeploymentDesiredState},
        },
        deployment::v1::ExactPlacement,
        site::v1::{ExecutionTarget, ModelRuntimeBinding, execution_target},
    };
    use dotcore_site::{
        repository::SiteRepository, store::FileCurrentState,
        view::desired::ApplicationNodeViewOwner as _,
    };

    use super::super::{
        InferenceRouteResolutionError, ResolvedInferenceRoute,
        test_support::{
            application_execution_target, application_with_route_input, deployment, resolved_route,
            resolved_snapshot, test_inference_route_resolver_with,
        },
    };
    use super::ApplicationNodeViewSection;
    use crate::application::classify_application_assignment;

    #[test]
    fn linked_routes_project_their_current_name_or_an_explicit_unknown_marker() {
        let linked = linked_declaration();
        let mut unlinked = deployment(ApplicationDeploymentDesiredState::Paused);
        unlinked.application_deployment_id = "unlinked-1".to_owned();

        assert_eq!(
            section(Ok(resolved_route("route-one")))
                .assignments(&[linked.clone(), unlinked.clone()]),
            [
                ApplicationAssignment {
                    declaration: Some(linked.clone()),
                    linked_route: Some(route_name("route-one")),
                },
                ApplicationAssignment {
                    declaration: Some(unlinked),
                    linked_route: None,
                },
            ]
        );
        for error in [
            InferenceRouteResolutionError::Unknown,
            InferenceRouteResolutionError::Unavailable,
        ] {
            assert_eq!(
                section(Err(error)).assignments(std::slice::from_ref(&linked))[0].linked_route,
                Some(unknown_route()),
                "{error}"
            );
        }
    }

    #[test]
    fn each_invalid_assignment_is_only_its_own_typed_failure() {
        let targets = [application_execution_target(), model_target()];
        let valid = assignment(linked_declaration(), Some(route_name("route-one")));
        assert_eq!(
            classify_application_assignment("node-one", &targets, &valid),
            Ok(())
        );
        assert_eq!(
            classify_application_assignment(
                "node-one",
                &targets,
                &assignment(deployment(ApplicationDeploymentDesiredState::Present), None)
            ),
            Ok(())
        );
        let dotted = assignment(linked_declaration(), Some(route_name("claude-3.5-sonnet")));
        assert_eq!(
            classify_application_assignment("node-one", &targets, &dotted),
            Ok(())
        );

        let mut never_generated = linked_declaration();
        never_generated.lifecycle_generation = 0;
        let mut on_model_target = linked_declaration();
        place(&mut on_model_target, "node-one", "model-target-one");
        let mut on_other_node = linked_declaration();
        place(&mut on_other_node, "node-two", "application-target-one");
        let mut arm64_target = application_execution_target();
        arm64_target
            .platform
            .as_mut()
            .expect("target platform")
            .architecture = "arm64".to_owned();
        let cases = [
            (
                "unknown linked route",
                &targets[..],
                assignment(linked_declaration(), Some(unknown_route())),
                ApplicationStatusFailureCode::LinkedRouteUnknown,
            ),
            (
                "linked route without projection",
                &targets[..],
                assignment(linked_declaration(), None),
                ApplicationStatusFailureCode::InvalidAssignment,
            ),
            (
                "projection without a linked route",
                &targets[..],
                assignment(
                    deployment(ApplicationDeploymentDesiredState::Present),
                    Some(route_name("route-one")),
                ),
                ApplicationStatusFailureCode::InvalidAssignment,
            ),
            (
                "projected route name that is not an identifier",
                &targets[..],
                assignment(linked_declaration(), Some(route_name("Route One"))),
                ApplicationStatusFailureCode::InvalidAssignment,
            ),
            (
                "zero lifecycle generation",
                &targets[..],
                assignment(never_generated, Some(route_name("route-one"))),
                ApplicationStatusFailureCode::InvalidAssignment,
            ),
            (
                "placement on a model target",
                &targets[..],
                assignment(on_model_target, Some(route_name("route-one"))),
                ApplicationStatusFailureCode::TargetMismatch,
            ),
            (
                "placement on another node",
                &targets[..],
                assignment(on_other_node, Some(route_name("route-one"))),
                ApplicationStatusFailureCode::TargetMismatch,
            ),
            (
                "installed target platform differs from the snapshot",
                std::slice::from_ref(&arm64_target),
                valid.clone(),
                ApplicationStatusFailureCode::TargetMismatch,
            ),
        ];
        for (case, targets, assignment, failure) in cases {
            assert_eq!(
                classify_application_assignment("node-one", targets, &assignment),
                Err(failure),
                "{case}"
            );
        }
    }

    fn section(
        route: Result<ResolvedInferenceRoute, InferenceRouteResolutionError>,
    ) -> ApplicationNodeViewSection {
        let directory = crate::test_support::canonical_tempdir().expect("Site directory");
        let path = directory.path().join("site.yaml");
        std::fs::write(
            &path,
            dotcore_proto::yaml::to_string(&super::super::test_support::node_site(Vec::new()))
                .expect("site YAML"),
        )
        .expect("Site fixture");
        ApplicationNodeViewSection::new(
            test_inference_route_resolver_with(route),
            SiteRepository::load(&path, crate::test_support::site_declaration_rules())
                .expect("validated site"),
            FileCurrentState::open(&path, "site-one").expect("current state"),
        )
    }

    fn linked_declaration() -> ApplicationDeploymentDeclaration {
        let mut declaration = deployment(ApplicationDeploymentDesiredState::Present);
        declaration.snapshot = Some(resolved_snapshot(application_with_route_input()));
        declaration.inference_route_id = Some("route-id".to_owned());
        declaration
    }

    fn assignment(
        declaration: ApplicationDeploymentDeclaration,
        linked_route: Option<ApplicationLinkedRouteProjection>,
    ) -> ApplicationAssignment {
        ApplicationAssignment {
            declaration: Some(declaration),
            linked_route,
        }
    }

    fn place(declaration: &mut ApplicationDeploymentDeclaration, node_id: &str, target_id: &str) {
        let placement = ExactPlacement {
            site_node_id: node_id.to_owned(),
            execution_target_id: target_id.to_owned(),
        };
        declaration.placement = Some(placement.clone());
        declaration
            .snapshot
            .as_mut()
            .expect("declaration snapshot")
            .placement = Some(placement);
    }

    fn route_name(name: &str) -> ApplicationLinkedRouteProjection {
        ApplicationLinkedRouteProjection {
            resolution: Some(application_linked_route_projection::Resolution::RouteName(
                name.to_owned(),
            )),
        }
    }

    fn unknown_route() -> ApplicationLinkedRouteProjection {
        ApplicationLinkedRouteProjection {
            resolution: Some(application_linked_route_projection::Resolution::Unknown(
                dotcore_proto::google::protobuf::Empty {},
            )),
        }
    }

    fn model_target() -> ExecutionTarget {
        let mut target = application_execution_target();
        target.execution_target_id = "model-target-one".to_owned();
        target.runtime_binding = Some(execution_target::RuntimeBinding::ModelRuntime(
            ModelRuntimeBinding::default(),
        ));
        target
    }
}
