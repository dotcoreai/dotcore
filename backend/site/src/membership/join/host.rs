//! Hosts the manager side of one node join: a TLS listener presenting the manager's identity,
//! the join secret check, operator approval, certificate signing, and the two-step commit that
//! records the approved node. The offline node addition runs it while the manager is stopped.
//! A declared node joining again from its intact folder takes the same steps, except that it
//! proves its pinned identity, nothing is signed, and the commit records only its new addresses.
//!
//! The site it reads and writes, the approval step, and the listener are inputs, so a running
//! manager can host the same handshake later. Nothing is written before the joining node
//! commits an offer on the connection that received it, and a rejected or abandoned attempt
//! leaves the token usable for a corrected retry.

use std::{
    fmt,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use dotcore_domain::{HostResolver, SiteNodeHost, SystemHostResolver, real_time};
use dotcore_proto::dotcore::{
    node::v1::{
        CommitRequest, CommitResponse, JoinRequest, JoinResponse, NodeObservationConfiguration,
        node_join_service_server::{NodeJoinService, NodeJoinServiceServer},
    },
    site::v1::{ExecutionTarget, Site, SiteNode},
};
use futures_util::future::BoxFuture;
use tokio::{net::TcpListener, sync::watch};
use tokio_stream::{StreamExt as _, wrappers::TcpListenerStream};
use tokio_util::sync::CancellationToken;
use tonic::{
    Request, Response, Status,
    metadata::MetadataValue,
    transport::{Identity, ServerTlsConfig, server::TlsConnectInfo},
};
use tracing::{info, warn};
use url::Url;

use super::{
    JoinProposal, JoinToken, JoinTokenError,
    admission::{AdmissionError, RejoinProof, admit, admit_rejoin, certified_node_id},
    connection::{JoinConnectionInfo, JoinConnections},
    csr_key_fingerprint,
    proposal::{
        CERTIFICATE_EXPIRED_REJECTION, CERTIFICATE_NOT_PINNED_REJECTION,
        CERTIFICATE_NOT_YET_VALID_REJECTION, DUPLICATE_HOST_REJECTION, DUPLICATE_NAME_REJECTION,
        NODE_NOT_DECLARED_REJECTION, ProposedJoin, REJECTION_METADATA_KEY, UNUSABLE_HOST_REJECTION,
    },
    publication::NodePublicationError,
};
use crate::{
    node::folder::CertificateFingerprint, private_transport::pem, validation::SiteDeclarationRules,
};

/// The manager side of one node join, from listening to the committed node.
///
/// One host serves one token on its own listener: a commit is bound to the TLS connection that
/// received its offer, so the host must own its connections. A running manager that hosts joins
/// runs one host per token next to its other listeners.
pub struct JoinHost {
    shared: HostShared,
}

impl JoinHost {
    /// Prepares a host that admits at most one node with `settings.token`, reading and recording
    /// through `site` and asking `approval` before issuing any certificate.
    pub fn new(
        settings: JoinHostSettings,
        site: Arc<dyn JoinSiteWriter>,
        approval: Arc<dyn JoinApproval>,
    ) -> Self {
        let (finished, _) = watch::channel(false);
        Self {
            shared: HostShared {
                settings,
                site,
                approval,
                resolver: Arc::new(SystemHostResolver),
                state: Mutex::new(HostState::default()),
                finished,
            },
        }
    }

    /// Replaces the system resolver used to look up a proposed host name before comparing it
    /// with the join connection's address.
    #[cfg(test)]
    pub(crate) fn with_host_resolver(mut self, resolver: Arc<dyn HostResolver>) -> Self {
        self.shared.resolver = resolver;
        self
    }

    /// Serves the join on `listener` until one node is committed, the token expires, or too many
    /// wrong secrets arrive. Returns the recorded node.
    pub async fn serve(self, listener: TcpListener) -> Result<JoinedNode, JoinHostError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let shared = Arc::new(self.shared);
        shared.validate_settings().await?;
        let limits = shared.settings.limits.clone();
        let identity = &shared.settings.identity;
        let tls = ServerTlsConfig::new()
            .identity(Identity::from_pem(
                format!(
                    "{}{}",
                    pem("CERTIFICATE", &identity.certificate_der),
                    pem("CERTIFICATE", &identity.site_ca_der)
                ),
                pem("PRIVATE KEY", &identity.private_key_pkcs8),
            ))
            .timeout(limits.handshake_timeout);
        if let Ok(address) = listener.local_addr() {
            info!(%address, site_id = %identity.site_id, "node join host listening");
        }

        let closed = Arc::clone(&shared);
        let connections = Arc::new(JoinConnections::new(
            limits.max_connections,
            limits.idle_timeout,
            move |connection| closed.connection_closed(connection),
        ));
        // The server ignores its own TCP options for a supplied stream, so each accepted socket
        // sets no delay here and small responses aren't held for the peer's acknowledgment.
        let incoming = TcpListenerStream::new(listener).map(move |accepted| {
            accepted.and_then(|stream| {
                stream.set_nodelay(true)?;
                connections.accept(stream)
            })
        });
        let shutdown_shared = Arc::clone(&shared);
        let shutdown = async move {
            let finished = shutdown_shared.wait_finished();
            if real_time::bound(limits.overall_timeout, finished)
                .await
                .is_err()
            {
                shutdown_shared.stop(JoinHostError::Expired);
                shutdown_shared.wait_finished().await;
            }
        };
        let server = tonic::transport::Server::builder()
            .tls_config(tls)
            .map_err(JoinHostError::Transport)?
            .add_service(NodeJoinServiceServer::new(JoinService {
                shared: Arc::clone(&shared),
            }))
            .serve_with_incoming_shutdown(incoming, shutdown);

        // Graceful shutdown waits for every connection; the grace period bounds how long an
        // unrelated idle client can delay the result once the join has finished.
        let grace = async {
            shared.wait_finished().await;
            real_time::pause(limits.shutdown_grace).await;
        };
        tokio::select! {
            served = server => served.map_err(JoinHostError::Transport)?,
            () = grace => {}
        }
        shared.take_outcome()
    }
}

/// Fixed inputs of one join host.
#[derive(Debug)]
pub struct JoinHostSettings {
    /// Manager identity presented by the listener.
    pub identity: JoinHostIdentity,

    /// Token handed to the operator; its secret authorizes the join.
    pub token: JoinToken,

    /// Port of the manager's private listener. The joined node's folder records it with the
    /// manager address that node dialed.
    pub manager_private_port: u16,

    /// Records an observed loopback source address as the node host. A local forwarder hides the
    /// joining node's own address behind loopback, so this stays off outside same-machine tests.
    pub allow_observed_loopback: bool,

    /// Timeouts and attempt limits.
    pub limits: JoinHostLimits,
}

/// The manager's node identity the join listener presents, with the site CA sent in the chain.
pub struct JoinHostIdentity {
    site_id: String,
    manager_node_id: String,
    site_ca_der: Vec<u8>,
    certificate_der: Vec<u8>,
    private_key_pkcs8: Vec<u8>,
}

impl JoinHostIdentity {
    /// Wraps the manager's leaf certificate, its PKCS#8 key, and the site CA certificate.
    pub fn new(
        site_id: impl Into<String>,
        manager_node_id: impl Into<String>,
        site_ca_der: Vec<u8>,
        certificate_der: Vec<u8>,
        private_key_pkcs8: Vec<u8>,
    ) -> Self {
        Self {
            site_id: site_id.into(),
            manager_node_id: manager_node_id.into(),
            site_ca_der,
            certificate_der,
            private_key_pkcs8,
        }
    }

    /// Creates a token with a fresh secret that pins this identity and names `endpoint`.
    pub fn token(&self, endpoint: Url) -> Result<JoinToken, JoinTokenError> {
        JoinToken::generate(
            endpoint,
            &self.site_id,
            &self.manager_node_id,
            CertificateFingerprint::for_certificate(&self.certificate_der),
            CertificateFingerprint::for_certificate(&self.site_ca_der),
        )
    }

    /// Returns the site CA fingerprint the operator can compare on the joining node.
    pub fn site_ca_sha256(&self) -> CertificateFingerprint {
        CertificateFingerprint::for_certificate(&self.site_ca_der)
    }
}

impl fmt::Debug for JoinHostIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JoinHostIdentity")
            .field("site_id", &self.site_id)
            .field("manager_node_id", &self.manager_node_id)
            .field("site_ca_der", &"<DER certificate>")
            .field("certificate_der", &"<DER certificate>")
            .field("private_key_pkcs8", &"<redacted>")
            .finish()
    }
}

/// Timeouts and attempt limits of a join host.
#[derive(Clone, Debug)]
pub struct JoinHostLimits {
    /// Lifetime of the token; the host stops when it ends without a committed node.
    pub overall_timeout: Duration,

    /// Longest TLS handshake one connection may take.
    pub handshake_timeout: Duration,

    /// Longest silence of a connection with no request in progress, including between an offer
    /// and its commit.
    pub idle_timeout: Duration,

    /// Connections open at once; further connections are refused until one closes.
    pub max_connections: usize,

    /// Wrong secrets accepted across all connections before the host stops.
    pub max_failed_attempts: u32,

    /// Longest wait for open connections to close once the join has finished.
    pub shutdown_grace: Duration,
}

impl Default for JoinHostLimits {
    fn default() -> Self {
        Self {
            overall_timeout: Duration::from_secs(15 * 60),
            handshake_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(60),
            max_connections: 16,
            max_failed_attempts: 5,
            shutdown_grace: Duration::from_secs(5),
        }
    }
}

/// The site authority a join host reads, signs with, and records into. The offline
/// implementation holds the exclusive site lock; a running manager supplies its own.
///
/// Calls block and run on the blocking thread pool.
pub trait JoinSiteWriter: Send + Sync + 'static {
    /// Returns the owner rules every site this writer reads or writes must satisfy.
    fn declaration_rules(&self) -> SiteDeclarationRules;

    /// Returns the current site that proposals are checked against.
    fn site(&self) -> Result<Site, JoinSiteError>;

    /// Signs the node's certificate signing request for `node` with the site CA. Called only
    /// after operator approval.
    fn sign_node_certificate(
        &self,
        site: &Site,
        node: &SiteNode,
        csr_der: &[u8],
    ) -> Result<Vec<u8>, JoinSiteError>;

    /// Records `node`, its targets, and the pin of `certificate_der` in the site, completely or
    /// not at all.
    fn publish_node(
        &self,
        node: &SiteNode,
        targets: &[ExecutionTarget],
        certificate_der: &[u8],
    ) -> Result<(), JoinSiteError>;

    /// Returns the certificate pin the site trust holds for `node_id`, or `None` without one.
    fn node_pin(&self, node_id: &str) -> Result<Option<CertificateFingerprint>, JoinSiteError>;

    /// Records the `host` and `manager_host` of the declared node `node`, after checking again
    /// that the node is a declared worker pinned to `certificate_der` and that no other node
    /// uses its host. Nothing else changes, and nothing is written when the addresses are equal.
    fn replace_node_hosts(
        &self,
        node: &SiteNode,
        certificate_der: &[u8],
    ) -> Result<(), JoinSiteError>;
}

/// Why the site authority refused or failed a join step.
#[derive(Debug, thiserror::Error)]
pub enum JoinSiteError {
    #[error("the site refuses this node: {0}")]
    Rejected(#[source] anyhow::Error),

    #[error("the site could not complete the join step; nothing was recorded: {0}")]
    Failed(#[source] anyhow::Error),

    #[error("the site may or may not have recorded the node: {0}")]
    OutcomeUncertain(#[source] anyhow::Error),
}

impl From<NodePublicationError> for JoinSiteError {
    fn from(error: NodePublicationError) -> Self {
        match error {
            NodePublicationError::OutcomeUncertain(_) => {
                Self::OutcomeUncertain(anyhow::Error::new(error))
            }
            NodePublicationError::SiteUnreadable(_)
            | NodePublicationError::SerializeSite(_)
            | NodePublicationError::PinUnreadable(_)
            | NodePublicationError::Storage(_) => Self::Failed(anyhow::Error::new(error)),
            NodePublicationError::AdditionalSiteManager
            | NodePublicationError::DuplicateHost { .. }
            | NodePublicationError::UndeclaredNode { .. }
            | NodePublicationError::PinMismatch { .. }
            | NodePublicationError::ForeignTarget { .. }
            | NodePublicationError::InvalidSite(_) => Self::Rejected(anyhow::Error::new(error)),
        }
    }
}

/// The operator's decision on one join proposal; reviews never run concurrently.
pub trait JoinApproval: Send + Sync + 'static {
    /// Shows `review` to the operator and resolves to the decision.
    ///
    /// `cancel` fires when the join is abandoned, because its connection closed or the host
    /// finished; the host then drops the returned future. An implementation that blocks, such as
    /// a terminal prompt, runs through [`blocking_review`] and must stop once `cancel` fires.
    fn review(
        &self,
        review: JoinReview,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, JoinDecision>;
}

/// Runs a blocking review on the blocking thread pool for a [`JoinApproval`] implementation.
///
/// Resolves to [`JoinDecision::Reject`] as soon as `cancel` fires. `decide` receives the same
/// token and must return promptly once it fires, so no prompt outlives its join.
pub fn blocking_review(
    review: JoinReview,
    cancel: CancellationToken,
    decide: impl FnOnce(&JoinReview, &CancellationToken) -> JoinDecision + Send + 'static,
) -> BoxFuture<'static, JoinDecision> {
    Box::pin(async move {
        let token = cancel.clone();
        let decision = tokio::task::spawn_blocking(move || decide(&review, &token));
        tokio::select! {
            decided = decision => decided.unwrap_or(JoinDecision::Reject),
            () = cancel.cancelled() => JoinDecision::Reject,
        }
    })
}

/// Everything the operator sees before allowing a node.
#[derive(Clone, Debug)]
pub struct JoinReview {
    /// What the node asks for: to join as a new node, or to join again as a declared one.
    pub request: ReviewedJoin,

    /// SiteNode identity the manager derived for a new node, or the declared node's identity.
    pub node_id: String,

    /// Address or host name the site will record for reaching the node.
    pub host: SiteNodeHost,

    /// Whether `host` was proposed by the node or observed on the join connection.
    pub host_source: JoinHostSource,

    /// Whether a proposed or declared host, once a name is resolved on the manager, differs from
    /// the address the connection came from, which the operator should double-check. Always
    /// `false` for an observed host, which is the connection address.
    pub host_differs_from_peer: bool,

    /// Address or host name the node dialed to reach the manager, which it keeps using
    /// afterwards.
    pub manager_host: SiteNodeHost,

    /// Address the join connection came from.
    pub peer_address: SocketAddr,

    /// Fingerprint of the key in the node's signing request, also shown on the node.
    pub csr_key_fingerprint: CertificateFingerprint,
}

/// The kind of join under review.
#[derive(Clone, Debug, PartialEq)]
pub enum ReviewedJoin {
    /// A new node and what it proposes about itself.
    New(JoinProposal),

    /// A declared node joining again from its intact node folder.
    Rejoin(NodeRejoin),
}

/// A declared node joining again: it keeps its identity, targets, and deployments; only its two
/// addresses and its folder's observation configuration may change.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeRejoin {
    /// The node as the site declares it now.
    pub declared: SiteNode,

    /// The node as the rejoin records it; equal to `declared` when nothing changes.
    pub rejoined: SiteNode,

    /// Observation configuration the node's folder holds now.
    pub installed_observation: NodeObservationConfiguration,

    /// Observation configuration the site derives for the node, which its folder takes on.
    pub observation: NodeObservationConfiguration,

    /// Port of the node's private listener.
    pub private_port: u16,

    /// Model and application deployments placed on the node, which stay on it.
    pub deployments: usize,
}

impl NodeRejoin {
    /// Reports whether the rejoin leaves both the site and the node's folder configuration as
    /// they are.
    pub fn changes_nothing(&self) -> bool {
        self.declared == self.rejoined && self.installed_observation == self.observation
    }
}

/// Where the host recorded for a joining node comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinHostSource {
    /// The joining node proposed it, for example because its operator named its address.
    Proposed,

    /// The manager observed it as the join connection's source address.
    Observed,

    /// A rejoining node proposed none, so it keeps the host the site declares for it.
    Declared,
}

/// Operator decision on a join proposal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinDecision {
    /// Sign the node's certificate and offer it the site configuration.
    Allow,

    /// Refuse the proposal; the joining node may retry with another one.
    Reject,
}

/// The node a join host recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinedNode {
    /// SiteNode identity the manager derived.
    pub node_id: String,

    /// Operator-confirmed display name.
    pub display_name: String,

    /// Address or host name the site dials for the node.
    pub host: SiteNodeHost,

    /// Address or host name the node reaches the manager at, when it differs from the declared
    /// manager host.
    pub manager_host: Option<SiteNodeHost>,

    /// Pin of the node's certificate, as written to the site trust.
    pub certificate_sha256: CertificateFingerprint,

    /// Whether a declared node joined again, keeping its pin and recording only its addresses.
    pub rejoined: bool,
}

/// Why a join host stopped without a recorded node.
#[derive(Debug, thiserror::Error)]
pub enum JoinHostError {
    #[error("the join host settings are inconsistent: {0}")]
    InvalidSettings(#[source] anyhow::Error),

    #[error("the join token expired before a node joined")]
    Expired,

    #[error("too many join attempts used a wrong secret; the join token is revoked")]
    TooManyFailedAttempts,

    #[error("the node may or may not have been recorded: {0}")]
    PublicationUncertain(#[source] JoinSiteError),

    #[error("the join listener failed")]
    Transport(#[source] tonic::transport::Error),
}

struct HostShared {
    settings: JoinHostSettings,
    site: Arc<dyn JoinSiteWriter>,
    approval: Arc<dyn JoinApproval>,
    resolver: Arc<dyn HostResolver>,
    state: Mutex<HostState>,
    finished: watch::Sender<bool>,
}

impl HostShared {
    async fn validate_settings(&self) -> Result<(), JoinHostError> {
        let invalid = |message: &str| JoinHostError::InvalidSettings(anyhow::anyhow!("{message}"));
        let settings = &self.settings;
        let identity = &settings.identity;
        if settings.token.site_id != identity.site_id
            || settings.token.manager_node_id != identity.manager_node_id
            || settings.token.manager_certificate_sha256
                != CertificateFingerprint::for_certificate(&identity.certificate_der)
            || settings.token.site_ca_sha256 != identity.site_ca_sha256()
        {
            return Err(invalid("the join token does not pin the host identity"));
        }
        if settings.manager_private_port == 0 {
            return Err(invalid("the manager private port must not be zero"));
        }

        let site = Arc::clone(&self.site);
        let site = blocking(move || site.site())
            .await
            .map_err(|_| invalid("the site could not be read"))?
            .map_err(|source| JoinHostError::InvalidSettings(anyhow::Error::new(source)))?;
        let manager = site
            .nodes
            .iter()
            .find(|node| node.site_manager.is_some())
            .ok_or_else(|| invalid("the site declares no manager"))?;
        if site.site_id != identity.site_id || manager.node_id != identity.manager_node_id {
            return Err(invalid("the host identity is not this site's manager"));
        }
        Ok(())
    }

    /// Checks a decoded request against the current site: a new node's proposal, or a declared
    /// node's proof that it holds its pinned identity for this join. A new node without a
    /// proposed host is recorded at `peer_address`; a rejoining one keeps its declared host.
    async fn admit(
        &self,
        request: &JoinRequest,
        proposed: ProposedJoin,
        peer_address: SocketAddr,
        manager_host: &SiteNodeHost,
    ) -> Result<Admitted, Status> {
        let site_writer = Arc::clone(&self.site);
        let site = blocking(move || site_writer.site())
            .await?
            .map_err(|error| site_status(&error))?;
        match proposed {
            ProposedJoin::New(proposal) => {
                let (host, host_source) = recorded_host(
                    proposal.host.as_ref(),
                    peer_address,
                    self.settings.allow_observed_loopback,
                )?;
                let rules = self.site.declaration_rules();
                let admission = admit(&site, &rules, &proposal, &host, manager_host)
                    .map_err(|error| admission_status(&error))?;
                Ok(Admitted {
                    site,
                    host,
                    host_source,
                    review: ReviewedJoin::New(*proposal),
                    node: admission.node,
                    targets: admission.targets,
                    observation: admission.observation,
                    kind: OfferKind::NewNode,
                })
            }
            ProposedJoin::Rejoin(proposal) => {
                let node_id = certified_node_id(&site, &request.node_certificate_der)
                    .map_err(|error| admission_status(&error))?;
                let site_writer = Arc::clone(&self.site);
                let pinned = node_id.clone();
                let pin = blocking(move || site_writer.node_pin(&pinned))
                    .await?
                    .map_err(|error| site_status(&error))?;
                let proof = RejoinProof {
                    certificate_der: &request.node_certificate_der,
                    csr_der: &request.csr_der,
                    secret: &self.settings.token.secret,
                };
                let rules = self.site.declaration_rules();
                let admission = admit_rejoin(
                    &site,
                    &rules,
                    &node_id,
                    &proof,
                    pin.as_ref(),
                    proposal.host.as_ref(),
                    manager_host,
                )
                .map_err(|error| admission_status(&error))?;
                let host = SiteNodeHost::parse(&admission.node.host)
                    .map_err(|_| Status::internal("the declared node host is invalid"))?;
                let host_source = match proposal.host {
                    Some(_) => JoinHostSource::Proposed,
                    None => JoinHostSource::Declared,
                };
                Ok(Admitted {
                    site,
                    host,
                    host_source,
                    review: ReviewedJoin::Rejoin(NodeRejoin {
                        declared: admission.declared,
                        rejoined: admission.node.clone(),
                        installed_observation: request.node_observation.clone().unwrap_or_default(),
                        observation: admission.observation.clone(),
                        private_port: proposal.private_port,
                        deployments: admission.deployments,
                    }),
                    node: admission.node,
                    targets: admission.targets,
                    observation: admission.observation,
                    kind: OfferKind::Rejoin,
                })
            }
        }
    }

    /// Counts a wrong secret and stops the host once the limit is reached.
    fn check_secret(&self, presented: &[u8]) -> Result<(), Status> {
        if self.settings.token.secret.matches(presented) {
            return Ok(());
        }
        let failed = {
            let mut state = self.lock_state();
            state.failed_attempts += 1;
            state.failed_attempts
        };
        warn!(failed_attempts = failed, "join attempt used a wrong secret");
        if failed >= self.settings.limits.max_failed_attempts {
            self.stop(JoinHostError::TooManyFailedAttempts);
        }
        Err(Status::unauthenticated(
            "the join secret does not match this join token",
        ))
    }

    /// Reserves the single offer slot for one connection while its proposal is reviewed.
    fn claim_slot(self: &Arc<Self>, connection: u64) -> Result<SlotClaim, Status> {
        let mut state = self.lock_state();
        if state.outcome.is_some() {
            return Err(finished_status());
        }
        if !matches!(state.slot, Slot::Free) {
            return Err(Status::failed_precondition(
                "another node join is in progress on this manager; retry when it finishes",
            ));
        }
        state.next_ticket += 1;
        let ticket = state.next_ticket;
        let cancel = CancellationToken::new();
        state.slot = Slot::Reviewing {
            ticket,
            connection,
            cancel: cancel.clone(),
        };
        Ok(SlotClaim {
            shared: Arc::clone(self),
            ticket,
            cancel,
            armed: true,
        })
    }

    /// Turns a reviewed claim into the outstanding offer, unless the join was abandoned meanwhile.
    fn store_offer(&self, claim: &mut SlotClaim, offer: PendingOffer) -> Result<(), Status> {
        let mut state = self.lock_state();
        match &state.slot {
            Slot::Reviewing {
                ticket, connection, ..
            } if *ticket == claim.ticket => {
                state.slot = Slot::Offered {
                    connection: *connection,
                    offer,
                };
                claim.armed = false;
                Ok(())
            }
            _ => Err(abandoned_status()),
        }
    }

    /// Moves the outstanding offer into the commit step when this connection owns it.
    fn begin_commit(&self, connection: u64, offer_id: &str) -> Result<PendingOffer, Status> {
        let mut state = self.lock_state();
        if state.outcome.is_some() {
            return Err(finished_status());
        }
        match std::mem::replace(&mut state.slot, Slot::Free) {
            Slot::Offered {
                connection: owner,
                offer,
            } if owner == connection && offer.offer_id == offer_id => {
                state.slot = Slot::Committing;
                Ok(offer)
            }
            other => {
                state.slot = other;
                Err(Status::failed_precondition(
                    "no matching join offer is outstanding on this connection",
                ))
            }
        }
    }

    /// Records the node, or a rejoining node's new addresses, then finishes the host or frees the
    /// slot for a retry.
    async fn commit(self: Arc<Self>, offer: PendingOffer) -> Result<(), Status> {
        let site = Arc::clone(&self.site);
        let node = offer.node.clone();
        let targets = offer.targets.clone();
        let certificate = offer.certificate_der.clone();
        let kind = offer.kind;
        let published = blocking(move || match kind {
            OfferKind::NewNode => site.publish_node(&node, &targets, &certificate),
            OfferKind::Rejoin => site.replace_node_hosts(&node, &certificate),
        })
        .await
        .unwrap_or_else(|_| {
            Err(JoinSiteError::OutcomeUncertain(anyhow::anyhow!(
                "the publication task stopped unexpectedly"
            )))
        });
        let joined = JoinedNode {
            node_id: offer.node.node_id.clone(),
            display_name: offer.node.display_name.clone(),
            host: offer.host,
            manager_host: SiteNodeHost::parse(&offer.node.manager_host).ok(),
            certificate_sha256: CertificateFingerprint::for_certificate(&offer.certificate_der),
            rejoined: kind == OfferKind::Rejoin,
        };
        let mut state = self.lock_state();
        match published {
            Ok(()) => {
                info!(
                    node_id = %joined.node_id,
                    display_name = %joined.display_name,
                    host = %joined.host,
                    rejoined = joined.rejoined,
                    "node join committed"
                );
                self.finish_locked(&mut state, Ok(joined));
                Ok(())
            }
            Err(error @ JoinSiteError::OutcomeUncertain(_)) => {
                warn!(node_id = %joined.node_id, error = %error, "node join outcome is uncertain");
                self.finish_locked(&mut state, Err(JoinHostError::PublicationUncertain(error)));
                Err(Status::unknown(
                    "the manager could not confirm that it recorded the node",
                ))
            }
            Err(error) => {
                warn!(node_id = %joined.node_id, error = %error, "node join publication failed");
                // A stop requested during the commit takes effect now that nothing was recorded.
                match state.stop_after_commit.take() {
                    Some(reason) => self.finish_locked(&mut state, Err(reason)),
                    None => state.slot = Slot::Free,
                }
                Err(Status::aborted(format!(
                    "the manager could not record the node and recorded nothing: {error}"
                )))
            }
        }
    }

    /// Frees the slot a closed connection held, unless its offer is being committed.
    fn connection_closed(&self, connection: u64) {
        let mut state = self.lock_state();
        let owned = match &state.slot {
            Slot::Reviewing {
                connection: owner, ..
            }
            | Slot::Offered {
                connection: owner, ..
            } => *owner == connection,
            Slot::Free | Slot::Committing => false,
        };
        if owned {
            release_slot(&mut state);
        }
    }

    /// Stops the host for `reason`. A running commit reports its own outcome first; the reason
    /// applies only if that commit records nothing.
    fn stop(&self, reason: JoinHostError) {
        let mut state = self.lock_state();
        if state.outcome.is_some() {
            return;
        }
        if matches!(state.slot, Slot::Committing) {
            state.stop_after_commit.get_or_insert(reason);
        } else {
            self.finish_locked(&mut state, Err(reason));
        }
    }

    fn finish_locked(&self, state: &mut HostState, outcome: Result<JoinedNode, JoinHostError>) {
        if state.outcome.is_none() {
            state.outcome = Some(outcome);
            release_slot(state);
            self.finished.send_replace(true);
        }
    }

    async fn wait_finished(&self) {
        let mut finished = self.finished.subscribe();
        let _ = finished.wait_for(|finished| *finished).await;
    }

    fn take_outcome(&self) -> Result<JoinedNode, JoinHostError> {
        self.lock_state()
            .outcome
            .take()
            .unwrap_or(Err(JoinHostError::Expired))
    }

    fn lock_state(&self) -> MutexGuard<'_, HostState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Default)]
struct HostState {
    failed_attempts: u32,
    next_ticket: u64,
    slot: Slot,
    stop_after_commit: Option<JoinHostError>,
    outcome: Option<Result<JoinedNode, JoinHostError>>,
}

/// The single join a host handles at a time.
#[derive(Default)]
enum Slot {
    #[default]
    Free,

    Reviewing {
        ticket: u64,
        connection: u64,
        cancel: CancellationToken,
    },

    Offered {
        connection: u64,
        offer: PendingOffer,
    },

    Committing,
}

struct PendingOffer {
    offer_id: String,
    node: SiteNode,
    host: SiteNodeHost,
    targets: Vec<ExecutionTarget>,
    certificate_der: Vec<u8>,
    kind: OfferKind,
}

/// Whether an offer records a new node or a declared node's new addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OfferKind {
    NewNode,

    Rejoin,
}

/// A request the site accepts, with what the operator reviews and what the offer carries.
struct Admitted {
    /// The site the request was checked against, which signs a new node's certificate.
    site: Site,
    host: SiteNodeHost,
    host_source: JoinHostSource,
    review: ReviewedJoin,
    node: SiteNode,
    targets: Vec<ExecutionTarget>,
    observation: NodeObservationConfiguration,
    kind: OfferKind,
}

/// Frees the reviewed slot and cancels its review when the join request ends without an offer.
struct SlotClaim {
    shared: Arc<HostShared>,
    ticket: u64,
    cancel: CancellationToken,
    armed: bool,
}

impl Drop for SlotClaim {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self.shared.lock_state();
        if matches!(state.slot, Slot::Reviewing { ticket, .. } if ticket == self.ticket) {
            release_slot(&mut state);
        }
    }
}

struct JoinService {
    shared: Arc<HostShared>,
}

#[tonic::async_trait]
impl NodeJoinService for JoinService {
    async fn join(&self, request: Request<JoinRequest>) -> Result<Response<JoinResponse>, Status> {
        let connection = connection_info(&request)?;
        let _busy = connection.begin_request();
        let request = request.into_inner();
        let shared = &self.shared;
        shared.check_secret(&request.join_secret)?;
        let mut claim = shared.claim_slot(connection.id)?;

        let csr_key_fingerprint = csr_key_fingerprint(&request.csr_der)
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        let proposed = ProposedJoin::from_request(&request)
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        let manager_host = SiteNodeHost::normalize(&request.manager_host).map_err(|_| {
            Status::invalid_argument(
                "the manager host the node dialed must be a specific IPv4 address or a DNS host name",
            )
        })?;
        let admitted = shared
            .admit(&request, proposed, connection.peer_address, &manager_host)
            .await?;
        let host = admitted.host.clone();
        let host_source = admitted.host_source;

        let host_differs_from_peer = host_source != JoinHostSource::Observed
            && host_differs_from(&host, connection.peer_address, shared.resolver.as_ref()).await;
        let review = JoinReview {
            request: admitted.review,
            node_id: admitted.node.node_id.clone(),
            host: host.clone(),
            host_source,
            host_differs_from_peer,
            manager_host: manager_host.clone(),
            peer_address: connection.peer_address,
            csr_key_fingerprint,
        };
        let cancel = claim.cancel.clone();
        let decision = tokio::select! {
            decision = shared.approval.review(review, cancel.clone()) => decision,
            () = cancel.cancelled() => return Err(abandoned_status()),
        };
        if decision == JoinDecision::Reject {
            info!(node_id = %admitted.node.node_id, peer = %connection.peer_address, "node join rejected by the operator");
            return Err(Status::permission_denied(
                "the manager operator rejected this join",
            ));
        }

        let certificate_der = match admitted.kind {
            OfferKind::NewNode => {
                let site_writer = Arc::clone(&shared.site);
                let site = admitted.site;
                let node = admitted.node.clone();
                let csr = request.csr_der;
                blocking(move || site_writer.sign_node_certificate(&site, &node, &csr))
                    .await?
                    .map_err(|error| site_status(&error))?
            }
            OfferKind::Rejoin => request.node_certificate_der,
        };
        let offer_id = random_offer_id()?;
        let identity = &shared.settings.identity;
        // The node keeps dialing the manager at the address it reached the join listener on.
        let manager_private_endpoint = Url::parse(&format!(
            "https://{manager_host}:{}",
            shared.settings.manager_private_port
        ))
        .map_err(|_| Status::internal("could not compose the manager private endpoint"))?;
        let response = JoinResponse {
            offer_id: offer_id.clone(),
            site_id: identity.site_id.clone(),
            node_id: admitted.node.node_id.clone(),
            node_certificate_der: certificate_der.clone(),
            site_ca_der: identity.site_ca_der.clone(),
            manager_node_id: identity.manager_node_id.clone(),
            manager_private_endpoint: manager_private_endpoint.to_string(),
            manager_certificate_sha256: CertificateFingerprint::for_certificate(
                &identity.certificate_der,
            )
            .as_str()
            .to_owned(),
            observation: Some(admitted.observation),
            realization_targets: admitted.targets.clone(),
            node_host: host.to_string(),
            node_display_name: admitted.node.display_name.clone(),
        };
        shared.store_offer(
            &mut claim,
            PendingOffer {
                offer_id,
                node: admitted.node,
                host,
                targets: admitted.targets,
                certificate_der,
                kind: admitted.kind,
            },
        )?;
        info!(node_id = %response.node_id, peer = %connection.peer_address, "node join offer issued");
        Ok(Response::new(response))
    }

    async fn commit(
        &self,
        request: Request<CommitRequest>,
    ) -> Result<Response<CommitResponse>, Status> {
        let connection = connection_info(&request)?;
        let _busy = connection.begin_request();
        let request = request.into_inner();
        self.shared.check_secret(&request.join_secret)?;
        let offer = self.shared.begin_commit(connection.id, &request.offer_id)?;
        // The commit runs on its own task so a dropped request cannot leave it half-reported.
        tokio::spawn(Arc::clone(&self.shared).commit(offer))
            .await
            .map_err(|_| {
                Status::unknown("the manager could not confirm that it recorded the node")
            })??;
        Ok(Response::new(CommitResponse {}))
    }
}

/// Frees the slot, cancelling a review in progress.
fn release_slot(state: &mut HostState) {
    if let Slot::Reviewing { cancel, .. } = std::mem::take(&mut state.slot) {
        cancel.cancel();
    }
}

/// Returns the address the site records for the joining node: the proposed host, else the
/// connection's source address. An observed address that other machines cannot dial is refused,
/// since a local forwarder or proxy would otherwise be recorded in place of the node.
fn recorded_host(
    proposed_host: Option<&SiteNodeHost>,
    peer_address: SocketAddr,
    allow_loopback: bool,
) -> Result<(SiteNodeHost, JoinHostSource), Status> {
    if let Some(host) = proposed_host {
        return Ok((host.clone(), JoinHostSource::Proposed));
    }
    let observed = peer_address.ip().to_canonical();
    let host = match observed {
        IpAddr::V4(host) if !host.is_multicast() && (allow_loopback || !host.is_loopback()) => {
            SiteNodeHost::from_ipv4(host).ok()
        }
        _ => None,
    };
    host.map(|host| (host, JoinHostSource::Observed))
        .ok_or_else(|| {
            rejection_status(
                Status::invalid_argument(format!(
                    "the manager observed this node at {observed}, which it cannot record as the \
                     node address; provide the address the manager reaches this node at"
                )),
                UNUSABLE_HOST_REJECTION,
            )
        })
}

/// Reports whether `host` is not the address the join connection came from. A name is resolved
/// on the manager first and matches when any of its addresses is the peer's; a name that does
/// not resolve counts as different.
async fn host_differs_from(
    host: &SiteNodeHost,
    peer_address: SocketAddr,
    resolver: &dyn HostResolver,
) -> bool {
    match host.resolve_all(resolver).await {
        Ok(addresses) => !addresses
            .into_iter()
            .any(|address| peer_address.ip().to_canonical() == IpAddr::V4(address)),
        Err(error) => {
            warn!(%host, error = %error, "proposed node host could not be resolved for review");
            true
        }
    }
}

/// Projects an admission refusal into a status; refusals the joining node acts on carry their
/// stable reason.
fn admission_status(error: &AdmissionError) -> Status {
    let duplicate = |reason| rejection_status(Status::already_exists(error.to_string()), reason);
    let stale = |reason| rejection_status(Status::failed_precondition(error.to_string()), reason);
    match error {
        AdmissionError::DuplicateName { .. } => duplicate(DUPLICATE_NAME_REJECTION),
        AdmissionError::DuplicateHost { .. } => duplicate(DUPLICATE_HOST_REJECTION),
        AdmissionError::NodeNotDeclared { .. } => stale(NODE_NOT_DECLARED_REJECTION),
        AdmissionError::CertificateNotPinned { .. } => stale(CERTIFICATE_NOT_PINNED_REJECTION),
        AdmissionError::CertificateExpired { .. } => stale(CERTIFICATE_EXPIRED_REJECTION),
        AdmissionError::CertificateNotYetValid { .. } => stale(CERTIFICATE_NOT_YET_VALID_REJECTION),
        AdmissionError::ForeignCertificate
        | AdmissionError::ManagerNode
        | AdmissionError::InvalidProof(_)
        | AdmissionError::ProofKeyMismatch => Status::permission_denied(error.to_string()),
        AdmissionError::Site(error) => site_status(error),
    }
}

/// Tags a refusal with the stable reason the joining node classifies it by.
fn rejection_status(mut status: Status, reason: &'static str) -> Status {
    status
        .metadata_mut()
        .insert(REJECTION_METADATA_KEY, MetadataValue::from_static(reason));
    status
}

/// Projects a site authority error into a safe status; refusals keep their message.
fn site_status(error: &JoinSiteError) -> Status {
    match error {
        JoinSiteError::Rejected(_) => Status::invalid_argument(error.to_string()),
        JoinSiteError::Failed(_) | JoinSiteError::OutcomeUncertain(_) => {
            warn!(error = %error, "join host site step failed");
            Status::internal("the manager could not complete the join step")
        }
    }
}

/// The host has finished or stopped; nothing more will be recorded with this token.
fn finished_status() -> Status {
    Status::failed_precondition("this join token is no longer accepted")
}

fn abandoned_status() -> Status {
    Status::aborted("the join was abandoned before an offer was made")
}

fn connection_info<T>(request: &Request<T>) -> Result<JoinConnectionInfo, Status> {
    request
        .extensions()
        .get::<TlsConnectInfo<JoinConnectionInfo>>()
        .map(|info| info.get_ref().clone())
        .ok_or_else(|| Status::internal("join connection information is missing"))
}

fn random_offer_id() -> Result<String, Status> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| Status::internal("could not create a join offer"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Status> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| Status::internal("join host task failed"))
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use dotcore_domain::test_support::host_resolver::FakeHostResolver;

    use super::*;

    #[tokio::test]
    async fn a_host_name_matches_the_peer_through_any_of_its_addresses() {
        let resolver = FakeHostResolver::default().with(
            "gpu-box.example",
            [Ipv4Addr::new(100, 64, 0, 7), Ipv4Addr::new(10, 0, 0, 7)],
        );
        let host = SiteNodeHost::parse("gpu-box.example").expect("host");
        let differs = |peer: &str| {
            let peer = peer.parse().expect("peer address");
            let host = host.clone();
            let resolver = &resolver;
            async move { host_differs_from(&host, peer, resolver).await }
        };

        assert!(!differs("10.0.0.7:50000").await);
        assert!(!differs("[::ffff:100.64.0.7]:50000").await);
        assert!(differs("10.0.0.8:50000").await);
        let unknown = SiteNodeHost::parse("unknown.example").expect("host");
        assert!(
            host_differs_from(&unknown, "10.0.0.7:50000".parse().expect("peer"), &resolver).await
        );
    }
}
