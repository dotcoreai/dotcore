//! Accepts the TLS connections of a node's private listener, gives each one an identity and a
//! close notice, and bounds the connections whose certificate is not pinned.
//!
//! The listener presents the node's certificate with the site CA and asks for a client
//! certificate without requiring one, so a machine that holds no site certificate yet can reach
//! the join service. After the handshake, a connection whose certificate is pinned is never capped
//! or closed for being idle; any other connection counts toward a fixed limit and closes once
//! idle with no request in progress.

use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use anyhow::Context as _;
use dotcore_domain::real_time;
use futures_util::{Stream, StreamExt as _, future, stream};
use rustls::{
    RootCertStore, ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    server::WebPkiClientVerifier,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    time::{Instant, Sleep},
};
use tokio_rustls::{TlsAcceptor, server::TlsStream};
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::{CancellationToken, DropGuard};
use tonic::transport::server::Connected;
use tracing::debug;

use super::{PeerPins, PrivateServerConfiguration};

/// Longest TLS handshake one private connection may take; it bounds real network I/O.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Default number of connections without a pinned certificate open at once.
pub const DEFAULT_MAX_UNPINNED_CONNECTIONS: usize = 16;

/// The private listener's connection acceptance: TLS identity, live pins and unpinned limits.
pub struct PrivateConnections {
    acceptor: TlsAcceptor,
    site_id: String,
    pins: PeerPins,
    limits: UnpinnedConnectionLimits,
    next_id: AtomicU64,
    unpinned_open: Arc<AtomicUsize>,
}

impl PrivateConnections {
    /// Prepares acceptance for the listener described by `configuration`, classifying each
    /// connection against its live peer pins.
    pub fn new(
        configuration: &PrivateServerConfiguration,
        limits: UnpinnedConnectionLimits,
    ) -> anyhow::Result<Arc<Self>> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let trust_anchor = CertificateDer::from(configuration.trust_anchor_der.clone());
        let mut roots = RootCertStore::empty();
        roots
            .add(trust_anchor.clone())
            .context("add the site trust anchor")?;
        let verifier =
            WebPkiClientVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
                .allow_unauthenticated()
                .build()
                .context("build the private client verifier")?;
        // The site CA follows the leaf so a joining machine, which trusts only the CA fingerprint
        // in its token, can verify the chain.
        let mut tls = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .context("select the private TLS versions")?
            .with_client_cert_verifier(verifier)
            .with_single_cert(
                vec![
                    CertificateDer::from(configuration.certificate_der.clone()),
                    trust_anchor,
                ],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                    configuration.private_key_pkcs8.clone(),
                )),
            )
            .context("load the private listener identity")?;
        tls.alpn_protocols = vec![b"h2".to_vec()];
        Ok(Arc::new(Self {
            acceptor: TlsAcceptor::from(Arc::new(tls)),
            site_id: configuration.site_id.clone(),
            pins: configuration.peers.clone(),
            limits,
            next_id: AtomicU64::new(0),
            unpinned_open: Arc::default(),
        }))
    }

    /// Accepts connections from `listener` and yields each one once its handshake completed and
    /// its limits admitted it. Handshakes run concurrently, so a slow peer never holds back
    /// another; failed handshakes and refused connections are closed and never yielded.
    pub fn incoming(
        self: Arc<Self>,
        listener: TcpListener,
    ) -> impl Stream<Item = io::Result<PrivateConnectionIo>> + Send + 'static {
        TcpListenerStream::new(listener)
            .flat_map_unordered(None, move |accepted| {
                let connections = Arc::clone(&self);
                stream::once(Box::pin(async move {
                    match accepted {
                        Ok(stream) => connections.accept(stream).await.map(Ok),
                        Err(error) => Some(Err(error)),
                    }
                }))
            })
            .filter_map(future::ready)
    }

    async fn accept(&self, stream: TcpStream) -> Option<PrivateConnectionIo> {
        let peer_address = stream.peer_addr().ok()?;
        // The server ignores its own TCP options for a supplied stream, so each accepted socket
        // sets no delay here and small responses aren't held for the peer's acknowledgment.
        stream.set_nodelay(true).ok()?;
        let stream = match real_time::bound(HANDSHAKE_TIMEOUT, self.acceptor.accept(stream)).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => {
                debug!(peer = %peer_address, error = %error, "private connection handshake failed");
                return None;
            }
            Err(_) => {
                debug!(peer = %peer_address, "private connection handshake timed out");
                return None;
            }
        };
        let peer_certificate = stream
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|certificates| certificates.first())
            .map(|certificate| Arc::<[u8]>::from(certificate.as_ref()));
        let pinned = peer_certificate
            .as_deref()
            .is_some_and(|leaf| self.pins.authenticate(&self.site_id, leaf).is_some());
        let unpinned = if pinned {
            None
        } else {
            Some(self.claim_unpinned(peer_address)?)
        };
        let closed = CancellationToken::new();
        let now = Instant::now();
        Some(PrivateConnectionIo {
            stream,
            info: PrivateConnectionInfo {
                id: self.next_id.fetch_add(1, Ordering::Relaxed),
                peer_address,
                peer_certificate,
                activity: Arc::new(Mutex::new(ConnectionActivity {
                    requests: 0,
                    idle_since: now,
                })),
                closed: closed.clone(),
            },
            unpinned,
            _closed: closed.drop_guard(),
        })
    }

    /// Counts one more unpinned connection, or refuses it at the limit.
    fn claim_unpinned(&self, peer_address: SocketAddr) -> Option<UnpinnedSlot> {
        if self.unpinned_open.fetch_add(1, Ordering::SeqCst) >= self.limits.max_open {
            self.unpinned_open.fetch_sub(1, Ordering::SeqCst);
            debug!(peer = %peer_address, "private connection refused: too many unpinned connections");
            return None;
        }
        let now = Instant::now();
        Some(UnpinnedSlot {
            open: Arc::clone(&self.unpinned_open),
            idle_timeout: self.limits.idle_timeout,
            last_transfer: now,
            idle: Box::pin(real_time::deadline(now + self.limits.idle_timeout)),
        })
    }
}

/// Bounds on the private connections whose certificate is not pinned, such as a joining
/// machine's.
#[derive(Clone, Copy, Debug)]
pub struct UnpinnedConnectionLimits {
    /// Such connections open at once; a further one is closed after its handshake.
    pub max_open: usize,

    /// Longest silence of such a connection with no request in progress.
    pub idle_timeout: Duration,
}

/// Identity of one private connection, attached to each of its requests.
#[derive(Clone, Debug)]
pub struct PrivateConnectionInfo {
    id: u64,
    peer_address: SocketAddr,
    peer_certificate: Option<Arc<[u8]>>,
    activity: Arc<Mutex<ConnectionActivity>>,
    closed: CancellationToken,
}

impl PrivateConnectionInfo {
    /// Returns the id that tells this connection apart from every other one of its listener.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Returns the address the connection came from.
    pub fn peer_address(&self) -> SocketAddr {
        self.peer_address
    }

    /// Returns the leaf certificate the peer presented, which the site CA issued, if any.
    pub fn peer_certificate(&self) -> Option<&[u8]> {
        self.peer_certificate.as_deref()
    }

    /// Pauses the idle timeout of an unpinned connection until the returned guard drops, for a
    /// request that waits long, such as an operator review.
    pub fn begin_request(&self) -> RequestInProgress {
        self.lock_activity().requests += 1;
        RequestInProgress(Arc::clone(&self.activity))
    }

    /// Completes once the connection has closed.
    pub async fn closed(&self) {
        self.closed.cancelled().await;
    }

    /// Returns when the connection last became idle, or `None` while a request is running.
    fn idle_since(&self) -> Option<Instant> {
        let activity = self.lock_activity();
        (activity.requests == 0).then_some(activity.idle_since)
    }

    fn lock_activity(&self) -> MutexGuard<'_, ConnectionActivity> {
        self.activity.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Keeps the idle timeout of a connection paused while a request is running.
pub struct RequestInProgress(Arc<Mutex<ConnectionActivity>>);

impl Drop for RequestInProgress {
    fn drop(&mut self) {
        let mut activity = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        activity.requests -= 1;
        if activity.requests == 0 {
            activity.idle_since = Instant::now();
        }
    }
}

/// One accepted private connection after its TLS handshake.
pub struct PrivateConnectionIo {
    stream: TlsStream<TcpStream>,
    info: PrivateConnectionInfo,
    unpinned: Option<UnpinnedSlot>,

    // Dropped last, so the close notice fires once the unpinned slot is free.
    _closed: DropGuard,
}

impl PrivateConnectionIo {
    /// Fails a pending read of an unpinned connection once it stayed idle past its timeout,
    /// counted from the later of the last transfer and the end of the last request.
    fn poll_idle(&mut self, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Some(unpinned) = self.unpinned.as_mut() else {
            return Poll::Pending;
        };
        let Some(idle_since) = self.info.idle_since() else {
            return Poll::Pending;
        };
        let deadline = idle_since.max(unpinned.last_transfer) + unpinned.idle_timeout;
        if unpinned.idle.deadline() != deadline {
            unpinned.idle.as_mut().reset(deadline);
        }
        match unpinned.idle.as_mut().poll(context) {
            Poll::Ready(()) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "unpinned private connection was idle too long",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn record_transfer(&mut self) {
        if let Some(unpinned) = self.unpinned.as_mut() {
            unpinned.last_transfer = Instant::now();
        }
    }
}

impl Connected for PrivateConnectionIo {
    type ConnectInfo = PrivateConnectionInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.info.clone()
    }
}

impl AsyncRead for PrivateConnectionIo {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut this.stream).poll_read(context, buffer) {
            Poll::Ready(result) => {
                this.record_transfer();
                Poll::Ready(result)
            }
            Poll::Pending => this.poll_idle(context),
        }
    }
}

impl AsyncWrite for PrivateConnectionIo {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.stream).poll_write(context, buffer);
        if result.is_ready() {
            this.record_transfer();
        }
        result
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(context)
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(context)
    }
}

/// Requests in progress on one connection and when it last became idle.
#[derive(Debug)]
struct ConnectionActivity {
    requests: usize,
    idle_since: Instant,
}

/// The place one unpinned connection holds in the limit, with its idle timeout.
struct UnpinnedSlot {
    open: Arc<AtomicUsize>,
    idle_timeout: Duration,
    last_transfer: Instant,
    idle: Pin<Box<Sleep>>,
}

impl Drop for UnpinnedSlot {
    fn drop(&mut self) {
        self.open.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use tokio::io::AsyncReadExt as _;

    use super::*;
    use crate::{
        node::folder::CertificateFingerprint,
        private_transport::{PinnedPeerVerifier, PrivateNodeRole},
        test_support::private_tls::{TestAuthority, accept_private_connections, connect_tls},
    };

    const IDLE_TIMEOUT: Duration = Duration::from_millis(100);

    /// Deadline for an outcome that arrives at once on loopback.
    const DEADLINE: Duration = Duration::from_secs(10);

    #[tokio::test]
    async fn pinned_connections_are_never_capped_or_idle_closed() {
        let fixture = Fixture::new();
        let (address, mut accepted) = accept_private_connections(
            &fixture.configuration,
            UnpinnedConnectionLimits {
                max_open: 1,
                idle_timeout: IDLE_TIMEOUT,
            },
        )
        .await;
        let mut next = async || {
            tokio::time::timeout(DEADLINE, accepted.recv())
                .await
                .expect("an accepted connection")
                .expect("acceptance runs")
        };

        let mut pinned = Vec::new();
        for _ in 0..3 {
            let client = fixture.connect(address, true).await;
            let server = next().await;
            assert_eq!(
                server.connect_info().peer_certificate(),
                Some(fixture.worker.0.as_slice())
            );
            pinned.push((client, server));
        }
        let ids = pinned
            .iter()
            .map(|(_, server)| server.connect_info().id())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids.len(), 3, "every connection has its own id");

        let _unpinned_client = fixture.connect(address, false).await;
        let mut unpinned = next().await;
        assert_eq!(unpinned.connect_info().peer_certificate(), None);
        let mut refused = fixture.connect(address, false).await;
        let mut byte = [0_u8; 1];
        let read = tokio::time::timeout(DEADLINE, refused.read(&mut byte))
            .await
            .expect("the connection over the limit is closed");
        assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");

        let pinned_reads = pinned
            .into_iter()
            .map(|(client, mut server)| {
                tokio::spawn(async move {
                    let _client = client;
                    let mut byte = [0_u8; 1];
                    server.read(&mut byte).await
                })
            })
            .collect::<Vec<_>>();
        let idle = tokio::time::timeout(DEADLINE, unpinned.read(&mut byte))
            .await
            .expect("the unpinned connection idles out")
            .expect_err("idle unpinned connection");
        assert_eq!(idle.kind(), io::ErrorKind::TimedOut);
        for read in &pinned_reads {
            assert!(!read.is_finished(), "a pinned connection stays open");
        }

        let closed = unpinned.connect_info();
        drop(unpinned);
        tokio::time::timeout(DEADLINE, closed.closed())
            .await
            .expect("the close notice fires");
        let _again = fixture.connect(address, false).await;
        assert_eq!(next().await.connect_info().peer_certificate(), None);
        for read in pinned_reads {
            read.abort();
        }
    }

    #[tokio::test]
    async fn a_request_in_progress_keeps_an_unpinned_connection_open() {
        let fixture = Fixture::new();
        let (address, mut accepted) = accept_private_connections(
            &fixture.configuration,
            UnpinnedConnectionLimits {
                max_open: 1,
                idle_timeout: IDLE_TIMEOUT,
            },
        )
        .await;
        let _client = fixture.connect(address, false).await;
        let mut server = tokio::time::timeout(DEADLINE, accepted.recv())
            .await
            .expect("an accepted connection")
            .expect("acceptance runs");
        let request = server.connect_info().begin_request();

        let mut byte = [0_u8; 1];
        let paused = tokio::time::timeout(IDLE_TIMEOUT * 3, server.read(&mut byte)).await;
        assert!(paused.is_err(), "the request keeps the connection open");

        drop(request);
        let idle = tokio::time::timeout(DEADLINE, server.read(&mut byte))
            .await
            .expect("the connection idles out once the request ends")
            .expect_err("idle connection");
        assert_eq!(idle.kind(), io::ErrorKind::TimedOut);
    }

    /// A manager's private configuration pinning one worker.
    struct Fixture {
        authority: TestAuthority,
        manager_certificate: Vec<u8>,
        worker: (Vec<u8>, Vec<u8>),
        configuration: PrivateServerConfiguration,
    }

    impl Fixture {
        fn new() -> Self {
            let authority = TestAuthority::new();
            let manager = authority.issue("site-one", "manager-node", true);
            let worker = authority.issue("site-one", "worker-one", false);
            let configuration = authority.private_configuration(
                "site-one",
                "manager-node",
                PrivateNodeRole::Manager,
                manager.clone(),
                [("worker-one", PrivateNodeRole::Worker, worker.0.as_slice())],
            );
            Self {
                authority,
                manager_certificate: manager.0,
                worker,
                configuration,
            }
        }

        /// Connects as the pinned worker, or without a certificate.
        async fn connect(
            &self,
            address: SocketAddr,
            pinned: bool,
        ) -> tokio_rustls::client::TlsStream<TcpStream> {
            let verifier = PinnedPeerVerifier::new(
                self.authority.certificate.der(),
                "site-one",
                "manager-node",
                CertificateFingerprint::for_certificate(&self.manager_certificate),
            )
            .expect("manager verifier");
            connect_tls(
                address,
                Arc::new(verifier),
                pinned.then(|| self.worker.clone()),
            )
            .await
            .expect("TLS handshake")
        }
    }
}
