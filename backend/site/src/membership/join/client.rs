//! The joining node's side of a node join, run on the new machine: it reaches the manager's
//! join listener named by the token, or at another manager address on a split network,
//! authenticates it from the token alone, sends the node's proposal and signing request, installs
//! the returned node folder, and commits.
//!
//! The private key never leaves this machine. The folder is moved into place before the commit,
//! removed only when the manager clearly refuses the commit, and kept when the outcome is unknown
//! so a node the site already recorded never loses its key.
//!
//! A declared node joining again from its intact folder proves it holds its key for this join,
//! checks that the offer keeps its certificate and targets exactly, commits, and only then
//! rewrites its folder's addresses and observation configuration. Its key, certificate, and trust
//! files never change, and the folder is never removed, so repeating the rejoin repairs any
//! interrupted attempt.

use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use dotcore_domain::{BindAddress, HostResolver, SiteNodeHost, SystemHostResolver, timing::Timing};
use dotcore_proto::dotcore::node::v1::{
    CommitRequest, DeclaredMachine, JoinResponse, node_join_service_client::NodeJoinServiceClient,
};
use tonic::{
    Code, Status,
    transport::{Channel, ClientTlsConfig, Endpoint},
};
use tracing::{info, warn};
use url::Url;

use super::{
    JoinProposal, JoinProposalError, JoinToken, NodeKey, NodeKeyError, RejoinProposal,
    RejoiningNodeFolder,
    installation::{
        NodeFolderContents, NodeFolderError, install_node_folder, reject_existing_folder,
        remove_node_folder, rewrite_bootstrap,
    },
    proposal::{
        CERTIFICATE_EXPIRED_REJECTION, CERTIFICATE_NOT_PINNED_REJECTION,
        CERTIFICATE_NOT_YET_VALID_REJECTION, DUPLICATE_HOST_REJECTION, DUPLICATE_NAME_REJECTION,
        NODE_NOT_DECLARED_REJECTION, REJECTION_METADATA_KEY, UNUSABLE_HOST_REJECTION,
    },
    verifier::{JoinHostVerifier, VerifiedJoinHost},
};
use crate::{
    atomic::{FileSystem, StdFileSystem},
    node::folder::{
        CertificateFingerprint, NodeFolderBootstrap, NodeFolderManager, certificate_identity_uri,
    },
    private_transport::exact_node_identity,
};

/// An authenticated connection to a manager's join listener.
///
/// The connection already proved the manager identity the token pins, so the operator can be
/// shown the site before anything is sent. Several joins may be attempted on it, for example
/// after the manager rejects a duplicate name.
pub struct JoinConnection {
    token: JoinToken,
    endpoint: Url,
    manager_host: SiteNodeHost,
    client: NodeJoinServiceClient<Channel>,
    host: VerifiedJoinHost,
    commit_timeout: Duration,
    resolver: Arc<dyn HostResolver>,
    filesystem: Arc<dyn FileSystem>,
}

impl JoinConnection {
    /// Connects to the token's join endpoint and verifies the manager against the token.
    ///
    /// `manager_host` replaces the endpoint's host, for a node that reaches the manager at
    /// another address or host name than the token names; the port and the pinned identity stay
    /// the token's. A host name is resolved by this machine when it connects.
    pub async fn connect(
        token: JoinToken,
        manager_host: Option<SiteNodeHost>,
        connect_timeout: Duration,
    ) -> Result<Self, NodeJoinError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let manager_host = match manager_host {
            Some(host) => host,
            None => token
                .endpoint
                .host_str()
                .and_then(|host| SiteNodeHost::normalize(host).ok())
                .ok_or_else(|| {
                    NodeJoinError::Connect(anyhow::anyhow!(
                        "the join endpoint {} has no IPv4 address or host name; give the \
                         manager host",
                        token.endpoint
                    ))
                })?,
        };
        let endpoint = join_endpoint(&token.endpoint, &manager_host)?;
        let verifier = Arc::new(JoinHostVerifier::new(&token));
        let channel = Endpoint::from_shared(endpoint.to_string())
            .map_err(|source| NodeJoinError::Connect(anyhow::Error::new(source)))?
            .connect_timeout(connect_timeout)
            .tls_config_with_verifier(
                ClientTlsConfig::new()
                    .domain_name(manager_host.to_string())
                    .assume_http2(true),
                Arc::clone(&verifier) as _,
            )
            .map_err(|source| NodeJoinError::Connect(anyhow::Error::new(source)))?
            .connect()
            .await
            .map_err(|source| {
                NodeJoinError::Connect(anyhow::Error::new(source).context(format!(
                    "reach and authenticate the join host at {endpoint}"
                )))
            })?;
        let host = verifier.verified().ok_or_else(|| {
            NodeJoinError::Connect(anyhow::anyhow!("the join host was not verified"))
        })?;
        Ok(Self {
            token,
            endpoint,
            manager_host,
            client: NodeJoinServiceClient::new(channel),
            host,
            commit_timeout: Timing::production().join_commit_timeout,
            resolver: Arc::new(SystemHostResolver),
            filesystem: Arc::new(StdFileSystem),
        })
    }

    /// Replaces the system resolver used to look up host names when choosing the address the
    /// node's private listener binds.
    #[cfg(test)]
    pub(crate) fn with_host_resolver(mut self, resolver: Arc<dyn HostResolver>) -> Self {
        self.resolver = resolver;
        self
    }

    /// Replaces the file system a rejoin rewrites the node folder's `worker.yaml` through.
    #[cfg(test)]
    pub(crate) fn with_file_system(mut self, filesystem: Arc<dyn FileSystem>) -> Self {
        self.filesystem = filesystem;
        self
    }

    /// Bounds the commit round trip; when it elapses the commit outcome is unknown and the
    /// installed folder is kept.
    pub fn with_commit_timeout(mut self, commit_timeout: Duration) -> Self {
        self.commit_timeout = commit_timeout;
        self
    }

    /// Returns the site this connection reached.
    pub fn site_id(&self) -> &str {
        &self.token.site_id
    }

    /// Returns the join endpoint this connection dialed.
    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// Returns the manager address or host name this connection dialed, which the node keeps
    /// using afterwards.
    pub fn manager_host(&self) -> &SiteNodeHost {
        &self.manager_host
    }

    /// Returns the manager node this connection authenticated.
    pub fn manager_node_id(&self) -> &str {
        &self.token.manager_node_id
    }

    /// Returns the verified site CA fingerprint, for the operator to compare with the manager.
    pub fn site_ca_sha256(&self) -> CertificateFingerprint {
        CertificateFingerprint::for_certificate(&self.host.site_ca_der)
    }

    /// Proposes this node, installs the offered node folder at `node_root`, and commits it.
    ///
    /// `node_root` must not exist. A rejected proposal changes nothing on either side and the
    /// same connection may retry.
    pub async fn join(
        &mut self,
        proposal: &JoinProposal,
        key: &NodeKey,
        node_root: &Path,
    ) -> Result<JoinedNodeFolder, NodeJoinError> {
        proposal.validate()?;
        reject_existing_folder(node_root)?;
        let request = proposal.to_request(&self.token.secret, &self.manager_host, key.csr_der()?);
        let offer = self
            .client
            .join(request)
            .await
            .map_err(NodeJoinError::from_join_status)?
            .into_inner();
        let host = offered_host(&offer, proposal.host.as_ref())?;
        let private_bind_address = private_bind_address(
            &host,
            proposal.private_port,
            &self.endpoint,
            self.resolver.as_ref(),
        )
        .await?;
        let contents = self.offer_contents(
            &offer,
            key,
            private_bind_address.clone(),
            proposal.targets.declared_machine.clone(),
        )?;
        let site_id = offer.site_id.clone();
        let node_id = offer.node_id.clone();

        let root = node_root.to_owned();
        tokio::task::spawn_blocking(move || install_node_folder(&root, contents))
            .await
            .map_err(|source| NodeJoinError::Folder(NodeFolderError::Storage(source.into())))??;
        info!(%node_id, root = %node_root.display(), "joined node folder installed");

        match self.commit(offer.offer_id).await {
            Ok(()) => Ok(JoinedNodeFolder {
                site_id,
                node_id,
                display_name: proposal.display_name.clone(),
                root: node_root.to_owned(),
                host,
                private_bind_address,
            }),
            Err(status) if is_clear_rejection(status.code()) => {
                let root = node_root.to_owned();
                tokio::task::spawn_blocking(move || remove_node_folder(&root))
                    .await
                    .map_err(|source| {
                        NodeJoinError::Folder(NodeFolderError::Storage(source.into()))
                    })??;
                Err(NodeJoinError::CommitRejected(status))
            }
            Err(status) => {
                warn!(%node_id, root = %node_root.display(), status = %status, "node join commit outcome is unknown");
                Err(NodeJoinError::CommitOutcomeUnknown {
                    root: node_root.to_owned(),
                    status,
                })
            }
        }
    }

    /// Proposes joining again from the intact node folder `folder`, commits the offer, and then
    /// rewrites the folder's manager endpoint, private bind address, and observation
    /// configuration.
    ///
    /// The offer must keep the folder's node, certificate, and targets exactly; its observation
    /// configuration replaces the folder's, repairing drift from the site. A refusal or an
    /// unknown commit outcome leaves the folder unchanged; repeating the rejoin with a fresh token
    /// completes a commit whose folder rewrite failed or whose outcome was unknown.
    pub async fn rejoin(
        &mut self,
        proposal: &RejoinProposal,
        folder: &RejoiningNodeFolder,
    ) -> Result<JoinedNodeFolder, NodeJoinError> {
        proposal.validate()?;
        let installed = folder.folder();
        let request = proposal.to_request(
            &self.token.secret,
            &self.manager_host,
            folder.key().rejoin_csr_der(&self.token.secret)?,
            installed.worker_certificate_der().to_vec(),
            installed.observation_configuration().clone(),
        );
        let offer = self
            .client
            .join(request)
            .await
            .map_err(NodeJoinError::from_join_status)?
            .into_inner();
        let host = offered_host(&offer, proposal.host.as_ref())?;
        self.check_offered_site(&offer)?;
        check_offered_folder(&offer, folder)?;
        let private_bind_address = private_bind_address(
            &host,
            proposal.private_port,
            &self.endpoint,
            self.resolver.as_ref(),
        )
        .await?;
        let observation = offer
            .observation
            .clone()
            .ok_or(NodeJoinError::InvalidOffer(
                "the offer has no observation configuration",
            ))?;
        let bootstrap = folder
            .rejoined_bootstrap(
                &offer.manager_private_endpoint,
                private_bind_address.clone(),
                observation,
            )
            .map_err(|_| {
                NodeJoinError::InvalidOffer("the offered node configuration is invalid")
            })?;
        let node_id = offer.node_id.clone();
        let root = folder.root().to_owned();

        match self.commit(offer.offer_id).await {
            Ok(()) => {}
            Err(status) if is_clear_rejection(status.code()) => {
                return Err(NodeJoinError::CommitRejected(status));
            }
            Err(status) => {
                warn!(%node_id, root = %root.display(), status = %status, "node rejoin commit outcome is unknown");
                return Err(NodeJoinError::CommitOutcomeUnknown { root, status });
            }
        }
        let filesystem = Arc::clone(&self.filesystem);
        let manifest_root = root.clone();
        let rewritten = tokio::task::spawn_blocking(move || {
            rewrite_bootstrap(filesystem.as_ref(), &manifest_root, &bootstrap)
        })
        .await
        .unwrap_or_else(|source| Err(NodeFolderError::Storage(source.into())));
        match rewritten {
            Ok(()) => {}
            Err(source @ NodeFolderError::RewriteUncertain(_)) => {
                warn!(%node_id, root = %root.display(), error = %source, "node rejoin folder rewrite may not be durable");
                return Err(NodeJoinError::FolderRewriteUncertain { root, source });
            }
            Err(source) => {
                warn!(%node_id, root = %root.display(), error = %source, "node rejoin folder rewrite failed");
                return Err(NodeJoinError::FolderRewrite { root, source });
            }
        }
        info!(%node_id, root = %root.display(), %host, "node rejoined with its folder");
        Ok(JoinedNodeFolder {
            site_id: offer.site_id,
            node_id,
            display_name: offer.node_display_name,
            root,
            host,
            private_bind_address,
        })
    }

    /// Commits the offer `offer_id`; past the commit timeout the outcome is unknown.
    #[expect(
        clippy::disallowed_methods,
        reason = "bounds a real join commit round trip"
    )]
    async fn commit(&mut self, offer_id: String) -> Result<(), Status> {
        let commit = CommitRequest {
            join_secret: self.token.secret.as_bytes().to_vec(),
            offer_id,
        };
        tokio::time::timeout(self.commit_timeout, self.client.commit(commit))
            .await
            .unwrap_or_else(|_| {
                Err(Status::deadline_exceeded(
                    "the commit reply did not arrive in time",
                ))
            })
            .map(|_| ())
    }

    /// Checks that the offer names the site, manager, manager certificate, and site CA that the
    /// token pins and the join host presented.
    fn check_offered_site(&self, offer: &JoinResponse) -> Result<(), NodeJoinError> {
        let invalid = |reason: &'static str| NodeJoinError::InvalidOffer(reason);
        if offer.site_id != self.token.site_id {
            return Err(invalid("the offer names another site than the token"));
        }
        if offer.manager_node_id != self.token.manager_node_id {
            return Err(invalid("the offer names another manager than the token"));
        }
        if offer.manager_certificate_sha256 != self.token.manager_certificate_sha256.as_str() {
            return Err(invalid(
                "the offer pins another manager certificate than the token",
            ));
        }
        if offer.site_ca_der != self.host.site_ca_der {
            return Err(invalid(
                "the offer carries another site CA than the join host presented",
            ));
        }
        Ok(())
    }

    /// Checks the offer against the token, the verified host, and the node's own key, and
    /// assembles the folder it describes, with the private listener bound at
    /// `private_bind_address` and the node declaring `declared_machine`, if any.
    fn offer_contents(
        &self,
        offer: &JoinResponse,
        key: &NodeKey,
        private_bind_address: BindAddress,
        declared_machine: Option<DeclaredMachine>,
    ) -> Result<NodeFolderContents, NodeJoinError> {
        let invalid = |reason: &'static str| NodeJoinError::InvalidOffer(reason);
        self.check_offered_site(offer)?;
        certificate_identity_uri(&offer.site_id, &offer.node_id)
            .map_err(|_| invalid("the offer node identity is invalid"))?;
        if !key.certifies(&offer.node_certificate_der) {
            return Err(invalid(
                "the offered certificate is not issued for this node's key",
            ));
        }
        if !certificate_names_node(&offer.node_certificate_der, &offer.site_id, &offer.node_id) {
            return Err(invalid(
                "the offered certificate does not name the offered node",
            ));
        }
        let observation = offer
            .observation
            .clone()
            .ok_or(invalid("the offer has no observation configuration"))?;

        let bootstrap = NodeFolderManager::new(
            &offer.manager_node_id,
            &offer.manager_private_endpoint,
            self.token.manager_certificate_sha256.clone(),
        )
        .and_then(|manager| {
            NodeFolderBootstrap::new(
                &offer.site_id,
                &offer.node_id,
                CertificateFingerprint::for_certificate(&offer.node_certificate_der),
                manager,
                observation,
                offer.realization_targets.clone(),
                private_bind_address,
            )
        })
        .and_then(|bootstrap| bootstrap.with_declared_machine(declared_machine))
        .map_err(|_| invalid("the offered node configuration is invalid"))?;

        Ok(NodeFolderContents {
            bootstrap,
            manager_certificate_der: self.host.manager_certificate_der.clone(),
            site_ca_der: self.host.site_ca_der.clone(),
            node_certificate_der: offer.node_certificate_der.clone(),
            private_key: key.private_key(),
        })
    }
}

/// Returns the local IPv4 address this machine uses to reach `endpoint`, such as the join
/// endpoint a connection dialed. A host name is resolved through `resolver` first.
pub async fn default_join_host(
    endpoint: &Url,
    resolver: &dyn HostResolver,
) -> std::io::Result<Ipv4Addr> {
    let host = endpoint
        .host_str()
        .and_then(|host| SiteNodeHost::normalize(host).ok())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the endpoint has no IPv4 address or host name",
            )
        })?;
    let address = host
        .resolve(resolver)
        .await
        .map_err(std::io::Error::other)?;
    let port = endpoint.port().unwrap_or_default();
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    // Connecting a UDP socket sends nothing; it only selects the route and its local address.
    socket.connect((address, port))?;
    match socket.local_addr()? {
        SocketAddr::V4(address) => Ok(*address.ip()),
        SocketAddr::V6(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "the route to the join endpoint has no IPv4 address",
        )),
    }
}

/// The node folder a successful join installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinedNodeFolder {
    /// Site the node joined.
    pub site_id: String,

    /// SiteNode identity the manager assigned.
    pub node_id: String,

    /// SiteNode display name the site declares for the node.
    pub display_name: String,

    /// Directory holding the node folder, which the joined node's server starts from.
    pub root: PathBuf,

    /// Address or host name the site recorded for reaching this node.
    pub host: SiteNodeHost,

    /// Address or host name the node's private listener binds, as recorded in `worker.yaml`.
    pub private_bind_address: BindAddress,
}

impl JoinedNodeFolder {
    /// Describes the address translation the network must provide when the recorded host is not
    /// an address of this machine, so the listener binds another one; `None` when it binds the
    /// recorded host itself. Host names are compared by the addresses they resolve to here; a
    /// name that does not resolve is reported instead, since nothing can be compared.
    pub async fn address_translation_warning(&self, resolver: &dyn HostResolver) -> Option<String> {
        if self.private_bind_address.host() == Some(&self.host) {
            return None;
        }
        let unresolved =
            |error| format!("{error}; the node's server resolves it again when it starts");
        let host = match self.host.resolve(resolver).await {
            Ok(host) => host,
            Err(error) => return Some(unresolved(error)),
        };
        let bind = match self.private_bind_address.resolve(resolver).await {
            Ok(bind) => bind,
            Err(error) => return Some(unresolved(error)),
        };
        if bind.ip() == std::net::IpAddr::V4(host) {
            return None;
        }
        Some(format!(
            "the manager reaches this node at {host}, which is not an address of this \
             machine; the private listener binds {bind}, so the network must forward \
             {host}:{port} to it with the same port",
            host = self.host,
            bind = self.private_bind_address,
            port = self.private_bind_address.port(),
        ))
    }
}

/// A node join did not complete. Each variant says what was left on this machine.
#[derive(Debug, thiserror::Error)]
pub enum NodeJoinError {
    #[error("the proposal is invalid: {0}")]
    Proposal(#[from] JoinProposalError),

    #[error("could not reach or authenticate the join host: {0}")]
    Connect(#[source] anyhow::Error),

    #[error("the node key or signing request could not be created: {0}")]
    Key(#[from] NodeKeyError),

    #[error("the manager refused the join ({code:?}): {message}", code = .0.code(), message = .0.message())]
    Join(Status),

    #[error("{message}", message = .0.message())]
    DuplicateName(Status),

    #[error("{message}", message = .0.message())]
    DuplicateHost(Status),

    #[error("{message}", message = .0.message())]
    UnusableHost(Status),

    #[error("{message}", message = .0.message())]
    NodeNotDeclared(Status),

    #[error("{message}", message = .0.message())]
    CertificateNotPinned(Status),

    #[error("{message}", message = .0.message())]
    CertificateExpired(Status),

    #[error("{message}", message = .0.message())]
    CertificateNotYetValid(Status),

    #[error("the manager's offer is not acceptable: {0}; nothing was installed or recorded")]
    InvalidOffer(&'static str),

    #[error(
        "no local address to bind the node's private listener: {0}; \
         nothing was installed or recorded"
    )]
    BindAddress(#[source] std::io::Error),

    #[error("the node folder could not be installed; nothing was recorded: {0}")]
    Folder(#[from] NodeFolderError),

    #[error(
        "the manager refused the commit ({code:?}): {message}; nothing was recorded and the \
         node folder is as it was before the join",
        code = .0.code(),
        message = .0.message()
    )]
    CommitRejected(Status),

    #[error(
        "the commit outcome is unknown ({code:?}: {message}); the node folder at {root} was kept",
        code = status.code(),
        message = status.message(),
        root = root.display()
    )]
    CommitOutcomeUnknown { root: PathBuf, status: Status },

    #[error(
        "the site recorded the node's new addresses, but the node folder at {root} could not be \
         updated: {source}; join again to update it",
        root = root.display()
    )]
    FolderRewrite {
        root: PathBuf,
        #[source]
        source: NodeFolderError,
    },

    #[error(
        "the site recorded the node's new addresses and the node folder at {root} was updated, \
         but the update may not be durable: {source}; join again to make sure",
        root = root.display()
    )]
    FolderRewriteUncertain {
        root: PathBuf,
        #[source]
        source: NodeFolderError,
    },
}

impl NodeJoinError {
    /// Classifies a refused Join by the reason the host tagged it with; a duplicate name can be
    /// retried on the same connection with another name, and a stale node folder cannot rejoin.
    fn from_join_status(status: Status) -> Self {
        let reason = status
            .metadata()
            .get(REJECTION_METADATA_KEY)
            .and_then(|value| value.to_str().ok());
        match reason {
            Some(DUPLICATE_NAME_REJECTION) => Self::DuplicateName(status),
            Some(DUPLICATE_HOST_REJECTION) => Self::DuplicateHost(status),
            Some(UNUSABLE_HOST_REJECTION) => Self::UnusableHost(status),
            Some(NODE_NOT_DECLARED_REJECTION) => Self::NodeNotDeclared(status),
            Some(CERTIFICATE_NOT_PINNED_REJECTION) => Self::CertificateNotPinned(status),
            Some(CERTIFICATE_EXPIRED_REJECTION) => Self::CertificateExpired(status),
            Some(CERTIFICATE_NOT_YET_VALID_REJECTION) => Self::CertificateNotYetValid(status),
            _ => Self::Join(status),
        }
    }
}

/// Returns the join endpoint at `manager_host`, keeping the token endpoint's scheme and port.
fn join_endpoint(token_endpoint: &Url, manager_host: &SiteNodeHost) -> Result<Url, NodeJoinError> {
    let mut endpoint = token_endpoint.clone();
    endpoint
        .set_host(Some(&manager_host.to_string()))
        .map_err(|source| {
            NodeJoinError::Connect(anyhow::Error::new(source).context("invalid join endpoint"))
        })?;
    Ok(endpoint)
}

/// Returns the host the offer records for this node, which must be the proposed one when the
/// node proposed a host.
fn offered_host(
    offer: &JoinResponse,
    proposed_host: Option<&SiteNodeHost>,
) -> Result<SiteNodeHost, NodeJoinError> {
    let host = SiteNodeHost::parse(&offer.node_host)
        .map_err(|_| NodeJoinError::InvalidOffer("the offer records no valid node host"))?;
    if proposed_host.is_some_and(|proposed| *proposed != host) {
        return Err(NodeJoinError::InvalidOffer(
            "the offer records another node host than proposed",
        ));
    }
    Ok(host)
}

/// Checks that a rejoin offer keeps the folder's node, certificate, and targets exactly, so the
/// rejoin can change nothing but the folder's addresses and observation configuration.
fn check_offered_folder(
    offer: &JoinResponse,
    folder: &RejoiningNodeFolder,
) -> Result<(), NodeJoinError> {
    let installed = folder.folder();
    let invalid = |reason: &'static str| Err(NodeJoinError::InvalidOffer(reason));
    if offer.node_id != installed.worker_node_id() {
        return invalid("the offer names another node than the node folder");
    }
    if offer.node_certificate_der != installed.worker_certificate_der() {
        return invalid("the offer carries another certificate than the node folder");
    }
    if offer.realization_targets != installed.realization_targets() {
        return invalid("the offer declares other execution targets than the node folder");
    }
    Ok(())
}

/// Returns the address the node's private listener binds at `port`: the recorded host, kept as
/// a name when it is one, when it resolves to an address of this machine; else the local address
/// on the route to the manager, which the network then must translate the recorded host to.
async fn private_bind_address(
    host: &SiteNodeHost,
    port: u16,
    manager_endpoint: &Url,
    resolver: &dyn HostResolver,
) -> Result<BindAddress, NodeJoinError> {
    // Binding succeeds only for an address of this machine; the socket is dropped right away.
    if let Ok(address) = host.resolve(resolver).await
        && UdpSocket::bind((address, 0)).is_ok()
    {
        return Ok(BindAddress::new(host.clone(), port));
    }
    let route = default_join_host(manager_endpoint, resolver)
        .await
        .map_err(NodeJoinError::BindAddress)?;
    Ok(BindAddress::from(SocketAddrV4::new(route, port)))
}

/// Reports whether the leaf's only site identity names `node_id`, so the folder never installs a
/// certificate issued for another node.
fn certificate_names_node(certificate_der: &[u8], site_id: &str, node_id: &str) -> bool {
    exact_node_identity(certificate_der, site_id).as_deref() == Some(node_id)
}

/// Codes the join host returns only when it definitely recorded nothing.
fn is_clear_rejection(code: Code) -> bool {
    matches!(
        code,
        Code::InvalidArgument
            | Code::AlreadyExists
            | Code::PermissionDenied
            | Code::Unauthenticated
            | Code::FailedPrecondition
            | Code::Aborted
    )
}

#[cfg(test)]
mod tests {
    use dotcore_domain::test_support::host_resolver::FakeHostResolver;

    use super::*;
    use crate::{manager_trust::ManagerTrust, membership::join::test_fixtures::initialized_site};

    #[test]
    fn offered_certificates_must_name_the_offered_node() {
        let fixture = initialized_site();
        let site_id = &fixture.site.site_id;
        let (certificate, _) = ManagerTrust::load(&fixture.root, &fixture.site)
            .expect("Site trust")
            .issue_worker(site_id, "node-worker")
            .expect("leaf");

        assert!(certificate_names_node(&certificate, site_id, "node-worker"));
        assert!(!certificate_names_node(&certificate, site_id, "node-other"));
        assert!(!certificate_names_node(
            &certificate,
            "site-other",
            "node-worker"
        ));
        assert!(!certificate_names_node(
            b"not a certificate",
            site_id,
            "node-worker"
        ));
    }

    #[tokio::test]
    async fn the_listener_binds_a_local_recorded_host_and_otherwise_the_route_to_the_manager() {
        let manager: Url = "https://127.0.0.1:7443".parse().expect("manager endpoint");
        // 192.0.2.0/24 is reserved for documentation and never assigned to this machine.
        let resolver = FakeHostResolver::default()
            .with("gpu-box.example", [Ipv4Addr::LOCALHOST])
            .with("remote.example", [Ipv4Addr::new(192, 0, 2, 8)]);
        let bind = async |host: &str| {
            let host = SiteNodeHost::parse(host).expect("host");
            private_bind_address(&host, 7443, &manager, &resolver)
                .await
                .expect("bind address")
                .to_string()
        };

        assert_eq!(bind("127.0.0.1").await, "127.0.0.1:7443");
        assert_eq!(bind("gpu-box.example").await, "gpu-box.example:7443");
        assert_eq!(bind("192.0.2.7").await, "127.0.0.1:7443");
        assert_eq!(bind("remote.example").await, "127.0.0.1:7443");
        assert_eq!(bind("unknown.example").await, "127.0.0.1:7443");
    }

    #[tokio::test]
    async fn translation_is_reported_only_when_the_listener_binds_another_address() {
        let resolver = FakeHostResolver::default()
            .with("gpu-box.example", [Ipv4Addr::new(10, 0, 0, 7)])
            .with("remote.example", [Ipv4Addr::new(192, 0, 2, 8)]);
        let folder = |host: &str, bind: &str| JoinedNodeFolder {
            site_id: "site-one".to_owned(),
            node_id: "node-one".to_owned(),
            display_name: "gpu-box".to_owned(),
            root: PathBuf::from("/node"),
            host: SiteNodeHost::parse(host).expect("host"),
            private_bind_address: BindAddress::parse(bind).expect("bind address"),
        };

        let same_name = folder("gpu-box.example", "gpu-box.example:7443");
        assert_eq!(same_name.address_translation_warning(&resolver).await, None);
        let same_address = folder("gpu-box.example", "10.0.0.7:7443");
        assert_eq!(
            same_address.address_translation_warning(&resolver).await,
            None
        );
        let translated = folder("remote.example", "10.0.0.7:7443");
        assert_eq!(
            translated
                .address_translation_warning(&resolver)
                .await
                .as_deref(),
            Some(
                "the manager reaches this node at remote.example, which is not an address of \
                 this machine; the private listener binds 10.0.0.7:7443, so the network must \
                 forward remote.example:7443 to it with the same port"
            )
        );
        for (host, bind) in [
            ("unknown.example", "10.0.0.7:7443"),
            ("gpu-box.example", "unknown.example:7443"),
        ] {
            assert_eq!(
                folder(host, bind)
                    .address_translation_warning(&resolver)
                    .await
                    .as_deref(),
                Some(
                    "could not resolve host name unknown.example; check that the network \
                     providing it is up; the node's server resolves it again when it starts"
                ),
                "{host} bound at {bind}"
            );
        }
    }
}
