//! Projects manager-authoritative site state into complete node-scoped desired views and installs
//! validated views into node-local read-only controller input.
//!
//! The manager publishes on each node's control stream and accepts the acknowledgements and status
//! that come back through owner commit sections. Nodes atomically replace a disposable projection
//! only after identity, content, and immutable installed-target validation, and a rejected status
//! never stops view refresh.

use std::sync::Arc;

use dotcore_proto::dotcore::{
    application::{
        private::v1::{
            ApplicationAssignment, ApplicationCleanupReceipt, ApplicationControllerStatusSection,
            ApplicationDesiredViewSection, ApplicationLinkedRouteProjection,
            ApplicationStatusFailureCode, application_linked_route_projection,
        },
        v1::{ApplicationDeploymentDeclaration, ApplicationDeploymentDesiredState},
    },
    model::{
        private::v1::{ModelAssignment, ModelControllerStatusSection, ModelDesiredViewSection},
        v1::{
            ModelDeploymentDeclaration, ModelDeploymentDesiredState,
            ModelDeploymentLifecycleTransition,
        },
    },
    site::{
        private::v1::{
            ControllerStatusReceipt, DesiredViewRejectionCode, DesiredViewSection,
            DesiredViewSectionKind, NodeControlOpen, NodeControllerStatus,
            NodeControllerStatusAcceptance, NodeDesiredView, NodeViewAcknowledgement,
            NodeViewAcknowledgementAcceptance, NodeViewIdentity, RequiredDesiredViewSection,
            controller_status_receipt, desired_view_section,
        },
        v1::{ExecutionTarget, Site, execution_target},
    },
};
use prost::Message;
use tonic::Status;
use tracing::{debug, warn};

use crate::store::StoreError;
use crate::{
    node::{
        control::{
            CoordinatorExchangeError, ManagerNodeControlCoordinator, NodeStreamAttachment,
            PublishedViewInput,
        },
        hardware_inventory::HardwareCollection,
    },
    repository::SiteRepository,
    view::{
        contract::{
            APPLICATION_SECTION_SCHEMA_VERSION, MAX_APPLICATION_ASSIGNMENTS, MAX_EXECUTION_TARGETS,
            MAX_MODEL_ASSIGNMENTS, MAX_NODE_VIEW_BYTES, MODEL_SECTION_SCHEMA_VERSION,
            NODE_VIEW_PROJECTION_SCHEMA_VERSION, ValidatedNodeView, validate_acknowledgement,
            validate_application_status, validate_controller_status, validate_published_view,
        },
        fencing::{
            AcceptedNodeViewAuthority, IncarnationFenceError, ManagerSession, NodeIncarnation,
            NodeViewProjection, NodeViewStream, ViewAcceptance,
        },
    },
};

/// Longest inference route name, the site identifier bound, used to size linked-route projections.
const MAX_ROUTE_NAME_BYTES: usize = 63;
/// Manager-owned projector and revision authority for complete node views.
///
/// Each stream publication projects the current site: model intent and targets directly,
/// and application intent through the apps-controller owner, which supplies each assignment's
/// linked-route projection. A changed projection publishes a new revision.
#[derive(Clone)]
pub struct ManagerNodeViewPublisher {
    site: SiteRepository,
    coordinator: ManagerNodeControlCoordinator,
    model: Arc<dyn ModelStatusOwner>,
    application: Arc<dyn ApplicationNodeViewOwner>,
}

impl ManagerNodeViewPublisher {
    pub fn new(
        site: SiteRepository,
        coordinator: ManagerNodeControlCoordinator,
        model: Arc<dyn ModelStatusOwner>,
        application: Arc<dyn ApplicationNodeViewOwner>,
    ) -> Self {
        Self {
            site,
            coordinator,
            model,
            application,
        }
    }

    /// Opens a node control stream of `incarnation` for the declared `SiteNode` `node_id` and
    /// publishes its first view under publication sequence 1, in one commit section.
    ///
    /// When the current view cannot be built, the node's last published view answers the open
    /// and a later republication recovers, so a failure that a republication would only report
    /// never keeps the node from connecting. Without any published view the stream is closed
    /// again and the open is unavailable.
    pub(crate) fn commit_stream_open(
        &self,
        node_id: &str,
        incarnation: &NodeIncarnation,
    ) -> Result<(NodeStreamAttachment, NodeDesiredView), DesiredViewDistributionError> {
        if !self
            .site
            .snapshot()?
            .nodes
            .iter()
            .any(|node| node.node_id == node_id)
        {
            return Err(DesiredViewDistributionError::UndeclaredNode);
        }
        let attachment = self.coordinator.open_stream(node_id, incarnation)?;
        let error = match self.commit_stream_publication(node_id, incarnation, attachment.epoch, 1)
        {
            Ok(view) => return Ok((attachment, view)),
            Err(
                error @ DesiredViewDistributionError::Incarnation(
                    IncarnationFenceError::StreamReplaced,
                ),
            ) => return Err(error),
            Err(error) => error,
        };
        let last =
            self.coordinator
                .publish_last_stream_view(node_id, incarnation, attachment.epoch, 1);
        if let Ok(Some(view)) = last {
            warn!(event_kind = "node_control_view_publication", outcome = "fallback", node_id, error = %error, "Manager could not build a node view at stream open and answered with the last published view");
            return Ok((attachment, view));
        }
        warn!(event_kind = "node_control_view_publication", outcome = "failed", node_id, error = %error, "Manager could not build the first view of a node control stream and has none published");
        self.coordinator
            .close_stream(node_id, incarnation, attachment.epoch);
        Err(DesiredViewDistributionError::FirstViewUnavailable(
            Box::new(error),
        ))
    }

    /// Publishes the node's current complete view on its open stream of `epoch`.
    pub(crate) fn commit_stream_publication(
        &self,
        node_id: &str,
        incarnation: &NodeIncarnation,
        epoch: u64,
        publication_sequence: u64,
    ) -> Result<NodeDesiredView, DesiredViewDistributionError> {
        self.coordinator
            .publish_stream_view(node_id, incarnation, epoch, publication_sequence, || {
                self.view_input(node_id)
            })
            .map_err(DesiredViewDistributionError::from_coordinator)
    }

    /// Returns the listener woken after each accepted site change, which may change node views.
    pub(crate) fn site_change_listener(&self) -> Arc<tokio::sync::Notify> {
        self.site.register_change_listener()
    }

    pub(crate) fn coordinator(&self) -> &ManagerNodeControlCoordinator {
        &self.coordinator
    }

    /// Projects the complete view of `node_id` from current site intent: model intent and
    /// targets directly, application intent through the apps-controller owner.
    fn view_input(
        &self,
        node_id: &str,
    ) -> Result<PublishedViewInput, DesiredViewDistributionError> {
        let site = self.site.snapshot()?;
        let assignments = self
            .application
            .assignments(&application_declarations(&site, node_id));
        let projection =
            project_requested_node(&site, node_id)?.with_application_assignments(assignments)?;
        Ok(PublishedViewInput {
            site_id: site.site_id,
            projection,
        })
    }

    pub(crate) fn commit_acknowledgement(
        &self,
        authenticated_node_id: &str,
        request: &NodeViewAcknowledgement,
    ) -> Result<NodeViewAcknowledgementAcceptance, DesiredViewDistributionError> {
        validate_acknowledgement(request)?;
        let reference = request
            .view
            .as_ref()
            .ok_or(DesiredViewDistributionError::IdentityMismatch)?;
        if reference.node_id != authenticated_node_id
            || reference.manager_session_id != self.coordinator.manager_session().as_str()
        {
            return Err(DesiredViewDistributionError::IdentityMismatch);
        }
        let incarnation = NodeIncarnation::parse(reference.node_incarnation_id.clone())?;
        self.coordinator
            .acknowledge(
                authenticated_node_id,
                &incarnation,
                request,
                (reference.view_revision, &reference.content_sha256),
            )
            .map_err(Into::into)
    }

    pub(crate) fn commit_status(
        &self,
        authenticated_node_id: &str,
        request: &NodeControllerStatus,
    ) -> Result<NodeControllerStatusAcceptance, DesiredViewDistributionError> {
        let model = validate_controller_status(request)?;
        let reference = request
            .view
            .as_ref()
            .ok_or(DesiredViewDistributionError::IdentityMismatch)?;
        let incarnation = NodeIncarnation::parse(reference.node_incarnation_id.clone())?;
        // An invalid application section is dropped on its own, so it never blocks model status.
        let application = validate_application_status(request)
            .inspect_err(|error| {
                warn!(event_kind = "manager_application_status", outcome = "rejected", node_id = authenticated_node_id, status_sequence = request.status_sequence, error = %error, "Manager ignored an invalid node application status section");
            })
            .ok()
            .flatten();
        let result = self
            .coordinator
            .accept_status(authenticated_node_id, &incarnation, request, |projection| {
                let mut receipts = self
                    .model
                    .accept_remote_status(
                        authenticated_node_id,
                        projection,
                        model,
                        self.coordinator.manager_session(),
                        &incarnation,
                        request.status_sequence,
                    )
                    .map_err(DesiredViewDistributionError::ModelStatus)?;
                if let Some(application) = application {
                    receipts.extend(
                        self.application
                            .accept_remote_status(
                                authenticated_node_id,
                                projection,
                                application,
                                self.coordinator.manager_session(),
                                &incarnation,
                                request.status_sequence,
                            )
                            .into_iter()
                            .map(|receipt| ControllerStatusReceipt {
                                payload: Some(controller_status_receipt::Payload::Application(
                                    receipt,
                                )),
                            }),
                    );
                }
                Ok(NodeControllerStatusAcceptance {
                    accepted_status_sequence: request.status_sequence,
                    receipts,
                })
            })
            .map_err(DesiredViewDistributionError::from_coordinator);
        match &result {
            Err(DesiredViewDistributionError::ModelStatus(ControllerStatusError::Rejected(
                error,
            ))) => {
                debug!(event_kind = "manager_model_status", outcome = "rejected", node_id = authenticated_node_id, status_sequence = request.status_sequence, error = %dotcore_domain::diagnostic::diagnostic_chain(error.as_ref()), "Manager rejected node model status");
            }
            Err(DesiredViewDistributionError::ModelStatus(error)) => {
                warn!(event_kind = "manager_model_status", outcome = "retryable", node_id = authenticated_node_id, status_sequence = request.status_sequence, error = %dotcore_domain::diagnostic::diagnostic_chain(error), "Manager could not record node model status");
            }
            _ => {}
        }
        result
    }
}

/// Manager owner that validates and durably accepts one complete node model status from the
/// authenticated `SiteNode` `node_id`, returning one disposition per cleanup evidence entry.
///
/// Acceptance runs under coordinator custody and may take the site lock and then the
/// current-state lock, never the reverse.
pub trait ModelStatusOwner: Send + Sync {
    fn accept_remote_status(
        &self,
        node_id: &str,
        projection: &NodeViewProjection,
        model: &ModelControllerStatusSection,
        manager_session: &ManagerSession,
        node_incarnation: &NodeIncarnation,
        status_sequence: u64,
    ) -> Result<Vec<ControllerStatusReceipt>, ControllerStatusError>;
}

/// A status owner's refusal of one node status report.
#[derive(Debug, thiserror::Error)]
pub enum ControllerStatusError {
    /// The report contradicts current manager intent; resending the same report cannot succeed.
    #[error("node controller status was rejected")]
    Rejected(#[source] anyhow::Error),

    /// The report could not be recorded now, or its recording outcome is uncertain; the same
    /// report may be retried.
    #[error("node controller status could not be recorded")]
    Retryable(#[source] anyhow::Error),
}

/// Apps-controller owner of the application section of node views and of the application
/// status those nodes report.
///
/// Both methods run under coordinator custody. Status acceptance may take the site lock and then
/// the current-state lock, never the reverse, and answers failures per cleanup evidence entry, so
/// application status never rejects a report or blocks model status.
pub trait ApplicationNodeViewOwner: Send + Sync {
    /// Builds one complete assignment per declaration placed on the node, attaching the current
    /// launch projection of each linked route.
    fn assignments(
        &self,
        declarations: &[ApplicationDeploymentDeclaration],
    ) -> Vec<ApplicationAssignment>;

    /// Accepts one validated application status section from the authenticated `SiteNode`
    /// `node_id` and returns at most one disposition per cleanup evidence entry.
    fn accept_remote_status(
        &self,
        node_id: &str,
        projection: &NodeViewProjection,
        application: &ApplicationControllerStatusSection,
        manager_session: &ManagerSession,
        node_incarnation: &NodeIncarnation,
        status_sequence: u64,
    ) -> Vec<ApplicationCleanupReceipt>;
}

/// Node apps-controller owner of the application section of node status reports.
///
/// A section that cannot be built is omitted from that report, and every failure is the owner's
/// own, so application status never blocks model status.
pub trait NodeApplicationStatusOwner: Send + Sync {
    /// Builds the complete application section for the installed view, attributing accelerator
    /// memory from `collection` only, or `None` when it cannot be built now.
    fn status_section(
        &self,
        collection: &HardwareCollection,
    ) -> Option<ApplicationControllerStatusSection>;

    /// Marks the evidence the manager acknowledged, keyed by deployment and lifecycle generation,
    /// so it is never reported again.
    fn accept_receipts(
        &self,
        acknowledged: &std::collections::BTreeSet<(String, u64)>,
    ) -> Result<(), StoreError>;

    /// Deletes acknowledged evidence whose teardown the installed view no longer carries.
    fn retire_withdrawn_cleanup_evidence(&self);
}

/// Node inference-controller owner of the model section of node status reports.
///
/// The model section is required: a report is sent only when the owner can build it.
pub trait NodeModelStatusOwner: Send + Sync {
    /// Builds the complete model section for the installed view, attributing accelerator memory
    /// from `collection` only, or `None` when it cannot be built now.
    fn status_section(
        &self,
        collection: &HardwareCollection,
    ) -> Option<ModelControllerStatusSection>;

    /// Marks the cleanup evidence the manager acknowledged, keyed by deployment and lifecycle
    /// generation, so it is never reported again.
    fn accept_receipts(
        &self,
        acknowledged: &std::collections::BTreeSet<(String, u64)>,
    ) -> Result<(), StoreError>;

    /// Deletes acknowledged evidence whose teardown the installed view no longer carries.
    fn retire_withdrawn_cleanup_evidence(&self);

    /// Registers `observer`, called after each change of what the node serves, so the change is
    /// reported without waiting for the next view publication.
    fn register_change_observer(&self, observer: Arc<dyn Fn() + Send + Sync>);
}

/// Owner rules the node installer applies to the assignments of each accepted view, supplied by
/// composition so the site names no owner.
#[derive(Clone, Copy)]
pub struct NodeViewOwnerRules {
    /// Whether a model assignment carries valid portable model intent; one invalid assignment
    /// rejects the whole view.
    pub valid_model_assignment: fn(&ModelDeploymentDeclaration) -> bool,

    /// Classifies one application assignment for the hosting node; a failure is only that
    /// deployment's own.
    pub classify_application_assignment: fn(
        &str,
        &[ExecutionTarget],
        &ApplicationAssignment,
    ) -> Result<(), ApplicationStatusFailureCode>,
}

/// Node-local atomic installer sharing one stable projection handle with its model realizer.
#[derive(Clone)]
pub struct NodeViewInstaller {
    authority: AcceptedNodeViewAuthority,
    projected: crate::view::fencing::AcceptedModelOwnerInput,
    installed_targets: Vec<ExecutionTarget>,
    node_id: String,
    rules: NodeViewOwnerRules,
}

impl NodeViewInstaller {
    pub fn new(
        authority: AcceptedNodeViewAuthority,
        installed_targets: &[ExecutionTarget],
        rules: NodeViewOwnerRules,
    ) -> Self {
        let mut installed_targets = installed_targets
            .iter()
            .filter(|target| projected_target(target))
            .cloned()
            .collect::<Vec<_>>();
        installed_targets
            .sort_by(|left, right| left.execution_target_id.cmp(&right.execution_target_id));
        let projected = authority.model_input();
        let node_id = authority.expected_node_id().to_owned();
        Self {
            authority,
            projected,
            installed_targets,
            node_id,
            rules,
        }
    }

    pub fn projected_input(&self) -> crate::view::fencing::AcceptedModelOwnerInput {
        self.projected.clone()
    }

    /// Installs one view published on the node control stream `stream`, opened with `open`,
    /// atomically. Invalid model intent or a target mismatch rejects the view; application
    /// assignments are installed as sent, and each invalid one becomes only its own deployment's
    /// typed failure. Each accepted publication, an unchanged one included, renews action
    /// freshness.
    pub(crate) fn install_publication(
        &self,
        open: &NodeControlOpen,
        stream: &NodeViewStream,
        view: &NodeDesiredView,
    ) -> Result<ViewAcceptance, DesiredViewDistributionError> {
        let validated = validate_published_view(open, stream, view)?;
        self.check_owner_rules(&validated)?;
        let acceptance =
            self.authority
                .accept_publication(stream, validated.identity, &validated.projection)?;
        self.warn_on_installation(acceptance, &validated.projection);
        Ok(acceptance)
    }

    /// Rejects a whole view carrying invalid portable model intent or other execution targets
    /// than the node installed.
    fn check_owner_rules(
        &self,
        validated: &ValidatedNodeView,
    ) -> Result<(), DesiredViewDistributionError> {
        if validated
            .projection
            .model_deployments()
            .iter()
            .any(|deployment| !(self.rules.valid_model_assignment)(deployment))
        {
            return Err(DesiredViewDistributionError::InvalidPortableModelIntent);
        }
        if validated.projection.execution_targets() != self.installed_targets {
            return Err(DesiredViewDistributionError::InstalledTargetMismatch);
        }
        Ok(())
    }

    fn warn_on_installation(&self, acceptance: ViewAcceptance, projection: &NodeViewProjection) {
        if acceptance == ViewAcceptance::Installed {
            warn_invalid_application_assignments(
                &self.node_id,
                projection,
                self.rules.classify_application_assignment,
            );
        }
    }
}

/// Reports each installed application assignment that is only its own deployment's failure.
fn warn_invalid_application_assignments(
    node_id: &str,
    projection: &NodeViewProjection,
    classify: fn(
        &str,
        &[ExecutionTarget],
        &ApplicationAssignment,
    ) -> Result<(), ApplicationStatusFailureCode>,
) {
    for assignment in projection.application_assignments() {
        if let Err(failure_code) = classify(node_id, projection.execution_targets(), assignment) {
            let declaration = assignment.declaration.as_ref();
            warn!(
                event_kind = "node_application_assignment_invalid",
                deployment_id = declaration.map_or("", |declaration| declaration
                    .application_deployment_id
                    .as_str()),
                lifecycle_generation =
                    declaration.map_or(0, |declaration| declaration.lifecycle_generation),
                failure_code = ?failure_code,
                "Node installed a view whose application assignment fails only its own deployment"
            );
        }
    }
}

/// Projects why the node rejected a published view to the code its acknowledgement carries.
pub(crate) fn rejection_code(error: &DesiredViewDistributionError) -> DesiredViewRejectionCode {
    match error {
        DesiredViewDistributionError::IdentityMismatch
        | DesiredViewDistributionError::NodeView(_) => DesiredViewRejectionCode::IdentityMismatch,
        DesiredViewDistributionError::InstalledTargetMismatch => {
            DesiredViewRejectionCode::SectionPayloadInvalid
        }
        DesiredViewDistributionError::Contract(
            crate::view::contract::DesiredViewContractError::LimitExceeded,
        ) => DesiredViewRejectionCode::LimitExceeded,
        DesiredViewDistributionError::Contract(_) => DesiredViewRejectionCode::ContentConflict,
        _ => DesiredViewRejectionCode::SectionPayloadInvalid,
    }
}

/// Logs why the manager rejected one node control `exchange` and projects it to the node's status.
/// A fenced node retries on its backoff, so the reason is logged at debug level; the node logs the
/// returned message itself.
pub(crate) fn rejected_exchange(
    node_id: &str,
    exchange: &'static str,
    error: DesiredViewDistributionError,
) -> Status {
    debug!(
        event_kind = "manager_node_control_exchange",
        outcome = "rejected",
        node_id,
        exchange,
        error = %error,
        "Manager rejected a node control exchange"
    );
    Status::from(error)
}

/// Projects one node's model and application targets with its model intent; application intent
/// is added from the apps-controller owner's assignments.
fn project_requested_node(
    site: &Site,
    node_id: &str,
) -> Result<NodeViewProjection, DesiredViewDistributionError> {
    let base = NodeViewProjection::for_site_node(site, node_id)?;
    let targets = base
        .execution_targets()
        .iter()
        .filter(|target| projected_target(target))
        .cloned()
        .collect();
    NodeViewProjection::new(targets, base.model_deployments().to_vec()).map_err(Into::into)
}

/// Application declarations placed on one `SiteNode`, in site order.
fn application_declarations(site: &Site, node_id: &str) -> Vec<ApplicationDeploymentDeclaration> {
    site.application_deployments
        .iter()
        .filter(|deployment| {
            deployment
                .placement
                .as_ref()
                .is_some_and(|placement| placement.site_node_id == node_id)
        })
        .cloned()
        .collect()
}

/// Whether an execution target belongs in node views: it binds a model or application runtime.
fn projected_target(target: &ExecutionTarget) -> bool {
    matches!(
        target.runtime_binding,
        Some(
            execution_target::RuntimeBinding::ModelRuntime(_)
                | execution_target::RuntimeBinding::ApplicationRuntime(_)
        )
    )
}

/// Builds one complete manager-authored node view carried by stream publication
/// `publication_sequence`.
pub(crate) fn node_desired_view(
    manager_session: &ManagerSession,
    node_incarnation: &NodeIncarnation,
    publication_sequence: u64,
    view_revision: u64,
    projection: &NodeViewProjection,
    site_id: &str,
    node_id: &str,
) -> NodeDesiredView {
    let digest = projection.digest();
    let assignments = projection
        .model_deployments()
        .iter()
        .cloned()
        .map(|declaration| ModelAssignment {
            declaration: Some(declaration),
        })
        .collect();
    let mut required_sections = vec![RequiredDesiredViewSection {
        kind: DesiredViewSectionKind::Model as i32,
        schema_version: MODEL_SECTION_SCHEMA_VERSION,
    }];
    let mut sections = vec![DesiredViewSection {
        kind: DesiredViewSectionKind::Model as i32,
        schema_version: MODEL_SECTION_SCHEMA_VERSION,
        payload: Some(desired_view_section::Payload::Model(
            ModelDesiredViewSection { assignments },
        )),
    }];
    // A node with an application target, or application intent, always receives an explicit
    // application section, even when empty, so an empty one removes only application intent.
    let application_capable = !projection.application_assignments().is_empty()
        || projection.execution_targets().iter().any(|target| {
            matches!(
                target.runtime_binding,
                Some(execution_target::RuntimeBinding::ApplicationRuntime(_))
            )
        });
    if application_capable {
        required_sections.push(RequiredDesiredViewSection {
            kind: DesiredViewSectionKind::Application as i32,
            schema_version: APPLICATION_SECTION_SCHEMA_VERSION,
        });
        sections.push(DesiredViewSection {
            kind: DesiredViewSectionKind::Application as i32,
            schema_version: APPLICATION_SECTION_SCHEMA_VERSION,
            payload: Some(desired_view_section::Payload::Application(
                ApplicationDesiredViewSection {
                    assignments: projection.application_assignments().to_vec(),
                },
            )),
        });
    }
    NodeDesiredView {
        identity: Some(NodeViewIdentity {
            site_id: site_id.to_owned(),
            node_id: node_id.to_owned(),
            manager_session_id: manager_session.as_str().to_owned(),
            node_incarnation_id: node_incarnation.as_str().to_owned(),
            publication_sequence,
            view_revision,
            content_sha256: digest.as_str().to_owned(),
            projection_schema_version: NODE_VIEW_PROJECTION_SCHEMA_VERSION,
        }),
        required_sections,
        sections,
        execution_targets: projection.execution_targets().to_vec(),
    }
}

/// Rejects a candidate site that would grow any `SiteNode`'s desired view past the limits every
/// node enforces on its view, so an accepted mutation never yields a view that node must refuse.
/// A view already over a limit may still shrink or stay unchanged, and a lifecycle transition such
/// as a teardown never grows the measured view.
pub(crate) fn reject_node_view_growth(
    current: &Site,
    candidate: &Site,
) -> Result<(), crate::repository::SiteRepositoryError> {
    for node in &candidate.nodes {
        let Some(grown) = node_view_size(candidate, &node.node_id) else {
            continue;
        };
        if grown.within_limits() {
            continue;
        }
        if node_view_size(current, &node.node_id).is_some_and(|retained| !grown.grows(&retained)) {
            continue;
        }
        warn!(
            event_kind = "node_view_limit_rejected",
            node_id = %node.node_id,
            execution_targets = grown.execution_targets,
            model_assignments = grown.model_assignments,
            application_assignments = grown.application_assignments,
            bytes = grown.bytes,
            "Site mutation rejected because it grows a node desired view past the node-view limits"
        );
        return Err(
            crate::repository::SiteRepositoryError::NodeViewLimitExceeded {
                node_id: node.node_id.clone(),
            },
        );
    }
    Ok(())
}

/// Wire-relevant size of one node's desired view.
#[derive(Clone, Copy)]
struct NodeViewSize {
    execution_targets: usize,
    model_assignments: usize,
    application_assignments: usize,
    bytes: usize,
}

impl NodeViewSize {
    fn within_limits(&self) -> bool {
        self.execution_targets <= MAX_EXECUTION_TARGETS
            && self.model_assignments <= MAX_MODEL_ASSIGNMENTS
            && self.application_assignments <= MAX_APPLICATION_ASSIGNMENTS
            && self.bytes <= MAX_NODE_VIEW_BYTES
    }

    fn grows(&self, retained: &Self) -> bool {
        self.execution_targets > retained.execution_targets
            || self.model_assignments > retained.model_assignments
            || self.application_assignments > retained.application_assignments
            || self.bytes > retained.bytes
    }
}

/// Measures the largest published view of one node, using maximum-width
/// identities, sequences, lifecycle fields, and linked-route names; `None` when the site does not
/// project for that node. The measurement is an upper bound that no lifecycle transition or
/// route rename changes.
fn node_view_size(site: &Site, node_id: &str) -> Option<NodeViewSize> {
    let requested = project_requested_node(site, node_id).ok()?;
    let projection = NodeViewProjection::new(
        requested.execution_targets().to_vec(),
        requested
            .model_deployments()
            .iter()
            .cloned()
            .map(with_widest_lifecycle)
            .collect(),
    )
    .ok()?
    .with_application_assignments(
        application_declarations(site, node_id)
            .into_iter()
            .map(widest_application_assignment)
            .collect(),
    )
    .ok()?;
    let manager_session =
        ManagerSession::parse("f".repeat(32)).expect("fixed-width manager session is valid");
    let node_incarnation =
        NodeIncarnation::parse("f".repeat(32)).expect("fixed-width node incarnation is valid");
    let view = node_desired_view(
        &manager_session,
        &node_incarnation,
        u64::MAX,
        u64::MAX,
        &projection,
        &site.site_id,
        node_id,
    );
    Some(NodeViewSize {
        execution_targets: projection.execution_targets().len(),
        model_assignments: projection.model_deployments().len(),
        application_assignments: projection.application_assignments().len(),
        bytes: view.encoded_len(),
    })
}

/// Sets the only fields a lifecycle transition changes to their widest encoding: the largest
/// generation, the recovery flag, and non-default transition and desired state, each of which
/// encodes in one byte.
fn with_widest_lifecycle(
    mut declaration: ModelDeploymentDeclaration,
) -> ModelDeploymentDeclaration {
    declaration.lifecycle_generation = u64::MAX;
    declaration.lifecycle_transition = ModelDeploymentLifecycleTransition::Teardown as i32;
    declaration.desired_state = ModelDeploymentDesiredState::Teardown as i32;
    declaration.lifecycle_recovery_required = true;
    declaration
}

/// Builds the widest assignment one application declaration can produce: the largest lifecycle
/// generation, a non-default desired state, and a linked route name of the maximum identifier
/// length, which is wider than the unknown-route marker.
fn widest_application_assignment(
    mut declaration: ApplicationDeploymentDeclaration,
) -> ApplicationAssignment {
    declaration.lifecycle_generation = u64::MAX;
    declaration.desired_state = ApplicationDeploymentDesiredState::Teardown as i32;
    let linked_route =
        declaration
            .inference_route_id
            .is_some()
            .then(|| ApplicationLinkedRouteProjection {
                resolution: Some(application_linked_route_projection::Resolution::RouteName(
                    "r".repeat(MAX_ROUTE_NAME_BYTES),
                )),
            });
    ApplicationAssignment {
        declaration: Some(declaration),
        linked_route,
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum DesiredViewDistributionError {
    #[error("desired-view peer or manager session identity does not match")]
    IdentityMismatch,

    #[error("node is not declared in this site")]
    UndeclaredNode,

    #[error("desired-view model intent is not a valid portable retained snapshot")]
    InvalidPortableModelIntent,

    #[error("desired-view target configuration does not match the immutable installed target")]
    InstalledTargetMismatch,

    #[error("desired-view lifecycle authority is unavailable")]
    LifecycleState(#[from] StoreError),

    #[error("the node's first view cannot be built and none was published before")]
    FirstViewUnavailable(#[source] Box<DesiredViewDistributionError>),

    #[error("controller status owner did not accept the report")]
    ModelStatus(#[source] ControllerStatusError),

    #[error(transparent)]
    Site(#[from] crate::repository::SiteRepositoryError),

    #[error(transparent)]
    NodeView(#[from] crate::view::fencing::NodeViewError),

    #[error(transparent)]
    Contract(#[from] crate::view::contract::DesiredViewContractError),

    #[error(transparent)]
    Incarnation(#[from] crate::view::fencing::IncarnationFenceError),

    #[error(transparent)]
    ProcessIdentity(#[from] crate::view::fencing::ProcessIdentityError),
}
impl DesiredViewDistributionError {
    fn from_coordinator(error: CoordinatorExchangeError<Self>) -> Self {
        match error {
            CoordinatorExchangeError::Fence(error) => Self::Incarnation(error),
            CoordinatorExchangeError::Owner(error) => error,
        }
    }
}

impl From<DesiredViewDistributionError> for Status {
    fn from(error: DesiredViewDistributionError) -> Self {
        match error {
            DesiredViewDistributionError::IdentityMismatch
            | DesiredViewDistributionError::Incarnation(_)
            | DesiredViewDistributionError::ProcessIdentity(_) => Status::failed_precondition(
                format!("desired-view fencing rejected the request: {error}"),
            ),
            DesiredViewDistributionError::InvalidPortableModelIntent
            | DesiredViewDistributionError::InstalledTargetMismatch
            | DesiredViewDistributionError::NodeView(_)
            | DesiredViewDistributionError::Contract(_)
            | DesiredViewDistributionError::ModelStatus(ControllerStatusError::Rejected(_)) => {
                Status::invalid_argument("desired-view input was rejected")
            }
            DesiredViewDistributionError::ModelStatus(ControllerStatusError::Retryable(_)) => {
                Status::unavailable("controller status could not be recorded; retry the report")
            }
            DesiredViewDistributionError::UndeclaredNode => {
                Status::permission_denied("node is not declared in this site")
            }
            DesiredViewDistributionError::FirstViewUnavailable(_) => {
                Status::unavailable("the node's view cannot be built now; reopen the stream later")
            }
            DesiredViewDistributionError::LifecycleState(_)
            | DesiredViewDistributionError::Site(_) => {
                Status::unavailable("desired-view authority is temporarily unavailable")
            }
        }
    }
}

#[cfg(test)]
mod tests;
