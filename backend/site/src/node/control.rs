//! Serializes manager-side node identity, protocol exchange, observation, view publication, and
//! node control stream liveness.
//!
//! One coordinator owns every mutable node-control record. Callers validate payload semantics before
//! entry and expose only read-only observation views outside coordinator custody.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use dotcore_domain::SiteNodeId;
use dotcore_domain::{clock::Clock, timing::NodeControlTiming};
use dotcore_proto::{
    dotcore::{
        application::v1::ApplicationServiceAcceleratorMemory,
        deployment::v1::AcceleratorMemoryObservation,
        model::private::v1::ModelDeploymentStatus,
        site::private::v1::{
            NodeControllerStatus, NodeControllerStatusAcceptance, NodeDesiredView, NodeObservation,
            NodeObservationAcceptance, NodeObservationSnapshot, NodeViewAcknowledgement,
            NodeViewAcknowledgementAcceptance, controller_status_section,
        },
    },
    google::protobuf::Timestamp,
};
use prost::Message;
use sha2::{Digest, Sha256};
use tokio::{
    sync::{Notify, watch},
    time::Instant,
};

use crate::{
    node::observation_configuration::NodeObservationConfigurationDigest,
    view::fencing::{IncarnationFenceError, ManagerSession, NodeIncarnation, NodeViewProjection},
};

const MAX_OFFERED_VIEWS: usize = 32;

/// Manager-side owner of node incarnations, protocol cursors, observations, and published views.
///
/// Lock order is coordinator, then site, then current state. Owner callbacks run under the
/// coordinator lock and may take the site lock and then the current-state lock; no path takes the
/// coordinator while holding either. Node control streams enter it only from blocking commit
/// sections, because status acceptance holds it across site and `state.pb` fsyncs.
#[derive(Clone)]
pub struct ManagerNodeControlCoordinator {
    manager_session: ManagerSession,
    /// Coordinator lock: outermost in the node-control lock order.
    state: Arc<Mutex<CoordinatorState>>,
    /// Node silence ending liveness and serving, and the ages after which statuses and
    /// observations count as stale.
    timing: Arc<NodeControlTiming>,
    /// Source of every contact, status, and serving instant, so they share one timeline.
    clock: Arc<dyn Clock>,
    /// Leaf signal for changed serving evidence, raised only after the coordinator lock drops.
    serving_evidence_changed: Arc<Notify>,
}

impl ManagerNodeControlCoordinator {
    /// Creates a coordinator that reads contact, status and serving time from `clock`, the
    /// clock of the backend it is composed in.
    ///
    /// Liveness ends after the node silence without contact, and a controller status stays fresh
    /// for its maximum age. Every message of the incarnation's open node control stream renews
    /// liveness, so it is independent of the site's observation interval and of how long hardware
    /// collection takes.
    pub fn new(
        manager_session: ManagerSession,
        timing: &NodeControlTiming,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            manager_session,
            state: Arc::new(Mutex::new(CoordinatorState::default())),
            timing: Arc::new(*timing),
            clock,
            serving_evidence_changed: Arc::new(Notify::new()),
        }
    }

    /// Longest silence after which a node incarnation is no longer live; a node control stream
    /// silent this long is ended.
    pub fn node_silence(&self) -> Duration {
        self.timing.node_silence
    }

    /// Age up to which a node's latest infrastructure collection counts as fresh.
    pub fn capability_max_age(&self) -> Duration {
        self.timing.capability_max_age
    }

    pub fn manager_session(&self) -> &ManagerSession {
        &self.manager_session
    }

    /// Returns the clock behind every instant in this coordinator's evidence, which consumers
    /// of that evidence must compare against.
    pub fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.clock)
    }

    /// Returns, for each node's current incarnation, how long ago it last made contact and had a
    /// status accepted.
    #[cfg(any(test, feature = "test-support"))]
    pub fn liveness_ages(&self) -> BTreeMap<String, NodeLivenessAges> {
        let state = self.state.lock().expect("manager node-control lock");
        let now = self.clock.now();
        state
            .nodes
            .iter()
            .filter_map(|(node_id, node)| {
                let incarnation = node.incarnations.get(node.current_incarnation.as_ref()?)?;
                Some((
                    node_id.clone(),
                    NodeLivenessAges {
                        contact: now.saturating_duration_since(incarnation.last_contact?),
                        status: incarnation
                            .accepted_status
                            .as_ref()
                            .map(|status| now.saturating_duration_since(status.received_monotonic)),
                    },
                ))
            })
            .collect()
    }

    /// Returns the signal raised after every accepted observation registration, including an
    /// incarnation replacement, and every accepted status report.
    pub fn serving_evidence_changed(&self) -> Arc<Notify> {
        Arc::clone(&self.serving_evidence_changed)
    }

    /// Copies each node's current serving evidence out of coordinator custody.
    ///
    /// Only the current incarnation of each node is included. The lock is released before this
    /// returns, so callers may then take the site guard without inverting the lock order.
    pub fn serving_evidence(&self) -> Vec<NodeServingEvidence> {
        let state = self.state.lock().expect("manager node-control lock");
        state
            .nodes
            .iter()
            .filter_map(|(node_id, node)| {
                let (incarnation, incarnation_state, observation) = node.current_observation()?;
                Some(NodeServingEvidence {
                    node_id: node_id.clone(),
                    incarnation: incarnation.clone(),
                    private_port: observation.private_port,
                    connected_until: incarnation_state.live_until(self.timing.node_silence)?,
                    status: incarnation_state.accepted_status.as_ref().map(|status| {
                        ServingStatusEvidence {
                            received: status.received_monotonic,
                            manager_session_id: status
                                .request
                                .view
                                .as_ref()
                                .map(|view| view.manager_session_id.clone())
                                .unwrap_or_default(),
                            deployments: status
                                .request
                                .sections
                                .iter()
                                .filter_map(|section| section.payload.as_ref())
                                .filter_map(|payload| match payload {
                                    controller_status_section::Payload::Model(model) => Some(model),
                                    controller_status_section::Payload::Application(_) => None,
                                })
                                .flat_map(|model| model.deployments.iter().cloned())
                                .collect(),
                        }
                    }),
                })
            })
            .collect()
    }

    /// Copies one node's current incarnation, private port, and connection state, leaving its
    /// accepted status inside coordinator custody.
    ///
    /// The lock is released before this returns, so callers may then take the site guard.
    pub fn node_endpoint(&self, node_id: &str) -> Option<NodeEndpointEvidence> {
        let state = self.state.lock().expect("manager node-control lock");
        let (incarnation, incarnation_state, observation) =
            state.nodes.get(node_id)?.current_observation()?;
        Some(NodeEndpointEvidence {
            incarnation: incarnation.clone(),
            private_port: observation.private_port,
            connection_state: self.connection_state(incarnation_state, observation),
        })
    }

    /// Returns an exact completed observation reply before current topology is revalidated.
    pub fn replay_observation(
        &self,
        node_id: &str,
        incarnation: &NodeIncarnation,
        request: &NodeObservation,
    ) -> Result<Option<NodeObservationAcceptance>, IncarnationFenceError> {
        let mut state = self.state.lock().expect("manager node-control lock");
        let Some(node) = state.nodes.get_mut(node_id) else {
            return Ok(None);
        };
        let replayed =
            node.replay_observation(incarnation, request.revision, message_digest(request))?;
        if replayed.is_some() {
            node.renew_contact(incarnation, self.clock.now());
        }
        Ok(replayed)
    }

    /// Registers or refreshes one observation and is the sole incarnation entry/replacement path.
    pub fn register_observation(
        &self,
        request: &NodeObservation,
        input: AcceptedNodeObservationInput,
    ) -> Result<NodeObservationAcceptance, IncarnationFenceError> {
        self.register_new_observation(request, input)
            .map(|(response, _)| response)
    }

    /// Registers like [`Self::register_observation`] and also tells whether the observation was
    /// newly accepted rather than a replay of one already accepted.
    pub(crate) fn register_new_observation(
        &self,
        request: &NodeObservation,
        input: AcceptedNodeObservationInput,
    ) -> Result<(NodeObservationAcceptance, bool), IncarnationFenceError> {
        let registered = self.register_observation_locked(request, input, None)?;
        self.serving_evidence_changed.notify_one();
        Ok(registered)
    }

    /// Registers like [`Self::register_new_observation`] an observation carried by the stream of
    /// `epoch`, refused unless that stream is still its incarnation's open one, so a commit that
    /// outlived its stream never registers, nor revives, an incarnation.
    pub(crate) fn register_stream_observation(
        &self,
        request: &NodeObservation,
        input: AcceptedNodeObservationInput,
        epoch: u64,
    ) -> Result<(NodeObservationAcceptance, bool), IncarnationFenceError> {
        let registered = self.register_observation_locked(request, input, Some(epoch))?;
        self.serving_evidence_changed.notify_one();
        Ok(registered)
    }

    fn register_observation_locked(
        &self,
        request: &NodeObservation,
        input: AcceptedNodeObservationInput,
        stream_epoch: Option<u64>,
    ) -> Result<(NodeObservationAcceptance, bool), IncarnationFenceError> {
        let digest = message_digest(request);
        let mut state = self.state.lock().expect("manager node-control lock");
        // Stamped under the lock, so the restart fence below judges liveness on this clock.
        let now = self.clock.now();
        let node = state.nodes.entry(input.node_id.clone()).or_default();
        if let Some(epoch) = stream_epoch
            && !node.stream_open(&input.incarnation, epoch)
        {
            return Err(IncarnationFenceError::StreamReplaced);
        }

        if let Some(replayed) =
            node.replay_observation(&input.incarnation, input.revision, digest)?
        {
            node.renew_contact(&input.incarnation, now);
            return Ok((replayed, false));
        }

        match node.current_incarnation.as_ref() {
            Some(current) if current != &input.incarnation => {
                // Only a silent incarnation may be replaced, so a live process is never fenced
                // out by another one claiming the same SiteNode.
                let replaceable = node
                    .incarnations
                    .get(current)
                    .is_none_or(|state| !state.connected_at(now, self.timing.node_silence));
                if !replaceable {
                    return Err(IncarnationFenceError::CurrentIncarnationActive);
                }
                if let Some(current) = node.current_incarnation.take() {
                    node.retire_incarnation(current);
                }
                node.current_incarnation = Some(input.incarnation.clone());
            }
            None => node.current_incarnation = Some(input.incarnation.clone()),
            Some(_) => {}
        }

        if node.replaced_incarnations.contains(&input.incarnation) {
            return Err(IncarnationFenceError::ReplacedIncarnation);
        }
        let incarnation = node
            .incarnations
            .entry(input.incarnation.clone())
            .or_default();
        ensure_new_sequence(&incarnation.observation_exchange, input.revision, digest)?;

        let response = NodeObservationAcceptance {
            accepted_revision: input.revision,
            received_at: Some(input.received_at),
        };
        incarnation.observation = Some(AcceptedNodeObservation {
            revision: input.revision,
            snapshot: input.snapshot,
            observation_configuration_sha256: input.observation_configuration_sha256,
            received_at: response
                .received_at
                .expect("observation response timestamp"),
            received_monotonic: now,
            private_port: input.private_port,
        });
        incarnation.observation_exchange =
            Some(CompletedExchange::new(input.revision, digest, response));
        // A commit finishing after its stream closed records the observation but is no contact.
        if incarnation.accepts_contact() {
            incarnation.contact(now);
        }
        Ok((response, true))
    }

    /// Accepts an acknowledgement only for an offered view and records its exact reply.
    pub(crate) fn acknowledge(
        &self,
        node_id: &str,
        incarnation: &NodeIncarnation,
        request: &NodeViewAcknowledgement,
        offered_view: (u64, &str),
    ) -> Result<NodeViewAcknowledgementAcceptance, IncarnationFenceError> {
        let digest = message_digest(request);
        let mut state = self.state.lock().expect("manager node-control lock");
        let node = current_node_mut(
            &mut state,
            node_id,
            incarnation,
            Channel::Acknowledgement,
            request.acknowledgement_sequence,
            digest,
        )?;
        if let Some(reply) = node
            .incarnations
            .get(incarnation)
            .and_then(|state| state.acknowledgement_exchange.as_ref())
            .and_then(|exchange| exchange.replay(request.acknowledgement_sequence, digest))
        {
            node.renew_contact(incarnation, self.clock.now());
            return Ok(reply);
        }
        if !node
            .offered_views
            .get(incarnation)
            .is_some_and(|offered| offered.contains(&(offered_view.0, offered_view.1.to_owned())))
        {
            return Err(IncarnationFenceError::UnofferedView);
        }
        node.renew_contact(incarnation, self.clock.now());
        let response = NodeViewAcknowledgementAcceptance {
            accepted_acknowledgement_sequence: request.acknowledgement_sequence,
        };
        node.incarnations
            .get_mut(incarnation)
            .expect("registered incarnation")
            .acknowledgement_exchange = Some(CompletedExchange::new(
            request.acknowledgement_sequence,
            digest,
            response,
        ));
        Ok(response)
    }

    /// Opens a node control stream of `incarnation` as the node's latest stream. An open stream of
    /// the same incarnation is replaced without any disconnection, and the returned receiver tells
    /// each stream when a newer one replaced it. Opening is contact.
    ///
    /// Refused for a replaced incarnation, and for a new incarnation while the current one is
    /// still connected. A new incarnation becomes current only when its observation registers.
    pub(crate) fn open_stream(
        &self,
        node_id: &str,
        incarnation: &NodeIncarnation,
    ) -> Result<NodeStreamAttachment, IncarnationFenceError> {
        let mut state = self.state.lock().expect("manager node-control lock");
        let now = self.clock.now();
        let node = state.nodes.entry(node_id.to_owned()).or_default();
        if node.replaced_incarnations.contains(incarnation) {
            return Err(IncarnationFenceError::ReplacedIncarnation);
        }
        if let Some(current) = node.current_incarnation.as_ref()
            && current != incarnation
            && node
                .incarnations
                .get(current)
                .is_some_and(|state| state.connected_at(now, self.timing.node_silence))
        {
            return Err(IncarnationFenceError::CurrentIncarnationActive);
        }
        node.forget_unregistered(incarnation);
        let epoch = node
            .stream_epoch
            .borrow()
            .checked_add(1)
            .expect("node control stream epoch exhausted");
        node.stream_epoch.send_replace(epoch);
        let replaced = node.stream_epoch.subscribe();
        node.incarnations
            .entry(incarnation.clone())
            .or_default()
            .stream = StreamPresence::Open { epoch };
        node.renew_contact(incarnation, now);
        Ok(NodeStreamAttachment { epoch, replaced })
    }

    /// Records contact from the stream of `epoch` while it is the node's latest open stream; a
    /// replaced or closed stream never renews liveness.
    pub(crate) fn stream_contact(&self, node_id: &str, incarnation: &NodeIncarnation, epoch: u64) {
        let mut state = self.state.lock().expect("manager node-control lock");
        let Some(node) = state.nodes.get_mut(node_id) else {
            return;
        };
        if node
            .incarnations
            .get(incarnation)
            .is_some_and(|state| state.stream == StreamPresence::Open { epoch })
        {
            node.renew_contact(incarnation, self.clock.now());
        }
    }

    /// Records that the stream of `epoch` closed, which disconnects its incarnation at once when
    /// it is still that incarnation's latest stream. Serving evidence keeps the last contact.
    pub(crate) fn close_stream(
        &self,
        node_id: &str,
        incarnation: &NodeIncarnation,
        epoch: u64,
    ) -> bool {
        let mut state = self.state.lock().expect("manager node-control lock");
        let Some(state) = state
            .nodes
            .get_mut(node_id)
            .and_then(|node| node.incarnations.get_mut(incarnation))
            .filter(|state| state.stream == StreamPresence::Open { epoch })
        else {
            return false;
        };
        state.stream = StreamPresence::Closed;
        true
    }

    /// Publishes the node's current projection on the open stream of `epoch` as one complete
    /// view, identified by the stream's `publication_sequence`, and offers it for acknowledgement.
    pub(crate) fn publish_stream_view<F, E>(
        &self,
        node_id: &str,
        incarnation: &NodeIncarnation,
        epoch: u64,
        publication_sequence: u64,
        publish: F,
    ) -> Result<NodeDesiredView, CoordinatorExchangeError<E>>
    where
        F: FnOnce() -> Result<PublishedViewInput, E>,
    {
        let mut state = self.state.lock().expect("manager node-control lock");
        let node = state
            .nodes
            .get_mut(node_id)
            .filter(|node| node.stream_open(incarnation, epoch))
            .ok_or(CoordinatorExchangeError::Fence(
                IncarnationFenceError::StreamReplaced,
            ))?;
        let input = publish().map_err(CoordinatorExchangeError::Owner)?;
        node.publish(input);
        Ok(node
            .offer_published(
                &self.manager_session,
                node_id,
                incarnation,
                publication_sequence,
            )
            .expect("published node view"))
    }

    /// Publishes the node's last published view again, without projecting the site, on the open
    /// stream of `epoch`; `None` when nothing was ever published for the node.
    pub(crate) fn publish_last_stream_view(
        &self,
        node_id: &str,
        incarnation: &NodeIncarnation,
        epoch: u64,
        publication_sequence: u64,
    ) -> Result<Option<NodeDesiredView>, IncarnationFenceError> {
        let mut state = self.state.lock().expect("manager node-control lock");
        let node = state
            .nodes
            .get_mut(node_id)
            .filter(|node| node.stream_open(incarnation, epoch))
            .ok_or(IncarnationFenceError::StreamReplaced)?;
        Ok(node.offer_published(
            &self.manager_session,
            node_id,
            incarnation,
            publication_sequence,
        ))
    }

    /// Commits a status cursor only after the synchronous owner callback accepts the report.
    pub fn accept_status<E, F>(
        &self,
        node_id: &str,
        incarnation: &NodeIncarnation,
        request: &NodeControllerStatus,
        accept: F,
    ) -> Result<NodeControllerStatusAcceptance, CoordinatorExchangeError<E>>
    where
        F: FnOnce(&NodeViewProjection) -> Result<NodeControllerStatusAcceptance, E>,
    {
        let response = self.accept_status_locked(node_id, incarnation, request, accept)?;
        self.serving_evidence_changed.notify_one();
        Ok(response)
    }

    fn accept_status_locked<E, F>(
        &self,
        node_id: &str,
        incarnation: &NodeIncarnation,
        request: &NodeControllerStatus,
        accept: F,
    ) -> Result<NodeControllerStatusAcceptance, CoordinatorExchangeError<E>>
    where
        F: FnOnce(&NodeViewProjection) -> Result<NodeControllerStatusAcceptance, E>,
    {
        let digest = message_digest(request);
        let mut state = self.state.lock().expect("manager node-control lock");
        let node = current_node_mut(
            &mut state,
            node_id,
            incarnation,
            Channel::Status,
            request.status_sequence,
            digest,
        )
        .map_err(CoordinatorExchangeError::Fence)?;
        if let Some(reply) = node
            .incarnations
            .get(incarnation)
            .and_then(|state| state.status_exchange.as_ref())
            .and_then(|exchange| exchange.replay(request.status_sequence, digest))
        {
            node.renew_contact(incarnation, self.clock.now());
            return Ok(reply);
        }
        let reference = request
            .view
            .as_ref()
            .ok_or(CoordinatorExchangeError::Fence(
                IncarnationFenceError::UnofferedView,
            ))?;
        let published = node
            .published
            .as_ref()
            .filter(|published| {
                published.revision == reference.view_revision
                    && published.projection.digest().as_str() == reference.content_sha256
            })
            .ok_or(CoordinatorExchangeError::Fence(
                IncarnationFenceError::UnofferedView,
            ))?;
        let projection = published.projection.clone();
        // The report passed every fence, so it is contact even when its owner rejects it.
        node.renew_contact(incarnation, self.clock.now());
        let response = accept(&projection).map_err(CoordinatorExchangeError::Owner)?;
        let incarnation_state = node
            .incarnations
            .get_mut(incarnation)
            .expect("registered incarnation");
        incarnation_state.accepted_status = Some(AcceptedControllerStatus {
            request: request.clone(),
            received_monotonic: self.clock.now(),
        });
        incarnation_state.status_exchange = Some(CompletedExchange::new(
            request.status_sequence,
            digest,
            response.clone(),
        ));
        Ok(response)
    }

    /// Classifies the public status evidence of node-hosted model deployments in one
    /// coordinator pass, returning one result per requested deployment in request order, with the
    /// accelerator memory a Current status reports.
    ///
    /// The lock is released before this returns. Callers pass owned snapshots and hold neither
    /// the site nor the current-state guard.
    pub fn model_status_evidence(
        &self,
        deployments: &[NodeDeploymentReference<'_>],
    ) -> Vec<ModelStatusEvidence> {
        self.status_evidence(deployments, |payload, deployment| match payload {
            controller_status_section::Payload::Model(model) => model
                .deployments
                .iter()
                .find(|reported| {
                    reported.model_deployment_id == deployment.deployment_id
                        && reported.lifecycle_generation == deployment.lifecycle_generation
                })
                .map(|reported| reported.accelerator_memory.clone()),
            controller_status_section::Payload::Application(_) => None,
        })
        .into_iter()
        .map(|(evidence, accelerator_memory)| ModelStatusEvidence {
            evidence,
            accelerator_memory: accelerator_memory.flatten(),
        })
        .collect()
    }

    /// Classifies the public status evidence of node-hosted application deployments, like
    /// [`Self::model_status_evidence`] for the application section of each status, with the
    /// per-unit accelerator memory a Current status reports.
    pub fn application_status_evidence(
        &self,
        deployments: &[NodeDeploymentReference<'_>],
    ) -> Vec<ApplicationStatusEvidence> {
        self.status_evidence(deployments, |payload, deployment| match payload {
            controller_status_section::Payload::Application(application) => application
                .deployments
                .iter()
                .find(|reported| {
                    reported.application_deployment_id == deployment.deployment_id
                        && reported.lifecycle_generation == deployment.lifecycle_generation
                })
                .map(|reported| reported.service_accelerator_memory.clone()),
            controller_status_section::Payload::Model(_) => None,
        })
        .into_iter()
        .map(|(evidence, accelerator_memory)| ApplicationStatusEvidence {
            evidence,
            accelerator_memory: accelerator_memory.unwrap_or_default(),
        })
        .collect()
    }

    /// Classifies each deployment against its node's public status; `listed` returns what a
    /// status section reports for the deployment at its declared generation, if it lists it.
    fn status_evidence<T>(
        &self,
        deployments: &[NodeDeploymentReference<'_>],
        listed: impl Fn(&controller_status_section::Payload, &NodeDeploymentReference<'_>) -> Option<T>,
    ) -> Vec<(DeploymentStatusEvidence, Option<T>)> {
        let now = self.clock.now();
        let state = self.state.lock().expect("manager node-control lock");
        deployments
            .iter()
            .map(|deployment| {
                let Some(status) = state.nodes.get(deployment.node_id).and_then(|node| {
                    node.public_status(
                        now,
                        self.timing.controller_status_max_age,
                        self.timing.node_silence,
                    )
                }) else {
                    return (DeploymentStatusEvidence::Unavailable, None);
                };
                // The status's view revision is not compared: a status accepted against an
                // older view stays evidence for every deployment it lists at the declared
                // generation, so an unrelated view change on the node changes nothing here.
                let reported = status.request.sections.iter().find_map(|section| {
                    section
                        .payload
                        .as_ref()
                        .and_then(|payload| listed(payload, deployment))
                });
                match reported {
                    Some(reported) => (DeploymentStatusEvidence::Current, Some(reported)),
                    None => (DeploymentStatusEvidence::AcknowledgementPending, None),
                }
            })
            .collect()
    }

    pub fn observation(&self, node_id: &SiteNodeId) -> Option<CoordinatorObservationView> {
        let state = self.state.lock().expect("manager node-control lock");
        let node = state.nodes.get(node_id.as_str())?;
        let (incarnation, incarnation_state, observation) = node.current_observation()?;
        Some(CoordinatorObservationView {
            incarnation: incarnation.clone(),
            revision: observation.revision,
            snapshot: observation.snapshot.clone(),
            observation_configuration_sha256: observation.observation_configuration_sha256.clone(),
            received_at: observation.received_at,
            connection_state: self.connection_state(incarnation_state, observation),
            replaced_incarnations: node.replaced_incarnations.clone(),
        })
    }

    /// Classifies one incarnation's connection now, on this coordinator's silence bound.
    fn connection_state(
        &self,
        incarnation: &IncarnationState,
        observation: &AcceptedNodeObservation,
    ) -> CoordinatorConnectionState {
        connection_state(incarnation, observation, self.clock.now(), &self.timing)
    }

    /// Opens a node control stream of `incarnation` for `node_id`, publishes `input` as its first
    /// view, and returns the reference a status or acknowledgement of that view names.
    #[cfg(any(test, feature = "test-support"))]
    pub fn publish_view_for_test(
        &self,
        node_id: &str,
        incarnation: &NodeIncarnation,
        input: PublishedViewInput,
    ) -> Result<dotcore_proto::dotcore::site::private::v1::NodeViewReference, IncarnationFenceError>
    {
        let attachment = self.open_stream(node_id, incarnation)?;
        let view = self
            .publish_stream_view(node_id, incarnation, attachment.epoch, 1, || {
                Ok::<_, std::convert::Infallible>(input)
            })
            .map_err(|error| match error {
                CoordinatorExchangeError::Fence(error) => error,
                CoordinatorExchangeError::Owner(never) => match never {},
            })?;
        let identity = view.identity.expect("published view identity");
        Ok(
            dotcore_proto::dotcore::site::private::v1::NodeViewReference {
                site_id: identity.site_id,
                node_id: identity.node_id,
                manager_session_id: identity.manager_session_id,
                node_incarnation_id: identity.node_incarnation_id,
                view_revision: identity.view_revision,
                content_sha256: identity.content_sha256,
            },
        )
    }

    /// Creates a coordinator on system time and the production timing, for unit tests that do
    /// not control time.
    #[cfg(any(test, feature = "test-support"))]
    pub fn on_system_clock(manager_session: ManagerSession) -> Self {
        Self::new(
            manager_session,
            &dotcore_domain::timing::Timing::production().node_control,
            Arc::new(dotcore_domain::clock::SystemClock),
        )
    }
}

pub struct AcceptedNodeObservationInput {
    pub node_id: String,
    pub incarnation: NodeIncarnation,
    pub revision: u64,
    pub snapshot: NodeObservationSnapshot,
    pub observation_configuration_sha256: NodeObservationConfigurationDigest,
    pub received_at: Timestamp,
    /// Validated private listener port, absent when the node reported none.
    pub private_port: Option<u16>,
}

/// A node control stream's place among its node's streams: its epoch, and a receiver of the
/// node's latest epoch that changes once a newer stream replaces it.
pub(crate) struct NodeStreamAttachment {
    pub epoch: u64,
    pub replaced: watch::Receiver<u64>,
}

/// How long ago one node's current incarnation made contact and, if ever, had a status accepted.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeLivenessAges {
    pub contact: Duration,
    pub status: Option<Duration>,
}

/// One node-hosted deployment as site declares it, for a public status evidence query.
pub struct NodeDeploymentReference<'a> {
    pub node_id: &'a str,
    pub deployment_id: &'a str,
    pub lifecycle_generation: u64,
}

/// Status evidence of one node-hosted model deployment and the accelerator memory its hosting
/// node reports; memory is present only with Current evidence.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelStatusEvidence {
    pub evidence: DeploymentStatusEvidence,
    pub accelerator_memory: Option<AcceleratorMemoryObservation>,
}

/// Status evidence of one node-hosted application deployment and the per-unit accelerator
/// memory its hosting node reports; memory is listed only with Current evidence.
#[derive(Clone, Debug, PartialEq)]
pub struct ApplicationStatusEvidence {
    pub evidence: DeploymentStatusEvidence,
    pub accelerator_memory: Vec<ApplicationServiceAcceleratorMemory>,
}

/// What the node's latest accepted status says about one declared deployment generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeploymentStatusEvidence {
    /// A fresh status from the live current incarnation lists the declared generation.
    Current,

    /// The node is live with a fresh status that does not list the declared generation
    /// yet. Once a newer view is published, older-view statuses are no longer accepted, so this
    /// state ends when the status freshness window passes without an acknowledgement.
    AcknowledgementPending,

    /// The node is unknown, silent past the liveness window, replaced, or has no fresh
    /// accepted status.
    Unavailable,
}

/// One node's current-incarnation serving evidence, copied out of coordinator custody.
pub struct NodeServingEvidence {
    pub node_id: String,
    pub incarnation: NodeIncarnation,
    pub private_port: Option<u16>,
    /// Instant after which the node is no longer live without another contact.
    pub connected_until: Instant,
    pub status: Option<ServingStatusEvidence>,
}

/// The latest accepted model status of one incarnation. It stays evidence after a newer view is
/// published, because its view revision is never part of serving identity.
pub struct ServingStatusEvidence {
    pub received: Instant,
    pub manager_session_id: String,
    pub deployments: Vec<ModelDeploymentStatus>,
}

/// One node's current incarnation and how to reach it, copied without its accepted status.
pub struct NodeEndpointEvidence {
    pub incarnation: NodeIncarnation,
    /// Private listener port the incarnation reported, absent when it reported none.
    pub private_port: Option<u16>,
    pub connection_state: CoordinatorConnectionState,
}

pub struct PublishedViewInput {
    pub site_id: String,
    pub projection: NodeViewProjection,
}

pub struct CoordinatorObservationView {
    pub incarnation: NodeIncarnation,
    pub revision: u64,
    pub snapshot: NodeObservationSnapshot,
    pub observation_configuration_sha256: NodeObservationConfigurationDigest,
    pub received_at: Timestamp,
    pub connection_state: CoordinatorConnectionState,
    pub replaced_incarnations: BTreeSet<NodeIncarnation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoordinatorConnectionState {
    Connected,
    Stale,
    Disconnected,
}

#[derive(Debug)]
pub enum CoordinatorExchangeError<E> {
    Fence(IncarnationFenceError),
    Owner(E),
}

#[derive(Default)]
struct CoordinatorState {
    nodes: BTreeMap<String, CoordinatorNode>,
}

#[derive(Default)]
struct CoordinatorNode {
    current_incarnation: Option<NodeIncarnation>,
    replaced_incarnations: BTreeSet<NodeIncarnation>,
    incarnations: BTreeMap<NodeIncarnation, IncarnationState>,
    published: Option<PublishedNodeView>,
    offered_views: BTreeMap<NodeIncarnation, BTreeSet<(u64, String)>>,
    /// Epoch of the node's latest control stream, whichever incarnation opened it; an open stream
    /// with an older epoch has been replaced.
    stream_epoch: watch::Sender<u64>,
}

impl CoordinatorNode {
    /// Returns the current incarnation with its state and accepted observation, if any.
    fn current_observation(
        &self,
    ) -> Option<(
        &NodeIncarnation,
        &IncarnationState,
        &AcceptedNodeObservation,
    )> {
        let incarnation = self.current_incarnation.as_ref()?;
        let state = self.incarnations.get(incarnation)?;
        Some((incarnation, state, state.observation.as_ref()?))
    }

    fn replay_observation(
        &self,
        incarnation: &NodeIncarnation,
        sequence: u64,
        digest: [u8; 32],
    ) -> Result<Option<NodeObservationAcceptance>, IncarnationFenceError> {
        let Some(state) = self.incarnations.get(incarnation) else {
            return Ok(None);
        };
        match state.observation_exchange.as_ref() {
            Some(exchange) if sequence == exchange.sequence && digest == exchange.digest => {
                exchange
                    .replay(sequence, digest)
                    .map(Some)
                    .ok_or(IncarnationFenceError::ReplacedIncarnation)
            }
            Some(exchange) if sequence <= exchange.sequence => Err(sequence_error(
                exchange.sequence,
                sequence,
                digest == exchange.digest,
            )),
            _ if self.replaced_incarnations.contains(incarnation) => {
                Err(IncarnationFenceError::ReplacedIncarnation)
            }
            _ => Ok(None),
        }
    }

    /// Returns the current incarnation's accepted status while it is public evidence: the
    /// node is live and the status is within `freshness`. Observation age plays no part.
    fn public_status(
        &self,
        now: Instant,
        freshness: Duration,
        silence_bound: Duration,
    ) -> Option<&AcceptedControllerStatus> {
        let incarnation = self.incarnations.get(self.current_incarnation.as_ref()?)?;
        incarnation.accepted_status.as_ref().filter(|status| {
            incarnation.live_at(now, silence_bound)
                && now.saturating_duration_since(status.received_monotonic) <= freshness
        })
    }

    /// Records authenticated contact from `incarnation` when it is the current one; a replaced
    /// or unknown incarnation, or one whose stream closed, never renews liveness.
    fn renew_contact(&mut self, incarnation: &NodeIncarnation, now: Instant) {
        if self.current_incarnation.as_ref() != Some(incarnation)
            || self.replaced_incarnations.contains(incarnation)
        {
            return;
        }
        if let Some(state) = self.incarnations.get_mut(incarnation)
            && state.accepts_contact()
        {
            state.contact(now);
        }
    }

    /// Records one view offered to `incarnation`, keeping only its most recent offers.
    fn offer(&mut self, incarnation: &NodeIncarnation, view: (u64, String)) {
        let offered = self.offered_views.entry(incarnation.clone()).or_default();
        offered.insert(view);
        while offered.len() > MAX_OFFERED_VIEWS {
            let oldest = offered.iter().next().cloned().expect("offered view exists");
            offered.remove(&oldest);
        }
    }

    /// Drops the records of incarnations that opened a stream but never registered, other than
    /// `kept`, so restarts that never report retain nothing.
    fn forget_unregistered(&mut self, kept: &NodeIncarnation) {
        let unregistered = self
            .incarnations
            .iter()
            .filter(|(incarnation, state)| {
                *incarnation != kept
                    && self.current_incarnation.as_ref() != Some(*incarnation)
                    && !self.replaced_incarnations.contains(*incarnation)
                    && state.observation_exchange.is_none()
            })
            .map(|(incarnation, _)| incarnation.clone())
            .collect::<Vec<_>>();
        for incarnation in unregistered {
            self.incarnations.remove(&incarnation);
            self.offered_views.remove(&incarnation);
        }
    }

    /// Fences a replaced incarnation. Its channel sequences and request digests stay as fencing
    /// identities, while its observation, accepted status, offered views, and completed reply
    /// payloads are dropped, so repeated node restarts retain no per-incarnation payloads.
    fn retire_incarnation(&mut self, incarnation: NodeIncarnation) {
        if let Some(state) = self.incarnations.get_mut(&incarnation) {
            state.retire();
        }
        self.offered_views.remove(&incarnation);
        self.replaced_incarnations.insert(incarnation);
    }

    fn publish(&mut self, input: PublishedViewInput) {
        match self.published.as_mut() {
            Some(current) if current.projection.digest() == input.projection.digest() => {}
            Some(current) => {
                current.revision = current
                    .revision
                    .checked_add(1)
                    .expect("node view revision exhausted");
                current.projection = input.projection;
                current.site_id = input.site_id;
            }
            None => {
                self.published = Some(PublishedNodeView {
                    revision: 1,
                    site_id: input.site_id,
                    projection: input.projection,
                })
            }
        }
    }

    /// Builds the last published view as publication `publication_sequence` for `incarnation`
    /// and offers it for acknowledgement; `None` when nothing was published.
    fn offer_published(
        &mut self,
        manager_session: &ManagerSession,
        node_id: &str,
        incarnation: &NodeIncarnation,
        publication_sequence: u64,
    ) -> Option<NodeDesiredView> {
        let published = self.published.as_ref()?;
        let view = crate::view::desired::node_desired_view(
            manager_session,
            incarnation,
            publication_sequence,
            published.revision,
            &published.projection,
            &published.site_id,
            node_id,
        );
        let offered = (
            published.revision,
            published.projection.digest().as_str().to_owned(),
        );
        self.offer(incarnation, offered);
        Some(view)
    }

    /// Whether `incarnation`'s latest stream is the open one of `epoch`.
    fn stream_open(&self, incarnation: &NodeIncarnation, epoch: u64) -> bool {
        self.incarnations
            .get(incarnation)
            .is_some_and(|state| state.stream == StreamPresence::Open { epoch })
    }
}

#[derive(Default)]
struct IncarnationState {
    observation: Option<AcceptedNodeObservation>,
    observation_exchange: Option<CompletedExchange<NodeObservationAcceptance>>,
    acknowledgement_exchange: Option<CompletedExchange<NodeViewAcknowledgementAcceptance>>,
    status_exchange: Option<CompletedExchange<NodeControllerStatusAcceptance>>,
    accepted_status: Option<AcceptedControllerStatus>,
    /// Manager monotonic time of the latest authenticated, fenced contact.
    last_contact: Option<Instant>,
    stream: StreamPresence,
}

impl IncarnationState {
    fn contact(&mut self, now: Instant) {
        self.last_contact = Some(now);
    }

    /// Whether this incarnation was in contact within `silence_bound` of `now`.
    fn live_at(&self, now: Instant, silence_bound: Duration) -> bool {
        self.live_until(silence_bound)
            .is_some_and(|until| now <= until)
    }

    fn live_until(&self, silence_bound: Duration) -> Option<Instant> {
        self.last_contact.map(|contact| contact + silence_bound)
    }

    /// Whether this incarnation is connected: live, and its control stream, if it ever opened
    /// one, not closed. Serving evidence follows liveness alone.
    fn connected_at(&self, now: Instant, silence_bound: Duration) -> bool {
        self.live_at(now, silence_bound) && self.stream != StreamPresence::Closed
    }

    /// Whether an exchange of this incarnation still renews its liveness: never after its stream
    /// closed, so a commit finishing late cannot revive it.
    fn accepts_contact(&self) -> bool {
        self.stream != StreamPresence::Closed
    }

    fn retire(&mut self) {
        self.observation = None;
        self.accepted_status = None;
        self.last_contact = None;
        if let Some(exchange) = self.observation_exchange.as_mut() {
            exchange.retire();
        }
        if let Some(exchange) = self.acknowledgement_exchange.as_mut() {
            exchange.retire();
        }
        if let Some(exchange) = self.status_exchange.as_mut() {
            exchange.retire();
        }
    }
}

/// Where an incarnation's node control stream stands. An incarnation registered before its first
/// stream opened, as the manager's own node is at startup, is live on that registration alone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum StreamPresence {
    #[default]
    Never,

    /// The node's latest stream, of this epoch, is open.
    Open { epoch: u64 },

    /// The incarnation's latest stream closed and no newer one of it opened.
    Closed,
}

struct AcceptedControllerStatus {
    request: NodeControllerStatus,
    received_monotonic: Instant,
}

struct AcceptedNodeObservation {
    revision: u64,
    snapshot: NodeObservationSnapshot,
    observation_configuration_sha256: NodeObservationConfigurationDigest,
    received_at: Timestamp,
    received_monotonic: Instant,
    private_port: Option<u16>,
}

/// Classifies one incarnation: Disconnected once its stream closed or without contact within the
/// node silence, otherwise Connected while its latest observation is within the capability age
/// and Stale once that observation is overdue.
fn connection_state(
    incarnation: &IncarnationState,
    observation: &AcceptedNodeObservation,
    now: Instant,
    timing: &NodeControlTiming,
) -> CoordinatorConnectionState {
    if !incarnation.connected_at(now, timing.node_silence) {
        CoordinatorConnectionState::Disconnected
    } else if now.saturating_duration_since(observation.received_monotonic)
        <= timing.capability_max_age
    {
        CoordinatorConnectionState::Connected
    } else {
        CoordinatorConnectionState::Stale
    }
}

struct PublishedNodeView {
    revision: u64,
    site_id: String,
    projection: NodeViewProjection,
}

/// Last completed exchange on one channel: its sequence and request digest fence later
/// requests, and its reply answers an exact retry until the incarnation is retired.
struct CompletedExchange<R> {
    sequence: u64,
    digest: [u8; 32],
    reply: Option<R>,
}

impl<R: Clone> CompletedExchange<R> {
    fn new(sequence: u64, digest: [u8; 32], reply: R) -> Self {
        Self {
            sequence,
            digest,
            reply: Some(reply),
        }
    }

    fn replay(&self, sequence: u64, digest: [u8; 32]) -> Option<R> {
        (self.sequence == sequence && self.digest == digest)
            .then(|| self.reply.clone())
            .flatten()
    }

    fn retire(&mut self) {
        self.reply = None;
    }

    #[cfg(test)]
    fn retained(&self) -> bool {
        self.reply.is_some()
    }
}

#[derive(Clone, Copy)]
enum Channel {
    Acknowledgement,
    Status,
}

fn current_node_mut<'a>(
    state: &'a mut CoordinatorState,
    node_id: &str,
    incarnation: &NodeIncarnation,
    channel: Channel,
    sequence: u64,
    digest: [u8; 32],
) -> Result<&'a mut CoordinatorNode, IncarnationFenceError> {
    let node = state
        .nodes
        .get_mut(node_id)
        .ok_or(IncarnationFenceError::UnregisteredIncarnation)?;
    let incarnation_state = node
        .incarnations
        .get(incarnation)
        .ok_or(IncarnationFenceError::UnregisteredIncarnation)?;
    let exchange_sequence = match channel {
        Channel::Acknowledgement => incarnation_state
            .acknowledgement_exchange
            .as_ref()
            .map(|record| (record.sequence, record.digest)),
        Channel::Status => incarnation_state
            .status_exchange
            .as_ref()
            .map(|record| (record.sequence, record.digest)),
    };
    if let Some((accepted, accepted_digest)) = exchange_sequence
        && sequence <= accepted
    {
        if sequence == accepted && digest == accepted_digest {
            // A replaced incarnation keeps no reply to return for its exact retry.
            if node.replaced_incarnations.contains(incarnation) {
                return Err(IncarnationFenceError::ReplacedIncarnation);
            }
            return Ok(node);
        }
        return Err(sequence_error(accepted, sequence, false));
    }
    if node.replaced_incarnations.contains(incarnation) {
        return Err(IncarnationFenceError::ReplacedIncarnation);
    }
    if node.current_incarnation.as_ref() != Some(incarnation) {
        return Err(IncarnationFenceError::CurrentIncarnationActive);
    }
    if sequence == 0 {
        return Err(IncarnationFenceError::InvalidSequence);
    }
    Ok(node)
}

fn ensure_new_sequence<R>(
    exchange: &Option<CompletedExchange<R>>,
    sequence: u64,
    digest: [u8; 32],
) -> Result<(), IncarnationFenceError> {
    if sequence == 0 {
        return Err(IncarnationFenceError::InvalidSequence);
    }
    if let Some(exchange) = exchange
        && sequence <= exchange.sequence
    {
        return Err(sequence_error(
            exchange.sequence,
            sequence,
            digest == exchange.digest,
        ));
    }
    Ok(())
}

fn sequence_error(accepted: u64, received: u64, same_digest: bool) -> IncarnationFenceError {
    if received == accepted && !same_digest {
        IncarnationFenceError::ConflictingSequence
    } else {
        IncarnationFenceError::StaleSequence { accepted, received }
    }
}

fn message_digest(message: &impl Message) -> [u8; 32] {
    Sha256::digest(message.encode_to_vec()).into()
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Barrier, mpsc},
        time::Duration,
    };

    use dotcore_domain::{clock::ManualClock, timing::Timing};
    use dotcore_proto::{
        dotcore::{
            application::private::v1::{
                ApplicationControllerStatusSection, ApplicationDeploymentStatus,
            },
            model::private::v1::{
                ModelBindingReadiness, ModelControllerStatusSection, ModelDeploymentStatus,
                ModelLifecycleObservation,
            },
            site::{
                private::v1::{
                    ControllerStatusSection, NodeControllerStatus, NodeControllerStatusAcceptance,
                    NodeObservation, NodeObservationSnapshot, NodeViewAcknowledgement,
                    NodeViewReference, controller_status_section, node_view_acknowledgement,
                },
                v1::ExecutionTarget,
            },
        },
        google::protobuf::{Empty, Timestamp},
    };
    use tokio::time::Instant;

    use super::*;

    const LIVENESS_TTL: Duration = Timing::production().node_control.node_silence;

    const CONTROLLER_STATUS_FRESHNESS: Duration =
        Timing::production().node_control.controller_status_max_age;

    #[test]
    fn mixed_channels_share_one_incarnation_owner_and_replacement_restarts_at_sequence_one() {
        let clock = ManualClock::new();
        let coordinator = controlled_coordinator(&clock);
        let first = incarnation('1');
        let replacement = incarnation('2');
        coordinator
            .register_observation(
                &observation_request(&first, 1),
                observation_input(&first, 1),
            )
            .expect("first observation");
        let first_reply = publish(&coordinator, &first, "a").expect("first publication");
        let first_reference = first_reply.clone();
        let acknowledgement = acknowledgement(first_reference, 1);
        coordinator
            .acknowledge(
                "worker-one",
                &first,
                &acknowledgement,
                offered(&first_reply),
            )
            .expect("first acknowledgement");
        let status = status_request(first_reply.clone(), 1);
        let accepted_status = coordinator
            .accept_status("worker-one", &first, &status, |_| {
                Ok::<_, ()>(NodeControllerStatusAcceptance {
                    accepted_status_sequence: 1,
                    receipts: Vec::new(),
                })
            })
            .expect("first status");

        clock.advance(LIVENESS_TTL + Duration::from_secs(1));
        coordinator
            .register_observation(
                &observation_request(&replacement, 1),
                observation_input(&replacement, 1),
            )
            .expect("replacement observation sequence one");
        let replacement_reply =
            publish(&coordinator, &replacement, "b").expect("replacement publication");

        assert_eq!(accepted_status.accepted_status_sequence, 1);
        assert_eq!(first_reply.view_revision, 1);
        assert_eq!(replacement_reply.view_revision, 2);
        assert!(matches!(
            publish(&coordinator, &first, "stale"),
            Err(IncarnationFenceError::ReplacedIncarnation)
        ));
    }

    #[test]
    fn replaced_incarnation_keeps_fencing_identities_without_reply_payloads() {
        let clock = ManualClock::new();
        let coordinator = controlled_coordinator(&clock);
        let first = incarnation('1');
        let replacement = incarnation('2');
        let first_observation = observation_request(&first, 2);
        coordinator
            .register_observation(&first_observation, observation_input(&first, 2))
            .expect("first observation");
        let first_reply = publish(&coordinator, &first, "a").expect("first publication");
        let acknowledgement = acknowledgement(first_reply.clone(), 2);
        coordinator
            .acknowledge(
                "worker-one",
                &first,
                &acknowledgement,
                offered(&first_reply),
            )
            .expect("first acknowledgement");
        let status = model_status_request(first_reply.clone(), 2, "model-one", 1);
        coordinator
            .accept_status("worker-one", &first, &status, |_| {
                Ok::<_, ()>(NodeControllerStatusAcceptance {
                    accepted_status_sequence: 2,
                    receipts: Vec::new(),
                })
            })
            .expect("first status");

        clock.advance(LIVENESS_TTL + Duration::from_secs(1));
        coordinator
            .register_observation(
                &observation_request(&replacement, 1),
                observation_input(&replacement, 1),
            )
            .expect("replacement observation");

        {
            let state = coordinator.state.lock().expect("coordinator state");
            let node = state.nodes.get("worker-one").expect("node");
            let retired = node.incarnations.get(&first).expect("replaced incarnation");
            assert!(retired.observation.is_none());
            assert!(retired.accepted_status.is_none());
            for exchange in [
                retired
                    .observation_exchange
                    .as_ref()
                    .map(CompletedExchange::retained),
                retired
                    .acknowledgement_exchange
                    .as_ref()
                    .map(CompletedExchange::retained),
                retired
                    .status_exchange
                    .as_ref()
                    .map(CompletedExchange::retained),
            ] {
                assert_eq!(exchange, Some(false));
            }
            assert!(!node.offered_views.contains_key(&first));
            assert_eq!(node.replaced_incarnations, BTreeSet::from([first.clone()]));
        }
        let replaced = |result: Result<(), IncarnationFenceError>| {
            assert!(matches!(
                result,
                Err(IncarnationFenceError::ReplacedIncarnation)
            ));
        };
        replaced(
            coordinator
                .replay_observation("worker-one", &first, &first_observation)
                .map(drop),
        );
        replaced(
            coordinator
                .register_observation(&first_observation, observation_input(&first, 2))
                .map(drop),
        );
        replaced(
            coordinator
                .acknowledge(
                    "worker-one",
                    &first,
                    &acknowledgement,
                    offered(&first_reply),
                )
                .map(drop),
        );
        replaced(
            coordinator
                .accept_status("worker-one", &first, &status, |_| -> Result<_, ()> {
                    panic!("a replaced incarnation's exact retry must not reach the owner")
                })
                .map(drop)
                .map_err(|error| match error {
                    CoordinatorExchangeError::Fence(error) => error,
                    CoordinatorExchangeError::Owner(()) => panic!("owner"),
                }),
        );
        replaced(publish(&coordinator, &first, "stale").map(drop));
        assert_eq!(
            evidence(&coordinator, "model-one", 1),
            DeploymentStatusEvidence::Unavailable
        );
    }

    #[test]
    fn old_offered_ack_and_exact_retry_survive_newer_publication() {
        let coordinator = coordinator();
        let incarnation = incarnation('1');
        coordinator
            .register_observation(
                &observation_request(&incarnation, 1),
                observation_input(&incarnation, 1),
            )
            .expect("observation");
        let old = publish(&coordinator, &incarnation, "old").expect("old publication");
        let acknowledgement = acknowledgement(old.clone(), 1);
        publish(&coordinator, &incarnation, "new").expect("new publication");
        let first = coordinator
            .acknowledge("worker-one", &incarnation, &acknowledgement, offered(&old))
            .expect("old offered acknowledgement");
        publish(&coordinator, &incarnation, "newest").expect("newest publication");
        let replay = coordinator
            .acknowledge(
                "worker-one",
                &incarnation,
                &acknowledgement,
                (u64::MAX, "not-offered"),
            )
            .expect("exact acknowledgement retry");

        assert_eq!(replay, first);
    }

    #[test]
    fn status_cursor_commits_only_after_owner_acceptance() {
        let coordinator = coordinator();
        let incarnation = incarnation('1');
        coordinator
            .register_observation(
                &observation_request(&incarnation, 1),
                observation_input(&incarnation, 1),
            )
            .expect("observation");
        let poll = publish(&coordinator, &incarnation, "status").expect("published view");
        let request = status_request(poll.clone(), 1);
        assert!(matches!(
            coordinator.accept_status("worker-one", &incarnation, &request, |_| Err("rejected")),
            Err(CoordinatorExchangeError::Owner("rejected"))
        ));
        let accepted = coordinator
            .accept_status("worker-one", &incarnation, &request, |_| {
                Ok::<_, &str>(NodeControllerStatusAcceptance {
                    accepted_status_sequence: 1,
                    receipts: Vec::new(),
                })
            })
            .expect("accepted retry");
        publish(
            &coordinator,
            &incarnation,
            "newer-after-lost-status-response",
        )
        .expect("newer publication");
        let replay = coordinator
            .accept_status(
                "worker-one",
                &incarnation,
                &request,
                |_| -> Result<NodeControllerStatusAcceptance, &str> {
                    panic!("exact retry must return completed reply")
                },
            )
            .expect("completed status retry");

        assert_eq!(accepted, replay);
        let stale_new_sequence = status_request(request.view.clone().expect("status view"), 2);
        assert!(matches!(
            coordinator.accept_status(
                "worker-one",
                &incarnation,
                &stale_new_sequence,
                |_| -> Result<NodeControllerStatusAcceptance, &str> {
                    panic!("stale view must be fenced before owner acceptance")
                },
            ),
            Err(CoordinatorExchangeError::Fence(
                IncarnationFenceError::UnofferedView
            ))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn expired_status_is_not_current_while_the_node_stays_live() {
        let coordinator = coordinator();
        let incarnation = incarnation('1');
        coordinator
            .register_observation(
                &observation_request(&incarnation, 1),
                observation_input(&incarnation, 1),
            )
            .expect("observation");
        let poll = publish(&coordinator, &incarnation, "status-freshness").expect("publication");
        let request = model_status_request(poll.clone(), 1, "model-one", 7);
        coordinator
            .accept_status("worker-one", &incarnation, &request, |_| {
                Ok::<_, ()>(NodeControllerStatusAcceptance {
                    accepted_status_sequence: 1,
                    receipts: Vec::new(),
                })
            })
            .expect("status");

        assert_eq!(
            evidence(&coordinator, "model-one", 7),
            DeploymentStatusEvidence::Current
        );
        stay_in_contact(
            &coordinator,
            &incarnation,
            CONTROLLER_STATUS_FRESHNESS + Duration::from_secs(1),
        )
        .await;
        assert_eq!(
            evidence(&coordinator, "model-one", 7),
            DeploymentStatusEvidence::Unavailable
        );
        // Still live through its stream, while its 91 s old observation is overdue.
        assert_eq!(
            coordinator
                .observation(&SiteNodeId::for_test("worker-one"))
                .expect("observation")
                .connection_state,
            CoordinatorConnectionState::Stale
        );
    }

    #[test]
    fn status_against_an_older_view_stays_current_for_the_generations_it_lists() {
        let coordinator = coordinator();
        let incarnation = incarnation('1');
        coordinator
            .register_observation(
                &observation_request(&incarnation, 1),
                observation_input(&incarnation, 1),
            )
            .expect("observation");
        let poll = publish(&coordinator, &incarnation, "old").expect("publication");
        accept_model_status(&coordinator, &incarnation, &poll, 1, "model-one", 7);

        publish(&coordinator, &incarnation, "new").expect("newer publication");

        assert_eq!(
            coordinator.model_status_evidence(&[
                reference("worker-one", "model-one", 7),
                reference("worker-one", "model-one", 8),
                reference("worker-one", "model-two", 1),
                reference("worker-two", "model-one", 7),
            ]),
            [
                (DeploymentStatusEvidence::Current, Some(reported_memory())),
                (DeploymentStatusEvidence::AcknowledgementPending, None),
                (DeploymentStatusEvidence::AcknowledgementPending, None),
                (DeploymentStatusEvidence::Unavailable, None),
            ]
            .map(|(evidence, accelerator_memory)| ModelStatusEvidence {
                evidence,
                accelerator_memory,
            })
        );
    }

    #[test]
    fn application_status_evidence_carries_unit_memory_only_while_current() {
        let coordinator = coordinator();
        let incarnation = incarnation('1');
        coordinator
            .register_observation(
                &observation_request(&incarnation, 1),
                observation_input(&incarnation, 1),
            )
            .expect("observation");
        let view = publish(&coordinator, &incarnation, "a").expect("publication");
        let memory = vec![ApplicationServiceAcceleratorMemory {
            service_name: "web".to_owned(),
            accelerator_memory: Some(reported_memory()),
        }];
        let request = NodeControllerStatus {
            view: Some(view),
            status_sequence: 1,
            sections: vec![ControllerStatusSection {
                payload: Some(controller_status_section::Payload::Application(
                    ApplicationControllerStatusSection {
                        deployments: vec![ApplicationDeploymentStatus {
                            application_deployment_id: "app-one".to_owned(),
                            lifecycle_generation: 3,
                            service_accelerator_memory: memory.clone(),
                            ..Default::default()
                        }],
                        cleanup_evidence: Vec::new(),
                    },
                )),
            }],
        };
        coordinator
            .accept_status("worker-one", &incarnation, &request, |_| {
                Ok::<_, ()>(NodeControllerStatusAcceptance {
                    accepted_status_sequence: 1,
                    receipts: Vec::new(),
                })
            })
            .expect("accepted status");

        assert_eq!(
            coordinator.application_status_evidence(&[
                reference("worker-one", "app-one", 3),
                reference("worker-one", "app-one", 4),
                reference("worker-two", "app-one", 3),
            ]),
            [
                (DeploymentStatusEvidence::Current, memory),
                (DeploymentStatusEvidence::AcknowledgementPending, Vec::new()),
                (DeploymentStatusEvidence::Unavailable, Vec::new()),
            ]
            .map(|(evidence, accelerator_memory)| ApplicationStatusEvidence {
                evidence,
                accelerator_memory,
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pending_acknowledgement_expires_once_older_view_statuses_are_refused() {
        let coordinator = coordinator();
        let incarnation = incarnation('1');
        coordinator
            .register_observation(
                &observation_request(&incarnation, 1),
                observation_input(&incarnation, 1),
            )
            .expect("observation");
        let old = publish(&coordinator, &incarnation, "old").expect("publication");
        accept_model_status(&coordinator, &incarnation, &old, 1, "model-one", 7);
        publish(&coordinator, &incarnation, "new").expect("newer publication");
        stay_in_contact(&coordinator, &incarnation, CONTROLLER_STATUS_FRESHNESS).await;
        assert_eq!(
            evidence(&coordinator, "model-one", 8),
            DeploymentStatusEvidence::AcknowledgementPending
        );

        // A node that never acknowledges can only resend against the older view.
        assert!(matches!(
            coordinator.accept_status(
                "worker-one",
                &incarnation,
                &model_status_request(old.clone(), 2, "model-one", 7),
                |_| -> Result<NodeControllerStatusAcceptance, ()> {
                    panic!("an older-view status must be fenced before owner acceptance")
                },
            ),
            Err(CoordinatorExchangeError::Fence(
                IncarnationFenceError::UnofferedView
            ))
        ));
        tokio::time::advance(Duration::from_secs(1)).await;

        assert_eq!(
            evidence(&coordinator, "model-one", 8),
            DeploymentStatusEvidence::Unavailable
        );
        assert_eq!(
            coordinator
                .observation(&SiteNodeId::for_test("worker-one"))
                .expect("observation")
                .connection_state,
            CoordinatorConnectionState::Stale
        );
    }

    #[tokio::test(start_paused = true)]
    async fn node_without_a_connected_status_bearing_incarnation_is_unavailable() {
        let coordinator = coordinator();
        let incarnation = incarnation('1');
        assert_eq!(
            evidence(&coordinator, "model-one", 7),
            DeploymentStatusEvidence::Unavailable,
            "unknown node"
        );

        coordinator
            .register_observation(
                &observation_request(&incarnation, 1),
                observation_input(&incarnation, 1),
            )
            .expect("observation");
        let poll = publish(&coordinator, &incarnation, "view").expect("publication");
        assert_eq!(
            evidence(&coordinator, "model-one", 7),
            DeploymentStatusEvidence::Unavailable,
            "Connected node with no accepted status"
        );

        accept_model_status(&coordinator, &incarnation, &poll, 1, "model-one", 7);
        assert_eq!(
            evidence(&coordinator, "model-one", 7),
            DeploymentStatusEvidence::Current
        );
        tokio::time::advance(Duration::from_secs(31)).await;
        for generation in [7, 8] {
            assert_eq!(
                evidence(&coordinator, "model-one", generation),
                DeploymentStatusEvidence::Unavailable,
                "node no longer Connected"
            );
        }
    }

    #[test]
    fn concurrent_snapshot_and_publication_are_serialized_in_capture_order() {
        let coordinator = coordinator();
        let incarnation = incarnation('1');
        coordinator
            .register_observation(
                &observation_request(&incarnation, 1),
                observation_input(&incarnation, 1),
            )
            .expect("observation");
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let (started_tx, started_rx) = mpsc::channel();

        let old_coordinator = coordinator.clone();
        let old_incarnation = incarnation.clone();
        let old_entered = Arc::clone(&entered);
        let old_release = Arc::clone(&release);
        let old = std::thread::spawn(move || {
            let attachment = old_coordinator
                .open_stream("worker-one", &old_incarnation)
                .expect("old stream");
            old_coordinator
                .publish_stream_view("worker-one", &old_incarnation, attachment.epoch, 1, || {
                    old_entered.wait();
                    old_release.wait();
                    Ok::<_, ()>(published("old"))
                })
                .expect("old publication")
        });
        entered.wait();

        let new_coordinator = coordinator.clone();
        let new_incarnation = incarnation.clone();
        let new = std::thread::spawn(move || {
            started_tx.send(()).expect("new publication started");
            publish(&new_coordinator, &new_incarnation, "new").expect("new publication")
        });
        started_rx.recv().expect("new thread started");
        release.wait();
        let old = old.join().expect("old thread");
        let new = new.join().expect("new thread");
        let unchanged = publish(&coordinator, &incarnation, "new").expect("unchanged publication");

        assert_eq!(old.identity.expect("old view identity").view_revision, 1);
        assert_eq!(new.view_revision, 2);
        assert_eq!(unchanged.view_revision, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn recent_contact_keeps_status_current_while_an_hour_old_observation_is_stale() {
        let coordinator = coordinator();
        let incarnation = incarnation('1');
        coordinator
            .register_observation(
                &observation_request(&incarnation, 1),
                observation_input(&incarnation, 1),
            )
            .expect("observation");
        // The node keeps receiving views and reporting status for an hour but never observes
        // again.
        for sequence in 1..=720 {
            tokio::time::advance(Duration::from_secs(5)).await;
            let poll = publish(&coordinator, &incarnation, "hour").expect("publication");
            accept_model_status(&coordinator, &incarnation, &poll, sequence, "model-one", 7);
        }

        let observation = coordinator
            .observation(&SiteNodeId::for_test("worker-one"))
            .expect("observation");
        assert_eq!(
            observation.connection_state,
            CoordinatorConnectionState::Stale
        );
        assert_eq!(
            observation.received_at.seconds, 1,
            "the hour-old observation is kept"
        );
        assert_eq!(
            evidence(&coordinator, "model-one", 7),
            DeploymentStatusEvidence::Current
        );
        let serving = coordinator.serving_evidence();
        assert_eq!(serving.len(), 1);
        assert_eq!(serving[0].connected_until, Instant::now() + LIVENESS_TTL);
        assert!(serving[0].status.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn fenced_requests_never_renew_liveness() {
        let coordinator = coordinator();
        let first = incarnation('1');
        coordinator
            .register_observation(
                &observation_request(&first, 1),
                observation_input(&first, 1),
            )
            .expect("first observation");
        tokio::time::advance(LIVENESS_TTL + Duration::from_secs(1)).await;
        let replacement = incarnation('2');
        coordinator
            .register_observation(
                &observation_request(&replacement, 1),
                observation_input(&replacement, 1),
            )
            .expect("replacement after silence");
        let view =
            publish(&coordinator, &replacement, "replacement").expect("replacement publication");

        // For the whole window, the replaced process keeps reopening its stream and the current
        // one sends only statuses for a view it was never offered; neither counts as contact.
        for sequence in 2..=(LIVENESS_TTL.as_secs() + 1) {
            tokio::time::advance(Duration::from_secs(1)).await;
            assert!(matches!(
                publish(&coordinator, &first, "replaced"),
                Err(IncarnationFenceError::ReplacedIncarnation)
            ));
            let mut unoffered = view.clone();
            unoffered.view_revision += 1;
            assert!(matches!(
                coordinator.accept_status(
                    "worker-one",
                    &replacement,
                    &model_status_request(unoffered, sequence, "model-one", 7),
                    |_| -> Result<NodeControllerStatusAcceptance, ()> {
                        panic!("an unoffered view is fenced before owner acceptance")
                    },
                ),
                Err(CoordinatorExchangeError::Fence(
                    IncarnationFenceError::UnofferedView
                ))
            ));
        }
        tokio::time::advance(Duration::from_millis(1)).await;

        assert_eq!(
            coordinator
                .observation(&SiteNodeId::for_test("worker-one"))
                .expect("observation")
                .connection_state,
            CoordinatorConnectionState::Disconnected
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_advanced_clock_releases_the_restart_fence() {
        let clock = ManualClock::new();
        let coordinator = controlled_coordinator(&clock);
        let first = incarnation('1');
        let replacement = incarnation('2');
        coordinator
            .register_observation(
                &observation_request(&first, 1),
                observation_input(&first, 1),
            )
            .expect("first observation");

        clock.advance(LIVENESS_TTL);
        assert!(matches!(
            coordinator.register_observation(
                &observation_request(&replacement, 1),
                observation_input(&replacement, 1),
            ),
            Err(IncarnationFenceError::CurrentIncarnationActive)
        ));

        clock.advance(Duration::from_millis(1));
        coordinator
            .register_observation(
                &observation_request(&replacement, 1),
                observation_input(&replacement, 1),
            )
            .expect("replacement once the silent incarnation's lease passed on the clock");
        assert_eq!(
            coordinator
                .observation(&SiteNodeId::for_test("worker-one"))
                .expect("observation")
                .incarnation,
            replacement
        );
    }

    #[tokio::test(start_paused = true)]
    async fn liveness_ages_follow_the_clock() {
        let clock = ManualClock::new();
        let coordinator = controlled_coordinator(&clock);
        let incarnation = incarnation('1');
        coordinator
            .register_observation(
                &observation_request(&incarnation, 1),
                observation_input(&incarnation, 1),
            )
            .expect("observation");
        clock.advance(Duration::from_secs(2));
        assert_eq!(
            coordinator.liveness_ages(),
            BTreeMap::from([(
                "worker-one".to_owned(),
                NodeLivenessAges {
                    contact: Duration::from_secs(2),
                    status: None,
                }
            )])
        );

        let poll = publish(&coordinator, &incarnation, "ages").expect("publication");
        clock.advance(Duration::from_secs(3));
        accept_model_status(&coordinator, &incarnation, &poll, 1, "model-one", 7);
        clock.advance(Duration::from_secs(4));
        tokio::time::advance(Duration::from_secs(1)).await;

        assert_eq!(
            coordinator.liveness_ages(),
            BTreeMap::from([(
                "worker-one".to_owned(),
                NodeLivenessAges {
                    contact: Duration::from_secs(5),
                    status: Some(Duration::from_secs(5)),
                }
            )])
        );
    }

    #[test]
    fn a_closed_stream_disconnects_at_once_while_serving_keeps_the_silence_bound() {
        let clock = ManualClock::new();
        let coordinator = controlled_coordinator(&clock);
        let node = incarnation('1');
        let stream = coordinator
            .open_stream("worker-one", &node)
            .expect("open stream");
        coordinator
            .register_observation(&observation_request(&node, 1), observation_input(&node, 1))
            .expect("first observation");
        let view = stream_view(&coordinator, &node, stream.epoch, 1);
        accept_stream_status(&coordinator, &node, &view, 1);
        let before = serving_snapshot(&coordinator);

        assert!(coordinator.close_stream("worker-one", &node, stream.epoch));

        assert_eq!(
            connection(&coordinator),
            CoordinatorConnectionState::Disconnected
        );
        assert_eq!(
            coordinator
                .node_endpoint("worker-one")
                .expect("endpoint")
                .connection_state,
            CoordinatorConnectionState::Disconnected
        );
        assert_eq!(
            serving_snapshot(&coordinator),
            before,
            "serving evidence keeps the last contact after the close"
        );
        let (_, connected_until, _) = before.expect("serving evidence");
        assert_eq!(
            connected_until,
            coordinator_contact(&coordinator) + LIVENESS_TTL
        );
        clock.advance(LIVENESS_TTL + Duration::from_secs(1));
        assert!(
            serving_snapshot(&coordinator)
                .is_some_and(|(_, until, _)| until < coordinator.clock().now()),
            "serving ends at the silence bound"
        );
    }

    #[test]
    fn a_commit_finishing_after_its_stream_closed_never_renews_liveness() {
        let clock = ManualClock::new();
        let coordinator = controlled_coordinator(&clock);
        let node = incarnation('1');
        let stream = coordinator
            .open_stream("worker-one", &node)
            .expect("open stream");
        coordinator
            .register_observation(&observation_request(&node, 1), observation_input(&node, 1))
            .expect("first observation");
        let view = stream_view(&coordinator, &node, stream.epoch, 1);
        let before = serving_snapshot(&coordinator);
        assert!(coordinator.close_stream("worker-one", &node, stream.epoch));
        clock.advance(Duration::from_secs(2));

        // Commits that were already running when the stream closed record their content late.
        accept_stream_status(&coordinator, &node, &view, 1);
        coordinator
            .register_observation(&observation_request(&node, 2), observation_input(&node, 2))
            .expect("late observation recorded");
        coordinator.stream_contact("worker-one", &node, stream.epoch);

        let observation = coordinator
            .observation(&SiteNodeId::for_test("worker-one"))
            .expect("observation");
        assert_eq!(observation.revision, 2);
        assert_eq!(
            observation.connection_state,
            CoordinatorConnectionState::Disconnected
        );
        assert!(coordinator.liveness_ages()["worker-one"].contact >= Duration::from_secs(2));
        let (_, connected_until, status) = serving_snapshot(&coordinator).expect("serving");
        assert_eq!(connected_until, before.expect("serving before").1);
        assert!(status.is_some(), "the late status is still recorded");
    }

    #[test]
    fn a_replaced_stream_neither_disconnects_nor_renews_and_its_late_close_changes_nothing() {
        let clock = ManualClock::new();
        let coordinator = controlled_coordinator(&clock);
        let node = incarnation('1');
        let older = coordinator
            .open_stream("worker-one", &node)
            .expect("older stream");
        coordinator
            .register_observation(&observation_request(&node, 1), observation_input(&node, 1))
            .expect("first observation");
        let view = stream_view(&coordinator, &node, older.epoch, 1);
        accept_stream_status(&coordinator, &node, &view, 1);

        let newer = coordinator
            .open_stream("worker-one", &node)
            .expect("newer stream of the same incarnation");
        assert_eq!(*older.replaced.borrow(), newer.epoch);
        assert!(newer.epoch > older.epoch);
        clock.advance(Duration::from_secs(3));
        coordinator.stream_contact("worker-one", &node, older.epoch);
        assert!(coordinator.liveness_ages()["worker-one"].contact >= Duration::from_secs(3));
        assert!(!coordinator.close_stream("worker-one", &node, older.epoch));

        assert_eq!(
            connection(&coordinator),
            CoordinatorConnectionState::Connected
        );
        let (_, _, status) = serving_snapshot(&coordinator).expect("serving evidence");
        assert!(
            status.is_some(),
            "the replacement keeps the accepted status"
        );
        coordinator.stream_contact("worker-one", &node, newer.epoch);
        assert!(coordinator.liveness_ages()["worker-one"].contact < Duration::from_secs(3));
        assert!(matches!(
            coordinator.publish_stream_view("worker-one", &node, older.epoch, 2, || Ok::<_, ()>(
                published("a")
            )),
            Err(CoordinatorExchangeError::Fence(
                IncarnationFenceError::StreamReplaced
            ))
        ));

        assert!(coordinator.close_stream("worker-one", &node, newer.epoch));
        assert_eq!(
            connection(&coordinator),
            CoordinatorConnectionState::Disconnected
        );
    }

    #[test]
    fn a_stream_observation_outliving_its_forgotten_stream_neither_registers_nor_blocks_the_next() {
        let clock = ManualClock::new();
        let coordinator = controlled_coordinator(&clock);
        let older = incarnation('1');
        let newer = incarnation('2');
        let older_stream = coordinator
            .open_stream("worker-one", &older)
            .expect("older stream");
        // A newer process opens before the older one registered, forgetting the older record.
        let newer_stream = coordinator
            .open_stream("worker-one", &newer)
            .expect("newer stream");

        assert_eq!(
            coordinator.register_stream_observation(
                &observation_request(&older, 1),
                observation_input(&older, 1),
                older_stream.epoch,
            ),
            Err(IncarnationFenceError::StreamReplaced),
            "the older stream's in-flight observation never registers"
        );
        assert!(!coordinator.close_stream("worker-one", &older, older_stream.epoch));
        coordinator
            .register_stream_observation(
                &observation_request(&newer, 1),
                observation_input(&newer, 1),
                newer_stream.epoch,
            )
            .expect("newer observation registers at once");

        let observation = coordinator
            .observation(&SiteNodeId::for_test("worker-one"))
            .expect("observation");
        assert_eq!(observation.incarnation, newer);
        assert_eq!(
            observation.connection_state,
            CoordinatorConnectionState::Connected
        );
        assert!(coordinator.close_stream("worker-one", &newer, newer_stream.epoch));
        assert!(matches!(
            coordinator.register_stream_observation(
                &observation_request(&newer, 2),
                observation_input(&newer, 2),
                newer_stream.epoch,
            ),
            Err(IncarnationFenceError::StreamReplaced)
        ));
    }

    #[test]
    fn a_new_incarnation_opens_only_once_the_current_stream_closed() {
        let clock = ManualClock::new();
        let coordinator = controlled_coordinator(&clock);
        let first = incarnation('1');
        let replacement = incarnation('2');
        let stream = coordinator
            .open_stream("worker-one", &first)
            .expect("first stream");
        coordinator
            .register_observation(
                &observation_request(&first, 1),
                observation_input(&first, 1),
            )
            .expect("first observation");

        assert!(matches!(
            coordinator.open_stream("worker-one", &replacement),
            Err(IncarnationFenceError::CurrentIncarnationActive)
        ));
        assert!(coordinator.close_stream("worker-one", &first, stream.epoch));
        let replacing = coordinator
            .open_stream("worker-one", &replacement)
            .expect("replacement stream once the first closed");
        let view = stream_view(&coordinator, &replacement, replacing.epoch, 1);
        coordinator
            .register_observation(
                &observation_request(&replacement, 1),
                observation_input(&replacement, 1),
            )
            .expect("replacement observation");

        let observation = coordinator
            .observation(&SiteNodeId::for_test("worker-one"))
            .expect("observation");
        assert_eq!(observation.incarnation, replacement);
        assert_eq!(
            observation.connection_state,
            CoordinatorConnectionState::Connected
        );
        assert_eq!(
            observation.replaced_incarnations,
            BTreeSet::from([first.clone()])
        );
        accept_stream_status(&coordinator, &replacement, &view, 1);
        assert!(matches!(
            coordinator.open_stream("worker-one", &first),
            Err(IncarnationFenceError::ReplacedIncarnation)
        ));
    }

    #[test]
    fn a_configured_silence_bound_sets_liveness_and_serving() {
        let clock = ManualClock::new();
        let bound = Duration::from_secs(60);
        let coordinator = ManagerNodeControlCoordinator::new(
            manager_session(),
            &NodeControlTiming {
                node_silence: bound,
                ..Timing::production().node_control
            },
            Arc::new(clock.clone()),
        );
        let node = incarnation('1');
        coordinator
            .register_observation(&observation_request(&node, 1), observation_input(&node, 1))
            .expect("observation");

        clock.advance(LIVENESS_TTL + Duration::from_secs(1));
        assert_eq!(coordinator.node_silence(), bound);
        assert_eq!(
            connection(&coordinator),
            CoordinatorConnectionState::Connected
        );
        let (_, connected_until, _) = serving_snapshot(&coordinator).expect("serving");
        assert_eq!(connected_until, coordinator_contact(&coordinator) + bound);
        clock.advance(bound);
        assert_eq!(
            connection(&coordinator),
            CoordinatorConnectionState::Disconnected
        );
    }

    fn coordinator() -> ManagerNodeControlCoordinator {
        ManagerNodeControlCoordinator::on_system_clock(manager_session())
    }

    /// Publishes worker-one's view on its stream of `epoch` under `sequence`.
    fn stream_view(
        coordinator: &ManagerNodeControlCoordinator,
        incarnation: &NodeIncarnation,
        epoch: u64,
        sequence: u64,
    ) -> NodeDesiredView {
        coordinator
            .publish_stream_view("worker-one", incarnation, epoch, sequence, || {
                Ok::<_, ()>(published("a"))
            })
            .expect("published stream view")
    }

    fn accept_stream_status(
        coordinator: &ManagerNodeControlCoordinator,
        incarnation: &NodeIncarnation,
        view: &NodeDesiredView,
        status_sequence: u64,
    ) {
        let identity = view.identity.as_ref().expect("view identity");
        let reference = NodeViewReference {
            site_id: identity.site_id.clone(),
            node_id: identity.node_id.clone(),
            manager_session_id: identity.manager_session_id.clone(),
            node_incarnation_id: identity.node_incarnation_id.clone(),
            view_revision: identity.view_revision,
            content_sha256: identity.content_sha256.clone(),
        };
        coordinator
            .accept_status(
                "worker-one",
                incarnation,
                &model_status_request(reference, status_sequence, "model-one", 1),
                |_| {
                    Ok::<_, ()>(NodeControllerStatusAcceptance {
                        accepted_status_sequence: status_sequence,
                        receipts: Vec::new(),
                    })
                },
            )
            .expect("accepted status");
    }

    /// Worker-one's serving evidence: incarnation, end of serving, and status receipt time.
    fn serving_snapshot(
        coordinator: &ManagerNodeControlCoordinator,
    ) -> Option<(NodeIncarnation, Instant, Option<Instant>)> {
        coordinator
            .serving_evidence()
            .into_iter()
            .find(|evidence| evidence.node_id == "worker-one")
            .map(|evidence| {
                (
                    evidence.incarnation,
                    evidence.connected_until,
                    evidence.status.map(|status| status.received),
                )
            })
    }

    fn connection(coordinator: &ManagerNodeControlCoordinator) -> CoordinatorConnectionState {
        coordinator
            .observation(&SiteNodeId::for_test("worker-one"))
            .expect("observation")
            .connection_state
    }

    /// Instant of worker-one's latest contact on the coordinator's clock.
    fn coordinator_contact(coordinator: &ManagerNodeControlCoordinator) -> Instant {
        let state = coordinator.state.lock().expect("manager node-control lock");
        let node = state.nodes.get("worker-one").expect("node");
        node.incarnations[node.current_incarnation.as_ref().expect("current")]
            .last_contact
            .expect("contact")
    }

    fn controlled_coordinator(clock: &ManualClock) -> ManagerNodeControlCoordinator {
        ManagerNodeControlCoordinator::new(
            manager_session(),
            &Timing::production().node_control,
            Arc::new(clock.clone()),
        )
    }

    fn manager_session() -> ManagerSession {
        ManagerSession::parse("a".repeat(32)).expect("manager session")
    }

    fn incarnation(character: char) -> NodeIncarnation {
        NodeIncarnation::parse(character.to_string().repeat(32)).expect("incarnation")
    }

    fn observation_request(incarnation: &NodeIncarnation, revision: u64) -> NodeObservation {
        NodeObservation {
            site_id: "site-one".to_owned(),
            node_id: "worker-one".to_owned(),
            incarnation_id: incarnation.as_str().to_owned(),
            revision,
            snapshot: Some(NodeObservationSnapshot::default()),
            observation_configuration_sha256: "0".repeat(64),
            private_port: None,
        }
    }

    /// Keeps `incarnation` live for `duration` through contact on one open stream every few
    /// seconds, as a node whose observation reports stopped but whose stream continues.
    async fn stay_in_contact(
        coordinator: &ManagerNodeControlCoordinator,
        incarnation: &NodeIncarnation,
        duration: Duration,
    ) {
        let attachment = coordinator
            .open_stream("worker-one", incarnation)
            .expect("open stream");
        let step = LIVENESS_TTL / 2;
        let mut remaining = duration;
        while !remaining.is_zero() {
            let advanced = remaining.min(step);
            tokio::time::advance(advanced).await;
            remaining -= advanced;
            coordinator.stream_contact("worker-one", incarnation, attachment.epoch);
        }
    }

    /// Publishes a view naming `target_id` as the first publication of a new stream of
    /// `incarnation`.
    fn publish(
        coordinator: &ManagerNodeControlCoordinator,
        incarnation: &NodeIncarnation,
        target_id: &str,
    ) -> Result<NodeViewReference, IncarnationFenceError> {
        coordinator.publish_view_for_test("worker-one", incarnation, published(target_id))
    }

    fn observation_input(
        incarnation: &NodeIncarnation,
        revision: u64,
    ) -> AcceptedNodeObservationInput {
        AcceptedNodeObservationInput {
            node_id: "worker-one".to_owned(),
            incarnation: incarnation.clone(),
            revision,
            snapshot: NodeObservationSnapshot::default(),
            observation_configuration_sha256: NodeObservationConfigurationDigest::parse(
                "0".repeat(64),
            )
            .expect("digest"),
            received_at: Timestamp {
                seconds: revision as i64,
                nanos: 0,
            },
            private_port: None,
        }
    }

    fn published(target_id: &str) -> PublishedViewInput {
        PublishedViewInput {
            site_id: "site-one".to_owned(),
            projection: NodeViewProjection::new(
                vec![ExecutionTarget {
                    execution_target_id: target_id.to_owned(),
                    site_node_id: "worker-one".to_owned(),
                    ..Default::default()
                }],
                Vec::new(),
            )
            .expect("projection"),
        }
    }

    fn offered(view: &NodeViewReference) -> (u64, &str) {
        (view.view_revision, &view.content_sha256)
    }

    fn acknowledgement(
        view: NodeViewReference,
        acknowledgement_sequence: u64,
    ) -> NodeViewAcknowledgement {
        NodeViewAcknowledgement {
            view: Some(view),
            acknowledgement_sequence,
            outcome: Some(node_view_acknowledgement::Outcome::Accepted(Empty {})),
        }
    }

    fn status_request(view: NodeViewReference, status_sequence: u64) -> NodeControllerStatus {
        NodeControllerStatus {
            view: Some(view),
            status_sequence,
            sections: Vec::new(),
        }
    }

    fn model_status_request(
        view: NodeViewReference,
        status_sequence: u64,
        deployment_id: &str,
        lifecycle_generation: u64,
    ) -> NodeControllerStatus {
        NodeControllerStatus {
            view: Some(view),
            status_sequence,
            sections: vec![ControllerStatusSection {
                payload: Some(controller_status_section::Payload::Model(
                    ModelControllerStatusSection {
                        deployments: vec![ModelDeploymentStatus {
                            model_deployment_id: deployment_id.to_owned(),
                            lifecycle_generation,
                            lifecycle: ModelLifecycleObservation::Ready as i32,
                            binding_readiness: ModelBindingReadiness::Ready as i32,
                            state_revision: 1,
                            accelerator_memory: Some(reported_memory()),
                            ..Default::default()
                        }],
                        cleanup_evidence: Vec::new(),
                    },
                )),
            }],
        }
    }

    fn reported_memory() -> AcceleratorMemoryObservation {
        AcceleratorMemoryObservation {
            result: Some(
                dotcore_proto::dotcore::deployment::v1::accelerator_memory_observation::Result::NotApplicable(
                    dotcore_proto::google::protobuf::Empty {},
                ),
            ),
            ..Default::default()
        }
    }

    fn accept_model_status(
        coordinator: &ManagerNodeControlCoordinator,
        incarnation: &NodeIncarnation,
        view: &NodeViewReference,
        status_sequence: u64,
        deployment_id: &str,
        lifecycle_generation: u64,
    ) {
        coordinator
            .accept_status(
                "worker-one",
                incarnation,
                &model_status_request(
                    view.clone(),
                    status_sequence,
                    deployment_id,
                    lifecycle_generation,
                ),
                |_| {
                    Ok::<_, ()>(NodeControllerStatusAcceptance {
                        accepted_status_sequence: status_sequence,
                        receipts: Vec::new(),
                    })
                },
            )
            .expect("accepted status");
    }

    fn reference<'a>(
        node_id: &'a str,
        deployment_id: &'a str,
        lifecycle_generation: u64,
    ) -> NodeDeploymentReference<'a> {
        NodeDeploymentReference {
            node_id,
            deployment_id,
            lifecycle_generation,
        }
    }

    /// Evidence for one deployment generation placed on `worker-one`.
    fn evidence(
        coordinator: &ManagerNodeControlCoordinator,
        deployment_id: &str,
        lifecycle_generation: u64,
    ) -> DeploymentStatusEvidence {
        coordinator.model_status_evidence(&[reference(
            "worker-one",
            deployment_id,
            lifecycle_generation,
        )])[0]
            .evidence
    }
}
