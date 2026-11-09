//! Publishes node-hosted model deployments as node serving targets for the manager's inference
//! router, using fresh node status as the only readiness evidence.
//!
//! A deployment is routable only while its node is connected, its latest accepted status is under
//! ten seconds old, and the site still declares that exact generation Present on that node with a
//! Ready binding. A node in another process must also have reported a private port; the node
//! sharing the manager process is dispatched in process instead. Each pass copies node evidence
//! first and then publishes while holding the site guard, so a pass that started before a pause or
//! unload can never republish the target that operation withdrew.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use dotcore_proto::dotcore::{
    model::{
        private::v1::{
            ModelBindingReadiness, ModelDeploymentStatus as PrivateModelDeploymentStatus,
            ModelLifecycleObservation,
        },
        v1::{ModelDeploymentDeclaration, ModelDeploymentDesiredState},
    },
    site::v1::Site,
};
use dotcore_site::{
    node::control::{ManagerNodeControlCoordinator, NodeServingEvidence},
    repository::SiteRepository,
    view::fencing::ManagerSession,
    worker_channels::{WorkerAddress, declared_worker_host},
};
use tokio::time::Instant;
use tracing::{Instrument, info, info_span, warn};

use super::serving_targets::{
    NodeDispatch, NodeServingTarget, NodeTransport, ServingTargetsPublisher, declared_request_kinds,
};
use crate::model::{domain::ModelDeploymentId, resolved::ResolvedModelLaunch};

/// The inference controller's single projector of node evidence into node serving targets.
#[derive(Clone)]
pub(crate) struct NodeTargetProjector {
    site: SiteRepository,
    coordinator: ManagerNodeControlCoordinator,
    publisher: ServingTargetsPublisher,
    /// SiteNode sharing the manager process, whose targets are dispatched in process.
    local_node_id: String,
    published: Arc<Mutex<BTreeMap<String, String>>>,
}

impl NodeTargetProjector {
    pub(crate) fn new(
        site: SiteRepository,
        coordinator: ManagerNodeControlCoordinator,
        publisher: ServingTargetsPublisher,
        local_node_id: String,
    ) -> Self {
        Self {
            site,
            coordinator,
            publisher,
            local_node_id,
            published: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Starts the projector task, which runs until the returned handle drops.
    ///
    /// Status acceptance, incarnation replacement, and site acceptance only signal one shared
    /// `Notify`; the task reprojects on that signal or when the earliest published target's
    /// evidence expires, so no trigger ever projects while holding its own lock.
    pub(crate) fn start(self) -> NodeTargetProjectorTask {
        let wakeup = self.coordinator.serving_evidence_changed();
        let clock = self.coordinator.clock();
        let site_changed = Arc::clone(&wakeup);
        self.site
            .register_change_observer(Arc::new(move |_| site_changed.notify_one()));
        // Detached from the caller's span: a child span would keep a short-lived operation span,
        // such as backend composition, open for the whole life of this task.
        let span = info_span!(
            parent: None,
            "model_node_targets",
            operation = "model_node_targets"
        );
        let task = tokio::spawn(
            async move {
                loop {
                    match self.project() {
                        Some(expiry) => {
                            tokio::select! {
                                () = wakeup.notified() => {}
                                () = clock.sleep_until(expiry) => {}
                            }
                        }
                        None => wakeup.notified().await,
                    }
                }
            }
            .instrument(span),
        );
        NodeTargetProjectorTask { task }
    }

    /// Runs one projection pass and returns when the earliest published target expires.
    pub(crate) fn project(&self) -> Option<Instant> {
        self.publish(self.coordinator.serving_evidence())
    }

    /// Publishes node targets from already copied evidence while holding the site guard.
    fn publish(&self, evidence: Vec<NodeServingEvidence>) -> Option<Instant> {
        let session = self.coordinator.manager_session().clone();
        let result = self.site.with_site(|site| {
            let projection = project_node_targets(
                site,
                &evidence,
                &session,
                &self.local_node_id,
                self.coordinator.clock().now(),
            );
            self.publisher.replace_node_targets(
                projection
                    .targets
                    .iter()
                    .map(|(target, _)| target.clone())
                    .collect(),
            );
            projection
        });
        match result {
            Ok(projection) => {
                self.record_changes(&projection);
                projection.targets.iter().map(|(_, expiry)| *expiry).min()
            }
            Err(error) => {
                self.publisher.replace_node_targets(Vec::new());
                warn!(event_kind = "model_node_serving_targets_withdrawn", reason = "site_unavailable", error = %error, "Manager withdrew every node serving target because the site could not be read");
                self.record_changes(&NodeTargetProjection::default());
                None
            }
        }
    }

    /// Emits one event per deployment whose node publication changed in this pass.
    fn record_changes(&self, projection: &NodeTargetProjection) {
        let mut published = self
            .published
            .lock()
            .unwrap_or_else(|value| value.into_inner());
        let current = projection
            .targets
            .iter()
            .map(|(target, _)| (target.deployment_id.0.clone(), target.site_node_id.clone()))
            .collect::<BTreeMap<_, _>>();
        for (deployment_id, node_id) in &current {
            if published.get(deployment_id) != Some(node_id) {
                info!(event_kind = "model_node_serving_target_published", deployment_id = %deployment_id, node_id = %node_id, "Manager published a node-hosted serving target");
            }
        }
        for (deployment_id, node_id) in published.iter() {
            if !current.contains_key(deployment_id) {
                let reason = projection
                    .excluded
                    .get(deployment_id)
                    .map_or("not_reported", |reason| reason.as_str());
                info!(event_kind = "model_node_serving_target_withdrawn", deployment_id = %deployment_id, node_id = %node_id, reason, "Manager withdrew a node-hosted serving target");
            }
        }
        *published = current;
    }
}

/// Running projector task of one composition; dropping it stops the projection.
pub(crate) struct NodeTargetProjectorTask {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for NodeTargetProjectorTask {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Node targets with their evidence expiry, plus the reason each reported deployment is not one.
#[derive(Default)]
struct NodeTargetProjection {
    targets: Vec<(NodeServingTarget, Instant)>,
    excluded: BTreeMap<String, NodeTargetExclusion>,
}

/// Why a worker-reported deployment is not published as a node target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NodeTargetExclusion {
    NodeNotConnected,

    NoPrivatePort,

    StatusStale,

    NotDeclaredPresent,

    BindingNotReady,

    InvalidDeclaration,
}

impl NodeTargetExclusion {
    fn as_str(self) -> &'static str {
        match self {
            Self::NodeNotConnected => "node_not_connected",
            Self::NoPrivatePort => "no_private_port",
            Self::StatusStale => "status_stale",
            Self::NotDeclaredPresent => "not_declared_present",
            Self::BindingNotReady => "binding_not_ready",
            Self::InvalidDeclaration => "invalid_declaration",
        }
    }
}

/// Projects every node's evidence against the authoritative site at `now`.
///
/// Identity is the node, manager session, node incarnation, deployment, lifecycle generation,
/// and binding token; view revisions and receipt times never enter a target, so an unrelated view
/// change or a fresher identical status leaves the published target, and its lease, unchanged.
/// The transport is chosen by node identity before any address is resolved: the node sharing the
/// manager process, `local_node_id`, is always dispatched in process.
fn project_node_targets(
    site: &Site,
    evidence: &[NodeServingEvidence],
    manager_session: &ManagerSession,
    local_node_id: &str,
    now: Instant,
) -> NodeTargetProjection {
    let mut projection = NodeTargetProjection::default();
    for node in evidence {
        let Some(status) = node.status.as_ref() else {
            continue;
        };
        // A node re-sends its status on every change, so its latest status stays current until
        // the node falls silent.
        let session_current = status.manager_session_id == manager_session.as_str();
        let transport = node_transport(site, node, local_node_id);
        // Evidence expires exactly at its deadline, so a pass woken by that deadline withdraws.
        let node_exclusion = match &transport {
            Err(NodeTargetExclusion::InvalidDeclaration) => {
                Some(NodeTargetExclusion::InvalidDeclaration)
            }
            _ if now >= node.connected_until => Some(NodeTargetExclusion::NodeNotConnected),
            Err(exclusion) => Some(*exclusion),
            Ok(_) if !session_current => Some(NodeTargetExclusion::StatusStale),
            Ok(_) => None,
        };
        for reported in &status.deployments {
            let candidate = match (node_exclusion, &transport) {
                (None, Ok(transport)) => node_target(
                    site,
                    node,
                    reported,
                    NodeDispatch {
                        transport: transport.clone(),
                        manager_session: manager_session.clone(),
                        node_incarnation: node.incarnation.clone(),
                        binding_token: reported.binding_token.clone(),
                    },
                ),
                (exclusion, _) => Err(exclusion.unwrap_or(NodeTargetExclusion::NodeNotConnected)),
            };
            match candidate {
                Ok(target) => projection.targets.push((target, node.connected_until)),
                Err(reason) => {
                    projection
                        .excluded
                        .insert(reported.model_deployment_id.clone(), reason);
                }
            }
        }
    }
    projection
}

/// Chooses how one node is reached: in process for the node sharing the manager process, else at
/// its declared host and reported private port.
fn node_transport(
    site: &Site,
    node: &NodeServingEvidence,
    local_node_id: &str,
) -> Result<NodeTransport, NodeTargetExclusion> {
    if node.node_id == local_node_id {
        return Ok(NodeTransport::InProcess);
    }
    let host =
        declared_worker_host(site, &node.node_id).ok_or(NodeTargetExclusion::InvalidDeclaration)?;
    let port = node
        .private_port
        .ok_or(NodeTargetExclusion::NoPrivatePort)?;
    Ok(NodeTransport::Remote {
        address: WorkerAddress { host, port },
    })
}

/// Builds one target when the site declares exactly the reported generation Present on this node
/// and the node reports its binding Ready for that generation.
fn node_target(
    site: &Site,
    node: &NodeServingEvidence,
    reported: &PrivateModelDeploymentStatus,
    dispatch: NodeDispatch,
) -> Result<NodeServingTarget, NodeTargetExclusion> {
    let declaration = site
        .model_deployments
        .iter()
        .find(|declaration| {
            declaration.model_deployment_id == reported.model_deployment_id
                && declared_present_on(declaration, &node.node_id, reported)
        })
        .ok_or(NodeTargetExclusion::NotDeclaredPresent)?;
    if reported.lifecycle != ModelLifecycleObservation::Ready as i32
        || reported.binding_readiness != ModelBindingReadiness::Ready as i32
        || reported.binding_token.is_empty()
    {
        return Err(NodeTargetExclusion::BindingNotReady);
    }
    let resolved = ResolvedModelLaunch::from_declaration(declaration)
        .map_err(|_| NodeTargetExclusion::InvalidDeclaration)?;
    Ok(NodeServingTarget::new(
        ModelDeploymentId(declaration.model_deployment_id.clone()),
        declaration.lifecycle_generation,
        node.node_id.clone(),
        resolved.model_id,
        resolved.input_capabilities.image,
        declared_request_kinds(declaration),
        dispatch,
    ))
}

fn declared_present_on(
    declaration: &ModelDeploymentDeclaration,
    node_id: &str,
    reported: &PrivateModelDeploymentStatus,
) -> bool {
    declaration.desired_state == ModelDeploymentDesiredState::Present as i32
        && declaration.lifecycle_generation == reported.lifecycle_generation
        && declaration
            .placement
            .as_ref()
            .is_some_and(|placement| placement.site_node_id == node_id)
        && declaration.placement == reported.placement
        && !super::is_upstream_deployment(declaration)
}

#[cfg(test)]
mod tests {
    use std::{sync::Barrier, time::Duration};

    use dotcore_domain::clock::ManualClock;
    use dotcore_proto::{
        dotcore::{
            deployment::v1::ExactPlacement,
            inference::v1::{FallbackGroup, FallbackGroupSelection, InferenceRoute},
            model::{
                private::v1::ModelControllerStatusSection, v1::ModelDeploymentLifecycleTransition,
            },
            site::private::v1::{
                ControllerStatusSection, NodeControllerStatus, NodeControllerStatusAcceptance,
                NodeObservation, NodeViewReference, controller_status_section,
            },
        },
        google::protobuf::Timestamp,
    };
    use dotcore_site::{
        node::{
            control::{
                AcceptedNodeObservationInput, CoordinatorConnectionState, PublishedViewInput,
                ServingStatusEvidence,
            },
            observation_configuration::NodeObservationConfigurationDigest,
        },
        view::fencing::{ManagerSession, NodeIncarnation, NodeViewProjection},
    };

    use super::*;
    use crate::{
        model::controller::serving_targets::{
            ServingTarget, ServingTargetsRevocationReason, ServingTargetsSource,
        },
        route_manager::RouteAvailabilityTargetsSource,
    };

    const LIVENESS_TTL: Duration = dotcore_domain::timing::Timing::production()
        .node_control
        .node_silence;

    #[test]
    fn publishes_only_fresh_connected_ready_evidence_for_the_present_generation() {
        let manager = Manager::new(vec![declaration("model-one", "worker-one", 1)]);
        manager.register("worker-one", '1', 1, None);
        manager.report(
            "worker-one",
            '1',
            1,
            vec![ready("model-one", "worker-one", 1, 'b')],
        );
        manager.projector.project();
        assert!(manager.targets().is_empty(), "no private port");

        manager.register("worker-one", '1', 2, Some(47_123));
        manager.projector.project();
        let targets = manager.targets();
        let [ServingTarget::Node(target)] = targets.as_slice() else {
            panic!("expected one node target");
        };
        assert_eq!(target.deployment_id.0, "model-one");
        assert_eq!(target.lifecycle_generation, 1);
        assert_eq!(target.site_node_id, "worker-one");
        assert_eq!(
            target.dispatch().transport,
            NodeTransport::Remote {
                address: "127.0.0.2:47123".parse().unwrap()
            }
        );
        assert_eq!(target.dispatch().binding_token, "b".repeat(64));
        assert_eq!(target.dispatch().node_incarnation, incarnation('1'));
        assert_eq!(target.dispatch().manager_session, session());

        let mut not_ready = ready("model-one", "worker-one", 1, 'b');
        not_ready.binding_readiness = ModelBindingReadiness::NotReady as i32;
        not_ready.binding_token.clear();
        manager.report("worker-one", '1', 2, vec![not_ready]);
        manager.projector.project();
        assert!(manager.targets().is_empty(), "binding not Ready");

        manager.report(
            "worker-one",
            '1',
            3,
            vec![ready("model-one", "worker-one", 2, 'b')],
        );
        manager.projector.project();
        assert!(manager.targets().is_empty(), "generation not declared");

        manager.report(
            "worker-one",
            '1',
            4,
            vec![ready("model-one", "worker-one", 1, 'b')],
        );
        manager.projector.project();
        assert_eq!(manager.targets().len(), 1);
    }

    #[test]
    fn a_node_declared_by_host_name_is_dialed_at_that_name() {
        let manager = Manager::new(vec![declaration("model-two", "worker-two", 1)]);
        manager.register("worker-two", '2', 1, Some(47_124));
        manager.report(
            "worker-two",
            '2',
            1,
            vec![ready("model-two", "worker-two", 1, 'c')],
        );

        manager.projector.project();

        let targets = manager.targets();
        let [ServingTarget::Node(target)] = targets.as_slice() else {
            panic!("expected one node target, got {}", targets.len());
        };
        assert_eq!(
            target.dispatch().transport,
            NodeTransport::Remote {
                address: "worker-two.example.ts.net:47124".parse().unwrap()
            }
        );
    }

    #[test]
    fn the_manager_node_target_is_dispatched_in_process_without_a_private_port() {
        let manager = Manager::new(vec![declaration("model-m", MANAGER_NODE_ID, 1)]);
        manager.register(MANAGER_NODE_ID, 'e', 1, None);
        manager.report(
            MANAGER_NODE_ID,
            'e',
            1,
            vec![ready("model-m", MANAGER_NODE_ID, 1, 'c')],
        );

        manager.projector.project();

        let targets = manager.targets();
        let [ServingTarget::Node(target)] = targets.as_slice() else {
            panic!("expected one in-process node target");
        };
        assert_eq!(target.site_node_id, MANAGER_NODE_ID);
        assert_eq!(target.dispatch().transport, NodeTransport::InProcess);
        assert_eq!(target.dispatch().binding_token, "c".repeat(64));
        assert_eq!(target.dispatch().node_incarnation, incarnation('e'));
    }

    #[test]
    fn unrelated_view_change_revokes_nothing_while_older_view_status_stays_evidence() {
        let manager = Manager::new(vec![declaration("model-one", "worker-one", 1)]);
        manager.register("worker-one", '1', 1, Some(47_123));
        manager.report(
            "worker-one",
            '1',
            1,
            vec![ready("model-one", "worker-one", 1, 'b')],
        );
        manager.projector.project();
        let target = manager.targets().remove(0);
        let (epoch, revocation) = manager.lease(&target);

        manager.declare(declaration("model-two", "worker-one", 1));
        manager.publish_view("worker-one", '1');
        manager.projector.project();

        assert_eq!(manager.targets(), vec![target.clone()]);
        assert_eq!(manager.lease(&target).0, epoch);
        assert_eq!(*revocation.borrow(), None);
    }

    #[test]
    fn token_pause_and_unload_revoke_only_the_affected_deployment() {
        let manager = Manager::new(vec![
            declaration("model-one", "worker-one", 1),
            declaration("model-two", "worker-one", 1),
            declaration("model-three", "worker-one", 1),
            declaration("model-other", "worker-two", 1),
        ]);
        manager.register("worker-one", '1', 1, Some(47_123));
        manager.register("worker-two", '1', 1, Some(47_124));
        let statuses = |token| {
            vec![
                ready("model-one", "worker-one", 1, token),
                ready("model-two", "worker-one", 1, 'b'),
                ready("model-three", "worker-one", 1, 'b'),
            ]
        };
        manager.report("worker-one", '1', 1, statuses('b'));
        manager.report(
            "worker-two",
            '1',
            1,
            vec![ready("model-other", "worker-two", 1, 'b')],
        );
        manager.projector.project();
        let leases = manager.leases();
        assert_eq!(leases.len(), 4);

        manager.report("worker-one", '1', 2, statuses('c'));
        manager.projector.project();
        assert_eq!(manager.revoked(&leases), ["model-one"]);

        let leases = manager.leases();
        manager.transition("model-two", ModelDeploymentDesiredState::Paused);
        manager.projector.project();
        assert_eq!(manager.revoked(&leases), ["model-two"]);

        let leases = manager.leases();
        manager.transition("model-three", ModelDeploymentDesiredState::Teardown);
        manager.projector.project();
        assert_eq!(manager.revoked(&leases), ["model-three"]);
        assert_eq!(manager.published_ids(), ["model-one", "model-other"]);
    }

    #[test]
    fn a_status_serves_until_its_node_falls_silent_only_in_the_current_manager_session() {
        let manager = Manager::new(vec![declaration("model-one", "worker-one", 1)]);
        let site = manager.site.snapshot().expect("site");
        let now = Instant::now() + Duration::from_secs(60);
        let live_until = now + Duration::from_secs(5);
        let evidence = |manager_session: &ManagerSession| NodeServingEvidence {
            node_id: "worker-one".to_owned(),
            incarnation: incarnation('1'),
            private_port: Some(47_123),
            connected_until: live_until,
            status: Some(ServingStatusEvidence {
                received: now - LIVENESS_TTL * 3,
                manager_session_id: manager_session.as_str().to_owned(),
                deployments: vec![ready("model-one", "worker-one", 1, 'b')],
            }),
        };
        let project = |evidence: NodeServingEvidence, at: Instant| {
            project_node_targets(&site, &[evidence], &session(), MANAGER_NODE_ID, at)
        };

        let live = project(evidence(&session()), now);
        assert_eq!(
            live.targets
                .iter()
                .map(|(target, expiry)| (target.deployment_id.0.as_str(), *expiry))
                .collect::<Vec<_>>(),
            [("model-one", live_until)],
            "an old status serves until the node falls silent"
        );

        let silent = project(evidence(&session()), live_until);
        assert!(silent.targets.is_empty());
        assert_eq!(
            silent.excluded.get("model-one"),
            Some(&NodeTargetExclusion::NodeNotConnected)
        );

        let previous_session =
            ManagerSession::parse("f".repeat(32)).expect("previous manager session");
        let replaced = project(evidence(&previous_session), now);
        assert!(replaced.targets.is_empty());
        assert_eq!(
            replaced.excluded.get("model-one"),
            Some(&NodeTargetExclusion::StatusStale),
            "a status of another manager session never serves"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_node_revokes_only_its_own_targets() {
        let manager = Manager::new(vec![
            declaration("model-one", "worker-one", 1),
            declaration("model-other", "worker-two", 1),
        ]);
        manager.register("worker-one", '1', 1, Some(47_123));
        manager.register("worker-two", '1', 1, Some(47_124));
        manager.report(
            "worker-one",
            '1',
            1,
            vec![ready("model-one", "worker-one", 1, 'b')],
        );
        manager.report(
            "worker-two",
            '1',
            1,
            vec![ready("model-other", "worker-two", 1, 'b')],
        );
        let expiry = manager
            .projector
            .project()
            .expect("published targets expire");
        assert_eq!(expiry, Instant::now() + LIVENESS_TTL);
        let leases = manager.leases();

        tokio::time::advance(LIVENESS_TTL).await;
        manager.report(
            "worker-two",
            '1',
            2,
            vec![ready("model-other", "worker-two", 1, 'b')],
        );
        manager.projector.project();

        assert_eq!(manager.revoked(&leases), ["model-one"]);
        assert_eq!(manager.published_ids(), ["model-other"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_node_and_an_unreadable_site_keep_the_upstream_target_and_lease() {
        let manager = Manager::new(vec![declaration("model-one", "worker-one", 1)]);
        let upstream = ServingTarget::Upstream(
            crate::model::controller::serving_targets::upstream_target_for_test(
                "upstream-deployment",
            ),
        );
        manager
            .publisher
            .upsert_deployment(upstream.deployment_id(), vec![upstream.clone()]);
        let (epoch, revocation) = manager.lease(&upstream);
        manager.register("worker-one", '1', 1, Some(47_123));
        manager.report(
            "worker-one",
            '1',
            1,
            vec![ready("model-one", "worker-one", 1, 'b')],
        );
        manager.projector.project();
        assert_eq!(
            manager.published_ids(),
            ["model-one", "upstream-deployment"]
        );
        let node_leases = manager.leases();

        tokio::time::advance(LIVENESS_TTL).await;
        manager.projector.project();
        assert_eq!(manager.published_ids(), ["upstream-deployment"]);
        assert_eq!(manager.revoked(&node_leases), ["model-one"]);

        manager.report(
            "worker-one",
            '1',
            2,
            vec![ready("model-one", "worker-one", 1, 'b')],
        );
        manager.projector.project();
        assert_eq!(
            manager.published_ids(),
            ["model-one", "upstream-deployment"]
        );
        let site = manager.site.clone();
        std::thread::spawn(move || {
            let _ = site.mutate_model_deployments(|_| -> Result<(), std::convert::Infallible> {
                panic!("a panicking site mutation leaves the site unreadable")
            });
        })
        .join()
        .expect_err("the site mutation panicked");
        manager.projector.project();

        assert_eq!(manager.targets(), vec![upstream.clone()]);
        assert_eq!(manager.lease(&upstream).0, epoch);
        assert_eq!(*revocation.borrow(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_live_node_with_an_hour_old_observation_keeps_serving() {
        let manager = Manager::new(vec![declaration("model-one", "worker-one", 1)]);
        manager.register("worker-one", '1', 1, Some(47_123));
        // Status reports continue for an hour while observations stop after the first one.
        for sequence in 1..=720 {
            tokio::time::advance(Duration::from_secs(5)).await;
            manager.report(
                "worker-one",
                '1',
                sequence,
                vec![ready("model-one", "worker-one", 1, 'b')],
            );
        }
        manager.projector.project();

        assert_eq!(
            manager
                .coordinator
                .observation(&dotcore_domain::SiteNodeId::for_test("worker-one"))
                .expect("observation")
                .connection_state,
            CoordinatorConnectionState::Stale
        );
        assert_eq!(manager.published_ids(), ["model-one"]);
    }

    #[tokio::test(start_paused = true)]
    async fn incarnation_replacement_revokes_only_that_node() {
        let manager = Manager::new(vec![
            declaration("model-one", "worker-one", 1),
            declaration("model-other", "worker-two", 1),
        ]);
        manager.register("worker-one", '1', 1, Some(47_123));
        manager.register("worker-two", '1', 1, Some(47_124));
        manager.report(
            "worker-one",
            '1',
            1,
            vec![ready("model-one", "worker-one", 1, 'b')],
        );
        manager.report(
            "worker-two",
            '1',
            1,
            vec![ready("model-other", "worker-two", 1, 'b')],
        );
        manager.projector.project();
        let leases = manager.leases();

        // Node-one falls silent past the liveness window while worker-two keeps reporting.
        tokio::time::advance(LIVENESS_TTL / 2).await;
        manager.report(
            "worker-two",
            '1',
            2,
            vec![ready("model-other", "worker-two", 1, 'b')],
        );
        tokio::time::advance(LIVENESS_TTL / 2 + Duration::from_secs(1)).await;
        manager.register("worker-one", '2', 1, Some(47_123));
        manager.report(
            "worker-one",
            '2',
            1,
            vec![ready("model-one", "worker-one", 1, 'b')],
        );
        manager.projector.project();

        assert_eq!(manager.revoked(&leases), ["model-one"]);
        assert_eq!(manager.published_ids(), ["model-one", "model-other"]);
        let replacement = manager
            .targets()
            .into_iter()
            .find_map(|target| match target {
                ServingTarget::Node(target) if target.deployment_id.0 == "model-one" => {
                    Some(target)
                }
                _ => None,
            })
            .expect("replacement target");
        assert_eq!(replacement.dispatch().node_incarnation, incarnation('2'));
    }

    #[test]
    fn a_paused_deployment_publishes_no_target_before_or_after_a_node_report() {
        let manager = Manager::new(vec![declaration("model-one", "worker-one", 1)]);
        manager.transition("model-one", ModelDeploymentDesiredState::Paused);
        manager.register("worker-one", '1', 1, Some(47_123));

        // A restarted manager with a Connected node and no accepted report publishes nothing.
        manager.projector.project();
        assert!(manager.targets().is_empty(), "no node report");

        manager.report(
            "worker-one",
            '1',
            1,
            vec![ready("model-one", "worker-one", 1, 'b')],
        );
        manager.projector.project();
        assert!(manager.targets().is_empty(), "paused intent");
    }

    #[test]
    fn a_projection_started_before_pause_cannot_republish_the_withdrawn_target() {
        let manager = Manager::new(vec![declaration("model-one", "worker-one", 1)]);
        manager.register("worker-one", '1', 1, Some(47_123));
        manager.report(
            "worker-one",
            '1',
            1,
            vec![ready("model-one", "worker-one", 1, 'b')],
        );
        manager.projector.project();
        assert_eq!(manager.published_ids(), ["model-one"]);
        let captured = Arc::new(Barrier::new(2));
        let paused = Arc::new(Barrier::new(2));

        let projector = manager.projector.clone();
        let coordinator = manager.coordinator.clone();
        let projection = {
            let captured = Arc::clone(&captured);
            let paused = Arc::clone(&paused);
            std::thread::spawn(move || {
                let evidence = coordinator.serving_evidence();
                captured.wait();
                paused.wait();
                projector.publish(evidence);
            })
        };
        captured.wait();
        // The same order as a pause operation: accept site intent, then withdraw the target.
        manager.transition("model-one", ModelDeploymentDesiredState::Paused);
        manager
            .publisher
            .withdraw_deployment(&ModelDeploymentId("model-one".to_owned()));
        assert!(manager.targets().is_empty());
        paused.wait();
        projection.join().expect("projection thread");

        assert!(
            manager.targets().is_empty(),
            "the node sent nothing new, yet the stale evidence cannot republish the target"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn projector_task_follows_status_site_changes_and_expiry_into_route_availability() {
        let manager = Manager::new(vec![
            declaration("model-one", "worker-one", 1),
            declaration("model-two", "worker-one", 1),
        ]);
        manager.route("route-one", "model-one");
        let routes = RouteAvailabilityTargetsSource::new(manager.source.clone());
        let route = manager.site_route("route-one");
        manager.register("worker-one", '1', 1, Some(47_123));
        let _task = manager.projector.clone().start();
        assert!(!routes.availability(&route).available);

        manager.report(
            "worker-one",
            '1',
            1,
            vec![
                ready("model-one", "worker-one", 1, 'b'),
                ready("model-two", "worker-one", 1, 'b'),
            ],
        );
        manager
            .settle(|ids| ids == ["model-one", "model-two"])
            .await;
        assert!(routes.availability(&route).available);

        manager.transition("model-two", ModelDeploymentDesiredState::Paused);
        manager.settle(|ids| ids == ["model-one"]).await;

        tokio::time::sleep(LIVENESS_TTL + Duration::from_millis(1)).await;
        manager.settle(|ids| ids.is_empty()).await;
        assert!(!routes.availability(&route).available);
    }

    #[tokio::test(start_paused = true)]
    async fn advancing_the_clock_wakes_the_projector_to_withdraw_expired_targets() {
        let clock = ManualClock::new();
        let manager = Manager::with_clock(
            vec![declaration("model-one", "worker-one", 1)],
            clock.clone(),
        );
        manager.register("worker-one", '1', 1, Some(47_123));
        let _task = manager.projector.clone().start();
        manager.report(
            "worker-one",
            '1',
            1,
            vec![ready("model-one", "worker-one", 1, 'b')],
        );
        manager.settle(|ids| ids == ["model-one"]).await;
        let start = Instant::now();

        clock.advance(LIVENESS_TTL / 2);
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert_eq!(manager.published_ids(), ["model-one"]);

        clock.advance(LIVENESS_TTL / 2);
        manager.settle(|ids| ids.is_empty()).await;
        assert_eq!(Instant::now(), start, "only the manual clock moved");
    }

    #[tokio::test(start_paused = true)]
    async fn projector_task_stops_projecting_once_its_handle_drops() {
        let manager = Manager::new(vec![
            declaration("model-one", "worker-one", 1),
            declaration("model-two", "worker-one", 1),
        ]);
        manager.register("worker-one", '1', 1, Some(47_123));
        let task = manager.projector.clone().start();
        manager.report(
            "worker-one",
            '1',
            1,
            vec![ready("model-one", "worker-one", 1, 'b')],
        );
        manager.settle(|ids| ids == ["model-one"]).await;

        drop(task);
        manager.report(
            "worker-one",
            '1',
            2,
            vec![
                ready("model-one", "worker-one", 1, 'b'),
                ready("model-two", "worker-one", 1, 'b'),
            ],
        );
        for _ in 0..1_000 {
            tokio::task::yield_now().await;
        }

        assert_eq!(manager.published_ids(), ["model-one"]);
    }

    /// Manager site, node-control coordinator, and serving targets over real files.
    struct Manager {
        _directory: tempfile::TempDir,
        site: SiteRepository,
        coordinator: ManagerNodeControlCoordinator,
        publisher: ServingTargetsPublisher,
        source: ServingTargetsSource,
        projector: NodeTargetProjector,
    }

    impl Manager {
        fn new(deployments: Vec<ModelDeploymentDeclaration>) -> Self {
            Self::with_coordinator(
                deployments,
                ManagerNodeControlCoordinator::on_system_clock(session()),
            )
        }

        fn with_clock(deployments: Vec<ModelDeploymentDeclaration>, clock: ManualClock) -> Self {
            Self::with_coordinator(
                deployments,
                ManagerNodeControlCoordinator::new(
                    session(),
                    &dotcore_domain::timing::Timing::production().node_control,
                    Arc::new(clock),
                ),
            )
        }

        fn with_coordinator(
            deployments: Vec<ModelDeploymentDeclaration>,
            coordinator: ManagerNodeControlCoordinator,
        ) -> Self {
            let directory =
                crate::test_support::canonical_tempdir().expect("manager site directory");
            let site_path = directory.path().join("site.yaml");
            std::fs::write(&site_path, MANAGER_SITE).expect("manager site");
            let site =
                SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
                    .expect("validated manager site");
            site.mutate_model_deployments(|transaction| {
                transaction.deployments.extend(deployments);
                Ok::<_, std::convert::Infallible>(())
            })
            .expect("accepted deployments");
            let (publisher, source) = ServingTargetsPublisher::new();
            let projector = NodeTargetProjector::new(
                site.clone(),
                coordinator.clone(),
                publisher.clone(),
                MANAGER_NODE_ID.to_owned(),
            );
            Self {
                _directory: directory,
                site,
                coordinator,
                publisher,
                source,
                projector,
            }
        }

        fn register(
            &self,
            node_id: &str,
            incarnation_id: char,
            revision: u64,
            private_port: Option<u16>,
        ) {
            self.coordinator
                .register_observation(
                    &NodeObservation {
                        site_id: "site-one".to_owned(),
                        node_id: node_id.to_owned(),
                        incarnation_id: incarnation(incarnation_id).as_str().to_owned(),
                        revision,
                        snapshot: None,
                        observation_configuration_sha256: "0".repeat(64),
                        private_port: private_port.map(u32::from),
                    },
                    AcceptedNodeObservationInput {
                        node_id: node_id.to_owned(),
                        incarnation: incarnation(incarnation_id),
                        revision,
                        snapshot: Default::default(),
                        observation_configuration_sha256:
                            NodeObservationConfigurationDigest::parse("0".repeat(64))
                                .expect("configuration digest"),
                        received_at: Timestamp::default(),
                        private_port,
                    },
                )
                .expect("registered node");
        }

        /// Publishes the node's current view on a new stream of its incarnation.
        fn publish_view(&self, node_id: &str, incarnation_id: char) -> NodeViewReference {
            self.coordinator
                .publish_view_for_test(
                    node_id,
                    &incarnation(incarnation_id),
                    PublishedViewInput {
                        site_id: "site-one".to_owned(),
                        projection: NodeViewProjection::for_site_node(
                            &self.site.snapshot().expect("Site"),
                            node_id,
                        )
                        .expect("node view"),
                    },
                )
                .expect("published view")
        }

        /// Accepts one status on `sequence` against a freshly published view.
        fn report(
            &self,
            node_id: &str,
            incarnation_id: char,
            sequence: u64,
            deployments: Vec<PrivateModelDeploymentStatus>,
        ) {
            let view = self.publish_view(node_id, incarnation_id);
            self.coordinator
                .accept_status(
                    node_id,
                    &incarnation(incarnation_id),
                    &NodeControllerStatus {
                        view: Some(view),
                        status_sequence: sequence,
                        sections: vec![ControllerStatusSection {
                            payload: Some(controller_status_section::Payload::Model(
                                ModelControllerStatusSection {
                                    deployments,
                                    cleanup_evidence: Vec::new(),
                                },
                            )),
                        }],
                    },
                    |_| {
                        Ok::<_, ()>(NodeControllerStatusAcceptance {
                            accepted_status_sequence: sequence,
                            receipts: Vec::new(),
                        })
                    },
                )
                .expect("accepted status");
        }

        fn declare(&self, deployment: ModelDeploymentDeclaration) {
            self.site
                .mutate_model_deployments(|transaction| {
                    transaction.deployments.push(deployment);
                    Ok::<_, std::convert::Infallible>(())
                })
                .expect("accepted deployment");
        }

        fn transition(&self, deployment_id: &str, desired_state: ModelDeploymentDesiredState) {
            self.site
                .mutate_model_deployments(|transaction| {
                    let deployment = transaction
                        .deployments
                        .iter_mut()
                        .find(|deployment| deployment.model_deployment_id == deployment_id)
                        .expect("declared deployment");
                    deployment.desired_state = desired_state as i32;
                    deployment.lifecycle_generation += 1;
                    deployment.lifecycle_transition = match desired_state {
                        ModelDeploymentDesiredState::Paused => {
                            ModelDeploymentLifecycleTransition::Pause
                        }
                        _ => ModelDeploymentLifecycleTransition::Teardown,
                    } as i32;
                    Ok::<_, std::convert::Infallible>(())
                })
                .expect("accepted transition");
        }

        fn route(&self, name: &str, deployment_id: &str) {
            self.site
                .mutate_inference_routes(|transaction| {
                    transaction.routes.push(InferenceRoute {
                        inference_route_id: format!("{name}-id"),
                        name: name.to_owned(),
                        enabled: true,
                        fallback_groups: vec![FallbackGroup {
                            selection: FallbackGroupSelection::Single as i32,
                            model_deployment_ids: vec![deployment_id.to_owned()],
                        }],
                        generation: 1,
                    });
                    Ok::<_, std::convert::Infallible>(())
                })
                .expect("accepted route");
        }

        fn site_route(&self, name: &str) -> InferenceRoute {
            self.site
                .snapshot()
                .expect("Site")
                .inference_routes
                .into_iter()
                .find(|route| route.name == name)
                .expect("declared route")
        }

        fn targets(&self) -> Vec<ServingTarget> {
            self.source.snapshot().targets
        }

        fn published_ids(&self) -> Vec<String> {
            let mut ids = self
                .targets()
                .iter()
                .map(|target| target.deployment_id().0.clone())
                .collect::<Vec<_>>();
            ids.sort();
            ids
        }

        fn lease(
            &self,
            target: &ServingTarget,
        ) -> (
            u64,
            tokio::sync::watch::Receiver<Option<ServingTargetsRevocationReason>>,
        ) {
            self.source
                .lease_for(target)
                .expect("published target lease")
                .into_parts()
        }

        fn leases(
            &self,
        ) -> Vec<(
            String,
            tokio::sync::watch::Receiver<Option<ServingTargetsRevocationReason>>,
        )> {
            self.targets()
                .iter()
                .map(|target| (target.deployment_id().0.clone(), self.lease(target).1))
                .collect()
        }

        fn revoked(
            &self,
            leases: &[(
                String,
                tokio::sync::watch::Receiver<Option<ServingTargetsRevocationReason>>,
            )],
        ) -> Vec<String> {
            let mut revoked = leases
                .iter()
                .filter(|(_, revocation)| revocation.borrow().is_some())
                .map(|(deployment_id, _)| deployment_id.clone())
                .collect::<Vec<_>>();
            revoked.sort();
            revoked
        }

        /// Yields to the projector task until the published deployment IDs satisfy `expected`.
        async fn settle(&self, expected: impl Fn(&[String]) -> bool) {
            for _ in 0..1_000 {
                if expected(&self.published_ids()) {
                    return;
                }
                tokio::task::yield_now().await;
            }
            panic!("projector did not settle: {:?}", self.published_ids());
        }
    }

    fn session() -> ManagerSession {
        ManagerSession::parse("a".repeat(32)).expect("manager session")
    }

    fn incarnation(character: char) -> NodeIncarnation {
        NodeIncarnation::parse(character.to_string().repeat(32)).expect("node incarnation")
    }

    fn declaration(id: &str, node_id: &str, generation: u64) -> ModelDeploymentDeclaration {
        ModelDeploymentDeclaration {
            model_deployment_id: id.to_owned(),
            model_deployment_name: id.to_owned(),
            placement: Some(placement(node_id)),
            requested_parallel_shape: dotcore_proto::dotcore::catalog::v1::ModelParallelShape::One
                as i32,
            snapshot: Some(crate::test_support::local_model_snapshot(id)),
            desired_state: ModelDeploymentDesiredState::Present as i32,
            lifecycle_generation: generation,
            lifecycle_transition: ModelDeploymentLifecycleTransition::Create as i32,
            ..Default::default()
        }
    }

    fn placement(node_id: &str) -> ExactPlacement {
        ExactPlacement {
            site_node_id: node_id.to_owned(),
            execution_target_id: match node_id {
                "worker-two" => "target-two",
                MANAGER_NODE_ID => "manager-target",
                _ => "target-one",
            }
            .to_owned(),
        }
    }

    fn ready(
        id: &str,
        node_id: &str,
        generation: u64,
        token: char,
    ) -> PrivateModelDeploymentStatus {
        PrivateModelDeploymentStatus {
            model_deployment_id: id.to_owned(),
            lifecycle_generation: generation,
            placement: Some(placement(node_id)),
            realization_generation: Some(3),
            lifecycle: ModelLifecycleObservation::Ready as i32,
            status_message: "Ready".to_owned(),
            preparation_progress: None,
            binding_readiness: ModelBindingReadiness::Ready as i32,
            failure: None,
            state_revision: 1,
            observed_placements: Vec::new(),
            binding_token: token.to_string().repeat(64),
            resolved_launch_identities: Vec::new(),
            accelerator_memory: None,
        }
    }

    const MANAGER_NODE_ID: &str = "manager-node";

    const MANAGER_SITE: &str = r#"siteId: site-one
modelDeploymentIdentityAllocator:
  namespace: 0123456789abcdef
  nextSequence: '1'
nodes:
- nodeId: manager-node
  host: 127.0.0.1
  displayName: Manager node
  siteManager: {}
- nodeId: worker-one
  host: 127.0.0.2
  displayName: Worker one
- nodeId: worker-two
  host: worker-two.example.ts.net
  displayName: Worker two
executionTargets:
- executionTargetId: target-one
  siteNodeId: worker-one
  displayName: target-one
  platform: { operatingSystem: linux, architecture: amd64 }
  acceleratorDeviceIds: [GPU-00000000-0000-0000-0000-000000005090]
  modelRuntime:
    simulatedRuntime:
      defaultScenario: { readyDelayMillis: 0, stopDelayMillis: 0 }
- executionTargetId: target-two
  siteNodeId: worker-two
  displayName: target-two
  platform: { operatingSystem: linux, architecture: amd64 }
  acceleratorDeviceIds: [GPU-00000000-0000-0000-0000-000000005090]
  modelRuntime:
    simulatedRuntime:
      defaultScenario: { readyDelayMillis: 0, stopDelayMillis: 0 }
- executionTargetId: manager-target
  siteNodeId: manager-node
  displayName: manager-target
  platform: { operatingSystem: linux, architecture: amd64 }
  acceleratorDeviceIds: [GPU-00000000-0000-0000-0000-000000005090]
  modelRuntime:
    simulatedRuntime:
      defaultScenario: { readyDelayMillis: 0, stopDelayMillis: 0 }
modelDeployments: []
applicationDeployments: []
"#;
}
