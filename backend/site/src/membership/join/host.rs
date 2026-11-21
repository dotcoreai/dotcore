//! The manager side of a node join, served by the join registry on the private listener: the
//! join secret check, a dial-back proving the manager reaches the joining node by its key,
//! the operator's decision, certificate signing, and the two-step commit that records the node.
//! A declared node joining again from its intact folder takes the same steps, except that it
//! proves its pinned identity, nothing is signed, and the commit records only its new addresses.
//!
//! Nothing is written before the joining node commits an offer on the connection that received
//! it, and a rejected or abandoned attempt leaves the token usable for a corrected retry.

use std::{
    fmt,
    net::{IpAddr, SocketAddr},
    panic::AssertUnwindSafe,
    sync::Arc,
};

use dotcore_domain::{HostResolver, SiteNodeHost};
use dotcore_proto::dotcore::{
    node::v1::{
        CommitRequest, CommitResponse, JoinRequest, JoinResponse, NodeObservationConfiguration,
        node_join_service_server::NodeJoinService,
    },
    site::v1::{ExecutionTarget, Site, SiteNode},
};
use futures_util::FutureExt as _;
use tonic::{Request, Response, Status, metadata::MetadataValue};
use tracing::{info, warn};
use url::Url;

use super::{
    JoinProposal, JoinToken, JoinTokenError,
    admission::{AdmissionError, RejoinProof, admit, admit_rejoin, certified_node_id},
    csr_key_fingerprint,
    dial_back::{DialBack, dial_back},
    node_key::verified_csr_key,
    proposal::{
        CERTIFICATE_EXPIRED_REJECTION, CERTIFICATE_NOT_PINNED_REJECTION,
        CERTIFICATE_NOT_YET_VALID_REJECTION, DUPLICATE_HOST_REJECTION, DUPLICATE_NAME_REJECTION,
        NODE_NOT_DECLARED_REJECTION, ProposedJoin, REJECTION_METADATA_KEY,
        UNREACHABLE_NODE_REJECTION, UNUSABLE_HOST_REJECTION,
    },
    registry::{
        ActiveJoin, JoinAttemptOutcome, JoinEnd, Registry, ReservationConflict, SlotClaim,
        abandoned_status,
    },
};
use crate::{
    node::folder::CertificateFingerprint, private_transport::PrivateConnectionInfo,
    validation::SiteDeclarationRules,
};

/// The manager's node identity a join token pins and an offer names: its leaf certificate and
/// the site CA.
pub struct JoinHostIdentity {
    site_id: String,
    manager_node_id: String,
    site_ca_der: Vec<u8>,
    certificate_der: Vec<u8>,
}

impl JoinHostIdentity {
    /// Wraps the manager's leaf certificate and the site CA certificate.
    pub fn new(
        site_id: impl Into<String>,
        manager_node_id: impl Into<String>,
        site_ca_der: Vec<u8>,
        certificate_der: Vec<u8>,
    ) -> Self {
        Self {
            site_id: site_id.into(),
            manager_node_id: manager_node_id.into(),
            site_ca_der,
            certificate_der,
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

    pub(super) fn site_id(&self) -> &str {
        &self.site_id
    }

    pub(super) fn manager_node_id(&self) -> &str {
        &self.manager_node_id
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
            .finish()
    }
}

/// The site authority a join reads, signs with, and records into.
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

/// Everything the operator sees before allowing a node.
#[derive(Clone, Debug, PartialEq)]
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

    /// Execution targets the site records for a new node, or the declared ones of a rejoining
    /// node.
    pub targets: Vec<ExecutionTarget>,
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
pub(super) enum JoinDecision {
    /// Sign the node's certificate and offer it the site configuration.
    Allow,

    /// Refuse the proposal; the joining node may retry with another one.
    Reject,
}

/// The node a join recorded.
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

/// The join service joining machines call on the private listener, for every join the registry
/// holds.
pub struct JoinService {
    registry: Arc<Registry>,
}

impl JoinService {
    pub(super) fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

#[tonic::async_trait]
impl NodeJoinService for JoinService {
    async fn join(&self, request: Request<JoinRequest>) -> Result<Response<JoinResponse>, Status> {
        let connection = connection_info(&request)?;
        let request = request.into_inner();
        let join = self
            .registry
            .join_for_secret(&connection, &request.join_secret)?;
        let _busy = connection.begin_request();
        let mut claim = join.claim_slot(&connection)?;
        match self
            .registry
            .attempt(&join, &mut claim, &connection, request)
            .await
        {
            Ok(response) => Ok(Response::new(response)),
            Err(failure) => {
                claim.outcome = Some(failure.outcome);
                Err(failure.status)
            }
        }
    }

    async fn commit(
        &self,
        request: Request<CommitRequest>,
    ) -> Result<Response<CommitResponse>, Status> {
        let connection = connection_info(&request)?;
        let request = request.into_inner();
        let join = self
            .registry
            .join_for_secret(&connection, &request.join_secret)?;
        let _busy = connection.begin_request();
        let offer = join.begin_commit(connection.id(), &request.offer_id)?;
        // The commit runs on its own task so a dropped request cannot leave it half-reported, and
        // a panic in it still ends the join, which releases its hold on the private listener.
        let registry = Arc::clone(&self.registry);
        tokio::spawn(async move {
            let committed = AssertUnwindSafe(registry.commit(Arc::clone(&join), offer))
                .catch_unwind()
                .await;
            committed.unwrap_or_else(|_| {
                warn!(join_id = %join.id(), "node join commit stopped unexpectedly");
                join.commit_uncertain();
                Err(uncertain_commit_status())
            })
        })
        .await
        .map_err(|_| uncertain_commit_status())??;
        Ok(Response::new(CommitResponse {}))
    }
}

impl Registry {
    /// Runs one join request from its proposal to the offer: admission, dial-back, the
    /// operator's decision, and signing.
    async fn attempt(
        &self,
        join: &Arc<ActiveJoin>,
        claim: &mut SlotClaim,
        connection: &PrivateConnectionInfo,
        request: JoinRequest,
    ) -> Result<JoinResponse, AttemptFailure> {
        let peer_address = connection.peer_address();
        let csr_key_fingerprint = csr_key_fingerprint(&request.csr_der)
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        let proposed = ProposedJoin::from_request(&request)
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        let manager_host = SiteNodeHost::normalize(&request.manager_host).map_err(|_| {
            Status::invalid_argument(
                "the manager host the node dialed must be a specific IPv4 address or a DNS host name",
            )
        })?;
        let admitted = self
            .admit(join, claim, &request, proposed, peer_address, &manager_host)
            .await?;
        let host = admitted.host.clone();
        let host_source = admitted.host_source;

        let host_differs_from_peer = host_source != JoinHostSource::Observed
            && host_differs_from(&host, peer_address, self.resolver.as_ref()).await;
        let review = JoinReview {
            request: admitted.review,
            node_id: admitted.node.node_id.clone(),
            host: host.clone(),
            host_source,
            host_differs_from_peer,
            manager_host: manager_host.clone(),
            peer_address,
            csr_key_fingerprint,
            targets: admitted.targets.clone(),
        };
        let proposal_id =
            random_id().map_err(|_| Status::internal("could not create a join proposal"))?;
        let decided = join.await_decision(claim, &proposal_id, review)?;
        let decision = tokio::select! {
            decision = decided => decision.unwrap_or(JoinDecision::Reject),
            () = claim.cancel.cancelled() => return Err(abandoned_status().into()),
        };
        if decision == JoinDecision::Reject {
            info!(join_id = %join.id(), node_id = %admitted.node.node_id, peer = %peer_address, "node join rejected by the operator");
            return Err(AttemptFailure {
                status: Status::permission_denied("the manager operator rejected this join"),
                outcome: JoinAttemptOutcome::Rejected,
            });
        }

        let certificate_der = match admitted.kind {
            OfferKind::NewNode => {
                let site_writer = Arc::clone(&self.site);
                let site = admitted.site;
                let node = admitted.node.clone();
                let csr = request.csr_der;
                blocking(move || site_writer.sign_node_certificate(&site, &node, &csr))
                    .await?
                    .map_err(|error| site_status(&error))?
            }
            OfferKind::Rejoin => request.node_certificate_der,
        };
        let offer_id =
            random_id().map_err(|_| Status::internal("could not create a join offer"))?;
        let identity = &self.identity;
        let private_port = self
            .offered_private_port()
            .ok_or_else(|| Status::internal("the manager private port is unknown"))?;
        // The node keeps dialing the manager at the address it reached the join service on.
        let manager_private_endpoint =
            Url::parse(&format!("https://{manager_host}:{private_port}"))
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
        join.store_offer(
            claim,
            PendingOffer {
                offer_id,
                node: admitted.node,
                host,
                targets: admitted.targets,
                certificate_der,
                kind: admitted.kind,
            },
        )?;
        info!(join_id = %join.id(), node_id = %response.node_id, peer = %peer_address, "node join offer issued");
        Ok(response)
    }

    /// Checks a decoded request against the current site: a new node's proposal, or a declared
    /// node's proof that it holds its pinned identity for this join, reserves what it would
    /// record, then dials the node back at the host it would record. A new node without a
    /// proposed host is recorded at `peer_address`; a rejoining one keeps its declared host.
    async fn admit(
        &self,
        join: &ActiveJoin,
        claim: &SlotClaim,
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
                let (host, host_source) = recorded_host(proposal.host.as_ref(), peer_address)?;
                let rules = self.site.declaration_rules();
                let admission = admit(&site, &rules, &proposal, &host, manager_host)
                    .map_err(|error| admission_status(&error))?;
                self.reserve(join, claim.ticket, &admission.node)
                    .map_err(reservation_status)?;
                self.dial_back(join, &host, proposal.private_port, &request.csr_der)
                    .await?;
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
                    secret: &join.token.secret,
                };
                let rules = self.site.declaration_rules();
                let admission = admit_rejoin(
                    &site,
                    &rules,
                    &node_id,
                    &proof,
                    pin.as_ref(),
                    &proposal,
                    manager_host,
                )
                .map_err(|error| admission_status(&error))?;
                self.reserve(join, claim.ticket, &admission.node)
                    .map_err(reservation_status)?;
                let host = SiteNodeHost::parse(&admission.node.host)
                    .map_err(|_| Status::internal("the declared node host is invalid"))?;
                self.dial_back(join, &host, proposal.private_port, &request.csr_der)
                    .await?;
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

    /// Dials the joining node back at `host` and `port`, which must answer with the key its
    /// signing request `csr_der` proves. It counts as failed from the start and is refunded only
    /// once it succeeds, so an attempt abandoned mid-dial still counts; the join fails at the
    /// limit.
    async fn dial_back(
        &self,
        join: &ActiveJoin,
        host: &SiteNodeHost,
        port: u16,
        csr_der: &[u8],
    ) -> Result<(), Status> {
        let node_key = verified_csr_key(csr_der)
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        let limit = self.limits.max_failed_dial_backs;
        let failed = join
            .count_dial_back()
            .ok_or_else(super::registry::finished_status)?;
        if failed > limit {
            join.refund_dial_back();
            join.finish(JoinEnd::Failed(too_many_dial_backs()));
            return Err(super::registry::finished_status());
        }
        info!(join_id = %join.id(), %host, port, "dialing the joining node back");
        let outcome = dial_back(
            host,
            port,
            &node_key,
            self.resolver.as_ref(),
            self.timing().connect_timeout,
        )
        .await;
        let dial = DialBack {
            host: host.clone(),
            port,
            outcome,
        };
        let error = match dial.outcome {
            Ok(address) => {
                join.refund_dial_back();
                info!(join_id = %join.id(), %host, port, %address, "joining node reached by its key");
                return Ok(());
            }
            Err(error) => error.detailed(),
        };
        warn!(join_id = %join.id(), %host, port, %error, failed_dial_backs = failed, "joining node not reached");
        if failed >= limit {
            join.finish(JoinEnd::Failed(too_many_dial_backs()));
        }
        Err(rejection_status(
            Status::failed_precondition(format!(
                "the manager could not reach this node at {host}:{port}: {error}"
            )),
            UNREACHABLE_NODE_REJECTION,
        ))
    }

    /// Records the node, or a rejoining node's new addresses, then ends the join or frees it for
    /// a retry.
    async fn commit(
        self: Arc<Self>,
        join: Arc<ActiveJoin>,
        offer: PendingOffer,
    ) -> Result<(), Status> {
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
        match published {
            Ok(()) => {
                info!(
                    join_id = %join.id(),
                    node_id = %joined.node_id,
                    display_name = %joined.display_name,
                    host = %joined.host,
                    rejoined = joined.rejoined,
                    "node join committed"
                );
                join.committed(joined);
                Ok(())
            }
            Err(error @ JoinSiteError::OutcomeUncertain(_)) => {
                warn!(join_id = %join.id(), node_id = %joined.node_id, error = %error, "node join outcome is uncertain");
                join.commit_uncertain();
                Err(uncertain_commit_status())
            }
            Err(error) => {
                warn!(join_id = %join.id(), node_id = %joined.node_id, error = %error, "node join recorded nothing");
                // A refusal says why; a storage failure stays in the manager's log.
                let status = match &error {
                    JoinSiteError::Rejected(_) => Status::aborted(format!(
                        "the manager could not record the node and recorded nothing: {error}"
                    )),
                    JoinSiteError::Failed(_) | JoinSiteError::OutcomeUncertain(_) => {
                        Status::aborted(
                            "the manager could not record the node and recorded nothing",
                        )
                    }
                };
                join.commit_failed(status.message());
                Err(status)
            }
        }
    }
}

/// A join request that ended without an offer, with what the operator is shown.
struct AttemptFailure {
    status: Status,
    outcome: JoinAttemptOutcome,
}

impl From<Status> for AttemptFailure {
    fn from(status: Status) -> Self {
        Self {
            outcome: JoinAttemptOutcome::Failed(status.message().to_owned()),
            status,
        }
    }
}

/// An offer made to a joining node, recorded once it commits on the connection that received it.
pub(super) struct PendingOffer {
    pub(super) offer_id: String,
    pub(super) node: SiteNode,
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

/// Returns the address the site records for the joining node: the proposed host, else the
/// connection's source address, which must be a specific IPv4 address.
fn recorded_host(
    proposed_host: Option<&SiteNodeHost>,
    peer_address: SocketAddr,
) -> Result<(SiteNodeHost, JoinHostSource), Status> {
    if let Some(host) = proposed_host {
        return Ok((host.clone(), JoinHostSource::Proposed));
    }
    let observed = peer_address.ip().to_canonical();
    let host = match observed {
        IpAddr::V4(host) if !host.is_multicast() => SiteNodeHost::from_ipv4(host).ok(),
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

/// Projects a reservation conflict with another join into a status the joining node acts on.
fn reservation_status(conflict: ReservationConflict) -> Status {
    match conflict {
        ReservationConflict::Name => rejection_status(
            Status::already_exists("another machine is joining with this node name"),
            DUPLICATE_NAME_REJECTION,
        ),
        ReservationConflict::Host => rejection_status(
            Status::already_exists("another machine is joining with this node host"),
            DUPLICATE_HOST_REJECTION,
        ),
        ReservationConflict::Node => {
            Status::failed_precondition("another join of this node is in progress")
        }
        ReservationConflict::Abandoned => abandoned_status(),
    }
}

/// The commit's outcome is unknown; the operator checks the site.
fn uncertain_commit_status() -> Status {
    Status::unknown("the manager could not confirm that it recorded the node")
}

fn too_many_dial_backs() -> String {
    "too many joins named a host and port where the manager could not reach the machine".to_owned()
}

fn connection_info<T>(request: &Request<T>) -> Result<PrivateConnectionInfo, Status> {
    request
        .extensions()
        .get::<PrivateConnectionInfo>()
        .cloned()
        .ok_or_else(|| Status::internal("join connection information is missing"))
}

/// Returns a random identity for a join, a proposal or an offer.
pub(super) fn random_id() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub(super) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Status> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| Status::internal("join task failed"))
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
