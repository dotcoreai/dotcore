//! Management and private route assembly for the capabilities selected by backend composition.

use std::{path::PathBuf, sync::Arc, time::Duration};

use dotcore_domain::clock::Clock;
use dotcore_site::{
    membership::{
        MachineRemoval, MembershipManagementService, RescanManagementService, RescanRegistry,
        join::JoinRegistry,
    },
    private_transport::PeerPins,
    view::fencing::AcceptedNodeViewAuthority,
};

use super::BackendServices;

/// The site manager's join and re-scan registries, what its node removal changes, and how its
/// management watches send heartbeats.
pub(super) struct ManagerMembership {
    pub(super) registry: JoinRegistry,

    /// Re-scan invitations the manager holds for its worker nodes.
    pub(super) rescans: RescanRegistry,
    /// Root of the site whose worker pins a removal deletes.
    pub(super) site_root: PathBuf,

    /// Live pins a removal withdraws the removed node's trust from.
    pub(super) pins: PeerPins,

    pub(super) clock: Arc<dyn Clock>,
    pub(super) heartbeat_interval: Duration,
}

pub(super) fn assemble(
    services: BackendServices,
    membership: ManagerMembership,
) -> (
    tonic::service::Routes,
    crate::router::InferenceRouter,
    super::ObserverStarters,
    crate::server::private::PrivateServiceAssembly,
    super::backend::BackendLifecycleHandles,
    crate::resource_watch::ResourceWatchHub,
) {
    let BackendServices {
        model_service,
        application_service,
        manager,
        observers,
        node_lifecycle,
        node,
    } = services;
    let super::ManagerBackendServices {
        catalog_service,
        route_service,
        inference_router,
        site_service,
        diagnostic_service,
        metrics_service,
        capacity_service,
        resource_watch,
        metric_buckets,
        node_control_streams,
        node_targets,
        site_repository,
    } = manager;
    let removal = MachineRemoval::new(
        site_repository,
        &membership.site_root,
        membership.pins,
        Arc::new(node_lifecycle.clone()),
    );
    let lifecycle = super::backend::BackendLifecycleHandles::Manager {
        _nodes: node_lifecycle,
        _node_targets: node_targets,
        _node: node,
    };

    let join_service = membership.registry.join_service();
    let rescan_service = membership.rescans.rescan_service();
    let private_services =
        crate::server::private::PrivateServiceAssembly::new(move |context, routes| {
            // Machines without a site certificate reach only this service, while a join is active.
            routes.add_service(join_service);
            // A declared worker re-scans its targets here, with its own pinned identity.
            routes.add_service(rescan_service);
            routes.add_service(node_control_streams.private_service(&context).into_server());
            routes.add_service(
                crate::metric_stream::MetricBucketStreamService::new(metric_buckets, &context)
                    .into_server(),
            );
        });

    let routes = tonic::service::Routes::new(
        dotcore_proto::dotcore::catalog::v1::catalog_service_server::CatalogServiceServer::new(
            catalog_service,
        ),
    )
    .add_service(
        dotcore_proto::dotcore::model::v1::model_deployment_service_server::ModelDeploymentServiceServer::new(
            model_service,
        ),
    )
    .add_service(
        dotcore_proto::dotcore::application::v1::application_deployment_service_server::ApplicationDeploymentServiceServer::new(
            application_service,
        ),
    )
    .add_service(
        dotcore_proto::dotcore::inference::v1::route_service_server::RouteServiceServer::new(
            route_service,
        ),
    )
    .add_service(
        dotcore_proto::dotcore::site::v1::site_service_server::SiteServiceServer::from_arc(site_service),
    )
    .add_service(
        dotcore_proto::dotcore::diagnostic::v1::diagnostic_service_server::DiagnosticServiceServer::new(
            diagnostic_service,
        ),
    )
    .add_service(metrics_service.into_server())
    .add_service(capacity_service.into_server())
    .add_service(
        crate::resource_watch::ResourceWatchService::new(resource_watch.clone()).into_server(),
    )
    .add_service(
        MembershipManagementService::new(
            membership.registry,
            removal,
            Arc::clone(&membership.clock),
            membership.heartbeat_interval,
        )
        .into_server(),
    )
    .add_service(
        RescanManagementService::new(
            membership.rescans,
            membership.clock,
            membership.heartbeat_interval,
        )
        .into_server(),
    );
    (
        routes,
        inference_router,
        observers,
        private_services,
        lifecycle,
        resource_watch,
    )
}

/// Private services a node in its own process hosts for the site manager: exact-target inference
/// over the node's installed view and its startup-created serving targets, deployment logs of
/// the deployments that view assigns, and the node's retained backend diagnostics.
pub(super) fn worker_private_services(
    view: AcceptedNodeViewAuthority,
    serving_targets: crate::model::controller::serving_targets::LocalServingTargetsSource,
    log_owners: crate::deployment_logs::DeploymentLogOwners,
    diagnostics: crate::diagnostic::NodeDiagnosticReader,
) -> crate::server::private::PrivateServiceAssembly {
    crate::server::private::PrivateServiceAssembly::new(move |context, routes| {
        routes.add_service(
            crate::router::peer_service::PeerInferenceService::new(
                crate::router::peer_service::ExactTargetAdmission::new(
                    view.clone(),
                    serving_targets,
                ),
                &context,
            )
            .into_server(),
        );
        routes.add_service(
            crate::deployment_logs::DeploymentLogService::new(
                crate::deployment_logs::DeploymentLogReader::new(
                    view,
                    log_owners,
                    crate::management_logs::ManagementLogAdmission::new(),
                ),
                &context,
            )
            .into_server(),
        );
        routes.add_service(
            crate::diagnostic::NodeDiagnosticService::new(diagnostics, &context).into_server(),
        );
    })
}
