//! Gives the site manager authenticated private channels to each worker: one for forwarded
//! inference and one for diagnostics such as deployment logs, each its own connection so a slow
//! log reader can never hold back inference through shared HTTP/2 flow control.
//!
//! Each channel presents the manager's node identity and accepts only the exact pinned
//! certificate of the worker it dials; it is reused while the worker keeps its address and
//! rebuilt when the address changes. A worker's address is its host as the site declares it
//! plus the private port its current incarnation reported, for example `10.0.0.12:7443` or
//! `gpu-box.example.ts.net:7443`; a host name is resolved by this machine on each connect.

use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use anyhow::Context as _;
use dotcore_domain::SiteNodeHost;
use dotcore_proto::dotcore::site::v1::Site;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint, Identity};

use crate::private_transport::{
    PinnedPeerVerifier, PrivateNodeRole, PrivateServerConfiguration, pem,
};
use crate::{
    node::{
        control::{CoordinatorConnectionState, ManagerNodeControlCoordinator},
        folder::CertificateFingerprint,
    },
    repository::{SiteRepository, SiteRepositoryError},
    view::fencing::{ManagerSession, NodeIncarnation},
};

/// Longest wait for a worker to accept the connection, and again to complete the TLS handshake.
const WORKER_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Interval of HTTP/2 pings that detect a lost worker connection, including while idle.
const WORKER_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);

/// Longest wait for a ping acknowledgement before the worker connection is considered lost.
const WORKER_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(5);

/// The manager's single holder of worker certificate pins and per-worker private channels.
///
/// Channels connect lazily, so handing one out never waits on the network. Clones share the
/// same channels.
#[derive(Clone)]
pub struct WorkerChannels {
    identity: Arc<ManagerClientIdentity>,
    channels: Arc<Mutex<BTreeMap<(String, WorkerChannelPurpose), WorkerChannel>>>,
}

impl WorkerChannels {
    /// Projects the manager's private listener identity and its worker pins.
    pub fn new(configuration: &PrivateServerConfiguration) -> Self {
        Self {
            identity: Arc::new(ManagerClientIdentity {
                site_id: configuration.site_id.clone(),
                trust_anchor_der: configuration.trust_anchor_der.clone(),
                certificate_der: configuration.certificate_der.clone(),
                private_key_pkcs8: configuration.private_key_pkcs8.clone(),
                worker_pins: configuration
                    .peers
                    .iter()
                    .filter(|(_, policy)| policy.role == PrivateNodeRole::Worker)
                    .map(|(node_id, policy)| (node_id.clone(), policy.certificate_sha256.clone()))
                    .collect(),
            }),
            channels: Arc::default(),
        }
    }

    /// Resolves the worker's current address and returns its diagnostics channel with the
    /// manager session and worker incarnation a request to it must name.
    ///
    /// Only a live current incarnation that reported a private port is reachable; the
    /// deployment's own readiness plays no part.
    pub fn connect(
        &self,
        site: &SiteRepository,
        coordinator: &ManagerNodeControlCoordinator,
        node_id: &str,
    ) -> Result<WorkerConnection, WorkerChannelError> {
        // The coordinator lock is released before the site guard is taken.
        let endpoint = coordinator
            .node_endpoint(node_id)
            .filter(|endpoint| {
                endpoint.connection_state != CoordinatorConnectionState::Disconnected
            })
            .ok_or(WorkerChannelError::NotConnected)?;
        let port = endpoint
            .private_port
            .ok_or(WorkerChannelError::NoPrivatePort)?;
        let host = site
            .with_site(|site| declared_worker_host(site, node_id))
            .map_err(WorkerChannelError::Site)?
            .ok_or(WorkerChannelError::UndeclaredWorker)?;
        Ok(WorkerConnection {
            channel: self.channel(
                node_id,
                WorkerChannelPurpose::Diagnostics,
                WorkerAddress { host, port },
            )?,
            manager_session: coordinator.manager_session().clone(),
            node_incarnation: endpoint.incarnation,
        })
    }

    /// Returns the reused channel of one purpose to one worker at `address`, replacing it when
    /// the address changed.
    pub fn channel(
        &self,
        node_id: &str,
        purpose: WorkerChannelPurpose,
        address: WorkerAddress,
    ) -> Result<Channel, WorkerChannelError> {
        let mut channels = self.channels.lock().unwrap_or_else(PoisonError::into_inner);
        let key = (node_id.to_owned(), purpose);
        match channels.get(&key) {
            Some(existing) if existing.address == address => Ok(existing.channel.clone()),
            _ => {
                let channel = self.endpoint(node_id, &address)?.connect_lazy();
                channels.insert(
                    key,
                    WorkerChannel {
                        address,
                        channel: channel.clone(),
                    },
                );
                Ok(channel)
            }
        }
    }

    /// Returns each worker's currently cached channel address for one purpose.
    #[cfg(any(test, feature = "test-support"))]
    pub fn channel_addresses(&self, purpose: WorkerChannelPurpose) -> Vec<(String, WorkerAddress)> {
        self.channels
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|((_, cached), _)| *cached == purpose)
            .map(|((node_id, _), channel)| (node_id.clone(), channel.address.clone()))
            .collect()
    }

    /// Builds the endpoint to `address`. The connector resolves a host name on every connect and
    /// tries each resolved address in turn, so a name whose first address refuses still connects.
    fn endpoint(
        &self,
        node_id: &str,
        address: &WorkerAddress,
    ) -> Result<Endpoint, WorkerChannelError> {
        let identity = &self.identity;
        let pin = identity
            .worker_pins
            .get(node_id)
            .ok_or(WorkerChannelError::MissingPin)?;
        let verifier = Arc::new(
            PinnedPeerVerifier::new(
                &identity.trust_anchor_der,
                &identity.site_id,
                node_id,
                pin.clone(),
            )
            .context("build the pinned worker verifier")
            .map_err(WorkerChannelError::Endpoint)?,
        );
        let client_identity = Identity::from_pem(
            pem("CERTIFICATE", &identity.certificate_der),
            pem("PRIVATE KEY", &identity.private_key_pkcs8),
        );
        Endpoint::from_shared(format!("https://{address}"))
            .context("build the worker endpoint")
            .map_err(WorkerChannelError::Endpoint)?
            .connect_timeout(WORKER_CONNECT_TIMEOUT)
            .http2_keep_alive_interval(WORKER_KEEPALIVE_INTERVAL)
            .keep_alive_timeout(WORKER_KEEPALIVE_TIMEOUT)
            .keep_alive_while_idle(true)
            .tls_config_with_verifier(
                // No verifier checks the server name; the worker is authenticated by its pin.
                ClientTlsConfig::new()
                    .domain_name(address.host.to_string())
                    .identity(client_identity)
                    .assume_http2(true)
                    .timeout(WORKER_CONNECT_TIMEOUT),
                verifier,
            )
            .context("configure worker TLS")
            .map_err(WorkerChannelError::Endpoint)
    }
}

/// The kind of traffic a worker channel carries. Each purpose has its own connection, because
/// HTTP/2 shares one receive window across a connection's streams: a stalled follow that leaves
/// its data unread would otherwise hold back every other stream to the worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorkerChannelPurpose {
    /// Forwarded exact-target inference.
    Inference,

    /// Diagnostics read on demand, such as deployment logs.
    Diagnostics,
}

/// Where the manager dials a worker: its declared host and its reported private port, for
/// example `gpu-box.example.ts.net:7443`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerAddress {
    pub host: SiteNodeHost,
    pub port: u16,
}

impl fmt::Display for WorkerAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.host, self.port)
    }
}

#[cfg(any(test, feature = "test-support"))]
impl std::str::FromStr for WorkerAddress {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (host, port) = value
            .rsplit_once(':')
            .ok_or_else(|| anyhow::anyhow!("worker address {value:?} has no port"))?;
        Ok(Self {
            host: SiteNodeHost::parse(host)?,
            port: port.parse()?,
        })
    }
}

/// A channel to one worker with the identities that name its current incarnation.
pub struct WorkerConnection {
    pub channel: Channel,
    pub manager_session: ManagerSession,
    pub node_incarnation: NodeIncarnation,
}

/// Why the manager cannot hand out a channel to a worker.
#[derive(Debug, thiserror::Error)]
pub enum WorkerChannelError {
    #[error("worker is not connected")]
    NotConnected,

    #[error("worker reported no private listener port")]
    NoPrivatePort,

    #[error("worker is not declared with a valid host in the site")]
    UndeclaredWorker,

    #[error("site could not be read to resolve the worker host")]
    Site(#[source] SiteRepositoryError),

    #[error("no certificate pin is configured for the worker")]
    MissingPin,

    #[error("worker channel could not be configured")]
    Endpoint(#[source] anyhow::Error),
}

/// Returns the host the site declares for a worker node, the one source of every worker address.
pub fn declared_worker_host(site: &Site, node_id: &str) -> Option<SiteNodeHost> {
    site.nodes
        .iter()
        .find(|declared| declared.node_id == node_id && declared.site_manager.is_none())
        .and_then(|declared| SiteNodeHost::parse(&declared.host).ok())
}

/// The manager credentials and worker pins every worker channel is built from.
struct ManagerClientIdentity {
    site_id: String,
    trust_anchor_der: Vec<u8>,
    certificate_der: Vec<u8>,
    private_key_pkcs8: Vec<u8>,
    worker_pins: BTreeMap<String, CertificateFingerprint>,
}

/// One worker's channel and the address it dials.
struct WorkerChannel {
    address: WorkerAddress,
    channel: Channel,
}

/// Lets tests name a worker by the socket address of a local listener.
#[cfg(any(test, feature = "test-support"))]
impl From<std::net::SocketAddr> for WorkerAddress {
    fn from(address: std::net::SocketAddr) -> Self {
        address.to_string().parse().expect("an IPv4 worker address")
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use dotcore_proto::{dotcore::site::private::v1::NodeObservation, google::protobuf::Timestamp};

    use super::*;
    use crate::{
        node::{
            control::AcceptedNodeObservationInput,
            observation_configuration::NodeObservationConfigurationDigest,
        },
        test_support::private_tls::TestAuthority,
    };

    #[tokio::test(start_paused = true)]
    async fn connects_a_connected_worker_at_its_declared_host_and_reported_port() {
        let fixture = Fixture::new();
        fixture.register("worker-one", Some(47_123));

        let connection = fixture.connect("worker-one").expect("worker connection");

        assert_eq!(connection.manager_session, session());
        assert_eq!(connection.node_incarnation, incarnation());
        assert_eq!(
            fixture
                .channels
                .channel_addresses(WorkerChannelPurpose::Diagnostics),
            [(
                "worker-one".to_owned(),
                "127.0.0.2:47123".parse().expect("address")
            )]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_worker_without_a_live_current_incarnation_is_not_connected() {
        let fixture = Fixture::new();
        assert!(matches!(
            fixture.connect("worker-one"),
            Err(WorkerChannelError::NotConnected)
        ));

        fixture.register("worker-one", Some(47_123));
        tokio::time::advance(crate::node::control::LIVENESS_TTL + Duration::from_secs(1)).await;

        assert!(matches!(
            fixture.connect("worker-one"),
            Err(WorkerChannelError::NotConnected)
        ));
        assert!(
            fixture
                .channels
                .channel_addresses(WorkerChannelPurpose::Diagnostics)
                .is_empty()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_worker_is_reachable_only_with_a_port_a_declared_host_and_a_pin() {
        let fixture = Fixture::new();
        fixture.register("worker-one", None);
        fixture.register("manager-node", Some(47_123));
        fixture.register("worker-three", Some(47_123));
        fixture.register("worker-two", Some(47_123));

        assert!(matches!(
            fixture.connect("worker-one"),
            Err(WorkerChannelError::NoPrivatePort)
        ));
        assert!(matches!(
            fixture.connect("manager-node"),
            Err(WorkerChannelError::UndeclaredWorker)
        ));
        assert!(matches!(
            fixture.connect("worker-three"),
            Err(WorkerChannelError::UndeclaredWorker)
        ));
        assert!(matches!(
            fixture.connect("worker-two"),
            Err(WorkerChannelError::MissingPin)
        ));
        assert!(
            fixture
                .channels
                .channel_addresses(WorkerChannelPurpose::Diagnostics)
                .is_empty()
        );
    }

    /// Manager site, node-control coordinator, and worker channels pinning only `worker-one`.
    struct Fixture {
        _directory: tempfile::TempDir,
        site: SiteRepository,
        coordinator: ManagerNodeControlCoordinator,
        channels: WorkerChannels,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().expect("manager site directory");
            let site_path = directory.path().join("site.yaml");
            std::fs::write(&site_path, MANAGER_SITE).expect("manager site");
            let authority = TestAuthority::new();
            let (worker_certificate, _) = authority.issue("site-one", "worker-one", true);
            let channels = WorkerChannels::new(&authority.private_configuration(
                "site-one",
                "manager-node",
                PrivateNodeRole::Manager,
                authority.issue("site-one", "manager-node", false),
                [(
                    "worker-one",
                    PrivateNodeRole::Worker,
                    worker_certificate.as_slice(),
                )],
            ));
            Self {
                site: SiteRepository::load(
                    &site_path,
                    crate::test_support::site_declaration_rules(),
                )
                .expect("validated manager site"),
                _directory: directory,
                coordinator: ManagerNodeControlCoordinator::new(session()),
                channels,
            }
        }

        fn connect(&self, node_id: &str) -> Result<WorkerConnection, WorkerChannelError> {
            self.channels
                .connect(&self.site, &self.coordinator, node_id)
        }

        fn register(&self, node_id: &str, private_port: Option<u16>) {
            self.coordinator
                .register_observation(
                    &NodeObservation {
                        site_id: "site-one".to_owned(),
                        node_id: node_id.to_owned(),
                        incarnation_id: incarnation().as_str().to_owned(),
                        revision: 1,
                        snapshot: None,
                        observation_configuration_sha256: "0".repeat(64),
                        private_port: private_port.map(u32::from),
                    },
                    AcceptedNodeObservationInput {
                        node_id: node_id.to_owned(),
                        incarnation: incarnation(),
                        revision: 1,
                        snapshot: Default::default(),
                        observation_configuration_sha256:
                            NodeObservationConfigurationDigest::parse("0".repeat(64))
                                .expect("configuration digest"),
                        received_at: Timestamp::default(),
                        private_port,
                    },
                )
                .expect("registered worker");
        }
    }

    fn session() -> ManagerSession {
        ManagerSession::parse("a".repeat(32)).expect("manager session")
    }

    fn incarnation() -> NodeIncarnation {
        NodeIncarnation::parse("1".repeat(32)).expect("worker incarnation")
    }

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
  host: 127.0.0.3
  displayName: Worker two
executionTargets: []
modelDeployments: []
applicationDeployments: []
"#;
}
