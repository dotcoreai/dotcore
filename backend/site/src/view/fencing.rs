//! Defines process-lifetime fencing identities, normalized node-view identity, and node action
//! authorization before the private synchronization transport exists.
//!
//! Manager and node composition own random process identities, while complete validated node
//! projections renew short monotonic leases that controllers check before each runtime mutation.

use std::{
    collections::BTreeSet,
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

use dotcore_domain::{deployment::DeploymentOwner, valid_id};
use dotcore_proto::dotcore::{
    application::private::v1::ApplicationAssignment,
    model::v1::{ModelDeploymentDeclaration, ModelDeploymentDesiredState},
    site::v1::{ExecutionTarget, Site},
};
use prost::Message;
use sha2::{Digest, Sha256};
use tokio::{sync::Notify, time::Instant};

const PROCESS_ID_BYTES: usize = 16;
const NODE_VIEW_SCHEMA_VERSION: u32 = 1;

/// Random identity shared by every lifecycle channel from one node process.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeIncarnation(String);

impl NodeIncarnation {
    /// Generates one process-lifetime node identity from operating-system entropy.
    pub fn generate() -> Result<Self, ProcessIdentityError> {
        random_process_identity().map(Self)
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, ProcessIdentityError> {
        let value = value.into();
        validate_process_identity(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Random identity owned by one manager process session.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ManagerSession(String);

impl ManagerSession {
    /// Generates one process-lifetime manager identity from operating-system entropy.
    pub fn generate() -> Result<Self, ProcessIdentityError> {
        random_process_identity().map(Self)
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, ProcessIdentityError> {
        let value = value.into();
        validate_process_identity(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Complete semantic controller input projected for one node.
///
/// Model declarations must be placed on included targets, because an invalid model intent rejects
/// the whole view. Application assignments are kept exactly as their owner built them: each one
/// is validated on its own by the node, so one invalid assignment fails only its deployment.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeViewProjection {
    execution_targets: Vec<ExecutionTarget>,
    model_deployments: Vec<ModelDeploymentDeclaration>,
    application_assignments: Vec<ApplicationAssignment>,
}

impl NodeViewProjection {
    /// Projects the concrete targets and model owner inputs assigned to one SiteNode. Application
    /// assignments carry apps-controller route projections, so their owner adds them separately.
    pub fn for_site_node(site: &Site, node_id: &str) -> Result<Self, NodeViewError> {
        if !valid_id(&site.site_id) || !valid_id(node_id) {
            return Err(NodeViewError::InvalidIdentity);
        }
        let execution_targets = site
            .execution_targets
            .iter()
            .filter(|target| target.site_node_id == node_id)
            .cloned()
            .collect();
        let model_deployments = site
            .model_deployments
            .iter()
            .filter(|deployment| {
                deployment
                    .placement
                    .as_ref()
                    .is_some_and(|placement| placement.site_node_id == node_id)
            })
            .cloned()
            .collect();
        Self::new(execution_targets, model_deployments)
    }

    /// Builds a normalized projection with unique identities and placements on its own targets.
    pub fn new(
        mut execution_targets: Vec<ExecutionTarget>,
        mut model_deployments: Vec<ModelDeploymentDeclaration>,
    ) -> Result<Self, NodeViewError> {
        reject_duplicate_identities(
            execution_targets
                .iter()
                .map(|target| target.execution_target_id.as_str()),
        )?;
        reject_duplicate_identities(
            model_deployments
                .iter()
                .map(|deployment| deployment.model_deployment_id.as_str()),
        )?;
        let target_ids = execution_targets
            .iter()
            .map(|target| target.execution_target_id.as_str())
            .collect::<BTreeSet<_>>();
        if model_deployments.iter().any(|deployment| {
            deployment.placement.as_ref().is_none_or(|placement| {
                !target_ids.contains(placement.execution_target_id.as_str())
            })
        }) {
            return Err(NodeViewError::InvalidPlacement);
        }
        execution_targets
            .sort_by(|left, right| left.execution_target_id.cmp(&right.execution_target_id));
        model_deployments
            .sort_by(|left, right| left.model_deployment_id.cmp(&right.model_deployment_id));
        Ok(Self {
            execution_targets,
            model_deployments,
            application_assignments: Vec::new(),
        })
    }

    /// Replaces the application section with complete owner-built assignments, normalized by
    /// deployment identity. Only an assignment without a valid, unique deployment identity is
    /// refused, since no other problem can be attributed to one deployment.
    pub fn with_application_assignments(
        mut self,
        mut assignments: Vec<ApplicationAssignment>,
    ) -> Result<Self, NodeViewError> {
        if assignments
            .iter()
            .any(|assignment| assignment.declaration.is_none())
        {
            return Err(NodeViewError::InvalidIdentity);
        }
        reject_duplicate_identities(assignments.iter().map(application_assignment_id))?;
        assignments.sort_by(|left, right| {
            application_assignment_id(left).cmp(application_assignment_id(right))
        });
        self.application_assignments = assignments;
        Ok(self)
    }

    pub fn execution_targets(&self) -> &[ExecutionTarget] {
        &self.execution_targets
    }

    pub fn model_deployments(&self) -> &[ModelDeploymentDeclaration] {
        &self.model_deployments
    }

    pub fn application_assignments(&self) -> &[ApplicationAssignment] {
        &self.application_assignments
    }

    /// Reports whether the accepted view still carries the exact assignment an action was
    /// authorized for: the same owner section, deployment identity, and assignment digest.
    pub(crate) fn contains_assignment(&self, assignment: &ActionAssignment) -> bool {
        match assignment.owner {
            DeploymentOwner::Model => self
                .model_deployments
                .binary_search_by(|candidate| {
                    candidate
                        .model_deployment_id
                        .as_str()
                        .cmp(&assignment.deployment_id)
                })
                .is_ok_and(|index| {
                    ActionAssignment::from(&self.model_deployments[index]) == *assignment
                }),
            DeploymentOwner::Application => self
                .application_assignments
                .binary_search_by(|candidate| {
                    application_assignment_id(candidate).cmp(&assignment.deployment_id)
                })
                .is_ok_and(|index| {
                    ActionAssignment::from(&self.application_assignments[index]) == *assignment
                }),
        }
    }

    /// Reports whether one owner section assigns `deployment_id` at all.
    fn assigns(&self, owner: DeploymentOwner, deployment_id: &str) -> bool {
        match owner {
            DeploymentOwner::Model => self
                .model_deployments
                .iter()
                .any(|deployment| deployment.model_deployment_id == deployment_id),
            DeploymentOwner::Application => self
                .application_assignments
                .iter()
                .any(|assignment| application_assignment_id(assignment) == deployment_id),
        }
    }

    pub(crate) fn digest(&self) -> NodeViewDigest {
        let mut hasher = Sha256::new();
        hasher.update(b"dotcore.node-view.semantic.v1\0");
        hasher.update(NODE_VIEW_SCHEMA_VERSION.to_be_bytes());
        hash_messages(&mut hasher, b"execution-target\0", &self.execution_targets);
        hash_messages(&mut hasher, b"model-deployment\0", &self.model_deployments);
        // An empty application section adds nothing, so a view without application intent has
        // one digest whether its application section is explicit or absent.
        if !self.application_assignments.is_empty() {
            hash_messages(
                &mut hasher,
                b"application-assignment\0",
                &self.application_assignments,
            );
        }
        NodeViewDigest(hex::encode(hasher.finalize()))
    }
}

/// Owner-neutral identity of one accepted assignment that a runtime action acts for: the owner
/// section, the deployment identity, and a digest of the complete assignment. Changes to another
/// deployment, or to another owner's section, never change it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionAssignment {
    owner: DeploymentOwner,
    deployment_id: String,
    digest: [u8; 32],
}

impl ActionAssignment {
    fn new(owner: DeploymentOwner, deployment_id: &str, message: &impl Message) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(match owner {
            DeploymentOwner::Model => b"dotcore.action-assignment.model.v1\0".as_slice(),
            DeploymentOwner::Application => {
                b"dotcore.action-assignment.application.v1\0".as_slice()
            }
        });
        hash_bytes(&mut hasher, &message.encode_to_vec());
        Self {
            owner,
            deployment_id: deployment_id.to_owned(),
            digest: hasher.finalize().into(),
        }
    }
}

/// Digests a model declaration without its node-resolved launch identities. The node produced
/// them itself and the manager only records them, so recording one is not new intent: it never
/// invalidates an action in flight, and the node's own pin and the projected one digest alike.
impl From<&ModelDeploymentDeclaration> for ActionAssignment {
    fn from(declaration: &ModelDeploymentDeclaration) -> Self {
        let intent = ModelDeploymentDeclaration {
            node_resolved_launch_identities: Vec::new(),
            ..declaration.clone()
        };
        Self::new(
            DeploymentOwner::Model,
            &declaration.model_deployment_id,
            &intent,
        )
    }
}

/// Digests an application assignment without its declaration's node-resolved launch
/// identities, for the same reason as a model declaration: recording a node's own pin is not
/// new intent and never invalidates an action in flight.
impl From<&ApplicationAssignment> for ActionAssignment {
    fn from(assignment: &ApplicationAssignment) -> Self {
        let mut intent = assignment.clone();
        if let Some(declaration) = intent.declaration.as_mut() {
            declaration.node_resolved_launch_identities.clear();
        }
        Self::new(
            DeploymentOwner::Application,
            application_assignment_id(assignment),
            &intent,
        )
    }
}

/// Domain-separated digest of normalized node-view semantics, excluding envelope identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NodeViewDigest(String);

impl NodeViewDigest {
    pub(crate) fn parse(value: impl Into<String>) -> Result<Self, NodeViewError> {
        let value = value.into();
        if !valid_sha256(&value) {
            return Err(NodeViewError::InvalidContentDigest);
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Identity of one view publication on a node control stream: the manager session that published
/// it, the node incarnation it was published to, and its sequence within the stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeViewPublicationIdentity {
    manager_session: ManagerSession,
    node_incarnation: NodeIncarnation,
    sequence: u64,
}

impl NodeViewPublicationIdentity {
    pub(crate) fn new(
        manager_session: ManagerSession,
        node_incarnation: NodeIncarnation,
        sequence: u64,
    ) -> Result<Self, NodeViewError> {
        if sequence == 0 {
            return Err(NodeViewError::IdentityMismatch);
        }
        Ok(Self {
            manager_session,
            node_incarnation,
            sequence,
        })
    }
}

/// Envelope identity for one complete manager-authored node view publication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedNodeViewIdentity {
    pub(crate) site_id: String,
    pub(crate) node_id: String,
    pub(crate) publication: NodeViewPublicationIdentity,
    pub(crate) revision: u64,
    pub(crate) content_digest: NodeViewDigest,
}

impl AcceptedNodeViewIdentity {
    pub(crate) fn new(
        site_id: impl Into<String>,
        node_id: impl Into<String>,
        publication: NodeViewPublicationIdentity,
        revision: u64,
        content_digest: NodeViewDigest,
    ) -> Self {
        Self {
            site_id: site_id.into(),
            node_id: node_id.into(),
            publication,
            revision,
            content_digest,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_projection(
        site_id: impl Into<String>,
        node_id: impl Into<String>,
        publication: NodeViewPublicationIdentity,
        revision: u64,
        projection: &NodeViewProjection,
    ) -> Self {
        Self::new(site_id, node_id, publication, revision, projection.digest())
    }
}

/// One node control stream's claim on view installation, from the node's side: only the
/// stream opened last installs publications, and each of its publications must carry a higher
/// sequence than the one before.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeViewStream {
    id: u64,
    manager_session: ManagerSession,
    node_incarnation: NodeIncarnation,
}

impl NodeViewStream {
    /// Manager session that answered the stream's open.
    pub fn manager_session(&self) -> &ManagerSession {
        &self.manager_session
    }

    /// Node incarnation that holds the stream.
    pub fn node_incarnation(&self) -> &NodeIncarnation {
        &self.node_incarnation
    }
}

/// Node-local accepted-view state and monotonic runtime-action lease.
#[derive(Clone)]
pub struct AcceptedNodeViewAuthority {
    expected_site_id: String,
    expected_node_id: String,
    node_incarnation: NodeIncarnation,
    lease_duration: Duration,
    state: Arc<Mutex<AcceptedViewState>>,
    listener: Arc<Notify>,
    application_listener: Arc<Notify>,
}

impl AcceptedNodeViewAuthority {
    pub fn new(
        expected_site_id: impl Into<String>,
        expected_node_id: impl Into<String>,
        node_incarnation: NodeIncarnation,
        manager_session: ManagerSession,
        lease_duration: Duration,
    ) -> Result<Self, NodeViewError> {
        let expected_site_id = expected_site_id.into();
        let expected_node_id = expected_node_id.into();
        if !valid_id(&expected_site_id) || !valid_id(&expected_node_id) || lease_duration.is_zero()
        {
            return Err(NodeViewError::InvalidIdentity);
        }
        Ok(Self {
            expected_site_id,
            expected_node_id,
            node_incarnation,
            lease_duration,
            state: Arc::new(Mutex::new(AcceptedViewState {
                manager_session,
                replaced_sessions: BTreeSet::new(),
                next_stream_id: 1,
                stream: None,
                accepted: None,
                lease_expires_at: None,
            })),
            listener: Arc::new(Notify::new()),
            application_listener: Arc::new(Notify::new()),
        })
    }

    /// Opens one node control stream with the authenticated manager session that answered it,
    /// adopting that session when it replaces the current one, which freezes old actions at once.
    ///
    /// Only publications of the stream opened last install, each with a higher sequence than the
    /// stream's previous one.
    pub fn open_stream(
        &self,
        manager_session: ManagerSession,
    ) -> Result<NodeViewStream, NodeViewError> {
        let mut state = self.state.lock().expect("accepted node view lock");
        if state.replaced_sessions.contains(&manager_session) {
            return Err(NodeViewError::ReplacedManagerSession);
        }
        if state.manager_session != manager_session {
            adopt_manager_session(&mut state, manager_session);
        }
        let id = state.next_stream_id;
        state.next_stream_id = id
            .checked_add(1)
            .expect("node control stream identity exhausted");
        state.stream = Some(StreamFence {
            id,
            accepted_publication: 0,
        });
        Ok(NodeViewStream {
            id,
            manager_session: state.manager_session.clone(),
            node_incarnation: self.node_incarnation.clone(),
        })
    }

    /// Validates and atomically installs one view published on `stream` before renewing action
    /// freshness, so an unchanged republished view keeps actions allowed.
    pub fn accept_publication(
        &self,
        stream: &NodeViewStream,
        identity: AcceptedNodeViewIdentity,
        projection: &NodeViewProjection,
    ) -> Result<ViewAcceptance, NodeViewError> {
        let mut state = self.state.lock().expect("accepted node view lock");
        self.check_envelope(&state, &identity)?;
        let fence = state
            .stream
            .as_mut()
            .filter(|fence| fence.id == stream.id)
            .ok_or(NodeViewError::ReplacedStream)?;
        if identity.publication.sequence <= fence.accepted_publication {
            return Err(NodeViewError::StalePublication {
                accepted: fence.accepted_publication,
                received: identity.publication.sequence,
            });
        }
        fence.accepted_publication = identity.publication.sequence;
        let (acceptance, ()) =
            install_view(&mut state, identity, projection, self.lease_duration, || ())?;
        drop(state);
        if acceptance == ViewAcceptance::Installed {
            self.listener.notify_one();
            self.application_listener.notify_one();
        }
        Ok(acceptance)
    }

    /// Process incarnation that holds this authority.
    pub(crate) fn node_incarnation(&self) -> &NodeIncarnation {
        &self.node_incarnation
    }

    /// SiteNode identity every accepted view must name.
    pub(crate) fn expected_node_id(&self) -> &str {
        &self.expected_node_id
    }

    pub fn model_input(&self) -> AcceptedModelOwnerInput {
        AcceptedModelOwnerInput {
            authority: self.clone(),
        }
    }

    pub fn application_input(&self) -> AcceptedApplicationOwnerInput {
        AcceptedApplicationOwnerInput {
            authority: self.clone(),
        }
    }

    /// Checks that a view names this node incarnation and the current manager session.
    fn check_envelope(
        &self,
        state: &AcceptedViewState,
        identity: &AcceptedNodeViewIdentity,
    ) -> Result<(), NodeViewError> {
        if identity.site_id != self.expected_site_id
            || identity.node_id != self.expected_node_id
            || identity.publication.node_incarnation != self.node_incarnation
            || identity.revision == 0
        {
            return Err(NodeViewError::IdentityMismatch);
        }
        if state
            .replaced_sessions
            .contains(&identity.publication.manager_session)
        {
            return Err(NodeViewError::ReplacedManagerSession);
        }
        if identity.publication.manager_session != state.manager_session {
            return Err(NodeViewError::ManagerSessionMismatch);
        }
        Ok(())
    }

    pub(crate) fn snapshot_with_permit(
        &self,
    ) -> Result<(AcceptedNodeViewSnapshot, RuntimeActionPermit), ActionPermitError> {
        let state = self.state.lock().expect("accepted node view lock");
        let accepted = fresh_accepted_view(&state)?;
        Ok((
            accepted.clone(),
            RuntimeActionPermit::Leased {
                #[cfg(any(test, feature = "test-support"))]
                manager_session: state.manager_session.clone(),
                #[cfg(any(test, feature = "test-support"))]
                view_revision: accepted.identity.revision,
            },
        ))
    }

    pub(crate) fn action_permit(&self) -> Result<RuntimeActionPermit, ActionPermitError> {
        self.snapshot_with_permit().map(|(_, permit)| permit)
    }

    pub(crate) fn accepted_view_reference(
        &self,
    ) -> Option<dotcore_proto::dotcore::site::private::v1::NodeViewReference> {
        let state = self.state.lock().expect("accepted node view lock");
        let accepted = state.accepted.as_ref()?;
        Some(
            dotcore_proto::dotcore::site::private::v1::NodeViewReference {
                site_id: accepted.identity.site_id.clone(),
                node_id: accepted.identity.node_id.clone(),
                manager_session_id: accepted
                    .identity
                    .publication
                    .manager_session
                    .as_str()
                    .to_owned(),
                node_incarnation_id: accepted
                    .identity
                    .publication
                    .node_incarnation
                    .as_str()
                    .to_owned(),
                view_revision: accepted.identity.revision,
                content_sha256: accepted.identity.content_digest.as_str().to_owned(),
            },
        )
    }

    /// Authorizes serving one exact deployment generation from the installed view.
    ///
    /// Serving ignores action-lease freshness, so a node whose view went stale keeps serving.
    /// Only another incarnation, another manager session, or an installed view that does not
    /// assign the deployment Present at that lifecycle generation refuses it.
    pub fn authorize_serving(
        &self,
        manager_session: &str,
        node_incarnation: &str,
        deployment_id: &str,
        lifecycle_generation: u64,
    ) -> Result<(), ServingAuthorizationError> {
        let state = self.authorize_manager_identity(manager_session, node_incarnation)?;
        // Replacing the manager session clears the installed view, so an installed view always
        // belongs to the current session.
        let assigned = state.accepted.as_ref().is_some_and(|accepted| {
            accepted
                .projection
                .model_deployments()
                .iter()
                .any(|deployment| {
                    deployment.model_deployment_id == deployment_id
                        && deployment.lifecycle_generation == lifecycle_generation
                        && deployment.desired_state == ModelDeploymentDesiredState::Present as i32
                })
        });
        if !assigned {
            return Err(ServingAuthorizationError::DeploymentNotAssigned);
        }
        Ok(())
    }

    /// Authorizes a manager request that names this node incarnation and the manager session
    /// whose view the node installed, without naming a deployment. Each deployment owner then
    /// checks the assignment in its own section of the installed view.
    pub fn authorize_manager_request(
        &self,
        manager_session: &str,
        node_incarnation: &str,
    ) -> Result<(), ServingAuthorizationError> {
        self.authorize_manager_identity(manager_session, node_incarnation)
            .map(drop)
    }

    /// Authorizes a manager request that names this node incarnation and the manager session
    /// whose view the node installed, and returns the locked view state.
    fn authorize_manager_identity(
        &self,
        manager_session: &str,
        node_incarnation: &str,
    ) -> Result<std::sync::MutexGuard<'_, AcceptedViewState>, ServingAuthorizationError> {
        if node_incarnation != self.node_incarnation.as_str() {
            return Err(ServingAuthorizationError::NodeIncarnationMismatch);
        }
        let state = self.state.lock().expect("accepted node view lock");
        if manager_session != state.manager_session.as_str() {
            return Err(ServingAuthorizationError::ManagerSessionMismatch);
        }
        Ok(state)
    }

    /// Activates a replacement manager session, which immediately freezes old actions.
    #[cfg(any(test, feature = "test-support"))]
    pub fn replace_manager_session(
        &self,
        manager_session: ManagerSession,
    ) -> Result<(), NodeViewError> {
        let mut state = self.state.lock().expect("accepted node view lock");
        if state.replaced_sessions.contains(&manager_session) {
            return Err(NodeViewError::ReplacedManagerSession);
        }
        if state.manager_session == manager_session {
            return Ok(());
        }
        adopt_manager_session(&mut state, manager_session);
        Ok(())
    }

    /// Installs `projection` as view `revision` of the current manager session through a newly
    /// opened stream, as its first publication.
    #[cfg(any(test, feature = "test-support"))]
    pub fn install_for_test(
        &self,
        site_id: impl Into<String>,
        node_id: impl Into<String>,
        revision: u64,
        projection: &NodeViewProjection,
    ) -> Result<ViewAcceptance, NodeViewError> {
        let manager_session = self
            .state
            .lock()
            .expect("accepted node view lock")
            .manager_session
            .clone();
        let stream = self.open_stream(manager_session)?;
        let publication = NodeViewPublicationIdentity::new(
            stream.manager_session.clone(),
            stream.node_incarnation.clone(),
            1,
        )?;
        self.accept_publication(
            &stream,
            AcceptedNodeViewIdentity::new(
                site_id,
                node_id,
                publication,
                revision,
                projection.digest(),
            ),
            projection,
        )
    }
}

/// Read-only node access to the model section of the installed node view.
#[derive(Clone)]
pub struct AcceptedModelOwnerInput {
    authority: AcceptedNodeViewAuthority,
}

impl AcceptedModelOwnerInput {
    /// Returns the installed model section with its site identity and execution targets, or
    /// fails before a view is installed.
    pub fn declaration_snapshot(
        &self,
    ) -> Result<crate::repository::ModelSiteSnapshot, crate::repository::SiteRepositoryError> {
        let state = self
            .authority
            .state
            .lock()
            .expect("accepted node view lock");
        let accepted = state
            .accepted
            .as_ref()
            .ok_or(crate::repository::SiteRepositoryError::NoAcceptedView)?;
        Ok(crate::repository::ModelSiteSnapshot {
            site_id: accepted.identity.site_id.clone(),
            execution_targets: accepted.projection.execution_targets().to_vec(),
            deployments: accepted.projection.model_deployments().to_vec(),
        })
    }

    /// Returns the listener woken when a new view is installed.
    pub fn register_change_listener(&self) -> Arc<Notify> {
        Arc::clone(&self.authority.listener)
    }
}

/// Read-only node access to the application section of the installed node view.
#[derive(Clone)]
pub struct AcceptedApplicationOwnerInput {
    authority: AcceptedNodeViewAuthority,
}

impl AcceptedApplicationOwnerInput {
    /// Returns the installed application section with the identity of its view, or `None`
    /// before a view is installed or after a manager-session change cleared it.
    pub fn installed(&self) -> Option<InstalledApplicationSection> {
        let state = self
            .authority
            .state
            .lock()
            .expect("accepted node view lock");
        let accepted = state.accepted.as_ref()?;
        Some(InstalledApplicationSection {
            site_id: accepted.identity.site_id.clone(),
            node_id: accepted.identity.node_id.clone(),
            manager_session: accepted.identity.publication.manager_session.clone(),
            view_revision: accepted.identity.revision,
            execution_targets: accepted.projection.execution_targets().to_vec(),
            assignments: accepted.projection.application_assignments().to_vec(),
        })
    }

    /// Returns the listener woken when a new view is installed.
    pub fn change_listener(&self) -> Arc<Notify> {
        Arc::clone(&self.authority.application_listener)
    }
}

/// Application section of one installed node view, with the identity of that view.
#[derive(Clone, Debug, PartialEq)]
pub struct InstalledApplicationSection {
    pub site_id: String,
    pub node_id: String,
    pub manager_session: ManagerSession,
    pub view_revision: u64,
    pub execution_targets: Vec<ExecutionTarget>,
    pub assignments: Vec<ApplicationAssignment>,
}

struct AcceptedViewState {
    manager_session: ManagerSession,
    replaced_sessions: BTreeSet<ManagerSession>,
    next_stream_id: u64,
    /// The latest opened node control stream, the only one whose publications install.
    stream: Option<StreamFence>,
    accepted: Option<AcceptedNodeViewSnapshot>,
    lease_expires_at: Option<Instant>,
}

/// Node-local fence of one node control stream: its identity and the highest publication
/// sequence consumed from it.
struct StreamFence {
    id: u64,
    accepted_publication: u64,
}

/// Makes `manager_session` current and drops everything the previous session authorized.
fn adopt_manager_session(state: &mut AcceptedViewState, manager_session: ManagerSession) {
    let previous = std::mem::replace(&mut state.manager_session, manager_session);
    state.replaced_sessions.insert(previous);
    state.stream = None;
    state.accepted = None;
    state.lease_expires_at = None;
}

/// Installs one envelope-checked view whose content matches its digest, renewing the action
/// lease; an equal revision only refreshes, and an older one is refused.
fn install_view<T>(
    state: &mut AcceptedViewState,
    identity: AcceptedNodeViewIdentity,
    projection: &NodeViewProjection,
    lease_duration: Duration,
    install: impl FnOnce() -> T,
) -> Result<(ViewAcceptance, T), NodeViewError> {
    let expected_digest = projection.digest();
    if identity.content_digest != expected_digest {
        return Err(NodeViewError::ContentDigestMismatch);
    }
    let acceptance = if let Some(accepted) = &state.accepted {
        if identity.revision < accepted.identity.revision {
            return Err(NodeViewError::StaleRevision {
                accepted: accepted.identity.revision,
                received: identity.revision,
            });
        }
        if identity.revision == accepted.identity.revision {
            if identity.content_digest != accepted.identity.content_digest {
                return Err(NodeViewError::ConflictingRevision(identity.revision));
            }
            ViewAcceptance::Refreshed
        } else {
            ViewAcceptance::Installed
        }
    } else {
        ViewAcceptance::Installed
    };
    state.accepted = Some(AcceptedNodeViewSnapshot {
        identity,
        projection: projection.clone(),
    });
    state.lease_expires_at = Some(Instant::now() + lease_duration);
    let installed = install();
    Ok((acceptance, installed))
}

fn validate_operation_state(
    authority: &AcceptedNodeViewAuthority,
    state: &AcceptedViewState,
    assignment: &ActionAssignment,
) -> Result<(), ActionPermitError> {
    let accepted = fresh_accepted_view(state)?;
    if !accepted.projection.contains_assignment(assignment) {
        return Err(ActionPermitError::DeploymentNotAccepted);
    }
    debug_assert_eq!(
        accepted.identity.publication.node_incarnation,
        authority.node_incarnation
    );
    Ok(())
}

/// Returns the accepted view while its action lease is still fresh.
fn fresh_accepted_view(
    state: &AcceptedViewState,
) -> Result<&AcceptedNodeViewSnapshot, ActionPermitError> {
    let accepted = state
        .accepted
        .as_ref()
        .ok_or(ActionPermitError::NoAcceptedView)?;
    let expires_at = state
        .lease_expires_at
        .ok_or(ActionPermitError::NoAcceptedView)?;
    if Instant::now() >= expires_at {
        return Err(ActionPermitError::Expired);
    }
    Ok(accepted)
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AcceptedNodeViewSnapshot {
    identity: AcceptedNodeViewIdentity,
    projection: NodeViewProjection,
}

/// Caller-defined identity for one preparation or runtime attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LifecycleOperationAttempt {
    Preparation { incarnation: u64, generation: u64 },
    Runtime { reconcile_generation: u64 },
}

/// Identity captured before asynchronous lifecycle work begins.
#[derive(Clone, Debug, PartialEq)]
pub struct LifecycleOperationToken {
    authority: OperationAuthorityIdentity,
    assignment: ActionAssignment,
    attempt: LifecycleOperationAttempt,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum OperationAuthorityIdentity {
    #[cfg(any(test, feature = "test-support"))]
    LocalAuthoritative,

    Leased {
        manager_session: ManagerSession,
        node_incarnation: NodeIncarnation,
    },
}

/// Controller runtime-action authorization mode: every node's runtime actions are fenced by the
/// lease of its accepted view.
#[derive(Clone)]
pub enum RuntimeActionAuthority {
    /// Admits every action, for tests of runtime dispatch alone.
    #[cfg(any(test, feature = "test-support"))]
    LocalAuthoritative,

    Leased(AcceptedNodeViewAuthority),
}

impl RuntimeActionAuthority {
    /// Captures the exact accepted assignment, model or application, that one lifecycle attempt
    /// acts for. The token stays valid while the shared lease is fresh and the view still carries
    /// that assignment, whatever changes elsewhere in the view.
    pub fn capture_operation(
        &self,
        assignment: impl Into<ActionAssignment>,
        attempt: LifecycleOperationAttempt,
    ) -> Result<LifecycleOperationToken, ActionPermitError> {
        let assignment = assignment.into();
        let authority = match self {
            #[cfg(any(test, feature = "test-support"))]
            Self::LocalAuthoritative => OperationAuthorityIdentity::LocalAuthoritative,
            Self::Leased(accepted) => {
                let state = accepted.state.lock().expect("accepted node view lock");
                validate_operation_state(accepted, &state, &assignment)?;
                OperationAuthorityIdentity::Leased {
                    manager_session: state.manager_session.clone(),
                    node_incarnation: accepted.node_incarnation.clone(),
                }
            }
        };
        Ok(LifecycleOperationToken {
            authority,
            assignment,
            attempt,
        })
    }

    /// Runs one synchronous owner mutation while accepted-view replacement is excluded.
    pub fn commit_operation<T>(
        &self,
        token: &LifecycleOperationToken,
        commit: impl FnOnce() -> T,
    ) -> Result<T, ActionPermitError> {
        match (self, &token.authority) {
            #[cfg(any(test, feature = "test-support"))]
            (Self::LocalAuthoritative, OperationAuthorityIdentity::LocalAuthoritative) => {
                Ok(commit())
            }
            (
                Self::Leased(accepted),
                OperationAuthorityIdentity::Leased {
                    manager_session,
                    node_incarnation,
                },
            ) => {
                let state = accepted.state.lock().expect("accepted node view lock");
                if manager_session != &state.manager_session
                    || node_incarnation != &accepted.node_incarnation
                {
                    return Err(ActionPermitError::StaleOperation);
                }
                validate_operation_state(accepted, &state, &token.assignment)?;
                Ok(commit())
            }
            #[cfg(any(test, feature = "test-support"))]
            _ => Err(ActionPermitError::StaleOperation),
        }
    }

    /// Runs one orphan-workload admission while accepted-view replacement is excluded. A leased
    /// authority admits only while its view is fresh and the `owner` section declares no
    /// deployment for `owner_id`; local authority admits unconditionally because its caller
    /// rechecks the intent it owns.
    pub fn commit_orphan_action<T>(
        &self,
        owner: DeploymentOwner,
        owner_id: &str,
        commit: impl FnOnce() -> T,
    ) -> Result<T, ActionPermitError> {
        match self {
            #[cfg(any(test, feature = "test-support"))]
            Self::LocalAuthoritative => Ok(commit()),
            Self::Leased(accepted) => {
                let state = accepted.state.lock().expect("accepted node view lock");
                if fresh_accepted_view(&state)?
                    .projection
                    .assigns(owner, owner_id)
                {
                    return Err(ActionPermitError::OwnerAccepted);
                }
                Ok(commit())
            }
        }
    }

    pub fn authorize(&self) -> Result<RuntimeActionPermit, ActionPermitError> {
        match self {
            #[cfg(any(test, feature = "test-support"))]
            Self::LocalAuthoritative => Ok(RuntimeActionPermit::LocalAuthoritative),
            Self::Leased(authority) => authority.action_permit(),
        }
    }

    pub fn authorize_deployment(
        &self,
        assignment: impl Into<ActionAssignment>,
    ) -> Result<RuntimeActionPermit, ActionPermitError> {
        match self {
            #[cfg(any(test, feature = "test-support"))]
            Self::LocalAuthoritative => Ok(RuntimeActionPermit::LocalAuthoritative),
            Self::Leased(authority) => {
                let (snapshot, permit) = authority.snapshot_with_permit()?;
                if !snapshot.projection.contains_assignment(&assignment.into()) {
                    return Err(ActionPermitError::DeploymentNotAccepted);
                }
                Ok(permit)
            }
        }
    }
}

/// Proof that action freshness was checked immediately before one runtime mutation.
#[derive(Debug)]
pub enum RuntimeActionPermit {
    #[cfg(any(test, feature = "test-support"))]
    LocalAuthoritative,

    Leased {
        #[cfg(any(test, feature = "test-support"))]
        manager_session: ManagerSession,

        #[cfg(any(test, feature = "test-support"))]
        view_revision: u64,
    },
}

#[cfg(any(test, feature = "test-support"))]
impl RuntimeActionPermit {
    pub fn view_revision(&self) -> Option<u64> {
        match self {
            Self::LocalAuthoritative => None,
            Self::Leased { view_revision, .. } => Some(*view_revision),
        }
    }

    pub fn manager_session(&self) -> Option<&ManagerSession> {
        match self {
            Self::LocalAuthoritative => None,
            Self::Leased {
                manager_session, ..
            } => Some(manager_session),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewAcceptance {
    Installed,
    Refreshed,
}

#[derive(Debug, thiserror::Error)]
pub enum ProcessIdentityError {
    #[error("process identity must be 32 lowercase hexadecimal characters")]
    Invalid,

    #[error("operating-system entropy failed while generating a process identity: {0}")]
    Entropy(String),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IncarnationFenceError {
    #[error("node lifecycle sequence must be nonzero")]
    InvalidSequence,

    #[error("another node process incarnation remains active")]
    CurrentIncarnationActive,

    #[error("node process incarnation is not registered by observation")]
    UnregisteredIncarnation,

    #[error("node process incarnation was replaced")]
    ReplacedIncarnation,

    #[error("node lifecycle sequence was reused with different content")]
    ConflictingSequence,

    #[error("desired-view acknowledgement does not name an offered view")]
    UnofferedView,

    #[error("node control stream was replaced or closed")]
    StreamReplaced,

    #[error("node lifecycle sequence {received} is not newer than accepted sequence {accepted}")]
    StaleSequence { accepted: u64, received: u64 },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NodeViewError {
    #[error("node-view identity is invalid")]
    InvalidIdentity,

    #[error("node-view projection contains a duplicate identity")]
    DuplicateIdentity,

    #[error("node-view deployment placement does not reference an included execution target")]
    InvalidPlacement,

    #[error("node-view content digest is not a lowercase SHA-256 value")]
    InvalidContentDigest,

    #[error("node-view envelope identity does not match this node")]
    IdentityMismatch,

    #[error("node-view content digest does not match normalized semantic content")]
    ContentDigestMismatch,

    #[error("node-view publication belongs to a replaced node control stream")]
    ReplacedStream,

    #[error(
        "node-view publication sequence {received} is not newer than accepted sequence {accepted}"
    )]
    StalePublication { accepted: u64, received: u64 },

    #[error("node-view manager session does not match the active authenticated manager")]
    ManagerSessionMismatch,

    #[error("node-view manager session was replaced")]
    ReplacedManagerSession,

    #[error("node-view revision {received} is older than accepted revision {accepted}")]
    StaleRevision { accepted: u64, received: u64 },

    #[error("node-view revision {0} was reused with conflicting semantic content")]
    ConflictingRevision(u64),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ActionPermitError {
    #[error("the lifecycle operation belongs to a replaced authority session")]
    StaleOperation,

    #[error("no complete node view has been accepted")]
    NoAcceptedView,

    #[error("the accepted node-view action lease expired")]
    Expired,

    #[error("the deployment is not part of the accepted node view")]
    DeploymentNotAccepted,

    #[error("the orphan workload's deployment is part of the accepted node view")]
    OwnerAccepted,
}

/// Why the installed node view refuses to serve one exact deployment generation.
#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
pub enum ServingAuthorizationError {
    #[error("the serving request names another node incarnation")]
    NodeIncarnationMismatch,

    #[error("the serving request names another manager session")]
    ManagerSessionMismatch,

    #[error("the installed node view does not assign the deployment at this lifecycle generation")]
    DeploymentNotAssigned,
}

fn random_process_identity() -> Result<String, ProcessIdentityError> {
    let mut bytes = [0_u8; PROCESS_ID_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|error| ProcessIdentityError::Entropy(error.to_string()))?;
    let mut identity = String::with_capacity(PROCESS_ID_BYTES * 2);
    for byte in bytes {
        use fmt::Write as _;
        write!(&mut identity, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(identity)
}

fn validate_process_identity(value: &str) -> Result<(), ProcessIdentityError> {
    if value.len() != PROCESS_ID_BYTES * 2
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ProcessIdentityError::Invalid);
    }
    Ok(())
}

fn reject_duplicate_identities<'a>(
    identities: impl IntoIterator<Item = &'a str>,
) -> Result<(), NodeViewError> {
    let mut unique = BTreeSet::new();
    for identity in identities {
        if !valid_id(identity) || !unique.insert(identity) {
            return Err(NodeViewError::DuplicateIdentity);
        }
    }
    Ok(())
}

/// Deployment identity of an assignment whose declaration the projection already required.
fn application_assignment_id(assignment: &ApplicationAssignment) -> &str {
    assignment.declaration.as_ref().map_or("", |declaration| {
        declaration.application_deployment_id.as_str()
    })
}

fn hash_messages<M: Message>(hasher: &mut Sha256, domain: &[u8], messages: &[M]) {
    hasher.update(domain);
    hasher.update((messages.len() as u64).to_be_bytes());
    for message in messages {
        hash_bytes(hasher, &message.encode_to_vec());
    }
}

fn hash_bytes(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use dotcore_domain::deployment::DeploymentOwner;
    use dotcore_proto::dotcore::{
        application::{private::v1::ApplicationAssignment, v1::ApplicationDeploymentDeclaration},
        deployment::v1::ExactPlacement,
        model::v1::{ModelDeploymentDeclaration, ModelDeploymentDesiredState},
        site::v1::{ExecutionTarget, Site},
    };

    use super::{
        AcceptedNodeViewAuthority, AcceptedNodeViewIdentity, ActionPermitError,
        LifecycleOperationAttempt, ManagerSession, NodeIncarnation, NodeViewError,
        NodeViewProjection, NodeViewPublicationIdentity, RuntimeActionAuthority,
        ServingAuthorizationError, ViewAcceptance,
    };

    #[test]
    fn a_model_action_digest_ignores_only_node_resolved_launch_identities() {
        let declaration = ModelDeploymentDeclaration {
            model_deployment_id: "deployment".to_owned(),
            lifecycle_generation: 1,
            ..Default::default()
        };
        let pinned = ModelDeploymentDeclaration {
            node_resolved_launch_identities: vec![
                dotcore_proto::dotcore::deployment::v1::NodeResolvedLaunchIdentity {
                    selection_key: "engine".to_owned(),
                    launch_identity: format!("sha256:{}", "1".repeat(64)),
                },
            ],
            ..declaration.clone()
        };
        let resumed = ModelDeploymentDeclaration {
            lifecycle_generation: 2,
            ..declaration.clone()
        };

        assert_eq!(
            super::ActionAssignment::from(&declaration),
            super::ActionAssignment::from(&pinned)
        );
        assert_ne!(
            super::ActionAssignment::from(&declaration),
            super::ActionAssignment::from(&resumed)
        );
    }

    #[test]
    fn normalized_node_view_digest_has_fixed_vector_and_excludes_envelope_identity() {
        let projection = projection(false).expect("normalized projection");
        assert_eq!(
            projection.digest().as_str(),
            "cb9fb807af933378f5a186939311975c17601bab4323dd5ffc076032973d2af9"
        );
        let first = AcceptedNodeViewIdentity::for_projection(
            "site-one",
            "worker-one",
            request(manager_session('a'), node_incarnation('1'), 1),
            1,
            &projection,
        );
        let second = AcceptedNodeViewIdentity::for_projection(
            "site-two",
            "worker-two",
            request(manager_session('b'), node_incarnation('2'), 99),
            99,
            &projection,
        );
        assert_eq!(first.content_digest, second.content_digest);
    }

    #[test]
    fn normalized_node_view_digest_is_reorder_invariant_and_rejects_duplicates() {
        let canonical = projection(false).expect("canonical projection");
        let reordered = projection(true).expect("reordered projection");
        assert_eq!(canonical, reordered);
        assert_eq!(canonical.digest(), reordered.digest());

        let mut duplicate = test_site();
        duplicate
            .execution_targets
            .push(duplicate.execution_targets[0].clone());
        assert_eq!(
            NodeViewProjection::for_site_node(&duplicate, "worker-one"),
            Err(NodeViewError::DuplicateIdentity)
        );

        let mut duplicate = test_site();
        duplicate
            .model_deployments
            .push(duplicate.model_deployments[0].clone());
        assert_eq!(
            NodeViewProjection::for_site_node(&duplicate, "worker-one"),
            Err(NodeViewError::DuplicateIdentity)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn accepted_views_are_idempotent_fence_sessions_and_rejected_views_do_not_renew_lease() {
        let projection = projection(false).expect("projection");
        let node = node_incarnation('1');
        let first_session = manager_session('a');
        let authority = AcceptedNodeViewAuthority::new(
            "site-one",
            "worker-one",
            node.clone(),
            first_session.clone(),
            Duration::from_secs(5),
        )
        .expect("view authority");
        let first_stream = authority
            .open_stream(first_session.clone())
            .expect("first session stream");
        assert_eq!(
            authority.install_for_test("site-one", "worker-one", 1, &projection),
            Ok(ViewAcceptance::Installed)
        );
        tokio::time::advance(Duration::from_secs(4)).await;
        assert_eq!(
            authority.install_for_test("site-one", "worker-one", 1, &projection),
            Ok(ViewAcceptance::Refreshed)
        );

        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(
            authority
                .action_permit()
                .expect("unchanged publication refreshed lease")
                .view_revision(),
            Some(1)
        );

        let installed = authority
            .snapshot_with_permit()
            .expect("installed snapshot")
            .0;
        let mut conflicting_site = test_site();
        conflicting_site.model_deployments[0].model_deployment_name = "changed".to_owned();
        let conflicting = NodeViewProjection::for_site_node(&conflicting_site, "worker-one")
            .expect("conflicting projection");
        assert_eq!(
            authority.install_for_test("site-one", "worker-one", 1, &conflicting),
            Err(NodeViewError::ConflictingRevision(1))
        );
        assert_eq!(
            authority
                .snapshot_with_permit()
                .expect("rejected view preserves snapshot")
                .0,
            installed
        );
        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(matches!(
            authority.action_permit(),
            Err(ActionPermitError::Expired)
        ));

        let replacement_session = manager_session('b');
        authority
            .replace_manager_session(replacement_session.clone())
            .expect("replace manager session");
        let late = AcceptedNodeViewIdentity::for_projection(
            "site-one",
            "worker-one",
            request(first_session, node.clone(), 77),
            2,
            &projection,
        );
        assert_eq!(
            authority.accept_publication(&first_stream, late, &projection),
            Err(NodeViewError::ReplacedManagerSession)
        );
        assert_eq!(
            authority.install_for_test("site-one", "worker-one", 1, &projection),
            Ok(ViewAcceptance::Installed)
        );
        assert_eq!(
            authority
                .action_permit()
                .expect("renewed permit")
                .view_revision(),
            Some(1)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stale_view_revision_preserves_snapshot_and_does_not_refresh_lease() {
        let projection = projection(false).expect("projection");
        let node = node_incarnation('1');
        let authority = AcceptedNodeViewAuthority::new(
            "site-one",
            "worker-one",
            node,
            manager_session('a'),
            Duration::from_secs(5),
        )
        .expect("view authority");
        authority
            .install_for_test("site-one", "worker-one", 2, &projection)
            .expect("current view");
        let installed = authority
            .snapshot_with_permit()
            .expect("installed snapshot")
            .0;

        tokio::time::advance(Duration::from_secs(4)).await;
        assert_eq!(
            authority.install_for_test("site-one", "worker-one", 1, &projection),
            Err(NodeViewError::StaleRevision {
                accepted: 2,
                received: 1
            })
        );
        assert_eq!(
            authority
                .snapshot_with_permit()
                .expect("stale publication preserves snapshot")
                .0,
            installed
        );

        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(matches!(
            authority.snapshot_with_permit(),
            Err(ActionPermitError::Expired)
        ));
    }
    #[test]
    fn guarded_commit_orders_session_replacement_and_rejects_old_tokens() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        };

        let projection = projection(false).expect("projection");
        let accepted = AcceptedNodeViewAuthority::new(
            "site-one",
            "worker-one",
            node_incarnation('1'),
            manager_session('a'),
            Duration::from_secs(30),
        )
        .expect("view authority");
        accepted
            .install_for_test("site-one", "worker-one", 1, &projection)
            .expect("accepted view");
        let authority = RuntimeActionAuthority::Leased(accepted.clone());
        let deployment = projection.model_deployments[0].clone();
        let token = authority
            .capture_operation(
                &deployment,
                LifecycleOperationAttempt::Runtime {
                    reconcile_generation: 7,
                },
            )
            .expect("captured operation");
        let stale_token = token.clone();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let commit_authority = authority.clone();
        let commit_entered = Arc::clone(&entered);
        let commit_release = Arc::clone(&release);
        let commit = std::thread::spawn(move || {
            commit_authority
                .commit_operation(&token, || {
                    commit_entered.wait();
                    commit_release.wait();
                    41
                })
                .expect("guarded commit")
        });
        entered.wait();

        let (replaced_tx, replaced_rx) = mpsc::channel();
        let replacement_authority = accepted.clone();
        let replacement = std::thread::spawn(move || {
            replacement_authority
                .replace_manager_session(manager_session('b'))
                .expect("replacement session");
            replaced_tx.send(()).expect("replacement result");
        });
        assert!(
            replaced_rx
                .recv_timeout(std::time::Duration::from_millis(20))
                .is_err(),
            "session replacement must wait for the guarded commit"
        );
        release.wait();
        assert_eq!(commit.join().expect("commit thread"), 41);
        replaced_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("replacement completed after commit");
        replacement.join().expect("replacement thread");

        let closure_ran = AtomicBool::new(false);
        assert_eq!(
            authority.commit_operation(&stale_token, || {
                closure_ran.store(true, Ordering::SeqCst)
            }),
            Err(ActionPermitError::StaleOperation)
        );
        assert!(!closure_ran.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn serving_follows_the_installed_assignment_and_outlives_view_freshness() {
        let mut site = test_site();
        for deployment in &mut site.model_deployments {
            deployment.lifecycle_generation = 3;
            deployment.desired_state = if deployment.model_deployment_id == "model-one" {
                ModelDeploymentDesiredState::Present as i32
            } else {
                ModelDeploymentDesiredState::Paused as i32
            };
        }
        let projection =
            NodeViewProjection::for_site_node(&site, "worker-one").expect("serving projection");
        let session = manager_session('a');
        let node = node_incarnation('1');
        let authority = AcceptedNodeViewAuthority::new(
            "site-one",
            "worker-one",
            node.clone(),
            session.clone(),
            Duration::from_secs(5),
        )
        .expect("view authority");
        let serve = |session: &ManagerSession, node: &NodeIncarnation, id, generation| {
            authority.authorize_serving(session.as_str(), node.as_str(), id, generation)
        };

        assert_eq!(
            serve(&session, &node, "model-one", 3),
            Err(ServingAuthorizationError::DeploymentNotAssigned),
            "nothing is served before a view is installed"
        );
        authority
            .install_for_test("site-one", "worker-one", 1, &projection)
            .expect("installed view");

        assert_eq!(serve(&session, &node, "model-one", 3), Ok(()));
        for (id, generation) in [
            ("model-one", 2),
            ("model-one", 4),
            ("model-two", 3),
            ("x", 3),
        ] {
            assert_eq!(
                serve(&session, &node, id, generation),
                Err(ServingAuthorizationError::DeploymentNotAssigned),
                "{id} at generation {generation} is not a Present assignment"
            );
        }
        assert_eq!(
            serve(&session, &node_incarnation('2'), "model-one", 3),
            Err(ServingAuthorizationError::NodeIncarnationMismatch)
        );
        assert_eq!(
            serve(&manager_session('b'), &node, "model-one", 3),
            Err(ServingAuthorizationError::ManagerSessionMismatch)
        );

        tokio::time::advance(Duration::from_secs(6)).await;
        assert_eq!(
            authority.action_permit().map(|_| ()),
            Err(ActionPermitError::Expired)
        );
        assert_eq!(
            serve(&session, &node, "model-one", 3),
            Ok(()),
            "a stale view keeps serving"
        );

        authority
            .replace_manager_session(manager_session('b'))
            .expect("replacement session");
        assert_eq!(
            serve(&session, &node, "model-one", 3),
            Err(ServingAuthorizationError::ManagerSessionMismatch)
        );
        assert_eq!(
            serve(&manager_session('b'), &node, "model-one", 3),
            Err(ServingAuthorizationError::DeploymentNotAssigned),
            "a replacement session serves nothing until it installs a view"
        );
    }

    #[test]
    fn application_assignments_join_the_digest_only_when_present_and_need_unique_identities() {
        let model_only = projection(false).expect("model projection");
        let applications = model_only
            .clone()
            .with_application_assignments(vec![
                application("application-two", "Two"),
                application("application-one", "One"),
            ])
            .expect("application projection");
        let reordered = model_only
            .clone()
            .with_application_assignments(vec![
                application("application-one", "One"),
                application("application-two", "Two"),
            ])
            .expect("reordered application projection");

        assert_eq!(
            model_only
                .clone()
                .with_application_assignments(Vec::new())
                .expect("empty application section")
                .digest(),
            model_only.digest()
        );
        assert_ne!(applications.digest(), model_only.digest());
        assert_eq!(applications, reordered);
        assert_eq!(applications.digest(), reordered.digest());
        assert_eq!(
            model_only.clone().with_application_assignments(vec![
                application("application-one", "One"),
                application("application-one", "Other"),
            ]),
            Err(NodeViewError::DuplicateIdentity)
        );
        assert_eq!(
            model_only.with_application_assignments(vec![ApplicationAssignment::default()]),
            Err(NodeViewError::InvalidIdentity)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn model_and_application_tokens_survive_each_others_changes_under_one_lease() {
        let model = deployment("model-one", "target-one");
        let application = application("application-one", "Application one");
        let accepted = AcceptedNodeViewAuthority::new(
            "site-one",
            "worker-one",
            node_incarnation('1'),
            manager_session('a'),
            Duration::from_secs(30),
        )
        .expect("view authority");
        install(&accepted, 1, &mixed(&model, &application));
        let authority = RuntimeActionAuthority::Leased(accepted.clone());
        let attempt = || LifecycleOperationAttempt::Runtime {
            reconcile_generation: 1,
        };
        let model_token = authority
            .capture_operation(&model, attempt())
            .expect("model token");
        let application_token = authority
            .capture_operation(&application, attempt())
            .expect("application token");

        let mut renamed_application = application.clone();
        renamed_application
            .declaration
            .as_mut()
            .expect("application declaration")
            .application_deployment_name = "Renamed application".to_owned();
        install(&accepted, 2, &mixed(&model, &renamed_application));
        assert_eq!(authority.commit_operation(&model_token, || 1), Ok(1));
        assert_eq!(
            authority.commit_operation(&application_token, || 2),
            Err(ActionPermitError::DeploymentNotAccepted)
        );
        let application_token = authority
            .capture_operation(&renamed_application, attempt())
            .expect("renamed application token");

        let mut renamed_model = model.clone();
        renamed_model.model_deployment_name = "Renamed model".to_owned();
        install(&accepted, 3, &mixed(&renamed_model, &renamed_application));
        assert_eq!(authority.commit_operation(&application_token, || 3), Ok(3));
        assert_eq!(
            authority.commit_operation(&model_token, || 4),
            Err(ActionPermitError::DeploymentNotAccepted)
        );
        assert!(authority.authorize_deployment(&renamed_application).is_ok());
        assert_eq!(
            authority
                .authorize_deployment(&application)
                .map(|permit| permit.view_revision()),
            Err(ActionPermitError::DeploymentNotAccepted)
        );

        tokio::time::advance(Duration::from_secs(31)).await;
        assert_eq!(
            authority.commit_operation(&application_token, || 5),
            Err(ActionPermitError::Expired),
            "both owners share one lease"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn recording_an_application_pin_keeps_the_in_flight_application_token() {
        let model = deployment("model-one", "target-one");
        let application = application("application-one", "Application one");
        let accepted = AcceptedNodeViewAuthority::new(
            "site-one",
            "worker-one",
            node_incarnation('1'),
            manager_session('a'),
            Duration::from_secs(30),
        )
        .expect("view authority");
        install(&accepted, 1, &mixed(&model, &application));
        let authority = RuntimeActionAuthority::Leased(accepted.clone());
        let attempt = || LifecycleOperationAttempt::Runtime {
            reconcile_generation: 1,
        };
        let token = authority
            .capture_operation(&application, attempt())
            .expect("application token");

        let mut pinned = application.clone();
        pinned
            .declaration
            .as_mut()
            .expect("application declaration")
            .node_resolved_launch_identities = vec![
            dotcore_proto::dotcore::deployment::v1::NodeResolvedLaunchIdentity {
                selection_key: "web".to_owned(),
                launch_identity: format!("sha256:{}", "1".repeat(64)),
            },
        ];
        install(&accepted, 2, &mixed(&model, &pinned));
        let mut resumed = pinned.clone();
        resumed
            .declaration
            .as_mut()
            .expect("application declaration")
            .lifecycle_generation = 2;

        assert_eq!(
            super::ActionAssignment::from(&application),
            super::ActionAssignment::from(&pinned)
        );
        assert_ne!(
            super::ActionAssignment::from(&pinned),
            super::ActionAssignment::from(&resumed)
        );
        assert_eq!(
            authority.commit_operation(&token, || 1),
            Ok(1),
            "the recorded pin is not new intent"
        );
        install(&accepted, 3, &mixed(&model, &resumed));
        assert_eq!(
            authority.commit_operation(&token, || 2),
            Err(ActionPermitError::DeploymentNotAccepted)
        );
    }

    #[test]
    fn orphan_admission_checks_only_the_owning_section() {
        let model = deployment("shared-id", "target-one");
        let application = application("shared-id", "Application");
        let accepted = AcceptedNodeViewAuthority::new(
            "site-one",
            "worker-one",
            node_incarnation('1'),
            manager_session('a'),
            Duration::from_secs(30),
        )
        .expect("view authority");
        let authority = RuntimeActionAuthority::Leased(accepted.clone());
        install(
            &accepted,
            1,
            &NodeViewProjection::new(vec![target("target-one")], vec![model])
                .expect("model projection"),
        );

        assert_eq!(
            authority.commit_orphan_action(DeploymentOwner::Model, "shared-id", || ()),
            Err(ActionPermitError::OwnerAccepted)
        );
        assert_eq!(
            authority.commit_orphan_action(DeploymentOwner::Application, "shared-id", || 1),
            Ok(1)
        );

        install(
            &accepted,
            2,
            &NodeViewProjection::new(vec![target("target-one")], Vec::new())
                .expect("target projection")
                .with_application_assignments(vec![application])
                .expect("application projection"),
        );
        assert_eq!(
            authority.commit_orphan_action(DeploymentOwner::Model, "shared-id", || 2),
            Ok(2)
        );
        assert_eq!(
            authority.commit_orphan_action(DeploymentOwner::Application, "shared-id", || ()),
            Err(ActionPermitError::OwnerAccepted)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stream_publications_install_only_from_the_latest_stream_in_sequence() {
        let projection = projection(false).expect("projection");
        let node = node_incarnation('1');
        let authority = AcceptedNodeViewAuthority::new(
            "site-one",
            "worker-one",
            node.clone(),
            manager_session('0'),
            Duration::from_secs(5),
        )
        .expect("view authority");
        let stream = authority
            .open_stream(manager_session('a'))
            .expect("stream adopts the answering manager session");
        assert_eq!(stream.manager_session(), &manager_session('a'));
        let publication = |sequence: u64, revision: u64| {
            AcceptedNodeViewIdentity::for_projection(
                "site-one",
                "worker-one",
                request(manager_session('a'), node.clone(), sequence),
                revision,
                &projection,
            )
        };
        assert_eq!(
            authority.accept_publication(&stream, publication(1, 1), &projection),
            Ok(ViewAcceptance::Installed)
        );
        tokio::time::advance(Duration::from_secs(4)).await;
        assert_eq!(
            authority.accept_publication(&stream, publication(1, 1), &projection),
            Err(NodeViewError::StalePublication {
                accepted: 1,
                received: 1
            })
        );
        assert_eq!(
            authority.accept_publication(&stream, publication(3, 1), &projection),
            Ok(ViewAcceptance::Refreshed),
            "an unchanged republication refreshes the lease"
        );
        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(
            authority.action_permit().is_ok(),
            "lease renewed at the republication"
        );

        let newer = authority
            .open_stream(manager_session('a'))
            .expect("newer stream");
        assert_eq!(
            authority.accept_publication(&stream, publication(4, 1), &projection),
            Err(NodeViewError::ReplacedStream)
        );
        assert_eq!(
            authority.accept_publication(&newer, publication(1, 1), &projection),
            Ok(ViewAcceptance::Refreshed),
            "a newer stream restarts its publication sequence"
        );

        authority
            .open_stream(manager_session('b'))
            .expect("replacement manager session");
        assert_eq!(
            authority.action_permit().map(|_| ()),
            Err(ActionPermitError::NoAcceptedView),
            "a new manager session freezes the old view's actions"
        );
        assert_eq!(
            authority.open_stream(manager_session('a')),
            Err(NodeViewError::ReplacedManagerSession)
        );
    }

    fn projection(reversed: bool) -> Result<NodeViewProjection, NodeViewError> {
        let mut site = test_site();
        if reversed {
            site.execution_targets.reverse();
            site.model_deployments.reverse();
        }
        NodeViewProjection::for_site_node(&site, "worker-one")
    }

    fn test_site() -> Site {
        Site {
            site_id: "site-one".to_owned(),
            execution_targets: vec![target("target-two"), target("target-one")],
            model_deployments: vec![
                deployment("model-two", "target-two"),
                deployment("model-one", "target-one"),
            ],
            ..Default::default()
        }
    }

    fn target(id: &str) -> ExecutionTarget {
        ExecutionTarget {
            execution_target_id: id.to_owned(),
            site_node_id: "worker-one".to_owned(),
            ..Default::default()
        }
    }

    fn deployment(id: &str, target_id: &str) -> ModelDeploymentDeclaration {
        ModelDeploymentDeclaration {
            model_deployment_id: id.to_owned(),
            model_deployment_name: id.to_owned(),
            placement: Some(ExactPlacement {
                site_node_id: "worker-one".to_owned(),
                execution_target_id: target_id.to_owned(),
            }),
            ..Default::default()
        }
    }

    fn application(id: &str, name: &str) -> ApplicationAssignment {
        ApplicationAssignment {
            declaration: Some(ApplicationDeploymentDeclaration {
                application_deployment_id: id.to_owned(),
                application_deployment_name: name.to_owned(),
                placement: Some(ExactPlacement {
                    site_node_id: "worker-one".to_owned(),
                    execution_target_id: "application-target".to_owned(),
                }),
                lifecycle_generation: 1,
                ..Default::default()
            }),
            linked_route: None,
        }
    }

    fn mixed(
        model: &ModelDeploymentDeclaration,
        application: &ApplicationAssignment,
    ) -> NodeViewProjection {
        NodeViewProjection::new(
            vec![target("target-one"), target("application-target")],
            vec![model.clone()],
        )
        .expect("model projection")
        .with_application_assignments(vec![application.clone()])
        .expect("mixed projection")
    }

    fn install(
        accepted: &AcceptedNodeViewAuthority,
        revision: u64,
        projection: &NodeViewProjection,
    ) {
        accepted
            .install_for_test("site-one", "worker-one", revision, projection)
            .expect("installed view");
    }

    fn request(
        manager_session: ManagerSession,
        node_incarnation: NodeIncarnation,
        sequence: u64,
    ) -> NodeViewPublicationIdentity {
        NodeViewPublicationIdentity {
            manager_session,
            node_incarnation,
            sequence,
        }
    }

    fn node_incarnation(character: char) -> NodeIncarnation {
        NodeIncarnation::parse(character.to_string().repeat(32)).expect("node incarnation")
    }

    fn manager_session(character: char) -> ManagerSession {
        ManagerSession::parse(character.to_string().repeat(32)).expect("manager session")
    }
}
